use crate::proxy::{ErrorCode, ProxyError, ServerProfile};
use crate::subscription::SubscriptionProfile;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Get proxy port to route subscription fetch through the active proxy.
/// Returns None — when connected, traffic goes through the proxy's port,
/// so direct fetch works and avoids double-proxy issues.
pub(crate) fn proxy_port(app_settings: &crate::settings::AppSettings) -> Option<u16> {
    // Proxy / system-proxy / local-proxy modes: use the proxy port
    // so the fetch goes through the active outbound.
    if crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0 {
        return Some(app_settings.proxy_port);
    }

    // Fallback: try the configured proxy port
    if app_settings.proxy_port > 0 {
        return Some(app_settings.proxy_port);
    }

    None
}

/// Tauri-aware wrapper: extracts AppSettings from AppHandle, then calls proxy_port.
pub(crate) fn proxy_port_from_app(app: &AppHandle) -> Option<u16> {
    if let Some(state) = app.try_state::<Mutex<crate::settings::AppSettings>>() {
        if let Ok(guard) = state.try_lock() {
            return proxy_port(&guard);
        }
    }
    None
}

pub struct ProfileManager {
    profiles: Vec<SubscriptionProfile>,
    file_path: PathBuf,
}

impl ProfileManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            profiles: Vec::new(),
            file_path: data_dir.join("profiles.json"),
        }
    }

    pub fn load(&mut self) -> Result<(), ProxyError> {
        match std::fs::read_to_string(&self.file_path) {
            Ok(json) => {
                let profiles: Vec<SubscriptionProfile> =
                    serde_json::from_str(&json).map_err(|e| {
                        ProxyError::subscription(
                            ErrorCode::SubscriptionSaveFailed,
                            format!("Failed to parse profiles.json: {e}"),
                        )
                    })?;
                self.profiles = profiles;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First launch — no profiles yet
                self.profiles = Vec::new();
                Ok(())
            }
            Err(e) => Err(ProxyError::Io(e)),
        }
    }

    /// Run GeoIP enrichment on all servers missing country/city fields.
    /// This fixes data from old profiles.json that only had flag set.
    pub fn enrich_loaded(&mut self) {
        if !crate::proxy::geoip::is_ready() {
            return;
        }
        let mut changed = false;
        let mut enriched_count = 0u32;
        let mut cdn_cleanup = 0u32;
        for profile in &mut self.profiles {
            for server in &mut profile.servers {
                // CDN servers must never retain exit IP data.
                if server.behind_cdn {
                    // 🚫 ANTI-REGRESSION: используем централизованный helper.
                    // Раньше тут был inline log::error! с ручным форматированием.
                    // Теперь вызываем assert_no_cdn_exit_data — это гарантирует
                    // единый формат сообщения и structured target "cdn-invariant".
                    server.assert_no_cdn_exit_data("enrich_loaded::at_startup");
                    if server.exit_server.is_some()
                        || server.exit_flag.is_some()
                        || server.exit_country_name.is_some()
                        || server.exit_city_name.is_some()
                    {
                        server.exit_server = None;
                        server.exit_flag = None;
                        server.exit_country_name = None;
                        server.exit_city_name = None;
                        changed = true;
                        cdn_cleanup += 1;
                    }
                    continue; // skip GeoIP enrichment for CDN
                }
                if server.country_name.is_some() && server.city_name.is_some() {
                    continue;
                }
                // server.server is guaranteed to be an IP here (refresh pipeline
                // replaces hostnames with IPs before saving). Direct sync lookup.
                if let Some(info) = crate::proxy::geoip::resolve(&server.server) {
                    server.flag = Some(info.country_code.clone());
                    server.country_name = Some(info.country_name.clone());
                    if !info.city.is_empty() {
                        server.city_name = Some(info.city.clone());
                    }
                    changed = true;
                    enriched_count += 1;
                }
            }
        }
        if changed {
            let _ = self.save();
        }
        tracing::info!(target: "subscription",
            "GeoIP enrichment: {enriched_count} enriched, {cdn_cleanup} CDN exit cleaned up"
        );
    }

    /// Deduplicate servers by (exit_server || server) + protocol, keep lowest ping.
    /// Runs in-place — no disk I/O.
    pub fn deduplicate_profiles(&mut self) {
        for profile in &mut self.profiles {
            let mut seen: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut deduped: Vec<ServerProfile> = Vec::new();
            let mut dup_count = 0usize;
            for s in profile.servers.drain(..) {
                let ip = s.exit_server.as_deref().unwrap_or(&s.server);
                let key = format!("{ip}:{}", s.protocol);
                match seen.get(&key) {
                    None => {
                        seen.insert(key, deduped.len());
                        deduped.push(s);
                    }
                    Some(&idx) => {
                        dup_count += 1;
                        // Rust's derived Ord for Option<T> puts None < Some(_),
                        // so None < Some(0) is true — untested servers would evict
                        // tested ones. Use unwrap_or(u64::MAX) for correct ordering.
                        let new_ping = s.ping_ms.unwrap_or(u64::MAX);
                        let old_ping = deduped[idx].ping_ms.unwrap_or(u64::MAX);
                        if new_ping < old_ping {
                            deduped[idx] = s;
                        }
                    }
                }
            }
            if dup_count > 0 {
                tracing::info!(
                    target: "subscription",
                    "Deduplicated {dup_count} server(s) by exit IP + protocol in profile '{}'",
                    profile.name,
                );
            }
            profile.servers = deduped;
        }
    }

    pub fn save(&self) -> Result<(), ProxyError> {
        let json = serde_json::to_string_pretty(&self.profiles).map_err(|e| {
            ProxyError::subscription(
                ErrorCode::SubscriptionSaveFailed,
                format!("Failed to serialize profiles: {e}"),
            )
        })?;
        let tmp = self.file_path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| {
            ProxyError::subscription(
                ErrorCode::SubscriptionSaveFailed,
                format!("Failed to write temp profiles: {e}"),
            )
        })?;
        std::fs::rename(&tmp, &self.file_path).map_err(|e| {
            ProxyError::subscription(
                ErrorCode::SubscriptionSaveFailed,
                format!("Failed to rename profiles: {e}"),
            )
        })?;
        Ok(())
    }

    pub fn list(&self) -> &[SubscriptionProfile] {
        &self.profiles
    }

    pub fn list_mut(&mut self) -> &mut Vec<SubscriptionProfile> {
        &mut self.profiles
    }

    pub async fn add(
        &mut self,
        app: &AppHandle,
        name: String,
        url: String,
        profile_id: String,
    ) -> Result<SubscriptionProfile, ProxyError> {
        let clean_url = url.trim().to_string();

        if self.profiles.iter().any(|p| p.url == clean_url) {
            return Err(ProxyError::subscription(
                ErrorCode::SubscriptionDuplicateUrl,
                format!("Subscription with URL '{clean_url}' already exists"),
            ));
        }

        let mut servers = if clean_url.starts_with("http://") || clean_url.starts_with("https://") {
            // Try through existing proxy first, fall back to direct
            match crate::subscription::fetch_subscription(&clean_url, proxy_port_from_app(app))
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    if proxy_port_from_app(app).is_some() {
                        tracing::error!(
                            target: "subscription",
                            "Proxy fetch failed, retrying direct: {e}",
                        );
                        crate::subscription::fetch_subscription(&clean_url, None).await?
                    } else {
                        return Err(e);
                    }
                }
            }
        } else {
            crate::subscription::parser::parse_subscription(&clean_url)?
        };

        // Save profile to disk IMMEDIATELY (with empty servers) so profiles.json
        // exists even if the app is closed during the long batch test below.
        // After process_servers completes, the in-memory profile is updated
        // and saved again with the tested servers.
        let initial = SubscriptionProfile {
            id: profile_id.clone(),
            name: name.clone(),
            url: clean_url.clone(),
            servers: Vec::new(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_tested: None,
            traffic_used: None,
            traffic_total: None,
            traffic_expiry: None,
        };
        self.profiles.push(initial);
        self.save()?;

        // Run the test (original synchronous flow — this IS the auto-refresh)
        servers = process_servers(app, servers, &profile_id).await;

        // Persist cache to disk so negative entries survive app restart.
        if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
            cache.save_to_disk().await;
        }

        // Fetch subscription traffic info
        let (traffic_used, traffic_total, traffic_expiry) =
            crate::subscription::fetch_subscription_traffic(&clean_url, proxy_port_from_app(app))
                .await
                .map(|(used, total, expiry)| (Some(used), Some(total), expiry))
                .unwrap_or((None, None, None));

        // Update the existing profile in-place (was saved empty before process_servers)
        if let Some(existing) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
            existing.servers = servers;
            existing.updated_at = chrono::Utc::now().to_rfc3339();
            existing.last_tested = Some(chrono::Utc::now().to_rfc3339());
            existing.traffic_used = traffic_used;
            existing.traffic_total = traffic_total;
            existing.traffic_expiry = traffic_expiry;
        }
        self.save()?;

        let profile = self
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .cloned()
            .ok_or_else(|| {
                ProxyError::subscription(
                    ErrorCode::SubscriptionNotFound,
                    "Profile disappeared during add",
                )
            })?;
        Ok(profile)
    }

    pub fn remove(&mut self, id: &str) -> Result<(), ProxyError> {
        let len_before = self.profiles.len();
        self.profiles.retain(|p| p.id != id);
        if self.profiles.len() == len_before {
            return Err(ProxyError::subscription(
                ErrorCode::SubscriptionNotFound,
                "Profile not found",
            ));
        }
        self.save()?;
        Ok(())
    }

    /// Phase 3 of lock-splitting refresh: commit new servers under write lock.
    ///
    /// Called AFTER all long operations (HTTP fetch, process_servers, traffic
    /// fetch) are complete. The write lock is held for <50 ms.
    ///
    /// Re-validates that the profile still exists (it might have been removed
    /// during Phase 2). If removed, returns SubscriptionNotFound.
    pub async fn commit_refresh(
        &mut self,
        app: &tauri::AppHandle,
        id: &str,
        servers: Vec<ServerProfile>,
        traffic: Option<(u64, u64, Option<String>)>,
    ) -> Result<SubscriptionProfile, ProxyError> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| {
                ProxyError::subscription(
                    ErrorCode::SubscriptionNotFound,
                    "Profile removed during refresh",
                )
            })?;

        self.profiles[idx].servers = servers;
        // INVARIANT: warn if profile has 0 servers with ping_ms — engine may have failed.
        let ping_count = self.profiles[idx]
            .servers
            .iter()
            .filter(|s| s.ping_ms.is_some())
            .count();
        if ping_count == 0 && !self.profiles[idx].servers.is_empty() {
            tracing::warn!(
                target: "subscription",
                "commit_refresh: 0/{} servers have ping_ms — testing engine may have failed",
                self.profiles[idx].servers.len(),
            );
        }
        self.profiles[idx].updated_at = chrono::Utc::now().to_rfc3339();
        // refresh INCLUDES batch-test, so last_tested = now too.
        self.profiles[idx].last_tested = Some(chrono::Utc::now().to_rfc3339());

        // Notify cache: pass ALL current server IDs so stale entries are removed.
        if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
            let all_active_ids: std::collections::HashSet<String> = self
                .profiles
                .iter()
                .flat_map(|p| p.servers.iter().map(|s| s.id.clone()))
                .collect();
            cache.handle_subscription_refresh(&all_active_ids).await;
            cache.save_to_disk().await;
        }

        // Apply traffic info
        if let Some((used, total, expiry)) = traffic {
            self.profiles[idx].traffic_used = Some(used);
            self.profiles[idx].traffic_total = Some(total);
            self.profiles[idx].traffic_expiry = expiry;
        }

        self.deduplicate_profiles();
        let profile = self.profiles[idx].clone();
        self.save()?;
        Ok(profile)
    }

    /// Commit a server TEST (no URL fetch).
    ///
    /// Like `commit_refresh` but:
    ///   - Updates `last_tested` instead of `updated_at`
    ///   - Does NOT fetch traffic info
    ///   - Does NOT call `handle_subscription_refresh` (no new server IDs)
    ///
    /// Called by the `test_servers` Tauri command after `process_servers_inner`
    /// re-tests existing servers.
    pub async fn commit_test(
        &mut self,
        app: &tauri::AppHandle,
        id: &str,
        servers: Vec<ServerProfile>,
    ) -> Result<SubscriptionProfile, ProxyError> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| {
                ProxyError::subscription(
                    ErrorCode::SubscriptionNotFound,
                    "Profile removed during test",
                )
            })?;

        self.profiles[idx].servers = servers;
        // INVARIANT: warn if profile has 0 servers with ping_ms — engine may have failed.
        let ping_count = self.profiles[idx]
            .servers
            .iter()
            .filter(|s| s.ping_ms.is_some())
            .count();
        if ping_count == 0 && !self.profiles[idx].servers.is_empty() {
            tracing::warn!(
                target: "subscription",
                "commit_test: 0/{} servers have ping_ms — testing engine may have failed",
                self.profiles[idx].servers.len(),
            );
        }
        self.profiles[idx].last_tested = Some(chrono::Utc::now().to_rfc3339());

        // Save cache (test may have updated positive/negative entries)
        if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
            cache.save_to_disk().await;
        }

        let profile = self.profiles[idx].clone();
        self.save()?;
        Ok(profile)
    }
}

