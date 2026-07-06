use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use rmcp::{
    ErrorData,
    model::{
        self, CallToolRequestParams, CallToolResult, ContentBlock,
        Implementation, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool, ToolsCapability,
    },
    service::{MaybeSendFuture, RequestContext, RoleServer},
    transport::{self, async_rw::AsyncRwTransport},
    ServerHandler, serve_server,
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
    cancel_token: tokio_util::sync::CancellationToken,
}

struct Handler {
    state: Arc<tokio::sync::Mutex<ServerState>>,
}

fn binary_name() -> &'static str {
    #[cfg(target_os = "windows")] { "sing-box.exe" }
    #[cfg(not(target_os = "windows"))] { "sing-box" }
}

fn mk_state(data_dir: PathBuf) -> Result<ServerState, String> {
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
    let logger = Arc::new(Mutex::new(Logger::new(1000, None)));
    let settings_mgr = SettingsManager::new(data_dir.clone());
    let bin_path = data_dir.join("binaries").join(binary_name());
    let process_mgr = ProcessManager::new(bin_path.clone());
    let mut profile_mgr = super::profile::ProfileManager::new(data_dir.clone());
    let _ = profile_mgr.load();

    // Create cache, port pool, and cancellation token
    let cache = Arc::new(ServerCache::new(&data_dir));
    let port_pool = Arc::new(PortPool::new());
    let cancel_token = tokio_util::sync::CancellationToken::new();

    log_to_logger(&*logger, LogLevel::Info, LogSource::System,
        format!("MCP server starting (data_dir: {:?})", data_dir));

    Ok(ServerState {
        process_mgr, profile_mgr, settings_mgr, bin_path, data_dir, _logger: logger,
        cache, port_pool, cancel_token,
    })
}

// ── Helper: build ServerProfile from JSON args ──────────────────────────

fn profile_from_args(args: &serde_json::Value) -> ServerProfile {
    ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        protocol: args.get("protocol").and_then(|v| v.as_str()).unwrap_or("vless").into(),
        server: args.get("server").and_then(|v| v.as_str()).unwrap_or("").into(),
        port: args.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16,
        uuid: args.get("uuid").and_then(|v| v.as_str()).map(String::from),
        password: args.get("password").and_then(|v| v.as_str()).map(String::from),
        method: args.get("method").and_then(|v| v.as_str()).map(String::from),
        sni: args.get("sni").and_then(|v| v.as_str()).map(String::from),
        flow: args.get("flow").and_then(|v| v.as_str()).map(String::from),
        public_key: args.get("public_key").and_then(|v| v.as_str()).map(String::from),
        private_key: args.get("private_key").and_then(|v| v.as_str()).map(String::from),
        transport_type: args.get("transport_type").and_then(|v| v.as_str()).map(String::from),
        transport_path: args.get("transport_path").and_then(|v| v.as_str()).map(String::from),
        transport_host: args.get("transport_host").and_then(|v| v.as_str()).map(String::from),
        reality_pbk: args.get("reality_pbk").and_then(|v| v.as_str()).map(String::from),
        reality_sid: args.get("reality_sid").and_then(|v| v.as_str()).map(String::from),
        username: args.get("username").and_then(|v| v.as_str()).map(String::from),
        ..Default::default()
    }
}

// ── Helper: launch sing-box with config ─────────────────────────────────

async fn launch_sing_box(state: &ServerState, proxy_port: u16, routing_mode: &str, profiles: &[ServerProfile]) -> Result<u32, ErrorData> {
    let (cfg, _) = crate::proxy::config::generate_config(
        &state.data_dir.join("config"), proxy_port, routing_mode, profiles, &AppSettings::default(),
    ).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    let cpath = crate::proxy::config::write_config(&cfg, &state.data_dir.join("config"))
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

    let b = state.bin_path.to_str().unwrap_or("");
    let c = cpath.to_str().unwrap_or("");

    // Open stderr log file next to config.json for diagnostics.
    let stderr_path = state.data_dir.join("config").join("sing-box-stderr.log");
    let stderr_stdio = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
    {
        Ok(f) => std::process::Stdio::from(f),
        Err(_) => std::process::Stdio::null(),
    };

    let child = std::process::Command::new(b)
        .args(["run", "-c", c])
        .stdout(std::process::Stdio::null())
        .stderr(stderr_stdio)
        .spawn()
        .map_err(|e| ErrorData::internal_error(format!("spawn: {e}"), None))?;

    Ok(child.id())
}

// ── Command handlers ─────────────────────────────────────────────────────

