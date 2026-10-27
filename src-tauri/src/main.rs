#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use idl_core::{
    manager::{AddRequest, AddResult, ItemView, Manager, StartMode},
    types::RequestInfo,
};
use idl_ipc::IpcServer;
use std::sync::{Arc, Mutex};
use tauri::{Manager as TauriManager, RunEvent, State};

struct AppState {
    manager: Arc<Manager>,
    ipc_server: Mutex<Option<IpcServer>>,
}

#[tauri::command]
fn list_downloads(state: State<'_, AppState>) -> Vec<ItemView> {
    state.manager.list()
}

#[tauri::command]
async fn add_download(state: State<'_, AppState>, url: String) -> Result<AddResult, String> {
    let parsed = url::Url::parse(&url).map_err(|_| "Enter a valid HTTP(S) URL.".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none()
        || !parsed.username().is_empty() || parsed.password().is_some()
    {
        return Err("Only HTTP(S) URLs without embedded credentials are supported.".into());
    }
    state.manager.add(AddRequest {
        request: RequestInfo::new(url),
        start: StartMode::Now,
        ..Default::default()
    }).await.map_err(|error| error.to_string())
}

#[tauri::command]
fn pause_download(state: State<'_, AppState>, id: u64) {
    state.manager.pause(id);
}

#[tauri::command]
fn resume_download(state: State<'_, AppState>, id: u64) {
    state.manager.resume(id);
}

#[tauri::command]
fn remove_download(state: State<'_, AppState>, id: u64, delete_file: bool) {
    state.manager.remove(id, delete_file);
}

#[tauri::command]
fn pause_all_downloads(state: State<'_, AppState>) {
    state.manager.pause_all();
}

#[tauri::command]
fn resume_all_downloads(state: State<'_, AppState>) {
    state.manager.resume_all();
}

fn main() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let (manager, ipc_server) = tauri::async_runtime::block_on(async move {
                let handle = tokio::runtime::Handle::current();
                let manager = Manager::new(data_dir, handle)
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                let ipc_server = IpcServer::bind_with_bootstrap(manager.clone()).await?;
                Ok::<_, std::io::Error>((manager, ipc_server))
            })?;
            app.manage(AppState {
                manager,
                ipc_server: Mutex::new(Some(ipc_server)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_downloads,
            add_download,
            pause_download,
            resume_download,
            remove_download,
            pause_all_downloads,
            resume_all_downloads
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Internet Downloader");

    app.run(|app_handle, event| {
        if matches!(event, RunEvent::Exit) {
            if let Some(state) = app_handle.try_state::<AppState>() {
                state.manager.shutdown();
                if let Ok(mut server) = state.ipc_server.lock() {
                    server.take();
                }
            }
        }
    });
}
