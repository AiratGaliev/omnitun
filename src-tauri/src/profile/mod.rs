use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::subscription::SubscriptionProfile;
use crate::vpn::{ServerProfile, VpnError};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tokio::sync::Semaphore;

/// Get proxy port to use for subscription fetch: try running proxy first,
/// then configured port as fallback.
fn proxy_port(app: &AppHandle) -> Option<u16> {
    use crate::settings::AppSettings;
    // If sing-box is running, use its proxy port
    if crate::vpn::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0 {
        if let Some(settings) = app.try_state::<tokio::sync::Mutex<AppSettings>>() {
            if let Ok(settings) = settings.try_lock() {
                return Some(settings.proxy_port);
            }
        }
    }
    // Fallback: always try the configured proxy port — may connect if the
    // previous sing-box instance is still running, or user has external proxy
    if let Some(settings) = app.try_state::<tokio::sync::Mutex<AppSettings>>() {
        if let Ok(settings_guard) = settings.try_lock() {
            if settings_guard.proxy_port > 0 {
                return Some(settings_guard.proxy_port);
            }
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

    pub fn load(&mut self) -> Result<(), VpnError> {
        match std::fs::read_to_string(&self.file_path) {
            Ok(json) => {
                let profiles: Vec<SubscriptionProfile> =
                    serde_json::from_str(&json).map_err(|e| {
                        VpnError::Subscription(format!("Failed to parse profiles.json: {e}"))
                    })?;
                self.profiles = profiles;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First launch — no profiles yet
                self.profiles = Vec::new();
                Ok(())
            }
            Err(e) => Err(VpnError::Io(e)),
        }
    }

    pub fn save(&self) -> Result<(), VpnError> {
        let json = serde_json::to_string_pretty(&self.profiles).map_err(|e| {
            VpnError::Subscription(format!("Failed to serialize profiles: {e}"))
        })?;
        std::fs::write(&self.file_path, json)?;
        Ok(())
    }

    pub fn list(&self) -> &[SubscriptionProfile] {
        &self.profiles
    }

    pub fn list_mut(&mut self) -> &mut Vec<SubscriptionProfile> {
        &mut self.profiles
    }

    pub async fn add(&mut self, app: &AppHandle, name: String, url: String) -> Result<SubscriptionProfile, VpnError> {
        let clean_url = url.trim().to_string();

        if self.profiles.iter().any(|p| p.url == clean_url) {
            return Err(VpnError::Subscription(format!(
                "Subscription with URL '{}' already exists",
                clean_url
            )));
        }

        let mut servers = if clean_url.starts_with("http://") || clean_url.starts_with("https://") {
            crate::subscription::fetch_subscription(&clean_url, proxy_port(app)).await?
        } else {
            crate::subscription::parser::parse_subscription(&clean_url)?
        };

        servers = process_servers(app, servers).await;

        // Fetch subscription traffic info
        let (traffic_used, traffic_total, traffic_expiry) = crate::subscription::fetch_subscription_traffic(&clean_url, proxy_port(app)).await
            .map(|(used, total, expiry)| (Some(used), Some(total), expiry))
            .unwrap_or((None, None, None));

        let profile = SubscriptionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            url: clean_url,
            servers,
            updated_at: chrono::Utc::now().to_rfc3339(),
            traffic_used,
            traffic_total,
            traffic_expiry,
        };

        self.profiles.push(profile.clone());
        self.save()?;
        Ok(profile)
    }

    pub fn remove(&mut self, id: &str) -> Result<(), VpnError> {
        let len_before = self.profiles.len();
        self.profiles.retain(|p| p.id != id);
        if self.profiles.len() == len_before {
            return Err(VpnError::Subscription("Profile not found".into()));
        }
        self.save()?;
        Ok(())
    }

    pub async fn refresh(&mut self, app: &AppHandle, id: &str) -> Result<SubscriptionProfile, VpnError> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| VpnError::Subscription("Profile not found".into()))?;

        let url = &self.profiles[idx].url;
        let mut servers = if url.starts_with("http://") || url.starts_with("https://") {
            crate::subscription::fetch_subscription(url, proxy_port(app)).await?
        } else {
            crate::subscription::parser::parse_subscription(url)?
        };

        // Preserve server IDs across refresh — match by (protocol, server, port, uuid, password)
        let old_index: std::collections::HashMap<String, String> = self.profiles[idx]
            .servers
            .iter()
            .map(|s| {
                let key = format!(
                    "{}://{}:{}@{}:{}",
                    s.protocol,
                    s.server,
                    s.port,
                    s.uuid.as_deref().unwrap_or(""),
                    s.password.as_deref().unwrap_or(""),
                );
                (key, s.id.clone())
            })
            .collect();
        for server in &mut servers {
            let key = format!(
                "{}://{}:{}@{}:{}",
                server.protocol,
                server.server,
                server.port,
                server.uuid.as_deref().unwrap_or(""),
                server.password.as_deref().unwrap_or(""),
            );
            if let Some(old_id) = old_index.get(&key) {
                server.id = old_id.clone();
            }
        }

        servers = process_servers(app, servers).await;

        // Guard: don't overwrite servers with empty list (e.g. content:// URLs)
        if servers.is_empty() {
            return Err(VpnError::Subscription(
                "Refresh returned 0 servers — keeping existing servers".into()
            ));
        }

        self.profiles[idx].servers = servers;
        self.profiles[idx].updated_at = chrono::Utc::now().to_rfc3339();

        // Refresh traffic info
        let traffic = crate::subscription::fetch_subscription_traffic(&self.profiles[idx].url, proxy_port(app)).await;
        if let Some((used, total, expiry)) = traffic {
            self.profiles[idx].traffic_used = Some(used);
            self.profiles[idx].traffic_total = Some(total);
            self.profiles[idx].traffic_expiry = expiry;
        }

        let profile = self.profiles[idx].clone();
        self.save()?;
        Ok(profile)
    }
}

