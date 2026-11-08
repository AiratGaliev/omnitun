use crate::db::DbPool;
use crate::proxy::{ErrorCode, ProxyError, ServerProfile};
use crate::subscription::SubscriptionProfile;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, Semaphore};
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
    /// The async DbPool — shared with process_servers_incremental.
    db: DbPool,
    /// Path to the data directory (used to open the sync SQLite connection).
    _data_dir: PathBuf,
    /// Synchronous connection for load/save operations.
    /// Wrapped in `Mutex` because `rusqlite::Connection` is `!Sync`
    /// (it uses `RefCell` internally). The outer `Arc<Mutex<ProfileManager>>`
    /// guarantees exclusive access, but the type system requires `Sync`
    /// for MCP server's `&ServerState` across `.await` points.
    sync_conn: std::sync::Mutex<rusqlite::Connection>,
}

impl ProfileManager {
    pub fn new(data_dir: PathBuf, db: DbPool) -> Self {
        // Open a synchronous connection for load/save using the same DB path.
        // If the async connection was created via init_db, the DB file already exists.
        let db_path = data_dir.join("omnitun.db");

        // Ensure the directory exists for the DB file
        let _ = std::fs::create_dir_all(&data_dir);

        let sync_conn = match rusqlite::Connection::open(&db_path) {
            Ok(c) => {
                // Apply schema if tables don't exist yet (fresh DB)
                let _ = c.execute_batch(
                    "PRAGMA journal_mode=WAL;
                     PRAGMA synchronous=NORMAL;
                     PRAGMA busy_timeout=5000;
                     PRAGMA foreign_keys=ON;",
                );
                // Try to create tables — fails silently if they already exist
                let _ = c.execute_batch(crate::db::migrations::CREATE_TABLES_SQL);
                c
            }
            Err(e) => {
                tracing::error!(target: "db", "Failed to open sync DB connection: {e}");
                // Use an in-memory DB as fallback — profiles will be empty
                let c = rusqlite::Connection::open_in_memory()
                    .expect("Failed to create in-memory DB fallback");
                let _ = c.execute_batch(crate::db::migrations::CREATE_TABLES_SQL);
                c
            }
        };

        Self {
            profiles: Vec::new(),
            db,
            _data_dir: data_dir,
            sync_conn: std::sync::Mutex::new(sync_conn),
        }
    }

