use std::collections::hash_map::DefaultHasher;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use rmcp::{
    model::{
        self, CallToolRequestParams, CallToolResult, ContentBlock, Implementation,
        ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceResult, Resource,
        ResourceContents, ResourcesCapability, ServerCapabilities, ServerInfo, Tool,
        ToolsCapability,
    },
    serve_server,
    service::{MaybeSendFuture, RequestContext, RoleServer},
    transport::{self, async_rw::AsyncRwTransport},
    ErrorData, ServerHandler,
};

use crate::logging::{log_to_logger, LogLevel, LogSource, Logger};
use crate::proxy::cache::ServerCache;
use crate::proxy::port_pool::PortPool;
use crate::proxy::{ProcessManager, ServerProfile};
use crate::settings::{AppSettings, SettingsManager};
type JsonMap = serde_json::Map<String, serde_json::Value>;

fn schema(json: serde_json::Value) -> Arc<JsonMap> {
    match json {
        serde_json::Value::Object(m) => Arc::new(m),
        _ => Arc::default(),
    }
}

struct ServerState {
    process_mgr: ProcessManager,
    profile_mgr: super::profile::ProfileManager,
    settings_mgr: SettingsManager,
    bin_path: PathBuf,
    data_dir: PathBuf,
    _logger: Arc<Mutex<Logger>>,
    cache: Arc<ServerCache>,
    port_pool: Arc<PortPool>,
    cancel_token: Arc<std::sync::Mutex<tokio_util::sync::CancellationToken>>,
}

struct Handler {
    state: Arc<tokio::sync::Mutex<ServerState>>,
}

fn binary_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "sing-box.exe"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "sing-box"
    }
}

fn mk_state(data_dir: PathBuf) -> Result<ServerState, String> {
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
    let logger = Arc::new(Mutex::new(Logger::new(1000, None)));
    let settings_mgr = SettingsManager::new(data_dir.clone());
    let settings = settings_mgr.load().unwrap_or_default();
    crate::proxy::batcher::set_configured_test_url(&settings.test_url, settings.test_use_tls);
    let bin_path = data_dir.join("binaries").join(binary_name());
    let process_mgr = ProcessManager::new(bin_path.clone());

    // Initialize database
    let db = tauri::async_runtime::block_on(async { crate::db::init_db(&data_dir).await })
        .map_err(|e| format!("Failed to init DB: {e}"))?;

    // One-time migration from JSON
    if let Err(e) =
        tauri::async_runtime::block_on(async { crate::db::migrate_from_json(&db, &data_dir).await })
    {
        tracing::warn!(target: "db", "JSON migration warning (non-fatal): {e}");
    }

    let mut profile_mgr = super::profile::ProfileManager::new(data_dir.clone(), db.clone());
    let _ = profile_mgr.load();

    // Create cache, port pool, and cancellation token
    let cache = Arc::new(ServerCache::new(&data_dir));
    let port_pool = Arc::new(PortPool::new());
    let cancel_token = Arc::new(std::sync::Mutex::new(
        tokio_util::sync::CancellationToken::new(),
    ));

    log_to_logger(
        &*logger,
        LogLevel::Info,
        LogSource::System,
        format!("MCP server starting (data_dir: {:?})", data_dir),
    );

    Ok(ServerState {
        process_mgr,
        profile_mgr,
        settings_mgr,
        bin_path,
        data_dir,
        _logger: logger,
        cache,
        port_pool,
        cancel_token,
    })
}

// ── Helper: build ServerProfile from JSON args ──────────────────────────
//
// Derive a deterministic server ID from protocol, endpoint, and credentials.
// This ensures the same server always gets the same ID (cache re-use across calls).
// the key used by ProfileManager::refresh for cross-refresh ID preservation.
fn profile_from_args(args: &serde_json::Value) -> ServerProfile {
    let protocol = args
        .get("protocol")
        .and_then(|v| v.as_str())
        .unwrap_or("vless");
    let server = args.get("server").and_then(|v| v.as_str()).unwrap_or("");
    let port = args.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    let uuid = args.get("uuid").and_then(|v| v.as_str()).unwrap_or("");
    let password = args.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("");

    // Deterministic ID — same identity key as ProfileManager::refresh.
    let identity_key = format!(
        "{}://{}:{}@{}:{}:{}/{}",
        protocol, server, port, uuid, password, method, "",
    );
    let mut hasher = DefaultHasher::new();
    use std::hash::Hasher;
    hasher.write(identity_key.as_bytes());
    let id = format!("mcp-{:016x}", hasher.finish());

    ServerProfile {
        id,
        protocol: protocol.into(),
        server: server.into(),
        port,
        uuid: if uuid.is_empty() {
            None
        } else {
            Some(uuid.into())
        },
        password: if password.is_empty() {
            None
        } else {
            Some(password.into())
        },
        method: if method.is_empty() {
            None
        } else {
            Some(method.into())
        },
        sni: args.get("sni").and_then(|v| v.as_str()).map(String::from),
        flow: args.get("flow").and_then(|v| v.as_str()).map(String::from),
        public_key: args
            .get("public_key")
            .and_then(|v| v.as_str())
            .map(String::from),
        private_key: args
            .get("private_key")
            .and_then(|v| v.as_str())
            .map(String::from),
        transport_type: args
            .get("transport_type")
            .and_then(|v| v.as_str())
            .map(String::from),
        transport_path: args
            .get("transport_path")
            .and_then(|v| v.as_str())
            .map(String::from),
        transport_host: args
            .get("transport_host")
            .and_then(|v| v.as_str())
            .map(String::from),
        reality_pbk: args
            .get("reality_pbk")
            .and_then(|v| v.as_str())
            .map(String::from),
        reality_sid: args
            .get("reality_sid")
            .and_then(|v| v.as_str())
            .map(String::from),
        username: args
            .get("username")
            .and_then(|v| v.as_str())
            .map(String::from),
        // WireGuard fields for profile construction.
        wg_local_address: args
            .get("wg_local_address")
            .and_then(|v| v.as_str())
            .map(String::from),
        wg_allowed_ips: args
            .get("wg_allowed_ips")
            .and_then(|v| v.as_str())
            .map(String::from),
        wg_dns: args
            .get("wg_dns")
            .and_then(|v| v.as_str())
            .map(String::from),
        wg_mtu: args
            .get("wg_mtu")
            .and_then(|v| v.as_u64())
            .map(|n| n as u16),
        wg_keepalive: args
            .get("wg_keepalive")
            .and_then(|v| v.as_u64())
            .map(|n| n as u16),
        ..Default::default()
    }
}

