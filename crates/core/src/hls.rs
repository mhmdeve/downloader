//! HLS (`.m3u8`) support: playlist parsing + a resumable segment downloader.
//!
//! Supported: master playlists (best variant is chosen), VOD media playlists, AES-128 (CBC)
//! encryption, fMP4 (`#EXT-X-MAP`), byte-range segments.
//! Not supported: live streams, SAMPLE-AES / DRM, separate audio renditions.
//!
//! Segments are stored in `<dest>.hls/NNNNNN.seg` so an interrupted download resumes at segment
//! granularity; at the end they are concatenated. If `ffmpeg` is on PATH and `remux` is enabled,
//! MPEG-TS output is remuxed to MP4 (stream copy, no re-encode).

use crate::engine::{interruptible_sleep, wait_not_run, Job, Outcome, Shared, Signal};
use crate::limiter::Limiter;
use crate::net::apply_request;
use crate::types::RequestInfo;
use crate::{Error, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinSet;
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub resolution: Option<String>,
    pub codecs: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    pub uri: String,
    pub iv: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HlsSegment {
    pub url: String,
    pub duration: f64,
    pub seq: u64,
    pub key: Option<Key>,
    /// (length, offset)
    pub range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaPlaylist {
    pub segments: Vec<HlsSegment>,
    /// fMP4 initialisation segment: (url, range).
    pub init: Option<(String, Option<(u64, u64)>)>,
    pub endlist: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Playlist {
    Master(Vec<Variant>),
    Media(MediaPlaylist),
}

/// Summary shown in the "Download File Info" dialog.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HlsInfo {
    pub variants: Vec<Variant>,
    pub segments: usize,
    pub duration: f64,
    pub encrypted: bool,
    pub live: bool,
}

pub fn is_hls_url(url: &str) -> bool {
    Url::parse(url).map(|u| u.path().to_ascii_lowercase().ends_with(".m3u8")).unwrap_or(false)
}

pub fn is_hls_mime(mime: &str) -> bool {
    let m = mime.to_ascii_lowercase();
    m.contains("mpegurl")
}

/// Split `KEY=VALUE,KEY="quoted, value"` attribute lists.
pub fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut key = String::new();
    let mut val = String::new();
    let mut in_key = true;
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => in_q = !in_q,
            '=' if in_key && !in_q => in_key = false,
            ',' if !in_q => {
                if !key.trim().is_empty() {
                    out.push((key.trim().to_ascii_uppercase(), val.trim().to_string()));
                }
                key.clear();
                val.clear();
                in_key = true;
            }
            c => {
                if in_key {
                    key.push(c)
                } else {
                    val.push(c)
                }
            }
        }
    }
    if !key.trim().is_empty() {
        out.push((key.trim().to_ascii_uppercase(), val.trim().to_string()));
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

fn parse_hex_iv(s: &str) -> Option<[u8; 16]> {
    let h = s.trim_start_matches("0x").trim_start_matches("0X");
    if h.is_empty() || h.len() > 32 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let padded = format!("{h:0>32}");
    let mut iv = [0u8; 16];
    for i in 0..16 {
        iv[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

fn parse_byterange(s: &str, prev_end: Option<u64>) -> Option<(u64, u64)> {
    let (len, off) = match s.split_once('@') {
        Some((l, o)) => (l.trim().parse().ok()?, o.trim().parse().ok()?),
        None => (s.trim().parse().ok()?, prev_end?),
    };
    Some((len, off))
}

fn resolve(base: &Url, r: &str) -> Result<String> {
    base.join(r.trim()).map(|u| u.to_string()).map_err(|e| Error::Other(format!("bad playlist URI '{r}': {e}")))
}

pub fn parse(text: &str, base: &Url) -> Result<Playlist> {
    let text = text.trim_start_matches('\u{feff}');
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err(Error::Other("not an HLS playlist (missing #EXTM3U)".into()));
    }
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    if lines.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF")) {
        let mut variants = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if let Some(rest) = lines[i].strip_prefix("#EXT-X-STREAM-INF:") {
                let a = parse_attrs(rest);
                if let Some(uri) = lines[i + 1..].iter().find(|l| !l.starts_with('#')) {
                    variants.push(Variant {
                        url: resolve(base, uri)?,
                        bandwidth: attr(&a, "BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0),
                        resolution: attr(&a, "RESOLUTION").map(String::from),
                        codecs: attr(&a, "CODECS").map(String::from),
                        name: attr(&a, "NAME").map(String::from),
                    });
                }
            }
            i += 1;
        }
        variants.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
        return Ok(Playlist::Master(variants));
    }

    let mut seq: u64 = 0;
    let mut key: Option<Key> = None;
    let mut init = None;
    let mut endlist = false;
    let mut pending_dur = 0.0;
    let mut pending_range: Option<(u64, u64)> = None;
    let mut last_range_end: Option<u64> = None;
    let mut segments = Vec::new();

    for l in lines {
        if let Some(v) = l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("#EXT-X-KEY:") {
            let a = parse_attrs(v);
            match attr(&a, "METHOD").unwrap_or("NONE") {
                "NONE" => key = None,
                "AES-128" => {
                    let uri = attr(&a, "URI").ok_or_else(|| Error::Other("AES-128 key without URI".into()))?;
                    key = Some(Key { uri: resolve(base, uri)?, iv: attr(&a, "IV").and_then(parse_hex_iv) });
                }
                other => return Err(Error::Other(format!("unsupported HLS encryption: {other} (DRM-protected streams cannot be downloaded)"))),
            }
        } else if let Some(v) = l.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attrs(v);
            if let Some(uri) = attr(&a, "URI") {
                let range = attr(&a, "BYTERANGE").and_then(|r| parse_byterange(r, None));
                init = Some((resolve(base, uri)?, range));
            }
        } else if let Some(v) = l.strip_prefix("#EXTINF:") {
            pending_dur = v.split(',').next().and_then(|d| d.trim().parse().ok()).unwrap_or(0.0);
        } else if let Some(v) = l.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = parse_byterange(v, last_range_end);
        } else if l == "#EXT-X-ENDLIST" {
            endlist = true;
        } else if !l.starts_with('#') {
            if let Some((len, off)) = pending_range {
                last_range_end = Some(off + len);
            }
            segments.push(HlsSegment {
                url: resolve(base, l)?,
                duration: pending_dur,
                seq,
                key: key.clone(),
                range: pending_range.take(),
            });
            seq += 1;
            pending_dur = 0.0;
        }
    }
    Ok(Playlist::Media(MediaPlaylist { segments, init, endlist }))
}