    pub fn load(&mut self) -> Result<(), ProxyError> {
        // Use the synchronous connection — lock once for all queries.
        // Recover from poisoned mutex — see save_to_disk rationale.
        let conn = self.sync_conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare(
                "SELECT id, name, url, updated_at, last_tested,
                    traffic_used, traffic_total, traffic_expiry
             FROM profiles ORDER BY updated_at DESC",
            )
            .map_err(|e| {
                ProxyError::subscription(
                    ErrorCode::SubscriptionSaveFailed,
                    format!("Failed to query profiles: {e}"),
                )
            })?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>("id")?,
                    row.get::<_, String>("name")?,
                    row.get::<_, String>("url")?,
                    row.get::<_, String>("updated_at")?,
                    row.get::<_, Option<String>>("last_tested")?,
                    row.get::<_, Option<u64>>("traffic_used")?,
                    row.get::<_, Option<u64>>("traffic_total")?,
                    row.get::<_, Option<String>>("traffic_expiry")?,
                ))
            })
            .map_err(|e| {
                ProxyError::subscription(
                    ErrorCode::SubscriptionSaveFailed,
                    format!("Failed to query profiles: {e}"),
                )
            })?;

        // Collect profile tuples first — drop the rows/stmt before querying servers
        #[allow(clippy::type_complexity)]
        let mut profile_tuples: Vec<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<u64>,
            Option<u64>,
            Option<String>,
        )> = Vec::new();
        for row in rows {
            let (
                id,
                name,
                url,
                updated_at,
                last_tested,
                traffic_used,
                traffic_total,
                traffic_expiry,
            ) = row.map_err(|e| {
                ProxyError::subscription(
                    ErrorCode::SubscriptionSaveFailed,
                    format!("Failed to read profile row: {e}"),
                )
            })?;
            profile_tuples.push((
                id,
                name,
                url,
                updated_at,
                last_tested,
                traffic_used,
                traffic_total,
                traffic_expiry,
            ));
        }

        // Drop stmt so conn is available for the server queries
        drop(stmt);

        // Now query servers for each profile (separate passes, no borrow conflict)
        let mut profiles: Vec<SubscriptionProfile> = Vec::with_capacity(profile_tuples.len());
        for (id, name, url, updated_at, last_tested, traffic_used, traffic_total, traffic_expiry) in
            &profile_tuples
        {
            let mut srv_stmt = conn
                .prepare("SELECT * FROM servers WHERE profile_id = ?1 ORDER BY ping_ms ASC")
                .map_err(|e| {
                    ProxyError::subscription(
                        ErrorCode::SubscriptionSaveFailed,
                        format!("Failed to query servers: {e}"),
                    )
                })?;
            let servers = srv_stmt
                .query_map(rusqlite::params![id], map_server_row)
                .map_err(|e| {
                    ProxyError::subscription(
                        ErrorCode::SubscriptionSaveFailed,
                        format!("Failed to query servers: {e}"),
                    )
                })?;
            let mut server_vec = Vec::new();
            for s in servers {
                match s {
                    Ok(srv) => server_vec.push(srv),
                    Err(e) => {
                        return Err(ProxyError::subscription(
                            ErrorCode::SubscriptionSaveFailed,
                            format!("Failed to read server row: {e}"),
                        ))
                    }
                }
            }
            profiles.push(SubscriptionProfile {
                id: id.clone(),
                name: name.clone(),
                url: url.clone(),
                updated_at: updated_at.clone(),
                last_tested: last_tested.clone(),
                traffic_used: *traffic_used,
                traffic_total: *traffic_total,
                traffic_expiry: traffic_expiry.clone(),
                servers: server_vec,
            });
        }
        self.profiles = profiles;
        Ok(())
    }

    /// Save in-memory profiles to SQLite.
    /// Uses the synchronous connection — safe to call from sync contexts.
    ///
    /// ═══ save() — SECONDARY writer for servers, PRIMARY for profiles ═══
    /// OWNERSHIP:
    ///
    ///   - `profiles` table: ONLY save() writes here. No other path touches profiles.
    ///   - `servers` table: SECONDARY writer. Phase 3 POST is the PRIMARY.
    ///     save() rewrites ALL servers across ALL profiles, so it duplicates
    ///     Phase 3 POST's work. This is acceptable because:
    ///       1. save() runs after Phase 3 POST (never before)
    ///       2. save() gracefully skips rows that hit CHECK/NOT NULL constraints
    ///   - `server_cache`: save() does NOT touch this table.
    pub fn save(&mut self) -> Result<(), ProxyError> {
        let mut conn = self.sync_conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(|e| {
            ProxyError::subscription(
                ErrorCode::SubscriptionSaveFailed,
                format!("Failed to start transaction: {e}"),
            )
        })?;

        // Delete ALL existing profiles — we rewrite the full list
        tx.execute("DELETE FROM profiles", []).map_err(|e| {
            ProxyError::subscription(
                ErrorCode::SubscriptionSaveFailed,
                format!("Failed to delete profiles: {e}"),
            )
        })?;

        // Delete ALL existing servers — we rewrite the full list
        tx.execute("DELETE FROM servers", []).map_err(|e| {
            ProxyError::subscription(
                ErrorCode::SubscriptionSaveFailed,
                format!("Failed to delete servers: {e}"),
            )
        })?;

        let mut inserted_ok = std::collections::HashSet::new();

        for profile in &self.profiles {
            // Upsert profile
            tx.execute(
                "INSERT OR REPLACE INTO profiles
                 (id, name, url, updated_at, last_tested,
                  traffic_used, traffic_total, traffic_expiry)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    profile.id,
                    profile.name,
                    profile.url,
                    profile.updated_at,
                    profile.last_tested,
                    profile.traffic_used,
                    profile.traffic_total,
                    profile.traffic_expiry,
                ],
            )
            .map_err(|e| {
                ProxyError::subscription(
                    ErrorCode::SubscriptionSaveFailed,
                    format!("Failed to upsert profile: {e}"),
                )
            })?;

            for server in &profile.servers {
                // If a server violates the CHECK constraint (e.g. behind_cdn=0
                // but exit_server=NULL because outlet IP fetch failed),
                // skip it instead of aborting the entire transaction.
                // 🚫 ANTI-REGRESSION: do NOT remove — prevents data loss
                // when one problematic server blocks all other servers.
                if let Err(e) = tx.execute(
                    "INSERT OR REPLACE INTO servers
                     (profile_id, server_hash, protocol, server, port,
                      uuid, password, method, sni, flow,
                      public_key, private_key,
                      transport_type, transport_path, transport_host,
                      reality_pbk, reality_sid,
                      wg_local_address, wg_allowed_ips, wg_dns, wg_mtu, wg_keepalive,
                      username,
                      ping_ms, behind_cdn,
                      flag, country_name, city_name,
                      exit_server, exit_flag, exit_country_name, exit_city_name,
                      original_name)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                             ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                             ?18, ?19, ?20, ?21, ?22, ?23,
                             ?24, ?25, ?26, ?27, ?28,
                             ?29, ?30, ?31, ?32, ?33)",
                    rusqlite::params![
                        profile.id,
                        server.id, // server_hash = deterministic ID
                        server.protocol,
                        server.server,
                        server.port,
                        server.uuid,
                        server.password,
                        server.method,
                        server.sni,
                        server.flow,
                        server.public_key,
                        server.private_key,
                        server.transport_type,
                        server.transport_path,
                        server.transport_host,
                        server.reality_pbk,
                        server.reality_sid,
                        server.wg_local_address,
                        server.wg_allowed_ips,
                        server.wg_dns,
                        server.wg_mtu,
                        server.wg_keepalive,
                        server.username,
                        server.ping_ms,
                        server.behind_cdn as i32,
                        server.flag,
                        server.country_name,
                        server.city_name,
                        server.exit_server,
                        server.exit_flag,
                        server.exit_country_name,
                        server.exit_city_name,
                        server.original_name,
                    ],
                ) {
                    tracing::warn!(
                        target: "subscription",
                        "Skipping server {}: DB constraint: {e}",
                        server.id,
                    );
                } else {
                    inserted_ok.insert(server.id.clone());
                }
            }
        }

        tx.commit().map_err(|e| {
            ProxyError::subscription(
                ErrorCode::SubscriptionSaveFailed,
                format!("Failed to commit transaction: {e}"),
            )
        })?;

        // Sync in-memory profiles with DB: remove servers that were rejected
        // by constraints (e.g. duplicate server_hash, missing exit data).
        for profile in &mut self.profiles {
            let before = profile.servers.len();
            profile.servers.retain(|s| inserted_ok.contains(&s.id));
            if profile.servers.len() < before {
                tracing::info!(
                    target: "subscription",
                    "Removed {} server(s) from in-memory profile '{}' that failed DB constraint",
                    before - profile.servers.len(),
                    profile.name,
                );
            }
        }

        Ok(())
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

        let cancel_token = app
            .try_state::<std::sync::Arc<std::sync::Mutex<tokio_util::sync::CancellationToken>>>()
            .map(|token| token.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_else(tokio_util::sync::CancellationToken::new);

        if self.profiles.iter().any(|p| p.url == clean_url) {
            return Err(ProxyError::subscription(
                ErrorCode::SubscriptionDuplicateUrl,
                format!("Subscription with URL '{clean_url}' already exists"),
            ));
        }

        let fetch = async {
            if clean_url.starts_with("http://") || clean_url.starts_with("https://") {
                // Try through the existing proxy first, then fall back to direct.
                match crate::subscription::fetch_subscription(&clean_url, proxy_port_from_app(app))
                    .await
                {
                    Ok(servers) => Ok(servers),
                    Err(error) if proxy_port_from_app(app).is_some() => {
                        tracing::error!(
                            target: "subscription",
                            "Proxy fetch failed, retrying direct: {error}",
                        );
                        crate::subscription::fetch_subscription(&clean_url, None).await
                    }
                    Err(error) => Err(error),
                }
            } else {
                crate::subscription::parser::parse_subscription(&clean_url)
            }
        };

        let mut servers = tokio::select! {
            _ = cancel_token.cancelled() => {
                return Err(ProxyError::subscription(
                    ErrorCode::SubscriptionCancelled,
                    "Subscription operation was cancelled",
                ));
            }
            result = fetch => result?,
        };

        if cancel_token.is_cancelled() {
            return Err(ProxyError::subscription(
                ErrorCode::SubscriptionCancelled,
                "Subscription operation was cancelled",
            ));
        }

        // Save profile to SQLite IMMEDIATELY (with empty servers) so the profile
        // persists even if the app is closed during the long batch test below.
        // After process_servers_incremental completes, the in-memory profile is updated
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

        // Run the test — direct call to process_servers_incremental with self.db
        servers = {
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let event_app = app.clone();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    crate::events::emit_server_tested(&event_app, event);
                }
            });
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
                if let Some(cache) = app
                    .try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>()
                    .map(|c| c.as_ref().clone())
                {
                    process_servers_incremental(
                        &data_dir,
                        &settings,
                        servers,
                        &profile_id,
                        cancel_token.clone(),
                        event_tx,
                        Some(progress_tx),
                        &self.db,
                        &cache,
                    )
                    .await
                } else {
                    servers
                }
            } else {
                servers
            }
        };

        if cancel_token.is_cancelled() && servers.is_empty() {
            self.profiles.retain(|profile| profile.id != profile_id);
            self.save()?;
            return Err(ProxyError::subscription(
                ErrorCode::SubscriptionCancelled,
                "Subscription operation was cancelled",
            ));
        }

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

        // Update the existing profile in-place (was saved empty before process_servers_incremental)
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
    /// Called AFTER all long operations (HTTP fetch, process_servers_incremental, traffic
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
        let cache = app
            .try_state::<std::sync::Arc<crate::proxy::cache::ServerCache>>()
            .map(|s| std::sync::Arc::clone(&*s));
        self.commit_refresh_core(id, servers, traffic, cache).await
    }

    pub async fn commit_refresh_headless(
        &mut self,
        id: &str,
        servers: Vec<ServerProfile>,
        cache: Option<std::sync::Arc<crate::proxy::cache::ServerCache>>,
        traffic: Option<(u64, u64, Option<String>)>,
    ) -> Result<SubscriptionProfile, ProxyError> {
        self.commit_refresh_core(id, servers, traffic, cache).await
    }

    async fn commit_refresh_core(
        &mut self,
        id: &str,
        servers: Vec<ServerProfile>,
        traffic: Option<(u64, u64, Option<String>)>,
        cache: Option<std::sync::Arc<crate::proxy::cache::ServerCache>>,
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
        if let Some(cache) = cache {
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
    /// Called by the `test_servers` Tauri command after `clash_api_test`
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
    /// Temp dir created by `spawn_test_engine` (config.json + sing-box logs).
    /// Retained so it can be removed on engine replacement and shutdown —
    /// otherwise every spawn leaks an `omnitun-engine-<uuid>` dir in %TEMP%.
    test_dir: std::path::PathBuf,
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

    /// Best-effort cleanup of the engine's temp dir (config.json + logs).
    /// Idempotent — safe to call multiple times. Never panics.
    fn cleanup_test_dir(&self) {
        if self.test_dir.as_os_str().is_empty() {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.test_dir) {
            // NotFound is expected when already cleaned — only log real failures.
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target: "subscription",
                    "Testing engine: could not remove temp dir {:?}: {e}",
                    self.test_dir
                );
            }
        }
    }
}