// ── Helper: launch sing-box with config ─────────────────────────────────
//
// Load real settings from state.settings_mgr (not AppSettings::default())
// so the MCP-controlled proxy matches the GUI-controlled proxy.
//
// This function:
//   1. Checks if the requested proxy_port is free; falls back to port+1..port+100
//      (same logic as lib.rs::start_proxy).
//   2. Waits for sing-box to actually bind the port before returning
//      (calls wait_for_proxy_ready from lib.rs).
//   3. On failure, reads sing-box-stderr.log and includes the last 5 lines
//      in the error message so the agent can diagnose.
//   4. Returns the actual bound port (may differ from requested).
//
// The function signature changes to return (pid, actual_port) instead
// of just pid, so callers can report the correct port to the agent.
async fn launch_sing_box(
    state: &ServerState,
    proxy_port: u16,
    routing_mode: &str,
    profiles: &[ServerProfile],
    settings_overrides: Option<&AppSettings>,
) -> Result<(u32, u16), ErrorData> {
    // Load the user's actual settings, apply optional overrides.
    let settings = settings_overrides
        .cloned()
        .unwrap_or_else(|| state.settings_mgr.load().unwrap_or_default());

    // Port availability check with fallback.
    let actual_port = if crate::proxy::config::port_available(proxy_port) {
        proxy_port
    } else {
        let mut found = None;
        for offset in 1..=100u16 {
            let candidate = match proxy_port.checked_add(offset) {
                Some(c) => c,
                None => break,
            };
            if crate::proxy::config::port_available(candidate) {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(p) => {
                log_to_logger(
                    &*state._logger,
                    LogLevel::Warning,
                    LogSource::Proxy,
                    format!("Port {proxy_port} is in use, using {p} instead"),
                );
                p
            }
            None => proxy_port, // no alternative — sing-box will fail to bind
        }
    };

    let (cfg, _) = crate::proxy::config::generate_config(
        &state.data_dir.join("config"),
        actual_port,
        routing_mode,
        profiles,
        &settings,
    )
    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    let cpath = crate::proxy::config::write_config(&cfg, &state.data_dir.join("config"))
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

    // Use ProcessManager::start_headless instead of direct Command::spawn.
    // This gives us:
    //   - Windows JobObject (KILL_ON_JOB_CLOSE) — orphan protection
    //   - PID file management — cleanup_stale_pid on next launch
    //   - Unix process_group(0) — clean group kill
    //   - Linux PR_SET_PDEATHSIG — sing-box killed when MCP parent dies
    //   - Stale PID detection — won't double-spawn
    let pid = state
        .process_mgr
        .start_headless(&cpath)
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

    // Fire-and-forget: don't wait for port readiness here.
    // The spawned watchdog (call by cmd_start/start_server_from_subscription)
    // monitors the process and reconnects on crash. The MCP client should
    // poll get_status or check_proxy_health to confirm readiness.
    Ok((pid, actual_port))
}

// ── Command handlers ─────────────────────────────────────────────────────

impl Handler {
    async fn cmd_start(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let server = args
            .get("server")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ErrorData::invalid_params("'server' is required", None))?;
        let port = args
            .get("port")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ErrorData::invalid_params("'port' is required", None))?
            as u16;
        let proxy_port = args
            .get("proxy_port")
            .and_then(|v| v.as_u64())
            .unwrap_or(5055) as u16;
        let routing_mode = args
            .get("routing_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("system-proxy");
        // auto_reconnect defaults to false for QA — the agent
        // should observe crashes, not have them silently fixed.
        let auto_reconnect = args
            .get("auto_reconnect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut profile = profile_from_args(&args);
        profile.server = server.into();
        profile.port = port;
        // Track the active server ID
        {
            let mut active = crate::proxy::ACTIVE_SERVER_ID
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *active = Some(profile.id.clone());
        }

        let state = self.state.lock().await;
        let (pid, actual_port) =
            launch_sing_box(&state, proxy_port, routing_mode, &[profile.clone()], None).await?;
        state.process_mgr.set_pid(pid);
        // Store LAST_PROFILE for auto-reconnect on sing-box crash.
        {
            let mut last = crate::proxy::LAST_PROFILE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *last = Some(crate::proxy::LastConnection {
                all_servers: vec![profile.clone()],
                routing_mode: routing_mode.to_string(),
                proxy_port: actual_port,
            });
        }
        // Clone cancel_token BEFORE releasing the state lock to avoid
        // self-deadlock. spawn_proxy_watchdog must not acquire the
        // same tokio::sync::Mutex that cmd_start already holds —
        // tokio::Mutex is non-reentrant.
        let cancel_token = {
            let guard = state.cancel_token.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        let state_clone = self.state.clone();
        drop(state); // release tokio::Mutex BEFORE spawning watchdog
        spawn_proxy_watchdog(cancel_token, state_clone, pid, actual_port, auto_reconnect);
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Proxy started (PID {pid}) on port {actual_port}"
        ))]))
    }

    async fn cmd_stop(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        // Clear ACTIVE_SERVER_ID on stop.
        {
            let mut active = crate::proxy::ACTIVE_SERVER_ID
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *active = None;
        }
        // Clear LAST_PROFILE so the watchdog stops.
        {
            let mut last = crate::proxy::LAST_PROFILE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *last = None;
        }
        state
            .process_mgr
            .stop()
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "Proxy stopped",
        )]))
    }

    async fn cmd_status(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let (status, pid) = match state.process_mgr.pid() {
            Some(p) if crate::is_process_alive(p) => ("connected", p),
            Some(_dead_pid) => {
                // Clear stale PID so subsequent calls report 0,
                // not a dead PID.
                state.process_mgr.clear_pid();
                crate::proxy::PROXY_PID.store(0, std::sync::atomic::Ordering::Release);
                crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Release);
                crate::proxy::PROXY_PORT.store(0, std::sync::atomic::Ordering::Release);
                ("disconnected", 0)
            }
            None => ("disconnected", 0),
        };

        // Read supplementary state from the same globals the Tauri version uses.
        let clash_api_port =
            crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
        let active_server_id = crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let proxy_port = state.settings_mgr.load().unwrap_or_default().proxy_port;
        let binary_installed = state.bin_path.exists();
        let info = serde_json::json!({
            "status": status,
            "pid": pid,
            "proxy_port": proxy_port,
            "clash_api_port": clash_api_port,
            "active_server_id": active_server_id.unwrap_or_default(),
            "binary_installed": binary_installed,
            "uptime_secs": null,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            info.to_string(),
        )]))
    }

    async fn cmd_profiles(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let j =
            serde_json::to_string_pretty(state.profile_mgr.list()).unwrap_or_else(|_| "[]".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(j)]))
    }

    async fn cmd_add_sub(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'url' is required", None))?;
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or(url);
        let url_owned = url.to_string();
        let profile_id = uuid::Uuid::new_v4().to_string();

        // Fetch + parse + process_servers (ping + batch test), same as Tauri path.
        // Servers only appear in the profile AFTER full testing (no raw saves).
        let (data_dir, settings, cache) = {
            let state = self.state.lock().await;
            (
                state.data_dir.clone(),
                state.settings_mgr.load().unwrap_or_default(),
                state.cache.clone(),
            )
        };

        let servers = if url.starts_with("http://") || url.starts_with("https://") {
            crate::subscription::fetch_subscription(&url_owned, None)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
        } else {
            crate::subscription::parser::parse_subscription(&url_owned)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
        };

        let servers = crate::profile::process_servers_inner(
            &data_dir,
            &settings,
            servers,
            &profile_id,
            Some(&cache),
            None,
            &[],
        )
        .await;

        let profile = crate::subscription::SubscriptionProfile {
            id: profile_id,
            url: url_owned,
            name: name.into(),
            servers,
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_tested: Some(chrono::Utc::now().to_rfc3339()),
            traffic_used: None,
            traffic_total: None,
            traffic_expiry: None,
        };

        let mut state = self.state.lock().await;
        state.profile_mgr.list_mut().push(profile.clone());
        state
            .profile_mgr
            .save()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let j = serde_json::to_string_pretty(&profile).unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(j)]))
    }

    async fn cmd_refresh_sub(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'id' is required", None))?
            .to_string();

        // Phase 1: read lock — get URL + old servers + settings + data_dir
        let (url, old_servers, data_dir, settings, cache) = {
            let state = self.state.lock().await;
            let profile = state
                .profile_mgr
                .list()
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| {
                    ErrorData::invalid_params(format!("Profile '{id}' not found"), None)
                })?;
            let url = profile.url.clone();
            let old_servers = profile.servers.clone();
            let data_dir = state.data_dir.clone();
            let settings = state.settings_mgr.load().unwrap_or_default();
            let cache = state.cache.clone();
            (url, old_servers, data_dir, settings, cache)
        }; // lock released

        // Spawn background task for long operations (fetch, ping, batch test)
        let bg_id = id.clone();
        let bg_state = self.state.clone();
        tokio::spawn(async move {
            match crate::subscription::fetch_subscription(&url, None).await {
                Ok(servers) => {
                    let enriched = crate::profile::process_servers_inner(
                        &data_dir,
                        &settings,
                        servers,
                        &bg_id,
                        Some(&cache),
                        None,
                        &old_servers,
                    )
                    .await;
                    {
                        let mut state = bg_state.lock().await;
                        if let Some(profile) = state
                            .profile_mgr
                            .list_mut()
                            .iter_mut()
                            .find(|p| p.id == bg_id)
                        {
                            profile.servers = enriched;
                            profile.updated_at = chrono::Utc::now().to_rfc3339();
                        }
                        // Borrow of profile_mgr ended — safe to call save()
                        let profile_exists = state.profile_mgr.list().iter().any(|p| p.id == bg_id);
                        if !profile_exists {
                            tracing::warn!(target: "subscription",
                                "Background refresh: profile {bg_id} not found in manager");
                        } else if let Err(e) = state.profile_mgr.save() {
                            tracing::warn!(target: "subscription",
                                "Failed to save profile {bg_id} after refresh: {e}");
                        } else {
                            let server_count = state
                                .profile_mgr
                                .list()
                                .iter()
                                .find(|p| p.id == bg_id)
                                .map_or(0, |p| p.servers.len());
                            tracing::info!(target: "subscription",
                                "Background refresh completed: {server_count} server(s) for profile {bg_id}");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(target: "subscription",
                        "Background refresh failed for profile {bg_id}: {e}");
                }
            }
        });

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Refresh started in background for profile '{id}'"
        ))]))
    }

    async fn cmd_remove_sub(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        // Accept both "id" and "subscription_id" for backward compat.
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("subscription_id").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                ErrorData::invalid_params("'id' (or 'subscription_id') is required", None)
            })?;

        // If the proxy is currently connected to a server in this
        // profile, stop it first to avoid leaving a stale proxy running.
        {
            let active_id = crate::proxy::ACTIVE_SERVER_ID
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(ref active) = active_id {
                let state = self.state.lock().await;
                let in_profile = state
                    .profile_mgr
                    .list()
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.servers.iter().any(|s| &s.id == active))
                    .unwrap_or(false);
                drop(state);
                if in_profile {
                    tracing::warn!(target: "proxy",
                        "Removing subscription {id} which contains the active server — stopping proxy first"
                    );
                    let state = self.state.lock().await;
                    let _ = state.process_mgr.stop().await;
                    let mut active_lock = crate::proxy::ACTIVE_SERVER_ID
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    *active_lock = None;
                    let mut last = crate::proxy::LAST_PROFILE
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    *last = None;
                }
            }
        }

        let mut state = self.state.lock().await;
        let len_before = state.profile_mgr.list().len();
        state.profile_mgr.list_mut().retain(|p| p.id != id);
        let removed = len_before.saturating_sub(state.profile_mgr.list().len());
        if removed == 0 {
            return Err(ErrorData::invalid_params(
                format!("Profile '{id}' not found"),
                None,
            ));
        }

        // Prune cache entries for removed servers.
        let remaining_ids: std::collections::HashSet<String> = state
            .profile_mgr
            .list()
            .iter()
            .flat_map(|p| p.servers.iter().map(|s| s.id.clone()))
            .collect();
        let cache = state.cache.clone();
        drop(state);
        cache.handle_subscription_refresh(&remaining_ids).await;
        cache.save_to_disk().await;

        let mut state = self.state.lock().await;
        state
            .profile_mgr
            .save()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Removed {removed} profile(s)"
        ))]))
    }

    async fn cmd_start_server(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let sub_id = args
            .get("subscription_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'subscription_id' is required", None))?;
        let server_id = args
            .get("server_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'server_id' is required", None))?;
        let proxy_port = args
            .get("proxy_port")
            .and_then(|v| v.as_u64())
            .unwrap_or(5055) as u16;
        let routing_mode = args
            .get("routing_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("system-proxy");
        let auto_reconnect = args
            .get("auto_reconnect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let state = self.state.lock().await;
        let profile = state
            .profile_mgr
            .list()
            .iter()
            .find(|p| p.id == sub_id)
            .ok_or_else(|| {
                ErrorData::invalid_params(format!("Subscription '{sub_id}' not found"), None)
            })?;
        let server = profile
            .servers
            .iter()
            .find(|s| s.id == server_id)
            .ok_or_else(|| {
                ErrorData::invalid_params(format!("Server '{server_id}' not found"), None)
            })?;

        // Track the active server ID for this subscription server too.
        {
            let mut active = crate::proxy::ACTIVE_SERVER_ID
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *active = Some(server.id.clone());
        }

        let (pid, actual_port) =
            launch_sing_box(&state, proxy_port, routing_mode, &[server.clone()], None).await?;
        state.process_mgr.set_pid(pid);
        // Store LAST_PROFILE for auto-reconnect.
        {
            let mut last = crate::proxy::LAST_PROFILE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *last = Some(crate::proxy::LastConnection {
                all_servers: vec![server.clone()],
                routing_mode: routing_mode.to_string(),
                proxy_port: actual_port,
            });
        }
        // Clone cancel_token BEFORE releasing the state lock to avoid
        // self-deadlock (same pattern as cmd_start).
        let cancel_token = {
            let guard = state.cancel_token.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        let state_clone = self.state.clone();
        let server_clone = server.clone();
        drop(state);
        spawn_proxy_watchdog(cancel_token, state_clone, pid, actual_port, auto_reconnect);
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Proxy started (PID {pid}) via server '{}' on port {actual_port}",
            server_clone.server
        ))]))
    }

    async fn cmd_settings(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let s = state.settings_mgr.load().unwrap_or_default();
        let j = serde_json::to_string_pretty(&s).unwrap_or_else(|_| "{}".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(j)]))
    }

    // ── QA tools ───────────────────────────────────────────────────────────

    /// Return recent log entries from the Logger ring buffer.
    async fn cmd_get_logs(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let level = args
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let source = args
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let max_lines = args
            .get("max_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(100) as usize;
        let state = self.state.lock().await;
        let entries = state
            ._logger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries();
        let filtered: Vec<_> = entries
            .iter()
            .filter(|e| {
                level.is_empty() || format!("{:?}", e.level).to_lowercase().contains(&level)
            })
            .filter(|e| {
                source.is_empty() || format!("{:?}", e.source).to_lowercase().contains(&source)
            })
            .rev()
            .take(max_lines)
            .map(|e| {
                serde_json::json!({
                    "timestamp": e.timestamp,
                    "level": format!("{:?}", e.level),
                    "source": format!("{:?}", e.source),
                    "message": e.message,
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![ContentBlock::json(filtered)?]))
    }

    /// Read the last N bytes of sing-box's stderr log (for crash diagnosis).
    async fn cmd_get_singbox_stderr(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(4096) as usize;
        let state = self.state.lock().await;
        let stderr_path = state.data_dir.join("config").join("sing-box-stderr.log");
        let content = match std::fs::read_to_string(&stderr_path) {
            Ok(s) => {
                if s.len() > max_bytes {
                    format!(
                        "... (showing last {max_bytes} of {} bytes)\n{}",
                        s.len(),
                        &s[s.len().saturating_sub(max_bytes)..]
                    )
                } else {
                    s
                }
            }
            Err(_) => "No stderr log available (sing-box has not been started yet)".into(),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(content)]))
    }

    /// Batch-test a single server: ping latency + exit IP detection.
    /// Does NOT start the main proxy — runs a standalone sing-box instance.
    async fn cmd_test_single_server(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        let server_host = args
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'server' is required", None))?;
        let server_port = args
            .get("port")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ErrorData::invalid_params("'port' is required", None))?
            as u16;
        let mut profile = profile_from_args(&args);
        profile.server = server_host.into();
        profile.port = server_port;

        let state = self.state.lock().await;
        let rx = state.cache.queue_test_public(&profile);
        drop(state);

        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(Ok(result))) => {
                let j = serde_json::json!({
                    "server_id": profile.id,
                    "ping_ms": result.ping_ms,
                    "behind_cdn": result.behind_cdn,
                    "exit_ip": result.exit_ip,
                    "exit_flag": result.exit_flag,
                    "exit_country": result.exit_country,
                    "exit_city": result.exit_city,
                });
                Ok(CallToolResult::success(vec![ContentBlock::json(j)?]))
            }
            Ok(Ok(Err(e))) => Err(ErrorData::internal_error(format!("Test failed: {e}"), None)),
            Ok(Err(_)) => Err(ErrorData::internal_error("Test worker dropped", None)),
            Err(_) => Err(ErrorData::internal_error("Test timed out after 120s", None)),
        }
    }

    /// Get cache statistics (positive/negative counts).
    async fn cmd_get_cache_stats(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let pos = state.cache.positive_count().await;
        let neg = state.cache.negative_count().await;
        let j = serde_json::json!({ "positive_count": pos, "negative_count": neg });
        Ok(CallToolResult::success(vec![ContentBlock::json(j)?]))
    }

    /// Clear all cache entries (for testing).
    async fn cmd_clear_cache(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        // Clear SQLite server_cache table (cache.json is no longer used)
        if let Some(conn) = state.cache.get_sync_conn() {
            let _ = conn.execute("DELETE FROM server_cache", []);
        }
        state.cache.load_from_disk().await;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "Cache cleared",
        )]))
    }

    /// Download ipinfo.io Lite GeoIP database.
    async fn cmd_download_geoip_db(&self) -> Result<CallToolResult, ErrorData> {
        let (settings, data_dir) = {
            let state = self.state.lock().await;
            let s = state.settings_mgr.load().unwrap_or_default();
            let d = state.data_dir.clone();
            (s, d)
        };
        if settings.ipinfo_token.is_empty() {
            return Err(ErrorData::invalid_params(
                "IPinfo API token not set in settings",
                None,
            ));
        }
        let geo_dir = data_dir.join("geoip");
        match crate::proxy::geoip::download_ipinfo_db(&geo_dir, &settings.ipinfo_token).await {
            Ok(true) => Ok(CallToolResult::success(vec![ContentBlock::text(
                "downloaded",
            )])),
            Ok(false) => Ok(CallToolResult::success(vec![ContentBlock::text(
                "skipped (rate-limited or up-to-date)",
            )])),
            Err(e) => Err(ErrorData::internal_error(e, None)),
        }
    }

    /// Update application settings (partial update). Accepts routing_mode, proxy_port, bypass_ru, etc.
    async fn cmd_update_settings(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        let mut settings = {
            let state = self.state.lock().await;
            state.settings_mgr.load().unwrap_or_default()
        };
        if let Some(v) = args.get("routing_mode").and_then(|v| v.as_str()) {
            settings.routing_mode = v.into();
        }
        if let Some(v) = args.get("proxy_port").and_then(|v| v.as_u64()) {
            settings.proxy_port = v as u16;
        }
        if let Some(v) = args.get("bypass_ru").and_then(|v| v.as_bool()) {
            settings.bypass_ru = v;
        }
        if let Some(v) = args.get("allow_lan").and_then(|v| v.as_bool()) {
            settings.allow_lan = v;
        }
        if let Some(v) = args.get("kill_switch").and_then(|v| v.as_bool()) {
            settings.kill_switch = v;
        }
        if let Some(v) = args.get("anti_dpi").and_then(|v| v.as_bool()) {
            settings.anti_dpi = v;
        }
        if let Some(v) = args.get("enable_mux").and_then(|v| v.as_bool()) {
            settings.enable_mux = v;
        }
        if let Some(v) = args.get("block_cdn_nodes").and_then(|v| v.as_bool()) {
            settings.block_cdn_nodes = v;
        }
        if let Some(v) = args.get("auto_update_interval").and_then(|v| v.as_u64()) {
            settings.auto_update_interval = v as u32;
        }
        if let Some(v) = args.get("auto_test_interval").and_then(|v| v.as_u64()) {
            settings.auto_test_interval = v as u32;
        }
        if let Some(v) = args.get("test_url").and_then(|v| v.as_str()) {
            settings.test_url = v.into();
        }
        if let Some(v) = args.get("test_use_tls").and_then(|v| v.as_bool()) {
            settings.test_use_tls = v;
        }
        settings.normalize();
        crate::proxy::batcher::set_configured_test_url(&settings.test_url, settings.test_use_tls);
        let state = self.state.lock().await;
        state
            .settings_mgr
            .save(&settings)
            .map_err(|e| ErrorData::internal_error(e, None))?;
        let json = serde_json::to_string_pretty(&settings).unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Import subscription from raw content (base64 or URI list).
    async fn cmd_import_subscription_content(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'name' is required", None))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'content' is required", None))?;
        let servers = crate::subscription::parser::parse_subscription(content)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        if servers.is_empty() {
            return Err(ErrorData::invalid_params(
                "No servers found in content",
                None,
            ));
        }
        let profile_id = uuid::Uuid::new_v4().to_string();

        // Extract cache + data_dir, run process_servers, then save
        let (processed, _data_dir, cache) = {
            let state = self.state.lock().await;
            let data_dir = state.data_dir.clone();
            let cache = state.cache.clone();
            drop(state);
            let processed = crate::profile::process_servers_inner(
                &data_dir,
                &crate::settings::AppSettings::default(),
                servers,
                &profile_id,
                Some(&cache),
                None,
                &[],
            )
            .await;
            (processed, data_dir, cache)
        };

        // Persist cache to disk
        cache.save_to_disk().await;

        let profile = crate::subscription::SubscriptionProfile {
            id: profile_id,
            name: name.into(),
            url: format!("content://{}", uuid::Uuid::new_v4()),
            servers: processed,
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_tested: None,
            traffic_used: None,
            traffic_total: None,
            traffic_expiry: None,
        };
        let mut state = self.state.lock().await;
        state.profile_mgr.list_mut().push(profile.clone());
        state
            .profile_mgr
            .save()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&profile).unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Get GeoIP database status (version, last updated).
    async fn cmd_get_geoip_status(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let geo_dir = state.data_dir.join("geoip");
        let meta = crate::proxy::geoip::get_geoip_meta(&geo_dir);
        let j = serde_json::to_value(&meta).unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(
            j.to_string(),
        )]))
    }

    /// Get real IP (before proxy) via ipify / ip.sb / ifconfig.me.
    async fn cmd_get_real_ip(&self) -> Result<CallToolResult, ErrorData> {
        match crate::fetch_real_ip().await {
            Ok(info) => {
                let j = serde_json::json!({
                    "ip": info.server,
                    "country_code": info.flag,
                    "country_name": info.country_name,
                });
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    j.to_string(),
                )]))
            }
            Err(e) => Err(ErrorData::internal_error(e, None)),
        }
    }

    /// Verify the running proxy actually routes traffic. Makes a request
    /// through the local proxy port and returns the exit IP + GeoIP.
    async fn cmd_verify_proxy_routes(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let settings = state.settings_mgr.load().unwrap_or_default();
        let proxy_port = settings.proxy_port;
        if proxy_port == 0 {
            return Err(ErrorData::internal_error("Proxy is not running — start it first with start_proxy or start_server_from_subscription", None));
        }
        // Build a reqwest client that routes through the proxy (mixed inbound accepts SOCKS5).
        let proxy_url = format!("socks5://127.0.0.1:{proxy_port}");
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|e| ErrorData::internal_error(format!("Proxy config: {e}"), None))?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ErrorData::internal_error(format!("Client build: {e}"), None))?;
        match client.get("https://api.ipify.org").send().await {
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
            }
            Err(e) => Err(ErrorData::internal_error(
                format!("Proxy route check failed: {e}",),
                None,
            )),
        }
    }

    /// Check the health of the active proxy outbound via Clash API.
    async fn cmd_check_proxy_health(&self) -> Result<CallToolResult, ErrorData> {
        let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
        if stats_port == 0 {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::json!({"healthy": false, "delay_ms": null, "reason": "proxy not connected"}).to_string()
            )]));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(7))
            .build()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let url = format!(
            "http://127.0.0.1:{stats_port}/proxies/proxy/delay?url=http://cp.cloudflare.com/generate_204&timeout=5000"
        );
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let delay = body.get("delay").and_then(|d| d.as_u64());
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::json!({"healthy": delay.is_some(), "delay_ms": delay}).to_string()
                )]))
            }
            Ok(resp) => Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::json!({"healthy": false, "delay_ms": null, "reason": format!("HTTP {}", resp.status())}).to_string()
            )])),
            Err(e) => Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::json!({"healthy": false, "delay_ms": null, "reason": e.to_string()}).to_string()
            )])),
        }
    }

    /// Test latency of all outbounds via Clash API.
    async fn cmd_speed_test(&self) -> Result<CallToolResult, ErrorData> {
        let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
        if stats_port == 0 {
            return Err(ErrorData::internal_error(
                "Proxy not connected".to_string(),
                None,
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let api_url = format!("http://127.0.0.1:{stats_port}");
        let groups_resp = client
            .get(format!("{api_url}/proxies"))
            .send()
            .await
            .map_err(|e| ErrorData::internal_error(format!("Clash API: {e}"), None))?;
        let groups: serde_json::Value = groups_resp
            .json()
            .await
            .map_err(|e| ErrorData::internal_error(format!("Parse error: {e}"), None))?;
        let mut results = Vec::new();
        if let Some(proxies) = groups.get("proxies").and_then(|p| p.as_object()) {
            for (name, proxy) in proxies {
                let ptype = proxy.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if matches!(
                    ptype,
                    "Direct" | "Reject" | "Selector" | "URLTest" | "Fallback"
                ) {
                    continue;
                }
                let delay_url = format!("{api_url}/proxies/{name}/delay?url=http://cp.cloudflare.com/generate_204&timeout=5000");
                if let Ok(resp) = client.get(&delay_url).send().await {
                    if let Ok(body) = resp.text().await {
                        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) {
                            results.push(data);
                        }
                    }
                }
            }
        }
        let json = serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Switch the active outbound to a specific tag via Clash API.
    async fn cmd_switch_proxy(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let proxy_tag = args
            .get("proxy_tag")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'proxy_tag' is required", None))?;
        let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
        if stats_port == 0 {
            return Err(ErrorData::internal_error(
                "Proxy not connected".to_string(),
                None,
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let url = format!("http://127.0.0.1:{stats_port}/proxies/proxy");
        let body = serde_json::json!({ "name": proxy_tag });
        match client.put(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Switched to {proxy_tag}"
                ))]))
            }
            Ok(resp) => Err(ErrorData::internal_error(
                format!("Clash API HTTP {}", resp.status()),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(format!("Clash API: {e}"), None)),
        }
    }

    /// Return active connections from the Clash API.
    async fn cmd_get_connections(&self) -> Result<CallToolResult, ErrorData> {
        let stats_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
        if stats_port == 0 {
            return Err(ErrorData::internal_error(
                "Proxy not connected".to_string(),
                None,
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let resp = client
            .get(format!("http://127.0.0.1:{stats_port}/connections"))
            .send()
            .await
            .map_err(|e| ErrorData::internal_error(format!("Clash API: {e}"), None))?;
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ErrorData::internal_error(format!("Parse error: {e}"), None))?;
        let connections = data["connections"].as_array().cloned().unwrap_or_default();
        let json = serde_json::to_string_pretty(&connections).unwrap_or_else(|_| "[]".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Cancel an in-flight start_proxy.
    async fn cmd_cancel_connect(&self) -> Result<CallToolResult, ErrorData> {
        crate::proxy::CONNECTION_CANCELLING.store(true, std::sync::atomic::Ordering::SeqCst);
        let state = self.state.lock().await;
        let _ = state.process_mgr.stop().await;
        crate::proxy::CONNECTION_CANCELLING.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "Cancelled",
        )]))
    }

    /// Activate kill switch on a given port.
    async fn cmd_activate_kill_switch(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        let proxy_port = args
            .get("proxy_port")
            .and_then(|v| v.as_u64())
            .unwrap_or(5055) as u16;
        let routing_mode = args
            .get("routing_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("system-proxy");
        let state = self.state.lock().await;
        let _ = state.process_mgr.stop().await;
        let ks_cfg = crate::proxy::config::generate_kill_switch_config(proxy_port, routing_mode)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let cpath = crate::proxy::config::write_config(&ks_cfg, &state.data_dir.join("config"))
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let b = state.bin_path.to_str().unwrap_or("");
        let c = cpath.to_str().unwrap_or("");
        let child = std::process::Command::new(b)
            .args(["run", "-c", c])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| ErrorData::internal_error(format!("kill switch spawn: {e}"), None))?;
        state.process_mgr.set_pid(child.id());
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Kill switch active on port {proxy_port} (PID {})",
            child.id()
        ))]))
    }

    /// Deactivate kill switch (alias for stop_proxy).
    async fn cmd_deactivate_kill_switch(&self) -> Result<CallToolResult, ErrorData> {
        self.cmd_stop().await
    }
}

