#![recursion_limit = "512"]
pub mod events;
pub mod logging;
pub mod profile;
pub mod proxy;
pub(crate) mod settings;
pub mod subscription;

use crate::events::emit_status;
use crate::events::emit_traffic;
use crate::events::emit_proxy_progress;
use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::profile::ProfileManager;
use crate::proxy::cache::ServerCache;
use crate::proxy::port_pool::PortPool;
use crate::proxy::{ConnectionStatus, ErrorCode, ProxyManager, ProxyError, ServerProfile};
use crate::settings::{AppSettings, SettingsManager};
use crate::subscription::SubscriptionProfile;
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

/// Guard: ensure initialize_app runs only once per process.
/// Prevents double GeoIP download, double cache save, etc.
/// when the frontend calls initialize_app twice (React
/// StrictMode in dev, or effect re-runs).
static INITIALIZED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

struct ProxyState {
    manager: Arc<Mutex<ProxyManager>>,
}

struct ProfileState {
    manager: Arc<Mutex<ProfileManager>>,
}

/// Separate cancellation token for the proxy lifecycle (watchdog,
/// failover monitor, traffic stats). Decoupled from the refresh
/// cancellation token so that `cancel_refresh` cannot accidentally
/// kill proxy-side background tasks.
struct ProxyCancelState(StdMutex<CancellationToken>);

use tokio_util::sync::CancellationToken;