/// Map a row from the `servers` table to a `ServerProfile`.
/// Handles all 33 columns with proper Option/NULL handling.
fn map_server_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ServerProfile> {
    Ok(ServerProfile {
        id: row.get("server_hash")?,
        protocol: row.get("protocol")?,
        server: row.get("server")?,
        port: row.get("port")?,
        uuid: row.get("uuid")?,
        password: row.get("password")?,
        method: row.get("method")?,
        sni: row.get("sni")?,
        flow: row.get("flow")?,
        public_key: row.get("public_key")?,
        private_key: row.get("private_key")?,
        transport_type: row.get("transport_type")?,
        transport_path: row.get("transport_path")?,
        transport_host: row.get("transport_host")?,
        reality_pbk: row.get("reality_pbk")?,
        reality_sid: row.get("reality_sid")?,
        wg_local_address: row.get("wg_local_address")?,
        wg_allowed_ips: row.get("wg_allowed_ips")?,
        wg_dns: row.get("wg_dns")?,
        wg_mtu: row.get("wg_mtu")?,
        wg_keepalive: row.get("wg_keepalive")?,
        username: row.get("username")?,
        ping_ms: row.get("ping_ms")?,
        behind_cdn: row.get::<_, Option<i32>>("behind_cdn")?.unwrap_or(0) != 0,
        flag: row.get("flag")?,
        country_name: row.get("country_name")?,
        city_name: row.get("city_name")?,
        exit_server: row.get("exit_server")?,
        exit_flag: row.get("exit_flag")?,
        exit_country_name: row.get("exit_country_name")?,
        exit_city_name: row.get("exit_city_name")?,
        original_name: row.get("original_name")?,
    })
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
                // Engine alive with matching config — check process still runs.
                // Cross-platform: `crate::is_process_alive` uses OpenProcess (Windows)
                // or `kill -0` (Unix). The previous `tasklist` call only worked on
                // Windows and silently returned false on Unix, causing the engine to
                // be needlessly respawned on every clash_api_test call.
                if crate::is_process_alive(engine.pid) {
                    return Some((engine.clash_port, engine.socks_port));
                }
            }
        }
    }

    // Need to start a new engine — kill old one first (frees ports + temp dir)
    {
        let mut guard = TEST_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = guard.take() {
            tracing::info!(target: "subscription", "Testing engine: servers changed, killing old engine (pid {})", old.pid);
            // Cross-platform kill (matches process.rs pattern). Previously only
            // `taskkill` was used, which is a no-op on Unix and left the old
            // engine's sing-box process running.
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/pid", &old.pid.to_string(), "/f"])
                    .output();
            }
            #[cfg(not(windows))]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-9", &old.pid.to_string()])
                    .output();
            }
            // Clean up the replaced engine's temp dir (config.json + logs).
            old.cleanup_test_dir();
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

    if let Some((pid, clash_port, socks_port, pm, test_dir)) = last_result {
        let engine = TestEngine {
            pid,
            clash_port,
            socks_port,
            server_hash: hash,
            pm,
            test_dir,
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
/// Also removes the engine's temp dir (config.json + logs) to avoid the
/// `omnitun-engine-<uuid>` leak described in AUDIT-PLAN BUG-2.
pub(crate) fn shutdown_test_engine() {
    if let Ok(mut guard) = TEST_ENGINE.lock() {
        if let Some(engine) = guard.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let _ = runtime.block_on(engine.pm.stop());
            }
            engine.cleanup_test_dir();
        }
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
) -> Option<(
    u32,
    u16,
    u16,
    crate::proxy::process::ProcessManager,
    std::path::PathBuf,
)> {
    use serde_json::json;
    use std::time::Duration;

    let binary_path = data_dir
        .join("binaries")
        .join(crate::proxy::installer::Installer::binary_name());
    if !binary_path.exists() {
        tracing::warn!(target: "subscription", "Testing engine: sing-box not installed at {:?}", binary_path);
        return None;
    }

    let test_dir = std::env::temp_dir().join(format!("omnitun-engine-{}", uuid::Uuid::new_v4()));
    if std::fs::create_dir_all(&test_dir).is_err() {
        tracing::warn!(target: "subscription", "Testing engine: failed to create temp dir");
        return None;
    }

    // From here `test_dir` exists on disk. Every failure path below MUST
    // clean it up — otherwise each spawn leaks an `omnitun-engine-<uuid>`
    // directory in %TEMP% (see AUDIT-PLAN BUG-2). The helper closure makes
    // every early return a single line and cannot be forgotten.
    let cleanup_dir = |d: &std::path::Path| {
        let _ = std::fs::remove_dir_all(d);
    };

    // Find free ports dynamically (not from PortPool — stale TIME_WAIT ports
    // from failed engines would block sequential port reuse).
    fn find_free_port() -> Option<u16> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        // Drop listener immediately — port is free now
        drop(listener);
        Some(port)
    }
    let (clash_port, socks_port) = match (find_free_port(), find_free_port()) {
        (Some(c), Some(s)) => (c, s),
        _ => {
            cleanup_dir(&test_dir);
            return None;
        }
    };

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
        cleanup_dir(&test_dir);
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
    let config_str = match serde_json::to_string_pretty(&config) {
        Ok(s) => s,
        Err(_) => {
            cleanup_dir(&test_dir);
            return None;
        }
    };
    if std::fs::write(&config_path, config_str).is_err() {
        tracing::warn!(target: "subscription", "Testing engine: failed to write config");
        cleanup_dir(&test_dir);
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
            cleanup_dir(&test_dir);
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
            cleanup_dir(&test_dir);
            return None;
        }
    };

    // Wait briefly and check if process is still alive
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    if !crate::is_process_alive(pid) {
        let stderr_path = test_dir.join("sing-box-stderr.log");
        let stderr_content = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        tracing::warn!(target: "subscription", "Testing engine: process died immediately (pid {pid}):\n{stderr_content}");
        cleanup_dir(&test_dir);
        return None;
    }

    // Wait for Clash API to be ready (up to 15s)
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            cleanup_dir(&test_dir);
            return None;
        }
    };

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
        cleanup_dir(&test_dir);
        return None;
    }

    // Success — transfer test_dir ownership to the caller; it owns cleanup now.
    Some((pid, clash_port, socks_port, pm, test_dir))
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

/// Get old server keys from SQLite for ID preservation.
/// Returns a HashMap of (dedup_key → server_id).
async fn get_old_servers_map(
    profile_id: &str,
    db: &DbPool,
) -> Result<HashMap<String, String>, ProxyError> {
    let pid = profile_id.to_string();
    db.call(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT server_hash, protocol, server, port, uuid, password, method, transport_type
             FROM servers WHERE profile_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![pid], |row| {
            let server_hash: String = row.get(0)?;
            let protocol: String = row.get(1)?;
            let server: String = row.get(2)?;
            let port: u16 = row.get(3)?;
            let uuid: Option<String> = row.get(4)?;
            let password: Option<String> = row.get(5)?;
            let method: Option<String> = row.get(6)?;
            let transport: Option<String> = row.get(7)?;
            let key = format!(
                "{}://{}:{}@{}:{}:{}/{}",
                protocol,
                server,
                port,
                uuid.unwrap_or_default(),
                password.unwrap_or_default(),
                method.unwrap_or_default(),
                transport.unwrap_or_default()
            );
            Ok((key, server_hash))
        })?;
        rows.collect::<rusqlite::Result<HashMap<String, String>>>()
    })
    .await
    .map_err(|e| {
        ProxyError::subscription(
            ErrorCode::SubscriptionSaveFailed,
            format!("Failed to query old servers: {e}"),
        )
    })
}

/// Read old exit data (exit_server, exit_flag, exit_country_name, exit_city_name)
/// from the `servers` table for a given profile. Returns HashMap keyed by server_hash.
///
/// Used by Phase 0.5 in process_servers_incremental to restore exit data that would
/// otherwise be lost when the old servers are DELETEd and re-INSERTed.
type ExitData = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn get_old_servers_exit(profile_id: &str, db: &DbPool) -> HashMap<String, ExitData> {
    let pid = profile_id.to_string();
    db.call(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT server_hash, exit_server, exit_flag, exit_country_name, exit_city_name
             FROM servers WHERE profile_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![pid], |row| {
            let server_hash: String = row.get(0)?;
            let exit_server: Option<String> = row.get(1)?;
            let exit_flag: Option<String> = row.get(2)?;
            let exit_country_name: Option<String> = row.get(3)?;
            let exit_city_name: Option<String> = row.get(4)?;
            Ok((
                server_hash,
                (exit_server, exit_flag, exit_country_name, exit_city_name),
            ))
        })?;
        rows.collect::<rusqlite::Result<HashMap<String, ExitData>>>()
    })
    .await
    .unwrap_or_default()
}

/// Read old ping_ms from the `servers` table for a given profile.
/// Returns HashMap keyed by server_hash.
async fn get_old_servers_ping(profile_id: &str, db: &DbPool) -> HashMap<String, Option<u64>> {
    let pid = profile_id.to_string();
    db.call(move |conn| {
        let mut stmt =
            conn.prepare("SELECT server_hash, ping_ms FROM servers WHERE profile_id = ?1")?;
        let rows = stmt.query_map(rusqlite::params![pid], |row| {
            let server_hash: String = row.get(0)?;
            let ping_ms: Option<u64> = row.get(1)?;
            Ok((server_hash, ping_ms))
        })?;
        rows.collect::<rusqlite::Result<HashMap<String, Option<u64>>>>()
    })
    .await
    .unwrap_or_default()
}