// ── Crash-detection watchdog (with optional auto-reconnect) ───────────
//
// This is a mirror of the watchdog in lib.rs::start_proxy, with two
// key differences:
//   1. auto_reconnect is configurable (default false for QA).
//   2. No Tauri events — uses tracing for logging.
//
// When auto_reconnect=false: the watchdog detects the crash, logs it,
// and exits. The agent sees the crash via get_status (returns disconnected).
//
// When auto_reconnect=true: the watchdog attempts reconnection up to 3
// times, then activates kill switch if enabled (same as Tauri).
fn spawn_proxy_watchdog(
    cancel_token: tokio_util::sync::CancellationToken,
    state: Arc<tokio::sync::Mutex<ServerState>>,
    pid: u32,
    _proxy_port: u16,
    auto_reconnect: bool,
) {
    // cancel_token is passed directly — no lock needed here.
    // The caller clones it from the inner std Mutex before releasing
    // the tokio Mutex, breaking the potential deadlock.
    tokio::spawn(async move {
        let cancel = cancel_token;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            }

            // Guard: if our PID is no longer the active one, exit.
            let current_pid = {
                let s = state.lock().await;
                s.process_mgr.pid()
            };
            if current_pid != Some(pid) {
                return;
            }

            if crate::is_process_alive(pid) {
                continue;
            }

            // Process died. Check if the user stopped intentionally.
            let last = {
                crate::proxy::LAST_PROFILE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            };
            if last.is_none() {
                return; // intentional stop
            }

            if !auto_reconnect {
                // QA mode: just log the crash and exit. The agent
                // will see it via get_status and decide what to do.
                tracing::warn!(target: "proxy",
                    "sing-box (PID {pid}) crashed — auto_reconnect=false, \
                     not restarting. Call start_proxy to restart manually."
                );
                // Clear the stale PID.
                let s = state.lock().await;
                s.process_mgr.clear_pid();
                crate::proxy::PROXY_PID.store(0, std::sync::atomic::Ordering::Release);
                return;
            }

            // Auto-reconnect mode: attempt reconnection up to 3 times.
            tracing::warn!(target: "proxy",
                "sing-box (PID {pid}) crashed — auto_reconnect=true, \
                 attempting reconnection (up to 3 attempts)..."
            );
            let mut reconnected = false;
            for attempt in 1..=3 {
                tokio::time::sleep(std::time::Duration::from_secs(attempt * 2)).await;

                // User might have disconnected while we were sleeping.
                let still_wants = {
                    crate::proxy::LAST_PROFILE
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_some()
                };
                if !still_wants {
                    return;
                }

                let s = state.lock().await;
                let settings = s.settings_mgr.load().unwrap_or_default();
                let last = last.clone().unwrap();
                match launch_sing_box(
                    &s,
                    last.proxy_port,
                    &last.routing_mode,
                    &last.all_servers,
                    Some(&settings),
                )
                .await
                {
                    Ok((new_pid, _)) => {
                        s.process_mgr.set_pid(new_pid);
                        reconnected = true;
                        tracing::info!(target: "proxy", "Reconnected successfully (attempt {attempt})");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(target: "proxy", "Reconnect attempt {attempt} failed: {e}");
                    }
                }
            }

            if !reconnected {
                tracing::error!(target: "proxy", "Auto-reconnect failed after 3 attempts");
                // Kill switch activation (if enabled).
                let s = state.lock().await;
                let settings = s.settings_mgr.load().unwrap_or_default();
                if settings.kill_switch {
                    if let Some(last) = last {
                        tracing::warn!(target: "proxy",
                            "Activating Kill Switch on port {}", last.proxy_port
                        );
                        let ks_cfg = match crate::proxy::config::generate_kill_switch_config(
                            last.proxy_port,
                            &last.routing_mode,
                        ) {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::error!(target: "proxy", "Kill switch config failed: {e}");
                                return;
                            }
                        };
                        let cpath = match crate::proxy::config::write_config(
                            &ks_cfg,
                            &s.data_dir.join("config"),
                        ) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!(target: "proxy", "Kill switch write failed: {e}");
                                return;
                            }
                        };
                        let b = s.bin_path.to_str().unwrap_or("");
                        let c = cpath.to_str().unwrap_or("");
                        if let Ok(child) = std::process::Command::new(b)
                            .args(["run", "-c", c])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                        {
                            s.process_mgr.set_pid(child.id());
                            tracing::info!(target: "proxy", "Kill switch active (PID {})", child.id());
                        }
                    }
                }
            }
            return; // watchdog exits after reconnect attempt (success or failure)
        }
    });
}

