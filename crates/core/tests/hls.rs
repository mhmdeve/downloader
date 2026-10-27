mod common;

use cbc::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};
use common::*;
use idl_core::manager::{AddRequest, Manager};
use idl_core::settings::Settings;
use idl_core::types::{Kind, RequestInfo, Status};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SEG_LEN: usize = 40_000;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("idl-hls-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn manager(name: &str, speed: u64) -> (Arc<Manager>, PathBuf) {
    let root = tmpdir(name);
    let dl = root.join("dl");
    let s = Settings { download_dir: dl.clone(), category_folders: false, hls_remux: false, speed_limit: speed, ..Default::default() };
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data/settings.json"), serde_json::to_vec(&s).unwrap()).unwrap();
    (Manager::new(root.join("data"), tokio::runtime::Handle::current()).unwrap(), dl)
}

async fn wait_done(m: &Arc<Manager>, id: u64) -> idl_core::manager::ItemView {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let v = m.get(id).unwrap();
        if v.item.status == Status::Completed {
            return v;
        }
        assert!(v.item.status != Status::Error, "failed: {:?}", v.item.error);
        assert!(Instant::now() < end, "timeout; status {:?} downloaded {}", v.item.status, v.item.downloaded);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn plain_playlist(n: usize, ext: &str) -> String {
    let mut p = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n");
    for i in 0..n {
        p.push_str(&format!("#EXTINF:4.0,\nseg{i}.{ext}\n"));
    }
    p.push_str("#EXT-X-ENDLIST\n");
    p
}

fn expected(n: u64) -> Vec<u8> {
    (0..n).flat_map(|i| seg_bytes(i, SEG_LEN)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn master_playlist_picks_best_variant_and_joins_segments() {
    let routes = new_routes();
    {
        let mut r = routes.lock().unwrap();
        r.insert("/v/master.m3u8".into(), b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=500000\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720\nhigh/index.m3u8\n".to_vec());
        r.insert("/v/high/index.m3u8".into(), plain_playlist(6, "ts").into_bytes());
        r.insert("/v/low/index.m3u8".into(), plain_playlist(2, "ts").into_bytes());
        for i in 0..6 {
            r.insert(format!("/v/high/seg{i}.ts"), seg_bytes(i, SEG_LEN));
        }
        for i in 0..2 {
            r.insert(format!("/v/low/seg{i}.ts"), seg_bytes(100 + i, SEG_LEN));
        }
    }
    let addr = serve_routes(routes, None).await;
    let (m, dl) = manager("master", 0);
    let r = m.add(AddRequest { request: RequestInfo::new(format!("http://{addr}/v/master.m3u8")), ..Default::default() }).await.unwrap();
    let v = wait_done(&m, r.id).await;
    assert_eq!(v.item.kind, Kind::Hls);
    assert_eq!(v.item.filename, "v.ts"); // "master" is generic => parent folder name
    assert_eq!(v.item.dest, dl.join("v.ts"));
    assert_eq!(std::fs::read(&v.item.dest).unwrap(), expected(6));
    assert!(!idl_core::hls::work_dir(&v.item.dest).exists(), "work dir must be cleaned up");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aes128_explicit_and_implicit_iv() {
    let key = [0x42u8; 16];
    let explicit_iv = [0x11u8; 16];
    let routes = new_routes();
    {
        let mut r = routes.lock().unwrap();
        r.insert("/enc/key.bin".into(), key.to_vec());
        // seg0 uses an explicit IV; seg1.. use the media sequence number (starts at 5) as IV.
        let mut p = String::from("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:5\n");
        p.push_str(&format!("#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x{}\n#EXTINF:4,\nseg0.ts\n", hex(&explicit_iv)));
        p.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\nseg1.ts\n#EXTINF:4,\nseg2.ts\n#EXT-X-ENDLIST\n");
        r.insert("/enc/index.m3u8".into(), p.into_bytes());
        for i in 0..3u64 {
            let plain = seg_bytes(i, SEG_LEN);
            let iv: [u8; 16] = if i == 0 {
                explicit_iv
            } else {
                let mut iv = [0u8; 16];
                iv[8..].copy_from_slice(&(5 + i).to_be_bytes());
                iv
            };
            let enc = cbc::Encryptor::<aes::Aes128>::new((&key).into(), (&iv).into()).encrypt_padded_vec::<Pkcs7>(&plain);
            r.insert(format!("/enc/seg{i}.ts"), enc);
        }
    }
    let addr = serve_routes(routes, None).await;
    let (m, _) = manager("aes", 0);
    let r = m.add(AddRequest { request: RequestInfo::new(format!("http://{addr}/enc/index.m3u8")), filename: Some("Secret Movie.mp4".into()), ..Default::default() }).await.unwrap();
    let v = wait_done(&m, r.id).await;
    assert_eq!(v.item.filename, "Secret Movie.ts");
    assert_eq!(std::fs::read(&v.item.dest).unwrap(), expected(3));
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fmp4_with_init_segment_becomes_mp4() {
    let routes = new_routes();
    {
        let mut r = routes.lock().unwrap();
        let mut p = String::from("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n");
        for i in 0..3 {
            p.push_str(&format!("#EXTINF:4,\nc{i}.m4s\n"));
        }
        p.push_str("#EXT-X-ENDLIST\n");
        r.insert("/f/index.m3u8".into(), p.into_bytes());
        r.insert("/f/init.mp4".into(), b"INITDATA".to_vec());
        for i in 0..3 {
            r.insert(format!("/f/c{i}.m4s"), seg_bytes(i, SEG_LEN));
        }
    }
    let addr = serve_routes(routes, None).await;
    let (m, dl) = manager("fmp4", 0);
    let r = m.add(AddRequest { request: RequestInfo::new(format!("http://{addr}/f/index.m3u8")), filename: Some("clip".into()), ..Default::default() }).await.unwrap();
    let v = wait_done(&m, r.id).await;
    assert_eq!(v.item.dest, dl.join("clip.mp4"));
    assert_eq!(v.item.filename, "clip.mp4");
    let mut want = b"INITDATA".to_vec();
    want.extend(expected(3));
    assert_eq!(std::fs::read(&v.item.dest).unwrap(), want);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_stream_is_rejected_with_a_clear_error() {
    let routes = new_routes();
    routes.lock().unwrap().insert("/live/index.m3u8".into(), b"#EXTM3U\n#EXTINF:4,\ns0.ts\n#EXTINF:4,\ns1.ts\n".to_vec());
    let addr = serve_routes(routes, None).await;
    let (m, _) = manager("live", 0);
    let r = m.add(AddRequest { request: RequestInfo::new(format!("http://{addr}/live/index.m3u8")), ..Default::default() }).await.unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let v = m.get(r.id).unwrap();
        if v.item.status == Status::Error {
            assert!(v.item.error.unwrap().contains("live"));
            break;
        }
        assert!(Instant::now() < end);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_keeps_finished_segments() {
    let routes = new_routes();
    {
        let mut r = routes.lock().unwrap();
        r.insert("/p/index.m3u8".into(), plain_playlist(12, "ts").into_bytes());
        for i in 0..12 {
            r.insert(format!("/p/seg{i}.ts"), seg_bytes(i, SEG_LEN));
        }
    }
    let addr = serve_routes(routes, Some(Duration::from_millis(400))).await;
    let (m, _) = manager("hlspause", 0);
    m.update_settings(Settings { max_connections: 2, ..m.settings() }).unwrap(); // 6 waves x 400 ms
    let r = m.add(AddRequest { request: RequestInfo::new(format!("http://{addr}/p/index.m3u8")), ..Default::default() }).await.unwrap();
    // wait for a few segments, then pause
    let end = Instant::now() + Duration::from_secs(10);
    while m.get(r.id).unwrap().item.downloaded < 2 * SEG_LEN as u64 {
        assert!(Instant::now() < end);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    m.pause(r.id);
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let v = m.get(r.id).unwrap();
        if v.item.status == Status::Paused && v.connections == 0 {
            break;
        }
        assert!(Instant::now() < end, "never settled: status {:?} conns {} err {:?}", v.item.status, v.connections, v.item.error);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let before = m.get(r.id).unwrap().item.downloaded;
    assert!(before >= 2 * SEG_LEN as u64 && before < 12 * SEG_LEN as u64, "downloaded {before}");
    m.resume(r.id);
    let v = wait_done(&m, r.id).await;
    assert_eq!(std::fs::read(&v.item.dest).unwrap(), expected(12));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspect_lists_variants() {
    let routes = new_routes();
    routes.lock().unwrap().insert(
        "/i/master.m3u8".into(),
        b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360\na.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=9000,RESOLUTION=1920x1080\nb.m3u8\n".to_vec(),
    );
    let addr = serve_routes(routes, None).await;
    let (m, _) = manager("hlsinspect", 0);
    let i = m.inspect(&RequestInfo::new(format!("http://{addr}/i/master.m3u8"))).await.unwrap();
    assert_eq!(i.kind, Kind::Hls);
    let h = i.hls.unwrap();
    assert_eq!(h.variants.len(), 2);
    assert_eq!(h.variants[0].resolution.as_deref(), Some("1920x1080"));
}
