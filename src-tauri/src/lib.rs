mod events;
mod logging;
mod profile;
pub(crate) mod settings;
mod subscription;
mod vpn;

use crate::events::emit_status;
use crate::logging::{log, LogLevel, Logger, LogSource, MAX_LOG_FILES_COUNT};
use crate::profile::ProfileManager;
use crate::subscription::SubscriptionProfile;
use crate::settings::{AppSettings, SettingsManager};
use crate::vpn::{ServerProfile, VpnManager, VpnStatus};
use chrono::Local;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tauri::Manager;
use tokio::sync::Mutex;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const PROXY_TIMEOUT_SECS: u64 = 15;
const PROXY_POLL_MS: u64 = 300;

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
    settings_state: tauri::State<'_, Mutex<AppSettings>>,
    profile: ServerProfile,
    routing_mode: String,
    proxy_port: u16,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::System,
        format!("Starting VPN connection to {} ({routing_mode} mode, {} port)...", profile.name, proxy_port),
    );
    emit_status(&app, VpnStatus::Connecting);

    let manager_arc = state.manager.clone();
    let manager = manager_arc.lock().await;
    let settings = settings_state.lock().await.clone();
    match manager.start(&app, proxy_port, &routing_mode, &profile, &settings).await {
        Ok(_) => {
            log(
                &app,
                &logger,
                LogLevel::Info,
                LogSource::Vpn,
                "Verifying proxy connection...".into(),
            );

            let result = match manager.process_pid() {
                Some(pid) => wait_for_alive(pid).await,
                None => Err("no PID".into()),
            };
            match result {
                Ok(attempts) => {
                    log(
                        &app,
                        &logger,
                        LogLevel::Info,
                        LogSource::Vpn,
                        format!("Proxy ready after {attempts} attempt(s)"),
                    );
                    log(
                        &app,
                        &logger,
                        LogLevel::Success,
                        LogSource::Vpn,
                        format!("VPN connected to {}", profile.name),
                    );
                    emit_status(&app, VpnStatus::Connected);

                    // Watchdog: detect unexpected process exit and clear PID
                    let watch_pid = manager.process_pid();
                    if let Some(pid) = watch_pid {
                        let app_clone = app.clone();
                        let mgr_arc = manager_arc.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                if !is_process_alive(pid) {
                                    log(
                                        &app_clone,
                                        &app_clone.state::<StdMutex<Logger>>(),
                                        LogLevel::Warning,
                                        LogSource::Process,
                                        "VPN process exited unexpectedly".into(),
                                    );
                                    // Clear stale PID so reconnect works
                                    let _ = mgr_arc.lock().await.stop(&app_clone).await;
                                    emit_status(&app_clone, VpnStatus::Disconnected);
                                    break;
                                }
                            }
                        });
                    }

                    Ok(())
                }
                Err(e) => {
                    let _ = manager.stop(&app).await;
                    let msg = format!("Proxy not reachable: {e}");
                    log(&app, &logger, LogLevel::Error, LogSource::Vpn, msg.clone());
                    emit_status(&app, VpnStatus::Error(msg.clone()));
                    Err(msg)
                }
            }
        }
        Err(e) => {
            let msg = e.to_string();
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Vpn,
                format!("Connection failed: {msg}"),
            );
            emit_status(&app, VpnStatus::Error(msg.clone()));
            Err(msg)
        }
    }
}

#[tauri::command]
async fn stop_vpn(
    app: tauri::AppHandle,
    state: tauri::State<'_, VpnState>,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::System,
        "Disconnecting VPN...".into(),
    );

    let manager = state.manager.lock().await;
    match manager.stop(&app).await {
        Ok(_) => {
            log(
                &app,
                &logger,
                LogLevel::Success,
                LogSource::Vpn,
                "VPN disconnected".into(),
            );
            emit_status(&app, VpnStatus::Disconnected);
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Vpn,
                format!("Failed to stop VPN: {msg}"),
            );
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
    app: tauri::AppHandle,
    state: tauri::State<'_, ProfileState>,
    name: String,
    url: String,
) -> Result<SubscriptionProfile, String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Adding subscription: {name}"),
    );

    let mut mgr = state.manager.lock().await;
    match mgr.add(&app, name.clone(), url).await {
        Ok(profile) => {
            let count = profile.servers.len();
            log(
                &app,
                &logger,
                LogLevel::Success,
                LogSource::Subscription,
                format!("Subscription '{name}' added ({count} server(s))"),
            );
            Ok(profile)
        }
        Err(e) => {
            let msg = e.to_string();
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Subscription,
                format!("Failed to add subscription: {msg}"),
            );
            Err(msg)
        }
    }
}