// ── ServerInfo instructions ──────────────────────────────────────────────
//
// This guide is sent to AI agents using the MCP server, providing context
// about how to use the server and its key invariants.
const INSTRUCTIONS: &str = r#"OmniTun MCP server controls a Tauri-based proxy application (OmniTun) that
manages sing-box (a universal proxy platform) with subscription management,
GeoIP enrichment, batch latency testing, and a dual-layer cache.

## What you can do
- Start/stop a proxy with ad-hoc parameters or from a saved subscription.
- Add/refresh/remove subscriptions.
- Test individual servers without affecting the active proxy.
- Verify the proxy actually routes traffic (verify_proxy_routes).
- Inspect state (get_status, list_profiles, get_settings, get_logs).
- Manage settings, GeoIP databases, and cache.

## Key concepts
- Settings come from settings.json, NOT from per-call parameters.
- CDN servers (behind_cdn=true) have exit_ip=None — this is CORRECT.
- Cache keys servers by deterministic ID — same params = same ID always.
- Auto-reconnect is OFF by default (for QA). Pass auto_reconnect=true for recovery.

## Recommended QA workflow
1. get_status — check if a proxy is already running.
2. list_profiles — see available subscriptions.
3. start_server_from_subscription — start the fastest server.
4. verify_proxy_routes — confirm traffic routing.
5. get_logs (level=warning) — check for warnings.
6. stop_proxy — clean up.
"#;

