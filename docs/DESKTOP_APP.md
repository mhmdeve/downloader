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
static HTML/CSS/JavaScript. The single-instance plugin is configured to show/focus the existing
main window when another instance launches. Placeholder platform icons are generated at build
time so a clean checkout can compile; replace them with proper product artwork before release.

Still outstanding: native-host manifest registration, tray controls, desktop notifications,
release packaging, the browser extension, and end-to-end browser interception verification.
M6/M7 must remain unchecked until their relevant integration tests pass.