/// Atomically replace one profile's persisted server snapshot.
///
/// Empty or completely rejected snapshots leave the previous rows untouched.
/// Per-row constraint failures are logged and skipped; if no new row can be
/// stored, the transaction is rolled back so the previous snapshot survives.
fn persist_profile_servers(
    conn: &mut rusqlite::Connection,
    profile_id: &str,
    servers: &[ServerProfile],
) -> Result<Vec<String>, rusqlite::Error> {
    if servers.is_empty() {
        return Ok(Vec::new());
    }

    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM servers WHERE profile_id = ?1",
        rusqlite::params![profile_id],
    )?;

    let mut inserted_ids = Vec::with_capacity(servers.len());
    for server in servers {
        let result = tx.execute(
            "INSERT INTO servers
             (profile_id, server_hash, protocol, server, port,
              uuid, password, method, sni, flow,
              public_key, private_key,
              transport_type, transport_path, transport_host,
              reality_pbk, reality_sid,
              wg_local_address, wg_allowed_ips, wg_dns, wg_mtu, wg_keepalive,
              username, ping_ms, behind_cdn, flag, country_name, city_name,
              exit_server, exit_flag, exit_country_name, exit_city_name, original_name)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,
                     ?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,
                     ?29,?30,?31,?32,?33)",
            rusqlite::params![
                profile_id,
                server.id,
                server.protocol,
                server.server,
                server.port,
                server.uuid,
                server.password,
                server.method,
                server.sni,
                server.flow,
                server.public_key,
                server.private_key,
                server.transport_type,
                server.transport_path,
                server.transport_host,
                server.reality_pbk,
                server.reality_sid,
                server.wg_local_address,
                server.wg_allowed_ips,
                server.wg_dns,
                server.wg_mtu,
                server.wg_keepalive,
                server.username,
                server.ping_ms,
                server.behind_cdn as i32,
                server.flag,
                server.country_name,
                server.city_name,
                server.exit_server,
                server.exit_flag,
                server.exit_country_name,
                server.exit_city_name,
                server.original_name,
            ],
        );

        match result {
            Ok(_) => inserted_ids.push(server.id.clone()),
            Err(error) => {
                tracing::error!(
                    target: "subscription",
                    "Server {} skipped during snapshot commit: {error}",
                    server.id
                );
            }
        }
    }

    if inserted_ids.is_empty() {
        tx.rollback()?;
        return Ok(Vec::new());
    }

    tx.commit()?;
    Ok(inserted_ids)
}

#[cfg(test)]
mod snapshot_persistence_tests {
    use super::*;

    fn connection_with_profile() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::migrations::CREATE_TABLES_SQL)
            .unwrap();
        conn.execute(
            "INSERT INTO profiles (id, name, url) VALUES (?1, ?2, ?3)",
            rusqlite::params!["profile-test", "test", "content://test"],
        )
        .unwrap();
        conn
    }

    fn server(id: &str) -> ServerProfile {
        ServerProfile {
            id: id.to_string(),
            protocol: "trojan".to_string(),
            server: "example.com".to_string(),
            port: 443,
            ping_ms: Some(25),
            flag: Some("US".to_string()),
            country_name: Some("United States".to_string()),
            exit_server: Some("203.0.113.10".to_string()),
            ..Default::default()
        }
    }

    fn persisted_ids(conn: &rusqlite::Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT server_hash FROM servers WHERE profile_id = ?1 ORDER BY server_hash")
            .unwrap();
        stmt.query_map(rusqlite::params!["profile-test"], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    #[test]
    fn empty_snapshot_keeps_last_committed_servers() {
        let mut conn = connection_with_profile();
        persist_profile_servers(&mut conn, "profile-test", &[server("old-server")]).unwrap();

        let inserted = persist_profile_servers(&mut conn, "profile-test", &[]).unwrap();

        assert!(inserted.is_empty());
        assert_eq!(persisted_ids(&conn), vec!["old-server".to_string()]);
    }

    #[test]
    fn failed_snapshot_insert_rolls_back_server_deletion() {
        let mut conn = connection_with_profile();
        persist_profile_servers(&mut conn, "profile-test", &[server("old-server")]).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_server_inserts
             BEFORE INSERT ON servers
             BEGIN
                 SELECT RAISE(ABORT, 'forced persistence failure');
             END;",
        )
        .unwrap();

        let inserted =
            persist_profile_servers(&mut conn, "profile-test", &[server("new-server")]).unwrap();

        assert!(inserted.is_empty());
        assert_eq!(persisted_ids(&conn), vec!["old-server".to_string()]);
    }

    #[test]
    fn successful_snapshot_replaces_old_servers_atomically() {
        let mut conn = connection_with_profile();
        persist_profile_servers(&mut conn, "profile-test", &[server("old-server")]).unwrap();

        let inserted =
            persist_profile_servers(&mut conn, "profile-test", &[server("new-server")]).unwrap();

        assert_eq!(inserted, vec!["new-server".to_string()]);
        assert_eq!(persisted_ids(&conn), vec!["new-server".to_string()]);
    }
}

