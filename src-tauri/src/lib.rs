#![recursion_limit = "512"]
mod events;
mod logging;
mod profile;
pub(crate) mod settings;
mod subscription;
mod vpn;

use crate::events::emit_status;
use crate::events::emit_traffic;
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
use tauri::{Emitter, Manager};
use tauri::tray::{TrayIconBuilder, TrayIconEvent, MouseButton, MouseButtonState};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
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
    alternative_servers: Vec<ServerProfile>,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    let server_count = alternative_servers.len() + 1;
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::System,
        format!("Starting VPN connection to {} ({routing_mode} mode, {} server(s) available)...", profile.name, server_count),
    );
    emit_status(&app, VpnStatus::Connecting);

    let manager_arc = state.manager.clone();
    let manager = manager_arc.lock().await;
    let settings = settings_state.lock().await.clone();

    // Collect all servers for smart selector group
    let mut all_servers = vec![profile.clone()];
    all_servers.extend(alternative_servers);

    match manager.start(&app, proxy_port, &routing_mode, &all_servers, &settings).await {
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

                    // Auto-refresh subscriptions now that proxy is available
                    // (fetch may have failed earlier when not connected)
                    {
                        let profile_state = app.state::<ProfileState>();
                        let subs = {
                            let mgr = profile_state.manager.lock().await;
                            mgr.list().to_vec()
                        };
                        for sub in &subs {
                            if !sub.url.is_empty() {
                                let app_clone = app.clone();
                                let id = sub.id.clone();
                                let name = sub.name.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(3)).await;
                                    let logger = app_clone.state::<StdMutex<Logger>>();
                                    log(
                                        &app_clone,
                                        &logger,
                                        LogLevel::Info,
                                        LogSource::Subscription,
                                        format!("Auto-refreshing '{name}' via active proxy..."),
                                    );
                                    let state = app_clone.state::<ProfileState>();
                                    let mut pm = state.manager.lock().await;
                                    if let Ok(profile) = pm.refresh(&app_clone, &id).await {
                                        log(
                                            &app_clone,
                                            &logger,
                                            LogLevel::Success,
                                            LogSource::Subscription,
                                            format!("'{name}' refreshed ({} server(s))", profile.servers.len()),
                                        );
                                    }
                                });
                            }
                        }
                    }

                    // Save for auto-reconnect on unexpected exit
                    {
                        let mut last = crate::vpn::LAST_PROFILE.lock().unwrap();
                        *last = Some(crate::vpn::LastConnection {
                            all_servers: all_servers.clone(),
                            routing_mode: routing_mode.clone(),
                            proxy_port,
                        });
                    }

                    // Watchdog: detect unexpected process exit → auto-reconnect
                    let watch_pid = manager.process_pid();
                    if let Some(pid) = watch_pid {
                        let app_clone = app.clone();
                        let mgr_arc = manager_arc.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                if !is_process_alive(pid) {
                                    // Stop if the user disconnected (LAST_PROFILE cleared)
                                    {
                                        let lock = crate::vpn::LAST_PROFILE.lock().unwrap();
                                        if lock.is_none() {
                                            break; // intentional stop
                                        }
                                    }
                                    // Also check via manager status
                                    {
                                        let mgr = mgr_arc.lock().await;
                                        if mgr.status() == crate::vpn::VpnStatus::Disconnected {
                                            break; // intentional stop
                                        }
                                    }

                                    let logger = app_clone.state::<StdMutex<Logger>>();
                                    log(
                                        &app_clone,
                                        &logger,
                                        LogLevel::Warning,
                                        LogSource::Process,
                                        "Connection lost — reconnecting...".into(),
                                    );

                                    // Clear stale state
                                    let _ = mgr_arc.lock().await.stop(&app_clone).await;

                                    // Auto-reconnect up to 3 times
                                    let last = {
                                        let lock = crate::vpn::LAST_PROFILE.lock().unwrap();
                                        lock.clone()
                                    };
                                    let mut reconnected = false;
                                    if let Some(ref last) = last {
                                        for attempt in 1..=3 {
                                            tokio::time::sleep(Duration::from_secs(attempt * 2)).await;
                                            log(
                                                &app_clone,
                                                &logger,
                                                LogLevel::Info,
                                                LogSource::Process,
                                                format!("Reconnect attempt {attempt}..."),
                                            );

                                            let mgr = mgr_arc.lock().await;
                                            // User might have disconnected while we were sleeping
                                            if mgr.status() == crate::vpn::VpnStatus::Disconnected {
                                                drop(mgr);
                                                break;
                                            }
                                            let settings = app_clone.state::<Mutex<crate::settings::AppSettings>>()
                                                .lock().await.clone();
                                            let ok = mgr
                                                .start(
                                                    &app_clone,
                                                    last.proxy_port,
                                                    &last.routing_mode,
                                                    &last.all_servers,
                                                    &settings,
                                                )
                                                .await
                                                .is_ok();
                                            drop(mgr);

                                            if ok {
                                                log(
                                                    &app_clone,
                                                    &logger,
                                                    LogLevel::Success,
                                                    LogSource::Process,
                                                    "Reconnected successfully".into(),
                                                );
                                                emit_status(&app_clone, VpnStatus::Connected);
                                                reconnected = true;
                                                break;
                                            }
                                        }
                                    }

                                    if !reconnected {
                                        log(
                                            &app_clone,
                                            &logger,
                                            LogLevel::Error,
                                            LogSource::Process,
                                            "Auto-reconnect failed after 3 attempts".into(),
                                        );
                                        emit_status(&app_clone, VpnStatus::Disconnected);
                                    }
                                    break;
                                }
                            }
                        });
                    }

                    // Traffic stats: poll Clash API every second
                    // /traffic returns cumulative bytes — we compute speed as delta/timediff
                    let stats_port = crate::vpn::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
                    if stats_port > 0 {
                        let app_clone = app.clone();
                        let mgr_arc = manager_arc.clone();
                        tokio::spawn(async move {
                            let client = reqwest::Client::builder()
                                .timeout(Duration::from_secs(2))
                                .build()
                                .unwrap_or_default();
                            let url = format!("http://127.0.0.1:{stats_port}/traffic");
                            let mut prev_up: Option<f64> = None;
                            let mut prev_down: Option<f64> = None;
                            let mut prev_time: Option<std::time::Instant> = None;
                            loop {
                                // Stop polling if VPN disconnected
                                {
                                    let mgr = mgr_arc.lock().await;
                                    if mgr.status() == crate::vpn::VpnStatus::Disconnected {
                                        break;
                                    }
                                }
                                // GET /traffic streams newline-delimited JSON (chunked).
                                // Read chunks as they arrive, don't wait for EOF.
                                if let Ok(mut resp) = client.get(&url).send().await {
                                    let mut buf = String::new();
                                    loop {
                                        match resp.chunk().await {
                                            Ok(Some(bytes)) => {
                                                buf.push_str(&String::from_utf8_lossy(&bytes));
                                                while let Some(newline) = buf.find('\n') {
                                                    let line = buf[..newline].trim().to_string();
                                                    buf = buf[newline + 1..].to_string();
                                                    if let Some((up, down)) = parse_traffic_line(&line) {
                                                        let now = std::time::Instant::now();
                                                        let (speed_up, speed_down) = if let (Some(pu), Some(pd), Some(pt)) = (prev_up, prev_down, prev_time) {
                                                            let dt = now.saturating_duration_since(pt).as_secs_f64();
                                                            if dt > 0.001 {
                                                                (((up - pu) / dt).max(0.0), ((down - pd) / dt).max(0.0))
                                                            } else {
                                                                continue; // too fast, keep prev values
                                                            }
                                                        } else {
                                                            (0.0, 0.0) // first reading, no delta yet
                                                        };
                                                        prev_up = Some(up);
                                                        prev_down = Some(down);
                                                        prev_time = Some(now);
                                                        emit_traffic(&app_clone, speed_up, speed_down);
                                                    }
                                                }
                                            }
                                            Ok(None) => break, // stream ended gracefully
                                            Err(_) => break,   // transport error, reconnect
                                        }
                                    }
                                }
                                // On reconnect, reset state so first reading doesn't spike
                                prev_up = None;
                                prev_down = None;
                                prev_time = None;
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                        });
                    }

                    // Background latency scheduler: re-test server pings periodically
                    // using the configured auto_update_interval from settings (default 30 min).
                    let app_clone = app.clone();
                    let mgr_arc_sched = manager_arc.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(15)).await;
                        loop {
                            // Read configured interval (minutes → seconds, default 30 min, min 5 min)
                            let interval_secs = {
                                if let Some(state) = app_clone.try_state::<Mutex<crate::settings::AppSettings>>() {
                                    let settings = state.lock().await;
                                    let minutes = if settings.auto_update_interval < 5 { 5 } else { settings.auto_update_interval };
                                    minutes as u64 * 60
                                } else {
                                    1800
                                }
                            };
                            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
                            // Stop if disconnected
                            {
                                let mgr = mgr_arc_sched.lock().await;
                                if mgr.status() != crate::vpn::VpnStatus::Connected {
                                    break;
                                }
                            }
                            let profile_state = app_clone.state::<ProfileState>();
                            let mut pm = profile_state.manager.lock().await;
                            let profile_ids: Vec<String> = pm.list().iter().map(|p| p.id.clone()).collect();
                            for pid in &profile_ids {
                                let servers = {
                                    let p = pm.list().iter().find(|p| &p.id == pid);
                                    match p {
                                        Some(p) if !p.servers.is_empty() => p.servers.clone(),
                                        _ => continue,
                                    }
                                };
                                let re_pinged = crate::profile::process_servers(&app_clone, servers).await;
                                if let Some(profile) = pm.list_mut().iter_mut().find(|p| &p.id == pid) {
                                    profile.servers = re_pinged;
                                }
                            }
                            let _ = pm.save();
                        }
                    });

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
            // Clear last connection so stale watchdogs don't try to reconnect
            let mut last = crate::vpn::LAST_PROFILE.lock().unwrap();
            *last = None;
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
    mgr.save(&new_settings)?;

    // Handle startWithWindows
    set_start_on_boot(new_settings.start_with_windows);

    log(
        &app,
        &logger,
        LogLevel::Success,
        LogSource::Settings,
        "Settings saved".into(),
    );
    Ok(())
}

