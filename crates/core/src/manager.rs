//! The download manager: queue, concurrency limit, speed limit, scheduler, history, events.
//!
//! Everything the UI / IPC layer needs is on [`Manager`]. It is cheap to clone (`Arc`).

use crate::categories::Category;
use crate::engine::{self, Job, Outcome, Shared, Signal};
use crate::filename::{sanitize, sidecar_path, unique_path};
use crate::limiter::Limiter;
use crate::net::{build_client, ClientOptions};
use crate::probe::{probe, ProbeInfo};
use crate::settings::Settings;
use crate::types::{DownloadId, RequestInfo, Segment, Status};
use crate::{Error, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A download as stored in the history and shown in the list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub id: DownloadId,
    pub request: RequestInfo,
    pub filename: String,
    pub dest: PathBuf,
    pub category: Category,
    pub status: Status,
    /// Total size if known.
    pub size: Option<u64>,
    pub downloaded: u64,
    pub error: Option<String>,
    pub added_at: u64,
    pub completed_at: Option<u64>,
    /// Do not start before this unix time (scheduler).
    pub scheduled_at: Option<u64>,
    #[serde(default)]
    pub resumable: bool,
    #[serde(default)]
    pub content_type: Option<String>,
}

/// Item + live numbers, as sent to the UI.
#[derive(Clone, Debug, Serialize)]
pub struct ItemView {
    #[serde(flatten)]
    pub item: Item,
    /// Bytes per second.
    pub speed: u64,
    /// Seconds remaining, if computable.
    pub eta: Option<u64>,
    pub connections: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    Added(ItemView),
    Updated(ItemView),
    /// Batched live progress for running downloads (≈4 Hz).
    Progress(Vec<ItemView>),
    Removed { id: DownloadId },
    Completed(ItemView),
    Failed(ItemView),
    Settings(Box<Settings>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartMode {
    /// Queue and start as soon as a slot is free.
    #[default]
    Now,
    /// Add to the list without starting ("Download Later").
    Later,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AddRequest {
    pub request: RequestInfo,
    /// Override the detected file name.
    #[serde(default)]
    pub filename: Option<String>,
    /// Override the destination folder.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    #[serde(default)]
    pub category: Option<Category>,
    #[serde(default)]
    pub start: StartMode,
    #[serde(default)]
    pub scheduled_at: Option<u64>,
    /// Size already learned from `inspect` (display only).
    #[serde(default)]
    pub size: Option<u64>,
    /// Add even if the same URL is already in the list.
    #[serde(default)]
    pub allow_duplicate: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AddResult {
    pub id: DownloadId,
    /// The URL was already in the list (unfinished); that entry was resumed instead.
    pub duplicate: bool,
}

/// What the "Download File Info" dialog shows before the user confirms.
#[derive(Clone, Debug, Serialize)]
pub struct Inspection {
    pub probe: ProbeInfo,
    pub category: Category,
    pub dir: PathBuf,
    pub suggested_path: PathBuf,
    pub duplicate_of: Option<DownloadId>,
}

struct Run {
    ctl: watch::Sender<Signal>,
    shared: Arc<Shared>,
}

struct SpeedState {
    last_received: u64,
    last_at: Instant,
    ema: f64,
}

struct Inner {
    items: BTreeMap<DownloadId, Item>,
    runs: HashMap<DownloadId, Run>,
    speed: HashMap<DownloadId, SpeedState>,
    next_id: DownloadId,
    settings: Settings,
}

pub struct Manager {
    inner: Mutex<Inner>,
    client: Mutex<Client>,
    limiter: Arc<Limiter>,
    tx: broadcast::Sender<Event>,
    data_dir: PathBuf,
    handle: Handle,
    dirty: AtomicBool,
}

impl Manager {
    /// Create a manager that stores its state under `data_dir`.
    /// Must be called from within (or be given a handle to) a Tokio runtime.
    pub fn new(data_dir: impl Into<PathBuf>, handle: Handle) -> Result<Arc<Self>> {
        let data_dir = data_dir.into();
        std::fs::create_dir_all(&data_dir)?;
        let settings: Settings = read_json(&data_dir.join("settings.json")).unwrap_or_default();
        let settings = settings.sanitized();
        let mut items: BTreeMap<DownloadId, Item> = BTreeMap::new();
        let list: Vec<Item> = read_json(&data_dir.join("history.json")).unwrap_or_default();
        let mut next_id = 1;
        for mut it in list {
            next_id = next_id.max(it.id + 1);
            // Anything that was in flight when the app stopped is now just "unfinished".
            if matches!(it.status, Status::Downloading | Status::Queued) {
                it.status = if settings.auto_resume { Status::Queued } else { Status::Paused };
            }
            items.insert(it.id, it);
        }
        let client = build_client(&ClientOptions { proxy: settings.proxy.clone(), ..Default::default() })?;
        let limiter = Arc::new(Limiter::new(settings.speed_limit));
        let (tx, _) = broadcast::channel(512);
        let m = Arc::new(Self {
            inner: Mutex::new(Inner { items, runs: HashMap::new(), speed: HashMap::new(), next_id, settings }),
            client: Mutex::new(client),
            limiter,
            tx,
            data_dir,
            handle: handle.clone(),
            dirty: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&m);
        handle.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
            let mut n: u64 = 0;
            loop {
                tick.tick().await;
                let Some(m) = weak.upgrade() else { break };
                n += 1;
                m.tick();
                if n % 8 == 0 {
                    m.save_if_dirty();
                }
                if n % 20 == 0 {
                    m.pump();
                }
            }
        });
        m.pump();
        Ok(m)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    // ───────────────────────── queries ─────────────────────────

    pub fn list(&self) -> Vec<ItemView> {
        let g = self.inner.lock().unwrap();
        g.items.values().map(|it| view(&g, it)).collect()
    }

    pub fn get(&self, id: DownloadId) -> Option<ItemView> {
        let g = self.inner.lock().unwrap();
        g.items.get(&id).map(|it| view(&g, it))
    }

    /// Live segment map of a running download (for the per-connection progress bar).
    pub fn segments(&self, id: DownloadId) -> Vec<Segment> {
        let g = self.inner.lock().unwrap();
        g.runs.get(&id).map(|r| r.shared.segments()).unwrap_or_default()
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    pub fn find_duplicate(&self, url: &str) -> Option<DownloadId> {
        let g = self.inner.lock().unwrap();
        g.items.values().find(|i| i.request.url == url && i.status != Status::Completed).map(|i| i.id)
    }

    // ───────────────────────── settings ─────────────────────────

    pub fn update_settings(self: &Arc<Self>, s: Settings) -> Result<()> {
        let s = s.sanitized();
        let proxy_changed = {
            let mut g = self.inner.lock().unwrap();
            let changed = g.settings.proxy != s.proxy;
            g.settings = s.clone();
            changed
        };
        self.limiter.set_rate(s.speed_limit);
        if proxy_changed {
            *self.client.lock().unwrap() = build_client(&ClientOptions { proxy: s.proxy.clone(), ..Default::default() })?;
        }
        write_json(&self.data_dir.join("settings.json"), &s)?;
        let _ = self.tx.send(Event::Settings(Box::new(s)));
        self.pump();
        Ok(())
    }

    // ───────────────────────── adding ─────────────────────────

    /// Probe a URL for the confirmation dialog.
    pub async fn inspect(&self, request: &RequestInfo) -> Result<Inspection> {
        let client = self.client.lock().unwrap().clone();
        let info = probe(&client, request).await?;
        let category = Category::from_filename(&info.filename);
        let (dir, path) = {
            let g = self.inner.lock().unwrap();
            let dir = g.settings.dir_for(category);
            let path = reserve_path(&g, &dir, &info.filename);
            (dir, path)
        };
        Ok(Inspection { duplicate_of: self.find_duplicate(&request.url), probe: info, category, dir, suggested_path: path })
    }

    pub async fn add(self: &Arc<Self>, a: AddRequest) -> Result<AddResult> {
        url::Url::parse(&a.request.url).map_err(|e| Error::BadUrl(format!("{}: {e}", a.request.url)))?;
        if !a.allow_duplicate {
            if let Some(id) = self.find_duplicate(&a.request.url) {
                self.resume(id);
                return Ok(AddResult { id, duplicate: true });
            }
        }

        let mut size = a.size;
        let mut content_type = None;
        let filename = match &a.filename {
            Some(n) if !n.trim().is_empty() => sanitize(n),
            _ => {
                let client = self.client.lock().unwrap().clone();
                let info = probe(&client, &a.request).await?;
                size = info.size;
                content_type = info.content_type.clone();
                info.filename
            }
        };
        let category = a.category.unwrap_or_else(|| Category::from_filename(&filename));

        let view = {
            let mut g = self.inner.lock().unwrap();
            let dir = a.dir.clone().unwrap_or_else(|| g.settings.dir_for(category));
            let dest = reserve_path(&g, &dir, &filename);
            let id = g.next_id;
            g.next_id += 1;
            let status = match a.start {
                StartMode::Now => Status::Queued,
                StartMode::Later => Status::Paused,
            };
            let item = Item {
                id,
                request: a.request.clone(),
                filename: dest.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(filename),
                dest,
                category,
                status,
                size,
                downloaded: 0,
                error: None,
                added_at: now_secs(),
                completed_at: None,
                scheduled_at: a.scheduled_at,
                resumable: false,
                content_type,
            };
            g.items.insert(id, item.clone());
            view(&g, &item)
        };
        let id = view.item.id;
        self.dirty.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Event::Added(view));
        self.pump();
        Ok(AddResult { id, duplicate: false })
    }

    // ───────────────────────── control ─────────────────────────

    pub fn pause(&self, id: DownloadId) {
        let mut g = self.inner.lock().unwrap();
        let running = if let Some(r) = g.runs.get(&id) {
            let _ = r.ctl.send(Signal::Pause);
            true
        } else {
            false
        };
        if let Some(it) = g.items.get_mut(&id) {
            if matches!(it.status, Status::Queued | Status::Downloading) {
                it.status = Status::Paused;
                let v = view_of(&g, id);
                drop(g);
                self.dirty.store(true, Ordering::Relaxed);
                if let Some(v) = v {
                    let _ = self.tx.send(Event::Updated(v));
                }
                let _ = running;
            }
        }
    }

    pub fn resume(self: &Arc<Self>, id: DownloadId) {
        {
            let mut g = self.inner.lock().unwrap();
            let Some(it) = g.items.get_mut(&id) else { return };
            if !matches!(it.status, Status::Paused | Status::Error) {
                return;
            }
            it.status = Status::Queued;
            it.error = None;
            it.scheduled_at = None;
            let v = view_of(&g, id);
            drop(g);
            if let Some(v) = v {
                let _ = self.tx.send(Event::Updated(v));
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.pump();
    }

    pub fn pause_all(&self) {
        let ids: Vec<_> = self.inner.lock().unwrap().items.values().filter(|i| matches!(i.status, Status::Queued | Status::Downloading)).map(|i| i.id).collect();
        for id in ids {
            self.pause(id);
        }
    }

    pub fn resume_all(self: &Arc<Self>) {
        let ids: Vec<_> = self.inner.lock().unwrap().items.values().filter(|i| matches!(i.status, Status::Paused | Status::Error)).map(|i| i.id).collect();
        for id in ids {
            self.resume(id);
        }
    }

    /// Replace the request (URL / cookies / referer) of an unfinished download,
    /// e.g. when the browser extension captured a fresh link for an expired one.
    pub fn update_request(&self, id: DownloadId, request: RequestInfo) {
        let mut g = self.inner.lock().unwrap();
        if let Some(it) = g.items.get_mut(&id) {
            if it.status != Status::Completed {
                it.request = request;
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn set_schedule(self: &Arc<Self>, id: DownloadId, at: Option<u64>) {
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(it) = g.items.get_mut(&id) {
                if it.status == Status::Completed {
                    return;
                }
                it.scheduled_at = at;
                if at.is_some() && it.status == Status::Paused {
                    it.status = Status::Queued;
                }
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.pump();
    }

    /// Remove from the list. Partial data is always deleted; the finished file only if `delete_file`.
    pub fn remove(self: &Arc<Self>, id: DownloadId, delete_file: bool) {
        let item = {
            let mut g = self.inner.lock().unwrap();
            if let Some(r) = g.runs.get(&id) {
                let _ = r.ctl.send(Signal::Cancel); // engine deletes .part/.idl itself
            }
            g.speed.remove(&id);
            g.items.remove(&id)
        };
        let Some(it) = item else { return };
        let _ = std::fs::remove_file(engine::part_path(&it.dest));
        let _ = std::fs::remove_file(sidecar_path(&it.dest));
        if delete_file {
            let _ = std::fs::remove_file(&it.dest);
        }
        self.dirty.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Event::Removed { id });
        self.pump();
    }

    /// Remove all finished entries from the list (files stay).
    pub fn clear_completed(self: &Arc<Self>) {
        let ids: Vec<_> = self.inner.lock().unwrap().items.values().filter(|i| i.status == Status::Completed).map(|i| i.id).collect();
        for id in ids {
            self.remove(id, false);
        }
    }

    /// Stop everything and flush state to disk (call on app exit).
    pub fn shutdown(&self) {
        {
            let g = self.inner.lock().unwrap();
            for r in g.runs.values() {
                let _ = r.ctl.send(Signal::Pause);
            }
        }
        // Give workers a moment to flush their buffers and write sidecars.
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while Instant::now() < deadline && !self.inner.lock().unwrap().runs.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.save_if_dirty();
    }

    // ───────────────────────── scheduling ─────────────────────────

    /// Start queued downloads while there are free slots.
    pub fn pump(self: &Arc<Self>) {
        let mut starts = Vec::new();
        {
            let mut g = self.inner.lock().unwrap();
            let slots = (g.settings.max_concurrent as usize).saturating_sub(g.runs.len());
            if slots == 0 {
                return;
            }
            let now = now_secs();
            let ids: Vec<DownloadId> = g
                .items
                .values()
                .filter(|i| i.status == Status::Queued && !g.runs.contains_key(&i.id) && i.scheduled_at.map_or(true, |t| t <= now))
                .map(|i| i.id)
                .take(slots)
                .collect();
            for id in ids {
                let max_conn = g.settings.max_connections;
                let it = g.items.get_mut(&id).unwrap();
                it.status = Status::Downloading;
                it.error = None;
                let job = Job { request: it.request.clone(), dest: it.dest.clone(), max_connections: max_conn };
                let (tx, rx) = watch::channel(Signal::Run);
                let shared = Arc::new(Shared::default());
                g.runs.insert(id, Run { ctl: tx, shared: shared.clone() });
                starts.push((id, job, shared, rx));
            }
        }
        for (id, job, shared, rx) in starts {
            if let Some(v) = self.get(id) {
                let _ = self.tx.send(Event::Updated(v));
            }
            let me = self.clone();
            let client = self.client.lock().unwrap().clone();
            let limiter = self.limiter.clone();
            self.handle.spawn(async move {
                let res = engine::run(client, job, shared, rx, limiter).await;
                me.finished(id, res);
            });
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn finished(self: &Arc<Self>, id: DownloadId, res: Result<Outcome>) {
        let mut event = None;
        {
            let mut g = self.inner.lock().unwrap();
            let shared = g.runs.remove(&id).map(|r| r.shared);
            g.speed.remove(&id);
            if let Some(it) = g.items.get_mut(&id) {
                if let Some(sh) = &shared {
                    let total = sh.total.load(Ordering::Relaxed);
                    if total > 0 {
                        it.size = Some(total);
                    }
                    it.downloaded = sh.downloaded();
                    it.resumable = sh.resumable.load(Ordering::Relaxed);
                    if let Some(info) = sh.info.lock().unwrap().as_ref() {
                        it.content_type = info.content_type.clone();
                    }
                }
                match res {
                    Ok(Outcome::Completed) => {
                        it.status = Status::Completed;
                        it.completed_at = Some(now_secs());
                        it.downloaded = it.size.unwrap_or(it.downloaded);
                        it.error = None;
                        event = Some(true);
                    }
                    Ok(Outcome::Paused) => {
                        if it.status != Status::Queued {
                            it.status = Status::Paused;
                        }
                    }
                    Ok(Outcome::Cancelled) => {}
                    Err(e) => {
                        it.status = Status::Error;
                        it.error = Some(describe(&e));
                        event = Some(false);
                    }
                }
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        if let Some(v) = self.get(id) {
            let _ = self.tx.send(Event::Updated(v.clone()));
            match event {
                Some(true) => {
                    let _ = self.tx.send(Event::Completed(v));
                }
                Some(false) => {
                    let _ = self.tx.send(Event::Failed(v));
                }
                None => {}
            }
        }
        self.pump();
    }

    // ───────────────────────── ticker & persistence ─────────────────────────

    fn tick(&self) {
        let views = {
            let mut g = self.inner.lock().unwrap();
            let ids: Vec<DownloadId> = g.runs.keys().copied().collect();
            if ids.is_empty() {
                return;
            }
            let now = Instant::now();
            for id in &ids {
                let (received, downloaded, total, resumable) = {
                    let r = &g.runs[id];
                    (
                        r.shared.received.load(Ordering::Relaxed),
                        r.shared.downloaded(),
                        r.shared.total.load(Ordering::Relaxed),
                        r.shared.resumable.load(Ordering::Relaxed),
                    )
                };
                let st = g.speed.entry(*id).or_insert(SpeedState { last_received: received, last_at: now, ema: 0.0 });
                let dt = now.duration_since(st.last_at).as_secs_f64();
                if dt >= 0.2 {
                    let inst = received.saturating_sub(st.last_received) as f64 / dt;
                    st.ema = if st.ema == 0.0 { inst } else { 0.35 * inst + 0.65 * st.ema };
                    st.last_received = received;
                    st.last_at = now;
                }
                if let Some(it) = g.items.get_mut(id) {
                    it.downloaded = downloaded;
                    if total > 0 {
                        it.size = Some(total);
                    }
                    it.resumable = resumable;
                }
            }
            ids.iter().filter_map(|id| view_of(&g, *id)).collect::<Vec<_>>()
        };
        let _ = self.tx.send(Event::Progress(views));
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn save_if_dirty(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let items: Vec<Item> = self.inner.lock().unwrap().items.values().cloned().collect();
        if let Err(e) = write_json(&self.data_dir.join("history.json"), &items) {
            eprintln!("idl: could not save history: {e}");
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
}

fn describe(e: &Error) -> String {
    match e {
        Error::Io(io) if io.kind() == std::io::ErrorKind::StorageFull => "The disk is full.".into(),
        Error::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied => "Permission denied writing the file.".into(),
        Error::BadStatus(401) | Error::BadStatus(403) => "Access denied (HTTP 403). The link may have expired or needs you to sign in.".into(),
        Error::BadStatus(404) | Error::BadStatus(410) => "File not found on the server (HTTP 404).".into(),
        other => other.to_string(),
    }
}

fn view_of(g: &Inner, id: DownloadId) -> Option<ItemView> {
    g.items.get(&id).map(|it| view(g, it))
}

fn view(g: &Inner, it: &Item) -> ItemView {
    let speed = g.speed.get(&it.id).map(|s| s.ema as u64).unwrap_or(0);
    let eta = match (it.size, speed) {
        (Some(sz), s) if s > 0 && it.status == Status::Downloading => Some(sz.saturating_sub(it.downloaded) / s),
        _ => None,
    };
    let connections = g.runs.get(&it.id).map(|r| r.shared.active_conns.load(Ordering::Relaxed)).unwrap_or(0);
    ItemView { item: it.clone(), speed, eta, connections }
}

/// Unique destination that also does not collide with another queued item.
fn reserve_path(g: &Inner, dir: &Path, name: &str) -> PathBuf {
    let mut candidate = unique_path(dir, name);
    let mut n = 0;
    while g.items.values().any(|i| i.dest == candidate) && n < 10_000 {
        n += 1;
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        candidate = unique_path(dir, &format!("{stem} ({n}){ext}"));
    }
    candidate
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // History holds request cookies: keep it private to the user.
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
