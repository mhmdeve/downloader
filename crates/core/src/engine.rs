//! Segmented, resumable HTTP(S) download of a single file.
//!
//! Design (IDM-like):
//! * The file is pre-allocated and split into segments; each connection owns one segment.
//! * **Dynamic splitting**: when a connection finishes, it halves the largest remaining
//!   segment of a busy connection, so all connections stay busy until the very end.
//! * Bytes are buffered per connection and written with positional writes; a segment's `pos`
//!   only advances after bytes are flushed to disk, so `pos` is always safe to resume from.
//! * Progress is persisted every second to `<file>.idl` (JSON); data lives in `<file>.part`
//!   and is renamed to `<file>` on completion.
//! * Retry with exponential backoff; `If-Range` protects against the remote file changing.

use crate::filename::sidecar_path;
use crate::limiter::Limiter;
use crate::net::apply_request;
use crate::probe::{parse_content_range_total, probe, ProbeInfo};
use crate::types::{RequestInfo, Segment};
use crate::{Error, Result};
use reqwest::header::{CONTENT_RANGE, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Don't open a connection for fewer than this many bytes.
const MIN_SEGMENT: u64 = 1 << 20;
/// A segment is only split if the remainder is at least twice this.
const MIN_SPLIT: u64 = 512 << 10;
const FLUSH_BYTES: usize = 256 << 10;
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Run,
    Pause,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Paused,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub request: RequestInfo,
    /// Final destination path (already made unique by the caller).
    pub dest: PathBuf,
    pub max_connections: u32,
}

/// One connection's byte range. `pos` is the next byte *not yet flushed to disk*.
struct SegCell {
    start: u64,
    pos: AtomicU64,
    end: AtomicU64,
    active: AtomicBool,
}

impl SegCell {
    fn new(s: Segment, active: bool) -> Arc<Self> {
        Arc::new(Self {
            start: s.start,
            pos: AtomicU64::new(s.pos),
            end: AtomicU64::new(s.end),
            active: AtomicBool::new(active),
        })
    }
    fn snapshot(&self) -> Segment {
        let end = self.end.load(Ordering::Acquire);
        Segment { start: self.start, pos: self.pos.load(Ordering::Acquire).min(end), end }
    }
    fn done(&self) -> bool {
        self.pos.load(Ordering::Acquire) >= self.end.load(Ordering::Acquire)
    }
}

/// State shared between the engine and whoever displays it (the manager / UI).
#[derive(Default)]
pub struct Shared {
    /// Total size (0 = unknown).
    pub total: AtomicU64,
    /// Network bytes received this session (monotonic; used for speed).
    pub received: AtomicU64,
    pub active_conns: AtomicU32,
    pub resumable: AtomicBool,
    pub info: Mutex<Option<ProbeInfo>>,
    cells: Mutex<Vec<Arc<SegCell>>>,
}