/// Output extension implied by the segments.
pub fn output_extension(p: &MediaPlaylist) -> &'static str {
    if p.init.is_some() {
        return "mp4";
    }
    let first = p.segments.first().map(|s| s.url.to_ascii_lowercase()).unwrap_or_default();
    let path = first.split(['?', '#']).next().unwrap_or("");
    if path.ends_with(".m4s") || path.ends_with(".mp4") || path.ends_with(".cmfv") || path.ends_with(".cmfa") {
        "mp4"
    } else if path.ends_with(".aac") {
        "aac"
    } else if path.ends_with(".mp3") {
        "mp3"
    } else {
        "ts"
    }
}

fn with_url(req: &RequestInfo, url: &str) -> RequestInfo {
    RequestInfo { url: url.to_string(), ..req.clone() }
}

async fn fetch_text(client: &Client, req: &RequestInfo) -> Result<(String, Url)> {
    let resp = apply_request(client.get(&req.url), req).send().await?;
    if !resp.status().is_success() {
        return Err(Error::BadStatus(resp.status().as_u16()));
    }
    let final_url = resp.url().clone();
    Ok((resp.text().await?, final_url))
}

/// Resolve to a media playlist, following a master playlist to its best variant.
pub async fn load_media(client: &Client, req: &RequestInfo) -> Result<(MediaPlaylist, RequestInfo)> {
    let mut req = req.clone();
    for _ in 0..3 {
        let (text, base) = fetch_text(client, &req).await?;
        match parse(&text, &base)? {
            Playlist::Media(m) => return Ok((m, req)),
            Playlist::Master(vs) => {
                let best = vs.first().ok_or_else(|| Error::Other("master playlist has no variants".into()))?;
                req = with_url(&req, &best.url);
            }
        }
    }
    Err(Error::Other("playlist nesting too deep".into()))
}

/// For the info dialog.
pub async fn inspect(client: &Client, req: &RequestInfo) -> Result<HlsInfo> {
    let (text, base) = fetch_text(client, req).await?;
    Ok(match parse(&text, &base)? {
        Playlist::Master(variants) => HlsInfo { variants, segments: 0, duration: 0.0, encrypted: false, live: false },
        Playlist::Media(m) => HlsInfo {
            segments: m.segments.len(),
            duration: m.segments.iter().map(|s| s.duration).sum(),
            encrypted: m.segments.iter().any(|s| s.key.is_some()),
            live: !m.endlist,
            variants: vec![],
        },
    })
}

pub fn work_dir(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".hls");
    PathBuf::from(s)
}

#[derive(Serialize, Deserialize)]
struct WorkMeta {
    playlist: String,
    segments: usize,
}

