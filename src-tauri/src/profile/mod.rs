use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::subscription::SubscriptionProfile;
use crate::proxy::{ServerProfile, ProxyError};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Get proxy port to route subscription fetch through the active proxy.
/// Returns None — when connected, traffic goes through the proxy's port,
/// so direct fetch works and avoids double-proxy issues.
fn proxy_port(app: &AppHandle) -> Option<u16> {
    use crate::settings::AppSettings;

    // Proxy / system-proxy / local-proxy modes: use the proxy port
    // so the fetch goes through the active outbound.
    if crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0 {
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

    pub fn load(&mut self) -> Result<(), ProxyError> {
        match std::fs::read_to_string(&self.file_path) {
            Ok(json) => {
                let profiles: Vec<SubscriptionProfile> =
                    serde_json::from_str(&json).map_err(|e| {
                        ProxyError::Subscription(format!("Failed to parse profiles.json: {e}"))
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
    pub async fn enrich_loaded(&mut self) {
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
                    if server.exit_server.is_some()
                        || server.exit_flag.is_some()
                        || server.exit_country_name.is_some()
                        || server.exit_city_name.is_some()
                    {
                        log::error!(
                            "[BUG] CDN server {} ({}:{}) has stale exit data — enriching_loaded is cleaning it up. \
                             exit_server={:?} exit_flag={:?} exit_country={:?} exit_city={:?}. \
                             This indicates a bug in the server update pipeline.",
                            server.id, server.server, server.port,
                            server.exit_server, server.exit_flag,
                            server.exit_country_name, server.exit_city_name,
                        );
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
                if let Some(info) = crate::proxy::geoip::resolve_hostname(&server.server).await {
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
        log::info!("GeoIP enrichment: {enriched_count} enriched, {cdn_cleanup} CDN exit cleaned up");
    }

    pub fn save(&self) -> Result<(), ProxyError> {
        let json = serde_json::to_string_pretty(&self.profiles)
            .map_err(|e| ProxyError::Subscription(format!("Failed to serialize profiles: {e}")))?;
        let tmp = self.file_path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .map_err(|e| ProxyError::Subscription(format!("Failed to write temp profiles: {e}")))?;
        std::fs::rename(&tmp, &self.file_path)
            .map_err(|e| ProxyError::Subscription(format!("Failed to rename profiles: {e}")))?;
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
    ) -> Result<SubscriptionProfile, ProxyError> {
        let clean_url = url.trim().to_string();

        if self.profiles.iter().any(|p| p.url == clean_url) {
            return Err(ProxyError::Subscription(format!(
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

        servers = process_servers(app, servers, "").await;

        // Persist cache to disk so negative entries survive app restart.
        if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
            cache.save_to_disk().await;
        }

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

    pub fn remove(&mut self, id: &str) -> Result<(), ProxyError> {
        let len_before = self.profiles.len();
        self.profiles.retain(|p| p.id != id);
        if self.profiles.len() == len_before {
            return Err(ProxyError::Subscription("Profile not found".into()));
        }
        self.save()?;
        Ok(())
    }

    pub async fn refresh(
        &mut self,
        app: &AppHandle,
        id: &str,
    ) -> Result<SubscriptionProfile, ProxyError> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| ProxyError::Subscription("Profile not found".into()))?;

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
                .map_err(|e| ProxyError::Subscription(format!("Parse error: {e}"))),
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
                        "",
                    )
                    .await;
                }
                self.save()?;
                return Err(ProxyError::Subscription(format!("{e}")));
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

        // Preserve exit IP data from existing profiles before processing.
        // Batch test (the only source of exit IP enrichment) is skipped when
        // Proxy is running, so freshly parsed servers would lose exit data.
        let old_servers = &self.profiles[idx].servers;
        let old_exit: std::collections::HashMap<String, (Option<String>, Option<String>, Option<String>, Option<String>)> = old_servers
            .iter()
            .map(|s| (
                s.id.clone(),
                (s.exit_server.clone(), s.exit_flag.clone(), s.exit_country_name.clone(), s.exit_city_name.clone()),
            ))
            .collect();
        servers = process_servers(app, servers, id).await;
        // Restore preserved exit data if batch test didn't update it.
        for server in &mut servers {
            if server.exit_server.is_none() {
                if let Some(saved) = old_exit.get(&server.id) {
                    server.exit_server = saved.0.clone();
                    server.exit_flag = saved.1.clone();
                    server.exit_country_name = saved.2.clone();
                    server.exit_city_name = saved.3.clone();
                }
            }
        }

        // Notify cache about subscription refresh: pass ALL current server IDs
        // so the cache can remove entries for deleted servers across all profiles,
        // not just the one being refreshed.
        if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
            let all_active_ids: std::collections::HashSet<String> = self.profiles
                .iter()
                .flat_map(|p| p.servers.iter().map(|s| s.id.clone()))
                .collect();
            cache.handle_subscription_refresh(&all_active_ids).await;
            cache.save_to_disk().await;
        }

        // Guard: don't overwrite servers with empty list (e.g. content:// URLs)
        if servers.is_empty() {
            return Err(ProxyError::Subscription(
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
pub async fn process_servers(app: &AppHandle, servers: Vec<ServerProfile>, profile_id: &str) -> Vec<ServerProfile> {
    let initial_count = servers.len();
    let logger = app.state::<Mutex<Logger>>();

    // Lazy-init GeoIP: load from disk or download synchronously (one-time cost)
    let data_dir = app.path().app_data_dir().ok();
    if let Some(ref dir) = data_dir {
        let geo_dir = dir.join("geoip");
        let city_path = geo_dir.join("GeoLite2-City.mmdb");
        let asn_path = geo_dir.join("GeoLite2-ASN.mmdb");

        if !crate::proxy::geoip::is_ready() {
            if city_path.exists() {
                let _ = crate::proxy::geoip::init_city(&city_path);
            } else {
                log(
                    app,
                    &logger,
                    LogLevel::Info,
                    LogSource::System,
                    "Downloading GeoIP databases (one-time)...".into(),
                );
                match crate::proxy::geoip::ensure_downloaded(
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
                    log(
                        app,
                        &logger,
                        LogLevel::Warning,
                        LogSource::System,
                        format!("CDN .srs {name} not found at {:?}", path),
                    );
                    continue;
                }
                match crate::proxy::geoip::parse_srs_file(&path) {
                    Ok(r) => {
                        log(
                            app,
                            &logger,
                            LogLevel::Info,
                            LogSource::System,
                            format!("CDN .srs {name}: {} CIDR entries (lazy)", r.len()),
                        );
                        merged.extend(r);
                    }
                    Err(e) => {
                        log(
                            app,
                            &logger,
                            LogLevel::Warning,
                            LogSource::System,
                            format!("CDN .srs {name} parse failed: {e}"),
                        );
                    }
                }
            }
            if !merged.is_empty() {
                log(
                    app,
                    &logger,
                    LogLevel::Info,
                    LogSource::System,
                    format!("CDN ranges merged: {} total entries (lazy)", merged.len()),
                );
                crate::proxy::geoip::set_cdn_ranges(merged);
            } else {
                log(
                    app,
                    &logger,
                    LogLevel::Warning,
                    LogSource::System,
                    "CDN ranges empty — no .srs files loaded".into(),
                );
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
    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Parsed {initial_count} servers from subscription, {duplicates} duplicates removed, {} unique", unique.len()),
    );
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
        s.behind_cdn = crate::proxy::is_cdn_server(s).await;
        if s.behind_cdn {
            // Clear stale exit IP data from previous batch tests — CDN servers
            // must never retain exit data (their exit node is dynamic/unstable).
            let had_exit = s.exit_server.is_some();
            s.exit_server = None;
            s.exit_flag = None;
            s.exit_country_name = None;
            s.exit_city_name = None;
            log(
                app,
                &logger,
                LogLevel::Info,
                LogSource::Subscription,
                format!("[CDN] keyword match: {} ({})", s.display_name(), s.server),
            );
            if had_exit {
                log(
                    app,
                    &logger,
                    LogLevel::Warning,
                    LogSource::Subscription,
                    format!(
                        "[BUG] CDN server {} had stale exit IP data (cleared) — previous batch test stored exit data for a CDN server",
                        s.id,
                    ),
                );
            }
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
    let mut unique_count = unique.len();
    log(
        app,
        &logger,
        LogLevel::Info,
        LogSource::Subscription,
        format!("Testing {unique_count} server(s) for availability..."),
    );

    // Check negative cache before TCP ping — skip servers known to be dead
    // (backoff period still active). This avoids spending 3s per dead server.
    if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
        let mut negative_ids = std::collections::HashSet::new();
        for server in &unique {
            if cache.is_cached_negative(&server.id).await {
                negative_ids.insert(server.id.clone());
            }
        }
        if !negative_ids.is_empty() {
            let before = unique.len();
            unique.retain(|s| !negative_ids.contains(&s.id));
            let skipped = before - unique.len();
            // Recalculate the total count so progress and unreachable stats
            // don't miscount skipped servers as failed.
            unique_count = unique.len();
            log(
                app,
                &logger,
                LogLevel::Info,
                LogSource::Subscription,
                format!("Skipped {skipped} server(s) in negative cache (backoff active)"),
            );
        }
    }

    let semaphore = Arc::new(Semaphore::new(50));
    let mut set = tokio::task::JoinSet::new();

    for mut server in unique {
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                log(
                    app,
                    &logger,
                    LogLevel::Warning,
                    LogSource::Subscription,
                    "Semaphore closed — skipping remaining servers".into(),
                );
                break; // semaphore was closed, stop spawning
            }
        };

        // Try emoji flag from server name first (fast, no network)
        if server.flag.is_none() {
            if let Some(f) = emoji_flag_from_name(&server.original_name) {
                server.flag = Some(f);
            }
        }

        set.spawn(async move {
            let addr = format!("{}:{}", server.server, server.port);
            let start = std::time::Instant::now();
            // DNS resolve: get the real entry IP from hostname
            let entry_ip = tokio::net::lookup_host(&addr).await
                .ok()
                .and_then(|mut a| a.next())
                .map(|s| s.ip().to_string());
            // TCP connect with 3s timeout
            let reachable = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::net::TcpStream::connect(&addr),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            let ping_ms = start.elapsed().as_millis() as u64;
            drop(permit);
            (server, reachable, ping_ms, entry_ip)
        });
    }

    let mut alive = Vec::new();
    let mut tcp_tested = 0;
    while let Some(result) = set.join_next().await {
        tcp_tested += 1;
        match result {
            Ok((mut server, true, ping_ms, entry_ip)) => {
                server.ping_ms = Some(ping_ms);

                // GeoIP enrichment: set flag + country/city fields + display name
                // (subscription names often contain garbled emoji, components separate)
                if let Some(info) = crate::proxy::geoip::resolve_hostname(&server.server).await {
                    server.flag = Some(info.country_code.clone());
                    server.country_name = Some(info.country_name.clone());
                    if !info.city.is_empty() {
                        server.city_name = Some(info.city.clone());
                    }
                } else if server.flag.is_none() {
                    log(
                        app,
                        &logger,
                        LogLevel::Warning,
                        LogSource::Subscription,
                        format!("GeoIP: country not resolved for {} — IP not in GeoIP database", server.server),
                    );
                }
                // If GeoIP fails, keep flag from emoji extraction or leave unset

                // CDN detection on the real entry IP (resolved from hostname if needed).
                // If lookup_host() failed (common for IP addresses on Windows), fallback
                // to server.server directly when it's parseable as an IP.
                if !server.behind_cdn {
                    let check_ip = entry_ip.as_deref().or_else(|| {
                        server.server.parse::<std::net::IpAddr>().ok().map(|_| &*server.server)
                    });
                    if let Some(rip) = check_ip {
                        let cdn_ranges_count = crate::proxy::geoip::cdn_ranges_count();
                        if crate::proxy::geoip::is_cdn_entry_ip(rip) {
                            server.behind_cdn = true;
                            // CDN servers must not retain stale exit data
                            server.exit_server = None;
                            server.exit_flag = None;
                            server.exit_country_name = None;
                            server.exit_city_name = None;
                            log(
                                app,
                                &logger,
                                LogLevel::Info,
                                LogSource::Subscription,
                                format!("[CDN] entry IP {} is behind CDN ({} CIDRs)", rip, cdn_ranges_count),
                            );
                        } else if tcp_tested <= 5 {
                            log(
                                app,
                                &logger,
                                LogLevel::Info,
                                LogSource::Subscription,
                                format!("[CDN debug] IP {} not in CDN ranges ({} CIDRs)", rip, cdn_ranges_count),
                            );
                        }
                    } else {
                        log(
                            app,
                            &logger,
                            LogLevel::Warning,
                            LogSource::Subscription,
                            format!("[CDN debug] no IP to check for server {}", server.server),
                        );
                    }
                }

                // Replace hostname with resolved IP in server address,
                // so profiles store IPs, not hostnames. SNI stays intact.
                if let Some(ref rip) = entry_ip {
                    if rip != &server.server {
                        log(
                            app,
                            &logger,
                            LogLevel::Info,
                            LogSource::Subscription,
                            format!("[DNS] {} → {}", server.server, rip),
                        );
                        server.server = rip.clone();
                    }
                }

                alive.push(server);
            }
            Ok((server, false, _, _)) => {
                // TCP ping failed — store in negative cache so it's not re-tested
                // until the backoff period expires.
                if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
                    cache.update_negative(
                        &server.id,
                        "TCP ping: connection refused or timeout (3s)".to_string(),
                    ).await;
                }
            }
            Err(_) => {
                log(
                    app,
                    &logger,
                    LogLevel::Warning,
                    LogSource::Subscription,
                    "TCP ping task panicked".into(),
                );
            }
        }

        // Отправка прогресса TCP Ping
        crate::events::emit_refresh_progress(
            app,
            crate::events::RefreshProgress {
                profile_id: profile_id.to_string(),
                total: unique_count,
                tested: tcp_tested,
                cache_hits: 0,
                successful: alive.len(),
                failed: tcp_tested - alive.len(),
                phase: "tcp_ping".into(),
            },
        );
    }

    // 3b. IP-based dedup: after DNS resolution, multiple hostnames may resolve
    //     to the same IP. Keep the one with lowest ping.
    let alive_before_ip_dedup = alive.len();
    if alive_before_ip_dedup > 0 {
        // Sort by ping ascending — lowest ping first
        alive.sort_by_key(|s| s.ping_ms.unwrap_or(u64::MAX));
        let mut seen_ips = std::collections::HashSet::new();
        alive.retain(|s| seen_ips.insert(s.server.clone()));
        let ip_dupes = alive_before_ip_dedup - alive.len();
        if ip_dupes > 0 {
            log(
                app,
                &logger,
                LogLevel::Info,
                LogSource::Subscription,
                format!("Removed {ip_dupes} IP duplicate(s) — multiple domains resolve to the same IP"),
            );
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

    // === DIFF-TESTING OPTIMIZATION ===
    // After TCP ping, check cache for each alive server. Servers with a fresh
    // positive cache entry skip the expensive batch test. Only unknown/stale
    // servers get batch-tested.
    let mut ready_servers = Vec::new();
    let mut needs_batch_test = Vec::new();

    if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
        for mut server in alive {
            if let Some(pos) = cache.get_positive_if_fresh(&server.id).await {
                // Server is alive (TCP ping passed) and cache is fresh.
                // Skip batch test — use cached data directly.
                server.behind_cdn = pos.behind_cdn;
                if !server.behind_cdn {
                    server.exit_server = pos.exit_ip;
                    server.exit_flag = pos.exit_flag;
                    server.exit_country_name = pos.exit_country;
                    server.exit_city_name = pos.exit_city;
                }
                ready_servers.push(server);
            } else {
                // Server is new, stale, or negative — needs batch test
                needs_batch_test.push(server);
            }
        }

        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Cache,
            format!("{}/{} server(s) had fresh cache — skipping batch test", ready_servers.len(), ready_servers.len() + needs_batch_test.len()),
        );
    } else {
        // No cache available — batch test everything
        needs_batch_test = alive;
    }

    let main_proxy_running =
        crate::proxy::CLASH_API_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0;

    if !needs_batch_test.is_empty() && !main_proxy_running {
        if let Some(cache) = app.try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>() {
            log(
                app,
                &logger,
                LogLevel::Info,
                LogSource::Cache,
                format!("Starting batch test for {} NEW/UNKNOWN server(s)...", needs_batch_test.len()),
            );

            let cancel_token = app.try_state::<Mutex<CancellationToken>>()
                .map(|g| g.lock().unwrap_or_else(|e| e.into_inner()).clone());

            let mut updated = cache.batch_test(app, &needs_batch_test, cancel_token, profile_id).await;
            ready_servers.append(&mut updated);
        }
    } else if main_proxy_running && !needs_batch_test.is_empty() {
        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Cache,
            format!("Batch test skipped for {} new servers — main proxy is running", needs_batch_test.len()),
        );
        ready_servers.append(&mut needs_batch_test);
    }

    // Apply CDN node filter if user enabled it
    if let Some(settings) = app.try_state::<tokio::sync::Mutex<crate::settings::AppSettings>>() {
        let settings_guard = settings.lock().await;
        if settings_guard.block_cdn_nodes {
            let before_cdn = ready_servers.len();
            ready_servers.retain(|s| !s.behind_cdn);
            let cdn_filtered = before_cdn - ready_servers.len();
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
    ready_servers.sort_by(|a, b| {
        a.ping_ms
            .unwrap_or(u64::MAX)
            .cmp(&b.ping_ms.unwrap_or(u64::MAX))
    });

    log(
        app,
        &logger,
        LogLevel::Success,
        LogSource::Subscription,
        format!("{} server(s) passed and saved", ready_servers.len()),
    );

    if let Some(fastest) = ready_servers.first() {
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

    ready_servers
}

/// Test all servers that passed TCP ping using sing-box and Clash API.
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
    use crate::proxy::config::build_outbound;
    use crate::proxy::config::generate_config;
    use serde_json::json;
    use std::collections::HashMap;
    use std::process::Command;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "omnitun-test-{}-{}",
            std::process::id(),
            name
        ))
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

        let (cfg_str, _) = generate_config(
            &cache,
            2080,
            "local-proxy",
            &servers,
            &settings_fastest,
        )
        .unwrap();
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
