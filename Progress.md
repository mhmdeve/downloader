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
- [ ] **M4** Core: manager (queue, max-concurrent, speed limiter, scheduler, history, events)
- [ ] **M5** Core: HLS (.m3u8) downloader
- [ ] **M6** IPC server (loopback + token) + `native-host` binary
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

## Known issues / TODO

- Disk-space pre-check not implemented (ENOSPC surfaces as an I/O error). Candidate for M4/M7.
- Engine writes the whole `Request` (cookies!) into the `.idl` sidecar; consider stripping cookies at rest in M4.