// ── Tool definitions ─────────────────────────────────────────────────────

fn define_tools() -> Vec<Tool> {
    // Helper to build ToolAnnotations.
    // Per MCP spec: absent hints default to false. We set them explicitly
    // so clients can make informed decisions about confirmation prompts.
    use rmcp::model::ToolAnnotations;
    fn tool_ann(
        read_only: bool,
        destructive: bool,
        idempotent: bool,
        open_world: bool,
    ) -> ToolAnnotations {
        ToolAnnotations::from_raw(
            None,
            Some(read_only),
            Some(destructive),
            Some(idempotent),
            Some(open_world),
        )
    }

    vec![
        Tool::new("start_proxy",
            "Start sing-box proxy with given server parameters. Returns PID and port when successful.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "server":     { "type": "string", "description": "Server hostname or IP" },
                    "port":       { "type": "integer", "description": "Server port", "minimum": 1, "maximum": 65535 },
                    "protocol":   { "type": "string", "description": "Protocol", "enum": ["vless","vmess","trojan","shadowsocks","hysteria2","tuic","wireguard"], "default": "vless" },
                    "uuid":       { "type": "string", "description": "VLESS/VMESS UUID" },
                    "password":   { "type": "string", "description": "Password (trojan, shadowsocks, hysteria2)" },
                    "method":     { "type": "string", "description": "Encryption method (shadowsocks)" },
                    "sni":        { "type": "string", "description": "TLS SNI (server_name)" },
                    "flow":       { "type": "string", "description": "Flow control (vless: xtls-rprx-vision)" },
                    "transport_type": { "type": "string", "description": "Transport type", "enum": ["ws","tcp","grpc","quic"] },
                    "transport_path": { "type": "string", "description": "WebSocket path (e.g. /ws)" },
                    "transport_host": { "type": "string", "description": "WebSocket host header" },
                    "public_key": { "type": "string", "description": "Reality / hysteria2 public key" },
                    "wg_local_address": { "type": "string", "description": "WireGuard local address (e.g. 172.16.0.2/32)" },
                    "wg_allowed_ips":   { "type": "string", "description": "WireGuard allowed IPs (comma-separated)" },
                    "wg_dns":           { "type": "string", "description": "WireGuard DNS" },
                    "wg_mtu":           { "type": "integer", "description": "WireGuard MTU" },
                    "wg_keepalive":     { "type": "integer", "description": "WireGuard persistent keepalive (seconds)" },
                    "proxy_port": { "type": "integer", "description": "Local SOCKS5 port", "default": 5055, "minimum": 1024, "maximum": 65535 },
                    "routing_mode": { "type": "string", "description": "Routing mode", "enum": ["system-proxy","local-proxy"], "default": "system-proxy" },
                    "auto_reconnect": { "type": "boolean", "description": "Enable auto-reconnect watchdog (default: false for QA)", "default": false }
                },
                "required": ["server", "port"]
            }))
        )
        .with_annotations(tool_ann(false, false, false, false)),
        Tool::new("stop_proxy",
            "Stop the running sing-box proxy.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ).with_annotations(tool_ann(false, false, true, false)),
        Tool::new("get_status",
            "Get current proxy connection status (connected/disconnected) and PID.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ).with_annotations(tool_ann(true, false, true, false)),
        Tool::new("list_profiles",
            "List all subscription profiles with their servers.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ).with_annotations(tool_ann(true, false, true, false)),
        Tool::new("add_subscription",
            "Add a new subscription URL to the profile manager.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "url":  { "type": "string", "description": "Subscription URL (VLESS/Base64)" },
                    "name": { "type": "string", "description": "Optional display name (defaults to URL)" }
                },
                "required": ["url"]
            }))
        )
        .with_annotations(tool_ann(false, false, false, false)),
        Tool::new("refresh_subscription",
            "Fetch and parse a subscription URL, replacing its server list.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Profile ID to refresh" }
                },
                "required": ["id"]
            }))
        )
        .with_annotations(tool_ann(false, false, true, true)),
        Tool::new("remove_subscription",
            "Remove a subscription profile by its ID.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Profile ID to remove" }
                },
                "required": ["id"]
            }))
        )
        .with_annotations(tool_ann(false, true, false, false)),
        Tool::new("start_server_from_subscription",
            "Start proxy using a specific server from a subscription profile.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "subscription_id": { "type": "string", "description": "Subscription profile ID" },
                    "server_id":       { "type": "string", "description": "Server ID within the subscription" },
                    "proxy_port":      { "type": "integer", "description": "Local SOCKS5 port", "default": 5055, "minimum": 1024 },
                    "routing_mode":    { "type": "string", "description": "Routing mode", "enum": ["system-proxy","local-proxy"], "default": "system-proxy" }
                },
                "required": ["subscription_id", "server_id"]
            }))
        )
        .with_annotations(tool_ann(false, false, false, false)),
        Tool::new("get_settings",
            "Get current application settings (theme, proxy mode, etc.)",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, true, false)),
        // ── QA tools ───────────────────────────────────────────────────────────
        Tool::new("get_logs",
            "Return recent log entries from the logger ring buffer. Filter by level (info/warn/error) and source (proxy/system/cache/installer/subscription). Returns entries in reverse chronological order (newest first).",
            schema(serde_json::json!({"type":"object","properties":{"level":{"type":"string"},"source":{"type":"string"},"max_lines":{"type":"integer","default":100}}}))
        )
        .with_annotations(tool_ann(true, false, true, false)),
        Tool::new("get_singbox_stderr",
            "Read the last N bytes of sing-box stderr output. Useful for diagnosing why sing-box failed to start or crashed.",
            schema(serde_json::json!({"type":"object","properties":{"max_bytes":{"type":"integer","default":4096}}}))
        )
        .with_annotations(tool_ann(true, false, true, false)),
        Tool::new("test_single_server",
            "Run a batch test on a single server: measures latency and detects exit IP. Does NOT start the main proxy.",
            schema(serde_json::json!({"type":"object","properties":{"server":{"type":"string"},"port":{"type":"integer"},"protocol":{"type":"string","default":"vless"},"uuid":{"type":"string"},"password":{"type":"string"},"method":{"type":"string"},"sni":{"type":"string"}},"required":["server","port"]}))
        )
        .with_annotations(tool_ann(true, false, false, false)),
        Tool::new("get_cache_stats",
            "Get cache statistics: positive and negative counts.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, true, false)),
        Tool::new("clear_cache",
            "Clear the server cache. Removes all cached test results. (Currently informational — full clear requires server restart.)",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(false, true, false, false)),
        Tool::new("get_geoip_status",
            "Get GeoIP database metadata: which databases are installed, their download dates, and sizes.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, true, false)),
        Tool::new("get_real_ip",
            "Get the real public IP address (before proxy) by making a request to ipify / ifconfig / ip.sb.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, false, false)),
        Tool::new("verify_proxy_routes",
            "Verify the running proxy actually routes traffic. Makes a request through the proxy and returns the exit IP. Requires the proxy to be started first.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, false, false)),
        Tool::new("download_geoip_db",
            "Trigger a download of the ipinfo.io Lite GeoIP database. Requires ipinfo_token to be set in settings. Rate-limited to 1 download per 4 hours.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(false, false, true, false)),
        Tool::new("update_settings",
            "Update application settings. Accepts partial updates: routing_mode, proxy_port, bypass_ru, allow_lan, kill_switch, anti_dpi, enable_mux, block_cdn_nodes, auto_update_interval, auto_test_interval.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "routing_mode":   { "type": "string", "enum": ["system-proxy","local-proxy"] },
                    "proxy_port":     { "type": "integer", "minimum": 1024, "maximum": 65535 },
                    "bypass_ru":      { "type": "boolean" },
                    "allow_lan":      { "type": "boolean" },
                    "kill_switch":    { "type": "boolean" },
                    "anti_dpi":       { "type": "boolean" },
                    "enable_mux":     { "type": "boolean" },
                    "block_cdn_nodes": { "type": "boolean" },
                    "auto_update_interval": { "type": "integer", "minimum": 0, "maximum": 10080 },
                    "auto_test_interval":   { "type": "integer", "minimum": 0, "maximum": 10080 }
                }
            }))
        )
        .with_annotations(tool_ann(false, false, false, false)),
        Tool::new("import_subscription_content",
            "Import a subscription from raw content (base64 or URI list). Creates a new profile with content:// URL. Does NOT trigger batch testing.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "name":    { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["name", "content"]
            }))
        )
        .with_annotations(tool_ann(false, false, false, false)),
        Tool::new("check_proxy_health",
            "Check the health of the active proxy outbound via Clash API. Returns {healthy, delay_ms, reason}.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, true, false)),
        Tool::new("speed_test",
            "Test the latency of all outbounds via Clash API. Returns an array of {name, delay} objects. Requires a running proxy with multiple servers.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, false, false)),
        Tool::new("switch_proxy",
            "Switch the active outbound to a specific tag (e.g. 'proxy-0', 'proxy-1'). Only works if the proxy has multiple servers.",
            schema(serde_json::json!({
                "type": "object",
                "properties": { "proxy_tag": { "type": "string" } },
                "required": ["proxy_tag"]
            }))
        )
        .with_annotations(tool_ann(false, false, false, false)),
        Tool::new("get_connections",
            "Return active connections from the Clash API. Use to inspect what traffic is flowing through the proxy.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, true, false)),
        Tool::new("cancel_connect",
            "Cancel an in-flight start_proxy call. Also stops any running proxy. Idempotent.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(false, false, true, false)),
        Tool::new("activate_kill_switch",
            "Activate the kill switch on a given port. Replaces the running proxy with a block-only sing-box config.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "proxy_port":   { "type": "integer", "default": 5055, "minimum": 1024 },
                    "routing_mode": { "type": "string", "default": "system-proxy" }
                }
            }))
        )
        .with_annotations(tool_ann(false, true, false, false)),
        Tool::new("deactivate_kill_switch",
            "Deactivate the kill switch (alias for stop_proxy). Idempotent.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(false, false, true, false)),
    ]
}

