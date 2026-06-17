mod events;
mod subscription;
mod vpn;

use crate::events::emit_status;
use crate::vpn::{ServerProfile, VpnManager, VpnStatus};
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::Mutex;

struct VpnState {
    manager: Arc<Mutex<VpnManager>>,
}

#[tauri::command]
async fn start_vpn(
    app: tauri::AppHandle,
    state: tauri::State<'_, VpnState>,
    profile: ServerProfile,
) -> Result<(), String> {
    emit_status(&app, VpnStatus::Connecting);

    let manager = state.manager.lock().await;
    match manager.start(&profile).await {
        Ok(_) => {
            emit_status(&app, VpnStatus::Connected);
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            emit_status(&app, VpnStatus::Error(msg.clone()));
            Err(msg)
        }
    }
}

#[tauri::command]
async fn stop_vpn(app: tauri::AppHandle, state: tauri::State<'_, VpnState>) -> Result<(), String> {
    let manager = state.manager.lock().await;
    match manager.stop().await {
        Ok(_) => {
            emit_status(&app, VpnStatus::Disconnected);
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            emit_status(&app, VpnStatus::Error(msg.clone()));
            Err(msg)
        }
    }
}

#[tauri::command]
async fn get_vpn_status(state: tauri::State<'_, VpnState>) -> Result<VpnStatus, String> {
    let manager = state.manager.lock().await;
    Ok(manager.status())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("Failed to get app data dir");

            std::fs::create_dir_all(&data_dir).expect("Failed to create app data dir");

            let manager = VpnManager::new(app.handle().clone(), data_dir);
            let state = VpnState {
                manager: Arc::new(Mutex::new(manager)),
            };

            app.manage(state);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_vpn,
            stop_vpn,
            get_vpn_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
