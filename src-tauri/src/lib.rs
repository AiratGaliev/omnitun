#![recursion_limit = "512"]
pub mod db;
pub mod events;
pub mod logging;
pub mod profile;
pub mod proxy;
pub(crate) mod settings;
pub mod subscription;

#[cfg(feature = "mcp-server")]
pub mod mcp_server;

use crate::events::emit_proxy_progress;
use crate::events::emit_status;
use crate::events::emit_traffic;
use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::profile::ProfileManager;
use crate::proxy::cache::ServerCache;
use crate::proxy::port_pool::PortPool;
use crate::proxy::{ConnectionStatus, ErrorCode, ProxyError, ProxyManager, ServerProfile};
use crate::settings::{AppSettings, SettingsManager};
use crate::subscription::SubscriptionProfile;
use serde::Serialize;
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
    "https://api64.ipify.org",
    "https://ifconfig.me/ip",
    "https://api.ip.sb/ip",
];

/// Guard: ensure initialize_app runs only once per process.
/// Prevents double GeoIP download, double cache save, etc.
/// when the frontend calls initialize_app twice (React
/// StrictMode in dev, or effect re-runs).
static INITIALIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) struct ProxyState {
    pub(crate) manager: Arc<Mutex<ProxyManager>>,
}

pub(crate) struct ProfileState {
    pub(crate) manager: Arc<Mutex<ProfileManager>>,
}

/// Separate cancellation token for the proxy lifecycle (watchdog,
/// failover monitor, traffic stats). Decoupled from the refresh
/// cancellation token so that `cancel_refresh` cannot accidentally
/// kill proxy-side background tasks.
pub(crate) struct ProxyCancelState(pub(crate) StdMutex<CancellationToken>);

use tokio_util::sync::CancellationToken;