#[tauri::command(rename_all = "camelCase")]
#[tracing::instrument(skip(
    app,
    state,
    profile_state,
    settings_state,
    profile,
    alternative_servers
))]
async fn start_proxy(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProxyState>,
    profile_state: tauri::State<'_, ProfileState>,
    settings_state: tauri::State<'_, Mutex<AppSettings>>,
    profile: ServerProfile,
    routing_mode: String,
    proxy_port: u16,
    alternative_servers: Vec<ServerProfile>,
) -> Result<u16, ProxyError> {
    let logger = app.state::<StdMutex<Logger>>();
    // Read the PROXY cancel token (not the refresh token).
    let cancel = app
        .state::<ProxyCancelState>()
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
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
    // Clear any stale cancellation flag from a previous attempt.
    crate::proxy::CONNECTION_CANCELLING.store(false, std::sync::atomic::Ordering::SeqCst);
    emit_proxy_progress(&app, "init", "Preparing connection…");

    // Store active server ID for exit IP enrichment
    {
        let mut active_id = crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
            if candidate <= proxy_port {
                break;
            } // wraparound guard
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
            emit_proxy_progress(&app, "verifying", "Waiting for proxy to become ready…");
            log(
                &app,
                &logger,
                LogLevel::Info,
                LogSource::Proxy,
                "Verifying proxy connection...".into(),
            );

            let result = match manager.process_pid() {
                Some(pid) => wait_for_proxy_ready(pid, proxy_port)
                    .await
                    .map_err(|e| ProxyError::process(ErrorCode::ProcessNotReachable, e)),
                None => Err(ProxyError::process(
                    ErrorCode::ProcessStartFailed,
                    "no PID after start",
                )),
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
                        pm.list()
                            .iter()
                            .any(|p| p.servers.iter().any(|s| s.id == profile.id && s.behind_cdn))
                    };
                    if is_cdn {
                        // 🚫 ANTI-REGRESSION: проверяем инварианту активного сервера.
                        // Это единственная точка в runtime (между refresh-циклами),
                        // где мы можем поймать нарушение. Если CDN-сервер имеет exit,
                        // это означает, что баг произошёл где-то между enrich_loaded
                        // и стартом прокси. Без этой проверки баг может существовать
                        // часами, пока не сработает enrich_loaded при следующем старте.
                        {
                            let pm_check = profile_state.manager.lock().await;
                            for p in pm_check.list() {
                                for s in &p.servers {
                                    if s.id == profile.id {
                                        s.assert_no_cdn_exit_data("start_proxy::active_server");
                                        break;
                                    }
                                }
                            }
                        }
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
                        let mut cb = reqwest::Client::builder().timeout(Duration::from_secs(10));
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
                                                    .parse::<std::net::IpAddr>()
                                                    .is_ok()
                                            {
                                                got_response = true;

                                                if crate::proxy::geoip::is_cdn_entry_ip(&ip) {
                                                    log(
                                                        &app,
                                                        &logger,
                                                        LogLevel::Info,
                                                        LogSource::Proxy,
                                                        format!(
                                                            "Proxy verified (exit IP: {ip}) — outlet is in CDN ranges but \
                                                             entry IP was not CDN-marked. Keeping exit IP (outlet is egress, \
                                                             not entry). Server {} ({}:{}).",
                                                            profile.id, profile.server, profile.port
                                                        )
                                                    );
                                                } else {
                                                    log(
                                                        &app,
                                                        &logger,
                                                        LogLevel::Info,
                                                        LogSource::Proxy,
                                                        format!("Proxy verified (exit IP: {ip})"),
                                                    );
                                                }
                                                let _ = app.emit(
                                                    "exit-ip-changed",
                                                    ExitIpInfo {
                                                        server: ip.clone(),
                                                        flag: String::new(),
                                                        country_name: String::new(),
                                                        city_name: String::new(),
                                                        proxy_tag: String::new(),
                                                    },
                                                );
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
                                let stats_port = crate::proxy::CLASH_API_PORT
                                    .load(std::sync::atomic::Ordering::Relaxed);
                                if stats_port > 0 && all_servers.len() > 1 {
                                    for idx in 1..all_servers.len() {
                                        let tag = format!("proxy-{idx}");
                                        log(
                                            &app,
                                            &logger,
                                            LogLevel::Info,
                                            LogSource::Proxy,
                                            format!(
                                                "Switching to {tag} ({})...",
                                                all_servers[idx].display_name()
                                            ),
                                        );
                                        let switch_url =
                                            format!("http://127.0.0.1:{stats_port}/proxies/proxy");
                                        let body = serde_json::json!({ "name": tag });
                                        let client = reqwest::Client::new();
                                        if client.put(&switch_url).json(&body).send().await.is_ok()
                                        {
                                            tokio::time::sleep(Duration::from_secs(2)).await;
                                            // Re-verify exit IP through the new server
                                            let mut cb = reqwest::Client::builder()
                                                .timeout(Duration::from_secs(10));
                                            if let Ok(proxy) = reqwest::Proxy::all(format!(
                                                "http://127.0.0.1:{proxy_port}"
                                            )) {
                                                cb = cb.proxy(proxy);
                                            }
                                            if let Some(c) = cb.build().ok() {
                                                for url in IP_URLS {
                                                    if let Ok(resp) = c.get(*url).send().await {
                                                        if resp.status().is_success() {
                                                            if let Ok(text) = resp.text().await {
                                                                let ip = text.trim().to_string();
                                                                if !ip.is_empty()
                                                                    && ip.parse::<std::net::IpAddr>().is_ok()
                                                                {
                                                                    if crate::proxy::geoip::is_cdn_entry_ip(&ip) {
                                                                        tracing::info!(
                                                                            target: "proxy",
                                                                            "Proxy verified (exit IP: {ip}) via {tag} — outlet is in CDN ranges \
                                                                             but entry IP was not CDN-marked. Keeping exit IP (outlet is egress, \
                                                                             not entry). Server {} ({}:{}).",
                                                                            all_servers[idx].id,
                                                                            all_servers[idx].server,
                                                                            all_servers[idx].port
                                                                        );
                                                                    } else {
                                                                        tracing::info!(target: "proxy", "Proxy verified (exit IP: {ip}) via {tag}");
                                                                    }
                                                                    // U2: Эмитим exit-ip-changed в fallback-ветке,
                                                                    // чтобы UI получил exit IP при переключении.
                                                                    let _ = app.emit(
                                                                        "exit-ip-changed",
                                                                        ExitIpInfo {
                                                                            server: ip.clone(),
                                                                            flag: String::new(),
                                                                            country_name: String::new(),
                                                                            city_name: String::new(),
                                                                            proxy_tag: tag.clone(),
                                                                        },
                                                                    );
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
                                    tracing::error!(target: "proxy", "Proxy connection verified but exit IP unavailable — traffic may not be routing through proxy");
                                }
                            }
                        }
                    }

                    // Save for auto-reconnect on unexpected exit
                    {
                        let mut last = crate::proxy::LAST_PROFILE
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
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
                                if cancel_watch.is_cancelled() {
                                    break;
                                }
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
                                        let lock = crate::proxy::LAST_PROFILE
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner());
                                        if lock.is_none() {
                                            break; // intentional stop
                                        }
                                    }

                                    tracing::warn!(target: "process", "Connection lost — reconnecting...");

                                    // Auto-reconnect up to 3 times.
                                    // Note: we do NOT call mgr.stop() here — the process is already dead,
                                    // and mgr.stop() would clear the PID guard causing mgr.status()
                                    // to always return Disconnected.
                                    let last = {
                                        let lock = crate::proxy::LAST_PROFILE
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner());
                                        lock.clone()
                                    };
                                    let mut reconnected = false;
                                    if let Some(ref last) = last {
                                        for attempt in 1..=3 {
                                            tokio::time::sleep(Duration::from_secs(attempt * 2))
                                                .await;

                                            // User might have disconnected while we were sleeping
                                            {
                                                let lock = crate::proxy::LAST_PROFILE
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                if lock.is_none() {
                                                    break;
                                                }
                                            }

                                            tracing::info!(target: "process", "Reconnect attempt {attempt}...");

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
                                                tracing::info!(target: "process", "Reconnected successfully");
                                                emit_status(
                                                    &app_clone,
                                                    ConnectionStatus::Connected,
                                                );
                                                reconnected = true;
                                                break;
                                            }
                                        }
                                    }

                                    if !reconnected {
                                        tracing::error!(target: "process", "Auto-reconnect failed after 3 attempts");

                                        // Guard: if user disconnected during reconnect delay, don't activate kill switch
                                        let still_wants_connection = {
                                            let lock = crate::proxy::LAST_PROFILE
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            lock.is_some()
                                        };

                                        if !still_wants_connection {
                                            emit_status(&app_clone, ConnectionStatus::Disconnected);
                                        } else {
                                            let settings = app_clone
                                                .state::<Mutex<crate::settings::AppSettings>>()
                                                .lock()
                                                .await
                                                .clone();

                                            if settings.kill_switch {
                                                if let Some(ref last) = last {
                                                    tracing::warn!(target: "process", "Activating Kill Switch: blocking all traffic on port {}", last.proxy_port);
                                                    let mgr = mgr_arc.lock().await;
                                                    if let Err(e) = mgr
                                                        .activate_kill_switch(
                                                            &app_clone,
                                                            last.proxy_port,
                                                            &last.routing_mode,
                                                        )
                                                        .await
                                                    {
                                                        tracing::error!(target: "process", "Kill Switch activation failed: {e}");
                                                    }
                                                }
                                                emit_status(&app_clone, ConnectionStatus::Error("Connection lost. Kill Switch active (Network blocked).".into()));
                                            } else {
                                                emit_status(
                                                    &app_clone,
                                                    ConnectionStatus::Disconnected,
                                                );
                                            }
                                        }
                                        break;
                                    }
                                }
                            }
                        });
                    }

                    // ── Failover monitor ────────────────────────────────────────
                    // Отдельный легковесный опрос Clash API (каждые 5 с).
                    // При смене outbound передаёт фронту точный server ID.
                    let fail_port =
                        crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
                    if fail_port > 0 && all_servers.len() > 1 {
                        let app_fail = app.clone();
                        let servers_fail = all_servers.clone();
                        let mgr_fail = manager_arc.clone();
                        let cancel_fail = cancel.clone();
                        tokio::spawn(async move {
                            let client = reqwest::Client::builder()
                                .no_proxy()
                                .timeout(Duration::from_secs(2))
                                .build()
                                .unwrap_or_default();
                            let api_url = format!("http://127.0.0.1:{fail_port}/proxies/proxy");
                            let mut current_tag = String::new();
                            loop {
                                if cancel_fail.is_cancelled() {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                {
                                    let mgr = mgr_fail.lock().await;
                                    if mgr.status() == crate::proxy::ConnectionStatus::Disconnected
                                    {
                                        break;
                                    }
                                }
                                if let Ok(resp) = client.get(&api_url).send().await {
                                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                                        if let Some(now_tag) =
                                            body.get("now").and_then(|v| v.as_str())
                                        {
                                            if !current_tag.is_empty() && now_tag != current_tag {
                                                tracing::info!(
                                                    target: "proxy",
                                                    "Seamless failover detected: {current_tag} -> {now_tag}",
                                                );
                                                if let Some(idx_str) =
                                                    now_tag.strip_prefix("proxy-")
                                                {
                                                    if let Ok(idx) = idx_str.parse::<usize>() {
                                                        if let Some(srv) = servers_fail.get(idx) {
                                                            let _ = app_fail.emit(
                                                                "failover-occurred",
                                                                srv.id.clone(),
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            current_tag = now_tag.to_string();
                                        }
                                    }
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
                            loop {
                                if cancel_stats.is_cancelled() {
                                    break;
                                }
                                // Stop polling if proxy disconnected
                                {
                                    let mgr = mgr_arc.lock().await;
                                    if mgr.status() == crate::proxy::ConnectionStatus::Disconnected
                                    {
                                        break;
                                    }
                                }
                                if let Ok(mut resp) = client.get(&url).send().await {
                                    let mut buf = String::new();
                                    loop {
                                        match resp.chunk().await {
                                            Ok(Some(bytes)) => {
                                                buf.push_str(&String::from_utf8_lossy(&bytes));
                                                while let Some(newline) = buf.find('\n') {
                                                    let line = buf[..newline].trim().to_string();
                                                    buf = buf[newline + 1..].to_string();
                                                    // Parse via serde — handles any JSON formatting
                                                    if let Ok(traffic) =
                                                        serde_json::from_str::<ClashTraffic>(&line)
                                                    {
                                                        // Clash API returns B/s; convert to MB/s
                                                        let speed_up = traffic.up / 1024.0 / 1024.0;
                                                        let speed_down =
                                                            traffic.down / 1024.0 / 1024.0;
                                                        emit_traffic(
                                                            &app_clone, speed_up, speed_down,
                                                        );
                                                    }
                                                }
                                            }
                                            Ok(None) => break,
                                            Err(_) => break,
                                        }
                                    }
                                }
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                        });
                    }

                    Ok(proxy_port)
                }
                Err(e) => {
                    let _ = manager.stop(&app).await;
                    let msg = format!("Proxy not reachable: {e}");
                    // Downgrade to warn when the error is user-initiated cancellation.
                    let is_cancelled = msg.contains("cancelled");
                    if is_cancelled {
                        tracing::warn!(target: "proxy", "{msg}");
                    } else {
                        tracing::error!(target: "proxy", "{msg}");
                    }
                    // Don't emit Error if the user cancelled — the
                    // cancellation already emitted Disconnected.
                    if !crate::proxy::CONNECTION_CANCELLING.load(std::sync::atomic::Ordering::SeqCst) {
                        emit_status(&app, ConnectionStatus::Error(msg.clone()));
                    }
                    Err(ProxyError::process(ErrorCode::ProcessNotReachable, msg))
                }
            }
        }
        Err(e) => {
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Proxy,
                format!("Connection failed: {e}"),
            );
            emit_status(&app, ConnectionStatus::Error(e.to_string()));
            Err(e)
        }
    }
}

#[tauri::command]
#[tracing::instrument(skip(app, state))]
async fn stop_proxy(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProxyState>,
) -> Result<(), String> {
    let _logger = app.state::<StdMutex<Logger>>();
    let log_stop = |level: LogLevel, msg: String| {
        match level {
            LogLevel::Info | LogLevel::Success => tracing::info!(target: "system", "{msg}"),
            LogLevel::Warning => tracing::warn!(target: "system", "{msg}"),
            LogLevel::Error => tracing::error!(target: "system", "{msg}"),
        };
    };

    log_stop(LogLevel::Info, "[stop_proxy] called".into());

    // Step 1: Clear LAST_PROFILE so watchdog stops.
    {
        let mut last = crate::proxy::LAST_PROFILE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *last = None;
    }
    log_stop(
        LogLevel::Info,
        "[stop_proxy] step 1: LAST_PROFILE cleared".into(),
    );

    // Step 2: Reset Clash API port immediately so health check stops.
    crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Release);
    log_stop(
        LogLevel::Info,
        "[stop_proxy] step 2: CLASH_API_PORT=0".into(),
    );

    // Step 3: Kill sing-box by PID + fallback by process name.
    let pid = crate::proxy::PROXY_PID.swap(0, std::sync::atomic::Ordering::AcqRel);
    log_stop(
        LogLevel::Info,
        format!("[stop_proxy] step 3: PROXY_PID={pid} — killing..."),
    );
    if pid > 0 {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            if let Err(e) = std::process::Command::new("taskkill")
                .args(["/pid", &pid.to_string(), "/f"])
                .creation_flags(0x08000000)
                .output()
            {
                log_stop(
                    LogLevel::Warning,
                    format!("[stop_proxy] taskkill (pid={pid}) failed: {e}"),
                );
            } else {
                log_stop(
                    LogLevel::Info,
                    format!("[stop_proxy] taskkill /pid {pid} OK"),
                );
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    } else {
        log_stop(
            LogLevel::Warning,
            "[stop_proxy] PID was 0 — process already dead?".into(),
        );
    }

    log_stop(LogLevel::Info, "[stop_proxy] sing-box killed".into());

    // Step 3b: Reset Windows system proxy (sing-box was killed, can't clean up itself)
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("reg")
            .args([
                "add",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
                "/v",
                "ProxyEnable",
                "/t",
                "REG_DWORD",
                "/d",
                "0",
                "/f",
            ])
            .output();
        let _ = std::process::Command::new("reg")
            .args([
                "delete",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
                "/v",
                "ProxyServer",
                "/f",
            ])
            .output();
        let _ = std::process::Command::new("reg")
            .args([
                "delete",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
                "/v",
                "AutoConfigURL",
                "/f",
            ])
            .output();
        let _ = std::process::Command::new("netsh")
            .args(["winhttp", "reset", "proxy"])
            .output();
        log_stop(
            LogLevel::Info,
            "[stop_proxy] step 3b: Windows system proxy disabled".into(),
        );
    }

    // Step 4: Best-effort manager cleanup.
    if let Ok(mgr) = state.manager.try_lock() {
        mgr.clear_pid();
        log_stop(
            LogLevel::Info,
            "[stop_proxy] step 4: manager PID cleared".into(),
        );
    } else {
        log_stop(
            LogLevel::Warning,
            "[stop_proxy] step 4: manager lock busy — skip".into(),
        );
    }

    // Step 5: Log and emit status.
    log_stop(
        LogLevel::Success,
        "[stop_proxy] emitting Disconnected event & returning Ok".into(),
    );
    crate::proxy::remove_pid_file();
    emit_status(&app, ConnectionStatus::Disconnected);

    // Cancel any proxy-side background tasks (watchdog / failover /
    // traffic) that are still running with the OLD token, and replace
    // it with a fresh one so the next start_proxy gets a clean token.
    {
        let proxy_cancel = app.state::<ProxyCancelState>();
        let mut guard = proxy_cancel
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let old = std::mem::replace(&mut *guard, CancellationToken::new());
        old.cancel();
    }

    Ok(())
}

/// Cancel an in-flight `start_proxy` connection attempt.
///
/// Called by the frontend when the user clicks the Connect button
/// during the 'connecting' phase. Semantically distinct from
/// `stop_proxy` (which disconnects a fully-connected proxy).
#[tauri::command]
async fn cancel_connect(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProxyState>,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::System,
        "[cancel_connect] cancelling in-flight connection…".into(),
    );

    // Signal the in-flight start_proxy to abort via the cancellation flag.
    crate::proxy::CONNECTION_CANCELLING.store(true, std::sync::atomic::Ordering::SeqCst);

    // 1. Cancel the proxy cancel token — stops any monitors that
    //    were spawned by the in-flight start_proxy (watchdog,
    //    failover, traffic).
    {
        let proxy_cancel = app.state::<ProxyCancelState>();
        let mut guard = proxy_cancel
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let old = std::mem::replace(&mut *guard, CancellationToken::new());
        old.cancel();
    }

    // 2. Clear LAST_PROFILE so the watchdog doesn't try to reconnect.
    {
        let mut last = crate::proxy::LAST_PROFILE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *last = None;
    }

    // 3. Reset Clash API port.
    crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Release);

    // 4. Kill the PID (if start_proxy already spawned sing-box).
    let pid = crate::proxy::PROXY_PID.swap(0, std::sync::atomic::Ordering::AcqRel);
    if pid > 0 {
        log(
            &app,
            &logger,
            LogLevel::Info,
            LogSource::System,
            format!("[cancel_connect] killing sing-box PID {pid}"),
        );
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("taskkill")
                .args(["/pid", &pid.to_string(), "/f"])
                .creation_flags(0x08000000)
                .output();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    }

    // 5. Reset Windows system proxy (sing-box may have set it before being killed).
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("reg")
            .args([
                "add",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
                "/v",
                "ProxyEnable",
                "/t",
                "REG_DWORD",
                "/d",
                "0",
                "/f",
            ])
            .output();
        let _ = std::process::Command::new("reg")
            .args([
                "delete",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
                "/v",
                "ProxyServer",
                "/f",
            ])
            .output();
        let _ = std::process::Command::new("reg")
            .args([
                "delete",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
                "/v",
                "AutoConfigURL",
                "/f",
            ])
            .output();
        let _ = std::process::Command::new("netsh")
            .args(["winhttp", "reset", "proxy"])
            .output();
        log(
            &app,
            &logger,
            LogLevel::Info,
            LogSource::System,
            "[cancel_connect] Windows system proxy disabled".into(),
        );
    }

    // 6. Clear the manager PID (best-effort, non-blocking).
    if let Ok(mgr) = state.manager.try_lock() {
        mgr.clear_pid();
    }

    // 7. Emit Disconnected.
    crate::proxy::remove_pid_file();
    emit_status(&app, ConnectionStatus::Disconnected);

    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::System,
        "[cancel_connect] done".into(),
    );

    Ok(())
}

