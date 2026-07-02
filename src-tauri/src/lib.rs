#![recursion_limit = "512"]
pub mod events;
pub mod logging;
pub mod profile;
pub(crate) mod settings;
pub mod subscription;
pub mod proxy;

use crate::events::emit_status;
use crate::events::emit_traffic;
use crate::logging::{log, LogLevel, LogSource, Logger, MAX_LOG_FILES_COUNT};
use crate::profile::ProfileManager;
use crate::settings::{AppSettings, SettingsManager};
use crate::subscription::SubscriptionProfile;
use crate::proxy::port_pool::PortPool;
use crate::proxy::cache::ServerCache;
use crate::proxy::{ServerProfile, ProxyManager, ConnectionStatus};
use chrono::Local;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;

const PROXY_TIMEOUT_SECS: u64 = 15;
const PROXY_POLL_MS: u64 = 300;

const IP_URLS: &[&str] = &[
    "https://api.ipify.org",
    "https://whatismyip.akamai.com",
    "http://myip.ipip.net/s",
    "https://www.cloudflare.com/cdn-cgi/trace",
];

struct ProxyState {
    manager: Arc<Mutex<ProxyManager>>,
}

struct ProfileState {
    manager: Arc<Mutex<ProfileManager>>,
}

use tokio_util::sync::CancellationToken;

