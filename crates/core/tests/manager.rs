mod common;

use common::*;
use idl_core::manager::{AddRequest, ItemView, Manager, StartMode};
use idl_core::settings::Settings;
use idl_core::types::{RequestInfo, Status};
use idl_core::categories::Category;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("idl-mgr-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn manager(name: &str, tweak: impl FnOnce(&mut Settings)) -> (Arc<Manager>, PathBuf, PathBuf) {
    let root = tmpdir(name);
    let data = root.join("data");
    let dl = root.join("dl");
    let mut s = Settings { download_dir: dl.clone(), category_folders: false, ..Default::default() };
    tweak(&mut s);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&s).unwrap()).unwrap();
    let m = Manager::new(&data, tokio::runtime::Handle::current()).unwrap();
    (m, data, dl)
}

async fn wait_for(m: &Arc<Manager>, id: u64, what: &str, pred: impl Fn(&ItemView) -> bool) -> ItemView {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = m.get(id) {
            if pred(&v) {
                return v;
            }
        }
        assert!(Instant::now() < end, "timed out waiting for {what}; now: {:?}", m.get(id).map(|v| (v.item.status, v.item.downloaded, v.item.error)));
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn add(addr: std::net::SocketAddr, name: &str) -> AddRequest {
    AddRequest { request: RequestInfo::new(format!("http://{addr}/files/{name}")), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_and_completes() {
    let len = 3 * 1024 * 1024;
    let (addr, _) = serve(Cfg::new(len)).await;
    let (m, _, dl) = manager("complete", |_| {});
    let r = m.add(add(addr, "a.bin")).await.unwrap();
    assert!(!r.duplicate);
    let v = wait_for(&m, r.id, "completion", |v| v.item.status == Status::Completed).await;
    assert_eq!(v.item.size, Some(len));
    assert_eq!(v.item.dest, dl.join("a.bin"));
    assert!(v.item.completed_at.is_some());
    verify_file(&v.item.dest, len);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_go_to_category_folders() {
    let (addr, _) = serve(Cfg::new(10_000)).await;
    let (m, _, dl) = manager("cats", |s| s.category_folders = true);
    let a = m.add(add(addr, "movie.mp4")).await.unwrap();
    let b = m.add(add(addr, "setup.exe")).await.unwrap();
    let c = m.add(add(addr, "weird.xyz")).await.unwrap();
    for id in [a.id, b.id, c.id] {
        wait_for(&m, id, "completion", |v| v.item.status == Status::Completed).await;
    }
    assert_eq!(m.get(a.id).unwrap().item.category, Category::Video);
    assert!(dl.join("Video/movie.mp4").exists());
    assert!(dl.join("Programs/setup.exe").exists());
    assert!(dl.join("General/weird.xyz").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_name_gets_unique_destination() {
    let (addr, _) = serve(Cfg::new(5_000)).await;
    let (m, _, dl) = manager("unique", |_| {});
    let mut r1 = add(addr, "dup.bin");
    r1.allow_duplicate = true;
    let a = m.add(r1.clone()).await.unwrap();
    let b = m.add(r1).await.unwrap();
    for id in [a.id, b.id] {
        wait_for(&m, id, "completion", |v| v.item.status == Status::Completed).await;
    }
    assert!(dl.join("dup.bin").exists() && dl.join("dup (1).bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn respects_max_concurrent() {
    let len = 2 * 1024 * 1024;
    let (addr, _) = serve(Cfg::new(len)).await;
    let (m, _, _) = manager("concurrent", |s| {
        s.max_concurrent = 1;
        s.speed_limit = 1024 * 1024;
    });
    let a = m.add(add(addr, "one.bin")).await.unwrap();
    let b = m.add(add(addr, "two.bin")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(m.get(a.id).unwrap().item.status, Status::Downloading);
    assert_eq!(m.get(b.id).unwrap().item.status, Status::Queued);
    wait_for(&m, b.id, "second to finish", |v| v.item.status == Status::Completed).await;
    assert_eq!(m.get(a.id).unwrap().item.status, Status::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume() {
    let len = 4 * 1024 * 1024;
    let (addr, _) = serve(Cfg::new(len)).await;
    let (m, _, _) = manager("pause", |s| s.speed_limit = 1024 * 1024);
    let r = m.add(add(addr, "p.bin")).await.unwrap();
    wait_for(&m, r.id, "some progress", |v| v.item.downloaded > 100_000).await;
    m.pause(r.id);
    let v = wait_for(&m, r.id, "paused", |v| v.item.status == Status::Paused && v.connections == 0).await;
    let got = v.item.downloaded;
    assert!(got > 0 && got < len);
    m.resume(r.id);
    m.update_settings(Settings { speed_limit: 0, ..m.settings() }).unwrap();
    let v = wait_for(&m, r.id, "completion", |v| v.item.status == Status::Completed).await;
    verify_file(&v.item.dest, len);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_survives_restart() {
    let (addr, _) = serve(Cfg::new(50_000)).await;
    let (m, data, _) = manager("restart", |_| {});
    let done = m.add(add(addr, "done.bin")).await.unwrap();
    wait_for(&m, done.id, "completion", |v| v.item.status == Status::Completed).await;
    let mut later = add(addr, "later.bin");
    later.start = StartMode::Later;
    let l = m.add(later).await.unwrap();
    m.shutdown();
    drop(m);

    let m2 = Manager::new(&data, tokio::runtime::Handle::current()).unwrap();
    let list = m2.list();
    assert_eq!(list.len(), 2);
    assert_eq!(m2.get(done.id).unwrap().item.status, Status::Completed);
    assert_eq!(m2.get(l.id).unwrap().item.status, Status::Paused);
    // ids keep increasing after a restart
    let n = m2.add(add(addr, "new.bin")).await.unwrap();
    assert!(n.id > l.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_url_resumes_existing() {
    let (addr, _) = serve(Cfg::new(20_000)).await;
    let (m, _, _) = manager("dup", |_| {});
    let mut a = add(addr, "x.bin");
    a.start = StartMode::Later;
    let first = m.add(a.clone()).await.unwrap();
    assert_eq!(m.get(first.id).unwrap().item.status, Status::Paused);
    let second = m.add(a).await.unwrap();
    assert!(second.duplicate);
    assert_eq!(second.id, first.id);
    wait_for(&m, first.id, "completion", |v| v.item.status == Status::Completed).await;
    assert_eq!(m.list().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_with_and_without_file() {
    let (addr, _) = serve(Cfg::new(20_000)).await;
    let (m, _, _) = manager("remove", |_| {});
    let a = m.add(add(addr, "keep.bin")).await.unwrap();
    let b = m.add(add(addr, "gone.bin")).await.unwrap();
    let va = wait_for(&m, a.id, "a", |v| v.item.status == Status::Completed).await;
    let vb = wait_for(&m, b.id, "b", |v| v.item.status == Status::Completed).await;
    m.remove(a.id, false);
    m.remove(b.id, true);
    assert!(va.item.dest.exists());
    assert!(!vb.item.dest.exists());
    assert!(m.list().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduled_download_waits() {
    let (addr, _) = serve(Cfg::new(20_000)).await;
    let (m, _, _) = manager("sched", |_| {});
    let mut a = add(addr, "s.bin");
    a.scheduled_at = Some(idl_core::manager::now_secs() + 3600);
    let r = m.add(a).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(m.get(r.id).unwrap().item.status, Status::Queued);
    m.set_schedule(r.id, None);
    wait_for(&m, r.id, "start after schedule cleared", |v| v.item.status == Status::Completed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspect_suggests_name_and_folder() {
    let mut cfg = Cfg::new(1234);
    cfg.disposition = Some("attachment; filename=\"Lecture 1.mp4\"".into());
    let (addr, _) = serve(cfg).await;
    let (m, _, dl) = manager("inspect", |s| s.category_folders = true);
    let i = m.inspect(&RequestInfo::new(format!("http://{addr}/dl?id=1"))).await.unwrap();
    assert_eq!(i.probe.size, Some(1234));
    assert_eq!(i.category, Category::Video);
    assert_eq!(i.suggested_path, dl.join("Video").join("Lecture 1.mp4"));
    assert!(i.duplicate_of.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_is_reported() {
    let mut cfg = Cfg::new(100);
    cfg.status = Some(404);
    let (addr, _) = serve(cfg).await;
    let (m, _, _) = manager("fail", |_| {});
    let mut a = add(addr, "missing.bin");
    a.filename = Some("missing.bin".into()); // skip the probe-at-add so the failure happens in the engine
    let r = m.add(a).await.unwrap();
    let v = wait_for(&m, r.id, "error", |v| v.item.status == Status::Error).await;
    assert!(v.item.error.unwrap().contains("not found"));
}