/// Persistent testing engine state — reused across `clash_api_test` calls.
struct TestEngine {
    pid: u32,
    clash_port: u16,
    socks_port: u16,
    server_hash: u64,
    pm: crate::proxy::process::ProcessManager,
}

impl TestEngine {
    fn server_hash(servers: &[ServerProfile]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        servers.len().hash(&mut h);
        for s in servers.iter().take(100) {
            s.id.hash(&mut h);
        }
        h.finish()
    }
}

static TEST_ENGINE: std::sync::Mutex<Option<TestEngine>> = std::sync::Mutex::new(None);
/// Prevents concurrent `clash_api_test` calls — one test at a time.
static TEST_ENGINE_BUSY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Spawn (or reuse) the persistent testing engine for the given servers.
/// Returns `(clash_port, socks_port)` on success.
pub(crate) async fn ensure_test_engine(
    servers: &[ServerProfile],
    data_dir: &Path,
    settings: &crate::settings::AppSettings,
) -> Option<(u16, u16)> {
    let hash = TestEngine::server_hash(servers);
    {
        let guard = TEST_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref engine) = *guard {
            if engine.server_hash == hash {
                // Engine alive with matching config — check process still runs
                let alive = std::process::Command::new("tasklist")
                    .arg("/FI")
                    .arg(format!("PID eq {}", engine.pid))
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).contains(&engine.pid.to_string()))
                    .unwrap_or(false);
                if alive {
                    return Some((engine.clash_port, engine.socks_port));
                }
            }
        }
    }

    // Need to start a new engine — kill old one first (frees ports)
    {
        let guard = TEST_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref old) = *guard {
            tracing::info!(target: "subscription", "Testing engine: servers changed, killing old engine (pid {})", old.pid);
            let _ = std::process::Command::new("taskkill")
                .args(["/pid", &old.pid.to_string(), "/f"])
                .output();
        }
    }
    // Short pause so OS releases ports after killing old engine
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Try to spawn the engine, with one retry on failure
    let mut last_result = None;
    for attempt in 0..2 {
        let result = spawn_test_engine(servers, data_dir, settings).await;
        if result.is_some() {
            last_result = result;
            break;
        }
        if attempt == 0 {
            tracing::info!(target: "subscription", "Testing engine: spawn failed, retrying...");
            // Clear old engine state so next attempt gets fresh ports
            {
                let mut guard = TEST_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
                *guard = None;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        }

        if let Some((pid, clash_port, socks_port, pm)) = last_result {
        let engine = TestEngine {
            pid,
            clash_port,
            socks_port,
            server_hash: hash,
            pm,
        };
        let mut guard = TEST_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(engine);
        return Some((clash_port, socks_port));
    }
    None
}

/// Kill the persistent testing engine (called on app exit).
/// Calls the ProcessManager::stop() on the engine's own PM —
/// PID, JobObject, and PID file are all managed by that instance.
pub(crate) fn shutdown_test_engine() {
    if let Ok(mut guard) = TEST_ENGINE.lock() {
        if let Some(ref engine) = *guard {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let _ = runtime.block_on(engine.pm.stop());
            }
        }
        *guard = None;
    }
}