impl Handler {
    async fn cmd_start(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let server = args.get("server").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            .ok_or_else(|| ErrorData::invalid_params("'server' is required", None))?;
        let port = args.get("port").and_then(|v| v.as_u64())
            .ok_or_else(|| ErrorData::invalid_params("'port' is required", None))? as u16;
        let proxy_port = args.get("proxy_port").and_then(|v| v.as_u64()).unwrap_or(5050) as u16;
        let routing_mode = args.get("routing_mode").and_then(|v| v.as_str()).unwrap_or("system-proxy");

        let mut profile = profile_from_args(&args);
        profile.server = server.into();
        profile.port = port;

        let state = self.state.lock().await;
        let pid = launch_sing_box(&state, proxy_port, routing_mode, &[profile]).await?;
        state.process_mgr.set_pid(pid);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!("Proxy started (PID {pid}) on port {proxy_port}"))
        ]))
    }

    async fn cmd_stop(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        state.process_mgr.stop().await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text("Proxy stopped")]))
    }

    async fn cmd_status(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let status = match state.process_mgr.pid() {
            Some(p) if crate::is_process_alive(p) => "connected",
            _ => "disconnected",
        };
        let pid = state.process_mgr.pid().unwrap_or(0);
        let info = serde_json::json!({"status": status, "pid": pid});
        Ok(CallToolResult::success(vec![ContentBlock::text(info.to_string())]))
    }

    async fn cmd_profiles(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let j = serde_json::to_string_pretty(state.profile_mgr.list())
            .unwrap_or_else(|_| "[]".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(j)]))
    }

    async fn cmd_add_sub(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let url = args.get("url").and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'url' is required", None))?;
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or(url);

        let profile = crate::subscription::SubscriptionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            url: url.into(),
            name: name.into(),
            servers: Vec::new(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            traffic_used: None,
            traffic_total: None,
            traffic_expiry: None,
        };

        let mut state = self.state.lock().await;
        state.profile_mgr.list_mut().push(profile.clone());
        state.profile_mgr.save()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let j = serde_json::to_string_pretty(&profile).unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(j)]))
    }

    async fn cmd_refresh_sub(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let id = args.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'id' is required", None))?;

        // Phase 1: read lock — get URL + settings + data_dir
        let (url, data_dir, settings) = {
            let state = self.state.lock().await;
            let url = state.profile_mgr.list().iter()
                .find(|p| p.id == id)
                .ok_or_else(|| ErrorData::invalid_params(format!("Profile '{id}' not found"), None))?
                .url
                .clone();
            let data_dir = state.data_dir.clone();
            let settings = state.settings_mgr.load()
                .unwrap_or_default();
            (url, data_dir, settings)
        }; // lock released

        // Phase 2: fetch + enrich + batch test (no lock — long operations)
        let servers = crate::subscription::fetch_subscription(&url, None).await
            .map_err(|e| ErrorData::internal_error(format!("fetch: {e}"), None))?;

        // Enrich servers via process_servers_inner (GeoIP, TCP ping, CDN detection)
        let cache = {
            let state = self.state.lock().await;
            state.settings_mgr.save(&settings).ok(); // ensure settings persisted
            state.cache.clone() // clone Arc before releasing lock
        }; // lock released
        let enriched = crate::profile::process_servers_inner(
            &data_dir,
            &settings,
            servers,
            id,
            Some(&cache),
            None, // no Tauri AppHandle — skip events
        )
        .await;

        // Phase 3: write lock — commit enriched servers
        let count = {
            let mut state = self.state.lock().await;
            let idx = state.profile_mgr.list().iter()
                .position(|p| p.id == id)
                .ok_or_else(|| ErrorData::invalid_params(format!("Profile '{id}' not found"), None))?;
            state.profile_mgr.list_mut()[idx].servers = enriched;
            state.profile_mgr.list_mut()[idx].updated_at = chrono::Utc::now().to_rfc3339();
            let count = state.profile_mgr.list()[idx].servers.len();
            state.profile_mgr.save()
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            count
        }; // lock released

        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!("Subscription refreshed: {count} server(s)"))
        ]))
    }

    async fn cmd_remove_sub(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let id = args.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'id' is required", None))?;

        let mut state = self.state.lock().await;
        let len_before = state.profile_mgr.list().len();
        state.profile_mgr.list_mut().retain(|p| p.id != id);
        let removed = len_before.saturating_sub(state.profile_mgr.list().len());
        if removed == 0 {
            return Err(ErrorData::invalid_params(format!("Profile '{id}' not found"), None));
        }
        state.profile_mgr.save()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!("Removed {removed} profile(s)"))]))
    }

    async fn cmd_start_server(&self, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        let sub_id = args.get("subscription_id").and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'subscription_id' is required", None))?;
        let server_id = args.get("server_id").and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("'server_id' is required", None))?;
        let proxy_port = args.get("proxy_port").and_then(|v| v.as_u64()).unwrap_or(5050) as u16;
        let routing_mode = args.get("routing_mode").and_then(|v| v.as_str()).unwrap_or("system-proxy");

        let state = self.state.lock().await;
        let profile = state.profile_mgr.list().iter()
            .find(|p| p.id == sub_id)
            .ok_or_else(|| ErrorData::invalid_params(format!("Subscription '{sub_id}' not found"), None))?;
        let server = profile.servers.iter()
            .find(|s| s.id == server_id)
            .ok_or_else(|| ErrorData::invalid_params(format!("Server '{server_id}' not found"), None))?;

        let pid = launch_sing_box(&state, proxy_port, routing_mode, &[server.clone()]).await?;
        state.process_mgr.set_pid(pid);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!("Proxy started (PID {pid}) via server '{}' on port {proxy_port}", server.server))
        ]))
    }

    async fn cmd_settings(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.state.lock().await;
        let s = state.settings_mgr.load().unwrap_or_default();
        let j = serde_json::to_string_pretty(&s).unwrap_or_else(|_| "{}".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(j)]))
    }
}