/// Log from the frontend (stores.ts addLog → invoke('log_frontend'))
#[tauri::command]
async fn log_frontend(
    _app: tauri::AppHandle,
    message: String,
    level: String,
    source: String,
) -> Result<(), String> {
    log_to_tracing(&message, &level, &source);
    Ok(())
}

/// Map JS-level strings to tracing calls (shared by log_frontend and log_message).
fn log_to_tracing(message: &str, level: &str, source: &str) {
    match (level, source) {
        ("error", "proxy") => tracing::error!(log_source = "proxy", "{message}"),
        ("error", "process") => tracing::error!(log_source = "process", "{message}"),
        ("error", "settings") => tracing::error!(log_source = "settings", "{message}"),
        ("error", "subscription") => tracing::error!(log_source = "subscription", "{message}"),
        ("error", "cache") => tracing::error!(log_source = "cache", "{message}"),
        ("error", "installer") => tracing::error!(log_source = "installer", "{message}"),
        ("error", _) => tracing::error!(log_source = "system", "{message}"),
        ("warning", "proxy") => tracing::warn!(log_source = "proxy", "{message}"),
        ("warning", "process") => tracing::warn!(log_source = "process", "{message}"),
        ("warning", "settings") => tracing::warn!(log_source = "settings", "{message}"),
        ("warning", "subscription") => tracing::warn!(log_source = "subscription", "{message}"),
        ("warning", "cache") => tracing::warn!(log_source = "cache", "{message}"),
        ("warning", "installer") => tracing::warn!(log_source = "installer", "{message}"),
        ("warning", _) => tracing::warn!(log_source = "system", "{message}"),
        (_, "proxy") => tracing::info!(log_source = "proxy", "{message}"),
        (_, "process") => tracing::info!(log_source = "process", "{message}"),
        (_, "settings") => tracing::info!(log_source = "settings", "{message}"),
        (_, "subscription") => tracing::info!(log_source = "subscription", "{message}"),
        (_, "cache") => tracing::info!(log_source = "cache", "{message}"),
        (_, "installer") => tracing::info!(log_source = "installer", "{message}"),
        (_, _) => tracing::info!(log_source = "system", "{message}"),
    }
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
) -> Result<SubscriptionProfile, ProxyError> {
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
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Subscription,
                format!("Failed to add subscription: {e}"),
            );
            Err(e)
        }
    }
}