impl Shared {
    pub fn segments(&self) -> Vec<Segment> {
        self.cells.lock().unwrap().iter().map(|c| c.snapshot()).collect()
    }
    /// Bytes safely on disk.
    pub fn downloaded(&self) -> u64 {
        self.cells.lock().unwrap().iter().map(|c| {
            let s = c.snapshot();
            s.pos - s.start
        }).sum()
    }
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    version: u32,
    request: RequestInfo,
    info: ProbeInfo,
    segments: Vec<Segment>,
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

struct Ctx {
    client: Client,
    req: RequestInfo,
    final_url: String,
    use_original: AtomicBool,
    ranged: bool,
    if_range: Option<String>,
    file: Arc<File>,
    shared: Arc<Shared>,
    limiter: Arc<Limiter>,
    ctl: watch::Receiver<Signal>,
    max_conn: u32,
}

#[cfg(unix)]
fn write_all_at(f: &File, buf: &[u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
fn write_all_at(f: &File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

fn initial_segments(size: Option<u64>, ranged: bool, max_conn: u32) -> Vec<Segment> {
    match size {
        Some(sz) if ranged && sz > 0 => {
            let n = (sz / MIN_SEGMENT).clamp(1, max_conn.max(1) as u64);
            let chunk = sz.div_ceil(n);
            (0..n)
                .map(|i| {
                    let start = i * chunk;
                    Segment { start, pos: start, end: ((i + 1) * chunk).min(sz) }
                })
                .filter(|s| s.end > s.start)
                .collect()
        }
        Some(sz) => vec![Segment { start: 0, pos: 0, end: sz }],
        None => vec![Segment { start: 0, pos: 0, end: u64::MAX }],
    }
}

fn validators_match(a: &ProbeInfo, b: &ProbeInfo) -> bool {
    if a.size != b.size {
        return false;
    }
    if let (Some(x), Some(y)) = (&a.etag, &b.etag) {
        return x == y;
    }
    match (&a.last_modified, &b.last_modified) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

fn save_sidecar(path: &Path, req: &RequestInfo, info: &ProbeInfo, shared: &Shared) -> Result<()> {
    let sc = Sidecar { version: 1, request: req.clone(), info: info.clone(), segments: shared.segments() };
    let tmp = path.with_extension("idl.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&sc)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn load_sidecar(path: &Path) -> Option<Sidecar> {
    let sc: Sidecar = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (sc.version == 1).then_some(sc)
}

async fn wait_not_run(ctl: &mut watch::Receiver<Signal>) {
    loop {
        if *ctl.borrow() != Signal::Run {
            return;
        }
        if ctl.changed().await.is_err() {
            return;
        }
    }
}

/// Sleep for `d`, returning `false` early if pause/cancel was requested.
async fn interruptible_sleep(ctl: &mut watch::Receiver<Signal>, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        _ = wait_not_run(ctl) => false,
    }
}

/// Run (or resume) one download until it completes, is paused, or is cancelled.
pub async fn run(
    client: Client,
    job: Job,
    shared: Arc<Shared>,
    ctl: watch::Receiver<Signal>,
    limiter: Arc<Limiter>,
) -> Result<Outcome> {
    let req = job.request.clone();
    let info = probe(&client, &req).await?;
    *shared.info.lock().unwrap() = Some(info.clone());

    let part = part_path(&job.dest);
    let side = sidecar_path(&job.dest);
    let ranged = info.accepts_ranges && info.size.is_some();
    shared.resumable.store(ranged, Ordering::Relaxed);
    shared.total.store(info.size.unwrap_or(0), Ordering::Relaxed);

    // Resume only if the sidecar matches the server's current state.
    let saved = if ranged && part.exists() {
        load_sidecar(&side).filter(|sc| validators_match(&sc.info, &info)).map(|sc| sc.segments)
    } else {
        None
    };
    let fresh = saved.is_none();

    if let Some(dir) = job.dest.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&part)?;
    if fresh {
        file.set_len(0)?;
        if let Some(sz) = info.size {
            file.set_len(sz)?; // pre-allocate (sparse where the OS supports it)
        }
    }
    let file = Arc::new(file);

    let segs = saved.unwrap_or_else(|| initial_segments(info.size, ranged, job.max_connections));
    {
        let mut cells = shared.cells.lock().unwrap();
        *cells = segs.iter().map(|s| SegCell::new(*s, false)).collect();
    }

    // Empty file: nothing to transfer.
    if info.size == Some(0) {
        return finish(&job, &part, &side, &file).await.map(|_| Outcome::Completed);
    }

    let if_range = if ranged {
        match (&info.etag, &info.last_modified) {
            (Some(e), _) if !e.starts_with("W/") => Some(e.clone()),
            (_, Some(l)) => Some(l.clone()),
            _ => None,
        }
    } else {
        None
    };

    let ctx = Arc::new(Ctx {
        client,
        req: req.clone(),
        final_url: info.final_url.clone(),
        use_original: AtomicBool::new(false),
        ranged,
        if_range,
        file: file.clone(),
        shared: shared.clone(),
        limiter,
        ctl: ctl.clone(),
        max_conn: job.max_connections.max(1),
    });

    let workers = if ranged { ctx.max_conn } else { 1 };
    let mut set: JoinSet<Result<()>> = JoinSet::new();
    for _ in 0..workers {
        set.spawn(worker(ctx.clone()));
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut first_err: Option<Error> = None;
    loop {
        tokio::select! {
            res = set.join_next() => match res {
                None => break,
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(e))) => {
                    if first_err.is_none() { first_err = Some(e); }
                    set.abort_all();
                }
                Some(Err(je)) => {
                    if !je.is_cancelled() && first_err.is_none() {
                        first_err = Some(Error::Other(format!("worker crashed: {je}")));
                    }
                }
            },
            _ = ticker.tick() => {
                if ranged { let _ = save_sidecar(&side, &req, &info, &shared); }
            }
        }
    }

    if ranged {
        let _ = save_sidecar(&side, &req, &info, &shared);
    }
    if let Some(e) = first_err {
        return Err(e);
    }

    match *ctl.borrow() {
        Signal::Cancel => {
            drop(file);
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&side);
            return Ok(Outcome::Cancelled);
        }
        Signal::Pause => return Ok(Outcome::Paused),
        Signal::Run => {}
    }

    // Everything must be on disk.
    let cells = shared.cells.lock().unwrap().clone();
    if !cells.iter().all(|c| c.done()) {
        return Err(Error::Other("download ended with incomplete segments".into()));
    }
    if let Some(sz) = info.size {
        if shared.downloaded() != sz {
            return Err(Error::Other(format!("size mismatch: got {} of {} bytes", shared.downloaded(), sz)));
        }
    } else {
        let total = shared.downloaded();
        shared.total.store(total, Ordering::Relaxed);
        file.set_len(total)?;
    }
    finish(&job, &part, &side, &file).await?;
    Ok(Outcome::Completed)
}

async fn finish(job: &Job, part: &Path, side: &Path, file: &Arc<File>) -> Result<()> {
    let f = file.clone();
    tokio::task::spawn_blocking(move || f.sync_all()).await.map_err(|e| Error::Other(e.to_string()))??;
    let _ = std::fs::remove_file(&job.dest);
    std::fs::rename(part, &job.dest)?;
    let _ = std::fs::remove_file(side);
    Ok(())
}

/// Pick the next segment for a connection: an unclaimed one, or half of the busiest.
fn claim(ctx: &Ctx) -> Option<Arc<SegCell>> {
    let mut cells = ctx.shared.cells.lock().unwrap();
    for c in cells.iter() {
        if !c.done() && c.active.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return Some(c.clone());
        }
    }
    if !ctx.ranged {
        return None;
    }
    let victim = cells
        .iter()
        .filter(|c| c.active.load(Ordering::Acquire))
        .max_by_key(|c| c.end.load(Ordering::Acquire).saturating_sub(c.pos.load(Ordering::Acquire)))?
        .clone();
    let pos = victim.pos.load(Ordering::Acquire);
    let end = victim.end.load(Ordering::Acquire);
    let remaining = end.saturating_sub(pos);
    if end == u64::MAX || remaining < 2 * MIN_SPLIT {
        return None;
    }
    let mut mid = pos + remaining / 2;
    mid -= mid % 4096; // keep splits sector-aligned
    if mid <= pos + MIN_SPLIT / 2 || mid >= end {
        return None;
    }
    victim.end.store(mid, Ordering::Release);
    let cell = SegCell::new(Segment { start: mid, pos: mid, end }, true);
    cells.push(cell.clone());
    Some(cell)
}

async fn worker(ctx: Arc<Ctx>) -> Result<()> {
    let mut ctl = ctx.ctl.clone();
    loop {
        if *ctl.borrow() != Signal::Run {
            return Ok(());
        }
        let Some(cell) = claim(&ctx) else { return Ok(()) };
        ctx.shared.active_conns.fetch_add(1, Ordering::Relaxed);
        let r = run_segment(&ctx, &cell, &mut ctl).await;
        ctx.shared.active_conns.fetch_sub(1, Ordering::Relaxed);
        cell.active.store(false, Ordering::Release);
        r?;
    }
}

async fn run_segment(ctx: &Ctx, cell: &Arc<SegCell>, ctl: &mut watch::Receiver<Signal>) -> Result<()> {
    let mut attempts = 0u32;
    loop {
        if *ctl.borrow() != Signal::Run || cell.done() {
            return Ok(());
        }
        let before = cell.pos.load(Ordering::Acquire);
        match fetch_range(ctx, cell, ctl).await {
            Ok(()) => return Ok(()),
            Err(e) if e.is_retryable() && attempts < MAX_RETRIES => {
                if cell.pos.load(Ordering::Acquire) > before + (64 << 10) {
                    attempts = 0; // made real progress; reset the backoff
                }
                attempts += 1;
                if !ctx.ranged {
                    // No range support: the only option is to start over.
                    cell.pos.store(cell.start, Ordering::Release);
                }
                let delay = Duration::from_millis((500u64 << (attempts - 1).min(5)).min(15_000));
                if !interruptible_sleep(ctl, delay).await {
                    return Ok(());
                }
            }
            Err(Error::BadStatus(c)) if matches!(c, 401 | 403 | 404 | 410) && !ctx.use_original.swap(true, Ordering::AcqRel) && ctx.final_url != ctx.req.url => {
                // The redirect target may have expired; fall back to the original URL once.
            }
            Err(e) => return Err(e),
        }
    }
}

async fn flush(ctx: &Ctx, cell: &SegCell, buf: &mut Vec<u8>, buf_off: &mut u64) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let mut data = std::mem::take(buf);
    // The segment may have been split while we were buffering: never write past its end.
    let end = cell.end.load(Ordering::Acquire);
    data.truncate(end.saturating_sub(*buf_off).min(data.len() as u64) as usize);
    let n = data.len() as u64;
    let file = ctx.file.clone();
    let off = *buf_off;
    tokio::task::spawn_blocking(move || write_all_at(&file, &data, off))
        .await
        .map_err(|e| Error::Other(e.to_string()))??;
    *buf_off += n;
    cell.pos.store(*buf_off, Ordering::Release);
    buf.reserve(FLUSH_BYTES + 64 * 1024);
    Ok(())
}