/// Remove duplicate servers and test reachability via TCP ping.
/// Logs each step (dedup count, testing, unreachable count, final count).
pub async fn process_servers(app: &AppHandle, servers: Vec<ServerProfile>) -> Vec<ServerProfile> {
    let initial_count = servers.len();
    let logger = app.state::<Mutex<Logger>>();

    // Lazy-init GeoIP: try to load existing MMDB; spawn download if missing
    if !crate::vpn::geoip::is_ready() {
        let data_dir = app.path().app_data_dir().ok();
        if let Some(dir) = data_dir {
            let db_path = dir.join("geoip").join("GeoLite2-Country.mmdb");
            if db_path.exists() {
                let _ = crate::vpn::geoip::init(&db_path);
            } else {
                // Spawn background download — will be ready for next refresh
                let app_clone = app.clone();
                tokio::spawn(async move {
                    let geo_dir = app_clone.path().app_data_dir().unwrap_or_default().join("geoip");
                    if let Err(e) = crate::vpn::geoip::ensure_downloaded(&geo_dir).await {
                        log(
                            &app_clone,
                            &app_clone.state::<Mutex<Logger>>(),
                            LogLevel::Warning,
                            LogSource::System,
                            format!("GeoIP download failed: {e}"),
                        );
                    }
                });
            }
        }
    }

    // 1. Deduplication by protocol + server + port + uuid + password
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for s in servers {
        let key = format!(
            "{}://{}:{}@{}:{}",
            s.protocol,
            s.server,
            s.port,
            s.uuid.as_deref().unwrap_or(""),
            s.password.as_deref().unwrap_or(""),
        );
        if seen.insert(key) {
            unique.push(s);
        }
    }
    let duplicates = initial_count - unique.len();
    if duplicates > 0 {
        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Subscription,
            format!("Removed {duplicates} duplicate(s)"),
        );
    }

    // 2. Apply user filters (country, protocol) from settings
    if let Some(settings) = app.try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>() {
        let settings_guard = settings.lock().await;
        let total_before = unique.len();
        unique.retain(|s| {
            // Filter by protocol
            if !settings_guard.allowed_protocols.is_empty() {
                if !settings_guard.allowed_protocols.contains(&s.protocol) {
                    return false;
                }
            }
            if !settings_guard.blocked_protocols.is_empty() {
                if settings_guard.blocked_protocols.contains(&s.protocol) {
                    return false;
                }
            }
            // Filter by country (flag must be resolved first)
            if !settings_guard.allowed_countries.is_empty() {
                if let Some(ref flag) = s.flag {
                    if !settings_guard.allowed_countries.contains(flag) {
                        return false;
                    }
                }
            }
            if !settings_guard.blocked_countries.is_empty() {
                if let Some(ref flag) = s.flag {
                    if settings_guard.blocked_countries.contains(flag) {
                        return false;
                    }
                }
            }
            true
        });
        let filtered = total_before - unique.len();
        if filtered > 0 {
            log(
                app,
                &logger,
                LogLevel::Info,
                LogSource::Subscription,
                format!("Filtered {filtered} server(s) by country/protocol settings"),
            );
        }
    }

    // 3. TCP ping — up to 50 concurrent connections, 3-second timeout each
    let unique_count = unique.len();
    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Testing {unique_count} server(s) for availability..."),
    );

    let semaphore = Arc::new(Semaphore::new(50));
    let mut set = tokio::task::JoinSet::new();

    for mut server in unique {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore not closed");

        // Resolve country flag from server name (emoji) or IP (GeoIP)
        if server.flag.is_none() {
            // 1. Try emoji flag in server name first
            let flag = emoji_flag_from_name(&server.name)
                .or_else(|| {
                    let c = crate::vpn::geoip::resolve(&server.server);
                    if c != "Unknown" { Some(c) } else { None }
                });
            if let Some(f) = flag {
                server.flag = Some(f);
            }
        }

        set.spawn(async move {
            let addr = format!("{}:{}", server.server, server.port);
            let start = std::time::Instant::now();
            // Retry up to 3 times with 1-second backoff (Karing-style)
            let mut reachable = false;
            for attempt in 1..=3 {
                if attempt > 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                if tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::TcpStream::connect(&addr),
                )
                .await
                .is_ok_and(|r| r.is_ok())
                {
                    reachable = true;
                    break;
                }
            }
            let ping_ms = start.elapsed().as_millis() as u64;
            drop(permit);
            (server, reachable, ping_ms)
        });
    }

    let mut alive = Vec::new();
    while let Some(result) = set.join_next().await {
        if let Ok((mut server, true, ping_ms)) = result {
            server.ping_ms = Some(ping_ms);
            alive.push(server);
        }
    }

    let unreachable = unique_count - alive.len();
    if unreachable > 0 {
        log(
            app,
            &logger,
            LogLevel::Warning,
            LogSource::Subscription,
            format!("Removed {unreachable} unreachable server(s)"),
        );
    }

    log(
        app,
        &logger,
        LogLevel::Success,
        LogSource::Subscription,
        format!("{} server(s) passed and saved", alive.len()),
    );

    // 4. Real latency test via Clash API — start a temporary sing-box with all servers as outbounds
    if !alive.is_empty() {
        let updated = test_servers_via_singbox_batch(app, &alive).await;
        if let Ok(mut updated) = updated {
            // Sort by ping (fastest first), unknown pings last
            updated.sort_by(|a, b| a.ping_ms.unwrap_or(u64::MAX).cmp(&b.ping_ms.unwrap_or(u64::MAX)));
            if let Some(fastest) = updated.first() {
                log(
                    app,
                    &logger,
                    LogLevel::Info,
                    LogSource::Subscription,
                    format!("Fastest server: {} ({} ms)", fastest.name, fastest.ping_ms.unwrap_or(0)),
                );
            }
            return updated;
        }
    }

    // Fallback: sort alive servers by TCP ping
    alive.sort_by(|a, b| a.ping_ms.unwrap_or(u64::MAX).cmp(&b.ping_ms.unwrap_or(u64::MAX)));
    alive
}