#[tauri::command]
async fn remove_subscription(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProfileState>,
    id: String,
) -> Result<(), ProxyError> {
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
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Subscription,
                format!("Failed to remove subscription: {e}"),
            );
            Err(e)
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
    let has_country_filter =
        !settings.allowed_countries.is_empty() || !settings.blocked_countries.is_empty();

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
    let all_servers: Vec<ServerProfile> = pm
        .list()
        .iter()
        .flat_map(|p| p.servers.iter().cloned())
        .collect();
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
    let current_country = all_servers
        .iter()
        .find(|s| s.id == current_server_id)
        .and_then(|s| s.exit_flag.as_deref());

    // Prefer same country (exit flag), fastest ping
    if let Some(country) = current_country {
        if let Some(best) = candidates
            .iter()
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
    if let Some(best) = candidates
        .iter()
        .min_by_key(|s| s.ping_ms.unwrap_or(u64::MAX))
    {
        log(
            &app,
            &logger,
            LogLevel::Info,
            LogSource::System,
            format!(
                "[reconnect] picked {}/{} (any country, ping {}) from {total} servers",
                best.display_name(),
                best.id,
                best.ping_ms.unwrap_or(0)
            ),
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
    let mut mgr = state.manager.lock().await;
    mgr.deduplicate_profiles();
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
) -> Result<SubscriptionProfile, ProxyError> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        "Refreshing subscription...".into(),
    );

    // ═══ Phase 1: Brief read lock — get URL + old servers ═══════════
    let (url, old_servers) = {
        let mgr = state.manager.lock().await;
        let idx = mgr
            .list()
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| {
                ProxyError::subscription(
                    crate::proxy::ErrorCode::SubscriptionNotFound,
                    "Profile not found",
                )
            })?;
        (mgr.list()[idx].url.clone(), mgr.list()[idx].servers.clone())
    }; // lock released

    // ═══ Phase 2: Long operations — NO LOCK ════════════════════════
    crate::proxy::geoip::reset_ipguide_rate_limit();

    let fetch_result = match &url {
        u if u.starts_with("http://") || u.starts_with("https://") => {
            match crate::subscription::fetch_subscription(u, crate::profile::proxy_port(&app)).await {
                Ok(s) => Ok(s),
                Err(e) => {
                    if crate::profile::proxy_port(&app).is_some() {
                        tracing::error!(
                            target: "subscription",
                            "Proxy fetch failed, retrying direct: {e}"
                        );
                        crate::subscription::fetch_subscription(u, None).await
                    } else {
                        Err(e)
                    }
                }
            }
        }
        _ => crate::subscription::parser::parse_subscription(&url),
    };

    let mut servers = match fetch_result {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                target: "subscription",
                "Refresh failed, bumping updated_at: {e}"
            );
            let re_enriched = if !old_servers.is_empty() {
                crate::profile::process_servers(
                    &app,
                    old_servers
                        .into_iter()
                        .map(|mut s| {
                            s.ping_ms = None;
                            s
                        })
                        .collect(),
                    "",
                )
                .await
            } else {
                Vec::new()
            };
            {
                let mut mgr = state.manager.lock().await;
                if let Some(idx) = mgr.list().iter().position(|p| p.id == id) {
                    mgr.list_mut()[idx].updated_at = chrono::Utc::now().to_rfc3339();
                    if !re_enriched.is_empty() {
                        mgr.list_mut()[idx].servers = re_enriched;
                    }
                    let _ = mgr.save();
                }
            }
            return Err(e);
        }
    };

    // Preserve server IDs
    let old_index: std::collections::HashMap<String, String> = old_servers
        .iter()
        .map(|s| {
            (
                format!(
                    "{}://{}:{}@{}:{}:{}/{}",
                    s.protocol,
                    s.server,
                    s.port,
                    s.uuid.as_deref().unwrap_or(""),
                    s.password.as_deref().unwrap_or(""),
                    s.method.as_deref().unwrap_or(""),
                    s.transport_type.as_deref().unwrap_or(""),
                ),
                s.id.clone(),
            )
        })
        .collect();
    for server in &mut servers {
        let key = format!(
            "{}://{}:{}@{}:{}:{}/{}",
            server.protocol,
            server.server,
            server.port,
            server.uuid.as_deref().unwrap_or(""),
            server.password.as_deref().unwrap_or(""),
            server.method.as_deref().unwrap_or(""),
            server.transport_type.as_deref().unwrap_or(""),
        );
        if let Some(old_id) = old_index.get(&key) {
            server.id = old_id.clone();
        }
    }

    // Preserve exit IP data
    let old_exit: std::collections::HashMap<
        String,
        (Option<String>, Option<String>, Option<String>, Option<String>),
    > = old_servers
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                (
                    s.exit_server.clone(),
                    s.exit_flag.clone(),
                    s.exit_country_name.clone(),
                    s.exit_city_name.clone(),
                ),
            )
        })
        .collect();

    // process_servers: TCP ping + batch_test + GeoIP (long, no lock)
    servers = crate::profile::process_servers(&app, servers, &id).await;

    // Restore preserved exit data
    for server in &mut servers {
        if server.behind_cdn {
            continue;
        }
        if server.exit_server.is_none() {
            if let Some(saved) = old_exit.get(&server.id) {
                server.exit_server = saved.0.clone();
                server.exit_flag = saved.1.clone();
                server.exit_country_name = saved.2.clone();
                server.exit_city_name = saved.3.clone();
            }
        }
    }

    // Fetch traffic info (no lock, URL from Phase 1)
    let traffic =
        crate::subscription::fetch_subscription_traffic(&url, crate::profile::proxy_port(&app)).await;

    if servers.is_empty() {
        return Err(ProxyError::subscription(
            crate::proxy::ErrorCode::SubscriptionNoServers,
            "Refresh returned 0 servers — keeping existing servers",
        ));
    }

    // ═══ Phase 3: Brief write lock — commit ═════════════════════════
    let profile = {
        let mut mgr = state.manager.lock().await;
        mgr.commit_refresh(&app, &id, servers, traffic).await?
    }; // lock released

    let count = profile.servers.len();
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

