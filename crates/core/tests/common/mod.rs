//! A tiny scriptable HTTP/1.1 server for engine tests (no extra dependencies).
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Cfg {
    pub len: u64,
    pub ranges: bool,
    pub etag: Mutex<String>,
    /// Abruptly close the *first* body after this many bytes.
    pub drop_first_after: Option<usize>,
    /// No Content-Length; body ends when the connection closes.
    pub unknown_length: bool,
    pub disposition: Option<String>,
    pub status: Option<u16>,
    pub piece_delay: Option<Duration>,
    pub peak: AtomicUsize,
    pub cur: AtomicUsize,
    pub range_starts: Mutex<Vec<u64>>,
    pub dropped: AtomicBool,
}

impl Cfg {
    pub fn new(len: u64) -> Self {
        Self {
            len,
            ranges: true,
            etag: Mutex::new("\"v1\"".into()),
            drop_first_after: None,
            unknown_length: false,
            disposition: None,
            status: None,
            piece_delay: None,
            peak: AtomicUsize::new(0),
            cur: AtomicUsize::new(0),
            range_starts: Mutex::new(vec![]),
            dropped: AtomicBool::new(false),
        }
    }
}

/// Deterministic content so we never have to keep the file in memory.
pub fn byte(i: u64) -> u8 {
    (i.wrapping_mul(2654435761) >> 13) as u8
}

pub fn verify_file(path: &std::path::Path, len: u64) {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    assert_eq!(data.len() as u64, len, "length mismatch");
    for (i, b) in data.iter().enumerate() {
        assert_eq!(*b, byte(i as u64), "content mismatch at byte {i}");
    }
}

pub async fn serve(cfg: Cfg) -> (SocketAddr, Arc<Cfg>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = Arc::new(cfg);
    let c2 = cfg.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { break };
            let cfg = c2.clone();
            tokio::spawn(async move {
                let _ = handle(sock, cfg).await;
            });
        }
    });
    (addr, cfg)
}

async fn handle(mut s: TcpStream, cfg: Arc<Cfg>) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut tmp = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&tmp[..n]);
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split("\r\n");
    let _req_line = lines.next().unwrap_or("");
    let mut h: HashMap<String, String> = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            h.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    if let Some(code) = cfg.status {
        let body = b"nope";
        let r = format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        s.write_all(r.as_bytes()).await?;
        s.write_all(body).await?;
        return Ok(());
    }

    let cur = cfg.cur.fetch_add(1, Ordering::SeqCst) + 1;
    cfg.peak.fetch_max(cur, Ordering::SeqCst);
    let res = respond(&mut s, &cfg, &h).await;
    cfg.cur.fetch_sub(1, Ordering::SeqCst);
    res
}

async fn respond(s: &mut TcpStream, cfg: &Arc<Cfg>, h: &HashMap<String, String>) -> std::io::Result<()> {
    let etag = cfg.etag.lock().unwrap().clone();
    let mut range: Option<(u64, u64)> = None;
    if cfg.ranges {
        if let Some(r) = h.get("range").and_then(|r| r.strip_prefix("bytes=")) {
            let (a, b) = r.split_once('-').unwrap_or((r, ""));
            let start: u64 = a.parse().unwrap_or(0);
            let end: u64 = b.parse().unwrap_or(cfg.len.saturating_sub(1)).min(cfg.len.saturating_sub(1));
            let if_range_ok = h.get("if-range").map(|v| *v == etag).unwrap_or(true);
            if if_range_ok {
                if start >= cfg.len {
                    let r = format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        cfg.len
                    );
                    return s.write_all(r.as_bytes()).await;
                }
                range = Some((start, end));
            }
        }
    }

    let mut head = String::new();
    let (start, end) = match range {
        Some((a, b)) => {
            cfg.range_starts.lock().unwrap().push(a);
            head.push_str("HTTP/1.1 206 Partial Content\r\n");
            head.push_str(&format!("Content-Range: bytes {a}-{b}/{}\r\n", cfg.len));
            head.push_str(&format!("Content-Length: {}\r\n", b - a + 1));
            (a, b)
        }
        None => {
            head.push_str("HTTP/1.1 200 OK\r\n");
            if !cfg.unknown_length {
                head.push_str(&format!("Content-Length: {}\r\n", cfg.len));
            }
            (0, cfg.len.saturating_sub(1))
        }
    };
    if cfg.ranges {
        head.push_str("Accept-Ranges: bytes\r\n");
    }
    head.push_str(&format!("ETag: {etag}\r\n"));
    head.push_str("Content-Type: application/octet-stream\r\n");
    if let Some(d) = &cfg.disposition {
        head.push_str(&format!("Content-Disposition: {d}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    s.write_all(head.as_bytes()).await?;

    if cfg.len == 0 {
        return Ok(());
    }
    let body_len = (end - start + 1) as usize;
    // Only a body larger than the limit can be cut (the probe's 1-byte range must not consume the drop).
    let first_drop = cfg.drop_first_after.filter(|l| body_len > *l && !cfg.dropped.swap(true, Ordering::SeqCst));
    let mut sent = 0usize;
    let mut i = start;
    while i <= end {
        let n = ((end - i + 1) as usize).min(32 * 1024);
        let piece: Vec<u8> = (i..i + n as u64).map(byte).collect();
        s.write_all(&piece).await?;
        i += n as u64;
        sent += n;
        if let Some(limit) = first_drop {
            if sent >= limit {
                return Ok(()); // close abruptly mid-body
            }
        }
        if let Some(d) = cfg.piece_delay {
            tokio::time::sleep(d).await;
        }
    }
    s.shutdown().await.ok();
    Ok(())
}

// ───────────── static-route server (used by the HLS tests) ─────────────

pub type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// Serve fixed bodies by path. Unknown path => 404. Optional per-response delay.
pub async fn serve_routes(routes: Routes, delay: Option<Duration>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { break };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut tmp = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&tmp[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/").to_string();
                let body = routes.lock().unwrap().get(&path).cloned();
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                let ctype = if path.ends_with(".m3u8") { "application/vnd.apple.mpegurl" } else { "application/octet-stream" };
                let _ = match body {
                    Some(b) => {
                        let h = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                        let _ = s.write_all(h.as_bytes()).await;
                        s.write_all(&b).await
                    }
                    None => s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await,
                };
                let _ = s.shutdown().await;
            });
        }
    });
    addr
}

pub fn new_routes() -> Routes {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Deterministic segment payload.
pub fn seg_bytes(seg: u64, len: usize) -> Vec<u8> {
    (0..len as u64).map(|i| byte(i + seg * 1_000_003)).collect()
}