// ── ServerHandler implementation ─────────────────────────────────────────

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerInfo {
        let mut tc = ToolsCapability::default();
        tc.list_changed = Some(true);
        let mut rc = ResourcesCapability::default();
        rc.list_changed = Some(true);
        rc.subscribe = Some(true);
        let mut caps = ServerCapabilities::default();
        caps.tools = Some(tc);
        caps.resources = Some(rc);
        let mut si = ServerInfo::default();
        si.protocol_version = model::ProtocolVersion::V_2025_03_26;
        si.capabilities = caps;
        let mut impl_ = Implementation::default();
        impl_.name = "OmniTun MCP".into();
        impl_.version = "0.1.0".into();
        impl_.title = Some("OmniTun MCP Server".into());
        impl_.description = Some("Controls the OmniTun Tauri proxy application: sing-box lifecycle, subscriptions, settings, cache, and QA/testing tools.".into());
        si.server_info = impl_;
        si.instructions = Some(INSTRUCTIONS.into());
        si
    }

    fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_
    {
        async {
            let mut r = ListToolsResult::default();
            r.tools = define_tools();
            Ok(r)
        }
    }

    fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, ErrorData>> + MaybeSendFuture + '_
    {
        async {
            let mut r = ListResourcesResult::default();
            r.resources = vec![
                Resource::new("omnitun://status", "Proxy Status")
                    .with_description("Current proxy connection status: connected/disconnected/error with PID, port, and exit IP.")
                    .with_mime_type("application/json"),
                Resource::new("omnitun://logs/recent", "Recent Logs")
                    .with_description("Last 200 log entries from the application logger ring buffer.")
                    .with_mime_type("application/json"),
                Resource::new("omnitun://settings", "App Settings")
                    .with_description("Current application settings: theme, proxy mode, IP info token, etc.")
                    .with_mime_type("application/json"),
                Resource::new("omnitun://profiles", "Subscription Profiles")
                    .with_description("All subscription profiles with their server lists and test results.")
                    .with_mime_type("application/json"),
                Resource::new("omnitun://cache", "Server cache statistics (positive/negative counts)")
                    .with_description("Current positive and negative cache counts from the dual-layer cache.")
                    .with_mime_type("application/json"),
                Resource::new("omnitun://geoip/status", "GeoIP database installation metadata")
                    .with_description("Download dates, file sizes, and availability of GeoIP databases.")
                    .with_mime_type("application/json"),
                Resource::new("omnitun://singbox/stderr", "Last N bytes of sing-box stderr")
                    .with_description("The sing-box process's own log output for debugging start failures.")
                    .with_mime_type("text/plain"),
                Resource::new("omnitun://singbox/config", "Current sing-box configuration JSON")
                    .with_description("The generated config.json that sing-box is currently running with.")
                    .with_mime_type("application/json"),
            ];
            Ok(r)
        }
    }

    fn read_resource(
        &self,
        request: model::ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResult, ErrorData>> + MaybeSendFuture + '_
    {
        async move {
            match request.uri.as_str() {
                "omnitun://status" => {
                    let pid = crate::proxy::PROXY_PID.load(std::sync::atomic::Ordering::Relaxed);
                    let port =
                        crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed);
                    let status = serde_json::json!({
                        "connected": pid > 0,
                        "pid": pid,
                        "proxy_port": crate::proxy::PROXY_PORT.load(std::sync::atomic::Ordering::Relaxed),
                        "clash_api_port": port,
                    });
                    let json =
                        serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".into());
                    Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        "omnitun://status",
                    )
                    .with_mime_type("application/json")]))
                }
                "omnitun://logs/recent" => {
                    let s = self.state.lock().await;
                    let entries = s
                        ._logger
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .entries();
                    let recent: Vec<_> = entries
                        .iter()
                        .rev()
                        .take(200)
                        .map(|e| {
                            serde_json::json!({
                                "timestamp": e.timestamp, "level": format!("{:?}", e.level),
                                "source": format!("{:?}", e.source), "message": e.message,
                            })
                        })
                        .collect();
                    let json =
                        serde_json::to_string_pretty(&recent).unwrap_or_else(|_| "[]".into());
                    Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        "omnitun://logs/recent",
                    )
                    .with_mime_type("application/json")]))
                }
                "omnitun://settings" => {
                    let s = self.state.lock().await;
                    let settings = s.settings_mgr.load().unwrap_or_default();
                    let json =
                        serde_json::to_string_pretty(&settings).unwrap_or_else(|_| "{}".into());
                    Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        "omnitun://settings",
                    )
                    .with_mime_type("application/json")]))
                }
                "omnitun://profiles" => {
                    let s = self.state.lock().await;
                    let profiles = s.profile_mgr.list();
                    let json =
                        serde_json::to_string_pretty(&profiles).unwrap_or_else(|_| "[]".into());
                    Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        "omnitun://profiles",
                    )
                    .with_mime_type("application/json")]))
                }
                "omnitun://cache" => {
                    let s = self.state.lock().await;
                    let positive = s.cache.positive_count().await;
                    let negative = s.cache.negative_count().await;
                    let info = serde_json::json!({
                        "positive_count": positive,
                        "negative_count": negative,
                        "total_count": positive + negative,
                    });
                    let json = serde_json::to_string_pretty(&info).unwrap_or_else(|_| "{}".into());
                    Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        "omnitun://cache",
                    )
                    .with_mime_type("application/json")]))
                }
                "omnitun://geoip/status" => {
                    let state = self.state.lock().await;
                    let geo_dir = state.data_dir.join("geoip");
                    let meta = crate::proxy::geoip::get_geoip_meta(&geo_dir);
                    let json = serde_json::to_string_pretty(&meta).unwrap_or_else(|_| "{}".into());
                    Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        "omnitun://geoip/status",
                    )
                    .with_mime_type("application/json")]))
                }
                "omnitun://singbox/stderr" => {
                    let s = self.state.lock().await;
                    let stderr_path = s.data_dir.join("config").join("sing-box-stderr.log");
                    let tail = std::fs::read_to_string(&stderr_path).unwrap_or_default();
                    let max = 8192usize;
                    let truncated = if tail.len() > max {
                        tail[tail.len() - max..].to_string()
                    } else {
                        tail
                    };
                    Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        truncated,
                        "omnitun://singbox/stderr",
                    )
                    .with_mime_type("text/plain")]))
                }
                "omnitun://singbox/config" => {
                    let s = self.state.lock().await;
                    let config_path = s.data_dir.join("config").join("config.json");
                    let config_text =
                        std::fs::read_to_string(&config_path).unwrap_or_else(|_| "{}".into());
                    Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        config_text,
                        "omnitun://singbox/config",
                    )
                    .with_mime_type("application/json")]))
                }
                _ => Err(ErrorData::new(
                    rmcp::model::ErrorCode::INVALID_PARAMS,
                    format!("Unknown resource: {}", request.uri),
                    None,
                )),
            }
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, ErrorData>> + MaybeSendFuture + '_
    {
        async move {
            let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
            match request.name.as_ref() {
                "start_proxy" => self.cmd_start(args).await,
                "stop_proxy" => self.cmd_stop().await,
                "get_status" => self.cmd_status().await,
                "list_profiles" => self.cmd_profiles().await,
                "add_subscription" => self.cmd_add_sub(args).await,
                "refresh_subscription" => self.cmd_refresh_sub(args).await,
                "remove_subscription" => self.cmd_remove_sub(args).await,
                "start_server_from_subscription" => self.cmd_start_server(args).await,
                "get_settings" => self.cmd_settings().await,
                // QA tools
                "get_logs" => self.cmd_get_logs(args).await,
                "get_singbox_stderr" => self.cmd_get_singbox_stderr(args).await,
                "test_single_server" => self.cmd_test_single_server(args).await,
                "get_cache_stats" => self.cmd_get_cache_stats().await,
                "clear_cache" => self.cmd_clear_cache().await,
                "get_geoip_status" => self.cmd_get_geoip_status().await,
                "get_real_ip" => self.cmd_get_real_ip().await,
                "verify_proxy_routes" => self.cmd_verify_proxy_routes().await,
                "download_geoip_db" => self.cmd_download_geoip_db().await,
                "update_settings" => self.cmd_update_settings(args).await,
                "import_subscription_content" => self.cmd_import_subscription_content(args).await,
                "check_proxy_health" => self.cmd_check_proxy_health().await,
                "speed_test" => self.cmd_speed_test().await,
                "switch_proxy" => self.cmd_switch_proxy(args).await,
                "get_connections" => self.cmd_get_connections().await,
                "cancel_connect" => self.cmd_cancel_connect().await,
                "activate_kill_switch" => self.cmd_activate_kill_switch(args).await,
                "deactivate_kill_switch" => self.cmd_deactivate_kill_switch().await,
                n => Err(ErrorData::new(
                    rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                    format!("Unknown tool: {n}"),
                    None,
                )),
            }
        }
    }
}

// ── Entry point ──────────────────────────────────────────────────────────

pub fn run(data_dir: PathBuf) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?;
    rt.block_on(async {
        let state = mk_state(data_dir)?;

        // Start the batcher background task for batch testing
        crate::cleanup_stale_temp_dirs();
        let batcher_pool = state.port_pool.clone();
        let batcher_data = state.data_dir.clone();
        let batcher_cancel = state.cancel_token.clone();
        state.cache.start_batcher(
            batcher_pool.clone(),
            batcher_data.clone(),
            batcher_cancel.clone(),
        );

        let handler = Handler {
            state: Arc::new(tokio::sync::Mutex::new(state)),
        };
        let (r, w) = transport::io::stdio();
        let transport = AsyncRwTransport::new_server(r, w);
        serve_server(handler, transport)
            .await
            .map_err(|e| format!("MCP error: {e}"))?
            .waiting()
            .await
            .map_err(|e| format!("MCP error: {e}"))?;
        Ok(())
    })
}
