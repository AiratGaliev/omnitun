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

    log_to_logger(&*logger, LogLevel::Info, LogSource::System,
        format!("MCP server starting (data_dir: {:?})", data_dir));

    Ok(ServerState {
        process_mgr, profile_mgr, settings_mgr, bin_path, data_dir, _logger: logger,
    })
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

        let profile = ServerProfile {
            id: uuid::Uuid::new_v4().to_string(),
            protocol: args.get("protocol").and_then(|v| v.as_str()).unwrap_or("vless").into(),
            server: server.into(),
            port,
            ..Default::default()
        };

        let state = self.state.lock().await;
        let (cfg, _) = crate::proxy::config::generate_config(
            &state.data_dir.join("config"), proxy_port, routing_mode, &[profile], &AppSettings::default(),
        ).map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let cpath = crate::proxy::config::write_config(&cfg, &state.data_dir.join("config"))
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let b = state.bin_path.to_str().unwrap_or("");
        let c = cpath.to_str().unwrap_or("");
        let child = std::process::Command::new(b)
            .args(["run", "-c", c])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| ErrorData::internal_error(format!("spawn: {e}"), None))?;

        state.process_mgr.set_pid(child.id());
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!("Proxy started (PID {}) on port {}", child.id(), proxy_port))
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
                    "protocol":   { "type": "string", "description": "Protocol (vless, vmess, trojan, shadowsocks, hysteria2, tuic)", "enum": ["vless","vmess","trojan","shadowsocks","hysteria2","tuic"], "default": "vless" },
                    "uuid":       { "type": "string", "description": "VLESS/VMESS/Trojan UUID or password (optional if in profile)" },
                    "proxy_port": { "type": "integer", "description": "Local SOCKS5 port", "default": 5050, "minimum": 1024, "maximum": 65535 },
                    "routing_mode": { "type": "string", "description": "Routing mode", "enum": ["system-proxy","local-proxy"], "default": "system-proxy" }
                },
                "required": ["server", "port"]
            }))
        ),
        Tool::new("stop_proxy",
            "Stop the running sing-box proxy. No parameters needed.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ),
        Tool::new("get_status",
            "Get current proxy connection status (connected/disconnected) and PID.",
            schema(serde_json::json!({"type":"object","properties":{}}))
        ),
        Tool::new("list_profiles",
            "List all subscription profiles with their metadata.",
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
                "start_proxy"       => self.cmd_start(args).await,
                "stop_proxy"        => self.cmd_stop().await,
                "get_status"        => self.cmd_status().await,
                "list_profiles"     => self.cmd_profiles().await,
                "add_subscription"  => self.cmd_add_sub(args).await,
                "remove_subscription" => self.cmd_remove_sub(args).await,
                "get_settings"      => self.cmd_settings().await,
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