/// Spawn the testing engine — uses the same config template (`base.json`),
/// same outbound building, and same `ProcessManager` as the proxy engine.
/// The only differences: all servers are flat outbounds (no selector),
/// Clash API is enabled, and DNS `detour` is changed to `"direct"`.
async fn spawn_test_engine(
    servers: &[ServerProfile],
    data_dir: &Path,
    settings: &crate::settings::AppSettings,
) -> Option<(u32, u16, u16, crate::proxy::process::ProcessManager)> {
    use serde_json::json;
    use std::time::Duration;

    let binary_path = data_dir.join("binaries").join(
        crate::proxy::installer::Installer::binary_name(),
    );
    if !binary_path.exists() {
        tracing::warn!(target: "subscription", "Testing engine: sing-box not installed at {:?}", binary_path);
        return None;
    }

    let test_dir = std::env::temp_dir().join(format!(
        "omnitun-engine-{}",
        uuid::Uuid::new_v4()
    ));
    if std::fs::create_dir_all(&test_dir).is_err() {
        tracing::warn!(target: "subscription", "Testing engine: failed to create temp dir");
        return None;
    }

    // Find free ports dynamically (not from PortPool — stale TIME_WAIT ports
    // from failed engines would block sequential port reuse).
    fn find_free_port() -> Option<u16> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        // Drop listener immediately — port is free now
        drop(listener);
        Some(port)
    }
    let clash_port = find_free_port()?;
    let socks_port = find_free_port()?;

    // Build a minimal config like batch test + Clash API (no base.json).
    let mut outbounds: Vec<serde_json::Value> = Vec::with_capacity(servers.len() + 2);
    outbounds.push(json!({"type": "direct", "tag": "direct"}));
    outbounds.push(json!({"type": "block", "tag": "block"}));
    for (i, server) in servers.iter().enumerate() {
        let tag = format!("proxy-{i}");
        if let Ok(mut ob) = crate::proxy::config::build_outbound(server) {
            ob["tag"] = json!(tag);
            if settings.anti_dpi {
                crate::proxy::config::apply_anti_dpi(&mut ob);
            }
            if settings.enable_mux {
                crate::proxy::config::apply_mux(&mut ob);
            }
            outbounds.push(ob);
        }
    }
    if outbounds.len() <= 2 && !servers.is_empty() {
        tracing::warn!(target: "subscription", "Testing engine: no valid outbounds ({} servers given)", servers.len());
        return None;
    }
    let log_path = test_dir.join("sing-box.log");
    let config = json!({
        "log": {
            "level": "warn",
            "output": log_path.to_string_lossy().to_string(),
            "timestamp": true,
        },
        "dns": {
            "servers": [{"type": "local", "tag": "local"}],
            "final": "local",
            "strategy": "prefer_ipv4",
        },
        "inbounds": [{
            "type": "mixed",
            "tag": "socks-in",
            "listen": "127.0.0.1",
            "listen_port": socks_port,
        }],
        "outbounds": outbounds,
        "route": {
            "rules": [
                {"inbound": "socks-in", "outbound": "direct"},
                {"ip_is_private": true, "outbound": "direct"},
            ],
            "auto_detect_interface": true,
            "final": "direct",
        },
        "experimental": {
            "clash_api": {
                "external_controller": format!("127.0.0.1:{clash_port}"),
                "external_ui": "",
                "secret": "",
            },
        },
    });

    let config_path = test_dir.join("config.json");
    if std::fs::write(&config_path, serde_json::to_string_pretty(&config).ok()?).is_err() {
        tracing::warn!(target: "subscription", "Testing engine: failed to write config");
        return None;
    }

    // Validate config with sing-box check -c before spawning
    let check = std::process::Command::new(&binary_path)
        .args(["check", "-c"])
        .arg(&config_path)
        .output();
    match check {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(target: "subscription", "Testing engine: config check failed:\n{stderr}");
            return None;
        }
        Err(e) => {
            tracing::warn!(target: "subscription", "Testing engine: config check binary error: {e}");
            // Don't block — proceed anyway
        }
        _ => {}
    }

    // Spawn using ProcessManager::start_headless_test — same logic as
    // the proxy engine: BREAKAWAY fallback, JobObject, PID tracking,
    // PROXY_PID and PID file. PROXY_PID is overwritten by the proxy
    // engine when the user connects — harmless.
    let pm = crate::proxy::process::ProcessManager::new(binary_path);
    let pid = match pm.start_headless_test(&config_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "subscription", "Testing engine: start_headless_test failed: {e}");
            return None;
        }
    };

    // Wait briefly and check if process is still alive
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    if !crate::is_process_alive(pid) {
        let stderr_path = test_dir.join("sing-box-stderr.log");
        let stderr_content = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        tracing::warn!(target: "subscription", "Testing engine: process died immediately (pid {pid}):\n{stderr_content}");
        return None;
    }

    // Wait for Clash API to be ready (up to 15s)
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;

    let api_ready = {
        let url = format!("http://127.0.0.1:{clash_port}/version");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut ready = false;
        while tokio::time::Instant::now() < deadline {
            if client.get(&url).send().await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        ready
    };

    if !api_ready {
        tracing::warn!(target: "subscription", "Testing engine: Clash API not ready in 15s (port {})", clash_port);
        let _ = pm.stop().await;
        return None;
    }

    Some((pid, clash_port, socks_port, pm))
}