// ── Tool definitions ─────────────────────────────────────────────────────

fn define_tools() -> Vec<Tool> {
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
                    "proxy_port": { "type": "integer", "description": "Local SOCKS5 port", "default": 5050, "minimum": 1024, "maximum": 65535 },
                    "routing_mode": { "type": "string", "description": "Routing mode", "enum": ["system-proxy","local-proxy"], "default": "system-proxy" }
                },
                "required": ["server", "port"]
            }))
        ),
        Tool::new("stop_proxy",
            "Stop the running sing-box proxy.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ),
        Tool::new("get_status",
            "Get current proxy connection status (connected/disconnected) and PID.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ),
        Tool::new("list_profiles",
            "List all subscription profiles with their servers.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ),
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
        ),
        Tool::new("refresh_subscription",
            "Fetch and parse a subscription URL, replacing its server list.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Profile ID to refresh" }
                },
                "required": ["id"]
            }))
        ),
        Tool::new("remove_subscription",
            "Remove a subscription profile by its ID.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Profile ID to remove" }
                },
                "required": ["id"]
            }))
        ),
        Tool::new("start_server_from_subscription",
            "Start proxy using a specific server from a subscription profile.",
            schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "subscription_id": { "type": "string", "description": "Subscription profile ID" },
                    "server_id":       { "type": "string", "description": "Server ID within the subscription" },
                    "proxy_port":      { "type": "integer", "description": "Local SOCKS5 port", "default": 5050, "minimum": 1024 },
                    "routing_mode":    { "type": "string", "description": "Routing mode", "enum": ["system-proxy","local-proxy"], "default": "system-proxy" }
                },
                "required": ["subscription_id", "server_id"]
            }))
        ),
        Tool::new("get_settings",
            "Get current application settings (theme, proxy mode, etc.)",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ),
    ]
}

// ── ServerHandler implementation ─────────────────────────────────────────

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerInfo {
        let mut tc = ToolsCapability::default();
        tc.list_changed = Some(true);
        let mut caps = ServerCapabilities::default();
        caps.tools = Some(tc);
        let mut si = ServerInfo::default();
        si.protocol_version = model::ProtocolVersion::V_2025_03_26;
        si.capabilities = caps;
        let mut impl_ = Implementation::default();
        impl_.name = "OmniTun MCP".into();
        impl_.version = "0.1.0".into();
        si.server_info = impl_;
        si
    }

    fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        async {
            let mut r = ListToolsResult::default();
            r.tools = define_tools();
            Ok(r)
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, ErrorData>> + MaybeSendFuture + '_ {
        async move {
            let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
            match request.name.as_ref() {
                "start_proxy"                 => self.cmd_start(args).await,
                "stop_proxy"                  => self.cmd_stop().await,
                "get_status"                  => self.cmd_status().await,
                "list_profiles"               => self.cmd_profiles().await,
                "add_subscription"            => self.cmd_add_sub(args).await,
                "refresh_subscription"        => self.cmd_refresh_sub(args).await,
                "remove_subscription"         => self.cmd_remove_sub(args).await,
                "start_server_from_subscription" => self.cmd_start_server(args).await,
                "get_settings"                => self.cmd_settings().await,
                n => Err(ErrorData::new(rmcp::model::ErrorCode::METHOD_NOT_FOUND, format!("Unknown tool: {n}"), None)),
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

        let handler = Handler { state: Arc::new(tokio::sync::Mutex::new(state)) };
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
