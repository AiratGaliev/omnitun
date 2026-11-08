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

fn settings_json(settings: &AppSettings) -> serde_json::Value {
    let mut value = serde_json::to_value(settings).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = value.as_object_mut() {
        object.remove("ipinfoToken");
        object.insert(
            "ipinfoTokenConfigured".into(),
            serde_json::Value::Bool(!settings.ipinfo_token.is_empty()),
        );
    }
    value
}

fn required_u16_arg(args: &serde_json::Value, key: &str, minimum: u16) -> Result<u16, ErrorData> {
    let value = args
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ErrorData::invalid_params(format!("'{key}' must be an integer"), None))?;
    if value < u64::from(minimum) || value > u64::from(u16::MAX) {
        return Err(ErrorData::invalid_params(
            format!("'{key}' must be between {minimum} and {}", u16::MAX),
            None,
        ));
    }
    Ok(value as u16)
}

fn optional_u16_arg(
    args: &serde_json::Value,
    key: &str,
    default: u16,
    minimum: u16,
) -> Result<u16, ErrorData> {
    match args.get(key) {
        Some(_) => required_u16_arg(args, key, minimum),
        None if default >= minimum => Ok(default),
        None => Err(ErrorData::invalid_params(
            format!("Configured default for '{key}' must be at least {minimum}"),
            None,
        )),
    }
}