/// Test all servers that passed TCP ping using sing-box and Clash API.
/// Generates a single config with ALL servers as outbounds, starts sing-box,
/// tests each via GET /proxies/{tag}/delay, returns servers with real latency.
async fn test_servers_via_singbox_batch(
    app: &AppHandle,
    servers: &[ServerProfile],
) -> Result<Vec<ServerProfile>, ()> {
    use crate::vpn::config::{build_outbound, find_available_port};
    use serde_json::json;
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let logger = app.state::<Mutex<Logger>>();

    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Testing {} server(s) via real proxy connection...", servers.len()),
    );

    let data_dir = app.path().app_data_dir().map_err(|_| ())?;
    let binary_path = data_dir
        .join("binaries")
        .join(crate::vpn::installer::Installer::binary_name());

    let proxy_port = find_available_port().ok_or(())?;
    let stats_port = find_available_port().ok_or(())?;

    // Build outbounds for all servers
    let mut outbounds: Vec<serde_json::Value> = vec![
        json!({"type": "direct", "tag": "direct"}),
        json!({"type": "block", "tag": "block"}),
    ];
    for server in servers {
        if let Ok(ob) = build_outbound(server) {
            // Tag must match server index for Clash API lookup
            let tag = format!("proxy-{}", &server.id);
            let mut ob = ob;
            ob["tag"] = json!(tag);
            outbounds.push(ob);
        }
    }

    let test_dir = std::env::temp_dir().join(format!("omnitun-latency-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&test_dir);

    let config_json = json!({
        "log": { "level": "error", "output": test_dir.join("sing-box.log").to_string_lossy().to_string(), "timestamp": true },
        "dns": {
            "servers": [
                {"type": "https", "tag": "remote", "server": "1.1.1.1", "server_port": 443, "detour": "proxy"},
                {"type": "https", "tag": "local", "server": "8.8.8.8", "server_port": 443}
            ],
            "final": "remote"
        },
        "inbounds": [{"type": "mixed", "tag": "mixed-in", "listen": "127.0.0.1", "listen_port": proxy_port}],
        "outbounds": outbounds,
        "experimental": {
            "clash_api": {
                "external_controller": format!("127.0.0.1:{stats_port}"),
                "secret": ""
            }
        },
        "route": {
            "final": "direct",
            "auto_detect_interface": true,
            "default_domain_resolver": "local",
            "rules": [{"ip_is_private": true, "outbound": "direct"}]
        }
    });

    let config_path = test_dir.join("test-batch.json");
    let config_str = serde_json::to_string_pretty(&config_json).map_err(|_| ())?;
    std::fs::write(&config_path, &config_str).map_err(|_| ())?;

    // Start sing-box
    #[cfg(windows)]
    let child = std::process::Command::new(&binary_path)
        .args(["run", "-c"])
        .arg(&config_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| ())?;
    #[cfg(not(windows))]
    let child = std::process::Command::new(&binary_path)
        .args(["run", "-c"])
        .arg(&config_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| ())?;

    let pid = child.id();

    // Wait for sing-box to start
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    // Test each server via Clash API
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|_| {
            let _ = kill_process(pid);
            ()
        })?;

    let api_url = format!("http://127.0.0.1:{stats_port}");
    let test_url = "http://cp.cloudflare.com/generate_204";

    let mut updated = Vec::new();

    for server in servers {
        let tag = format!("proxy-{}", &server.id);
        let delay_url = format!(
            "{api_url}/proxies/{tag}/delay?url={test_url}&timeout=5000"
        );

        match client.get(&delay_url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    if let Ok(body) = resp.text().await {
                        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) {
                            if let Some(ms) = data["delay"].as_u64() {
                                let mut s = server.clone();
                                s.ping_ms = Some(ms);
                                updated.push(s);
                                continue;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // Fallback: still include servers that passed TCP ping but Clash API test failed
        updated.push(server.clone());
    }

    // Cleanup
    let _ = kill_process(pid);
    let _ = std::fs::remove_dir_all(&test_dir);

    log(
        app,
        &logger,
        LogLevel::Success,
        LogSource::Subscription,
        format!("Real latency test complete for {} server(s)", updated.len()),
    );

    Ok(updated)
}

/// Kill a process by PID (cross-platform)
fn kill_process(pid: u32) -> Result<(), ()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|_| ())?;
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output()
            .map_err(|_| ())?;
    }
    Ok(())
}

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
    if c1 >= 0x1F1E6 && c1 <= 0x1F1FF && c2 >= 0x1F1E6 && c2 <= 0x1F1FF {
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
    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("omnitun-test-{name}"))
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