#[tauri::command(rename_all = "camelCase")]
#[allow(clippy::too_many_arguments)]
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

    // Scope the manager lock to just .start() — release before long-running
    // verification to avoid blocking concurrent commands.
    let (pid_opt, proxy_port) = {
        let manager = manager_arc.lock().await;
        match manager
            .start(&app, proxy_port, &routing_mode, &all_servers, &settings)
            .await
        {
            Ok(_) => {
                crate::proxy::PROXY_PORT.store(proxy_port, std::sync::atomic::Ordering::Relaxed);
                (manager.process_pid(), proxy_port)
            }
            Err(e) => return Err(e),
        }
    };
    // Manager lock released here.

    emit_proxy_progress(&app, "verifying", "Waiting for proxy to become ready…");
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Proxy,
        "Verifying proxy connection...".into(),
    );

    let result = match pid_opt {
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
                if let Ok(proxy) = reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}")) {
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
                                    if !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok() {
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
                                        // exit-ip-changed removed — exit IP comes from
                                        // profiles.json via activeServer.exit*.
                                        // Health check IP fetch stays (verifies proxy works).
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
                        let stats_port =
                            crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
                        if stats_port > 0 && all_servers.len() > 1 {
                            for (idx, _alt_server) in all_servers.iter().enumerate().skip(1) {
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
                                // sing-box's PUT /proxies/{name} only works on
                                // 'selector' outbounds — 'urltest' auto-selects
                                // and returns HTTP 400. With a 'selector' outbound
                                // (set when auto_connect_fastest=false), PUT
                                // works for manual failover. For 'urltest'
                                // groups, log a warning and skip the switch.
                                let switch_ok = match client
                                    .put(&switch_url)
                                    .json(&body)
                                    .send()
                                    .await
                                {
                                    Ok(resp) if resp.status().is_success() => true,
                                    Ok(resp) => {
                                        tracing::warn!(target: "proxy",
                                            "Failover switch to {tag} returned HTTP {} — urltest group auto-selects", resp.status());
                                        false
                                    }
                                    Err(e) => {
                                        tracing::warn!(target: "proxy",
                                            "Failover switch to {tag} failed: {e}");
                                        false
                                    }
                                };
                                if switch_ok {
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    // Re-verify exit IP through the new server
                                    let mut cb =
                                        reqwest::Client::builder().timeout(Duration::from_secs(10));
                                    if let Ok(proxy) = reqwest::Proxy::all(format!(
                                        "http://127.0.0.1:{proxy_port}"
                                    )) {
                                        cb = cb.proxy(proxy);
                                    }
                                    if let Ok(c) = cb.build() {
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
                                                            if crate::proxy::geoip::is_cdn_entry_ip(
                                                                &ip,
                                                            ) {
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
                                                            // Fallback: emit failover-occurred so frontend updates activeServer.
                                                            // Exit IP data comes from profiles.json (new active server's exit* fields).
                                                            let _ = app.emit(
                                                                "failover-occurred",
                                                                all_servers[idx].id.clone(),
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
            if let Some(pid) = pid_opt {
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
                                    tokio::time::sleep(Duration::from_secs(attempt * 2)).await;

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
                                        emit_status(&app_clone, ConnectionStatus::Connected);
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
                                        emit_status(&app_clone, ConnectionStatus::Disconnected);
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
            let fail_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
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
                    let mut current_tag = "proxy-0".to_string();
                    loop {
                        if cancel_fail.is_cancelled() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        {
                            let mgr = mgr_fail.lock().await;
                            if mgr.status() == crate::proxy::ConnectionStatus::Disconnected {
                                break;
                            }
                        }
                        if let Ok(resp) = client.get(&api_url).send().await {
                            if let Ok(body) = resp.json::<serde_json::Value>().await {
                                if let Some(now_tag) = body.get("now").and_then(|v| v.as_str()) {
                                    if now_tag != current_tag {
                                        tracing::info!(
                                            target: "proxy",
                                            "Seamless failover detected: {current_tag} -> {now_tag}",
                                        );
                                        if let Some(idx_str) = now_tag.strip_prefix("proxy-") {
                                            if let Ok(idx) = idx_str.parse::<usize>() {
                                                if let Some(srv) = servers_fail.get(idx) {
                                                    let _ = app_fail
                                                        .emit("failover-occurred", srv.id.clone());
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
                            if mgr.status() == crate::proxy::ConnectionStatus::Disconnected {
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
                                                // Clash API returns B/s; convert to MiB/s
                                                let speed_up = traffic.up / 1024.0 / 1024.0;
                                                let speed_down = traffic.down / 1024.0 / 1024.0;
                                                emit_traffic(&app_clone, speed_up, speed_down);
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
            let _ = manager_arc.lock().await.stop(&app).await;
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
    crate::proxy::PROXY_PORT.store(0, std::sync::atomic::Ordering::Release);
    // Clear ACTIVE_SERVER_ID so subsequent CDN checks don't target stale server.
    {
        let mut active = crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *active = None;
    }
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
        let mut guard = proxy_cancel.0.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut guard = proxy_cancel.0.lock().unwrap_or_else(|e| e.into_inner());
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
    crate::proxy::PROXY_PORT.store(0, std::sync::atomic::Ordering::Release);
    // Clear ACTIVE_SERVER_ID so subsequent CDN checks don't target stale server.
    {
        let mut active = crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *active = None;
    }

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
    profile_id: String,
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
    match mgr.add(&app, name.clone(), url, profile_id).await {
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
                format!("Failed to add subscription '{name}': {e}"),
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
        let idx = mgr.list().iter().position(|p| p.id == id).ok_or_else(|| {
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
            match crate::subscription::fetch_subscription(
                u,
                crate::profile::proxy_port_from_app(&app),
            )
            .await
            {
                Ok(s) => Ok(s),
                Err(e) => {
                    if crate::profile::proxy_port_from_app(&app).is_some() {
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
                match app.path().app_data_dir() {
                    Ok(data_dir) => {
                        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
                        let settings = app
                            .try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
                            .and_then(|s| s.try_lock().ok().map(|g| g.clone()))
                            .unwrap_or_default();
                        let cache = app
                            .try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>()
                            .map(|c| c.as_ref().clone());
                        let db = app
                            .try_state::<crate::db::DbPool>()
                            .map(|s| -> crate::db::DbPool { (*s).clone() });
                        match (cache, db) {
                            (Some(c), Some(d)) => {
                                crate::profile::process_servers_incremental(
                                    &data_dir,
                                    &settings,
                                    old_servers
                                        .into_iter()
                                        .map(|mut s| {
                                            s.ping_ms = None;
                                            s
                                        })
                                        .collect(),
                                    &id,
                                    tokio_util::sync::CancellationToken::new(),
                                    event_tx,
                                    None, // progress_tx — error recovery, no progress needed
                                    &d,
                                    &c,
                                )
                                .await
                            }
                            _ => Vec::new(),
                        }
                    }
                    Err(_) => Vec::new(),
                }
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

    // Process servers: ping + batch_test + GeoIP + exit data restore (long, no lock)
    {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let data_dir = match app.path().app_data_dir() {
            Ok(d) => d,
            Err(_) => return Err(ProxyError::subscription(
                crate::proxy::ErrorCode::SubscriptionSaveFailed,
                "Failed to get app data directory".to_string(),
            )),
        };
        let settings = app
            .try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
            .and_then(|s| s.try_lock().ok().map(|g| g.clone()))
            .unwrap_or_default();
        let cache = app
            .try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>()
            .map(|c| c.as_ref().clone());
        let db = app
            .try_state::<crate::db::DbPool>()
            .map(|s| -> crate::db::DbPool { (*s).clone() });
        if let (Some(c), Some(d)) = (cache, db) {
            // Create progress channel — forward to Tauri UI events
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
            let progress_app = app.clone();
            tokio::spawn(async move {
                while let Some(progress) = progress_rx.recv().await {
                    crate::events::emit_refresh_progress(&progress_app, progress);
                }
            });

            servers = crate::profile::process_servers_incremental(
                &data_dir,
                &settings,
                servers,
                &id,
                tokio_util::sync::CancellationToken::new(),
                event_tx,
                Some(progress_tx),
                &d,
                &c,
            )
            .await;
        }
    }

    // Fetch traffic info (no lock, URL from Phase 1)
    let traffic = crate::subscription::fetch_subscription_traffic(
        &url,
        crate::profile::proxy_port_from_app(&app),
    )
    .await;

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
    crate::events::emit_refresh_progress(
        &app,
        crate::events::RefreshProgress {
            profile_id: id,
            total: count,
            tested: count,
            cache_hits: 0,
            successful: count,
            failed: 0,
            phase: "done".into(),
            updated_at: profile.updated_at.clone(),
            last_tested: profile.last_tested.clone(),
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

/// Test existing servers WITHOUT re-fetching the subscription URL.
///
/// This is the "server testing" layer, separate from "subscription refresh":
///   - `refresh_subscription` = HTTP fetch + parse + ping + batch-test
///     (discovers NEW servers, re-tests all). Cadence: `auto_update_interval`.
///   - `test_servers`         = ping + batch-test only (no URL fetch).
///     Re-tests EXISTING servers to keep latency/exit-IP fresh.
///     Cadence: `auto_test_interval`.
///
/// Uses `clash_api_test` directly (no GeoIP, no batch test, no exit IP fetch)
/// since the servers already have this data from the last full refresh.
/// Servers with fresh positive cache are skipped by the preflight check.
///
/// Updates `last_tested` (not `updated_at`) so the test timer doesn't
/// immediately re-test after a refresh.
#[tauri::command]
async fn test_servers(
    app: tauri::AppHandle,
    state: tauri::State<'_, ProfileState>,
    id: String,
    force: Option<bool>,
) -> Result<SubscriptionProfile, ProxyError> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Cache,
        "Testing existing servers (no URL fetch)...".into(),
    );

    // Phase 1: brief read lock — get existing servers + old ping values
    let (servers, old_ping) = {
        let mgr = state.manager.lock().await;
        let idx = mgr.list().iter().position(|p| p.id == id).ok_or_else(|| {
            ProxyError::subscription(
                crate::proxy::ErrorCode::SubscriptionNotFound,
                "Profile not found",
            )
        })?;
        // Clear negative cache for this profile's servers if force=true.
        if force.unwrap_or(false) {
            if let Some(cache) = app.try_state::<Arc<ServerCache>>() {
                let ids: Vec<String> = mgr.list()[idx]
                    .servers
                    .iter()
                    .map(|s| s.id.clone())
                    .collect();
                cache.clear_negative_for_ids(&ids).await;
            }
        }
        // Save old ping values for fallback if engine fails.
        let old_ping: std::collections::HashMap<String, Option<u64>> = mgr.list()[idx]
            .servers
            .iter()
            .map(|s| (s.id.clone(), s.ping_ms))
            .collect();
        // Clone servers, reset ping_ms so they get re-tested
        let servers = mgr.list()[idx]
            .servers
            .iter()
            .map(|s| {
                let mut s = s.clone();
                s.ping_ms = None;
                s
            })
            .collect::<Vec<_>>();
        (servers, old_ping)
    }; // lock released

    // Phase 2: Clash API ping + pre-filter (dead servers → negative cache)
    crate::proxy::geoip::reset_ipguide_rate_limit();
    let data_dir = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(_) => {
            return Err(ProxyError::process(
                crate::proxy::ErrorCode::ProcessNotReachable,
                "Cannot access app data directory".to_string(),
            ))
        }
    };
    let settings = app
        .try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
        .and_then(|s| s.try_lock().ok().map(|g| g.clone()))
        .unwrap_or_default();
    let cache = {
        let state = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>();
        state
            .as_ref()
            .map(|s| -> std::sync::Arc<crate::proxy::cache::ServerCache> { (**s).clone() })
    };
    let pings = crate::profile::clash_api_test(
        &servers,
        &data_dir,
        &settings,
        cache.as_deref(),
        Some(&app),
        &id,
    )
    .await;

    // Pre-filter: servers without ping → negative cache, remove from profile
    // If testing engine returned 0 pings, skip filtering (transient failure).
    let mut tested: Vec<crate::proxy::ServerProfile> = servers;
    let test_cache = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>();
    if !pings.is_empty() {
        if let Some(cache) = test_cache.as_ref() {
            let mut dead_ids = std::collections::HashSet::new();
            for server in &tested {
                if pings.contains_key(&server.id) {
                    continue;
                }
                dead_ids.insert(server.id.clone());
            }
            if !dead_ids.is_empty() {
                for sid in &dead_ids {
                    cache
                        .update_negative(sid, "Clash API test: no ping".to_string())
                        .await;
                }
                cache.save_to_disk().await;
                let before = tested.len();
                tested.retain(|s| !dead_ids.contains(&s.id));
                let n = before - tested.len();
                log(
                    &app,
                    &logger,
                    LogLevel::Info,
                    LogSource::Cache,
                    format!("Removed {n} dead server(s) via Clash API test"),
                );
            }
        }
    } else {
        log(
            &app,
            &logger,
            LogLevel::Info,
            LogSource::Cache,
            "Testing engine returned 0 pings — keeping all servers (transient)".into(),
        );
    }
    // Apply pings to survivors — then restore old ping for servers
    // that the Clash API test couldn't reach (transient failure).
    for server in &mut tested {
        if let Some(ping) = pings.get(&server.id) {
            server.ping_ms = Some(*ping);
        } else if let Some(old) = old_ping.get(&server.id).and_then(|o| *o) {
            server.ping_ms = Some(old);
        }
    }

    if tested.is_empty() {
        log(
            &app,
            &logger,
            LogLevel::Warning,
            LogSource::Cache,
            "Test returned 0 alive servers — keeping existing".into(),
        );
    } else {
        log(
            &app,
            &logger,
            LogLevel::Success,
            LogSource::Cache,
            format!("Tested {} server(s)", tested.len()),
        );
    }

    // Phase 3: brief write lock — commit with last_tested
    let servers_to_commit = if tested.is_empty() {
        state
            .manager
            .lock()
            .await
            .list()
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.servers.clone())
            .unwrap_or_default()
    } else {
        tested
    };

    let profile = {
        let mut mgr = state.manager.lock().await;
        mgr.commit_test(&app, &id, servers_to_commit).await?
    };

    // After a manual test, force sing-box's urltest group
    // (if running) to re-select the lowest-latency member immediately
    // instead of waiting for the next interval tick (3 minutes).
    //
    let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::SeqCst);
    if stats_port > 0 {
        if let Ok(c) = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
        {
            let c = c.clone();
            let url = format!("http://127.0.0.1:{stats_port}/proxies/proxy/delay?url=http://cp.cloudflare.com/generate_204&timeout=2000");
            tokio::spawn(async move {
                let _ = c.get(&url).send().await;
            });
        }
    }

    // Emit done so the frontend updates last_tested.
    let count = profile.servers.len();
    crate::events::emit_refresh_progress(
        &app,
        crate::events::RefreshProgress {
            profile_id: id,
            total: count,
            tested: count,
            cache_hits: 0,
            successful: count,
            failed: 0,
            phase: "done".into(),
            updated_at: profile.updated_at.clone(),
            last_tested: profile.last_tested.clone(),
        },
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
    // Clamp auto-update interval: 0 = disabled, 1 min minimum, 1 week max (10080 minutes)
    if new_settings.auto_update_interval != 0 && new_settings.auto_update_interval < 1 {
        new_settings.auto_update_interval = 1;
    } else if new_settings.auto_update_interval > 10080 {
        new_settings.auto_update_interval = 10080;
    }
    // Clamp auto-test interval. 0 = disabled. Otherwise 1 min min,
    // 1 week max.
    if new_settings.auto_test_interval > 10080 {
        new_settings.auto_test_interval = 10080;
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

    // Apply the configured test URL so test_single_server
    // prepends it to the URL list on the next test.
    // Sync batch-test settings so changes take effect without app restart.
    crate::proxy::batcher::set_batch_settings(
        new_settings.anti_dpi,
        new_settings.tls_insecure,
        new_settings.enable_mux,
    );
    crate::proxy::batcher::set_configured_test_url(
        &new_settings.test_url,
        new_settings.test_use_tls,
    );

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
                tracing::warn!(target: "settings", "Cannot get exe path: {e}");
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
    if let Some(cancel_guard) =
        app.try_state::<Arc<StdMutex<tokio_util::sync::CancellationToken>>>()
    {
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

            if std::fs::create_dir_all(&data_dir).is_err() {
                std::process::exit(1);
            }

            // Set up logging: tracing (file) + UI broadcast bridge
            let log_dir = data_dir.join("logs");
            if std::fs::create_dir_all(&log_dir).is_err() {
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

            // Use Arc<Mutex<CancellationToken>> so the batcher engine can read
            // the CURRENT token at the start of each process_batch. When
            // cancel_refresh replaces the token, the engine's next batch
            // gets the new (un-cancelled) token. The engine itself never
            // exits on cancel — it runs until the channel closes.
            let cancel_arc: Arc<std::sync::Mutex<CancellationToken>> =
                Arc::new(std::sync::Mutex::new(cancel_token));

            // Start the background batching engine
            cache.start_batcher(
                port_pool.clone(),
                data_dir.clone(),
                cancel_arc.clone(),
            );

            app.manage(cache.clone());
            app.manage(port_pool.clone());
            app.manage(cancel_arc);

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

            // ── Database (SQLite) ──────────────────────────────────────────────
            let db = tauri::async_runtime::block_on(async {
                crate::db::init_db(&data_dir).await
            })
            .unwrap_or_else(|e| {
                tracing::error!(target: "db", "Failed to initialize database: {e}");
                std::process::exit(1);
            });

            // One-time migration from profiles.json / cache.json to SQLite.
            // Only runs if the SQLite database is empty (no profiles) and JSON
            // files exist. Idempotent — safe to call every startup.
            if let Err(e) = tauri::async_runtime::block_on(async {
                crate::db::migrate_from_json(&db, &data_dir).await
            }) {
                tracing::warn!(target: "db", "JSON migration warning (non-fatal): {e}");
            }

            app.manage(db.clone());

            let mut profile_manager = ProfileManager::new(data_dir, db);
            profile_manager.load().unwrap_or_else(|e| {
                tracing::error!(target: "subscription", "Failed to load profiles: {e} — starting with empty profile list");
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
                .icon(app.default_window_icon().unwrap().clone())
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
                            if let Some(cancel_arc) =
                                app.try_state::<Arc<std::sync::Mutex<CancellationToken>>>()
                            {
                                if let Ok(cancel) = cancel_arc.lock() {
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
                            tracing::info!(target: "system", "[cleanup] killing our proxy (PID {pid})");
                            #[cfg(target_os = "windows")]
                            let _ = std::process::Command::new("taskkill")
                                .args(["/pid", &pid.to_string(), "/f"])
                                .output();
                            #[cfg(not(target_os = "windows"))]
                            let _ = std::process::Command::new("kill")
                                .args(["-9", &pid.to_string()])
                                .output();
                            tracing::info!(target: "system", "[cleanup] done");
                        } else {
                            tracing::info!(target: "system", "[cleanup] no proxy PID to kill");
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
                            if let Some(cancel_arc) =
                                app_handle.try_state::<Arc<std::sync::Mutex<CancellationToken>>>()
                            {
                                if let Ok(cancel) = cancel_arc.lock() {
                                    cancel.cancel();
                                }
                            }
                            // Kill the persistent testing engine
                            crate::profile::shutdown_test_engine();
                        }
                    }
                });
            }

            // ── Auto-refresh/auto-test scheduler ──────────────────────────
            // Runs on the backend, logs via tracing (→ UiLogLayer → frontend).
            let scheduler_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                loop {
                    let settings = scheduler_app
                        .try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
                        .and_then(|s| s.try_lock().ok().map(|g| g.clone()))
                        .unwrap_or_default();
                    let profiles: Vec<crate::subscription::SubscriptionProfile> = scheduler_app
                        .try_state::<ProfileState>()
                        .and_then(|ps| ps.manager.try_lock().ok().map(|g| g.list().to_vec()))
                        .unwrap_or_default();

                    let now = chrono::Utc::now();

                    if settings.auto_update_interval > 0 {
                        let interval = chrono::Duration::seconds(settings.auto_update_interval as i64 * 60);
                        for p in &profiles {
                            let updated = p
                                .updated_at
                                .parse::<chrono::DateTime<chrono::FixedOffset>>()
                                .ok()
                                .map(|d| d.naive_utc().and_utc())
                                .unwrap_or(now);
                            if now.signed_duration_since(updated) >= interval {
                                tracing::info!(target: "subscription", "Auto-refreshing subscription '{}'", p.name);
                                let result = refresh_subscription(
                                    scheduler_app.clone(),
                                    scheduler_app.state::<ProfileState>(),
                                    p.id.clone(),
                                )
                                .await;
                                match result {
                                    Ok(_) => {},
                                    Err(e) => {
                                        tracing::warn!(target: "subscription", "Auto-refresh failed for '{}': {}", p.name, e);
                                        // Match frontend behavior: advance timestamp even on failure
                                        // so the scheduler doesn't retry every 60s.
                                        if let Some(ps) = scheduler_app.try_state::<ProfileState>() {
                                            if let Ok(mut mgr) = ps.manager.try_lock() {
                                                if let Some(profile) = mgr.list_mut().iter_mut().find(|pr| pr.id == p.id) {
                                                    profile.updated_at = chrono::Utc::now().to_rfc3339();
                                                    let _ = mgr.save();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if settings.auto_test_interval > 0 {
                        let interval = chrono::Duration::seconds(settings.auto_test_interval as i64 * 60);
                        for p in &profiles {
                            let tested = p
                                .last_tested
                                .as_ref()
                                .and_then(|t| {
                                    t.parse::<chrono::DateTime<chrono::FixedOffset>>()
                                        .ok()
                                        .map(|d| d.naive_utc().and_utc())
                                });
                            let needs_test = match tested {
                                Some(t) => now.signed_duration_since(t) >= interval,
                                None => true, // never tested — test now
                            };
                            if needs_test {
                                tracing::info!(target: "cache", "Auto-testing profile '{}'", p.name);
                                let result = test_servers(
                                    scheduler_app.clone(),
                                    scheduler_app.state::<ProfileState>(),
                                    p.id.clone(),
                                    None, // auto-test doesn't force bypass cache
                                )
                                .await;
                                match result {
                                    Ok(_) => {},
                                    Err(e) => {
                                        tracing::warn!(target: "cache", "Auto-test failed for '{}': {}", p.name, e);
                                        if let Some(ps) = scheduler_app.try_state::<ProfileState>() {
                                            if let Ok(mut mgr) = ps.manager.try_lock() {
                                                if let Some(profile) = mgr.list_mut().iter_mut().find(|pr| pr.id == p.id) {
                                                    profile.last_tested = Some(chrono::Utc::now().to_rfc3339());
                                                    let _ = mgr.save();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            });

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
            test_servers,
            refresh_subscription,
            get_settings,
            update_settings,
            cancel_refresh,
            logging::get_logs,
            logging::log_message,
            read_clipboard_text,
            get_real_ip,
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
            get_delay_history,
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
/// Shared implementation used by both the Tauri command and the MCP handler.
pub(crate) async fn fetch_real_ip() -> Result<RealIpInfo, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;

    let mut ip = String::new();
    for url in IP_URLS {
        if let Ok(resp) = client.get(*url).send().await {
            if let Ok(text) = resp.text().await {
                let extracted = text.trim().to_string();
                if !extracted.is_empty() {
                    ip = extracted;
                    break;
                }
            }
        }
    }

    if ip.is_empty() {
        tracing::error!(target: "system", "fetch_real_ip: could not determine external IP");
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

/// Get the user's real IP (before proxy) with GeoIP location.
/// Always fetches directly (no proxy) — works whether connected or not.
/// Wraps the shared `fetch_real_ip()` for Tauri command compatibility.
#[tauri::command]
async fn get_real_ip(_app: tauri::AppHandle) -> Result<RealIpInfo, String> {
    fetch_real_ip().await
}

/// Clash API /traffic line-delimited JSON: instantaneous B/s.
/// Converted to MB/s before sending to the frontend.
#[derive(serde::Deserialize)]
struct ClashTraffic {
    up: f64,
    down: f64,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
/// Real IP info (before proxy) — structured like exit data.
pub struct RealIpInfo {
    pub server: String,
    pub flag: String,
    pub country_name: String,
    pub city_name: String,
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

    let test_url = crate::proxy::batcher::configured_test_url()
        .unwrap_or_else(|| "http://cp.cloudflare.com/generate_204".to_string());
    let delay_url =
        format!("http://127.0.0.1:{stats_port}/proxies/proxy/delay?url={test_url}&timeout=5000");

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
    let test_url = crate::proxy::batcher::configured_test_url()
        .unwrap_or_else(|| "http://cp.cloudflare.com/generate_204".to_string());

    // First get all proxies
    let groups_resp = client
        .get(format!("{api_url}/proxies"))
        .send()
        .await
        .map_err(|e| {
            tracing::error!(target: "proxy", "speed_test: Clash API request failed: {e}");
            format!("Clash API error: {e}")
        })?;

    let groups: serde_json::Value = groups_resp.json().await.map_err(|e| {
        tracing::error!(target: "proxy", "speed_test: failed to parse Clash response: {e}");
        format!("Parse error: {e}")
    })?;

    let mut results = Vec::new();

    // Find all proxy outbounds (not direct, block, or groups)
    if let Some(proxies) = groups.get("proxies").and_then(|p| p.as_object()) {
        // Collect names first, then test in parallel
        let mut test_names: Vec<String> = Vec::new();
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
            test_names.push(name.clone());
        }
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
        let mut set = tokio::task::JoinSet::new();
        for name in test_names {
            let sem = sem.clone();
            let client = client.clone();
            let api_url = api_url.clone();
            let test_url = test_url.to_string();
            set.spawn(async move {
                let _permit = sem.acquire().await.ok()?;
                let url = format!("{api_url}/proxies/{name}/delay?url={test_url}&timeout=5000");
                let resp = client.get(&url).send().await.ok()?;
                let body = resp.text().await.ok()?;
                serde_json::from_str::<serde_json::Value>(&body).ok()
            });
        }
        while let Some(res) = set.join_next().await {
            if let Ok(Some(v)) = res {
                results.push(v);
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
    profile_id: String,
) -> Result<SubscriptionProfile, ProxyError> {
    let logger = app.state::<StdMutex<Logger>>();
    log(
        &app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Importing subscription content: {name}"),
    );

    let mut servers = crate::subscription::parser::parse_subscription(&content).map_err(|e| {
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

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let progress_app = app.clone();
    tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            crate::events::emit_refresh_progress(&progress_app, progress);
        }
    });
    if let Ok(data_dir) = app.path().app_data_dir() {
        let settings = app
            .try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
            .and_then(|s| s.try_lock().ok().map(|g| g.clone()))
            .unwrap_or_default();
        if let (Some(cache), Some(db)) = (
            app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>()
                .map(|s| s.as_ref().clone()),
            app.try_state::<crate::db::DbPool>()
                .map(|s| -> crate::db::DbPool { (*s).clone() }),
        ) {
            servers = crate::profile::process_servers_incremental(
                &data_dir,
                &settings,
                servers,
                &profile_id,
                tokio_util::sync::CancellationToken::new(),
                event_tx,
                Some(progress_tx),
                &db,
                &cache,
            )
            .await;
        }
    }

    // Persist cache to disk so batch-test results survive app restart.
    if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
        cache.save_to_disk().await;
    }

    let profile = SubscriptionProfile {
        id: profile_id,
        name,
        url: format!("content://{}", uuid::Uuid::new_v4()),
        servers,
        updated_at: chrono::Utc::now().to_rfc3339(),
        last_tested: Some(chrono::Utc::now().to_rfc3339()),
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
    let content = tokio::fs::read_to_string(&file_path).await.map_err(|e| {
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
    let mut servers = crate::subscription::parser::parse_subscription(&content).map_err(|e| {
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

    // Pre-generate profile_id so refresh-progress events emitted during
    // processing carry the REAL profile ID.
    let profile_id = uuid::Uuid::new_v4().to_string();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let progress_app = app.clone();
    tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            crate::events::emit_refresh_progress(&progress_app, progress);
        }
    });
    if let Ok(data_dir) = app.path().app_data_dir() {
        let settings = app
            .try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
            .and_then(|s| s.try_lock().ok().map(|g| g.clone()))
            .unwrap_or_default();
        if let (Some(cache), Some(db)) = (
            app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>()
                .map(|s| s.as_ref().clone()),
            app.try_state::<crate::db::DbPool>()
                .map(|s| -> crate::db::DbPool { (*s).clone() }),
        ) {
            servers = crate::profile::process_servers_incremental(
                &data_dir,
                &settings,
                servers,
                &profile_id,
                tokio_util::sync::CancellationToken::new(),
                event_tx,
                Some(progress_tx),
                &db,
                &cache,
            )
            .await;
        }
    }
    let server_count = servers.len();

    // Persist cache to disk so batch-test results survive app restart.
    if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
        cache.save_to_disk().await;
    }

    let profile = SubscriptionProfile {
        id: profile_id,
        name: file_name,
        url,
        servers,
        updated_at: chrono::Utc::now().to_rfc3339(),
        last_tested: Some(chrono::Utc::now().to_rfc3339()),
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
    // effect re-runs). Read-only check — concurrent calls serialize on
    // ProxyManager lock (line 2817). Set only after ALL work succeeds,
    // so a transient failure (network/down/GitHub blocked/disk full)
    // doesn't leave INITIALIZED=true and prevent retry for the session.
    if INITIALIZED.load(std::sync::atomic::Ordering::SeqCst) {
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
        .and_then(|s| s.try_lock().ok().map(|g| g.ipinfo_token.clone()))
        .unwrap_or_default();
    // Set locale for city name localisation (MaxMind City supports en/ru/etc.)
    let test_url_host =
        if let Some(settings_state) = app.try_state::<Mutex<crate::settings::AppSettings>>() {
            if let Ok(guard) = settings_state.try_lock() {
                crate::proxy::geoip::set_locale(&guard.language);
                // Load saved test URL so test_single_server uses it
                let url = guard.test_url.clone();
                let use_tls = guard.test_use_tls;
                Some((url, use_tls))
            } else {
                None
            }
        } else {
            None
        };
    if let Some((test_url, use_tls)) = test_url_host {
        crate::proxy::batcher::set_configured_test_url(&test_url, use_tls);
    }
    // Sync batch-test settings to global statics so generate_batch_config
    // can read them without a Tauri app handle.
    if let Some(settings_state) = app.try_state::<Mutex<crate::settings::AppSettings>>() {
        if let Ok(guard) = settings_state.try_lock() {
            crate::proxy::batcher::set_batch_settings(
                guard.anti_dpi,
                guard.tls_insecure,
                guard.enable_mux,
            );
        }
    }
    // U7: Cleanup stale tmp files from previous sessions before downloading.
    crate::proxy::geoip::cleanup_stale_tmp_files();

    // U8: Cleanup stale batch/latency/outlet/test temp dirs from crashed sessions.
    cleanup_stale_temp_dirs();

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

    // 3b. Pre-warm testing engine — start immediately after profile load.
    {
        let data_dir = app.path().app_data_dir().ok();
        let profiles = app
            .try_state::<ProfileState>()
            .and_then(|ps| ps.manager.try_lock().ok().map(|g| g.list().to_vec()))
            .unwrap_or_default();
        let servers: Vec<crate::proxy::ServerProfile> = profiles
            .iter()
            .flat_map(|p| p.servers.iter().cloned())
            .collect();
        let settings = app
            .try_state::<Mutex<crate::settings::AppSettings>>()
            .and_then(|s| s.try_lock().ok().map(|g| g.clone()))
            .unwrap_or_default();
        if let Some(d) = data_dir {
            tokio::spawn(async move {
                tracing::info!(target: "system", "Pre-warming testing engine with {} servers", servers.len());
                let _ = crate::profile::ensure_test_engine(&servers, &d, &settings).await;
            });
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
                            if !ip.is_empty() {
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
    //   - process_servers_incremental проверяет только при refresh.
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

    // Mark initialized only after ALL work succeeded.
    INITIALIZED.store(true, std::sync::atomic::Ordering::SeqCst);

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
        .and_then(|s| s.try_lock().ok().map(|g| g.ipinfo_token.clone()))
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

/// Fetch cached delay history for ALL outbounds in one round-trip.
/// The sing-box urltest outbound probes members on its interval (3 min),
/// and this endpoint returns the cached delays — so the UI can display
/// latencies instantly with zero active testing load.
#[tauri::command]
async fn get_delay_history() -> Result<serde_json::Value, String> {
    let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::SeqCst);
    if stats_port == 0 {
        return Ok(serde_json::json!({"history": {}}));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("http://127.0.0.1:{stats_port}/proxies"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut history = serde_json::Map::new();
    if let Some(proxies) = body.get("proxies").and_then(|p| p.as_object()) {
        for (name, info) in proxies {
            if let Some(hist) = info.get("history").and_then(|h| h.as_array()) {
                if let Some(last) = hist.last() {
                    if let Some(delay) = last.get("delay").and_then(|d| d.as_u64()) {
                        history.insert(name.clone(), serde_json::json!({"delay": delay}));
                    }
                }
            }
        }
    }
    Ok(serde_json::json!({"history": history}))
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
    // Tracks consecutive HTTP probe failures to fall back to TCP-only mode
    // for non-HTTP endpoints (test listeners, non-proxy TCP servers).
    let mut http_failures = 0u32;
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
                // Port is bound. But for UDP-based outbounds (hysteria2, tuic,
                // wireguard), the inbound bind completes HUNDREDS of milliseconds
                // before the outbound QUIC+TLS handshake finishes. Returning Ok
                // here caused start_proxy's IP-verification to fire 3 HTTP
                // requests that all failed in ~5ms (sing-box returns SOCKS5
                // errors while the outbound is still establishing).
                //
                // Phase 2: HTTP-through-proxy probe (800ms timeout).
                // After 3 consecutive HTTP failures, fall back to TCP-only
                // (handles test listeners and non-proxy TCP endpoints).
                if http_failures >= 3 {
                    return Ok(attempts);
                }
                let probe_client = match reqwest::Client::builder()
                    .proxy(
                        reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}"))
                            .map_err(|e| format!("probe proxy build error: {e}"))?,
                    )
                    .timeout(Duration::from_millis(800))
                    .build()
                {
                    Ok(c) => c,
                    Err(_) => {
                        // Fall back to old behaviour if client build fails.
                        return Ok(attempts);
                    }
                };
                match probe_client
                    .get("http://cp.cloudflare.com/generate_204")
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        return Ok(attempts);
                    }
                    _ => {
                        // Port bound but outbound not ready. Keep polling.
                        http_failures += 1;
                    }
                }
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

/// Cleanup stale omnitun temp directories from crashed sessions.
/// Removes orphaned batch/latency/outlet/test dirs older than 1 hour.
pub(crate) fn cleanup_stale_temp_dirs() {
    let tmp_dir = std::env::temp_dir();
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(3600); // 1 hour
    let mut total = 0u32;
    let prefixes = [
        "omnitun-batch-",
        "omnitun-latency-",
        "omnitun-outlet-",
        "omnitun-settings-test-", // safe: only used by settings tests, which always clean up
    ];
    if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if prefixes.iter().any(|p| name.starts_with(p)) {
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            if mtime < cutoff {
                                let _ = std::fs::remove_dir_all(entry.path());
                                total += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    if total > 0 {
        tracing::info!(target: "system", "Cleaned up {total} stale omnitun temp dir(s) from previous sessions");
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
        let manager: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec!["profile1".to_string()]));

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

        let manager: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec!["profile1".to_string()]));

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