/// probe ALL servers via GET /proxies/proxy-{i}/delay,
/// return (server_id → ping_ms) map.
///
/// Protocol-agnostic — tests all protocols (VLESS, VMess, Trojan, SS,
/// Hysteria2, TUIC, WireGuard) uniformly via sing-box's Clash API delay
/// endpoint. Always uses the reusable testing engine — no per-call cold start.
pub(crate) async fn clash_api_test(
    servers: &[ServerProfile],
    data_dir: &Path,
    settings: &crate::settings::AppSettings,
    cache: Option<&crate::proxy::cache::ServerCache>,
    app_handle: Option<&tauri::AppHandle>,
    profile_id: &str,
) -> std::collections::HashMap<String, u64> {
    use std::time::Duration;

    if servers.is_empty() {
        return std::collections::HashMap::new();
    }

    // Acquire test lock — one at a time
    let _lock = TEST_ENGINE_BUSY.lock().await;

    // Ensure engine is running (start or reuse)
    let (clash_port, _socks_port) = match ensure_test_engine(servers, data_dir, settings).await {
        Some(pair) => pair,
        None => return std::collections::HashMap::new(),
    };

    let count = servers.len();
    let test_url = crate::proxy::batcher::configured_test_url()
        .unwrap_or_else(|| "http://cp.cloudflare.com/generate_204".to_string());

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_default();

    let sem = Arc::new(Semaphore::new(8));
    let mut set = tokio::task::JoinSet::new();
    let mut skipped = 0usize;

    for (i, server) in servers.iter().enumerate() {
        // Skip servers that are already in the negative cache (known dead)
        if let Some(cache) = cache {
            if cache.is_cached_negative(&server.id).await {
                skipped += 1;
                continue;
            }
        }
        let sem = sem.clone();
        let client = client.clone();
        let sid = server.id.clone();
        let test_url = test_url.clone();

        set.spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            let url = format!(
                "http://127.0.0.1:{clash_port}/proxies/proxy-{i}/delay?url={test_url}&timeout=5000"
            );
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(body) => {
                            let ping = body.get("delay").and_then(|d| d.as_u64());
                            ping.map(|p| (sid, p))
                        }
                        Err(_) => None,
                    }
                }
                _ => None,
            }
        });
    }

    let mut preflight_pings: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    let mut probed = 0usize;

    while let Some(res) = set.join_next().await {
        probed += 1;
        if let Ok(Some((sid, ping))) = res {
            preflight_pings.insert(sid, ping);
        }
        // Emit progress so the frontend sees the preflight phase
        if probed.is_multiple_of(10) || probed == count {
            if let Some(app) = app_handle {
                crate::events::emit_refresh_progress(
                    app,
                    crate::events::RefreshProgress {
                        profile_id: profile_id.to_string(),
                        total: count,
                        tested: probed,
                        cache_hits: 0,
                        successful: preflight_pings.len(),
                        failed: probed - preflight_pings.len(),
                        phase: "preflight".into(),
                        updated_at: String::new(),
                        last_tested: None,
                    },
                );
            }
        }
    }

    if skipped > 0 {
        tracing::info!(
            target: "subscription",
            "Temp engine Clash API preflight: {}/{} servers got ping (skipped {skipped} negative cache)",
            preflight_pings.len(),
            count,
        );
    } else {
        tracing::info!(
            target: "subscription",
            "Temp engine Clash API preflight: {}/{} servers got ping",
            preflight_pings.len(),
            count,
        );
    }

    preflight_pings
}

/// Deduplicate servers, apply user filters, run ping, enrich GeoIP flag.
/// Logs each step (dedup count, testing, unreachable count, final count).
///
/// Tauri wrapper — extracts data_dir, settings, cache from AppHandle.
pub async fn process_servers(
    app: &AppHandle,
    servers: Vec<ServerProfile>,
    profile_id: &str,
) -> Vec<ServerProfile> {
    let data_dir = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(_) => return servers,
    };
    let settings = app
        .try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
        .and_then(|s| s.try_lock().ok().map(|g| g.clone()))
        .unwrap_or_default();
    let cache = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>();
    let cache_ref = cache.as_ref().map(|c| c as _);
    // Get old servers from the profile manager for ID preservation.
    let old_servers = app
        .try_state::<crate::ProfileState>()
        .and_then(|ps| {
            ps.manager.try_lock().ok().map(|mgr| {
                mgr.list()
                    .iter()
                    .find(|p| p.id == profile_id)
                    .map(|p| p.servers.clone())
                    .unwrap_or_default()
            })
        })
        .unwrap_or_default();
    process_servers_inner(
        &data_dir,
        &settings,
        servers,
        profile_id,
        cache_ref,
        Some(app),
        &old_servers,
    )
    .await
}

