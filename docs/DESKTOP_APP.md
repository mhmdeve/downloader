# Desktop app development

The Tauri v2 desktop shell lives in `src-tauri/`; its dependency-light frontend is the
static `ui/` directory (no Node.js bundler is required).

## Prerequisites

- Rust stable (the workspace currently targets Rust 2021).
- On Linux, WebKitGTK 4.1, GTK 3, AppIndicator, librsvg, libsoup 3, OpenSSL, and a C build toolchain.
- On Windows, the Microsoft C++ Build Tools and WebView2 Runtime.
- On macOS, Xcode Command Line Tools.

## Build and run

From the repository root:

```sh
cargo test --workspace
cargo run -p internet-downloader
```

The app stores its download history and settings under Tauri's per-user app-data directory.
At startup it creates the core `Manager` and launches the authenticated IPC service on
IPv4 loopback. The service publishes an ephemeral port and a fresh 256-bit token to the
per-user `internet-downloader/ipc.json` bootstrap file consumed by `idl-native-host`.
The service removes that bootstrap file when its server handle is dropped.

The UI uses Tauri's global API bridge (`app.withGlobalTauri`) so the frontend remains plain
static HTML/CSS/JavaScript. The app is still an early shell: native-host manifest registration,
tray controls, single-instance behavior, notifications, and release packaging are not yet
complete. The browser extension and end-to-end browser interception flow must be added and
tested before M6/M7 can be considered complete.
