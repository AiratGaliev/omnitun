mod events;
mod profile;
mod settings;
mod subscription;
mod vpn;

use crate::events::emit_status;
use crate::profile::ProfileManager;
use crate::subscription::SubscriptionProfile;
use crate::vpn::{ServerProfile, VpnManager, VpnStatus};
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::Mutex;

struct VpnState {
    manager: Arc<Mutex<VpnManager>>,
}

struct ProfileState {
    manager: Arc<Mutex<ProfileManager>>,
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

#[tauri::command]
async fn add_subscription(
    state: tauri::State<'_, ProfileState>,
    name: String,
    url: String,
) -> Result<SubscriptionProfile, String> {
    let mut mgr = state.manager.lock().await;
    mgr.add(name, url).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn remove_subscription(
    state: tauri::State<'_, ProfileState>,
    id: String,
) -> Result<(), String> {
    let mut mgr = state.manager.lock().await;
    mgr.remove(&id).map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_subscriptions(
    state: tauri::State<'_, ProfileState>,
) -> Result<Vec<SubscriptionProfile>, String> {
    let mgr = state.manager.lock().await;
    Ok(mgr.list().to_vec())
}

#[tauri::command]
async fn refresh_subscription(
    state: tauri::State<'_, ProfileState>,
    id: String,
) -> Result<SubscriptionProfile, String> {
    let mut mgr = state.manager.lock().await;
    mgr.refresh(&id).await.map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("Failed to get app data dir");

            std::fs::create_dir_all(&data_dir).expect("Failed to create app data dir");

            let vpn_manager = VpnManager::new(data_dir.clone());
            let vpn_state = VpnState {
                manager: Arc::new(Mutex::new(vpn_manager)),
            };
            app.manage(vpn_state);

            let mut profile_manager = ProfileManager::new(data_dir);
            profile_manager
                .load()
                .expect("Failed to load profiles");
            let profile_state = ProfileState {
                manager: Arc::new(Mutex::new(profile_manager)),
            };
            app.manage(profile_state);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_vpn,
            stop_vpn,
            get_vpn_status,
            add_subscription,
            remove_subscription,
            get_subscriptions,
            refresh_subscription,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