fn routing_mode_arg(args: &serde_json::Value, default: &str) -> Result<String, ErrorData> {
    let mode = match args.get("routing_mode") {
        Some(value) => value
            .as_str()
            .ok_or_else(|| ErrorData::invalid_params("'routing_mode' must be a string", None))?,
        None => default,
    };
    match mode {
        "system-proxy" | "local-proxy" => Ok(mode.to_string()),
        _ => Err(ErrorData::invalid_params(
            "'routing_mode' must be 'system-proxy' or 'local-proxy'",
            None,
        )),
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
    db: crate::db::DbPool,
}

struct Handler {
    state: Arc<tokio::sync::Mutex<ServerState>>,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeSnapshot {
    pid: u32,
    proxy_port: u16,
    clash_api_port: u16,
    system_proxy: bool,
}

fn runtime_snapshot(state: &ServerState) -> Option<RuntimeSnapshot> {
    let local_pid = state
        .process_mgr
        .pid()
        .filter(|pid| crate::is_process_alive(*pid));
    let pid = local_pid.or_else(crate::proxy::process::read_pid_file)?;

    let config_path = state.data_dir.join("config").join("config.json");
    let config = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .unwrap_or_default();
    let proxy_port = crate::proxy::PROXY_PORT.load(std::sync::atomic::Ordering::Acquire);
    let configured_port = config
        .get("inbounds")
        .and_then(serde_json::Value::as_array)
        .and_then(|inbounds| {
            inbounds.iter().find(|inbound| {
                inbound.get("type").and_then(serde_json::Value::as_str) == Some("mixed")
            })
        })
        .and_then(|inbound| inbound.get("listen_port"))
        .and_then(serde_json::Value::as_u64)
        .filter(|port| *port > 0 && *port <= u64::from(u16::MAX))
        .map(|port| port as u16)
        .unwrap_or(0);
    let proxy_port = if proxy_port > 0 {
        proxy_port
    } else {
        configured_port
    };

    let clash_api_port = crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Acquire);
    let configured_clash_port = config
        .pointer("/experimental/clash_api/external_controller")
        .and_then(serde_json::Value::as_str)
        .and_then(|address| address.rsplit_once(':').map(|(_, port)| port))
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(0);
    let clash_api_port = if clash_api_port > 0 {
        clash_api_port
    } else {
        configured_clash_port
    };
    let system_proxy = config
        .get("inbounds")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|inbounds| {
            inbounds.iter().any(|inbound| {
                inbound
                    .get("set_system_proxy")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            })
        });

    Some(RuntimeSnapshot {
        pid,
        proxy_port,
        clash_api_port,
        system_proxy,
    })
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

async fn mk_state(data_dir: PathBuf) -> Result<ServerState, String> {
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;

    // Set up tracing (JSONL file) so pipeline logs are captured
    let log_dir = data_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let (log_rx_tmp, log_file_path) = crate::logging::setup_logging(&log_dir);
    let mut log_rx = log_rx_tmp;
    // Spawn the broadcast receiver to prevent channel buildup, matching
    // the Tauri app's pattern. Pipeline logs go through tracing → JSONL file.
    tokio::spawn(async move {
        while let Ok(_entry) = log_rx.recv().await {
            // MCP server doesn't have Tauri events, but we consume the
            // broadcast to keep the channel from blocking.
        }
    });

    let logger = Arc::new(Mutex::new(Logger::new(1000, Some(log_file_path))));
    // Share the same PID file as the GUI so MCP can inspect/control the
    // process even when the proxy was started by the desktop application.
    // Do not call cleanup_stale_pid here: a second live client must not kill
    // the first client's active proxy during MCP startup.
    crate::proxy::set_pid_file_path(&data_dir);

    let settings_mgr = SettingsManager::new(data_dir.clone());
    let settings = settings_mgr.load().unwrap_or_default();
    crate::proxy::batcher::set_configured_test_url(&settings.test_url, settings.test_use_tls);
    let bin_path = data_dir.join("binaries").join(binary_name());
    let process_mgr = ProcessManager::new(bin_path.clone());

    // Initialize database
    let db = crate::db::init_db(&data_dir)
        .await
        .map_err(|e| format!("Failed to init DB: {e}"))?;

    // One-time migration from JSON
    if let Err(e) = crate::db::migrate_from_json(&db, &data_dir).await {
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
        &logger,
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
        db,
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
                    &state._logger,
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
        {
            let state = self.state.lock().await;
            if let Some(runtime) = runtime_snapshot(&state) {
                return Err(ErrorData::invalid_request(
                    format!("OmniTun proxy is already running (PID {}, port {}). Stop it before starting another proxy.", runtime.pid, runtime.proxy_port),
                    None,
                ));
            }
        }
        let server = args
            .get("server")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ErrorData::invalid_params("'server' is required", None))?;
        let port = required_u16_arg(&args, "port", 1)?;
        let auto_reconnect = args
            .get("auto_reconnect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let state = self.state.lock().await;
        let settings = state.settings_mgr.load().map_err(|e| {
            ErrorData::internal_error(format!("Failed to load settings: {e}"), None)
        })?;
        let proxy_port = optional_u16_arg(&args, "proxy_port", settings.proxy_port, 1024)?;
        let routing_mode = routing_mode_arg(&args, &settings.routing_mode)?;

        let mut profile = profile_from_args(&args);
        profile.server = server.into();
        profile.port = port;

        let (pid, actual_port) = launch_sing_box(
            &state,
            proxy_port,
            &routing_mode,
            &[profile.clone()],
            Some(&settings),
        )
        .await?;
        state.process_mgr.set_pid(pid);
        crate::proxy::PROXY_PORT.store(actual_port, std::sync::atomic::Ordering::Release);
        {
            let mut active = crate::proxy::ACTIVE_SERVER_ID
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *active = Some(profile.id.clone());
        }
        {
            let mut last = crate::proxy::LAST_PROFILE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *last = Some(crate::proxy::LastConnection {
                all_servers: vec![profile],
                routing_mode: routing_mode.clone(),
                proxy_port: actual_port,
            });
        }
        let cancel_token = {
            let guard = state.cancel_token.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        let state_clone = self.state.clone();
        drop(state);
        spawn_proxy_watchdog(cancel_token, state_clone, pid, actual_port, auto_reconnect);
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Proxy started (PID {pid}) on port {actual_port}"
        ))]))
    }

    async fn cmd_stop(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let runtime = runtime_snapshot(&state);
        if let Some(runtime) = runtime {
            if state.process_mgr.pid() != Some(runtime.pid) {
                state.process_mgr.set_pid(runtime.pid);
            }
        }
        if let Some(runtime) = runtime {
            crate::proxy::process::request_manual_stop(&state.data_dir, runtime.pid)
                .map_err(|e| ErrorData::internal_error(e, None))?;
        }
        let should_reset_system_proxy = runtime.is_some_and(|runtime| runtime.system_proxy)
            || crate::proxy::LAST_PROFILE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|last| last.routing_mode == "system-proxy");

        let stop_result = state.process_mgr.stop().await;
        if let (Err(error), Some(runtime)) = (&stop_result, runtime) {
            crate::proxy::process::clear_manual_stop_request(&state.data_dir, runtime.pid);
            return Err(ErrorData::internal_error(error.to_string(), None));
        }
        stop_result.map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reset_result = if should_reset_system_proxy {
            crate::proxy::process::reset_system_proxy()
        } else {
            Ok(())
        };
        crate::proxy::PROXY_PID.store(0, std::sync::atomic::Ordering::Release);
        crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Release);
        crate::proxy::PROXY_PORT.store(0, std::sync::atomic::Ordering::Release);
        *crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *crate::proxy::LAST_PROFILE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        reset_result.map_err(|e| ErrorData::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(
            "Proxy stopped",
        )]))
    }

    async fn status_snapshot(&self) -> Result<serde_json::Value, ErrorData> {
        let state = self.state.lock().await;
        let runtime = runtime_snapshot(&state);
        if let Some(runtime) = runtime {
            if state.process_mgr.pid() != Some(runtime.pid) {
                state.process_mgr.set_pid(runtime.pid);
            }
        } else if state.process_mgr.pid().is_some() {
            state.process_mgr.clear_pid();
            crate::proxy::PROXY_PID.store(0, std::sync::atomic::Ordering::Release);
        }

        let settings = state.settings_mgr.load().map_err(|e| {
            ErrorData::internal_error(format!("Failed to load settings: {e}"), None)
        })?;
        let clash_api_port = runtime.map(|snapshot| snapshot.clash_api_port).unwrap_or(0);
        let proxy_port = runtime.map(|snapshot| snapshot.proxy_port).unwrap_or(0);
        let active_server_id = crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();
        let routing_mode = runtime
            .map(|snapshot| {
                if snapshot.system_proxy {
                    "system-proxy"
                } else {
                    "local-proxy"
                }
            })
            .unwrap_or(settings.routing_mode.as_str());
        let pid = runtime.map(|snapshot| snapshot.pid).unwrap_or(0);
        Ok(serde_json::json!({
            "status": if pid > 0 { "connected" } else { "disconnected" },
            "connected": pid > 0,
            "pid": pid,
            "proxy_port": proxy_port,
            "configured_proxy_port": settings.proxy_port,
            "routing_mode": routing_mode,
            "clash_api_port": clash_api_port,
            "active_server_id": if pid > 0 { active_server_id } else { String::new() },
            "binary_installed": state.bin_path.exists(),
            "uptime_secs": null,
        }))
    }

    async fn cmd_status(&self) -> Result<CallToolResult, ErrorData> {
        let info = self.status_snapshot().await?;
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
        let name_owned = name.to_string();
        let profile_id = uuid::Uuid::new_v4().to_string();

        // Check for duplicate URL before fetching
        {
            let state = self.state.lock().await;
            if state.profile_mgr.list().iter().any(|p| p.url == url_owned) {
                return Err(ErrorData::invalid_params(
                    format!("Subscription with URL '{}' already exists", url_owned),
                    None,
                ));
            }
        }

        // Fetch + parse subscription
        let raw_servers = if url.starts_with("http://") || url.starts_with("https://") {
            crate::subscription::fetch_subscription(&url_owned, None)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
        } else {
            crate::subscription::parser::parse_subscription(&url_owned)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
        };

        // Step 0: save empty profile with real name/url BEFORE process_servers_incremental
        // so Phase 3 POST's FOREIGN KEY constraint has a parent row to reference.
        // Matches Tauri ProfileManager::add() behavior (lines 528-608).
        {
            let mut state = self.state.lock().await;
            let initial = crate::subscription::SubscriptionProfile {
                id: profile_id.clone(),
                name: name_owned.clone(),
                url: url_owned.clone(),
                servers: Vec::new(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                last_tested: None,
                traffic_used: None,
                traffic_total: None,
                traffic_expiry: None,
            };
            state.profile_mgr.list_mut().push(initial);
            state
                .profile_mgr
                .save()
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        }

        // process_servers_incremental — FK now satisfied because profile row exists
        let (data_dir, settings, cache, db) = {
            let state = self.state.lock().await;
            (
                state.data_dir.clone(),
                state.settings_mgr.load().unwrap_or_default(),
                state.cache.clone(),
                state.db.clone(),
            )
        };

        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let tested = crate::profile::process_servers_incremental(
            &data_dir,
            &settings,
            raw_servers,
            &profile_id,
            tokio_util::sync::CancellationToken::new(),
            event_tx,
            None,
            &db,
            &cache,
        )
        .await;
        cache.save_to_disk().await;

        // Update profile with tested servers
        let mut state = self.state.lock().await;
        if let Some(existing) = state
            .profile_mgr
            .list_mut()
            .iter_mut()
            .find(|p| p.id == profile_id)
        {
            existing.servers = tested;
            existing.updated_at = chrono::Utc::now().to_rfc3339();
            existing.last_tested = Some(chrono::Utc::now().to_rfc3339());
        }
        let _ = state.profile_mgr.save();
        let result = state
            .profile_mgr
            .list()
            .iter()
            .find(|p| p.id == profile_id)
            .map(|p| serde_json::to_string_pretty(p).unwrap_or_default())
            .unwrap_or_else(|| "null".to_string());
        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }

    async fn cmd_refresh_sub(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'id' is required", None))?
            .to_string();

        // Phase 1: read lock — get URL + old servers + settings + data_dir + db
        let (url, _old_servers, data_dir, settings, cache, db) = {
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
            let db = state.db.clone();
            (url, old_servers, data_dir, settings, cache, db)
        }; // lock released

        // Spawn background task for long operations (fetch, ping, batch test)
        let bg_id = id.clone();
        let bg_state = self.state.clone();
        tokio::spawn(async move {
            match crate::subscription::fetch_subscription(&url, None).await {
                Ok(servers) => {
                    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
                    let enriched = crate::profile::process_servers_incremental(
                        &data_dir,
                        &settings,
                        servers,
                        &bg_id,
                        tokio_util::sync::CancellationToken::new(),
                        event_tx,
                        None, // progress_tx — MCP has no Tauri events
                        &db,
                        &cache,
                    )
                    .await;

                    // Persist cache to disk so batch-test results survive app restart.
                    cache.save_to_disk().await;

                    // Use commit_refresh_headless to properly set last_tested and
                    // notify cache about stale entries.
                    {
                        let mut state = bg_state.lock().await;
                        match state
                            .profile_mgr
                            .commit_refresh_headless(&bg_id, enriched, Some(cache.clone()), None)
                            .await
                        {
                            Ok(_profile) => {}
                            Err(e) => {
                                tracing::warn!(target: "subscription",
                                    "commit_refresh_headless failed for {bg_id}: {e}");
                            }
                        }
                    };

                    // Re-read to log the server count
                    {
                        let state = bg_state.lock().await;
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
        {
            let state = self.state.lock().await;
            if let Some(runtime) = runtime_snapshot(&state) {
                return Err(ErrorData::invalid_request(
                    format!("OmniTun proxy is already running (PID {}, port {}). Stop it before starting another proxy.", runtime.pid, runtime.proxy_port),
                    None,
                ));
            }
        }
        let sub_id = args
            .get("subscription_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ErrorData::invalid_params("'subscription_id' is required", None))?;
        let server_id = args
            .get("server_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ErrorData::invalid_params("'server_id' is required", None))?;
        let auto_reconnect = args
            .get("auto_reconnect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let state = self.state.lock().await;
        let settings = state.settings_mgr.load().map_err(|e| {
            ErrorData::internal_error(format!("Failed to load settings: {e}"), None)
        })?;
        let proxy_port = optional_u16_arg(&args, "proxy_port", settings.proxy_port, 1024)?;
        let routing_mode = routing_mode_arg(&args, &settings.routing_mode)?;
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

        let (pid, actual_port) = launch_sing_box(
            &state,
            proxy_port,
            &routing_mode,
            std::slice::from_ref(server),
            Some(&settings),
        )
        .await?;
        state.process_mgr.set_pid(pid);
        crate::proxy::PROXY_PORT.store(actual_port, std::sync::atomic::Ordering::Release);
        *crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(server.id.clone());
        *crate::proxy::LAST_PROFILE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(crate::proxy::LastConnection {
            all_servers: vec![server.clone()],
            routing_mode: routing_mode.clone(),
            proxy_port: actual_port,
        });
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
        let settings = state.settings_mgr.load().map_err(|e| {
            ErrorData::internal_error(format!("Failed to load settings: {e}"), None)
        })?;
        let json =
            serde_json::to_string_pretty(&settings_json(&settings)).unwrap_or_else(|_| "{}".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

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
        let logger = state._logger.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries = logger.entries();

        // Also read from tracing JSONL file for cross-session continuity.
        // Pipeline code (batch testing, subscription refresh) uses tracing::info!
        // directly, which only goes to the JSONL file, not the ring buffer.
        if let Some(ref path) = logger._log_file {
            if path.exists() {
                let file_entries = crate::logging::read_tracing_log_file(path, logger.max_entries);
                let existing_ids: std::collections::HashSet<String> =
                    entries.iter().map(|e| e.id.clone()).collect();
                for e in file_entries {
                    if !existing_ids.contains(&e.id) {
                        entries.push(e);
                    }
                }
            }
        }
        drop(logger);

        entries.sort_by_key(|b| std::cmp::Reverse(b.timestamp));

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
        state
            .cache
            .clear_all()
            .await
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "All persistent and in-memory cache entries cleared",
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
    /// Update a partial set of application preferences. ipinfo_token is write-only.
    async fn cmd_update_settings(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        const FIELDS: &[(&str, &str)] = &[
            ("routing_mode", "routingMode"),
            ("proxy_port", "proxyPort"),
            ("bypass_ru", "bypassRu"),
            ("start_with_windows", "startWithWindows"),
            ("auto_connect_fastest", "autoConnectFastest"),
            ("kill_switch", "killSwitch"),
            ("allow_lan", "allowLan"),
            ("custom_dns", "customDns"),
            ("dns_address", "dnsAddress"),
            ("allowed_countries", "allowedCountries"),
            ("blocked_countries", "blockedCountries"),
            ("custom_countries", "customCountries"),
            ("allowed_protocols", "allowedProtocols"),
            ("blocked_protocols", "blockedProtocols"),
            ("language", "language"),
            ("auto_update_interval", "autoUpdateInterval"),
            ("auto_test_interval", "autoTestInterval"),
            ("anti_dpi", "antiDpi"),
            ("enable_mux", "enableMux"),
            ("minimize_to_tray", "minimizeToTray"),
            ("block_cdn_nodes", "blockCdnNodes"),
            ("ipinfo_token", "ipinfoToken"),
            ("tls_insecure", "tlsInsecure"),
            ("test_url", "testUrl"),
            ("test_use_tls", "testUseTls"),
        ];

        let input = args
            .as_object()
            .ok_or_else(|| ErrorData::invalid_params("Settings input must be an object", None))?;
        for key in input.keys() {
            if !FIELDS.iter().any(|(field, _)| field == key) {
                return Err(ErrorData::invalid_params(
                    format!("Unsupported setting '{key}'"),
                    None,
                ));
            }
        }

        // Keep the state lock for the complete read-modify-validate-save cycle.
        // Concurrent partial updates must not overwrite one another, and readers
        // must never observe an update halfway through its persistence step.
        let state = self.state.lock().await;
        let mut settings = state.settings_mgr.load().map_err(|e| {
            ErrorData::internal_error(format!("Failed to load settings: {e}"), None)
        })?;
        let mut value = serde_json::to_value(&settings).map_err(|e| {
            ErrorData::internal_error(format!("Failed to serialize settings: {e}"), None)
        })?;
        let object = value.as_object_mut().ok_or_else(|| {
            ErrorData::internal_error("Settings serialization was not an object", None)
        })?;
        for (input_key, settings_key) in FIELDS {
            if let Some(value) = input.get(*input_key) {
                object.insert((*settings_key).into(), value.clone());
            }
        }
        settings = serde_json::from_value(value)
            .map_err(|e| ErrorData::invalid_params(format!("Invalid setting value: {e}"), None))?;
        settings.normalize();

        if settings.proxy_port < 1024 {
            return Err(ErrorData::invalid_params(
                "'proxy_port' must be between 1024 and 65535",
                None,
            ));
        }
        if !matches!(
            settings.routing_mode.as_str(),
            "system-proxy" | "local-proxy"
        ) {
            return Err(ErrorData::invalid_params(
                "'routing_mode' must be 'system-proxy' or 'local-proxy'",
                None,
            ));
        }
        if !matches!(settings.language.as_str(), "en" | "ru") {
            return Err(ErrorData::invalid_params(
                "'language' must be 'en' or 'ru'",
                None,
            ));
        }
        if settings.auto_update_interval > 10080 || settings.auto_test_interval > 10080 {
            return Err(ErrorData::invalid_params(
                "Automatic intervals must be between 0 and 10080 minutes",
                None,
            ));
        }
        if settings.test_url.trim().is_empty() || settings.test_url.len() > 255 {
            return Err(ErrorData::invalid_params(
                "'test_url' must contain 1 to 255 characters",
                None,
            ));
        }
        if settings.custom_dns && settings.dns_address.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "'dns_address' is required when custom DNS is enabled",
                None,
            ));
        }

        state
            .settings_mgr
            .save(&settings)
            .map_err(|e| ErrorData::internal_error(e, None))?;
        crate::proxy::batcher::set_configured_test_url(&settings.test_url, settings.test_use_tls);
        let json =
            serde_json::to_string_pretty(&settings_json(&settings)).unwrap_or_else(|_| "{}".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

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
        let content_url = format!("content://{}", uuid::Uuid::new_v4());

        // Extract cache + data_dir + db, run process_servers_incremental, then save
        let (processed, cache) = {
            let state = self.state.lock().await;
            let data_dir = state.data_dir.clone();
            let cache = state.cache.clone();
            let db = state.db.clone();
            let settings = state.settings_mgr.load().unwrap_or_default();
            drop(state);
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            let processed = crate::profile::process_servers_incremental(
                &data_dir,
                &settings,
                servers,
                &profile_id,
                tokio_util::sync::CancellationToken::new(),
                event_tx,
                None,
                &db,
                &cache,
            )
            .await;
            (processed, cache)
        };
        cache.save_to_disk().await;

        let profile = crate::subscription::SubscriptionProfile {
            id: profile_id,
            name: name.into(),
            url: content_url,
            servers: processed,
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
        let runtime = runtime_snapshot(&state);
        let proxy_port = runtime.map(|snapshot| snapshot.proxy_port).unwrap_or(0);
        if proxy_port == 0 || runtime.is_none() {
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

    async fn current_clash_api_port(&self) -> u16 {
        let state = self.state.lock().await;
        runtime_snapshot(&state)
            .map(|runtime| runtime.clash_api_port)
            .unwrap_or(0)
    }

    /// Check the health of the active proxy outbound via Clash API.
    async fn cmd_check_proxy_health(&self) -> Result<CallToolResult, ErrorData> {
        let stats_port = self.current_clash_api_port().await;
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
        let stats_port = self.current_clash_api_port().await;
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
        let stats_port = self.current_clash_api_port().await;
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
        let stats_port = self.current_clash_api_port().await;
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
        let runtime = runtime_snapshot(&state);
        if let Some(runtime) = runtime {
            if state.process_mgr.pid() != Some(runtime.pid) {
                state.process_mgr.set_pid(runtime.pid);
            }
        }
        if let Some(runtime) = runtime {
            crate::proxy::process::request_manual_stop(&state.data_dir, runtime.pid)
                .map_err(|e| ErrorData::internal_error(e, None))?;
        }
        let should_reset_system_proxy = runtime.is_some_and(|runtime| runtime.system_proxy)
            || crate::proxy::LAST_PROFILE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|last| last.routing_mode == "system-proxy");
        let stop_result = state.process_mgr.stop().await;
        if let (Err(error), Some(runtime)) = (&stop_result, runtime) {
            crate::proxy::process::clear_manual_stop_request(&state.data_dir, runtime.pid);
            crate::proxy::PROXY_PID.store(0, std::sync::atomic::Ordering::Release);
            crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Release);
            crate::proxy::PROXY_PORT.store(0, std::sync::atomic::Ordering::Release);
            *crate::proxy::LAST_PROFILE
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            crate::proxy::CONNECTION_CANCELLING.store(false, std::sync::atomic::Ordering::SeqCst);
            return Err(ErrorData::internal_error(error.to_string(), None));
        }
        let reset_result = if should_reset_system_proxy {
            crate::proxy::process::reset_system_proxy()
        } else {
            Ok(())
        };
        crate::proxy::PROXY_PID.store(0, std::sync::atomic::Ordering::Release);
        crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Release);
        crate::proxy::PROXY_PORT.store(0, std::sync::atomic::Ordering::Release);
        *crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *crate::proxy::LAST_PROFILE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        crate::proxy::CONNECTION_CANCELLING.store(false, std::sync::atomic::Ordering::SeqCst);
        stop_result.map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        reset_result.map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "Cancelled",
        )]))
    }

    /// Activate kill switch on a given port.
    async fn cmd_activate_kill_switch(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let settings = state.settings_mgr.load().map_err(|e| {
            ErrorData::internal_error(format!("Failed to load settings: {e}"), None)
        })?;
        let proxy_port = optional_u16_arg(&args, "proxy_port", settings.proxy_port, 1024)?;
        let routing_mode = routing_mode_arg(&args, &settings.routing_mode)?;

        let runtime = runtime_snapshot(&state);
        if let Some(runtime) = runtime {
            if state.process_mgr.pid() != Some(runtime.pid) {
                state.process_mgr.set_pid(runtime.pid);
            }
            crate::proxy::process::request_manual_stop(&state.data_dir, runtime.pid)
                .map_err(|e| ErrorData::internal_error(e, None))?;
            *crate::proxy::LAST_PROFILE
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            state
                .process_mgr
                .stop()
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            if runtime.system_proxy && routing_mode != "system-proxy" {
                crate::proxy::process::reset_system_proxy()
                    .map_err(|e| ErrorData::internal_error(e, None))?;
            }
        }

        let ks_cfg = crate::proxy::config::generate_kill_switch_config(proxy_port, &routing_mode)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let cpath = crate::proxy::config::write_config(&ks_cfg, &state.data_dir.join("config"))
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let pid = state
            .process_mgr
            .start_headless(&cpath)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        crate::proxy::PROXY_PORT.store(proxy_port, std::sync::atomic::Ordering::Release);
        crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Release);
        *crate::proxy::ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *crate::proxy::LAST_PROFILE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Kill switch active on port {proxy_port} (PID {pid})"
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
- The ipinfo_token can be set with update_settings, but secrets are never returned by get_settings or the settings resource.
- start_proxy and start_server_from_subscription use saved proxy_port and routing_mode unless explicitly overridden.

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
                    "proxy_port": { "type": "integer", "description": "Optional local SOCKS5 port override; defaults to saved proxy_port", "minimum": 1024, "maximum": 65535 },
                    "routing_mode": { "type": "string", "description": "Optional routing mode override; defaults to saved routing_mode", "enum": ["system-proxy","local-proxy"] },
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
                    "proxy_port":      { "type": "integer", "description": "Optional local SOCKS5 port override; defaults to saved proxy_port", "minimum": 1024, "maximum": 65535 },
                    "routing_mode":    { "type": "string", "description": "Optional routing mode override; defaults to saved routing_mode", "enum": ["system-proxy","local-proxy"] },
                    "auto_reconnect":  { "type": "boolean", "description": "Restart the proxy after an unexpected process exit", "default": false }
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
        .with_annotations(tool_ann(false, false, false, true)),
        Tool::new("get_cache_stats",
            "Get cache statistics: positive and negative counts.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        )
        .with_annotations(tool_ann(true, false, true, false)),
        Tool::new("clear_cache",
            "Clear persistent cache records and all in-memory positive/negative cache layers immediately.",
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
        .with_annotations(tool_ann(true, false, false, true)),
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
            "Partially update supported application preferences. Supports language, filtering, network, startup and testing preferences. ipinfo_token is write-only and is never returned.",
            schema(serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "routing_mode": { "type": "string", "enum": ["system-proxy","local-proxy"] },
                    "proxy_port": { "type": "integer", "minimum": 1024, "maximum": 65535 },
                    "bypass_ru": { "type": "boolean" },
                    "start_with_windows": { "type": "boolean" },
                    "auto_connect_fastest": { "type": "boolean" },
                    "kill_switch": { "type": "boolean" },
                    "allow_lan": { "type": "boolean" },
                    "custom_dns": { "type": "boolean" },
                    "dns_address": { "type": "string", "maxLength": 255 },
                    "allowed_countries": { "type": "array", "items": { "type": "string" } },
                    "blocked_countries": { "type": "array", "items": { "type": "string" } },
                    "custom_countries": { "type": "array", "items": { "type": "object", "properties": { "code": { "type": "string" }, "name": { "type": "string" }, "flag": { "type": "string" } }, "required": ["code", "name", "flag"], "additionalProperties": false } },
                    "allowed_protocols": { "type": "array", "items": { "type": "string" } },
                    "blocked_protocols": { "type": "array", "items": { "type": "string" } },
                    "language": { "type": "string", "enum": ["en", "ru"] },
                    "auto_update_interval": { "type": "integer", "minimum": 0, "maximum": 10080 },
                    "auto_test_interval": { "type": "integer", "minimum": 0, "maximum": 10080 },
                    "anti_dpi": { "type": "boolean" },
                    "enable_mux": { "type": "boolean" },
                    "minimize_to_tray": { "type": "boolean" },
                    "block_cdn_nodes": { "type": "boolean" },
                    "ipinfo_token": { "type": "string", "description": "Sensitive write-only API token. The server never returns this value." },
                    "tls_insecure": { "type": "boolean" },
                    "test_url": { "type": "string", "minLength": 1, "maxLength": 255 },
                    "test_use_tls": { "type": "boolean" }
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

fn define_resources() -> Vec<Resource> {
    vec![
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

    #[allow(clippy::manual_async_fn)]
    fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_
    {
        async {
            Ok(ListToolsResult {
                tools: define_tools(),
                ..Default::default()
            })
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, ErrorData>> + MaybeSendFuture + '_
    {
        async {
            Ok(ListResourcesResult {
                resources: define_resources(),
                ..Default::default()
            })
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn read_resource(
        &self,
        request: model::ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResult, ErrorData>> + MaybeSendFuture + '_
    {
        async move {
            match request.uri.as_str() {
                "omnitun://status" => {
                    let status = self.status_snapshot().await?;
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
                    let logger = s._logger.lock().unwrap_or_else(|e| e.into_inner());
                    let mut entries = logger.entries();

                    // Read from tracing JSONL file for cross-session continuity
                    if let Some(ref path) = logger._log_file {
                        if path.exists() {
                            let file_entries =
                                crate::logging::read_tracing_log_file(path, logger.max_entries);
                            let existing_ids: std::collections::HashSet<String> =
                                entries.iter().map(|e| e.id.clone()).collect();
                            for e in file_entries {
                                if !existing_ids.contains(&e.id) {
                                    entries.push(e);
                                }
                            }
                        }
                    }
                    drop(logger);

                    entries.sort_by_key(|b| std::cmp::Reverse(b.timestamp));
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
                    let json = serde_json::to_string_pretty(&settings_json(&settings))
                        .unwrap_or_else(|_| "{}".into());
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

    #[allow(clippy::manual_async_fn)]
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
        let state = mk_state(data_dir).await?;

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

// ============================================================================
// MCP contract tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_exposes_required_tools_and_resources() {
        let tools = serde_json::to_value(define_tools()).unwrap();
        let tool_list = tools.as_array().expect("tools must be a JSON array");
        let tool_names: Vec<&str> = tool_list
            .iter()
            .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
            .collect();

        for required in [
            "get_status",
            "get_settings",
            "update_settings",
            "start_server_from_subscription",
            "stop_proxy",
            "clear_cache",
        ] {
            assert!(tool_names.contains(&required), "Missing MCP tool: {required}");
        }

        let resources = serde_json::to_value(define_resources()).unwrap();
        let resource_list = resources.as_array().expect("resources must be a JSON array");
        let resource_uris: Vec<&str> = resource_list
            .iter()
            .filter_map(|resource| resource.get("uri").and_then(serde_json::Value::as_str))
            .collect();
        for required in [
            "omnitun://status",
            "omnitun://settings",
            "omnitun://profiles",
            "omnitun://cache",
            "omnitun://singbox/config",
        ] {
            assert!(resource_uris.contains(&required), "Missing MCP resource: {required}");
        }
    }

    #[test]
    fn settings_json_never_exposes_ipinfo_token() {
        let mut settings = AppSettings::default();
        settings.ipinfo_token = "mcp-secret-token-must-not-leak".into();

        let json = settings_json(&settings);
        let serialized = json.to_string();

        assert!(json.get("ipinfoToken").is_none());
        assert!(!serialized.contains("mcp-secret-token-must-not-leak"));
        assert_eq!(json["ipinfoTokenConfigured"], true);
    }

    #[test]
    fn proxy_port_arguments_are_validated_without_truncation() {
        assert_eq!(
            required_u16_arg(&serde_json::json!({"port": 443}), "port", 1).unwrap(),
            443
        );
        assert_eq!(
            required_u16_arg(&serde_json::json!({"port": 65535}), "port", 1).unwrap(),
            65535
        );
        assert!(required_u16_arg(&serde_json::json!({"port": 0}), "port", 1).is_err());
        assert!(required_u16_arg(&serde_json::json!({"port": 70000}), "port", 1).is_err());
        assert!(required_u16_arg(&serde_json::json!({"port": -1}), "port", 1).is_err());
        assert!(required_u16_arg(&serde_json::json!({"port": 5055.5}), "port", 1).is_err());
        assert!(required_u16_arg(&serde_json::json!({}), "port", 1).is_err());
    }

    #[test]
    fn optional_proxy_port_uses_saved_value_and_validates_overrides() {
        assert_eq!(
            optional_u16_arg(&serde_json::json!({}), "proxy_port", 1080, 1024).unwrap(),
            1080
        );
        assert_eq!(
            optional_u16_arg(
                &serde_json::json!({"proxy_port": 18080}),
                "proxy_port",
                1080,
                1024
            )
            .unwrap(),
            18080
        );
        assert!(optional_u16_arg(&serde_json::json!({}), "proxy_port", 505, 1024).is_err());
        assert!(optional_u16_arg(
            &serde_json::json!({"proxy_port": 70000}),
            "proxy_port",
            1080,
            1024
        )
        .is_err());
    }

    #[test]
    fn routing_mode_accepts_only_supported_values() {
        assert_eq!(
            routing_mode_arg(&serde_json::json!({}), "local-proxy").unwrap(),
            "local-proxy"
        );
        assert_eq!(
            routing_mode_arg(
                &serde_json::json!({"routing_mode": "system-proxy"}),
                "local-proxy"
            )
            .unwrap(),
            "system-proxy"
        );
        assert!(
            routing_mode_arg(&serde_json::json!({"routing_mode": "auto"}), "local-proxy").is_err()
        );
        assert!(routing_mode_arg(&serde_json::json!({"routing_mode": 1}), "local-proxy").is_err());
    }
}