/// Multi-phase server processing with incremental event emission and
/// SQLite persistence.
///
/// Phase 0 (prepare): dedup, ID preservation, cross-profile dedup
/// Phase 0.5 (restore): read exit data + ping from the last committed DB snapshot
/// Phase 1 (cache): emit fresh cached entries from server_cache table
/// Phase 2 (preflight): Clash API ping test + pre-filter dead servers
/// Phase 3 (batch + commit): deep test, then atomically replace the DB snapshot
///
/// Each phase emits `ServerTestedEvent` via `event_tx` for streaming UI updates.
/// When `progress_tx` is `Some`, aggregate `RefreshProgress` is emitted after each
/// phase for the progress bar (caller must forward to `emit_refresh_progress`).
/// Pass `None` for headless contexts (MCP server).
#[allow(clippy::too_many_arguments)]
pub async fn process_servers_incremental(
    data_dir: &Path,
    settings: &crate::settings::AppSettings,
    servers: Vec<ServerProfile>,
    profile_id: &str,
    cancel: CancellationToken,
    event_tx: mpsc::UnboundedSender<crate::events::ServerTestedEvent>,
    progress_tx: Option<mpsc::UnboundedSender<crate::events::RefreshProgress>>,
    db: &DbPool,
    cache: &crate::proxy::cache::ServerCache,
) -> Vec<ServerProfile> {
    let initial_count = servers.len();

    // ═══ PHASE 0: Prepare + INSERT ═══════════════════════════════════
    let old_index = get_old_servers_map(profile_id, db)
        .await
        .unwrap_or_default();

    let unique = tokio::task::spawn_blocking({
        let servers = servers.clone();
        let settings = settings.clone();
        move || {
            let mut servers = servers;
            // ID preservation
            for server in &mut servers {
                let key = format!(
                    "{}://{}:{}@{}:{}:{}/{}",
                    server.protocol,
                    server.server,
                    server.port,
                    server.uuid.as_deref().unwrap_or(""),
                    server.password.as_deref().unwrap_or(""),
                    server.method.as_deref().unwrap_or(""),
                    server.transport_type.as_deref().unwrap_or("")
                );
                if let Some(old_id) = old_index.get(&key) {
                    server.id = old_id.clone();
                }
            }
            // Dedup by key
            let mut seen = std::collections::HashSet::new();
            servers.retain(|s| {
                seen.insert(format!(
                    "{}://{}:{}@{}:{}:{}/{}",
                    s.protocol,
                    s.server,
                    s.port,
                    s.uuid.as_deref().unwrap_or(""),
                    s.password.as_deref().unwrap_or(""),
                    s.method.as_deref().unwrap_or(""),
                    s.transport_type.as_deref().unwrap_or("")
                ))
            });

            // Keyword CDN pre-check
            for s in &mut servers {
                if !s.behind_cdn {
                    let lower_name = s.original_name.to_lowercase();
                    if lower_name.contains("workers.dev") || lower_name.contains("pages.dev") {
                        s.behind_cdn = true;
                        s.assert_no_cdn_exit_data("keyword_cdn");
                    }
                }
            }

            // User filters
            if !settings.allowed_protocols.is_empty() {
                servers.retain(|s| settings.allowed_protocols.contains(&s.protocol));
            }
            if !settings.blocked_protocols.is_empty() {
                servers.retain(|s| !settings.blocked_protocols.contains(&s.protocol));
            }

            servers
        }
    })
    .await
    .unwrap_or_default();
    let mut unique = unique;

    tracing::info!(target: "subscription",
        "Phase 0: {}/{} servers unique after dedup+filter",
        unique.len(), initial_count);

    // Cross-profile dedup: skip servers whose server_hash already belongs to
    // another profile. Query the `servers` table (which stores per-profile
    // servers), filtering out the current profile to avoid self-dedup.
    let known_other_hashes: std::collections::HashSet<String> = db
        .call({
            let pid = profile_id.to_string();
            move |conn| {
                let mut stmt = conn
                    .prepare("SELECT DISTINCT server_hash FROM servers WHERE profile_id != ?1")?;
                let rows = stmt.query_map(rusqlite::params![pid], |row| row.get::<_, String>(0))?;
                let mut hashes = std::collections::HashSet::new();
                for row in rows.flatten() {
                    hashes.insert(row);
                }
                Ok::<_, rusqlite::Error>(hashes)
            }
        })
        .await
        .unwrap_or_default();

    let before = unique.len();
    unique.retain(|s| !known_other_hashes.contains(&s.id));
    if unique.len() < before {
        tracing::info!(target: "subscription",
            "Phase 0: removed {} servers already positive in another subscription",
            before - unique.len());
    }

    // Phase 0.5: read the previous snapshot before final atomic replacement.
    let old_exit = get_old_servers_exit(profile_id, db).await;
    let old_ping = get_old_servers_ping(profile_id, db).await;

    // Preserve the currently committed server snapshot throughout testing.
    // Phase 3 POST replaces it atomically after validated results exist.
    // Replace it atomically in Phase 3 POST only after validated results exist.

    // ═══ PHASE 0.5: Restore exit data + ping from old rows ═══════════
    // Keyed by server_hash (= server.id after ID preservation).
    // Used for init events and cache-hit enrichment below.
    for s in &mut unique {
        if s.behind_cdn {
            continue;
        }
        if s.exit_server.is_none() {
            if let Some(saved) = old_exit.get(&s.id) {
                s.exit_server = saved.0.clone();
                s.exit_flag = saved.1.clone();
                s.exit_country_name = saved.2.clone();
                s.exit_city_name = saved.3.clone();
            }
        }
        if s.ping_ms.is_none() {
            if let Some(ping) = old_ping.get(&s.id) {
                s.ping_ms = *ping;
            }
        }
    }

    // Emit init events — now with restored exit data + ping_ms
    let pid = profile_id.to_string();
    for s in &unique {
        if cancel.is_cancelled() {
            break;
        }
        let _ = event_tx.send(crate::events::ServerTestedEvent {
            profile_id: pid.clone(),
            phase: "init".into(),
            server_id: s.id.clone(),
            static_data: Some(crate::events::ServerStaticData {
                protocol: s.protocol.clone(),
                server: s.server.clone(),
                port: s.port,
                original_name: s.original_name.clone(),
                flag: s.flag.clone(),
                country_name: s.country_name.clone(),
            }),
            ping_ms: s.ping_ms,
            exit_server: s.exit_server.clone(),
            exit_flag: s.exit_flag.clone(),
            exit_country_name: s.exit_country_name.clone(),
            exit_city_name: s.exit_city_name.clone(),
            behind_cdn: Some(s.behind_cdn),
            error: None,
        });
    }

    if cancel.is_cancelled() {
        tracing::info!(target: "subscription", "Refresh cancelled during Phase 0");
        return Vec::new();
    }

    // Progress: Phase 0 complete — emit total
    let total_servers = unique.len();
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(crate::events::RefreshProgress {
            profile_id: profile_id.to_string(),
            total: total_servers,
            tested: 0,
            cache_hits: 0,
            successful: 0,
            failed: 0,
            phase: "dedup".into(),
            updated_at: String::new(),
            last_tested: None,
        });
    }

    // ═══ PHASE 1: Cache hit — via server_cache (no JOIN with servers) ═
    // The previous committed server snapshot remains available until Phase 3 POST.
    // Instead, query server_cache directly for fresh positive entries,
    // then filter in-memory to only those in our `unique` set.
    //
    // Cache-hit servers skip testing and join the final atomic snapshot.
    // Other servers are preflighted and batch-tested before replacement.
    let fresh_cutoff = chrono::Utc::now().timestamp() - 600; // 10 min POSITIVE_STALE_SECS
    let all_fresh_positive: std::collections::HashSet<String> = {
        let cutoff = fresh_cutoff;
        match db
            .call(
                move |conn| -> Result<Vec<String>, tokio_rusqlite::Error<rusqlite::Error>> {
                    let mut stmt = conn.prepare(
                        "SELECT server_hash FROM server_cache
                         WHERE status = 'positive' AND tested_at >= ?1",
                    )?;
                    let rows =
                        stmt.query_map(rusqlite::params![cutoff], |row| row.get::<_, String>(0))?;
                    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
                },
            )
            .await
        {
            Ok(ids) => ids.into_iter().collect(),
            Err(_) => std::collections::HashSet::new(),
        }
    };
    let unique_ids: std::collections::HashSet<String> =
        unique.iter().map(|s| s.id.clone()).collect();
    let cached_server_ids: std::collections::HashSet<String> = all_fresh_positive
        .intersection(&unique_ids)
        .cloned()
        .collect();

    // Split: cache_hit_servers (not tested) vs needs_batch (full test cycle)
    let mut cache_hit_servers: Vec<ServerProfile> = Vec::new();
    let mut needs_batch: Vec<ServerProfile> = Vec::new();
    let mut cache_hit_count = 0usize;
    for s in unique {
        if cached_server_ids.contains(&s.id) {
            // Emit cached event
            let _ = event_tx.send(crate::events::ServerTestedEvent {
                profile_id: profile_id.to_string(),
                phase: "cache".into(),
                server_id: s.id.clone(),
                static_data: None,
                ping_ms: s.ping_ms,
                exit_server: s.exit_server.clone(),
                exit_flag: s.exit_flag.clone(),
                exit_country_name: s.exit_country_name.clone(),
                exit_city_name: s.exit_city_name.clone(),
                behind_cdn: Some(s.behind_cdn),
                error: None,
            });
            cache_hit_count += 1;
            cache_hit_servers.push(s);
        } else {
            needs_batch.push(s);
        }
    }
    tracing::info!(target: "subscription",
        "Phase 1: {cache_hit_count} cache hits (skip test), {} servers need batch test",
        needs_batch.len());

    // Progress: Phase 1 — emit cache hits
    let tested_so_far = cache_hit_count;
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(crate::events::RefreshProgress {
            profile_id: profile_id.to_string(),
            total: total_servers,
            tested: tested_so_far,
            cache_hits: cache_hit_count,
            successful: cache_hit_count,
            failed: 0,
            phase: "cache".into(),
            updated_at: String::new(),
            last_tested: None,
        });
    }

    if cancel.is_cancelled() {
        tracing::info!(target: "subscription", "Refresh cancelled during Phase 1");
        // Emit done progress before returning
        if let Some(ref tx) = progress_tx {
            let _ = tx.send(crate::events::RefreshProgress {
                profile_id: profile_id.to_string(),
                total: total_servers,
                tested: total_servers,
                cache_hits: cache_hit_count,
                successful: cache_hit_count,
                failed: 0,
                phase: "done".into(),
                updated_at: String::new(),
                last_tested: None,
            });
        }
        return Vec::new();
    }

    // ═══ PHASE 2: Preflight (Clash API) ══════════════════════════════
    if needs_batch.is_empty() {
        tracing::info!(target: "subscription", "All servers cached — skipping preflight and batch");
        // Emit done progress early
        if let Some(ref tx) = progress_tx {
            let _ = tx.send(crate::events::RefreshProgress {
                profile_id: profile_id.to_string(),
                total: total_servers,
                tested: total_servers,
                cache_hits: cache_hit_count,
                successful: cache_hit_count,
                failed: 0,
                phase: "done".into(),
                updated_at: String::new(),
                last_tested: None,
            });
        }
        return cache_hit_servers;
    }

    if cancel.is_cancelled() {
        tracing::info!(target: "subscription", "Refresh cancelled before preflight");
        return Vec::new();
    }

    let preflight_pings = tokio::select! {
        _ = cancel.cancelled() => {
            tracing::info!(target: "subscription", "Refresh cancelled during Clash API preflight");
            return Vec::new();
        }
        pings = clash_api_test(
            &needs_batch,
            data_dir,
            settings,
            Some(cache),
            None::<&AppHandle>,
            profile_id,
        ) => pings,
    };

    if cancel.is_cancelled() {
        tracing::info!(target: "subscription", "Refresh cancelled after preflight");
        return Vec::new();
    }

    let ping_count = preflight_pings.len();
    // Exclude negative-cache servers from dead-filter and remove them from
    // needs_batch entirely. clash_api_test already skipped them via
    // is_cached_negative, so they are not in preflight_pings. Without removal
    // they would still go to Phase 3 batch testing, wasting resources and
    // incrementing consecutive_failures on each cycle.
    let mut negative_cache_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for s in &needs_batch {
        if cache.is_cached_negative(&s.id).await {
            negative_cache_ids.insert(s.id.clone());
        }
    }
    let neg_count = negative_cache_ids.len();
    needs_batch.retain(|s| !negative_cache_ids.contains(&s.id));
    let non_cached: Vec<&crate::proxy::ServerProfile> = needs_batch.iter().collect();

    if ping_count > 0 && ping_count < non_cached.len() {
        // Pre-filter: dead servers go to negative cache
        let dead_ids: std::collections::HashSet<String> = non_cached
            .iter()
            .filter(|s| !preflight_pings.contains_key(&s.id))
            .map(|s| s.id.clone())
            .collect();
        let dead_count = dead_ids.len();
        let skipped_neg = neg_count;

        // ═══ PHASE 2 writes to server_cache (negative — dead servers) ═══
        // OWNERSHIP: Phase 2 may write negative entries to `server_cache`.
        // It must NOT write to `servers`, `profiles`, or `test_history`
        // (test_history writes happen later in Phase 1.5 for preflight pings).
        let dead: Vec<(String, String)> = dead_ids
            .iter()
            .map(|id| (id.clone(), "Clash API preflight: no ping".into()))
            .collect();
        crate::proxy::cache::flush_cache_batch(db, Some(cache), &[], &dead).await;

        // Emit error events for the dead servers
        for sid in &dead_ids {
            let _ = event_tx.send(crate::events::ServerTestedEvent {
                profile_id: profile_id.to_string(),
                phase: "error".into(),
                server_id: sid.clone(),
                static_data: None,
                ping_ms: None,
                exit_server: None,
                exit_flag: None,
                exit_country_name: None,
                exit_city_name: None,
                behind_cdn: None,
                error: Some("Clash API preflight failed".into()),
            });
        }

        needs_batch.retain(|s| !dead_ids.contains(&s.id));
        tracing::info!(target: "subscription",
            "Phase 2 pre-filter: {dead_count} dead servers moved to negative cache (skipped {skipped_neg} already negative)");
    }

    if ping_count == 0 && !needs_batch.is_empty() {
        tracing::warn!(target: "subscription",
            "Pre-flight returned 0 pings — keeping all servers (transient)");
    }

    // Emit preflight ping events for servers that got a Clash API response.
    for server in &needs_batch {
        if let Some(ping) = preflight_pings.get(&server.id).copied() {
            let _ = event_tx.send(crate::events::ServerTestedEvent {
                profile_id: profile_id.to_string(),
                phase: "preflight".into(),
                server_id: server.id.clone(),
                static_data: None,
                ping_ms: Some(ping),
                exit_server: None,
                exit_flag: None,
                exit_country_name: None,
                exit_city_name: None,
                behind_cdn: Some(server.behind_cdn),
                error: None,
            });
        }
    }

    // Progress: Phase 2 complete — emit preflight results
    let alive_after_preflight = needs_batch.len();
    let preflight_failed = total_servers.saturating_sub(tested_so_far + alive_after_preflight);
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(crate::events::RefreshProgress {
            profile_id: profile_id.to_string(),
            total: total_servers,
            tested: tested_so_far + ping_count,
            cache_hits: cache_hit_count,
            successful: cache_hit_count + alive_after_preflight,
            failed: preflight_failed,
            phase: "preflight".into(),
            updated_at: String::new(),
            last_tested: None,
        });
    }

    // ═══ PHASE 1.5: DNS resolve + GeoIP + CDN entry detection ════════
    if cancel.is_cancelled() {
        tracing::info!(target: "subscription", "Refresh cancelled after preflight filtering");
        return Vec::new();
    }
    if needs_batch.is_empty() {
        return cache_hit_servers;
    }

    // Lazy init GeoIP
    if !crate::proxy::geoip::is_ready() {
        let geo_dir = data_dir.join("geoip");
        let city_path = geo_dir.join("GeoLite2-City.mmdb");
        if city_path.exists() {
            let _ = crate::proxy::geoip::init_city(&city_path);
        }
        let asn_path = geo_dir.join("GeoLite2-ASN.mmdb");
        if asn_path.exists() && !crate::proxy::geoip::is_asn_ready() {
            let _ = crate::proxy::geoip::init_asn(&asn_path);
        }
        let ipinfo_path = geo_dir.join("ipinfo_lite.mmdb");
        if ipinfo_path.exists() && !crate::proxy::geoip::is_ipinfo_ready() {
            let _ = crate::proxy::geoip::init_ipinfo(&ipinfo_path);
        }
        if !crate::proxy::geoip::is_cdn_ready() {
            let _ = crate::proxy::geoip::init_cdn_ranges(&geo_dir);
        }
    }

    let mut dns_set = tokio::task::JoinSet::new();
    for s in &needs_batch {
        let sid = s.id.clone();
        let host = s.server.clone();
        let port = s.port;
        dns_set.spawn(async move {
            tokio::net::lookup_host(&format!("{}:{}", host, port))
                .await
                .ok()
                .and_then(|mut a| a.next())
                .map(|addr| (sid, addr.ip().to_string()))
        });
    }
    let mut dns_results: HashMap<String, String> = HashMap::new();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                dns_set.abort_all();
                while dns_set.join_next().await.is_some() {}
                tracing::info!(target: "subscription", "Refresh cancelled during DNS enrichment");
                return Vec::new();
            }
            result = dns_set.join_next() => result,
        };
        let Some(res) = next else {
            break;
        };
        if let Ok(Some((sid, ip))) = res {
            dns_results.insert(sid, ip);
        }
    }

    for s in &mut needs_batch {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        if let Some(ip) = dns_results.get(&s.id) {
            // Entry GeoIP
            let geo_info = tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(target: "subscription", "Refresh cancelled during GeoIP enrichment");
                    return Vec::new();
                }
                info = crate::proxy::geoip::resolve_hostname(ip) => info,
            };
            if let Some(info) = geo_info {
                if s.country_name.is_none() {
                    s.country_name = Some(info.country_name.clone());
                }
                if s.flag.is_none() {
                    s.flag = Some(info.country_code.clone());
                }
            }
            // CDN entry detection
            if !s.behind_cdn && crate::proxy::geoip::is_cdn_entry_ip(ip) {
                s.behind_cdn = true;
                s.assert_no_cdn_exit_data("entry_ip_match");
                s.exit_server = None;
                s.exit_flag = None;
                s.exit_country_name = None;
                s.exit_city_name = None;
            }
        }
        // Set ping from preflight
        if let Some(ping) = preflight_pings.get(&s.id).copied() {
            s.ping_ms = Some(ping);
        }
    }

    // Write preflight pings to test_history — NOT to `servers`.
    //
    // ── INVARIANT: `servers` is the FINAL table ──────────────────
    // The `servers` table is protected by CHECK constraint v3
    // (schema v3, db/mod.rs):
    //
    //   CHECK(
    //     (behind_cdn = 1 AND exit_server IS NULL AND exit_flag IS NULL
    //      AND exit_country_name IS NULL AND exit_city_name IS NULL)
    //     OR
    //     (behind_cdn = 0 AND exit_server IS NOT NULL)
    //   )
    //
    // This is the SECOND line of defence for invariant I-CDN-1
    // (CDN servers must never have exit data). The first line is
    // assert_no_cdn_exit_data() in code. The CHECK constraint
    // guarantees that even if a code bug writes exit data for a
    // CDN server, the INSERT will fail and the transaction will
    // roll back — CDN servers with exit data can NEVER persist
    // in `servers`.
    //
    // CDN servers skip batch exit-IP testing entirely (optimisation:
    // cdn_skip_count in batch_test, fetch_outlet_ip_through_port
    // skipped in test_single_server). Their exit IP is dynamic
    // (anycast / Workers / WARP) and testing it is pointless.
    //
    // Preflight is an INTERMEDIATE phase. Its results belong in
    // test_history (per-server test log), never in `servers`.
    // Writing preflight pings to `servers` would:
    //   1. Bypass the "all phases complete" gate — servers would
    //      appear in `servers` before batch test determines their
    //      exit IP (or confirms CDN status).
    //   2. Risk CHECK v3 violation on crash between Phase 2 and
    //      Phase 3 — a CDN server with a stale exit_server from a
    //      previous cycle could be UPDATEd with a new ping but
    //      retain stale exit data.
    // ─────────────────────────────────────────────────────────────
    if !preflight_pings.is_empty() {
        let pings = preflight_pings.clone();
        let now = chrono::Utc::now().timestamp();
        // ═══ PHASE 1.5 writes only to test_history ══════════════════
        // OWNERSHIP: preflight pings belong in test_history (per-server log).
        // Phase 1.5 must NOT write to servers, server_cache, or profiles.
        // Real ping_ms + exit data are written in Phase 3 POST after batch.
        let _ = db
            .call(
                move |conn| -> Result<(), tokio_rusqlite::Error<rusqlite::Error>> {
                    let tx = conn.transaction()?;
                    for (sid, ping) in &pings {
                        tx.execute(
                            "INSERT INTO test_history (server_id, tested_at, ping_ms, success, phase)
                                     VALUES (?1, ?2, ?3, 1, 'preflight')",
                            rusqlite::params![sid, now, *ping as i64],
                        )?;
                    }
                    tx.commit()?;
                    Ok(())
                },
            )
            .await;
    }

    // ═══ PHASE 1.5 POST: Geolocate + restore CDN status for cache_hit_servers ═══
    // cache_hit_servers from Phase 1 bypass Phase 1.5 DNS resolution, GeoIP
    // enrichment, and CDN entry-IP CIDR checking. This means:
    //   1. `flag`, `country_name` are empty → would fail NOT NULL constraint
    //   2. `behind_cdn` is from Phase 0 (keyword workers.dev/pages.dev ONLY) →
    //      CIDR-detected CDN servers would have behind_cdn=false + stale exit data
    //      → I-CDN-1 invariant violation
    //
    // Fix: read behind_cdn AND exit-IP status from the positive cache entry that
    // was set during the previous batch test. The cache entry has the definitive
    // CDN classification from the last full test cycle.
    // Fall back to GeoIP enrichment for flag/country_name if the cache is stale.
    if !cache_hit_servers.is_empty() {
        // Step 1: Restore behind_cdn + CDN exit clearing from cache
        for s in &mut cache_hit_servers {
            let mut pos = cache.get_positive_if_fresh(&s.id).await;
            if pos.is_none() {
                pos = cache.get_positive(&s.id).await;
            }
            if let Some(pos) = pos {
                // Restore the CDN classification from the cache (source of truth)
                if pos.behind_cdn {
                    s.behind_cdn = true;
                    s.assert_no_cdn_exit_data("cache_hit_cache_entry");
                    s.exit_server = None;
                    s.exit_flag = None;
                    s.exit_country_name = None;
                    s.exit_city_name = None;
                }
                // If exit flag is missing and cache has it, restore
                if s.exit_flag.is_none() {
                    s.exit_flag = pos.exit_flag.clone();
                }
                // Restore ping_ms from cache — without this, servers that had
                // NULL ping_ms in the DB (from previous cycle constraint failure)
                // stay None and fail NOT NULL on INSERT, creating a death spiral.
                s.ping_ms = Some(pos.ping_ms);
            }
        }

        // Step 2: GeoIP enrichment for flag/country_name (for UI display)
        if crate::proxy::geoip::is_ready() {
            for s in &mut cache_hit_servers {
                if cancel.is_cancelled() {
                    tracing::info!(target: "subscription", "Refresh cancelled during cached GeoIP enrichment");
                    return Vec::new();
                }
                if s.flag.is_none() || s.country_name.is_none() {
                    let geo_info = tokio::select! {
                        _ = cancel.cancelled() => {
                            tracing::info!(target: "subscription", "Refresh cancelled during cached GeoIP enrichment");
                            return Vec::new();
                        }
                        info = crate::proxy::geoip::resolve_hostname(&s.server) => info,
                    };
                    if let Some(info) = geo_info {
                        if s.flag.is_none() {
                            s.flag = Some(info.country_code.clone());
                        }
                        if s.country_name.is_none() {
                            s.country_name = Some(info.country_name.clone());
                        }
                        s.city_name = if info.city.is_empty() {
                            None
                        } else {
                            Some(info.city.clone())
                        };
                    }
                }
            }
        } else {
            tracing::warn!(target: "subscription",
                "GeoIP not ready — cache_hit_servers enriched without GeoIP (flag may be None)"
            );
        }
    }

    // ═══ PHASE 3: Deep batch test ════════════════════════════════════
    if needs_batch.is_empty() {
        return cache_hit_servers;
    }

    if cancel.is_cancelled() {
        tracing::info!(target: "subscription", "Refresh cancelled before deep batch test");
        return Vec::new();
    }

    crate::proxy::cache::batch_test_incremental(
        cache,
        &needs_batch,
        Some(cancel.clone()),
        profile_id,
        &event_tx,
        db,
    )
    .await;

    if cancel.is_cancelled() {
        tracing::info!(target: "subscription", "Refresh cancelled during batch; previous server snapshot retained");
        return Vec::new();
    }

    tracing::info!(target: "subscription",
        "Phase 3 (batch): {} servers sent to BatchingEngine", needs_batch.len());

    // Read exit data from cache (batch test wrote it) and update servers.
    // If batch test failed (cache miss), fall back to any existing cache entry
    // (even stale) so servers aren't silently dropped from profile.
    for s in &mut needs_batch {
        let mut pos = cache.get_positive_if_fresh(&s.id).await;
        if pos.is_none() {
            // Fallback: stale positive entry — better than no data (batch worker may have crashed).
            pos = cache.get_positive(&s.id).await;
        }
        if let Some(pos) = pos {
            s.behind_cdn = pos.behind_cdn;
            s.ping_ms = Some(pos.ping_ms);
            if !s.behind_cdn {
                s.exit_server = pos.exit_ip;
                s.exit_flag = pos.exit_flag;
                s.exit_country_name = pos.exit_country;
                s.exit_city_name = pos.exit_city;
            }
        }
    }

    // ═══ FILTER: non-CDN servers without exit data ════════════════════
    // Alive (passed ping) but outlet IP fetch failed — route issue.
    // Batcher already called update_negative. Remove from needs_batch
    // so they don't reach Phase 3 POST INSERT (avoids CHECK v3 violation).
    //
    // ═══ FILTER writes to server_cache (negative) ═════════════════════
    // OWNERSHIP: this filter may write negative entries to server_cache
    // via flush_cache_batch. It must NOT write to servers, profiles,
    // or test_history — those are owned by later phases.
    let reason = "No exit IP after batch test (transient route failure)".to_string();
    let mut no_exit: Vec<(String, String)> = Vec::new();
    needs_batch.retain(|s| {
        if !s.behind_cdn && s.exit_server.is_none() && s.ping_ms.is_some() {
            no_exit.push((s.id.clone(), reason.clone()));
            false
        } else {
            true
        }
    });
    if !no_exit.is_empty() {
        crate::proxy::cache::flush_cache_batch(db, Some(cache), &[], &no_exit).await;
        tracing::info!(
            target: "subscription",
            "Filtered {} non-CDN server(s) with no exit data → negative cache (previously_worked=1, TTL=1min)",
            no_exit.len(),
        );
    }

    // ═══ FILTER: servers that completely failed batch test (no ping_ms) ═
    // These servers got negative entries in server_cache via batch_test_incremental.
    // They have no ping_ms → cannot satisfy NOT NULL constraint. Remove from
    // needs_batch so Phase 3 POST doesn't attempt INSERT.
    // The negative cache entry will be re-tested when its TTL expires.
    let no_ping_before = needs_batch.len();
    needs_batch.retain(|s| s.ping_ms.is_some());
    let no_ping_count = no_ping_before - needs_batch.len();
    if no_ping_count > 0 {
        tracing::info!(
            target: "subscription",
            "Phase 3: {} server(s) with no ping_ms removed (failed batch test, negative cache only)",
            no_ping_count,
        );
    }

    // PHASE 3 POST-PROCESS: atomically replace the persisted server snapshot.
    // The previous snapshot is untouched until a completed result is ready.
    let survivors_after_filters = needs_batch.len();
    let mut return_servers = [&cache_hit_servers[..], &needs_batch[..]].concat();
    let mut all_to_persist = cache_hit_servers;
    all_to_persist.append(&mut needs_batch);

    if cancel.is_cancelled() {
        tracing::info!(target: "subscription", "Refresh cancelled before server snapshot commit");
        return Vec::new();
    }

    if !all_to_persist.is_empty() {
        let pid = profile_id.to_string();
        let persisted = db
            .call(move |conn| persist_profile_servers(conn, &pid, &all_to_persist))
            .await;

        let persisted_ids = match persisted {
            Ok(ids) if !ids.is_empty() => ids,
            Ok(_) => {
                tracing::error!(
                    target: "subscription",
                    "Phase 3 POST: no server passed persistence constraints; previous snapshot was retained"
                );
                return Vec::new();
            }
            Err(error) => {
                tracing::error!(
                    target: "subscription",
                    "Phase 3 POST: snapshot transaction failed; previous snapshot was retained: {error}"
                );
                return Vec::new();
            }
        };

        let persisted_ids: std::collections::HashSet<String> = persisted_ids.into_iter().collect();
        return_servers.retain(|server| persisted_ids.contains(&server.id));
    }

    // Progress: Phase 3 complete — emit batch results
    let batch_successful = cache_hit_count + needs_batch.len();
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(crate::events::RefreshProgress {
            profile_id: profile_id.to_string(),
            total: total_servers,
            tested: total_servers,
            cache_hits: cache_hit_count,
            successful: tested_so_far + batch_successful,
            failed: total_servers.saturating_sub(tested_so_far + batch_successful),
            phase: "batch".into(),
            updated_at: String::new(),
            last_tested: None,
        });
    }

    // Progress: emit done
    let final_successful = survivors_after_filters + cache_hit_count;
    if let Some(ref tx) = progress_tx {
        let _ = tx.send(crate::events::RefreshProgress {
            profile_id: profile_id.to_string(),
            total: total_servers,
            tested: total_servers,
            cache_hits: cache_hit_count,
            successful: final_successful,
            failed: total_servers.saturating_sub(final_successful),
            phase: "done".into(),
            updated_at: String::new(),
            last_tested: None,
        });
    }

    // Return the servers — caller (lib.rs) does the final commit
    return_servers
}

