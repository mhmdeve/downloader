# Progress.md — Internet Downloader

> **RESUME HERE.** If you are an AI agent (or human) picking this project up after an
> interruption: read this file top to bottom, find the first unchecked milestone in
> [Milestones](#milestones), read its notes in the [Step log](#step-log), then continue.
> Every milestone ends with a git commit whose message starts with the milestone id
> (e.g. `[M2] ...`). `git log --oneline` is the source of truth for what is done.
> After finishing each step: tick the box, append a line to the Step log, commit, push to `main`.

## Goal

A professional-grade, IDM-style download manager named **Internet Downloader**:

- Windows, macOS and Linux (one codebase)
- Segmented multi-connection downloads, pause/resume, queues, scheduler, speed limiter
- Downloads **any** file type (not only video) with IDM-style categories
- Browser extension (Chrome / Edge / Brave / Firefox) that intercepts downloads and
  detects videos, talking to the app through native messaging
- Modern UI built on the OS-native webview (WebView2 / WKWebView / WebKitGTK)
- Smallest practical package size

## Architecture decisions (and why)

| Decision | Reason |
|---|---|
| **Tauri v2 + Rust**, UI = plain HTML/CSS/JS in `ui/` (no bundler, no framework) | Uses the OS webview instead of shipping Chromium, so installers are a few MB instead of 100+ MB. No `node_modules` in the shipped app. |
| Engine is a separate crate `crates/core` (`idl-core`) with no GUI dependency | Testable headless; CI can run engine tests on all three OSes. |
| `reqwest` with **native-tls** and no compression features | Uses SChannel (Windows), Security.framework (macOS), OpenSSL (Linux) = OS-provided TLS and trust store, smaller binary. Raw bytes are downloaded (no transparent decompression). |
| State persisted as JSON (`*.idl` sidecar per download + `history.json`) | No SQLite dependency; trivial to inspect and to resume from. |
| Browser extension talks **only** via native messaging; `crates/native-host` forwards to the app over authenticated loopback TCP | No web-page-reachable HTTP endpoint (no CSRF / DNS-rebinding surface). Host can launch the app if it is not running, like IDM. |
| Fixed extension ID via `key` in `extension/manifest.json` | Chrome-family native-messaging manifests need a stable `allowed_origins`. |
| Native-host manifests registered by the app itself on first run (registry via `reg.exe` on Windows, files on macOS/Linux) | No installer scripts needed; also available from Settings. |
| Release profile: `opt-level="z"`, `lto`, `codegen-units=1`, `panic="abort"`, `strip` | Size. |
| HLS (`.m3u8`) handled in core; segments concatenated (TS) and optionally remuxed to MP4 if `ffmpeg` is on PATH | IDM-like video grabbing without bundling ffmpeg (~70 MB). |

## Layout

```
Cargo.toml              workspace
crates/core             idl-core: engine (probe, segments, resume, queue, scheduler, limiter, HLS, categories)
crates/native-host      idl-native-host: stdio native-messaging <-> app bridge (tiny, std-only)
src-tauri               the desktop app (Tauri v2): commands, tray, IPC server, windows
ui                      frontend (vanilla JS/CSS)
extension               MV3 browser extension (Chrome/Edge/Brave/Firefox)
.github/workflows       CI + release builds for Windows/macOS/Linux
Progress.md             this file
```

## Environment notes (for resuming in a fresh sandbox)

- Rust >= 1.97, Node 22 (only for extension tests / scripts).
- Linux build deps for Tauri:
  `apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libsoup-3.0-dev libssl-dev build-essential`
- Engine tests: `cargo test -p idl-core`
- App build: `cargo build -p internet-downloader --release` (or `cargo tauri build` with tauri-cli)

## Milestones

- [x] **M0** Repo attached, toolchain checked, Linux build deps installed
- [x] **M1** Skeleton: workspace, Progress.md, README, LICENSE, .gitignore
- [x] **M2** Core: types, categories, filename/URL helpers, probe (HEAD/Range, Content-Disposition)
- [x] **M3** Core: segmented downloader (dynamic split, retry, resume sidecar) + tests against a local range server
- [x] **M4** Core: manager (queue, max-concurrent, speed limiter, scheduler, history, events)
- [x] **M5** Core: HLS (.m3u8) downloader
- [ ] **M6** IPC server (loopback + token) + `native-host` bridge (service, bootstrap, and bridge tests pass; desktop app startup and browser end-to-end wiring remain)
- [ ] **M7** Tauri app shell: commands, tray, single-instance, native-host registration, notifications
- [ ] **M8** UI: main window, add-URL dialog, download-info dialog, progress, settings, dark/light
- [ ] **M9** Browser extension: interception, video detection, context menu, popup, overlay button
- [ ] **M10** CI + release workflows, size optimization, size report
- [ ] **M11** Docs polish (README, INSTALL, extension install), final verification

## Step log

Append-only. Format: `YYYY-MM-DD [Mx] what was done — commit sha`.

- 2026-10-27 [M0] Attached repo (empty), confirmed cargo 1.97 / node 22, installed webkit2gtk-4.1 + GTK deps.

- 2026-10-27 [M1] Workspace Cargo.toml (size-first release profile), LICENSE (MIT), README, .gitignore, this file.
- 2026-10-27 [M2] `crates/core`: error, types (RequestInfo/Status/Segment), categories, filename (Content-Disposition RFC 6266, sanitising, unique paths, `.idl` sidecar path), net (client + request decoration, identity encoding), probe (GET Range 0-0 -> size/ranges/name/validators). 7 unit tests pass. NOTE: workspace `members` currently only lists `crates/core`; re-add `crates/native-host` and `src-tauri` as they are created (M6/M7).

- 2026-10-27 [M3] `crates/core/src/engine.rs` + `limiter.rs`: segmented downloader. Pre-allocated `<file>.part`, per-connection buffered positional writes (pos = flushed bytes only), dynamic splitting of the busiest segment, retry w/ backoff (8x, resets on progress), `If-Range` + Content-Range validation, fallback to original URL if redirect target expires, sidecar `<file>.idl` saved every 1 s, resume validates size/ETag/Last-Modified else restarts, pause/cancel via `watch<Signal>`, unknown-length + no-range servers handled, global token-bucket limiter. Tests: `crates/core/tests/engine.rs` (11 integration tests against a scriptable local server in `tests/common/mod.rs`) all pass.

- 2026-10-27 [M4] `settings.rs` (+ per-category dirs, `extra` map for UI prefs) and `manager.rs`: `Manager::new(data_dir, tokio Handle)`; `inspect()` for the file-info dialog, `add()` (probe if no name, category folders, unique dest incl. queued items, duplicate-URL resume), `pause/resume/*_all/remove(delete_file)/clear_completed/set_schedule/update_request/update_settings`, queue with `max_concurrent`, scheduler (`scheduled_at`, pumped every 5 s), global speed limit, 4 Hz `Event::Progress` stream (speed EMA, ETA, connections), history in `history.json` (0600 on unix), in-flight items restored as Paused (or Queued if `auto_resume`). 11 integration tests in `tests/manager.rs` pass.

- 2026-10-27 [M5] `hls.rs`: playlist parser (master -> best variant first, media, AES-128 key/IV incl. implicit IV from sequence, `EXT-X-MAP` fMP4 init, `EXT-X-BYTERANGE`), parallel segment downloader storing `<dest>.hls/NNNNNN.seg` (resume = skip finished segments; playlist change => restart), concat to `.ts`/`.mp4`/`.aac`, optional ffmpeg stream-copy remux ts->mp4 (`Settings.hls_remux`). Rejects live streams and SAMPLE-AES/DRM with clear errors. Manager: `Kind::{Http,Hls}` on items, detection by `.m3u8` URL or mpegurl content-type, `Inspection.hls` (variants for the quality picker), final extension published through `Shared.final_dest`. 6 unit + 6 integration tests (`tests/hls.rs`) pass. Full suite: `cargo test -p idl-core`.

- 2026-10-27 [M7 desktop shell scaffold] Added the initial Tauri v2 app crate under `src-tauri`, a static HTML/CSS/JS download-list UI, URL validation and Tauri commands for listing, adding, pausing, resuming, and removing downloads, and startup wiring that creates the core Manager and starts `IpcServer::bind_with_bootstrap`. Added Linux WebKitGTK/GTK system dependencies to CI and included the Tauri app in the workspace. M7 is still in progress: cross-platform CI is pending; native-host manifest registration, single-instance handling, tray, notifications, and packaging verification remain. M6 remains unchecked because browser native-messaging end-to-end verification is still outstanding.

- 2026-10-27 [M7 single-instance behavior] Added the Tauri single-instance plugin; a second launch now brings the existing main window forward instead of opening a second app instance. Added `docs/DESKTOP_APP.md` with prerequisites, build commands, and the current integration boundaries. Cross-platform CI for the initial shell is still running; this plugin change also needs CI verification.

## Known issues / TODO

- HLS: separate audio renditions (`EXT-X-MEDIA`) and DASH (`.mpd`) are not handled (document as limitation).
- Disk-space pre-check not implemented (ENOSPC surfaces as an I/O error). Candidate for M4/M7.
- Engine writes the whole `Request` (cookies!) into the `.idl` sidecar; consider stripping cookies at rest in M4.

- 2026-10-27 [M6 checkpoint] Added crates/native-host with a bounded 4-byte little-endian native-messaging JSON frame reader/writer, protocol-only stdout, stderr diagnostics, and 9 unit tests. Added the crate to the Cargo workspace. The host returns APP_BRIDGE_NOT_CONFIGURED by design until authenticated loopback IPC is implemented; M6 remains unchecked. Tests have not been run here because this GitHub-connected environment cannot execute Cargo. Next: implement per-run token bootstrap and authenticated loopback IPC with narrow health/inspect/enqueue/status commands, then run cargo test --workspace and add end-to-end protocol tests.

## M6 implementation contract (resume checkpoint)

M6 is not complete until the host and app-side service are implemented and protocol-tested.
Do not tick the milestone merely because these acceptance criteria are documented.

### IPC security and protocol requirements

- Bind the app service only to IPv4 loopback (127.0.0.1), never 0.0.0.0 or a wildcard.
- Generate a high-entropy per-user bearer token when the app starts. Keep it outside the repository and pass it to the native host through a private local configuration channel; never put it in command-line arguments or logs.
- Require a token on every request and compare it without early-exit string comparison where practical. Reject requests from untrusted origins and limit request/header/body sizes.
- Use a versioned, newline-delimited JSON protocol with explicit request IDs and structured error responses. Bound each line and the number of concurrent clients.
- Implement only the minimal command surface needed by the extension: health/version, enqueue a download, inspect a URL, and query status. Do not expose arbitrary filesystem operations, shell execution, or general-purpose URL fetch/proxy functionality.
- Parse native-messaging frames as a 4-byte little-endian length followed by UTF-8 JSON. Enforce the browser protocol maximum frame size, reject truncated/oversized frames, write only framed protocol messages to stdout, and send diagnostics to stderr.
- Validate URLs as HTTP(S); reject embedded credentials and unsupported schemes. Treat browser-supplied cookies/headers as sensitive and never log them.
- Avoid SSRF: the IPC API is for browser-to-local-app actions, not a generic proxy. Loopback, link-local, and private-network URLs require an explicit documented policy so a page cannot trick the app into accessing local services.
- Add protocol tests for malformed frames, oversized input, missing/wrong token, invalid JSON, wrong protocol version, invalid URLs, and app-not-running behavior.

### Required M6 implementation sequence

1. Re-read Cargo.toml, the core manager public API, and Progress.md; confirm main HEAD before writing.
2. Add crates/native-host as a small std-only Rust binary. Start with correct frame parsing/writing and tests; keep dependencies minimal.
3. Add the loopback server/token implementation to a separate crate/module used by the app, with narrow command routing to idl_core::Manager.
4. Add an explicit local configuration/bootstrap contract between the desktop app and native host. Do not store reusable IPC tokens in committed files.
5. Re-add both crates to the workspace; update the lockfile using Cargo and run cargo test --workspace locally.
6. Mark M6 complete only after protocol tests pass and record the test command/result and commit SHA. If the environment cannot run Cargo, report the limitation and leave M6 unchecked.

### Current resume point

- Verified main at 42c1d518287fb98b8ff4c60a74e01b74a9ec7cde.
- Actual tree contains crates/core and crates/native-host; src-tauri and the browser extension do not exist yet. The workspace includes the core and native-host crates.
- Next action: implement the authenticated local IPC server and a private bootstrap mechanism from the app to the host. The host frame codec is present but intentionally returns APP_BRIDGE_NOT_CONFIGURED. After wiring commands, run cargo test --workspace and end-to-end tests; do not mark the integration complete before verifying it.

- 2026-10-27 [M6 follow-up] Added GitHub Actions workflow .github/workflows/rust.yml to run cargo test --workspace on Ubuntu, Windows, and macOS for pushes to main and pull requests. The workflow has been committed but has not yet run; inspect its first run before treating tests as passing. Current next step: implement strict host request schema/version/URL validation and the authenticated loopback app service; do not expose arbitrary fetch or filesystem APIs.


- 2026-10-27 [M6 protocol validation] GitHub Actions run 40491456548 passed `cargo test --workspace` on Ubuntu, macOS, and Windows (commit 0330b1b08118324bd97d511022cbb4a1e7f4a431). The native host now validates protocol version, request ID, command allowlist, and HTTP(S) URLs without embedded credentials, and returns structured validation errors. Added tests for these cases. Valid requests still fail closed with APP_BRIDGE_NOT_CONFIGURED because authenticated loopback IPC and private bootstrap are not implemented; M6 remains unchecked. CI will validate these new commits separately. Next: implement app-side authenticated loopback IPC and connect the native host through a private bootstrap channel.


- 2026-10-27 [M6 authenticated IPC service scaffold] Confirmed CI run 40491841207 passed on Ubuntu, macOS, and Windows for protocol validation commit 8c1f3a6245f12a01b83bf38a936ec7fbb6a720b7. Added crates/ipc with an IPv4-loopback-only TCP JSON-lines service, 64 KiB request limit, 16-client concurrency cap, per-request token authentication using constant-time comparison for equal-length tokens, protocol version checks, and narrow health/status/inspect/enqueue commands. Added URL validation and generic error responses that avoid exposing engine internals. The app must still generate a cryptographically random per-session token and securely provide its port/token to the native host; this crate does not persist or bootstrap secrets. The host is not connected to this service yet, so M6 remains unchecked. CI will compile/test this addition next.

- 2026-10-27 [M6 CI fix] GitHub Actions run 40492180700 failed on Ubuntu, macOS, and Windows because `crates/ipc/src/lib.rs` imported core API types from the crate root, where they are not re-exported. Corrected the imports to `idl_core::manager::{AddRequest, Manager, StartMode}` and `idl_core::types::RequestInfo` in commit b5b5fe3734858b02300d5cd4ac949b9608b8b617. Awaiting the next cross-platform CI run; M6 remains unchecked.

- 2026-10-27 [M6 cross-platform CI pass] GitHub Actions run 40492552661 passed `cargo test --workspace` on Ubuntu, macOS, and Windows after fixing the IPC crate imports (fix commit b5b5fe3734858b02300d5cd4ac949b9608b8b617). This confirms the current workspace compiles and tests pass across all three CI targets; it does not prove the browser-to-app bridge is end-to-end complete. M6 remains unchecked until private token bootstrap, native-host forwarding, and integration tests are implemented.

- 2026-10-27 [M6 native-host forwarding] Replaced the fail-closed placeholder with a loopback-only bridge: the native host reads a per-user ipc.json bootstrap, rejects invalid/overly broad Unix permissions, injects the bearer token into the validated request, connects only to 127.0.0.1, applies 3-second socket timeouts, and bounds IPC lines at 64 KiB. Missing bootstrap/app returns APP_NOT_RUNNING without exposing secrets. Added docs/IPC_BOOTSTRAP.md defining the runtime config shape and secure writer requirements (Unix 0700/0600; Windows user-only ACL). The desktop app does not yet write this file or start the service; M6 remains unchecked. CI must validate the forwarding changes next.

- 2026-10-27 [M6 bootstrap + bridge tests] Added IpcServer::bind_with_bootstrap: creates a fresh 256-bit OS-random token, binds IPv4 loopback on an ephemeral port, writes the matching per-user ipc.json with Unix directory/file permissions 0700/0600 and atomic rename on Unix, and removes the bootstrap file when the server handle drops. Native host now reads that same path, forwards authenticated requests, caps IPC lines/responses, and fails closed when the app is absent. Added an async end-to-end loopback test for valid and invalid tokens and documented the bootstrap contract. GitHub Actions run 40493362716 passed `cargo test --workspace` on Ubuntu, macOS, and Windows. M6 remains unchecked because Tauri app startup and browser native-messaging end-to-end verification are still missing.