/// Set or remove Windows auto-start via registry Run key.
fn set_start_on_boot(enabled: bool) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        let exe_path = std::env::current_exe().unwrap_or_default();
        let exe_str = exe_path.to_string_lossy();

        if enabled {
            let _ = std::process::Command::new("reg")
                .args([
                    "add",
                    "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                    "/v",
                    "OmniTun",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &exe_str,
                    "/f",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        } else {
            let _ = std::process::Command::new("reg")
                .args([
                    "delete",
                    "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                    "/v",
                    "OmniTun",
                    "/f",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = enabled; // no-op on non-Windows
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
            // Sync auto-start at boot (user may have manually removed the Run key)
            set_start_on_boot(settings.start_with_windows);
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

            // ── System tray ────────────────────────────────────────────────
            let show = MenuItem::with_id(app, "show", "Show OmniTun", true, None::<&str>)?;
            let toggle_vpn = MenuItem::with_id(app, "toggle_vpn", "Connect", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

            let menu = Menu::with_items(app, &[
                &show,
                &PredefinedMenuItem::separator(app)?,
                &toggle_vpn,
                &PredefinedMenuItem::separator(app)?,
                &quit,
            ])?;

            let _tray_icon = TrayIconBuilder::new()
                .tooltip("OmniTun")
                .menu(&menu)
                .on_menu_event(move |app, event| {
                    match event.id().as_ref() {
                        "show" => {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        "toggle_vpn" => {
                            // Emit toggle event — frontend handles the connect/disconnect
                            let _ = app.emit("tray-toggle-vpn", ());
                        }
                        "quit" => {
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up, ..
                    } = event {
                        if let Some(window) = tray.app_handle().get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            // Handle window close: minimize to tray if configured
            if let Some(window) = app.get_webview_window("main") {
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        let minimize = if let Some(state) = app_handle
                            .try_state::<Mutex<crate::settings::AppSettings>>()
                        {
                            state.try_lock().map(|g| g.minimize_to_tray).unwrap_or(false)
                        } else {
                            false
                        };
                        if minimize {
                            let _ = app_handle.get_webview_window("main").map(|w| w.hide());
                            api.prevent_close();
                        }
                    }
                });
            }

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
            get_real_ip,
            outlet_ip,
            speed_test,
            get_connections,
            import_subscription_content,
            import_subscription_file,
            initialize_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Parse a single Clash traffic line: {"up":123,"down":456}
fn parse_traffic_line(line: &str) -> Option<(f64, f64)> {
    let line = line.trim();
    let rest = line.strip_prefix("{\"up\":")?;
    let end = rest.find(',')?;
    let up = rest[..end].trim().parse::<f64>().ok()?;
    let tail = &rest[end..];
    let down_start = tail.find(':')?;
    let after = &tail[down_start + 1..];
    let end = after.find('}')?;
    let down = after[..end].trim().parse::<f64>().ok()?;
    Some((up, down))
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

/// Get the user's real IP (before VPN/proxy) with GeoIP location.
/// Always fetches directly (no proxy) — works whether VPN is connected or not.
#[tauri::command]
async fn get_real_ip() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;

    let ip_urls = [
        "http://checkip.amazonaws.com",
        "https://api.ipify.org",
        "https://ifconfig.me/ip",
        "https://icanhazip.com",
    ];

    let mut ip = String::new();
    for url in &ip_urls {
        if let Ok(resp) = client.get(*url).send().await {
            if let Ok(text) = resp.text().await {
                let trimmed = text.trim().to_string();
                if !trimmed.is_empty() {
                    ip = trimmed;
                    break;
                }
            }
        }
    }

    if ip.is_empty() {
        return Err("Could not determine external IP".into());
    }

    // GeoIP location from ip-api.com
    let location = match client.get(format!("http://ip-api.com/json/{ip}")).send().await {
        Ok(resp) => {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                let country = v.get("country").and_then(|c| c.as_str()).unwrap_or("");
                let city = v.get("city").and_then(|c| c.as_str()).unwrap_or("");
                if !country.is_empty() && !city.is_empty() {
                    Some(format!("{}, {}", city, country))
                } else if !country.is_empty() {
                    Some(country.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        }
        Err(_) => None,
    }
    .unwrap_or_default();

    if !location.is_empty() {
        Ok(format!("{} ({})", ip, location))
    } else {
        Ok(ip)
    }
}

/// Find the active proxy's outlet (external) IP address via checkip.amazonaws.com
/// Uses the active proxy port when connected in proxy mode so the request
/// actually goes through the VPN / proxy tunnel.
#[tauri::command]
async fn outlet_ip(
    app: tauri::AppHandle,
) -> Result<String, String> {
    let stats_port = crate::vpn::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        return Err("VPN not connected".into());
    }

    // If sing-box is running in proxy mode, route the IP check through it
    let proxy_port = app
        .try_state::<Mutex<crate::settings::AppSettings>>()
        .and_then(|state| {
            state.try_lock().ok().map(|guard| guard.proxy_port)
        });

    let mut client_builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(8));

    if let Some(port) = proxy_port {
        if let Ok(proxy) = reqwest::Proxy::http(format!("http://127.0.0.1:{port}")) {
            client_builder = client_builder.proxy(proxy);
        }
    }

    let client = client_builder
        .build()
        .map_err(|e| e.to_string())?;

    // Try multiple IP check services (some may be blocked depending on region)
    let ip_urls = [
        "http://checkip.amazonaws.com",
        "https://api.ipify.org",
        "https://ifconfig.me/ip",
        "https://icanhazip.com",
    ];

    let mut ip = String::new();
    for url in &ip_urls {
        if let Ok(resp) = client.get(*url).send().await {
            if let Ok(text) = resp.text().await {
                let trimmed = text.trim().to_string();
                if !trimmed.is_empty() {
                    ip = trimmed;
                    break;
                }
            }
        }
    }

    if ip.is_empty() {
        return Err("Could not determine external IP".into());
    }

    // Try to get location info from ip-api.com
    let location = match client.get(format!("http://ip-api.com/json/{ip}")).send().await {
        Ok(resp) => {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                let country = v.get("country").and_then(|c| c.as_str()).unwrap_or("");
                let city = v.get("city").and_then(|c| c.as_str()).unwrap_or("");
                if !country.is_empty() && !city.is_empty() {
                    Some(format!("{}, {}", city, country))
                } else if !country.is_empty() {
                    Some(country.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        }
        Err(_) => None,
    }
    .unwrap_or_default();

    if !location.is_empty() {
        Ok(format!("{} ({})", ip, location))
    } else {
        Ok(ip)
    }
}

/// Speed test: test latency of all servers via Clash API delay endpoint
#[tauri::command]
async fn speed_test() -> Result<Vec<serde_json::Value>, String> {
    let stats_port = crate::vpn::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        return Err("VPN not connected".into());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let api_url = format!("http://127.0.0.1:{stats_port}");
    let test_url = "http://cp.cloudflare.com/generate_204";

    // First get all proxies
    let groups_resp = client
        .get(format!("{api_url}/proxies"))
        .send()
        .await
        .map_err(|e| format!("Clash API error: {e}"))?;

    let groups: serde_json::Value = groups_resp
        .json()
        .await
        .map_err(|e| format!("Parse error: {e}"))?;

    let mut results = Vec::new();

    // Find all proxy outbounds (not direct, block, or groups)
    if let Some(proxies) = groups.get("proxies").and_then(|p| p.as_object()) {
        for (name, proxy) in proxies {
            let ptype = proxy.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ptype == "Direct" || ptype == "Reject" || ptype == "Selector" || ptype == "URLTest" || ptype == "Fallback" {
                continue;
            }
            // Test each proxy
            let delay_url = format!("{api_url}/proxies/{name}/delay?url={test_url}&timeout=5000");
            if let Ok(resp) = client.get(&delay_url).send().await {
                if let Ok(body) = resp.text().await {
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) {
                        results.push(data);
                    }
                }
            }
        }
    }

    // If no per-proxy results, test the proxy outbound directly
    if results.is_empty() {
        let delay_url = format!("{api_url}/proxies/proxy/delay?url={test_url}&timeout=5000");
        if let Ok(resp) = client.get(&delay_url).send().await {
            if let Ok(body) = resp.text().await {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) {
                    results.push(data);
                }
            }
        }
    }

    Ok(results)
}

/// Get active connections from sing-box Clash API
#[tauri::command]
async fn get_connections() -> Result<Vec<serde_json::Value>, String> {
    let stats_port = crate::vpn::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        return Err("VPN not connected".into());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get(format!("http://127.0.0.1:{stats_port}/connections"))
        .send()
        .await
        .map_err(|e| format!("Clash API error: {e}"))?;

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Parse error: {e}"))?;

    let connections = data["connections"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    Ok(connections)
}

/// Import subscription from raw content (file content or pasted data)
#[tauri::command]
async fn import_subscription_content(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProfileState>,
    name: String,
    content: String,
) -> Result<SubscriptionProfile, String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Importing subscription content: {name}"),
    );

    let servers = crate::subscription::parser::parse_subscription(&content)
        .map_err(|e| format!("Failed to parse subscription: {e}"))?;

    if servers.is_empty() {
        return Err("No servers found in content".into());
    }

    let servers = crate::profile::process_servers(&app, servers).await;

    let profile = SubscriptionProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        url: format!("content://{}", uuid::Uuid::new_v4()),
        servers,
        updated_at: chrono::Utc::now().to_rfc3339(),
        traffic_used: None,
        traffic_total: None,
        traffic_expiry: None,
    };

    let mut mgr = state.manager.lock().await;
    mgr.list_mut().push(profile.clone());
    mgr.save().map_err(|e| e.to_string())?;

    log(
        &app,
        &logger,
        LogLevel::Success,
        LogSource::Subscription,
        format!("Imported {} server(s) from content", profile.servers.len()),
    );

    Ok(profile)
}

/// Import subscription from a local file path
#[tauri::command]
async fn import_subscription_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProfileState>,
    file_path: String,
) -> Result<SubscriptionProfile, String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Importing subscription from file: {file_path}"),
    );

    // Read file content
    let content = tokio::fs::read_to_string(&file_path)
        .await
        .map_err(|e| format!("Failed to read file: {e}"))?;

    // Try to parse as subscription data
    let servers = crate::subscription::parser::parse_subscription(&content)
        .map_err(|e| format!("Failed to parse subscription: {e}"))?;

    if servers.is_empty() {
        return Err("No servers found in file".into());
    }

    let file_name = std::path::Path::new(&file_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Imported")
        .to_string();

    // Use file URL as unique identifier (hash of content)
    let url = format!("file://{}", file_path);

    let mut mgr = state.manager.lock().await;
    let servers = crate::profile::process_servers(&app, servers).await;
    let server_count = servers.len();

    let profile = SubscriptionProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: file_name,
        url,
        servers,
        updated_at: chrono::Utc::now().to_rfc3339(),
        traffic_used: None,
        traffic_total: None,
        traffic_expiry: None,
    };

    mgr.list_mut().push(profile.clone());
    mgr.save().map_err(|e| e.to_string())?;

    log(
        &app,
        &logger,
        LogLevel::Success,
        LogSource::Subscription,
        format!("Imported {server_count} server(s) from file"),
    );

    Ok(profile)
}

/// Initialize app resources: download GeoIP database, ensure sing-box is installed.
/// Call from frontend on startup to show a loading screen.
#[tauri::command]
async fn initialize_app(
    app: tauri::AppHandle,
    state: tauri::State<'_, VpnState>,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::System,
        "Initializing app...".into(),
    );

    // 1. Ensure sing-box is installed (checks for updates if needed)
    let manager = state.manager.lock().await;
    let _ = manager.ensure_installed(&app).await.map_err(|e| e.to_string())?;
    drop(manager);

    // 2. Download GeoIP database
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let geo_dir = data_dir.join("geoip");
    std::fs::create_dir_all(&geo_dir).map_err(|e| e.to_string())?;
    crate::vpn::geoip::ensure_downloaded(&geo_dir).await?;

    log(
        &app,
        &logger,
        LogLevel::Success,
        LogSource::System,
        "App initialized".into(),
    );
    Ok(())
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