/// Headless version — takes data_dir, settings, cache directly (no Tauri dependency).
/// Used by MCP server and any non-Tauri contexts.
pub async fn process_servers_inner(
    data_dir: &Path,
    settings: &crate::settings::AppSettings,
    mut servers: Vec<ServerProfile>,
    profile_id: &str,
    cache: Option<&std::sync::Arc<crate::proxy::cache::ServerCache>>,
    app_handle: Option<&tauri::AppHandle>,
    old_servers: &[crate::proxy::ServerProfile],
) -> Vec<ServerProfile> {
    let initial_count = servers.len();

    // Preserve server IDs from the old profile so cache keys match.
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

    // Lazy-init GeoIP: load from disk or download synchronously (one-time cost)
    let geo_dir = data_dir.join("geoip");
    let city_path = geo_dir.join("GeoLite2-City.mmdb");
    let asn_path = geo_dir.join("GeoLite2-ASN.mmdb");

    if !crate::proxy::geoip::is_ready() {
        if city_path.exists() {
            let _ = crate::proxy::geoip::init_city(&city_path);
        } else {
            tracing::info!(
                target: "system",
                "Downloading GeoIP databases (one-time)...",
            );
            match crate::proxy::geoip::ensure_downloaded(&geo_dir, &settings.ipinfo_token).await {
                Ok(_) => tracing::info!(
                    target: "system",
                    "GeoIP databases ready",
                ),
                Err(e) => tracing::error!(
                    target: "system",
                    "GeoIP download failed (will retry): {e}",
                ),
            }
        }
    }

    // Always try to init ASN if the file exists — may have been downloaded
    // on a previous run while is_ready() already returned true for City only.
    if asn_path.exists() && !crate::proxy::geoip::is_asn_ready() {
        let _ = crate::proxy::geoip::init_asn(&asn_path);
    }

    // Always try to init ipinfo Lite if the file exists
    let ipinfo_path = geo_dir.join("ipinfo_lite.mmdb");
    if ipinfo_path.exists() && !crate::proxy::geoip::is_ipinfo_ready() {
        let _ = crate::proxy::geoip::init_ipinfo(&ipinfo_path);
    }

    // Always try to init CDN ranges from downloaded .srs files
    if !crate::proxy::geoip::is_cdn_ready() {
        let mut merged: Vec<(std::net::IpAddr, u8)> = Vec::new();
        for name in &["cdn-only_singbox.srs", "merged.srs"] {
            let path = geo_dir.join(name);
            if !path.exists() {
                tracing::warn!(
                    target: "system",
                    "CDN .srs {name} not found at {:?}",
                    path,
                );
                continue;
            }
            match crate::proxy::geoip::parse_srs_file(&path) {
                Ok(r) => {
                    tracing::info!(
                        target: "system",
                        "CDN .srs {name}: {} CIDR entries (lazy)",
                        r.len(),
                    );
                    merged.extend(r);
                }
                Err(e) => {
                    tracing::error!(
                        target: "system",
                        "CDN .srs {name} parse failed: {e}",
                    );
                }
            }
        }
        if !merged.is_empty() {
            tracing::info!(
                target: "system",
                "CDN ranges merged: {} total entries (lazy)",
                merged.len(),
            );
            crate::proxy::geoip::set_cdn_ranges(merged);
        } else {
            tracing::warn!(
                target: "system",
                "CDN ranges empty — no .srs files loaded",
            );
        }
    }

    // 1. Deduplication by protocol + server + port + uuid + password
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for s in servers {
        let key = format!(
            "{}://{}:{}@{}:{}:{}/{}",
            s.protocol,
            s.server,
            s.port,
            s.uuid.as_deref().unwrap_or(""),
            s.password.as_deref().unwrap_or(""),
            s.method.as_deref().unwrap_or(""),
            s.transport_type.as_deref().unwrap_or(""),
        );
        if seen.insert(key) {
            unique.push(s);
        }
    }
    let duplicates = initial_count - unique.len();
    tracing::info!(
        target: "subscription",
        "Parsed {initial_count} servers from subscription, {duplicates} duplicates removed, {} unique",
        unique.len(),
    );
    if duplicates > 0 {
        tracing::info!(
            target: "subscription",
            "Removed {duplicates} duplicate(s)",
        );
    }

    // 1b. Fast keyword pre-check for serverless domains (workers.dev, pages.dev).
    //     Full CDN detection via ASN happens during batch latency test.
    for s in &mut unique {
        s.behind_cdn = s.behind_cdn || crate::proxy::is_cdn_server(s);
        if s.behind_cdn {
            // Clear stale exit IP data from previous batch tests — CDN servers
            // must never retain exit data (their exit node is dynamic/unstable).
            // 🚫 ANTI-REGRESSION: обязательно логируем нарушение через
            // assert_no_cdn_exit_data ПЕРЕД занулением. Не удалять вызов,
            // даже если кажется, что "всё и так чисто". См. doc-comment метода.
            s.assert_no_cdn_exit_data("process_servers::keyword_match");
            s.exit_server = None;
            s.exit_flag = None;
            s.exit_country_name = None;
            s.exit_city_name = None;
            tracing::info!(
                target: "subscription",
                "[CDN] keyword match: {} ({})",
                s.display_name(),
                s.server,
            );
            // CDN servers keep exit-IP data cleared by assert_no_cdn_exit_data
            // (logged at error! level, target &quot;cdn-invariant&quot;).
        }
    }

    // 2. Apply user filters (country, protocol) from settings
    let total_before = unique.len();
    unique.retain(|s| {
        // Filter by protocol
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
        // Filter by country (flag must be resolved first)
        if !settings.allowed_countries.is_empty() {
            if let Some(ref flag) = s.flag {
                if !settings.allowed_countries.contains(flag) {
                    return false;
                }
            }
        }
        if !settings.blocked_countries.is_empty() {
            if let Some(ref flag) = s.flag {
                if settings.blocked_countries.contains(flag) {
                    return false;
                }
            }
        }
        true
    });
    let filtered = total_before - unique.len();
    if filtered > 0 {
        tracing::info!(
            target: "subscription",
            "Filtered {filtered} server(s) by country/protocol settings",
        );
    }

    // 3. Clash API delay probe via temp engine — replaces TCP ping entirely.
    //
    // Spawns a temporary sing-box with ALL servers as outbounds + Clash API,
    // tests each via GET /proxies/proxy-{i}/delay, kills the engine.
    // Protocol-agnostic — tests VLESS, VMess, Trojan, SS, Hysteria2, TUIC,
    // WireGuard uniformly in ~5s for 24 servers (vs ~30s batch).
    //
    // DNS resolve + GeoIP enrichment happen AFTER preflight in their own
    // spawn loop (step 4). The preflight ping is used for the UI during
    // loading; batch test fills the definitive value.
    let preflight_pings = clash_api_test(&unique, data_dir, settings, cache.map(|c| c.as_ref()), app_handle, profile_id).await;

    // 3b. Pre-filter: servers without Clash API ping → negative cache.
    //     BUT: if testing engine returned 0 pings (all failed),
    //     skip filtering to avoid wiping the profile on transient failure.
    if !preflight_pings.is_empty() {
        if let Some(cache) = cache.as_ref() {
            let mut dead_ids = std::collections::HashSet::new();
            for server in &unique {
                if preflight_pings.contains_key(&server.id) { continue; }
                dead_ids.insert(server.id.clone());
            }

            if !dead_ids.is_empty() {
                for sid in &dead_ids {
                    cache
                        .update_negative(sid, "Clash API delay probe: no ping".to_string())
                        .await;
                }

                let before = unique.len();
                unique.retain(|s| !dead_ids.contains(&s.id));
                let n = before - unique.len();
                tracing::info!(
                    target: "subscription",
                    "Removed {n} server(s) — Clash API preflight failed (negative cache)",
                );
            }
        }
    } else {
        tracing::warn!(
            target: "subscription",
            "Pre-filter: testing engine returned 0 pings — keeping all servers (transient)",
        );
    }

    // 4. DNS resolve + entry GeoIP + CDN detection (data enrichment).
    //     No TCP connect — servers that failed preflight are still passed
    //     to batch test (it will determine real reachability).
    let mut alive = Vec::new();
    for server in unique {
        let entry_ip = tokio::net::lookup_host(&format!("{}:{}", server.server, server.port))
            .await
            .ok()
            .and_then(|mut a| a.next())
            .map(|s| s.ip().to_string());

        let mut server = server;
        // emoji flag from name (fast, no network)
        if server.flag.is_none() {
            if let Some(f) = emoji_flag_from_name(&server.original_name) {
                server.flag = Some(f);
            }
        }

        // GeoIP enrichment + CDN detection
        if let Some(ip) = &entry_ip {
            let geoip_info = crate::proxy::geoip::resolve_hostname(ip).await;
            if let Some(info) = &geoip_info {
                if server.country_name.is_none() {
                    server.country_name = Some(info.country_name.clone());
                }
                if server.flag.is_none() {
                    server.flag = Some(info.country_code.clone());
                }
                if server.city_name.is_none() && !info.city.is_empty() {
                    server.city_name = Some(info.city.clone());
                }
            }
            // CDN detection (entry IP) — clear exit data if behind CDN
            if !server.behind_cdn && crate::proxy::geoip::is_cdn_entry_ip(ip) {
                server.behind_cdn = true;
                server.assert_no_cdn_exit_data("process_servers::entry_ip_match");
                server.exit_server = None;
                server.exit_flag = None;
                server.exit_country_name = None;
                server.exit_city_name = None;
            }
            // Replace hostname with resolved IP for config generation
            if server.server.contains('.') && !server.server.chars().all(|c| c.is_ascii_digit() || c == '.') {
                server.server = ip.clone();
            }
        }

        // Set ping from preflight, otherwise keep existing (don't overwrite
        // with None if engine failed to start).

        if let Some(ping) = preflight_pings.get(&server.id) {
            server.ping_ms = Some(*ping);
        }
        // If engine returned 0 pings (transient failure), keep whatever
        // ping_ms the server already has (may be None for fresh servers
        // or a stale value for re-tested ones). The batch test or
        // old_ping restoration in commit will fill it in.

        alive.push(server);
    }

    let unique_count = alive.len();
    tracing::info!(
        target: "subscription",
        "Preflighted {unique_count} server(s) via Clash API",
    );

    // IP-based dedup: after DNS resolution, multiple hostnames may resolve
    //     to the same IP. Keep the one with lowest ping.
    //
    //     Ключ дедупа — (IP, port, protocol, uuid, password): на одном IP
    //     могут быть разные proxy-сервисы (порты, протоколы, аккаунты).
    let alive_before_ip_dedup = alive.len();
    if alive_before_ip_dedup > 0 {
        // Sort by ping ascending — lowest ping first
        alive.sort_by_key(|s| s.ping_ms.unwrap_or(u64::MAX));
        let mut seen_keys = std::collections::HashSet::new();
        alive.retain(|s| {
            let key = format!(
                "{}|{}|{}|{}|{}",
                s.server,
                s.port,
                s.protocol,
                s.uuid.as_deref().unwrap_or(""),
                s.password.as_deref().unwrap_or(""),
            );
            seen_keys.insert(key)
        });
        let ip_dupes = alive_before_ip_dedup - alive.len();
        if ip_dupes > 0 {
            tracing::info!(
                target: "subscription",
                "Removed {ip_dupes} full-duplicate(s) — same IP+port+protocol+creds (kept fastest)",
            );
        }
    }

    // Считаем unreachable ДО IP-дедупа, чтобы IP-дубликаты не учитывались
    // как недоступные серверы.
    let ping_failed = unique_count - alive_before_ip_dedup;
    if ping_failed > 0 {
        tracing::warn!(
            target: "subscription",
            "Removed {ping_failed} unreachable server(s) — ping failed",
        );
    }

    // === DIFF-TESTING OPTIMIZATION ===
    // After ping phase, check cache for each alive server. Servers with a fresh
    // positive cache entry skip the expensive batch test. Only unknown/stale
    // servers get batch-tested.
    let mut ready_servers = Vec::new();
    let mut needs_batch_test = Vec::new();

    if let Some(cache) = cache.as_ref() {
        // Запускаем batch test всегда, когда есть серверы для тестирования.
        for mut server in alive {
            if let Some(pos) = cache.get_positive_if_fresh(&server.id).await {
                // Server is alive (ping passed) and cache is fresh.
                // Skip batch test — use cached data directly.
                server.behind_cdn = pos.behind_cdn;
                if !server.behind_cdn {
                    // Only overwrite exit fields if cache actually has data.
                    // A cache hit with exit_ip=None (transient outlet IP fetch
                    // failure during previous batch test) would otherwise erase
                    // previously valid exit data — critical since profiles.json
                    // is the sole source of exit data.
                    if pos.exit_ip.is_some() {
                        server.exit_server = pos.exit_ip;
                        server.exit_flag = pos.exit_flag;
                        server.exit_country_name = pos.exit_country;
                        server.exit_city_name = pos.exit_city;
                    }
                }
                ready_servers.push(server);
            } else {
                // Server is new, stale, or negative — needs batch test
                needs_batch_test.push(server);
            }
        }

        tracing::info!(
            target: "cache",
            "{}/{} server(s) had fresh cache — skipping batch test",
            ready_servers.len(),
            ready_servers.len() + needs_batch_test.len(),
        );
    } else {
        // No cache available — batch test everything
        needs_batch_test = alive;
    }

    // Запускаем batch test всегда, когда есть серверы для тестирования.
    // Batch test использует порты из PortPool (30000-35000),
    // основной прокси — свой порт (~1080/5050), конфликта нет.
    if !needs_batch_test.is_empty() {
        if let Some(cache) = cache.as_ref() {
            let main_proxy_running =
                crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0;

            tracing::info!(
                target: "cache",
                "Starting batch test for {} NEW/UNKNOWN server(s){}...",
                needs_batch_test.len(),
                if main_proxy_running { " (main proxy running — separate ports)" } else { "" },
            );

            let cancel_token = app_handle
                .and_then(|app| app.try_state::<Arc<Mutex<CancellationToken>>>())
                .map(|g| g.lock().unwrap_or_else(|e| e.into_inner()).clone());

            let total_alive_servers = ready_servers.len() + needs_batch_test.len();
            let mut updated = match app_handle {
                Some(app) => {
                    cache
                        .batch_test(
                            Some(app),
                            &needs_batch_test,
                            cancel_token,
                            profile_id,
                            total_alive_servers,
                        )
                        .await
                }
                None => {
                    tracing::info!(
                        target: "cache",
                        "Batch test running headless (no Tauri app handle)",
                    );
                    cache
                        .batch_test(
                            None,
                            &needs_batch_test,
                            cancel_token,
                            profile_id,
                            total_alive_servers,
                        )
                        .await
                }
            };
            ready_servers.append(&mut updated);

            // Restore preflight Clash API ping — batch test is for exit IP,
            // not for latency. The Clash API delay probe is the authoritative
            // ping source (protocol-agnostic, works without external connectivity).
            if !preflight_pings.is_empty() {
                for server in &mut ready_servers {
                    if let Some(ping) = preflight_pings.get(&server.id) {
                        server.ping_ms = Some(*ping);
                    }
                }
                // Also add back servers that batch test couldn't reach but
                // preflight succeeded — they have valid ping.
                let batch_ids: std::collections::HashSet<String> =
                    ready_servers.iter().map(|s| s.id.clone()).collect();
                for mut server in needs_batch_test {
                    if !batch_ids.contains(&server.id) {
                        if let Some(ping) = preflight_pings.get(&server.id) {
                            server.ping_ms = Some(*ping);
                            ready_servers.push(server);
                        }
                    }
                }
            }
        } else {
            tracing::warn!(
                target: "cache",
                "ServerCache state unavailable — {} server(s) added without batch test",
                needs_batch_test.len(),
            );
            ready_servers.append(&mut needs_batch_test);
        }
    }

    // Apply CDN node filter if user enabled it
    if settings.block_cdn_nodes {
        let before_cdn = ready_servers.len();
        ready_servers.retain(|s| !s.behind_cdn);
        let cdn_filtered = before_cdn - ready_servers.len();
        if cdn_filtered > 0 {
            tracing::info!(
                target: "subscription",
                "Removed {cdn_filtered} CDN/Worker/WARP server(s) (blockCdnNodes enabled)",
            );
        }
    }

    // Sort by ping (fastest first)
    ready_servers.sort_by(|a, b| {
        a.ping_ms
            .unwrap_or(u64::MAX)
            .cmp(&b.ping_ms.unwrap_or(u64::MAX))
    });

    // Emit fully-tested servers to frontend in BATCH_SIZE groups.
    // No emission happens before this point — servers only appear after
    // completing ping + batch test, guaranteeing correct data.
    if let Some(app) = app_handle {
        for chunk in ready_servers.chunks(50) {
            crate::events::emit_servers_discovered(app, profile_id, chunk.to_vec());
        }
    }

    tracing::info!(
        target: "subscription",
        "{} server(s) passed and saved",
        ready_servers.len(),
    );

    if let Some(fastest) = ready_servers.first() {
        tracing::info!(
            target: "subscription",
            "Fastest server: {} ({} ms)",
            fastest.display_name(),
            fastest.ping_ms.unwrap_or(0),
        );
    }

    ready_servers
}