fn decrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec::<Pkcs7>(data)
        .map_err(|_| Error::Other("could not decrypt segment (bad key or padding)".into()))
}

struct HCtx {
    client: Client,
    req: RequestInfo,
    segments: Vec<HlsSegment>,
    dir: PathBuf,
    next: AtomicUsize,
    keys: Mutex<HashMap<String, [u8; 16]>>,
    shared: Arc<Shared>,
    limiter: Arc<Limiter>,
    ctl: watch::Receiver<Signal>,
    done_segments: AtomicUsize,
}

async fn fetch_bytes(ctx: &HCtx, url: &str, range: Option<(u64, u64)>, ctl: &mut watch::Receiver<Signal>) -> Result<Option<Vec<u8>>> {
    let mut attempt = 0;
    loop {
        let mut rb = apply_request(ctx.client.get(url), &with_url(&ctx.req, url));
        if let Some((len, off)) = range {
            rb = rb.header(reqwest::header::RANGE, format!("bytes={off}-{}", off + len - 1));
        }
        let r: Result<Option<Vec<u8>>> = async {
            let mut resp = tokio::select! { r = rb.send() => r?, _ = wait_not_run(ctl) => return Ok(None) };
            if !resp.status().is_success() {
                return Err(Error::BadStatus(resp.status().as_u16()));
            }
            let mut data = Vec::new();
            loop {
                let chunk = tokio::select! {
                    c = tokio::time::timeout(Duration::from_secs(30), resp.chunk()) => c.map_err(|_| Error::Timeout)??,
                    _ = wait_not_run(ctl) => return Ok(None),
                };
                let Some(b) = chunk else { break };
                let wait = ctx.limiter.reserve(b.len());
                if !wait.is_zero() && !interruptible_sleep(ctl, wait).await {
                    return Ok(None);
                }
                ctx.shared.received.fetch_add(b.len() as u64, Ordering::Relaxed);
                data.extend_from_slice(&b);
            }
            Ok(Some(data))
        }
        .await;
        match r {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < 5 => {
                attempt += 1;
                if !interruptible_sleep(ctl, Duration::from_millis(400 << attempt.min(4))).await {
                    return Ok(None);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

async fn key_for(ctx: &HCtx, k: &Key, ctl: &mut watch::Receiver<Signal>) -> Result<Option<[u8; 16]>> {
    if let Some(v) = ctx.keys.lock().unwrap().get(&k.uri) {
        return Ok(Some(*v));
    }
    let Some(bytes) = fetch_bytes(ctx, &k.uri, None, ctl).await? else { return Ok(None) };
    let arr: [u8; 16] = bytes.as_slice().try_into().map_err(|_| Error::Other("HLS key is not 16 bytes (unsupported key server?)".into()))?;
    ctx.keys.lock().unwrap().insert(k.uri.clone(), arr);
    Ok(Some(arr))
}

async fn hls_worker(ctx: Arc<HCtx>) -> Result<()> {
    let mut ctl = ctx.ctl.clone();
    let total = ctx.segments.len();
    loop {
        if *ctl.borrow() != Signal::Run {
            return Ok(());
        }
        let i = ctx.next.fetch_add(1, Ordering::Relaxed);
        if i >= total {
            return Ok(());
        }
        let path = ctx.dir.join(format!("{i:06}.seg"));
        if path.exists() {
            continue; // finished in a previous run (already counted)
        }
        ctx.shared.active_conns.fetch_add(1, Ordering::Relaxed);
        let res = download_segment(&ctx, i, &path, &mut ctl).await;
        ctx.shared.active_conns.fetch_sub(1, Ordering::Relaxed);
        res?;
    }
}

async fn download_segment(ctx: &HCtx, i: usize, path: &Path, ctl: &mut watch::Receiver<Signal>) -> Result<()> {
    let seg = &ctx.segments[i];
    let Some(mut data) = fetch_bytes(ctx, &seg.url, seg.range, ctl).await? else { return Ok(()) };
    if let Some(k) = &seg.key {
        let Some(key) = key_for(ctx, k, ctl).await? else { return Ok(()) };
        let iv = k.iv.unwrap_or_else(|| {
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&seg.seq.to_be_bytes());
            iv
        });
        data = decrypt(&data, &key, &iv)?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, &data).await?;
    tokio::fs::rename(&tmp, path).await?;
    let done = ctx.done_segments.fetch_add(1, Ordering::Relaxed) + 1;
    let bytes = ctx.shared.hls_done.fetch_add(data.len() as u64, Ordering::Relaxed) + data.len() as u64;
    // Estimate the final size from the average segment size so far.
    ctx.shared.total.store(bytes / done as u64 * ctx.segments.len() as u64, Ordering::Relaxed);
    Ok(())
}

/// Run (or resume) an HLS download. Returns the outcome; the final path (extension may differ
/// from `job.dest`) is published in `shared.final_dest`.
pub async fn run(
    client: Client,
    job: Job,
    shared: Arc<Shared>,
    ctl: watch::Receiver<Signal>,
    limiter: Arc<Limiter>,
    remux: bool,
) -> Result<Outcome> {
    let (playlist, media_req) = load_media(&client, &job.request).await?;
    if !playlist.endlist {
        return Err(Error::Other("live HLS streams are not supported".into()));
    }
    if playlist.segments.is_empty() {
        return Err(Error::Other("the playlist contains no segments".into()));
    }
    let dir = work_dir(&job.dest);
    let meta_path = dir.join("meta.json");
    let meta = WorkMeta { playlist: media_req.url.clone(), segments: playlist.segments.len() };
    // Different playlist than last time => start over.
    if let Ok(old) = std::fs::read(&meta_path).map(|b| serde_json::from_slice::<WorkMeta>(&b)) {
        if !matches!(&old, Ok(o) if o.segments == meta.segments) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&meta_path, serde_json::to_vec(&meta)?)?;

    shared.resumable.store(true, Ordering::Relaxed);
    // Count what earlier runs already stored.
    let mut have = 0usize;
    let mut bytes = 0u64;
    for i in 0..playlist.segments.len() {
        if let Ok(m) = std::fs::metadata(dir.join(format!("{i:06}.seg"))) {
            have += 1;
            bytes += m.len();
        }
    }
    shared.hls_done.store(bytes, Ordering::Relaxed);
    if have > 0 {
        shared.total.store(bytes / have as u64 * playlist.segments.len() as u64, Ordering::Relaxed);
    }

    let ctx = Arc::new(HCtx {
        client: client.clone(),
        req: media_req.clone(),
        segments: playlist.segments.clone(),
        dir: dir.clone(),
        next: AtomicUsize::new(0),
        keys: Mutex::new(HashMap::new()),
        shared: shared.clone(),
        limiter,
        ctl: ctl.clone(),
        done_segments: AtomicUsize::new(have),
    });

    // fMP4 init segment first (small; needed for the final file).
    if let Some((url, range)) = &playlist.init {
        let init_path = dir.join("init.seg");
        if !init_path.exists() {
            let mut c = ctl.clone();
            if let Some(d) = fetch_bytes(&ctx, url, *range, &mut c).await? {
                std::fs::write(&init_path, d)?;
            }
        }
    }

    let workers = job.max_connections.clamp(1, 16);
    let mut set: JoinSet<Result<()>> = JoinSet::new();
    for _ in 0..workers {
        set.spawn(hls_worker(ctx.clone()));
    }
    let mut first_err = None;
    while let Some(r) = set.join_next().await {
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                first_err.get_or_insert(e);
                set.abort_all();
            }
            Err(_) => {}
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    match *ctl.borrow() {
        Signal::Cancel => {
            let _ = std::fs::remove_dir_all(&dir);
            return Ok(Outcome::Cancelled);
        }
        Signal::Pause => return Ok(Outcome::Paused),
        Signal::Run => {}
    }
    if (0..playlist.segments.len()).any(|i| !dir.join(format!("{i:06}.seg")).exists()) {
        return Err(Error::Other("some segments are missing".into()));
    }

    // Concatenate.
    let ext = output_extension(&playlist);
    let mut out_path = job.dest.with_extension(ext);
    if out_path != job.dest {
        // keep the user's chosen stem, only fix the extension
        out_path = job.dest.with_extension(ext);
    }
    let (d, o, n, has_init) = (dir.clone(), out_path.clone(), playlist.segments.len(), playlist.init.is_some());
    tokio::task::spawn_blocking(move || concat(&d, &o, n, has_init)).await.map_err(|e| Error::Other(e.to_string()))??;
    let _ = std::fs::remove_dir_all(&dir);

    if remux && ext == "ts" {
        if let Some(mp4) = remux_to_mp4(&out_path).await {
            out_path = mp4;
        }
    }
    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    shared.total.store(size, Ordering::Relaxed);
    shared.hls_done.store(size, Ordering::Relaxed);
    *shared.final_dest.lock().unwrap() = Some(out_path);
    Ok(Outcome::Completed)
}

fn concat(dir: &Path, out: &Path, n: usize, has_init: bool) -> Result<()> {
    use std::io::{copy, BufWriter, Write};
    let tmp = out.with_extension("idlpart");
    {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
        if has_init {
            copy(&mut std::fs::File::open(dir.join("init.seg"))?, &mut w)?;
        }
        for i in 0..n {
            copy(&mut std::fs::File::open(dir.join(format!("{i:06}.seg")))?, &mut w)?;
        }
        w.flush()?;
        w.into_inner().map_err(|e| Error::Other(e.to_string()))?.sync_all()?;
    }
    let _ = std::fs::remove_file(out);
    std::fs::rename(&tmp, out)?;
    Ok(())
}

/// Stream-copy a `.ts` file into `.mp4` with the system `ffmpeg`, if there is one.
async fn remux_to_mp4(ts: &Path) -> Option<PathBuf> {
    let mp4 = ts.with_extension("mp4");
    let tmp = ts.with_extension("remux.mp4");
    let (ts_s, tmp_s) = (ts.to_path_buf(), tmp.clone());
    let ok = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args(["-nostdin", "-y", "-loglevel", "error", "-i"])
            .arg(&ts_s)
            .args(["-c", "copy", "-bsf:a", "aac_adtstoasc", "-movflags", "+faststart", "-f", "mp4"])
            .arg(&tmp_s)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        cmd.status().map(|s| s.success()).unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if ok && std::fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false) {
        let _ = std::fs::remove_file(&mp4);
        if std::fs::rename(&tmp, &mp4).is_ok() {
            let _ = std::fs::remove_file(ts);
            return Some(mp4);
        }
    }
    let _ = std::fs::remove_file(&tmp);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/video/master.m3u8").unwrap()
    }

    #[test]
    fn attrs() {
        let a = parse_attrs(r#"BANDWIDTH=800000,CODECS="avc1.4d401f,mp4a.40.2",RESOLUTION=640x360"#);
        assert_eq!(attr(&a, "CODECS"), Some("avc1.4d401f,mp4a.40.2"));
        assert_eq!(attr(&a, "BANDWIDTH"), Some("800000"));
        assert_eq!(attr(&a, "RESOLUTION"), Some("640x360"));
    }

    #[test]
    fn master_sorted_best_first() {
        let t = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n360/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\n/abs/720.m3u8\n";
        let Playlist::Master(v) = parse(t, &base()).unwrap() else { panic!() };
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].bandwidth, 2_800_000);
        assert_eq!(v[0].url, "https://cdn.example.com/abs/720.m3u8");
        assert_eq!(v[1].url, "https://cdn.example.com/video/360/index.m3u8");
    }

    #[test]
    fn media_with_key_map_and_ranges() {
        let t = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0x00000000000000000000000000000001\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\na.m4s\n#EXTINF:3.5,\n#EXT-X-BYTERANGE:100@50\nb.m4s\n#EXTINF:2,\n#EXT-X-BYTERANGE:40\nb.m4s\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(t, &base()).unwrap() else { panic!() };
        assert!(m.endlist);
        assert_eq!(m.init.as_ref().unwrap().0, "https://cdn.example.com/video/init.mp4");
        assert_eq!(m.segments.len(), 3);
        assert_eq!(m.segments[0].seq, 7);
        assert_eq!(m.segments[2].seq, 9);
        assert_eq!(m.segments[0].key.as_ref().unwrap().iv.unwrap()[15], 1);
        assert_eq!(m.segments[1].range, Some((100, 50)));
        assert_eq!(m.segments[2].range, Some((40, 150))); // continues after the previous range
        assert_eq!(output_extension(&m), "mp4");
    }

    #[test]
    fn rejects_drm_and_non_playlists() {
        let t = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:4,\na.ts\n";
        assert!(parse(t, &base()).is_err());
        assert!(parse("<html>", &base()).is_err());
    }

    #[test]
    fn ts_extension_default_and_url_detection() {
        let t = "#EXTM3U\n#EXTINF:4,\nseg0.ts?token=1\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(t, &base()).unwrap() else { panic!() };
        assert_eq!(output_extension(&m), "ts");
        assert!(is_hls_url("https://a/b/c.M3U8?x=1"));
        assert!(!is_hls_url("https://a/b/c.mp4"));
        assert!(is_hls_mime("application/vnd.apple.mpegurl"));
    }

    #[test]
    fn aes_roundtrip() {
        use cbc::cipher::BlockModeEncrypt;
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let plain = b"hello hls world, this is more than one block of data!".to_vec();
        let enc = cbc::Encryptor::<aes::Aes128>::new((&key).into(), (&iv).into()).encrypt_padded_vec::<Pkcs7>(&plain);
        assert_eq!(decrypt(&enc, &key, &iv).unwrap(), plain);
    }
}