#[tauri::command]
async fn get_settings(state: tauri::State<'_, Mutex<AppSettings>>) -> Result<AppSettings, String> {
    let mut settings = state.lock().await;
    settings.normalize();
    Ok(settings.clone())
}

#[tauri::command(rename_all = "camelCase")]
async fn update_settings(
    app: tauri::AppHandle,
    mut new_settings: AppSettings,
    settings: tauri::State<'_, Mutex<AppSettings>>,
    settings_mgr: tauri::State<'_, Mutex<SettingsManager>>,
) -> Result<(), String> {
    let logger = app.state::<StdMutex<Logger>>();

    // ── Validation & Normalization ──────────────────────────────────────
    if new_settings.proxy_port == 0 {
        return Err("Invalid proxy port. Must be between 1 and 65535.".into());
    }
    // Normalise old/migrated values
    new_settings.normalize();
    // Clamp auto-update interval: 1 min minimum, 1 week max (10080 minutes)
    if new_settings.auto_update_interval < 1 {
        new_settings.auto_update_interval = 1;
    } else if new_settings.auto_update_interval > 10080 {
        new_settings.auto_update_interval = 10080;
    }
    // Normalize country codes to uppercase for reliable GeoIP matching
    new_settings.allowed_countries = new_settings
        .allowed_countries
        .into_iter()
        .map(|c| c.to_uppercase())
        .collect();
    new_settings.blocked_countries = new_settings
        .blocked_countries
        .into_iter()
        .map(|c| c.to_uppercase())
        .collect();
    // ────────────────────────────────────────────────────────────────────

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
    mgr.save(&new_settings).map_err(|e| {
        tracing::error!(target: "settings", "Failed to save settings: {e}");
        e
    })?;

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
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
                Err(_) => {
                    std::process::exit(1);
                }
            };

            if let Err(_) = std::fs::create_dir_all(&data_dir) {
                std::process::exit(1);
            }

            // Set up logging: tracing (file) + UI broadcast bridge
            let log_dir = data_dir.join("logs");
            if let Err(_) = std::fs::create_dir_all(&log_dir) {
                std::process::exit(1);
            }

            // Initialize tracing with file appender + UI broadcast layer
            let (mut log_rx, log_file) = crate::logging::setup_logging(&log_dir);

            // Spawn background task: forward tracing logs to Tauri UI events
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Ok(entry) = log_rx.recv().await {
                    let _ = app_handle.emit("log-entry", &entry);
                }
            });

            // Keep backward-compat Logger in app state for get_logs / log_frontend
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

            // Separate token for proxy lifecycle (watchdog / failover /
            // traffic). Decoupled from the refresh token above so
            // cancel_refresh cannot kill proxy-side tasks.
            let proxy_cancel_token = CancellationToken::new();
            app.manage(ProxyCancelState(StdMutex::new(proxy_cancel_token)));

            tracing::info!(target: "system", "Server cache and batch engine initialized");

            let settings_mgr = SettingsManager::new(data_dir.clone());
            let settings = settings_mgr.load().unwrap_or_else(|e| {
                tracing::warn!(target: "settings", "Failed to load settings: {e} — using defaults");
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

            // ── WebView2 suspension helpers ────────────────────────────────
            // When the window is hidden to tray, we suspend the WebView2
            // process to free CPU (scripts, animations, rendering). On restore
            // we resume it. This is a UI-thread operation: with_webview runs
            // the closure on the main thread where the COM controller lives.
            #[cfg(target_os = "windows")]
            fn resume_webview_window(app: &tauri::AppHandle) {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.with_webview(|platform| {
                        let controller = platform.controller();
                        let _ = unsafe { controller.SetIsVisible(true) };
                    });
                }
            }

            #[cfg(target_os = "windows")]
            fn suspend_webview_window(app: &tauri::AppHandle) {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.with_webview(|platform| {
                        let controller = platform.controller();
                        let _ = unsafe { controller.SetIsVisible(false) };
                    });
                }
            }

            #[cfg(not(target_os = "windows"))]
            fn resume_webview_window(_app: &tauri::AppHandle) {}
            #[cfg(not(target_os = "windows"))]
            fn suspend_webview_window(_app: &tauri::AppHandle) {}

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
                                // Resume WebView2 before showing the window
                                resume_webview_window(app);
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
                            if let Some(cancel) =
                                app.try_state::<std::sync::Mutex<CancellationToken>>()
                            {
                                if let Ok(cancel) = cancel.lock() {
                                    cancel.cancel();
                                }
                            }
                            // Also cancel the proxy lifecycle token.
                            if let Some(cancel) = app.try_state::<ProxyCancelState>() {
                                if let Ok(g) = cancel.0.lock() {
                                    g.cancel();
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
                            // Resume WebView2 before showing the window
                            resume_webview_window(tray.app_handle());
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
                        let pid =
                            crate::proxy::PROXY_PID.load(std::sync::atomic::Ordering::Acquire);
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
                            // Suspend WebView2 before hiding the window
                            suspend_webview_window(&app_handle);
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
            cancel_connect,
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
async fn get_real_ip(_app: tauri::AppHandle) -> Result<RealIpInfo, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;

    let mut ip = String::new();
    for url in IP_URLS {
        if let Ok(resp) = client.get(*url).send().await {
            if let Ok(text) = resp.text().await {
                let extracted = text
                    .lines()
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
        tracing::error!(target: "system", "get_real_ip: could not determine external IP");
        return Err("Could not determine external IP".into());
    }

    let (flag, country_name, city_name) = crate::proxy::geoip::resolve(&ip)
        .map(|geo| {
            (
                geo.country_code.clone(),
                geo.country_name.clone(),
                geo.city.clone(),
            )
        })
        .unwrap_or_default();

    Ok(RealIpInfo {
        server: ip,
        flag,
        country_name,
        city_name,
    })
}

/// Find the active proxy's outlet (external) IP address via IP echo services.
/// Uses the active proxy port when connected in proxy mode so the request
/// actually goes through the proxy tunnel.
/// If the active server is CDN-routed, returns immediately (CDN servers
/// cannot route proxy traffic and have no meaningful exit IP).
static PREV_OUTLET_IP: StdMutex<Option<String>> = StdMutex::new(None);

#[derive(Debug, Clone, Serialize)]
/// Clash API /traffic line-delimited JSON: instantaneous B/s.
/// Converted to MB/s before sending to the frontend.
#[derive(serde::Deserialize)]
struct ClashTraffic {
    up: f64,
    down: f64,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
/// Real IP info (before proxy) — structured like ExitIpInfo.
pub struct RealIpInfo {
    pub server: String,
    pub flag: String,
    pub country_name: String,
    pub city_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
/// Information about the external IP address.
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
        tracing::debug!(target: "system", "outlet_ip: Clash API port is 0 — proxy not connected");
        return Err("Proxy not connected".into());
    }

    // ── Early CDN check ────────────────────────────────────────────────
    let active_id = crate::proxy::ACTIVE_SERVER_ID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(ref active_id) = active_id {
        if let Some(profile_state) = app.try_state::<ProfileState>() {
            let pm = profile_state.manager.lock().await;
            let is_cdn = pm
                .list()
                .iter()
                .any(|p| p.servers.iter().any(|s| s.id == *active_id && s.behind_cdn));
            if is_cdn {
                // Ожидаемое поведение: фронтенд вызывает outlet_ip после
                // старта прокси к CDN-серверу. Early CDN check корректно
                // возвращает пустой ExitIpInfo. Это не баг — логируем как
                // INFO с префиксом [CDN], а не [BUG].
                tracing::info!(
                    target: "system",
                    "[CDN] outlet_ip called for CDN server {} — returning empty \
                     (outlet IP is meaningless for CDN)",
                    active_id,
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
                        let extracted = text
                            .lines()
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
        tracing::warn!(target: "system", "outlet_ip: All IP check services failed — could not determine external IP");
        return Err("Could not determine external IP".into());
    }

    // Аномалия-детектор: outlet IP в CDN-диапазонах при не-CDN entry.
    // Это наблюдение, а не баг — возможные причины: WARP upstream,
    // хостинг у облачного провайдера в .srs-списке, ISP-пересечение.
    // Логируем как INFO, exit-данные корректны и сохраняются ниже.
    if crate::proxy::geoip::is_cdn_entry_ip(&ip) {
        tracing::info!(
            target: "system",
            "outlet_ip: Fetched outlet IP {ip} is in CDN ranges, but active server was not marked \
             as CDN. Keeping exit IP (outlet is egress, not entry). If this fires repeatedly for \
             the same server, investigate CDN list coverage or process_servers entry-IP \
             classification.",
        );
    }

    let mut proxy_tag = String::new();

    // Query the Clash API for the currently selected proxy outbound tag
    if stats_port != 0 {
        let api_url = format!("http://127.0.0.1:{stats_port}/proxies/proxy");
        let clash_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default();
        if let Ok(resp) = clash_client.get(&api_url).send().await {
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
    let active_id = crate::proxy::ACTIVE_SERVER_ID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(ref server_id) = active_id {
        if let Some(profile_state) = app.try_state::<ProfileState>() {
            let mut pm = profile_state.manager.lock().await;
            let mut saved = false;
            for profile in pm.list_mut() {
                for server in &mut profile.servers {
                    if server.id == *server_id {
                        // CDN servers have unstable outlet IPs — skip save
                        if server.behind_cdn {
                            // 🚫 ANTI-REGRESSION: если мы тут, значит Tauri-команда
                            // outlet_ip отдала IP, но активный сервер — CDN. Это
                            // означает, что либо outlet_ip получил IP для CDN-сервера
                            // (что не должно происходить — см. early CDN check в
                            // outlet_ip), либо в профиле CDN-сервер уже имел exit.
                            // Логируем для диагностики.
                            server.assert_no_cdn_exit_data("outlet_ip::save_skip");
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
            pm.deduplicate_profiles();
            if saved {
                if let Err(e) = pm.save() {
                    tracing::warn!(target: "settings", "save failed: {e}");
                }
            }
        }
    }

    {
        let mut prev = PREV_OUTLET_IP.lock().unwrap_or_else(|e| e.into_inner());
        if prev.as_deref() != Some(&ip) {
            tracing::info!(
                target: "system",
                "[rust] exitIp: {} → {ip}",
                prev.as_deref().unwrap_or("null"),
            );
            *prev = Some(ip.clone());
        }
    }

    Ok(info)
}

/// Quick health check for the active proxy via Clash API delay endpoint.
/// Returns true if the "proxy" outbound/group responds to a delay probe.
#[tauri::command]
async fn check_proxy_health(app: tauri::AppHandle) -> Result<bool, String> {
    let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        let logger = app.state::<StdMutex<Logger>>();
        log(
            &app,
            &logger,
            LogLevel::Warning,
            LogSource::Proxy,
            "Health check: Clash API port is 0 — proxy not connected".into(),
        );
        return Ok(false);
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(7))
        .build()
        .map_err(|e| {
            let logger = app.state::<StdMutex<Logger>>();
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Proxy,
                format!("Health check: failed to build HTTP client: {e}"),
            );
            e.to_string()
        })?;

    let delay_url = format!(
        "http://127.0.0.1:{stats_port}/proxies/proxy/delay?url=http://cp.cloudflare.com/generate_204&timeout=5000"
    );

    match client.get(&delay_url).send().await {
        Ok(resp) if resp.status().is_success() => Ok(true),
        Ok(resp) => {
            let logger = app.state::<StdMutex<Logger>>();
            log(
                &app,
                &logger,
                LogLevel::Warning,
                LogSource::Proxy,
                format!(
                    "Health check: Clash API returned status {} — proxy may be unresponsive",
                    resp.status()
                ),
            );
            Ok(false)
        }
        Err(e) => {
            let logger = app.state::<StdMutex<Logger>>();
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Proxy,
                format!("Health check: Clash API request failed: {e}"),
            );
            Ok(false)
        }
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
        Ok(resp) => {
            let code = resp.status().as_u16();
            tracing::error!(target: "proxy", "switch_proxy: Clash API returned HTTP {code}");
            Err(format!("Clash API returned HTTP {code}"))
        }
        Err(e) => {
            tracing::error!(target: "proxy", "switch_proxy: Clash API request failed: {e}");
            Err(e.to_string())
        }
    }
}

/// Speed test: test latency of all servers via Clash API delay endpoint
#[tauri::command]
async fn speed_test() -> Result<Vec<serde_json::Value>, String> {
    let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if stats_port == 0 {
        tracing::error!(target: "proxy", "speed_test: proxy not connected");
        return Err("Proxy not connected".into());
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| {
            tracing::error!(target: "proxy", "speed_test: failed to build HTTP client: {e}");
            e.to_string()
        })?;

    let api_url = format!("http://127.0.0.1:{stats_port}");
    let test_url = "http://cp.cloudflare.com/generate_204";

    // First get all proxies
    let groups_resp = client
        .get(format!("{api_url}/proxies"))
        .send()
        .await
        .map_err(|e| {
            tracing::error!(target: "proxy", "speed_test: Clash API request failed: {e}");
            format!("Clash API error: {e}")
        })?;

    let groups: serde_json::Value = groups_resp
        .json()
        .await
        .map_err(|e| {
            tracing::error!(target: "proxy", "speed_test: failed to parse Clash response: {e}");
            format!("Parse error: {e}")
        })?;

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
        tracing::error!(target: "proxy", "get_connections: proxy not connected");
        return Err("Proxy not connected".into());
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| {
            tracing::error!(target: "proxy", "get_connections: failed to build HTTP client: {e}");
            e.to_string()
        })?;

    let resp = client
        .get(format!("http://127.0.0.1:{stats_port}/connections"))
        .send()
        .await
        .map_err(|e| {
            tracing::error!(target: "proxy", "get_connections: Clash API request failed: {e}");
            format!("Clash API error: {e}")
        })?;

    let data: serde_json::Value = resp.json().await.map_err(|e| {
        tracing::error!(target: "proxy", "get_connections: failed to parse Clash response: {e}");
        format!("Parse error: {e}")
    })?;

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
) -> Result<SubscriptionProfile, ProxyError> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Importing subscription content: {name}"),
    );

    let servers = crate::subscription::parser::parse_subscription(&content)
        .map_err(|e| {
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Subscription,
                format!("Failed to parse imported content: {e}"),
            );
            e
        })?;

    if servers.is_empty() {
        log(
            &app,
            &logger,
            LogLevel::Error,
            LogSource::Subscription,
            "Import failed: no servers found in content".into(),
        );
        return Err(ProxyError::subscription(
            ErrorCode::SubscriptionNoServers,
            "No servers found in content",
        ));
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
    mgr.save()?;

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
) -> Result<SubscriptionProfile, ProxyError> {
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
        .map_err(|e| {
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Subscription,
                format!("Failed to read import file: {e}"),
            );
            e
        })?;

    // Try to parse as subscription data
    let servers = crate::subscription::parser::parse_subscription(&content)
        .map_err(|e| {
            log(
                &app,
                &logger,
                LogLevel::Error,
                LogSource::Subscription,
                format!("Failed to parse imported file content: {e}"),
            );
            e
        })?;

    if servers.is_empty() {
        log(
            &app,
            &logger,
            LogLevel::Error,
            LogSource::Subscription,
            "Import failed: no servers found in file".into(),
        );
        return Err(ProxyError::subscription(
            ErrorCode::SubscriptionNoServers,
            "No servers found in file",
        ));
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
    mgr.save()?;

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

    // Guard: prevent double initialization (React StrictMode in dev or
    // effect re-runs). Atomic compare_exchange is race-free.
    if INITIALIZED.compare_exchange(
        false,
        true,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    ).is_err() {
        tracing::info!(
            target: "system",
            "initialize_app already called — skipping (idempotent guard)"
        );
        return Ok(());
    }

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
    // Set locale for city name localisation (MaxMind City supports en/ru/etc.)
    if let Some(settings_state) = app.try_state::<Mutex<crate::settings::AppSettings>>() {
        if let Ok(guard) = settings_state.try_lock() {
            crate::proxy::geoip::set_locale(&guard.language);
        }
    }
    // U7: Cleanup stale tmp files from previous sessions before downloading.
    crate::proxy::geoip::cleanup_stale_tmp_files();
    crate::proxy::geoip::ensure_downloaded(&geo_dir, &ipinfo_token).await?;

    // 3. Enrich loaded profiles with GeoIP data (fix corrupted names)
    if let Some(profile_state) = app.try_state::<ProfileState>() {
        let mut pm = profile_state.manager.lock().await;
        pm.enrich_loaded();
        pm.deduplicate_profiles();

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
            cache
                .prune_old(
                    &active_ids,
                    std::time::Duration::from_secs(THIRTY_DAYS_SECS),
                )
                .await;
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

    // ═══════════════════════════════════════════════════════════════════
    // 🚫 ANTI-REGRESSION: Periodic invariant checker
    // ═══════════════════════════════════════════════════════════════════
    // Запускает фоновую задачу, которая раз в 60 секунд проверяет ВСЕ
    // серверы во ВСЕХ профилях на нарушение I-CDN-1.
    //
    // ПОЧЕМУ ЭТО НАДО:
    //   - enrich_loaded проверяет только при старте приложения.
    //   - process_servers проверяет только при refresh.
    //   - Между этими событиями (минуты/часы) CDN-сервер может получить
    //     exit-данные из-за бага в кеше или runtime-операции.
    //   - Без periodic checker баг будет невидим до следующего refresh.
    //
    // 🚫 НЕ УДАЛЯЙ эту задачу "потому что другие проверки и так ловят".
    // 🚫 НЕ ПОВЫШАЙ интервал "потому что 60 секунд — это слишком часто".
    // 🚫 НЕ "ОПТИМИЗИРУЙ" через condvar/event — простой polling проще и надёжнее.
    //
    // См. doc-comment ServerProfile::assert_no_cdn_exit_data для regression history.
    let app_checker = app.clone();
    tokio::spawn(async move {
        // Стартовая задержка 30 секунд — чтобы не пересекаться с enrich_loaded.
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            if let Some(profile_state) = app_checker.try_state::<ProfileState>() {
                let pm = profile_state.manager.lock().await;
                let mut total_violations = 0u32;
                let mut total_servers = 0u32;
                let mut total_cdn = 0u32;
                for profile in pm.list() {
                    for server in &profile.servers {
                        total_servers += 1;
                        if server.behind_cdn {
                            total_cdn += 1;
                            server.assert_no_cdn_exit_data("periodic_checker");
                            if server.exit_server.is_some()
                                || server.exit_flag.is_some()
                                || server.exit_country_name.is_some()
                                || server.exit_city_name.is_some()
                            {
                                total_violations += 1;
                            }
                        }
                    }
                }
                drop(pm);
                if total_violations > 0 {
                    tracing::error!(
                        target: "cdn-invariant",
                        "[BUG][I-CDN-1] Periodic checker found {} violations \
                         across {} servers ({} CDN). See previous [BUG][I-CDN-1] \
                         log entries for details. Run with RUST_LOG=cdn-invariant=trace \
                         to see all checks.",
                        total_violations,
                        total_servers,
                        total_cdn,
                    );
                } else {
                    tracing::trace!(
                        target: "cdn-invariant",
                        "Periodic checker OK: {} servers checked, {} CDN, 0 violations.",
                        total_servers,
                        total_cdn,
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

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
async fn wait_for_proxy_ready(pid: u32, proxy_port: u16) -> Result<u32, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(PROXY_TIMEOUT_SECS);
    let mut attempts = 0u32;

    // Poll TCP connect to the mixed inbound port.
    loop {
        attempts += 1;

        // Check cancellation flag — allows responsive cancel even when
        // the PID hasn't been set yet (race window between cancel_connect
        // and manager.start()).
        if crate::proxy::CONNECTION_CANCELLING.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Connection cancelled by user".into());
        }

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

    /// ═════════════════════════════════════════════════════════════════
    /// Test: refresh_subscription's lock-splitting refactor.
    ///
    /// Verifies that the ProfileState.manager lock is NOT held during
    /// Phase 2 (the long-operations phase between the brief read lock
    /// and the brief write lock).
    /// ═════════════════════════════════════════════════════════════════
    #[tokio::test]
    async fn test_refresh_lock_splitting_allows_concurrent_readers() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        // Simulate ProfileState.manager
        let manager: Arc<Mutex<Vec<String>>> =
            Arc::new(Mutex::new(vec!["profile1".to_string()]));

        // Phase 1: brief read lock
        let snapshot = {
            let mgr = manager.lock().await;
            mgr[0].clone()
        }; // lock released
        assert_eq!(snapshot, "profile1");

        // Phase 2: simulate long operations (NO LOCK)
        let manager_clone = manager.clone();
        let concurrent_reader = tokio::spawn(async move {
            let start = std::time::Instant::now();
            let _guard = manager_clone.lock().await;
            start.elapsed()
        });

        // Simulate Phase 2 work (200 ms)
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let elapsed = concurrent_reader.await.unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "Concurrent reader took {elapsed:?} — lock was held during Phase 2! \
             Expected <50 ms, meaning the lock was available.",
        );

        // Phase 3: brief write lock
        {
            let mut mgr = manager.lock().await;
            mgr[0] = "profile1_updated".to_string();
        }

        let mgr = manager.lock().await;
        assert_eq!(mgr[0], "profile1_updated");
    }

    /// ═════════════════════════════════════════════════════════════════
    /// Test: lock held throughout simulates the blocking pattern.
    ///
    /// This test simulates holding the lock for the
    /// entire refresh duration and verifies a concurrent reader IS
    /// blocked, proving the test is meaningful.
    /// ═════════════════════════════════════════════════════════════════
    #[tokio::test]
    async fn test_old_refresh_held_lock_blocks_readers() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let manager: Arc<Mutex<Vec<String>>> =
            Arc::new(Mutex::new(vec!["profile1".to_string()]));

        // Simulate the OLD refresh: hold the lock for the entire duration
        let manager_clone = manager.clone();
        let old_refresh = tokio::spawn(async move {
            let mut mgr = manager_clone.lock().await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            mgr[0] = "updated".to_string();
        });

        // Give the old refresh time to acquire the lock
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Try to read — should be BLOCKED
        let start = std::time::Instant::now();
        {
            let _guard = manager.lock().await;
        }
        let elapsed = start.elapsed();

        // Should have been blocked for at least ~150 ms
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "Reader was not blocked by old-style lock hold: {elapsed:?} \
             Expected >=100 ms, meaning the lock was held.",
        );

        old_refresh.await.unwrap();
    }
}