async fn fetch_range(ctx: &Ctx, cell: &Arc<SegCell>, ctl: &mut watch::Receiver<Signal>) -> Result<()> {
    let start = cell.pos.load(Ordering::Acquire);
    let end = cell.end.load(Ordering::Acquire);
    if start >= end {
        return Ok(());
    }
    let url = if ctx.use_original.load(Ordering::Acquire) { &ctx.req.url } else { &ctx.final_url };
    let mut rb = apply_request(ctx.client.get(url), &ctx.req);
    if ctx.ranged {
        let last = if end == u64::MAX { String::new() } else { (end - 1).to_string() };
        rb = rb.header(RANGE, format!("bytes={start}-{last}"));
        if let Some(v) = &ctx.if_range {
            rb = rb.header(IF_RANGE, v);
        }
    }
    let mut resp = tokio::select! {
        r = rb.send() => r?,
        _ = wait_not_run(ctl) => return Ok(()),
    };
    let status = resp.status();
    if ctx.ranged {
        match status {
            StatusCode::PARTIAL_CONTENT => {
                let ok = resp
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes "))
                    .and_then(|v| v.split(['-', '/']).next())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    == Some(start);
                if !ok {
                    return Err(Error::Other("server returned an unexpected byte range".into()));
                }
                let _ = parse_content_range_total; // (total already validated by the probe)
            }
            StatusCode::OK => return Err(Error::RemoteChanged),
            s => return Err(Error::BadStatus(s.as_u16())),
        }
    } else if !status.is_success() {
        return Err(Error::BadStatus(status.as_u16()));
    }

    let mut buf: Vec<u8> = Vec::with_capacity(FLUSH_BYTES + 64 * 1024);
    let mut buf_off = start;
    let mut last_flush = Instant::now();

    loop {
        let chunk = tokio::select! {
            c = tokio::time::timeout(READ_TIMEOUT, resp.chunk()) => match c {
                Err(_) => { flush(ctx, cell, &mut buf, &mut buf_off).await?; return Err(Error::Timeout); }
                Ok(Err(e)) => { flush(ctx, cell, &mut buf, &mut buf_off).await?; return Err(e.into()); }
                Ok(Ok(c)) => c,
            },
            _ = wait_not_run(ctl) => {
                flush(ctx, cell, &mut buf, &mut buf_off).await?;
                return Ok(());
            }
        };

        let Some(bytes) = chunk else {
            // EOF
            flush(ctx, cell, &mut buf, &mut buf_off).await?;
            let end_now = cell.end.load(Ordering::Acquire);
            if end_now == u64::MAX {
                cell.end.store(buf_off, Ordering::Release); // unknown length: whatever we got is the file
                return Ok(());
            }
            return if buf_off >= end_now { Ok(()) } else { Err(Error::Truncated) };
        };

        // Speed limiter (interruptible).
        let wait = ctx.limiter.reserve(bytes.len());
        if !wait.is_zero() && !interruptible_sleep(ctl, wait).await {
            // Drop this chunk; it will be re-fetched on resume (pos only covers flushed bytes).
            flush(ctx, cell, &mut buf, &mut buf_off).await?;
            return Ok(());
        }

        let end_now = cell.end.load(Ordering::Acquire);
        let cursor = buf_off + buf.len() as u64;
        if cursor >= end_now {
            flush(ctx, cell, &mut buf, &mut buf_off).await?;
            return Ok(());
        }
        let take = ((end_now - cursor).min(bytes.len() as u64)) as usize;
        buf.extend_from_slice(&bytes[..take]);
        ctx.shared.received.fetch_add(take as u64, Ordering::Relaxed);

        let finished = take < bytes.len() || buf_off + buf.len() as u64 >= end_now;
        if finished || buf.len() >= FLUSH_BYTES || last_flush.elapsed() >= FLUSH_INTERVAL {
            flush(ctx, cell, &mut buf, &mut buf_off).await?;
            last_flush = Instant::now();
            if finished && cell.done() {
                return Ok(());
            }
        }
    }
}
