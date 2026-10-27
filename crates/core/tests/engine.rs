mod common;

use common::*;
use idl_core::engine::{self, Job, Outcome, Shared, Signal};
use idl_core::filename::sidecar_path;
use idl_core::limiter::Limiter;
use idl_core::net::{build_client, ClientOptions};
use idl_core::probe::probe;
use idl_core::types::RequestInfo;
use idl_core::Error;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("idl-test-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(addr: std::net::SocketAddr, dir: &std::path::Path, name: &str, conns: u32) -> Job {
    Job { request: RequestInfo::new(format!("http://{addr}/file/{name}")), dest: dir.join(name), max_connections: conns }
}

async fn run_to_end(job: Job, limiter: Arc<Limiter>) -> (idl_core::Result<Outcome>, Arc<Shared>) {
    let client = build_client(&ClientOptions::default()).unwrap();
    let shared = Arc::new(Shared::default());
    let (_tx, rx) = watch::channel(Signal::Run);
    let r = engine::run(client, job, shared.clone(), rx, limiter).await;
    (r, shared)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_reports_size_ranges_and_name() {
    let mut cfg = Cfg::new(12_345);
    cfg.disposition = Some("attachment; filename=\"report final.pdf\"".into());
    let (addr, _) = serve(cfg).await;
    let client = build_client(&ClientOptions::default()).unwrap();
    let info = probe(&client, &RequestInfo::new(format!("http://{addr}/x"))).await.unwrap();
    assert_eq!(info.size, Some(12_345));
    assert!(info.accepts_ranges);
    assert_eq!(info.filename, "report final.pdf");
    assert_eq!(info.etag.as_deref(), Some("\"v1\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_connection_download_is_correct() {
    let len = 8 * 1024 * 1024 + 123;
    let mut cfg = Cfg::new(len);
    cfg.piece_delay = Some(Duration::from_millis(2));
    let (addr, cfg) = serve(cfg).await;
    let dir = tmpdir("multi");
    let j = job(addr, &dir, "big.bin", 8);
    let dest = j.dest.clone();
    let (r, shared) = run_to_end(j, Arc::new(Limiter::default())).await;
    assert_eq!(r.unwrap(), Outcome::Completed);
    verify_file(&dest, len);
    assert!(!engine::part_path(&dest).exists());
    assert!(!sidecar_path(&dest).exists());
    assert!(cfg.peak.load(Ordering::SeqCst) >= 3, "expected parallel connections, peak={}", cfg.peak.load(Ordering::SeqCst));
    assert_eq!(shared.downloaded(), len);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dynamic_split_keeps_connections_busy() {
    // One huge initial segment (conns=1 initial split is impossible), so use a size where
    // initial segments are fewer than workers: 2 MiB => 2 segments, 6 workers.
    let len = 2 * 1024 * 1024;
    let mut cfg = Cfg::new(len);
    cfg.piece_delay = Some(Duration::from_millis(5));
    let (addr, cfg) = serve(cfg).await;
    let dir = tmpdir("split");
    let j = job(addr, &dir, "split.bin", 6);
    let dest = j.dest.clone();
    let (r, _) = run_to_end(j, Arc::new(Limiter::default())).await;
    assert_eq!(r.unwrap(), Outcome::Completed);
    verify_file(&dest, len);
    assert!(cfg.peak.load(Ordering::SeqCst) > 2, "idle workers should split busy segments, peak={}", cfg.peak.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_without_ranges_uses_one_connection() {
    let len = 3 * 1024 * 1024;
    let mut cfg = Cfg::new(len);
    cfg.ranges = false;
    let (addr, cfg) = serve(cfg).await;
    let dir = tmpdir("noranges");
    let j = job(addr, &dir, "a.bin", 8);
    let dest = j.dest.clone();
    let (r, shared) = run_to_end(j, Arc::new(Limiter::default())).await;
    assert_eq!(r.unwrap(), Outcome::Completed);
    verify_file(&dest, len);
    assert!(!shared.resumable.load(Ordering::Relaxed));
    // probe + 1 download connection at most concurrently
    assert!(cfg.peak.load(Ordering::SeqCst) <= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_length_stream_completes() {
    let len = 1024 * 1024 + 17;
    let mut cfg = Cfg::new(len);
    cfg.ranges = false;
    cfg.unknown_length = true;
    let (addr, _) = serve(cfg).await;
    let dir = tmpdir("unknown");
    let j = job(addr, &dir, "stream.bin", 4);
    let dest = j.dest.clone();
    let (r, shared) = run_to_end(j, Arc::new(Limiter::default())).await;
    assert_eq!(r.unwrap(), Outcome::Completed);
    verify_file(&dest, len);
    assert_eq!(shared.total.load(Ordering::Relaxed), len);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_file() {
    let (addr, _) = serve(Cfg::new(0)).await;
    let dir = tmpdir("empty");
    let j = job(addr, &dir, "empty.bin", 4);
    let dest = j.dest.clone();
    let (r, _) = run_to_end(j, Arc::new(Limiter::default())).await;
    assert_eq!(r.unwrap(), Outcome::Completed);
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_after_dropped_connection() {
    let len = 2 * 1024 * 1024;
    let mut cfg = Cfg::new(len);
    cfg.drop_first_after = Some(200 * 1024);
    let (addr, cfg) = serve(cfg).await;
    let dir = tmpdir("flaky");
    let j = job(addr, &dir, "flaky.bin", 1);
    let dest = j.dest.clone();
    let (r, _) = run_to_end(j, Arc::new(Limiter::default())).await;
    assert_eq!(r.unwrap(), Outcome::Completed);
    verify_file(&dest, len);
    let starts = cfg.range_starts.lock().unwrap().clone();
    assert!(starts.iter().any(|s| *s > 0), "expected a resumed range request, got {starts:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_status_is_reported() {
    let mut cfg = Cfg::new(10);
    cfg.status = Some(404);
    let (addr, _) = serve(cfg).await;
    let dir = tmpdir("404");
    let (r, _) = run_to_end(job(addr, &dir, "x.bin", 2), Arc::new(Limiter::default())).await;
    assert!(matches!(r, Err(Error::BadStatus(404))), "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_then_resume_continues_where_it_left_off() {
    let len = 6 * 1024 * 1024;
    let (addr, cfg) = serve(Cfg::new(len)).await;
    let dir = tmpdir("pause");
    let j = job(addr, &dir, "p.bin", 4);
    let dest = j.dest.clone();
    let limiter = Arc::new(Limiter::new(2 * 1024 * 1024)); // ~3 s total
    let client = build_client(&ClientOptions::default()).unwrap();

    let shared1 = Arc::new(Shared::default());
    let (tx, rx) = watch::channel(Signal::Run);
    let h = tokio::spawn(engine::run(client.clone(), j.clone(), shared1.clone(), rx, limiter.clone()));
    tokio::time::sleep(Duration::from_millis(900)).await;
    tx.send(Signal::Pause).unwrap();
    let out = h.await.unwrap().unwrap();
    assert_eq!(out, Outcome::Paused);
    let part = engine::part_path(&dest);
    assert!(part.exists() && sidecar_path(&dest).exists(), "state must be kept on pause");
    let got = shared1.downloaded();
    assert!(got > 0 && got < len, "downloaded {got}");

    // Resume with a fresh controller and no throttle.
    cfg.range_starts.lock().unwrap().clear();
    let shared2 = Arc::new(Shared::default());
    let (_tx2, rx2) = watch::channel(Signal::Run);
    let out = engine::run(client, j, shared2.clone(), rx2, Arc::new(Limiter::default())).await.unwrap();
    assert_eq!(out, Outcome::Completed);
    verify_file(&dest, len);
    let again = shared2.received.load(Ordering::Relaxed);
    assert!(again < len, "resume must not re-download everything (received {again} of {len}, had {got})");
    assert!(!sidecar_path(&dest).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_change_restarts_from_scratch() {
    let len = 4 * 1024 * 1024;
    let (addr, cfg) = serve(Cfg::new(len)).await;
    let dir = tmpdir("changed");
    let j = job(addr, &dir, "c.bin", 4);
    let dest = j.dest.clone();
    let client = build_client(&ClientOptions::default()).unwrap();
    let (tx, rx) = watch::channel(Signal::Run);
    let h = tokio::spawn(engine::run(client.clone(), j.clone(), Arc::new(Shared::default()), rx, Arc::new(Limiter::new(2 * 1024 * 1024))));
    tokio::time::sleep(Duration::from_millis(700)).await;
    tx.send(Signal::Pause).unwrap();
    assert_eq!(h.await.unwrap().unwrap(), Outcome::Paused);

    *cfg.etag.lock().unwrap() = "\"v2\"".into(); // the file changed on the server
    let shared = Arc::new(Shared::default());
    let (_t, rx) = watch::channel(Signal::Run);
    assert_eq!(engine::run(client, j, shared.clone(), rx, Arc::new(Limiter::default())).await.unwrap(), Outcome::Completed);
    verify_file(&dest, len);
    assert!(shared.received.load(Ordering::Relaxed) >= len, "must have restarted from byte 0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_removes_partial_files() {
    let len = 6 * 1024 * 1024;
    let (addr, _) = serve(Cfg::new(len)).await;
    let dir = tmpdir("cancel");
    let j = job(addr, &dir, "x.bin", 4);
    let dest = j.dest.clone();
    let client = build_client(&ClientOptions::default()).unwrap();
    let (tx, rx) = watch::channel(Signal::Run);
    let h = tokio::spawn(engine::run(client, j, Arc::new(Shared::default()), rx, Arc::new(Limiter::new(1024 * 1024))));
    tokio::time::sleep(Duration::from_millis(500)).await;
    tx.send(Signal::Cancel).unwrap();
    assert_eq!(h.await.unwrap().unwrap(), Outcome::Cancelled);
    assert!(!engine::part_path(&dest).exists());
    assert!(!sidecar_path(&dest).exists());
    assert!(!dest.exists());
}