#[tauri::command(rename_all = "camelCase")]
async fn start_proxy(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProxyState>,
    profile_state: tauri::State<'_, ProfileState>,
    settings_state: tauri::State<'_, Mutex<AppSettings>>,
    profile: ServerProfile,
    routing_mode: String,
    proxy_port: u16,
    alternative_servers: Vec<ServerProfile>,
) -> Result<u16, String> {
    let logger = app.state::<StdMutex<Logger>>();
    let cancel_state = app.state::<StdMutex<CancellationToken>>();
    let cancel = cancel_state.lock().unwrap().clone();
    let server_count = alternative_servers.len() + 1;
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::System,
        format!(
            "Starting proxy connection to {} ({}:{}, {} server(s) available)...",
            profile.display_name(),
            profile.server,
            profile.port,
            server_count
        ),
    );
    emit_status(&app, ConnectionStatus::Connecting);

    // Store active server ID for exit IP enrichment
    {
        let mut active_id = crate::proxy::ACTIVE_SERVER_ID.lock().unwrap_or_else(|e| e.into_inner());
        *active_id = Some(profile.id.clone());
    }

    let manager_arc = state.manager.clone();
    let manager = manager_arc.lock().await;
    let settings = settings_state.lock().await.clone();

    // Collect all servers for smart selector group.
    // Filter out CDN servers from alternatives — they can't be tested via
    // batch proxy and may have unstable outlet IPs, but the main server
    // can still be connected (CDN-fronted proxies are legitimate).
    let mut all_servers = vec![profile.clone()];
    for s in alternative_servers {
        if !s.behind_cdn {
            all_servers.push(s);
        }
    }

    // Check if the target port is free BEFORE starting sing-box.
    // If busy, iterate port+1, port+2, ... up to 100 attempts
    // and warn the user so they can reconfigure their apps.
    let proxy_port = if crate::proxy::config::port_available(proxy_port) {
        log(
            &app,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            format!("Port {proxy_port} is available"),
        );
        proxy_port
    } else {
        let mut found = None;
        for offset in 1..=100 {
            let candidate = proxy_port.saturating_add(offset);
            if candidate == 0 { break; } // wraparound guard
            if crate::proxy::config::port_available(candidate) {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(new_port) => {
                log(
                    &app,
                    &logger,
                    LogLevel::Warning,
                    LogSource::Proxy,
                    format!("Port {proxy_port} is in use (by another app), using {new_port} instead — update your apps to use port {new_port}"),
                );
                new_port
            }
            None => {
                log(
                    &app,
                    &logger,
                    LogLevel::Error,
                    LogSource::Proxy,
                    format!("Port {proxy_port} is in use and no alternative port found in the next 100 ports — using it anyway"),
                );
                proxy_port
            }
        }
    };

    match manager
        .start(&app, proxy_port, &routing_mode, &all_servers, &settings)
        .await
    {
        Ok(_) => {
            log(
                &app,
                &logger,
                LogLevel::Info,
                LogSource::Proxy,
                "Verifying proxy connection...".into(),
            );

            let result = match manager.process_pid() {
                Some(pid) => wait_for_proxy_ready(pid, proxy_port).await,
                None => Err("no PID".into()),
            };
            match result {
                Ok(attempts) => {
                    log(
                        &app,
                        &logger,
                        LogLevel::Info,
                        LogSource::Proxy,
                        format!("Proxy ready after {attempts} attempt(s)"),
                    );
                    log(
                        &app,
                        &logger,
                        LogLevel::Success,
                        LogSource::Proxy,
                        format!("Proxy connected to {}", profile.display_name()),
                    );
                    emit_status(&app, ConnectionStatus::Connected);

                    // Verify proxy actually routes traffic — make real request through it.
                    // Check CDN status from the profile manager (where enrich_loaded
                    // sets it based on GeoIP/CIDR lookup), NOT from the frontend's
                    // profile which may have stale/missing behind_cdn.
                    let is_cdn = {
                        let pm = profile_state.manager.lock().await;
                        pm.list().iter().any(|p| p.servers.iter().any(|s| s.id == profile.id && s.behind_cdn))
                    };
                    if is_cdn {
                        log(
                            &app,
                            &logger,
                            LogLevel::Info,
                            LogSource::Proxy,
                            format!(
                                "CDN server {} — skipping outlet IP verification (dynamic exit IP)",
                                profile.display_name(),
                            ),
                        );
                        // Log confirmation that CDN was detected via profile manager
                        // (the frontend deliberately doesn't send behind_cdn, so the
                        // profile's `behind_cdn` field from JSON defaults to false).
                        if !profile.behind_cdn {
                            log(
                                &app,
                                &logger,
                                LogLevel::Info,
                                LogSource::Proxy,
                                format!(
                                    "[CDN] frontend profile had behind_cdn=false (default) — corrected to true via profile manager for {}",
                                    profile.display_name(),
                                ),
                            );
                        }
                    } else {
                        let mut cb = reqwest::Client::builder().timeout(Duration::from_secs(5));
                        // In proxy modes traffic goes through the mixed inbound — route through proxy
                        if let Ok(proxy) =
                                reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}"))
                            {
                                cb = cb.proxy(proxy);
                            }
                        let check_client = cb.build().ok();
                        if let Some(c) = check_client {
                            let mut got_response = false;
                            for url in IP_URLS {
                                if let Ok(resp) = c.get(*url).send().await {
                                    if resp.status().is_success() {
                                        if let Ok(text) = resp.text().await {
                                            let ip = text.trim().to_string();
                                            if !ip.is_empty()
                                                && ip
                                                    .chars()
                                                    .all(|c| c.is_ascii_digit() || c == '.')
                                            {
                                                got_response = true;

                                                if crate::proxy::geoip::is_cdn_entry_ip(&ip) {
                                                    log(
                                                        &app,
                                                        &logger,
                                                        LogLevel::Warning,
                                                        LogSource::Proxy,
                                                        format!("[BUG] start_proxy: Server {} was checked for outlet IP ({}), but it belongs to a CDN!", profile.id, ip)
                                                    );
                                                    let _ = app.emit("exit-ip-changed", ExitIpInfo {
                                                        server: String::new(),
                                                        flag: String::new(),
                                                        country_name: String::new(),
                                                        city_name: String::new(),
                                                        proxy_tag: String::new(),
                                                    });
                                                } else {
                                                    log(
                                                        &app,
                                                        &logger,
                                                        LogLevel::Info,
                                                        LogSource::Proxy,
                                                        format!("Proxy verified (exit IP: {ip})"),
                                                    );
                                                    let _ = app.emit("exit-ip-changed", ExitIpInfo {
                                                        server: ip.clone(),
                                                        flag: String::new(),
                                                        country_name: String::new(),
                                                        city_name: String::new(),
                                                        proxy_tag: String::new(),
                                                    });
                                                }
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            if !got_response {
                                // exit IP verification failed — server may be CDN.
                                // Try alternative servers via Clash API switch.
                                let mut switched = false;
                                let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
                                if stats_port > 0 && all_servers.len() > 1 {
                                    for idx in 1..all_servers.len() {
                                        let tag = format!("proxy-{idx}");
                                        log(
                                            &app,
                                            &logger,
                                            LogLevel::Info,
                                            LogSource::Proxy,
                                            format!("Switching to {tag} ({})...", all_servers[idx].display_name()),
                                        );
                                        let switch_url = format!("http://127.0.0.1:{stats_port}/proxies/proxy");
                                        let body = serde_json::json!({ "name": tag });
                                        let client = reqwest::Client::new();
                                        if client.put(&switch_url).json(&body).send().await.is_ok() {
                                            tokio::time::sleep(Duration::from_secs(2)).await;
                                            // Re-verify exit IP through the new server
                                            let mut cb = reqwest::Client::builder().timeout(Duration::from_secs(5));
                                            if let Ok(proxy) =
                                                    reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}"))
                                                {
                                                    cb = cb.proxy(proxy);
                                                }
                                            if let Some(c) = cb.build().ok() {
                                                for url in IP_URLS {
                                                    if let Ok(resp) = c.get(*url).send().await {
                                                        if resp.status().is_success() {
                                                            if let Ok(text) = resp.text().await {
                                                                let ip = text.trim().to_string();
                                                                if !ip.is_empty()
                                                                    && ip.chars().all(|c| c.is_ascii_digit() || c == '.')
                                                                {
                                                                    if crate::proxy::geoip::is_cdn_entry_ip(&ip) {
                                                                        log(&app, &logger, LogLevel::Warning, LogSource::Proxy,
                                                                            format!("[BUG] start_proxy (fallback): Server {} was checked for outlet IP ({}), but it belongs to a CDN!", all_servers[idx].id, ip)
                                                                        );
                                                                    } else {
                                                                        log(&app, &logger, LogLevel::Info, LogSource::Proxy,
                                                                            format!("Proxy verified (exit IP: {ip}) via {tag}"),
                                                                        );
                                                                    }
                                                                    switched = true;
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            if switched {
                                                break;
                                            }
                                        }
                                    }
                                }
                                if !switched {
                                    log(&app, &logger, LogLevel::Error, LogSource::Proxy,
                                        "Proxy connection verified but exit IP unavailable — traffic may not be routing through proxy".into());
                                }
                            }
                        }
                    }

                    // Save for auto-reconnect on unexpected exit
                    {
                        let mut last = crate::proxy::LAST_PROFILE.lock().unwrap_or_else(|e| e.into_inner());
                        *last = Some(crate::proxy::LastConnection {
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
                        let cancel_watch = cancel.clone();
                        tokio::spawn(async move {
                            loop {
                                if cancel_watch.is_cancelled() { break; }
                                tokio::time::sleep(Duration::from_secs(2)).await;

                                // Guard: If this watchdog is no longer tracking the active PID, exit.
                                // This prevents obsolete watchdogs from triggering on old PIDs after a reconnect or manual disconnect.
                                {
                                    let mgr = mgr_arc.lock().await;
                                    if mgr.process_pid() != Some(pid) {
                                        break;
                                    }
                                }

                                if !is_process_alive(pid) {
                                    // Stop if the user disconnected (LAST_PROFILE cleared)
                                    {
                                        let lock = crate::proxy::LAST_PROFILE.lock().unwrap_or_else(|e| e.into_inner());
                                        if lock.is_none() {
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

                                    // Auto-reconnect up to 3 times.
                                    // Note: we do NOT call mgr.stop() here — the process is already dead,
                                    // and mgr.stop() would clear the PID guard causing mgr.status()
                                    // to always return Disconnected.
                                    let last = {
                                        let lock = crate::proxy::LAST_PROFILE.lock().unwrap_or_else(|e| e.into_inner());
                                        lock.clone()
                                    };
                                    let mut reconnected = false;
                                    if let Some(ref last) = last {
                                        for attempt in 1..=3 {
                                            tokio::time::sleep(Duration::from_secs(attempt * 2))
                                                .await;

                                            // User might have disconnected while we were sleeping
                                            {
                                                let lock = crate::proxy::LAST_PROFILE.lock().unwrap_or_else(|e| e.into_inner());
                                                if lock.is_none() {
                                                    break;
                                                }
                                            }

                                            log(
                                                &app_clone,
                                                &logger,
                                                LogLevel::Info,
                                                LogSource::Process,
                                                format!("Reconnect attempt {attempt}..."),
                                            );

                                            let mgr = mgr_arc.lock().await;
                                            let settings = app_clone
                                                .state::<Mutex<crate::settings::AppSettings>>()
                                                .lock()
                                                .await
                                                .clone();
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
                                                emit_status(&app_clone, ConnectionStatus::Connected);
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
                                        emit_status(&app_clone, ConnectionStatus::Disconnected);
                                    }
                                    break;
                                }
                            }
                        });
                    }

                    // Traffic stats: poll Clash API every second
                    // /traffic returns cumulative bytes — we compute speed as delta/timediff
                    let stats_port =
                        crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
                    if stats_port > 0 {
                        let app_clone = app.clone();
                        let mgr_arc = manager_arc.clone();
                        let cancel_stats = cancel.clone();
                        tokio::spawn(async move {
                            let client = reqwest::Client::builder()
                                .no_proxy()
                                .timeout(Duration::from_secs(2))
                                .build()
                                .unwrap_or_default();
                            let url = format!("http://127.0.0.1:{stats_port}/traffic");
                            let mut prev_up: Option<f64> = None;
                            let mut prev_down: Option<f64> = None;
                            let mut prev_time: Option<std::time::Instant> = None;
                            loop {
                                if cancel_stats.is_cancelled() { break; }
                                // Stop polling if proxy disconnected
                                {
                                    let mgr = mgr_arc.lock().await;
                                    if mgr.status() == crate::proxy::ConnectionStatus::Disconnected {
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
                                                    if let Some((up, down)) =
                                                        parse_traffic_line(&line)
                                                    {
                                                        let now = std::time::Instant::now();
                                                        let (speed_up, speed_down) =
                                                            if let (Some(pu), Some(pd), Some(pt)) =
                                                                (prev_up, prev_down, prev_time)
                                                            {
                                                                let dt = now
                                                                    .saturating_duration_since(pt)
                                                                    .as_secs_f64();
                                                                if dt > 0.001 {
                                                                    (
                                                                        ((up - pu) / dt).max(0.0),
                                                                        ((down - pd) / dt).max(0.0),
                                                                    )
                                                                } else {
                                                                    continue; // too fast, keep prev values
                                                                }
                                                            } else {
                                                                (0.0, 0.0) // first reading, no delta yet
                                                            };
                                                        prev_up = Some(up);
                                                        prev_down = Some(down);
                                                        prev_time = Some(now);
                                                        emit_traffic(
                                                            &app_clone, speed_up, speed_down,
                                                        );
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

                    Ok(proxy_port)
                }
                Err(e) => {
                    let _ = manager.stop(&app).await;
                    let msg = format!("Proxy not reachable: {e}");
                    log(&app, &logger, LogLevel::Error, LogSource::Proxy, msg.clone());
                    emit_status(&app, ConnectionStatus::Error(msg.clone()));
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
                LogSource::Proxy,
                format!("Connection failed: {msg}"),
            );
            emit_status(&app, ConnectionStatus::Error(msg.clone()));
            Err(msg)
        }
    }
}

#[tauri::command]
async fn stop_proxy(app: tauri::AppHandle, state: tauri::State<'_, ProxyState>) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    let log_stop = |level: LogLevel, msg: String| {
        log(&app, &logger, level, LogSource::System, msg);
    };

    log_stop(LogLevel::Info, "[stop_proxy] called".into());

    // Step 1: Clear LAST_PROFILE so watchdog stops.
    {
        let mut last = crate::proxy::LAST_PROFILE.lock().unwrap_or_else(|e| e.into_inner());
        *last = None;
    }
    log_stop(LogLevel::Info, "[stop_proxy] step 1: LAST_PROFILE cleared".into());

    // Step 2: Reset Clash API port immediately so health check stops.
    crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Release);
    log_stop(LogLevel::Info, "[stop_proxy] step 2: CLASH_API_PORT=0".into());

    // Step 3: Kill sing-box by PID + fallback by process name.
    let pid = crate::proxy::PROXY_PID.swap(0, std::sync::atomic::Ordering::AcqRel);
    log_stop(LogLevel::Info, format!("[stop_proxy] step 3: PROXY_PID={pid} — killing..."));
    if pid > 0 {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            if let Err(e) = std::process::Command::new("taskkill")
                .args(["/pid", &pid.to_string(), "/f"])
                .creation_flags(0x08000000)
                .output()
            {
                log_stop(LogLevel::Warning, format!("[stop_proxy] taskkill (pid={pid}) failed: {e}"));
            } else {
                log_stop(LogLevel::Info, format!("[stop_proxy] taskkill /pid {pid} OK"));
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    } else {
        log_stop(LogLevel::Warning, "[stop_proxy] PID was 0 — process already dead?".into());
    }

    log_stop(LogLevel::Info, "[stop_proxy] sing-box killed".into());

    // Step 4: Best-effort manager cleanup.
    if let Ok(mgr) = state.manager.try_lock() {
        mgr.clear_pid();
        log_stop(LogLevel::Info, "[stop_proxy] step 4: manager PID cleared".into());
    } else {
        log_stop(LogLevel::Warning, "[stop_proxy] step 4: manager lock busy — skip".into());
    }

    // Step 5: Log and emit status.
    log_stop(LogLevel::Success, "[stop_proxy] emitting Disconnected event & returning Ok".into());
    crate::proxy::remove_pid_file();
    emit_status(&app, ConnectionStatus::Disconnected);

    Ok(())
}

/// Log from the frontend (stores.ts addLog → invoke('log_frontend'))
/// so that [toggle] and [evt:connection-status] messages appear in the JSONL file
/// alongside Rust backend logs, making debugging much easier.
///
/// NOTE: does NOT emit `log-entry` event — the frontend already called `addLog`
/// for this message. Emitting would create a duplicate in the frontend store.
#[tauri::command]
async fn log_frontend(
    app: tauri::AppHandle,
    message: String,
    level: String,
    source: String,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    let log_level = match level.as_str() {
        "error" => LogLevel::Error,
        "warning" => LogLevel::Warning,
        "success" => LogLevel::Success,
        _ => LogLevel::Info,
    };
    let log_source = match source.as_str() {
        "proxy" => LogSource::Proxy,
        "process" => LogSource::Process,
        "settings" => LogSource::Settings,
        "subscription" => LogSource::Subscription,
        "cache" => LogSource::Cache,
        // "system" is default
        _ => LogSource::System,
    };
    crate::logging::log_to_file(
        &app,
        &logger,
        log_level,
        log_source,
        message,
    );
    Ok(())
}

#[tauri::command]
async fn get_status(state: tauri::State<'_, ProxyState>) -> Result<ConnectionStatus, String> {
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

/// Filter servers by country & protocol settings.
/// Country filter uses exit IP only — CDN servers have no reliable exit IP.
/// When country filtering is active (allowed/blocked countries non-empty),
/// CDN servers are automatically excluded: their exit IP is unpredictable,
/// making country filtering meaningless for them.
fn filter_servers_by_settings(
    servers: &[ServerProfile],
    settings: &AppSettings,
) -> Vec<ServerProfile> {
    let has_country_filter = !settings.allowed_countries.is_empty()
        || !settings.blocked_countries.is_empty();

    servers
        .iter()
        .filter(|s| {
            // Protocol filter
            if !settings.allowed_protocols.is_empty()
                && !settings.allowed_protocols.contains(&s.protocol)
            {
                return false;
            }
            if !settings.blocked_protocols.is_empty()
                && settings.blocked_protocols.contains(&s.protocol)
            {
                return false;
            }
            // When country filtering is active, CDN servers are excluded:
            // they have no reliable exit IP for meaningful country matching.
            if has_country_filter && s.behind_cdn {
                return false;
            }
            // Country filter: only exit IP, never entry IP (flag).
            if let Some(flag) = s.exit_flag.as_deref() {
                if !settings.allowed_countries.is_empty()
                    && !settings.allowed_countries.contains(&flag.to_string())
                {
                    return false;
                }
                if !settings.blocked_countries.is_empty()
                    && settings.blocked_countries.contains(&flag.to_string())
                {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

/// Pick the best replacement server when the current one dies during health check.
/// Priority: same country (exit flag) → fastest any country (respecting user filters).
/// Uses `exit_flag` (resolved by batch test via the active proxy), matching backend filtering.
/// This moves the selection logic from the frontend to the backend.
#[tauri::command]
async fn pick_replacement_server(
    app: tauri::AppHandle,
    profile_state: tauri::State<'_, ProfileState>,
    settings_state: tauri::State<'_, Mutex<AppSettings>>,
    current_server_id: String,
) -> Result<Option<ServerProfile>, String> {
    let logger = app.state::<StdMutex<Logger>>();
    let settings = settings_state.lock().await;

    let pm = profile_state.manager.lock().await;
    let all_servers: Vec<ServerProfile> = pm.list().iter().flat_map(|p| p.servers.iter().cloned()).collect();
    let total = all_servers.len();

    // Apply user filters (country, protocol, CDN)
    let mut candidates = filter_servers_by_settings(&all_servers, &settings);

    // Exclude the dead server itself
    candidates.retain(|s| s.id != current_server_id);

    if candidates.is_empty() {
        log(
            &app,
            &logger,
            LogLevel::Warning,
            LogSource::System,
            format!("[reconnect] no replacement server available (filtered {total} → 0 candidates) — disconnecting"),
        );
        return Ok(None);
    }

    // Find current server's exit country for same-country preference
    let current_country = all_servers.iter()
        .find(|s| s.id == current_server_id)
        .and_then(|s| s.exit_flag.as_deref());

    // Prefer same country (exit flag), fastest ping
    if let Some(country) = current_country {
        if let Some(best) = candidates.iter()
            .filter(|s| s.exit_flag.as_deref() == Some(country))
            .min_by_key(|s| s.ping_ms.unwrap_or(u64::MAX))
        {
            log(
                &app,
                &logger,
                LogLevel::Info,
                LogSource::System,
                format!("[reconnect] picked {}/{} (same country {country}, ping {}) from {total} servers",
                    best.display_name(), best.id, best.ping_ms.unwrap_or(0)),
            );
            return Ok(Some(best.clone()));
        }
    }

    // Fallback: fastest any country
    if let Some(best) = candidates.iter()
        .min_by_key(|s| s.ping_ms.unwrap_or(u64::MAX))
    {
        log(
            &app,
            &logger,
            LogLevel::Info,
            LogSource::System,
            format!("[reconnect] picked {}/{} (any country, ping {}) from {total} servers",
                best.display_name(), best.id, best.ping_ms.unwrap_or(0)),
        );
        return Ok(Some(best.clone()));
    }

    Ok(None)
}

#[tauri::command]
async fn get_subscriptions(
    state: tauri::State<'_, ProfileState>,
    settings_state: tauri::State<'_, Mutex<AppSettings>>,
) -> Result<Vec<SubscriptionProfile>, String> {
    let mgr = state.manager.lock().await;
    let settings = settings_state.lock().await;
    let mut profiles = mgr.list().to_vec();
    for profile in &mut profiles {
        profile.servers = filter_servers_by_settings(&profile.servers, &settings);
    }
    Ok(profiles)
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

    // Emit tcp_ping progress phase
    let _ = crate::events::emit_refresh_progress(
        &app,
        crate::events::RefreshProgress {
            profile_id: id.clone(),
            total: 0,
            tested: 0,
            cache_hits: 0,
            successful: 0,
            failed: 0,
            phase: "tcp_ping".into(),
        },
    );

    let mut mgr = state.manager.lock().await;
    match mgr.refresh(&app, &id).await {
        Ok(profile) => {
            let count = profile.servers.len();

            // Emit done phase
            let _ = crate::events::emit_refresh_progress(
                &app,
                crate::events::RefreshProgress {
                    profile_id: id,
                    total: count,
                    tested: count,
                    cache_hits: 0,
                    successful: count,
                    failed: 0,
                    phase: "done".into(),
                },
            );

            log(
                &app,
                &logger,
                LogLevel::Success,
                LogSource::Subscription,
                format!(
                    "Subscription '{}' refreshed ({count} server(s))",
                    profile.name
                ),
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

#[tauri::command(rename_all = "camelCase")]
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

        let exe_path = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Cannot get exe path: {e}");
                return;
            }
        };
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
/// Cancel the current subscription refresh.
#[tauri::command]
async fn cancel_refresh(app: tauri::AppHandle) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Cache,
        "Cancelling refresh...".into(),
    );
    if let Some(cancel_guard) = app.try_state::<StdMutex<tokio_util::sync::CancellationToken>>() {
        if let Ok(mut token) = cancel_guard.lock() {
            let new_token = tokio_util::sync::CancellationToken::new();
            let old_token = std::mem::replace(&mut *token, new_token);
            old_token.cancel();
            log(
                &app,
                &logger,
                LogLevel::Info,
                LogSource::Cache,
                "Refresh cancelled".into(),
            );
        }
    }
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let data_dir = match app.path().app_data_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[FATAL] Failed to get app data dir: {e}");
                    std::process::exit(1);
                }
            };

            if let Err(e) = std::fs::create_dir_all(&data_dir) {
                eprintln!("[FATAL] Failed to create app data dir: {e}");
                std::process::exit(1);
            }

            // Set up logging
            let log_dir = data_dir.join("logs");
            if let Err(e) = std::fs::create_dir_all(&log_dir) {
                eprintln!("[FATAL] Failed to create logs dir: {e}");
                std::process::exit(1);
            }
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

            // Store sing-box binary path for .srs parsing
            let singbox_path = data_dir.join("binaries").join("sing-box.exe");
            crate::proxy::geoip::set_singbox_bin_path(singbox_path);

            // PID file for orphan cleanup
            crate::proxy::set_pid_file_path(&data_dir);
            crate::proxy::cleanup_stale_pid(app.handle());

            let proxy_manager = ProxyManager::new(data_dir.clone());
            let proxy_state = ProxyState {
                manager: Arc::new(Mutex::new(proxy_manager)),
            };
            app.manage(proxy_state);

            // ── Server cache (dual-layer with batcher) ──────────────────────
            let cache = Arc::new(ServerCache::new(&data_dir));
            let port_pool = Arc::new(PortPool::new());
            let cancel_token = tokio_util::sync::CancellationToken::new();

            // Start the background batching engine
            cache.start_batcher(
                port_pool.clone(),
                app.handle().clone(),
                cancel_token.clone(),
            );

            app.manage(cache.clone());
            app.manage(port_pool.clone());
            app.manage(StdMutex::new(cancel_token));

            log(
                app.handle(),
                &StdMutex::new(Logger::new(1, None)),
                LogLevel::Info,
                LogSource::System,
                "Server cache and batch engine initialized".into(),
            );

            let settings_mgr = SettingsManager::new(data_dir.clone());
            let settings = settings_mgr.load().unwrap_or_else(|e| {
                eprintln!("[WARN] Failed to load settings: {e} — using defaults");
                AppSettings::default()
            });
            // Sync auto-start at boot (user may have manually removed the Run key)
            set_start_on_boot(settings.start_with_windows);
            app.manage(Mutex::new(settings_mgr));
            app.manage(Mutex::new(settings));

            let mut profile_manager = ProfileManager::new(data_dir);
            profile_manager.load().unwrap_or_else(|e| {
                log::error!("Failed to load profiles: {e} — starting with empty profile list");
                // logger not available yet, but we can continue with empty
            });
            let profile_state = ProfileState {
                manager: Arc::new(Mutex::new(profile_manager)),
            };
            app.manage(profile_state);

            // ── System tray ────────────────────────────────────────────────
            let show = MenuItem::with_id(app, "show", "Show OmniTun", true, None::<&str>)?;
            let toggle = MenuItem::with_id(app, "toggle_proxy", "Connect", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

            let menu = Menu::with_items(
                app,
                &[
                    &show,
                    &PredefinedMenuItem::separator(app)?,
                    &toggle,
                    &PredefinedMenuItem::separator(app)?,
                    &quit,
                ],
            )?;

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
                        "toggle_proxy" => {
                            // Emit toggle event — frontend handles the connect/disconnect
                            let _ = app.emit("tray-toggle-proxy", ());
                        }
                        "quit" => {
                            // Cancel background tasks so AppHandle clones drop
                            // and WebView2 Manager can deallocate properly.
                            if let Some(cancel) = app.try_state::<std::sync::Mutex<CancellationToken>>() {
                                if let Ok(cancel) = cancel.lock() {
                                    cancel.cancel();
                                }
                            }
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
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
                        // Kill our own proxy PID only — NOT all sing-box.exe
                        // by name, because other applications may use sing-box
                        // and the batcher runs its own sing-box instances.
                        let pid = crate::proxy::PROXY_PID.load(std::sync::atomic::Ordering::Acquire);
                        if pid > 0 {
                            log::info!("[cleanup] killing our proxy (PID {pid})");
                            #[cfg(target_os = "windows")]
                            let _ = std::process::Command::new("taskkill")
                                .args(["/pid", &pid.to_string(), "/f"])
                                .output();
                            #[cfg(not(target_os = "windows"))]
                            let _ = std::process::Command::new("kill")
                                .args(["-9", &pid.to_string()])
                                .output();
                            log::info!("[cleanup] done");
                        } else {
                            log::info!("[cleanup] no proxy PID to kill");
                        }

                        let minimize = if let Some(state) =
                            app_handle.try_state::<Mutex<crate::settings::AppSettings>>()
                        {
                            state
                                .try_lock()
                                .map(|g| g.minimize_to_tray)
                                .unwrap_or(false)
                        } else {
                            false
                        };
                        if minimize {
                            let _ = app_handle.get_webview_window("main").map(|w| w.hide());
                            api.prevent_close();
                        } else {
                            // User is closing the window — cancel background tasks so
                            // AppHandle clones drop and WebView2 Manager can deallocate.
                            if let Some(cancel) =
                                app_handle.try_state::<std::sync::Mutex<CancellationToken>>()
                            {
                                if let Ok(cancel) = cancel.lock() {
                                    cancel.cancel();
                                }
                            }
                        }
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![




            start_proxy,
            stop_proxy,
            get_status,
            log_frontend,
            add_subscription,
            remove_subscription,
            get_subscriptions,
            refresh_subscription,
            get_settings,
            update_settings,
            cancel_refresh,
            logging::get_logs,
            logging::log_message,
            read_clipboard_text,
            get_real_ip,
            outlet_ip,
            speed_test,
            check_proxy_health,
            switch_proxy,
            get_connections,
            import_subscription_content,
            import_subscription_file,
            initialize_app,
            download_ipinfo_db,
            get_geoip_status,
            pick_replacement_server,
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
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("Clipboard init: {e}"))?;
    clipboard
        .get_text()
        .map_err(|e| format!("Clipboard read: {e}"))
}

/// Get the user's real IP (before proxy) with GeoIP location.
/// Always fetches directly (no proxy) — works whether connected or not.
#[tauri::command]
async fn get_real_ip(app: tauri::AppHandle) -> Result<String, String> {
    let _logger = app.state::<StdMutex<Logger>>();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;

    let mut ip = String::new();
    for url in IP_URLS {
        if let Ok(resp) = client.get(*url).send().await {
            if let Ok(text) = resp.text().await {
                // Try to extract ip=... line (Cloudflare trace)
                let extracted = text.lines()
                    .find_map(|l| l.strip_prefix("ip="))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| text.trim().to_string());
                if !extracted.is_empty() {
                    ip = extracted;
                    break;
                }
            }
        }
    }

    if ip.is_empty() {
        return Err("Could not determine external IP".into());
    }

    // GeoIP location from local GeoLite2 database (no network call, no rate limit)
    let location = crate::proxy::geoip::resolve(&ip)
        .map(|geo| {
            if !geo.city.is_empty() {
                format!("{}, {}", geo.city, geo.country_name)
            } else {
                geo.country_name
            }
        })
        .unwrap_or_default();

    if !location.is_empty() {
        Ok(format!("{} ({})", ip, location))
    } else {
        Ok(ip)
    }
}

/// Find the active proxy's outlet (external) IP address via IP echo services.
/// Uses the active proxy port when connected in proxy mode so the request
/// actually goes through the proxy tunnel.
/// If the active server is CDN-routed, returns immediately (CDN servers
/// cannot route proxy traffic and have no meaningful exit IP).
static PREV_OUTLET_IP: StdMutex<Option<String>> = StdMutex::new(None);

#[derive(Debug, Clone, Serialize)]
pub struct ExitIpInfo {
    pub server: String,
    pub flag: String,
    pub country_name: String,
    pub city_name: String,
    /// The sing-box outbound tag that handled the request (e.g. "proxy-5").
    /// Empty if not available.
    pub proxy_tag: String,
}

#[tauri::command]
async fn outlet_ip(app: tauri::AppHandle) -> Result<ExitIpInfo, String> {
    let stats_port = crate::proxy::CLASH_API_PORT.load(Ordering::Relaxed);
    if stats_port == 0 {
        return Err("Proxy not connected".into());
    }

    let logger = app.state::<StdMutex<Logger>>();

    // ── Early CDN check ────────────────────────────────────────────────
    // If the active server is behind CDN, exit IP is meaningless (dynamic
    // outlet IP per request). Log a [BUG] warning so the JSONL file has a
    // record of this — previously this check was only on the frontend.
    let active_id = crate::proxy::ACTIVE_SERVER_ID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(ref active_id) = active_id {
        if let Some(profile_state) = app.try_state::<ProfileState>() {
            let pm = profile_state.manager.lock().await;
            let is_cdn = pm.list().iter().any(|p| {
                p.servers
                    .iter()
                    .any(|s| s.id == *active_id && s.behind_cdn)
            });
            if is_cdn {
                log(
                    &app,
                    &logger,
                    LogLevel::Warning,
                    LogSource::System,
                    format!(
                        "[BUG] outlet_ip called for CDN server {} — outlet IP is meaningless for CDN",
                        active_id,
                    ),
                );
                return Ok(ExitIpInfo {
                    server: String::new(),
                    flag: String::new(),
                    country_name: String::new(),
                    city_name: String::new(),
                    proxy_tag: String::new(),
                });
            }
        }
    }

    // Always route through proxy in proxy modes
    let mut use_proxy = true;
    let mut proxy_port = 0u16;
    if let Some(state) = app.try_state::<Mutex<crate::settings::AppSettings>>() {
        if let Ok(guard) = state.try_lock() {
            use_proxy = true;
            proxy_port = guard.proxy_port;
        }
    }

    let mut client_builder = reqwest::Client::builder().timeout(Duration::from_secs(8));

    if use_proxy {
        if let Ok(proxy) = reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}")) {
            client_builder = client_builder.proxy(proxy);
        }
    }

    let client = client_builder.build().map_err(|e| e.to_string())?;

    let mut ip = String::new();

    // Concurrently check all URLs, return first valid IP
    let mut set = tokio::task::JoinSet::new();
    for &url in IP_URLS {
        let client = client.clone();
        set.spawn(async move {
            if let Ok(resp) = client.get(url).send().await {
                if resp.status().is_success() {
                    if let Ok(text) = resp.text().await {
                        // Try to extract ip=... (Cloudflare trace)
                        let extracted = text.lines()
                            .find_map(|l| l.strip_prefix("ip="))
                            .map(|s| s.trim().to_string())
                            .unwrap_or_else(|| text.trim().to_string());
                        if !extracted.is_empty() {
                            return Some(extracted);
                        }
                    }
                }
            }
            None
        });
    }

    while let Some(res) = set.join_next().await {
        if let Ok(Some(found)) = res {
            ip = found;
            break;
        }
    }

    if ip.is_empty() {
        return Err("Could not determine external IP".into());
    }

    if crate::proxy::geoip::is_cdn_entry_ip(&ip) {
        log(
            &app,
            &logger,
            LogLevel::Warning,
            LogSource::System,
            format!("[BUG] outlet_ip: Fetched outlet IP {} belongs to a CDN, but active server was not marked as CDN!", ip)
        );
    }

    let mut proxy_tag = String::new();

    // Query the Clash API for the currently selected proxy outbound tag
    if stats_port != 0 {
        let api_url = format!("http://127.0.0.1:{stats_port}/proxies/proxy");
        if let Ok(resp) = reqwest::get(&api_url).await {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                if let Some(current) = body.get("now").and_then(|v| v.as_str()) {
                    proxy_tag = current.to_string();
                }
            }
        }
    }

    let info = match crate::proxy::geoip::resolve(&ip) {
        Some(geo) => ExitIpInfo {
            server: ip.clone(),
            flag: geo.country_code.clone(),
            country_name: geo.country_name.clone(),
            city_name: geo.city.clone(),
            proxy_tag: proxy_tag.clone(),
        },
        None => ExitIpInfo {
            server: ip.clone(),
            flag: String::new(),
            country_name: String::new(),
            city_name: String::new(),
            proxy_tag,
        },
    };

    // Persist exit IP geoip to the active server in profiles.json
    let active_id = crate::proxy::ACTIVE_SERVER_ID.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(ref server_id) = active_id {
        if let Some(profile_state) = app.try_state::<ProfileState>() {
            let mut pm = profile_state.manager.lock().await;
            let mut saved = false;
            for profile in pm.list_mut() {
                for server in &mut profile.servers {
                    if server.id == *server_id {
                        // CDN servers have unstable outlet IPs — skip save
                        if server.behind_cdn {
                            saved = true;
                            break;
                        }

                        let cc = if info.flag.is_empty() {
                            None
                        } else {
                            Some(info.flag.clone())
                        };
                        let cn = if info.country_name.is_empty() {
                            None
                        } else {
                            Some(info.country_name.clone())
                        };
                        let city = if info.city_name.is_empty() {
                            None
                        } else {
                            Some(info.city_name.clone())
                        };
                        server.exit_server = Some(info.server.clone());
                        server.exit_flag = cc;
                        server.exit_country_name = cn;
                        server.exit_city_name = city;
                        saved = true;
                        break;
                    }
                }
                if saved {
                    break;
                }
            }
            if saved {
                if let Err(e) = pm.save() {
                    log(&app, &logger, LogLevel::Warning, LogSource::Settings, format!("save failed: {e}"));
                }
            }
        }
    }

    {
        let mut prev = PREV_OUTLET_IP.lock().unwrap_or_else(|e| e.into_inner());
        if prev.as_deref() != Some(&ip) {
            let logger = app.state::<StdMutex<Logger>>();
            log(
                &app,
                &logger,
                LogLevel::Info,
                LogSource::System,
                format!(
                    "[rust] exitIp: {} → {ip}",
                    prev.as_deref().unwrap_or("null")
                ),
            );
            *prev = Some(ip.clone());
        }
    }

    Ok(info)
}

/// Quick health check for the active proxy via Clash API delay endpoint.
/// Returns true if the "proxy" outbound/group responds to a delay probe.
#[tauri::command]
async fn check_proxy_health() -> Result<bool, String> {
    let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        return Ok(false);
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(7))
        .build()
        .map_err(|e| e.to_string())?;

    let delay_url = format!(
        "http://127.0.0.1:{stats_port}/proxies/proxy/delay?url=http://cp.cloudflare.com/generate_204&timeout=5000"
    );

    match client.get(&delay_url).send().await {
        Ok(resp) if resp.status().is_success() => Ok(true),
        _ => Ok(false),
    }
}

/// Switch the Selector proxy to the given outbound tag.
/// Called by health check when the current server is dead.
#[tauri::command(rename_all = "camelCase")]
async fn switch_proxy(proxy_tag: String) -> Result<(), String> {
    let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        return Err("Proxy not connected".into());
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;

    let url = format!("http://127.0.0.1:{stats_port}/proxies/proxy");
    let body = serde_json::json!({ "name": proxy_tag });

    match client.put(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(format!("Clash API returned HTTP {}", resp.status().as_u16())),
        Err(e) => Err(e.to_string()),
    }
}

/// Speed test: test latency of all servers via Clash API delay endpoint
#[tauri::command]
async fn speed_test() -> Result<Vec<serde_json::Value>, String> {
    let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        return Err("Proxy not connected".into());
    }

    let client = reqwest::Client::builder()
        .no_proxy()
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
            if ptype == "Direct"
                || ptype == "Reject"
                || ptype == "Selector"
                || ptype == "URLTest"
                || ptype == "Fallback"
            {
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
    let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        return Err("Proxy not connected".into());
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get(format!("http://127.0.0.1:{stats_port}/connections"))
        .send()
        .await
        .map_err(|e| format!("Clash API error: {e}"))?;

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("Parse error: {e}"))?;

    let connections = data["connections"].as_array().cloned().unwrap_or_default();

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

    let servers = crate::profile::process_servers(&app, servers, "").await;

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
    let servers = crate::profile::process_servers(&app, servers, "").await;
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
    state: tauri::State<'_, ProxyState>,
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
    let _ = manager
        .ensure_installed(&app)
        .await
        .map_err(|e| e.to_string())?;
    drop(manager);

    // 2. Download GeoIP database
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let geo_dir = data_dir.join("geoip");
    std::fs::create_dir_all(&geo_dir).map_err(|e| e.to_string())?;
    let ipinfo_token = app
        .try_state::<Mutex<crate::settings::AppSettings>>()
        .map(|s| s.try_lock().ok().map(|g| g.ipinfo_token.clone()))
        .flatten()
        .unwrap_or_default();
    crate::proxy::geoip::ensure_downloaded(&geo_dir, &ipinfo_token).await?;

    // 3. Enrich loaded profiles with GeoIP data (fix corrupted names)
    if let Some(profile_state) = app.try_state::<ProfileState>() {
        let mut pm = profile_state.manager.lock().await;
        pm.enrich_loaded().await;

        // 3a. Prune cache: remove entries not seen in any profile for >30 days.
        //     This prevents the cache from accumulating orphaned entries from
        //     deleted or long-removed subscriptions.
        if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
            let active_ids: std::collections::HashSet<String> = pm
                .list()
                .iter()
                .flat_map(|p| p.servers.iter().map(|s| s.id.clone()))
                .collect();
            const THIRTY_DAYS_SECS: u64 = 30 * 24 * 3600;
            cache.prune_old(&active_ids, std::time::Duration::from_secs(THIRTY_DAYS_SECS)).await;
            cache.save_to_disk().await;
        }
    }

    // 4. Detect and log real IP (no proxy, direct connection)
    {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .ok();
        if let Some(c) = client {
            for url in IP_URLS {
                if let Ok(resp) = c.get(*url).send().await {
                    if resp.status().is_success() {
                        if let Ok(text) = resp.text().await {
                            let ip = text.trim().to_string();
                            if !ip.is_empty() && ip.chars().all(|c| c.is_ascii_digit() || c == '.')
                            {
                                log(
                                    &app,
                                    &logger,
                                    LogLevel::Info,
                                    LogSource::System,
                                    format!("Real IP: {ip}"),
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    log(
        &app,
        &logger,
        LogLevel::Success,
        LogSource::System,
        "App initialized".into(),
    );
    Ok(())
}

/// Download the ipinfo.io Lite database on demand.
///
/// Called from the frontend after the user saves an IPinfo API token in
/// General Settings. Respects the 4-hour rate-limit (max 6/day), shows
/// result via the returned status string.
///
/// Returns:
///   - `"downloaded"` — fresh download completed
///   - `"skipped"`    — within rate-limit window, using cached copy
///   - `Err`          — token missing or download failed
#[tauri::command]
async fn download_ipinfo_db(app: tauri::AppHandle) -> Result<String, String> {
    let logger = app.state::<StdMutex<Logger>>();

    let ipinfo_token = app
        .try_state::<Mutex<crate::settings::AppSettings>>()
        .map(|s| s.try_lock().ok().map(|g| g.ipinfo_token.clone()))
        .flatten()
        .unwrap_or_default();

    if ipinfo_token.is_empty() {
        log(
            &app,
            &logger,
            LogLevel::Warning,
            LogSource::System,
            "IPinfo download skipped — token not configured".into(),
        );
        return Err("IPinfo API token not set".into());
    }

    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::System,
        "Downloading IPinfo GeoIP database...".into(),
    );

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {e}"))?;
    let geo_dir = data_dir.join("geoip");

    match crate::proxy::geoip::download_ipinfo_db(&geo_dir, &ipinfo_token).await {
        Ok(true) => {
            log(
                &app,
                &logger,
                LogLevel::Success,
                LogSource::System,
                "IPinfo GeoIP database downloaded".into(),
            );
            Ok("downloaded".into())
        }
        Ok(false) => {
            log(
                &app,
                &logger,
                LogLevel::Info,
                LogSource::System,
                "IPinfo database is up-to-date — using cached copy".into(),
            );
            Ok("skipped".into())
        }
        Err(e) => {
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::System,
                format!("IPinfo database download failed: {e}"),
            );
            Err(e)
        }
    }
}

/// Return download metadata for all GeoIP databases.
///
/// Used by the frontend to display last-updated timestamps.
#[tauri::command]
async fn get_geoip_status(app: tauri::AppHandle) -> Result<crate::proxy::geoip::GeoIpMeta, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {e}"))?;
    let geo_dir = data_dir.join("geoip");
    Ok(crate::proxy::geoip::get_geoip_meta(&geo_dir))
}

/// Check if a process is alive by PID.
///
/// Windows: uses Win32 `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION`.
/// This is an order of magnitude faster and lighter than spawning `tasklist`
/// every 2 seconds in the watchdog loop — no process creation, pipe I/O, or
/// CSV parsing.
///
/// Unix: sends `kill -0` signal to check existence.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        // Win32: OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid)
        // Returns a non-null handle if the process exists and is accessible.
        // PROCESS_QUERY_LIMITED_INFORMATION (0x1000) requires Vista+ and is
        // available without extra privileges on Win 8+.
        use std::ffi::c_void;

        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

        extern "system" {
            fn OpenProcess(
                dwDesiredAccess: u32,
                bInheritHandle: i32,
                dwProcessId: u32,
            ) -> *mut c_void;
            fn CloseHandle(hObject: *mut c_void) -> i32;
        }

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        // SAFETY: handle is valid (non-null) at this point.
        unsafe {
            CloseHandle(handle);
        }
        true
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

/// Wait until sing-box is actually ready to accept traffic, not just spawned.
///
/// Replaces the previous `wait_for_alive(pid)` call in `start_proxy`. The old
/// version returned as soon as `tasklist` showed the PID existed — which
/// happens ~50–100 ms after `CreateProcess`, long before sing-box has bound
/// the proxy inbound. This caused the dashboard to show "Connected" while
/// sing-box was still initializing, leading to dropped first-packet errors
/// and the integration test in `tests/selector_test.rs` failing with
/// "Clash API not reachable" after only a 1-second wait.
///
/// Actively TCP-connects to `127.0.0.1:{proxy_port}` until the connect
/// succeeds or the deadline expires. If the PID dies at any point, return
/// early with an error so the caller can read sing-box's stderr log.
async fn wait_for_proxy_ready(
    pid: u32,
    proxy_port: u16,
) -> Result<u32, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(PROXY_TIMEOUT_SECS);
    let mut attempts = 0u32;

    // Poll TCP connect to the mixed inbound port.
    loop {
        attempts += 1;

        if !is_process_alive(pid) {
            return Err(format!(
                "sing-box (PID {pid}) exited before the proxy port was ready (after {attempts} attempt(s))"
            ));
        }

        // Try to TCP-connect to the proxy inbound. A successful connect
        // means sing-box has bound the port and is accepting connections.
        match tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}")),
        )
        .await
        {
            Ok(Ok(_stream)) => {
                // Port is accepting connections. sing-box is ready.
                return Ok(attempts);
            }
            _ => {
                // Not ready yet (connection refused / timeout). Keep polling
                // until the deadline.
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out after {attempts} attempt(s) waiting for proxy port {proxy_port} (PID {pid})"
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

    /// `wait_for_proxy_ready` in proxy modes returns Ok once the inbound
    /// port is accepting TCP connections. We bind a TcpListener on a free
    /// port ourselves and call the function — it should succeed quickly.
    #[tokio::test]
    async fn test_wait_for_proxy_ready_succeeds_when_port_open() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let pid = std::process::id();

        // Spawn an accept loop so the listener stays alive during the poll.
        let accept_handle = tokio::spawn(async move {
            // Accept and immediately drop — we just need the port open.
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });

        let result = wait_for_proxy_ready(pid, port).await;
        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        assert!(result.unwrap() > 0);
        accept_handle.abort();
    }

    /// `wait_for_proxy_ready` returns Err when the target port never opens.
    /// We use the current process's PID (which is alive) and a port that's
    /// extremely unlikely to be open — the function should time out.
    ///
    /// To keep the test fast, we temporarily lower the deadline by
    /// hijacking PROXY_TIMEOUT_SECS via a short-circuit on a known-closed
    /// port. Since we can't change the const, we just accept that this
    /// test will run for up to PROXY_TIMEOUT_SECS (15 s). To avoid slowing
    /// down `cargo test`, this test is marked `#[ignore]` — run it with
    /// `cargo test test_wait_for_proxy_ready_times_out -- --ignored`.
    #[tokio::test]
    #[ignore = "slow: waits up to 15 s for the timeout; run with --ignored"]
    async fn test_wait_for_proxy_ready_times_out_when_port_closed() {
        // Bind + immediately drop to get a port that was free a moment ago
        // — very unlikely to be reused by another process during the test.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let pid = std::process::id();

        let result = wait_for_proxy_ready(pid, port).await;
        assert!(result.is_err(), "expected Err, got Ok");
        let err = result.unwrap_err();
        assert!(
            err.contains("timed out"),
            "expected error to mention 'timed out', got: {err}"
        );
    }

    /// `wait_for_proxy_ready` returns Err when the PID is dead.
    /// Should fail fast (within one poll cycle) instead of waiting for
    /// the full timeout.
    #[tokio::test]
    async fn test_wait_for_proxy_ready_dead_pid() {
        let start = tokio::time::Instant::now();
        let result = wait_for_proxy_ready(999_999_999, 65535).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected Err for dead PID, got Ok");
        let err = result.unwrap_err();
        assert!(
            err.contains("exited"),
            "expected error to mention 'exited', got: {err}"
        );
        // Should fail fast — under 2 seconds — not wait for the full 15 s.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "expected fast failure, took {elapsed:?}"
        );
    }
}