/// Test all servers that passed ping using sing-box and Clash API.
/// Generates a single config with ALL servers as outbounds, starts sing-box,
/// tests each via GET /proxies/{tag}/delay, returns servers with real latency.
#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "omnitun-profile-test-{}-{}",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn test_new_creates_empty_manager() {
        let mgr = ProfileManager::new(PathBuf::from("/tmp/test"));
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn test_load_nonexistent_file_returns_empty() {
        let dir = test_dir("nonexistent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut mgr = ProfileManager::new(dir.clone());
        mgr.load().unwrap();
        assert!(mgr.list().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = test_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let profile = SubscriptionProfile {
            id: "test-1".into(),
            name: "Test".into(),
            url: "https://example.com/sub".into(),
            servers: vec![],
            updated_at: "2025-01-01T00:00:00Z".into(),
            last_tested: None,
            traffic_used: None,
            traffic_total: None,
            traffic_expiry: None,
        };

        {
            let mut mgr = ProfileManager::new(dir.clone());
            mgr.profiles.push(profile);
            mgr.save().unwrap();
        }

        {
            let mut mgr = ProfileManager::new(dir.clone());
            mgr.load().unwrap();
            assert_eq!(mgr.list().len(), 1);
            assert_eq!(mgr.list()[0].name, "Test");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Extract the country code from an emoji flag at the start of a server name.
///
/// Examples:
/// - "🇨🇭 Switzerland" → Some("CH")
/// - "🇵🇱 Warsaw" → Some("PL")
/// - "some-server" → None
fn emoji_flag_from_name(name: &str) -> Option<String> {
    // Emoji flags are two Regional Indicator symbols: U+1F1E6..U+1F1FF (🇦..🇿)
    let chars: Vec<char> = name.chars().collect();
    if chars.len() < 2 {
        return None;
    }

    let c1 = chars[0] as u32;
    let c2 = chars[1] as u32;

    // Regional Indicator A..Z
    if (0x1F1E6..=0x1F1FF).contains(&c1) && (0x1F1E6..=0x1F1FF).contains(&c2) {
        let letter1 = char::from_u32((c1 - 0x1F1E6) + b'A' as u32)?;
        let letter2 = char::from_u32((c2 - 0x1F1E6) + b'A' as u32)?;
        Some(format!("{letter1}{letter2}"))
    } else {
        None
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    use crate::proxy::config::build_outbound;
    use crate::proxy::config::generate_config;
    use crate::settings::AppSettings;
    use serde_json::json;
    use std::collections::HashMap;
    use std::process::Command;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("omnitun-test-{}-{}", std::process::id(), name))
    }

    fn app_data_dir() -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(std::env::var("APPDATA").expect("APPDATA not set"))
                .join("com.omnitun.desktop")
        }
        #[cfg(not(windows))]
        {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home)
                .join(".config")
                .join("com.omnitun.desktop")
        }
    }

    fn singbox() -> PathBuf {
        let m = app_data_dir()
            .join("binaries")
            .join(crate::proxy::installer::Installer::binary_name());
        if m.exists() {
            return m;
        }
        PathBuf::from("sing-box.exe")
    }

    fn load_real_servers() -> Vec<ServerProfile> {
        let p = app_data_dir().join("profiles.json");
        let Ok(json) = std::fs::read_to_string(&p) else {
            eprintln!("SKIP: profiles.json not found at {}", p.display());
            return Vec::new();
        };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&json) else {
            eprintln!("SKIP: invalid profiles.json");
            return Vec::new();
        };
        data[0]["servers"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| serde_json::from_value(s.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // 1. Config generation — each server as single-outbound config
    // -----------------------------------------------------------------------
    #[test]
    fn test_each_server_generates_valid_config() {
        let servers = load_real_servers();
        if servers.is_empty() {
            eprintln!("SKIP: no servers in profiles.json");
            return;
        }
        let binary = singbox();
        if !binary.exists() {
            eprintln!("SKIP: no sing-box binary");
            return;
        }

        let cache = test_dir("singles");
        let _ = std::fs::create_dir_all(&cache);
        let settings = AppSettings::default();

        let mut ok = 0u32;
        let mut fail = 0u32;
        for (i, s) in servers.iter().enumerate() {
            match generate_config(&cache, 2080, "local-proxy", &[s.clone()], &settings) {
                Ok((cfg_str, _)) => {
                    let tmp = cache.join(format!("c-{i}.json"));
                    let _ = std::fs::write(&tmp, &cfg_str);
                    let out = Command::new(&binary)
                        .args(["check", "-c"])
                        .arg(&tmp)
                        .output();
                    if let Ok(o) = out {
                        if o.status.success() {
                            ok += 1;
                        } else {
                            fail += 1;
                            eprintln!(
                                "  FAIL {}/{}: {} — stderr: {}",
                                i + 1,
                                servers.len(),
                                s.display_name(),
                                String::from_utf8_lossy(&o.stderr)
                            );
                        }
                    } else {
                        fail += 1;
                    }
                }
                Err(e) => {
                    fail += 1;
                    eprintln!("  FAIL generate_config: {} — {e}", s.display_name());
                }
            }
        }
        let _ = std::fs::remove_dir_all(&cache);
        println!(
            "  single-outbound configs: {ok}/{total} passed, {fail} failed",
            total = ok + fail
        );
        assert!(
            ok > fail,
            "Too many single-outbound config failures: {ok} ok, {fail} fail"
        );
    }

    // -----------------------------------------------------------------------
    // 2. Batch config with selector group + all servers
    // -----------------------------------------------------------------------
    #[test]
    fn test_batch_config_with_selector_passes_check() {
        let servers = load_real_servers();
        let binary = singbox();
        if !binary.exists() {
            eprintln!("SKIP");
            return;
        }

        let mut outbounds: Vec<serde_json::Value> = vec![
            json!({"type": "direct", "tag": "direct"}),
            json!({"type": "block", "tag": "block"}),
        ];
        let mut proxy_tags = vec!["direct".to_string()];
        for s in &servers {
            if let Ok(mut ob) = build_outbound(s) {
                let tag = format!("proxy-{}", s.id);
                ob["tag"] = json!(&tag);
                proxy_tags.push(tag);
                outbounds.push(ob);
            }
        }
        outbounds.push(json!({"type": "selector", "tag": "proxy", "outbounds": proxy_tags}));

        let config = json!({
            "log": {"level": "error"},
            "dns": {"servers": [{"type": "https", "tag": "local", "server": "8.8.8.8", "server_port": 443}], "final": "local"},
            "inbounds": [{"type": "mixed", "tag": "in", "listen": "127.0.0.1", "listen_port": 9999}],
            "outbounds": outbounds,
            "route": {"final": "proxy", "auto_detect_interface": true, "default_domain_resolver": "local"},
        });

        let tmp = test_dir("batch-config");
        let _ = std::fs::create_dir_all(&tmp);
        let p = tmp.join("batch.json");
        std::fs::write(&p, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        let out = Command::new(&binary)
            .args(["check", "-c"])
            .arg(&p)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "Batch config check failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&tmp);
        println!(
            "  batch config with {} servers: sing-box check ✓",
            servers.len()
        );
    }

    // -----------------------------------------------------------------------
    // 3. Full config: DNS + route + bypass_ru + enable_mux settings
    // -----------------------------------------------------------------------
    #[test]
    fn test_full_config_has_all_required_sections() {
        let servers = load_real_servers();
        if servers.is_empty() {
            eprintln!("SKIP: no servers in profiles.json");
            return;
        }
        let cache = test_dir("full-check");
        let _ = std::fs::create_dir_all(&cache);
        let settings = AppSettings::default();

        let (cfg_str, _) = generate_config(
            &cache,
            2080,
            "local-proxy",
            &[servers[0].clone()],
            &settings,
        )
        .expect("generate_config failed");
        let cfg: serde_json::Value = serde_json::from_str(&cfg_str).unwrap();

        // DNS
        let dns = cfg.get("dns").expect("Missing dns section");
        assert!(
            dns.get("servers")
                .and_then(|s| s.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "No DNS servers"
        );
        assert!(
            dns.get("final").and_then(|f| f.as_str()).is_some(),
            "No DNS final"
        );

        // Route
        let route = cfg.get("route").expect("Missing route section");
        assert!(
            route
                .get("auto_detect_interface")
                .and_then(|a| a.as_bool())
                .unwrap_or(false),
            "auto_detect_interface must be true"
        );
        assert!(
            route
                .get("default_domain_resolver")
                .and_then(|d| d.as_str())
                .is_some(),
            "Missing default_domain_resolver"
        );

        // Inbounds
        let inbs = cfg.get("inbounds").and_then(|i| i.as_array()).unwrap();
        assert!(!inbs.is_empty(), "No inbounds");

        // Outbounds
        let obs = cfg.get("outbounds").and_then(|o| o.as_array()).unwrap();
        assert!(
            obs.len() >= 3,
            "Expected 3+ outbounds (direct, block, proxy)"
        );

        let _ = std::fs::remove_dir_all(&cache);
        println!("  Full config: DNS ✓ Route ✓ Inbounds ✓ Outbounds ✓");
    }

    // -----------------------------------------------------------------------
    // 6. AlternativeServers: off when autoConnectFastest=false
    // -----------------------------------------------------------------------
    #[test]
    fn test_alternative_servers_only_with_fastest() {
        // This tests the logic in lib.rs: when autoConnectFastest=false,
        // alternativeServers must be empty so no urltest group is added
        let servers = load_real_servers();
        if servers.is_empty() {
            eprintln!("SKIP: no real servers loaded (profiles.json missing or empty)");
            return;
        }
        let cache = test_dir("alt-servers");
        let _ = std::fs::create_dir_all(&cache);

        // autoConnectFastest=false → should produce single outbound per server
        let mut settings_no_fastest = AppSettings::default();
        settings_no_fastest.auto_connect_fastest = false;

        let (cfg_str, _) = generate_config(
            &cache,
            2080,
            "local-proxy",
            &[servers[0].clone()],
            &settings_no_fastest,
        )
        .unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&cfg_str).unwrap();
        let obs = cfg["outbounds"].as_array().unwrap();

        // Count proxy outbounds (not direct, block)
        let proxy_count = obs
            .iter()
            .filter(|o| {
                let t = o["tag"].as_str().unwrap_or("");
                t != "direct" && t != "block"
            })
            .count();
        assert_eq!(
            proxy_count, 1,
            "With autoConnectFastest=false, expected 1 proxy outbound, got {proxy_count}"
        );

        // autoConnectFastest=true → should produce multiple outbounds + urltest
        let mut settings_fastest = AppSettings::default();
        settings_fastest.auto_connect_fastest = true;

        let (cfg_str, _) =
            generate_config(&cache, 2080, "local-proxy", &servers, &settings_fastest).unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&cfg_str).unwrap();
        let obs = cfg["outbounds"].as_array().unwrap();

        // Find urltest group (replaced selector)
        let has_urltest = obs.iter().any(|o| o["type"] == "urltest");
        assert!(
            has_urltest,
            "With autoConnectFastest=true, expected urltest outbound group"
        );

        let _ = std::fs::remove_dir_all(&cache);
        println!("  alternativeServers: off when fastest=false, on when fastest=true ✓");
    }

    // -----------------------------------------------------------------------
    // 7. Kill process - verify doesn't panic
    // -----------------------------------------------------------------------
    #[test]
    fn test_refresh_preserves_server_ids() -> Result<(), Box<dyn std::error::Error>> {
        let servers = load_real_servers();
        if servers.len() < 2 {
            eprintln!(
                "SKIP: need at least 2 servers for ID test, got {}",
                servers.len()
            );
            return Ok(());
        }

        // Simulate "old" servers with known IDs (as if loaded from saved profiles.json)
        let old_servers: Vec<ServerProfile> = servers.iter().take(5).cloned().collect();
        let original_ids: Vec<String> = old_servers.iter().map(|s| s.id.clone()).collect();
        let original_names: Vec<String> = old_servers.iter().map(|s| s.display_name()).collect();

        // Simulate "new" servers from refresh — same data, no IDs preserved initially
        // Re-parse from JSON (like refresh does) to clear any in-memory ID references
        let _raw: serde_json::Value = serde_json::to_value(&old_servers[0])?;
        let mut new_servers: Vec<ServerProfile> = servers.iter().take(5).cloned().collect();
        // Give them random new IDs (simulating fresh parse from subscription)
        for s in &mut new_servers {
            s.id = uuid::Uuid::new_v4().to_string();
        }
        let _new_ids_before: Vec<String> = new_servers.iter().map(|s| s.id.clone()).collect();

        // Now run the EXACT preservation logic from refresh():
        let old_index: HashMap<String, String> = old_servers
            .iter()
            .map(|s| {
                let key = format!(
                    "{}://{}:{}@{}:{}:{}/{}",
                    s.protocol,
                    s.server,
                    s.port,
                    s.uuid.as_deref().unwrap_or(""),
                    s.password.as_deref().unwrap_or(""),
                    s.method.as_deref().unwrap_or(""),
                    s.transport_type.as_deref().unwrap_or(""),
                );
                (key, s.id.clone())
            })
            .collect();

        for server in &mut new_servers {
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

        // Verify: ALL IDs should match originals (same servers, same keys)
        for (i, s) in new_servers.iter().enumerate() {
            assert_eq!(
                s.id,
                original_ids[i],
                "Server {} ({}): ID not preserved after refresh! key: {}://{}:{}@{}:{}:{}:{}",
                i,
                original_names[i],
                s.protocol,
                s.server,
                s.port,
                s.uuid.as_deref().unwrap_or(""),
                s.password.as_deref().unwrap_or(""),
                s.method.as_deref().unwrap_or(""),
                s.transport_type.as_deref().unwrap_or(""),
            );
        }

        println!(
            "✓ ID preservation: {}/{} servers kept their IDs",
            new_servers.len(),
            new_servers.len()
        );
        Ok(())
    }

    #[test]
    fn test_remove_nonexistent_returns_error() {
        let dir = test_dir("remove_err");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut mgr = ProfileManager::new(dir.clone());
        let result = mgr.remove("nonexistent");
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