#[tauri::command]
async fn remove_subscription(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProfileState>,
    id: String,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();

    let mut mgr = state.manager.lock().await;
    match mgr.remove(&id) {
        Ok(()) => {
            log(
                &app,
                &logger,
                LogLevel::Info,
                LogSource::Subscription,
                "Subscription removed".into(),
            );
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Subscription,
                format!("Failed to remove subscription: {msg}"),
            );
            Err(msg)
        }
    }
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
    app: tauri::AppHandle,
    state: tauri::State<'_, ProfileState>,
    id: String,
) -> Result<SubscriptionProfile, String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        "Refreshing subscription...".into(),
    );

    let mut mgr = state.manager.lock().await;
    match mgr.refresh(&app, &id).await {
        Ok(profile) => {
            let count = profile.servers.len();
            log(
                &app,
                &logger,
                LogLevel::Success,
                LogSource::Subscription,
                format!("Subscription '{}' refreshed ({count} server(s))", profile.name),
            );
            Ok(profile)
        }
        Err(e) => {
            let msg = e.to_string();
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Subscription,
                format!("Failed to refresh subscription: {msg}"),
            );
            Err(msg)
        }
    }
}

#[tauri::command]
async fn get_settings(state: tauri::State<'_, Mutex<AppSettings>>) -> Result<AppSettings, String> {
    let settings = state.lock().await;
    Ok(settings.clone())
}

#[tauri::command]
async fn update_settings(
    app: tauri::AppHandle,
    new_settings: AppSettings,
    settings: tauri::State<'_, Mutex<AppSettings>>,
    settings_mgr: tauri::State<'_, Mutex<SettingsManager>>,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Settings,
        "Saving settings...".into(),
    );

    {
        let mut current = settings.lock().await;
        *current = new_settings.clone();
    }
    let mgr = settings_mgr.lock().await;
    match mgr.save(&new_settings) {
        Ok(()) => {
            log(
                &app,
                &logger,
                LogLevel::Success,
                LogSource::Settings,
                "Settings saved".into(),
            );
            Ok(())
        }
        Err(e) => {
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Settings,
                format!("Failed to save settings: {e}"),
            );
            Err(e)
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("Failed to get app data dir");

            std::fs::create_dir_all(&data_dir).expect("Failed to create app data dir");

            // Set up logging
            let log_dir = data_dir.join("logs");
            std::fs::create_dir_all(&log_dir).expect("Failed to create logs dir");
            let timestamp = Local::now().format("%Y%m%d-%H%M%S");
            let log_file = log_dir.join(format!("logs-{timestamp}.jsonl"));

            // Keep max MAX_LOG_FILES_COUNT log files, delete oldest excess
            if let Ok(entries) = std::fs::read_dir(&log_dir) {
                let mut log_files: Vec<_> = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().starts_with("logs-"))
                    .collect();
                log_files.sort_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()));
                if log_files.len() > MAX_LOG_FILES_COUNT {
                    for file in log_files.drain(..log_files.len() - MAX_LOG_FILES_COUNT) {
                        let _ = std::fs::remove_file(file.path());
                    }
                }
            }

            let logger = Logger::new(1000, Some(log_file));
            app.manage(StdMutex::new(logger));

            let vpn_manager = VpnManager::new(data_dir.clone());
            let vpn_state = VpnState {
                manager: Arc::new(Mutex::new(vpn_manager)),
            };
            app.manage(vpn_state);

            let settings_mgr = SettingsManager::new(data_dir.clone());
            let settings = settings_mgr.load().expect("Failed to load settings");
            app.manage(Mutex::new(settings_mgr));
            app.manage(Mutex::new(settings));

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
            get_settings,
            update_settings,
            logging::get_logs,
            logging::log_message,
            read_clipboard_text,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Read clipboard text via arboard (bypasses WebView2 clipboard restrictions).
#[tauri::command]
fn read_clipboard_text() -> Result<String, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("Clipboard init: {e}"))?;
    clipboard
        .get_text()
        .map_err(|e| format!("Clipboard read: {e}"))
}

/// Check if a process is alive by PID.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/fi", &format!("PID eq {pid}"), "/nh"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match output {
            Ok(o) => {
                let out = String::from_utf8_lossy(&o.stdout);
                !out.contains("INFO: No tasks")
            }
            Err(_) => false,
        }
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map_or(false, |o| o.status.success())
    }
}

/// Wait until a process PID is alive by polling `is_process_alive`.
async fn wait_for_alive(pid: u32) -> Result<u32, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(PROXY_TIMEOUT_SECS);
    let mut attempts = 0u32;

    loop {
        attempts += 1;
        if is_process_alive(pid) {
            return Ok(attempts);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out after {attempts} attempt(s) waiting for PID {pid}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(PROXY_POLL_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_alive_self() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn test_is_process_alive_nonexistent() {
        assert!(!is_process_alive(999_999_999));
    }

    #[tokio::test]
    async fn test_wait_for_alive_self() {
        let result = wait_for_alive(std::process::id()).await;
        assert!(result.is_ok());
        assert!(result.unwrap() > 0);
    }
}