/// Create an in-memory SQLite database for tests.
#[cfg(test)]
async fn test_db() -> crate::db::DbPool {
    let conn = tokio_rusqlite::Connection::open(":memory:").await.unwrap();
    conn.call(|inner| -> Result<(), rusqlite::Error> {
        inner.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        inner.execute_batch(crate::db::migrations::CREATE_TABLES_SQL)?;
        Ok(())
    })
    .await
    .unwrap();
    std::sync::Arc::new(conn)
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

    #[tokio::test]
    async fn test_new_creates_empty_manager() {
        let mgr = ProfileManager::new(PathBuf::from("/tmp/test"), test_db().await);
        assert!(mgr.list().is_empty());
    }

    #[tokio::test]
    async fn test_load_nonexistent_file_returns_empty() {
        let dir = test_dir("nonexistent");
        let _ = std::fs::remove_dir_all(&dir);

        let mut mgr = ProfileManager::new(dir.clone(), test_db().await);
        mgr.load().unwrap();
        assert!(mgr.list().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_save_and_load_roundtrip() {
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
            let mut mgr = ProfileManager::new(dir.clone(), test_db().await);
            mgr.profiles.push(profile);
            mgr.save().unwrap();
        }

        {
            let mut mgr = ProfileManager::new(dir.clone(), test_db().await);
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
// Manual-only suite; requires a disposable OMNITUN_TEST_DATA_DIR fixture.
#[cfg(all(test, feature = "user-data-smoke"))]
mod extra_tests {
    use super::*;
    use crate::proxy::config::build_outbound;
    use crate::proxy::config::generate_config;
    use crate::settings::AppSettings;
    use serde_json::json;
    use std::collections::HashMap;
    use std::process::Command;

    fn test_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("omnitun-{name}-"))
            .tempdir()
            .expect("Could not create isolated profile smoke-test directory")
    }

    fn app_data_dir() -> PathBuf {
        PathBuf::from(std::env::var_os("OMNITUN_TEST_DATA_DIR")
            .expect("Set OMNITUN_TEST_DATA_DIR to a disposable fixture directory"))
    }

    fn singbox() -> PathBuf {
        let binary_name = crate::proxy::installer::Installer::binary_name();
        let bundled = app_data_dir().join("binaries").join(&binary_name);
        if bundled.is_file() { return bundled; }
        if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join(&binary_name))
                .find(|candidate| candidate.is_file())
        }) { return path; }
        panic!("Required sing-box binary is missing from fixture binaries/ and PATH");
    }

    fn load_real_servers() -> Vec<ServerProfile> {
        let path = app_data_dir().join("profiles.json");
        let json = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("Required disposable fixture is missing at {}: {error}", path.display())
        });
        let data: serde_json::Value = serde_json::from_str(&json)
            .expect("profiles.json fixture must contain valid JSON");
        let servers = data.get(0).and_then(|p| p.get("servers"))
            .and_then(serde_json::Value::as_array)
            .expect("profiles.json must contain a first profile with a servers array");
        let parsed: Vec<ServerProfile> = servers.iter()
            .map(|server| serde_json::from_value(server.clone())
                .expect("Fixture server must deserialize as ServerProfile"))
            .collect();
        assert!(!parsed.is_empty(), "profiles.json fixture must contain at least one server");
        parsed
    }

    // -----------------------------------------------------------------------
    // 1. Config generation — each server as single-outbound config
    // -----------------------------------------------------------------------
    #[test]
    fn test_each_server_generates_valid_config() {
        let servers = load_real_servers();
        let binary = singbox();
        let cache = test_dir("singles");
        let settings = AppSettings::default();

        let mut ok = 0u32;
        let mut fail = 0u32;
        for (i, s) in servers.iter().enumerate() {
            match generate_config(
                cache.path(),
                2080,
                "local-proxy",
                std::slice::from_ref(s),
                &settings,
            ) {
                Ok((cfg_str, _)) => {
                    let tmp = cache.path().join(format!("c-{i}.json"));
                    std::fs::write(&tmp, &cfg_str).expect("Could not write generated sing-box config");
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
        let mut outbounds: Vec<serde_json::Value> = vec![
            json!({"type": "direct", "tag": "direct"}),
            json!({"type": "block", "tag": "block"}),
        ];
        let mut proxy_tags = vec!["direct".to_string()];
        for s in &servers {
            let mut outbound = build_outbound(s)
                .expect("Every fixture server must produce a supported sing-box outbound");
            let tag = format!("proxy-{}", s.id);
            outbound["tag"] = json!(&tag);
            proxy_tags.push(tag);
            outbounds.push(outbound);
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
        let p = tmp.path().join("batch.json");
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
        let settings = AppSettings::default();

        let (cfg_str, _) = generate_config(
            cache.path(),
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
        
        // autoConnectFastest=false → should produce single outbound per server
        let settings_no_fastest = AppSettings {
            auto_connect_fastest: false,
            ..AppSettings::default()
        };

        let (cfg_str, _) = generate_config(
            cache.path(),
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
        let settings_fastest = AppSettings {
            auto_connect_fastest: true,
            ..AppSettings::default()
        };

        let (cfg_str, _) =
            generate_config(cache.path(), 2080, "local-proxy", &servers, &settings_fastest).unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&cfg_str).unwrap();
        let obs = cfg["outbounds"].as_array().unwrap();

        // Find urltest group (replaced selector)
        let has_urltest = obs.iter().any(|o| o["type"] == "urltest");
        assert!(
            has_urltest,
            "With autoConnectFastest=true, expected urltest outbound group"
        );

                println!("  alternativeServers: off when fastest=false, on when fastest=true ✓");
    }

    // -----------------------------------------------------------------------
    // 7. Kill process - verify doesn't panic
    // -----------------------------------------------------------------------
    #[test]
    fn test_refresh_preserves_server_ids() -> Result<(), Box<dyn std::error::Error>> {
        let servers = load_real_servers();
        assert!(servers.len() >= 2, "The profile smoke fixture must contain at least two servers");

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

    #[tokio::test]
    async fn test_remove_nonexistent_returns_error() {
        let dir = test_dir("remove_err");

        let mut mgr = ProfileManager::new(dir.path().to_path_buf(), test_db().await);
        let result = mgr.remove("nonexistent");
        assert!(result.is_err());


    }
}
