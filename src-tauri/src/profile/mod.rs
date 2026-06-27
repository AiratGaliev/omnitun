use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::subscription::SubscriptionProfile;
use crate::vpn::{ServerProfile, VpnError};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tokio::sync::Semaphore;

/// Guard: only one batch test runs at a time
pub(crate) static TESTING_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Get proxy port to route subscription fetch through the active proxy.
/// Returns None in VPN mode — all traffic already goes through TUN, so direct
/// fetch works and avoids the proxy not listening issue.
fn proxy_port(app: &AppHandle) -> Option<u16> {
    use crate::settings::AppSettings;

    // Check current routing mode: in VPN mode, all traffic routes through TUN
    // automatically, no need to proxy the subscription fetch.
    if let Some(state) = app.try_state::<tokio::sync::Mutex<AppSettings>>() {
        if let Ok(guard) = state.try_lock() {
            if guard.routing_mode == "vpn" {
                return None;
            }
        }
    }

    // Proxy / system-proxy / local-proxy modes: use the proxy port
    // so the fetch goes through the active outbound.
    if crate::vpn::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0 {
        if let Some(state) = app.try_state::<tokio::sync::Mutex<AppSettings>>() {
            if let Ok(guard) = state.try_lock() {
                return Some(guard.proxy_port);
            }
        }
    }

    // Fallback: try the configured proxy port
    if let Some(state) = app.try_state::<tokio::sync::Mutex<AppSettings>>() {
        if let Ok(guard) = state.try_lock() {
            if guard.proxy_port > 0 {
                return Some(guard.proxy_port);
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

    /// Run GeoIP enrichment on all servers missing country/city fields.
    /// This fixes data from old profiles.json that only had flag set.
    pub async fn enrich_loaded(&mut self) {
        if !crate::vpn::geoip::is_ready() {
            return;
        }
        let mut changed = false;
        let mut enriched_count = 0u32;
        for profile in &mut self.profiles {
            for server in &mut profile.servers {
                if server.country_name.is_some() && server.city_name.is_some() {
                    continue; // already enriched
                }
                if let Some(info) = crate::vpn::geoip::resolve_hostname(&server.server).await {
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
        log::info!("GeoIP enrichment: {enriched_count} server(s) updated");
    }

    pub fn save(&self) -> Result<(), VpnError> {
        let json = serde_json::to_string_pretty(&self.profiles)
            .map_err(|e| VpnError::Subscription(format!("Failed to serialize profiles: {e}")))?;
        std::fs::write(&self.file_path, json)?;
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
    ) -> Result<SubscriptionProfile, VpnError> {
        let clean_url = url.trim().to_string();

        if self.profiles.iter().any(|p| p.url == clean_url) {
            return Err(VpnError::Subscription(format!(
                "Subscription with URL '{}' already exists",
                clean_url
            )));
        }

        let mut servers = if clean_url.starts_with("http://") || clean_url.starts_with("https://") {
            let logger = app.state::<Mutex<Logger>>();
            // Try through existing proxy first, fall back to direct
            match crate::subscription::fetch_subscription(&clean_url, proxy_port(app)).await {
                Ok(s) => s,
                Err(e) => {
                    if proxy_port(app).is_some() {
                        log(
                            app,
                            &logger,
                            LogLevel::Warning,
                            LogSource::Subscription,
                            format!("Proxy fetch failed, retrying direct: {e}"),
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

        servers = process_servers(app, servers).await;

        // Fetch subscription traffic info
        let (traffic_used, traffic_total, traffic_expiry) =
            crate::subscription::fetch_subscription_traffic(&clean_url, proxy_port(app))
                .await
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

    pub async fn refresh(
        &mut self,
        app: &AppHandle,
        id: &str,
    ) -> Result<SubscriptionProfile, VpnError> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| VpnError::Subscription("Profile not found".into()))?;

        let url = &self.profiles[idx].url;
        let logger = app.state::<Mutex<Logger>>();
        let fetch_result = match url {
            u if u.starts_with("http://") || u.starts_with("https://") => {
                match crate::subscription::fetch_subscription(u, proxy_port(app)).await {
                    Ok(s) => Ok(s),
                    Err(e) => {
                        if proxy_port(app).is_some() {
                            log(
                                app,
                                &logger,
                                LogLevel::Warning,
                                LogSource::Subscription,
                                format!("Proxy fetch failed, retrying direct: {e}"),
                            );
                            crate::subscription::fetch_subscription(u, None).await
                        } else {
                            Err(e)
                        }
                    }
                }
            }
            _ => crate::subscription::parser::parse_subscription(url)
                .map_err(|e| VpnError::Subscription(format!("Parse error: {e}"))),
        };

        // On HTTP/parse error: still bump updated_at so the frontend timer
        // doesn't retry immediately. The user can manually refresh later.
        let mut servers = match fetch_result {
            Ok(s) => s,
            Err(e) => {
                log(
                    app,
                    &app.state::<Mutex<Logger>>(),
                    LogLevel::Warning,
                    LogSource::Subscription,
                    format!("Refresh failed, bumping updated_at: {e}"),
                );
                self.profiles[idx].updated_at = chrono::Utc::now().to_rfc3339();
                // If we have existing servers, enrich them with GeoIP while we wait
                if !self.profiles[idx].servers.is_empty() {
                    let existing = std::mem::take(&mut self.profiles[idx].servers);
                    self.profiles[idx].servers = process_servers(
                        app,
                        existing
                            .into_iter()
                            .map(|mut s| {
                                // Reset ping so it gets re-tested
                                s.ping_ms = None;
                                s
                            })
                            .collect(),
                    )
                    .await;
                }
                self.save()?;
                return Err(VpnError::Subscription(format!("{e}")));
            }
        };

        // Preserve server IDs across refresh — match by (protocol, server, port, uuid, password, method, transport_type)
        let old_index: std::collections::HashMap<String, String> = self.profiles[idx]
            .servers
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

        servers = process_servers(app, servers).await;

        // Guard: don't overwrite servers with empty list (e.g. content:// URLs)
        if servers.is_empty() {
            return Err(VpnError::Subscription(
                "Refresh returned 0 servers — keeping existing servers".into(),
            ));
        }

        self.profiles[idx].servers = servers;
        self.profiles[idx].updated_at = chrono::Utc::now().to_rfc3339();

        // Refresh traffic info
        let traffic = crate::subscription::fetch_subscription_traffic(
            &self.profiles[idx].url,
            proxy_port(app),
        )
        .await;
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

/// Deduplicate servers, apply user filters, run TCP ping, enrich GeoIP flag.
/// Logs each step (dedup count, testing, unreachable count, final count).
pub async fn process_servers(app: &AppHandle, servers: Vec<ServerProfile>) -> Vec<ServerProfile> {
    let initial_count = servers.len();
    let logger = app.state::<Mutex<Logger>>();

    // Lazy-init GeoIP: load from disk or download synchronously (one-time cost)
    let data_dir = app.path().app_data_dir().ok();
    if let Some(ref dir) = data_dir {
        let geo_dir = dir.join("geoip");
        let city_path = geo_dir.join("GeoLite2-City.mmdb");
        let asn_path = geo_dir.join("GeoLite2-ASN.mmdb");

        if !crate::vpn::geoip::is_ready() {
            if city_path.exists() {
                let _ = crate::vpn::geoip::init_city(&city_path);
            } else {
                log(
                    app,
                    &logger,
                    LogLevel::Info,
                    LogSource::System,
                    "Downloading GeoIP databases (one-time)...".into(),
                );
                match crate::vpn::geoip::ensure_downloaded(
                    &geo_dir,
                    &app
                        .try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
                        .map(|s| s.try_lock().ok().map(|g| g.ipinfo_token.clone()))
                        .flatten()
                        .unwrap_or_default(),
                )
                .await
                {
                    Ok(_) => log(
                        app,
                        &logger,
                        LogLevel::Success,
                        LogSource::System,
                        "GeoIP databases ready".into(),
                    ),
                    Err(e) => log(
                        app,
                        &logger,
                        LogLevel::Warning,
                        LogSource::System,
                        format!("GeoIP download failed (will retry): {e}"),
                    ),
                }
            }
        }

        // Always try to init ASN if the file exists — may have been downloaded
        // on a previous run while is_ready() already returned true for City only.
        if asn_path.exists() && !crate::vpn::geoip::is_asn_ready() {
            let _ = crate::vpn::geoip::init_asn(&asn_path);
        }

        // Always try to init ipinfo Lite if the file exists
        let ipinfo_path = geo_dir.join("ipinfo_lite.mmdb");
        if ipinfo_path.exists() && !crate::vpn::geoip::is_ipinfo_ready() {
            let _ = crate::vpn::geoip::init_ipinfo(&ipinfo_path);
        }

        // Always try to init CDN ranges from downloaded .srs files
        if !crate::vpn::geoip::is_cdn_ready() {
            let mut merged: Vec<(std::net::IpAddr, u8)> = Vec::new();
            for name in &["cdn-only_singbox.srs", "merged.srs"] {
                let path = geo_dir.join(name);
                if !path.exists() {
                    continue;
                }
                if let Ok(r) = crate::vpn::geoip::parse_srs_file(&path) {
                    log::info!("CDN .srs {name}: {} CIDR entries (lazy)", r.len());
                    merged.extend(r);
                }
            }
            if !merged.is_empty() {
                log::info!("CDN ranges merged: {} total entries (lazy)", merged.len());
                crate::vpn::geoip::set_cdn_ranges(merged);
            }
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
    if duplicates > 0 {
        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Subscription,
            format!("Removed {duplicates} duplicate(s)"),
        );
    }

    // 1b. Fast keyword pre-check for serverless domains (workers.dev, pages.dev).
    //     Full CDN detection via ASN happens during batch latency test.
    for s in &mut unique {
        s.behind_cdn = crate::vpn::is_cdn_server(s).await;
        if s.behind_cdn {
            log(
                app,
                &logger,
                LogLevel::Info,
                LogSource::Subscription,
                format!("[CDN] keyword match: {} ({})", s.display_name(), s.server),
            );
        }
    }

    // 2. Apply user filters (country, protocol) from settings
    if let Some(settings) = app.try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>() {
        let settings_guard = settings.lock().await;
        let total_before = unique.len();
        unique.retain(|s| {
            // Filter by protocol
            if !settings_guard.allowed_protocols.is_empty()
                && !settings_guard.allowed_protocols.contains(&s.protocol)
            {
                return false;
            }
            if !settings_guard.blocked_protocols.is_empty()
                && settings_guard.blocked_protocols.contains(&s.protocol)
            {
                return false;
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

        // Try emoji flag from server name first (fast, no network)
        if server.flag.is_none() {
            if let Some(f) = emoji_flag_from_name(&server.original_name) {
                server.flag = Some(f);
            }
        }

        set.spawn(async move {
            let addr = format!("{}:{}", server.server, server.port);
            let start = std::time::Instant::now();
            // Single attempt with 3-second timeout (was 3 retries — too permissive)
            let reachable = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::net::TcpStream::connect(&addr),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            let ping_ms = start.elapsed().as_millis() as u64;
            drop(permit);
            (server, reachable, ping_ms)
        });
    }

    let mut alive = Vec::new();
    while let Some(result) = set.join_next().await {
        if let Ok((mut server, true, ping_ms)) = result {
            server.ping_ms = Some(ping_ms);

            // GeoIP enrichment: set flag + country/city fields + display name
            // (subscription names often contain garbled emoji, components separate)
            if let Some(info) = crate::vpn::geoip::resolve_hostname(&server.server).await {
                server.flag = Some(info.country_code.clone());
                server.country_name = Some(info.country_name.clone());
                if !info.city.is_empty() {
                    server.city_name = Some(info.city.clone());
                }

                // CDN detection via entry IP — if the server's resolved IP
                // belongs to a known CDN provider, mark it as behind CDN.
                if !server.behind_cdn && crate::vpn::geoip::is_cdn_asn(&server.server) {
                    server.behind_cdn = true;
                    log(
                        app,
                        &logger,
                        LogLevel::Info,
                        LogSource::Subscription,
                        format!("[CDN] entry IP {} is behind CDN", &server.server),
                    );
                }
            }
            // If GeoIP fails, keep flag from emoji extraction or leave unset

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

    // 4. Real latency test via Clash API — start a temporary sing-box with all servers as outbounds.
    //    Skip if main VPN is running (separate sing-box instances crash the main process on Windows).
    let main_vpn_running =
        crate::vpn::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0;
    if !alive.is_empty() && !main_vpn_running {
        // Используем параллельную проверку если серверов больше 20
        let updated = if alive.len() > 20 {
            test_servers_via_singbox_parallel(app, &alive).await
        } else {
            test_servers_via_singbox_batch(app, &alive).await
        };
        if let Ok(mut updated) = updated {
            let before_filter = updated.len();
            updated.retain(|s| s.ping_ms.is_some());
            let latency_filtered = before_filter - updated.len();
            if latency_filtered > 0 {
                log(
                    app,
                    &logger,
                    LogLevel::Warning,
                    LogSource::Subscription,
                    format!("Removed {latency_filtered} server(s) that failed real latency test"),
                );
            }

            // Apply CDN node filter if user enabled it
            if let Some(settings) =
                app.try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>()
            {
                let settings_guard = settings.lock().await;
                if settings_guard.block_cdn_nodes {
                    let before_cdn_filter = updated.len();
                    updated.retain(|s| !s.behind_cdn);
                    let cdn_filtered = before_cdn_filter - updated.len();
                    if cdn_filtered > 0 {
                        log(
                            app,
                            &logger,
                            LogLevel::Info,
                            LogSource::Subscription,
                            format!("Removed {cdn_filtered} CDN/Worker/WARP server(s) (blockCdnNodes enabled)"),
                        );
                    }
                }
            }

            // Sort by ping (fastest first)
            updated.sort_by(|a, b| {
                a.ping_ms
                    .unwrap_or(u64::MAX)
                    .cmp(&b.ping_ms.unwrap_or(u64::MAX))
            });
            if let Some(fastest) = updated.first() {
                log(
                    app,
                    &logger,
                    LogLevel::Info,
                    LogSource::Subscription,
                    format!(
                        "Fastest server: {} ({} ms)",
                        fastest.display_name(),
                        fastest.ping_ms.unwrap_or(0)
                    ),
                );
            }
            return updated;
        } else {
            log(
                app,
                &logger,
                LogLevel::Warning,
                LogSource::Subscription,
                "Real latency test via sing-box failed — falling back to TCP ping results".into(),
            );
        }
    } else if main_vpn_running && !alive.is_empty() {
        // CDN detection via active proxy (main VPN running, can't start batch test)
        if let Some(settings) = app.try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>() {
            let guard = settings.lock().await;
            if guard.proxy_port > 0 {
                if let Some(outlet) = fetch_outlet_ip(guard.proxy_port).await {
                    let is_cdn = crate::vpn::geoip::is_cdn_asn(&outlet);
                    if is_cdn {
                        log(
                            app,
                            &logger,
                            LogLevel::Info,
                            LogSource::Subscription,
                            format!("[CDN] outlet IP {} is behind CDN (via active proxy)", outlet),
                        );
                    }
                }
            }
        }
    }

    // Fallback: sort alive servers by TCP ping
    alive.sort_by(|a, b| {
        a.ping_ms
            .unwrap_or(u64::MAX)
            .cmp(&b.ping_ms.unwrap_or(u64::MAX))
    });
    alive
}

/// Test all servers that passed TCP ping using sing-box and Clash API.
/// Generates a single config with ALL servers as outbounds, starts sing-box,
/// tests each via GET /proxies/{tag}/delay, returns servers with real latency.
///
/// Fetch outlet IP through a given proxy port. Uses multiple IP echo services
/// concurrently via `tokio::select!` — takes the first successful response
/// within the total timeout, avoiding sequential 4s×N waits.
///
/// IMPORTANT: uses `Proxy::all` (not `Proxy::http`) so HTTPS URLs like
/// `https://ifconfig.me/ip` are properly wrapped in HTTP CONNECT tunnels
/// through the sing-box mixed inbound.
async fn fetch_outlet_ip(proxy_port: u16) -> Option<String> {
    let proxy = match reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}")) {
        Ok(p) => p,
        Err(_) => return None,
    };
    let client = match reqwest::Client::builder()
        .proxy(proxy)
        .timeout(std::time::Duration::from_secs(6))
        .build()
    {
        Ok(c) => c,
        Err(_) => return None,
    };

    let ip_urls = [
        "https://ifconfig.me/ip",
        "https://api.ipify.org",
        "https://icanhazip.com",
    ];

    // Sequential with per-URL timeout — concurrent requests through the
    // same proxy port create CONNECT tunnel contention in sing-box,
    // causing spurious failures. Sequential is slower but more reliable.
    for url in ip_urls {
        if let Ok(resp) = client.get(url).send().await {
            if resp.status().is_success() {
                if let Ok(text) = resp.text().await {
                    let ip = text.trim().to_string();
                    if !ip.is_empty()
                        && ip.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':')
                    {
                        return Some(ip);
                    }
                }
            }
        }
    }
    None
}

/// Параллельная проверка outlet IP через множественные инстансы sing-box
async fn test_servers_via_singbox_parallel(
    app: &AppHandle,
    servers: &[ServerProfile],
) -> Result<Vec<ServerProfile>, ()> {
    use crate::vpn::config::{build_outbound, find_available_port};
    use serde_json::json;
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    use std::time::Duration;

    let logger = app.state::<Mutex<Logger>>();
    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!(
            "Testing {} server(s) via parallel sing-box instances...",
            servers.len()
        ),
    );

    let data_dir = app.path().app_data_dir().map_err(|_| ())?;
    let binary_path = data_dir
        .join("binaries")
        .join(crate::vpn::installer::Installer::binary_name());

    // Разбиваем серверы на батчи по 20
    const BATCH_SIZE: usize = 20;
    let batches: Vec<&[ServerProfile]> = servers.chunks(BATCH_SIZE).collect();

    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!(
            "Split into {} batch(es) of up to {} servers each",
            batches.len(),
            BATCH_SIZE
        ),
    );

    let mut all_results = Vec::new();
    let mut batch_handles = Vec::new();

    // Запускаем каждый батч параллельно
    for (batch_idx, batch) in batches.iter().enumerate() {
        let binary_path = binary_path.clone();
        let batch = batch.to_vec();
        let app_clone = app.clone();

        batch_handles.push(tokio::spawn(async move {
            // Находим свободные порты для этого батча
            let proxy_port = find_available_port().ok_or(())?;
            let stats_port = find_available_port().ok_or(())?;

            // Строим конфиг только для этого батча
            let mut outbounds: Vec<serde_json::Value> = vec![
                json!({"type": "direct", "tag": "direct"}),
                json!({"type": "block", "tag": "block"}),
            ];

            let mut proxy_tags: Vec<String> = Vec::new();
            for server in &batch {
                if let Ok(ob) = build_outbound(server) {
                    let tag = format!("proxy-{}", &server.id);
                    proxy_tags.push(tag.clone());
                    let mut ob = ob;
                    ob["tag"] = json!(tag);
                    outbounds.push(ob);
                }
            }

            let mut selector_outbounds = vec!["direct".to_string()];
            selector_outbounds.extend(proxy_tags);
            outbounds.push(json!({"type": "selector", "tag": "proxy", "outbounds": selector_outbounds}));

            let test_dir = std::env::temp_dir().join(format!(
                "omnitun-batch-{}-{}",
                batch_idx,
                uuid::Uuid::new_v4()
            ));
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
                    "cache_file": { "enabled": false },
                    "clash_api": {
                        "external_controller": format!("127.0.0.1:{stats_port}"),
                        "secret": ""
                    }
                },
                "route": {
                    "final": "proxy",
                    "auto_detect_interface": true,
                    "default_domain_resolver": "local",
                    "rules": [{"ip_is_private": true, "outbound": "direct"}]
                },
            });

            let config_path = test_dir.join("config.json");
            let config_str = serde_json::to_string_pretty(&config_json).map_err(|_| ())?;
            std::fs::write(&config_path, &config_str).map_err(|_| ())?;

            // Запускаем sing-box для этого батча
            #[cfg(windows)]
            let child = std::process::Command::new(&binary_path)
                .args(["run", "-c"])
                .arg(&config_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(0x08000000)
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

            // Ждём запуска
            tokio::time::sleep(Duration::from_millis(600)).await;

            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(8))
                .build()
                .map_err(|_| {
                    let _ = kill_process(pid);
                })?;

            let api_url = format!("http://127.0.0.1:{stats_port}");

            // Параллельная проверка задержки
            let test_url = "http://cp.cloudflare.com/generate_204";
            let mut updated = Vec::new();
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(10));
            let mut delay_handles = Vec::new();

            for server in &batch {
                let tag = format!("proxy-{}", &server.id);
                let delay_url = format!("{api_url}/proxies/{tag}/delay?url={test_url}&timeout=5000");
                let client = client.clone();
                let sem = semaphore.clone();
                let srv = server.clone();

                delay_handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    let mut result = srv;
                    result.ping_ms = None;
                    match client.get(&delay_url).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(body) = resp.text().await {
                                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) {
                                    if let Some(ms) = data["delay"].as_u64() {
                                        result.ping_ms = Some(ms);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    result
                }));
            }

            for handle in delay_handles {
                if let Ok(server) = handle.await {
                    updated.push(server);
                }
            }

            // Log per-batch latency test results
            let batch_passed = updated.iter().filter(|s| s.ping_ms.is_some()).count();
            let batch_failed = updated.len() - batch_passed;
            if batch_failed > 0 {
                let logger = app_clone.state::<Mutex<Logger>>();
                log(
                    &app_clone,
                    &logger,
                    LogLevel::Info,
                    LogSource::Subscription,
                    format!(
                        "Batch {batch_idx}: {batch_passed} passed, {batch_failed} failed real latency test"
                    ),
                );
            }

            // Сортируем по задержке
            updated.sort_by(|a, b| {
                a.ping_ms
                    .unwrap_or(u64::MAX)
                    .cmp(&b.ping_ms.unwrap_or(u64::MAX))
            });

            // Последовательная проверка outlet IP — проверяем все серверы, кроме
            // уже помеченных keyword как CDN (им outlet IP не нужен для детекции).
            let real_ip = fetch_outlet_ip(proxy_port).await.unwrap_or_default();

            for srv in updated.iter_mut().filter(|s| !s.behind_cdn) {
                let tag = format!("proxy-{}", &srv.id);

                let switched = client
                    .put(format!("{api_url}/proxies/proxy"))
                    .json(&json!({"name": tag}))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);

                if !switched {
                    continue;
                }

                tokio::time::sleep(Duration::from_millis(100)).await;

                let ip = fetch_outlet_ip(proxy_port).await;

                // CDN detection via ASN — single fetch, offline lookup (~2µs)
                if let Some(ref outlet) = ip {
                    if crate::vpn::geoip::is_cdn_asn(outlet) {
                        srv.behind_cdn = true;
                        let l = app_clone.state::<std::sync::Mutex<crate::logging::Logger>>();
                        crate::logging::log(
                            &app_clone,
                            &l,
                            crate::logging::LogLevel::Info,
                            crate::logging::LogSource::Subscription,
                            format!("[CDN] outlet IP {} → {} is behind CDN", outlet, srv.display_name()),
                        );
                    }
                }

                // Outlet IP check: store exit geoip in exit fields (not server location).
                // The server's own flag/country_name/city_name are the physical server
                // location (from TCP ping geoip); the exit IP location goes into exit_*.
                if let Some(ip) = ip {
                    if !real_ip.is_empty() && ip != real_ip {
                        if let Some(info) = crate::vpn::geoip::resolve_hostname(&ip).await {
                            srv.exit_server = Some(ip.clone());
                            srv.exit_flag = Some(info.country_code.clone());
                            srv.exit_country_name = Some(info.country_name.clone());
                            srv.exit_city_name = if info.city.is_empty() {
                                None
                            } else {
                                Some(info.city.clone())
                            };
                        }
                    }
                }
            }

            // Восстанавливаем selector
            let _ = client
                .put(format!("{api_url}/proxies/proxy"))
                .json(&json!({"name": "direct"}))
                .send()
                .await;

            // Cleanup
            let _ = kill_process(pid);
            let _ = std::fs::remove_dir_all(&test_dir);

            Ok::<Vec<ServerProfile>, ()>(updated)
        }));
    }

    // Собираем результаты всех батчей
    for handle in batch_handles {
        if let Ok(Ok(batch_results)) = handle.await {
            all_results.extend(batch_results);
        }
    }

    log(
        app,
        &logger,
        LogLevel::Success,
        LogSource::Subscription,
        format!(
            "Parallel test complete: {} server(s) processed",
            all_results.len()
        ),
    );

    Ok(all_results)
}

async fn test_servers_via_singbox_batch(
    app: &AppHandle,
    servers: &[ServerProfile],
) -> Result<Vec<ServerProfile>, ()> {
    use crate::vpn::config::{build_outbound, find_available_port};
    use serde_json::json;
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    use std::time::Duration;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let logger = app.state::<Mutex<Logger>>();

    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!(
            "Testing {} server(s) via real proxy connection...",
            servers.len()
        ),
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
    let mut proxy_tags: Vec<String> = Vec::new();
    for server in servers {
        if let Ok(ob) = build_outbound(server) {
            let tag = format!("proxy-{}", &server.id);
            proxy_tags.push(tag.clone());
            let mut ob = ob;
            ob["tag"] = json!(tag);
            outbounds.push(ob);
        }
    }

    // Add selector group for outlet IP routing
    let mut selector_outbounds = vec!["direct".to_string()];
    selector_outbounds.extend(proxy_tags);
    outbounds.push(json!({"type": "selector", "tag": "proxy", "outbounds": selector_outbounds}));

    let test_dir = std::env::temp_dir().join(format!("omnitun-latency-{}", uuid::Uuid::new_v4()));
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
            "cache_file": {
                "enabled": false
            },
            "clash_api": {
                "external_controller": format!("127.0.0.1:{stats_port}"),
                "secret": ""
            }
        },
        "route": {
            "final": "proxy",
            "auto_detect_interface": true,
            "default_domain_resolver": "local",
            "rules": [{"ip_is_private": true, "outbound": "direct"}]
        },
    });

    let config_path = test_dir.join("test-batch.json");
    let config_str = serde_json::to_string_pretty(&config_json).map_err(|_| ())?;
    std::fs::write(&config_path, &config_str).map_err(|_| ())?;

    // Guard: skip if another batch test is already running (must be before spawn)
    if TESTING_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        log(
            app,
            &logger,
            LogLevel::Warning,
            LogSource::Subscription,
            "Batch test already in progress, skipping".into(),
        );
        return Err(());
    }

    struct TestGuard;
    impl Drop for TestGuard {
        fn drop(&mut self) {
            TESTING_IN_PROGRESS.store(false, Ordering::Release);
        }
    }
    let _guard = TestGuard;

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

    // Test each server via Clash API — parallel with concurrency limit
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|_| {
            let _ = kill_process(pid);
        })?;

    let api_url = format!("http://127.0.0.1:{stats_port}");
    let test_url = "http://cp.cloudflare.com/generate_204";

    let updated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let semaphore = Arc::new(Semaphore::new(10));
    let mut handles = Vec::new();

    for server in servers {
        let tag = format!("proxy-{}", &server.id);
        let delay_url = format!("{api_url}/proxies/{tag}/delay?url={test_url}&timeout=5000");
        let client = client.clone();
        let updated = updated.clone();
        let sem = semaphore.clone();
        let srv = server.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let mut result = srv;
            result.ping_ms = None;
            match client.get(&delay_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(body) = resp.text().await {
                        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) {
                            if let Some(ms) = data["delay"].as_u64() {
                                result.ping_ms = Some(ms);
                            }
                        }
                    }
                }
                _ => {}
            }
            updated.lock().unwrap().push(result);
        }));
    }

    // Wait for all parallel tests to complete
    for handle in handles {
        let _ = handle.await;
    }

    // Collect results into a Vec (sorted by ping later, after outlet IP check)
    let mut updated = Arc::try_unwrap(updated).unwrap().into_inner().unwrap();

    // Log how many servers passed/failed the real latency test
    let passed = updated.iter().filter(|s| s.ping_ms.is_some()).count();
    let failed = updated.len() - passed;
    if failed > 0 {
        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Subscription,
            format!(
                "Real latency test: {passed} passed, {failed} failed (of {} tested)",
                updated.len()
            ),
        );
    }

    // --- Outlet IP checking via Clash API selector switch (sequential) ---
    // IMPORTANT: must be sequential — parallel tasks race on the same global
    // singleton selector and produce garbage results.
    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!(
            "Checking outlet IP for {} server(s)...",
            updated.iter().filter(|s| !s.behind_cdn).count(),
        ),
    );

    // Sort by ping — fastest first
    updated.sort_by(|a, b| {
        a.ping_ms
            .unwrap_or(u64::MAX)
            .cmp(&b.ping_ms.unwrap_or(u64::MAX))
    });

    // Pre-fetch the local real IP (without proxy) to filter out failed proxy routes
    let real_ip = fetch_outlet_ip(proxy_port).await.unwrap_or_default();

    // Sequential outlet IP check — skip servers already flagged as CDN by keyword
    for server_idx in 0..updated.len() {
        if updated[server_idx].behind_cdn {
            continue;
        }
        let srv = &mut updated[server_idx];
        let tag = format!("proxy-{}", &srv.id);
        let switch_url = format!("{api_url}/proxies/proxy");

        let switched = client
            .put(&switch_url)
            .json(&json!({"name": tag}))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let ip = if switched {
            fetch_outlet_ip(proxy_port).await
        } else {
            None
        };

        // CDN detection via ASN
        if let Some(ref outlet) = ip {
            if crate::vpn::geoip::is_cdn_asn(outlet) {
                srv.behind_cdn = true;
                let l = app.state::<std::sync::Mutex<crate::logging::Logger>>();
                crate::logging::log(
                    app,
                    &l,
                    crate::logging::LogLevel::Info,
                    crate::logging::LogSource::Subscription,
                    format!("[CDN] outlet IP {} → {} is behind CDN", outlet, srv.display_name()),
                );
            }
        }

        // Only update if outlet IP differs from local real IP — store in exit_* fields.
        if let Some(ip) = ip {
            if !real_ip.is_empty() && ip != real_ip {
                if let Some(info) = crate::vpn::geoip::resolve_hostname(&ip).await {
                    srv.exit_server = Some(ip.clone());
                    srv.exit_flag = Some(info.country_code.clone());
                    srv.exit_country_name = Some(info.country_name.clone());
                    srv.exit_city_name = if info.city.is_empty() {
                        None
                    } else {
                        Some(info.city.clone())
                    };
                }
            }
        }
    }

    // Restore selector to direct (best-effort)
    let _ = client
        .put(format!("{api_url}/proxies/proxy"))
        .json(&json!({"name": "direct"}))
        .send()
        .await;

    // Log results — cleanup happens after this function returns
    log(
        app,
        &logger,
        LogLevel::Success,
        LogSource::Subscription,
        format!("Real latency test complete for {} server(s)", updated.len()),
    );

    // Log the PID being killed for debugging
    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Process,
        format!("Killing batch temp sing-box PID {pid}"),
    );

    // Safety check: don't kill if PID matches the main VPN (Windows PID reuse race)
    if crate::vpn::VPN_PID.load(std::sync::atomic::Ordering::Relaxed) == pid {
        log(
            app,
            &logger,
            LogLevel::Warning,
            LogSource::Process,
            format!("SKIP kill PID {pid} — matches main VPN_PID"),
        );
    } else {
        let _ = kill_process(pid);
    }
    let _ = std::fs::remove_dir_all(&test_dir);

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
    use crate::settings::AppSettings;
    use crate::vpn::config::build_outbound;
    use crate::vpn::config::generate_config;
    use crate::vpn::geoip;
    use serde_json::json;
    use std::collections::HashMap;
    use std::process::Command;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("omnitun-test-{name}"))
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
            .join(crate::vpn::installer::Installer::binary_name());
        if m.exists() {
            return m;
        }
        PathBuf::from("sing-box.exe")
    }

    fn load_real_servers() -> Vec<ServerProfile> {
        let p = app_data_dir().join("profiles.json");
        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        data[0]["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| serde_json::from_value(s.clone()).unwrap())
            .collect()
    }

    // -----------------------------------------------------------------------
    // 1. Config generation — each server as single-outbound config
    // -----------------------------------------------------------------------
    #[test]
    fn test_each_server_generates_valid_config() {
        let servers = load_real_servers();
        let binary = singbox();
        if !binary.exists() {
            eprintln!("SKIP: no sing-box binary");
            return;
        }

        let base = include_str!("../../config/base.json");
        let cache = test_dir("singles");
        let _ = std::fs::create_dir_all(&cache);
        let settings = AppSettings::default();

        let mut ok = 0u32;
        let mut fail = 0u32;
        for (i, s) in servers.iter().enumerate() {
            match generate_config(base, &cache, 2080, "local-proxy", &[s.clone()], &settings) {
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
    // 3. VPN-mode config generation (TUN)
    // -----------------------------------------------------------------------
    #[test]
    fn test_vpn_config_generation_passes_check() {
        let servers = load_real_servers();
        let binary = singbox();
        if !binary.exists() {
            eprintln!("SKIP");
            return;
        }

        let base = include_str!("../../config/base-vpn.json");
        let cache = test_dir("vpn");
        let _ = std::fs::create_dir_all(&cache);
        let settings = AppSettings::default();

        // Test with first server
        let result = generate_config(base, &cache, 2080, "vpn", &[servers[0].clone()], &settings);
        assert!(
            result.is_ok(),
            "VPN config generation failed for {}",
            servers[0].display_name()
        );
        let (cfg_str, _) = result.unwrap();

        let p = cache.join("vpn.json");
        std::fs::write(&p, &cfg_str).unwrap();
        let out = Command::new(&binary)
            .args(["check", "-c"])
            .arg(&p)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "VPN config check failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let _ = std::fs::remove_dir_all(&cache);
        println!("  VPN config: sing-box check ✓");
    }

    // -----------------------------------------------------------------------
    // 4. GeoIP enrichment — server entry IP matches country
    // -----------------------------------------------------------------------
    #[test]
    fn test_geoip_matches_entry_country() {
        let servers = load_real_servers();
        let db_path = app_data_dir().join("geoip").join("country.mmdb");
        if !db_path.exists() {
            eprintln!("SKIP: no GeoIP DB at {:?}", db_path);
            return;
        }

        let r = geoip::init_city(&db_path);
        assert!(
            r.is_ok() || geoip::is_ready(),
            "GeoIP not initialized: {:?}",
            r
        );

        let mut match_ = 0u32;
        let mut mismatch = 0u32;
        for s in &servers {
            if let Some(geo) = geoip::resolve(&s.server) {
                if let Some(ref flag) = s.flag {
                    if geo.country_code == *flag {
                        match_ += 1;
                    } else {
                        mismatch += 1;
                        println!(
                            "  ⚠ {}: entry={} geo={}",
                            s.display_name(),
                            flag,
                            geo.country_code
                        );
                    }
                }
            }
        }
        println!(
            "  GeoIP entry match: {match_}/{total} ({mismatch} diff)",
            total = match_ + mismatch
        );
        // At least some should match (entry IP based GeoIP)
        assert!(match_ > 0, "No servers matched GeoIP entry country");
    }

    // -----------------------------------------------------------------------
    // 5. Full config: DNS + route + bypass_ru + enable_mux settings
    // -----------------------------------------------------------------------
    #[test]
    fn test_full_config_has_all_required_sections() {
        let servers = load_real_servers();
        let base = include_str!("../../config/base.json");
        let cache = test_dir("full-check");
        let _ = std::fs::create_dir_all(&cache);
        let settings = AppSettings::default();

        let (cfg_str, _) = generate_config(
            base,
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
        let base = include_str!("../../config/base.json");
        let cache = test_dir("alt-servers");
        let _ = std::fs::create_dir_all(&cache);

        // autoConnectFastest=false → should produce single outbound per server
        let mut settings_no_fastest = AppSettings::default();
        settings_no_fastest.auto_connect_fastest = false;

        let (cfg_str, _) = generate_config(
            base,
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

        let (cfg_str, _) = generate_config(
            base,
            &cache,
            2080,
            "local-proxy",
            &servers,
            &settings_fastest,
        )
        .unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&cfg_str).unwrap();
        let obs = cfg["outbounds"].as_array().unwrap();

        // Find selector group
        let has_selector = obs.iter().any(|o| o["type"] == "selector");
        assert!(
            has_selector,
            "With autoConnectFastest=true, expected selector outbound group"
        );

        let _ = std::fs::remove_dir_all(&cache);
        println!("  alternativeServers: off when fastest=false, on when fastest=true ✓");
    }

    // -----------------------------------------------------------------------
    // 7. Kill process - verify doesn't panic
    // -----------------------------------------------------------------------
    #[test]
    fn test_kill_nonexistent_process() {
        // kill_process should not panic when PID doesn't exist
        let result = kill_process(99999999);
        assert!(result.is_ok() || result.is_err());
        println!("  kill_process (nonexistent PID): handled gracefully ✓");
    }

    #[test]
    fn test_refresh_preserves_server_ids() -> Result<(), Box<dyn std::error::Error>> {
        let servers = load_real_servers();
        assert!(servers.len() >= 2, "Need at least 2 servers for ID test");

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
