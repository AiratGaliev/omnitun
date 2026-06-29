//! Dual-layer server cache with Stale-While-Revalidate (SWR).
//!
//! Architecture:
//! ```
//! ┌──────────────────────────────┐
//! │     store: HashMap           │  ← Source of truth. Serializable, iterable.
//! │     (RwLock<HashMap>)        │     Written to disk by `persistence.rs`.
//! ├──────────────────────────────┤
//! │     hot: moka::future::Cache │  ← Read-through hot cache with request
//! │                              │     coalescing (try_get_with).
//! └──────────────────────────────┘
//! ```
//!
//! - `store` is the **only** thing serialized to disk.
//! - `hot` is a fast in-memory cache for concurrent reads.
//! - SWR: if a positive entry is stale (>10 min), we return it AND spawn a
//!   background refresh.
//! - Negative cache entries have exponential backoff TTL.

use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::vpn::persistence;
use crate::vpn::port_pool::PortPool;
use crate::vpn::ServerProfile;
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

// ── Constants ───────────────────────────────────────────────────────────────

/// Positive cache: entry is considered "fresh" for 10 minutes.
/// After 10 min: SWR — return stale value + background refresh.
const POSITIVE_STALE_SECS: i64 = 600; // 10 min

/// Positive cache: hard TTL — entry is evicted after 30 min.
const POSITIVE_TTL_SECS: u64 = 1800; // 30 min

/// Positive cache: entry is idle-evicted after 10 min of not being accessed.
const POSITIVE_IDLE_SECS: u64 = 600; // 10 min

/// Negative cache base TTL for servers that previously worked.
const NEGATIVE_BASE_WORKED_SECS: u64 = 60; // 1 min

/// Negative cache base TTL for servers that never worked.
const NEGATIVE_BASE_NEVER_WORKED_SECS: u64 = 300; // 5 min

/// Maximum negative cache TTL.
const NEGATIVE_MAX_TTL_SECS: u64 = 7200; // 2 hours

// ── Data Structures ─────────────────────────────────────────────────────────

/// Status of a cached server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CacheStatus {
    Positive {
        ping_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_ip: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_flag: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_country: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_city: Option<String>,
        #[serde(default)]
        behind_cdn: bool,
        #[serde(default)]
        consecutive_successes: u32,
    },
    Negative {
        first_failure_at: i64,
        last_failure_at: i64,
        consecutive_failures: u32,
        #[serde(default)]
        previously_worked: bool,
        reason: String,
    },
}

/// A single cache entry stored both in-memory and on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub server_id: String,
    pub last_updated: i64,
    pub status: CacheStatus,
}

impl CacheEntry {
    /// Extract positive data, if the entry is positive.
    pub fn to_positive(&self) -> Option<PositiveResult> {
        match &self.status {
            CacheStatus::Positive {
                ping_ms,
                exit_ip,
                exit_flag,
                exit_country,
                exit_city,
                behind_cdn,
                ..
            } => Some(PositiveResult {
                server_id: self.server_id.clone(),
                ping_ms: *ping_ms,
                exit_ip: exit_ip.clone(),
                exit_flag: exit_flag.clone(),
                exit_country: exit_country.clone(),
                exit_city: exit_city.clone(),
                behind_cdn: *behind_cdn,
                tested_at: self.last_updated,
            }),
            _ => None,
        }
    }
}

/// Positive test result returned from the cache or from a fresh test.
#[derive(Debug, Clone)]
pub struct PositiveResult {
    pub server_id: String,
    #[allow(dead_code)]
    pub ping_ms: u64,
    pub exit_ip: Option<String>,
    pub exit_flag: Option<String>,
    pub exit_country: Option<String>,
    pub exit_city: Option<String>,
    pub behind_cdn: bool,
    pub tested_at: i64,
}

/// Hot cache entry — lightweight, only for positive results.
#[derive(Debug, Clone)]
struct HotEntry {
    #[allow(dead_code)]
    pub ping_ms: u64,
    tested_at: i64,
}

/// Calculate the TTL for a negative cache entry using exponential backoff.
///
/// ⚠️ FIXED from original TZ: `consecutive_failures` is clamped to `[1, 6]`
/// before subtraction, so this function **never underflows**.
pub fn calculate_negative_ttl(consecutive_failures: u32, previously_worked: bool) -> Duration {
    let base_ttl = if previously_worked {
        NEGATIVE_BASE_WORKED_SECS
    } else {
        NEGATIVE_BASE_NEVER_WORKED_SECS
    };
    // Clamp to [1, 6] then subtract 1 for safe shift: 1→0, 6→5 → 2^0..2^5 → 1..32
    let clamped = consecutive_failures.max(1).min(6) - 1;
    let multiplier = 2_u64.pow(clamped); // 1, 2, 4, 8, 16, 32
    let ttl_secs = (base_ttl * multiplier).min(NEGATIVE_MAX_TTL_SECS);
    Duration::from_secs(ttl_secs)
}

/// Check whether a negative entry has expired (can be retested).
pub fn is_negative_expired(entry: &CacheEntry, now: i64) -> bool {
    match &entry.status {
        CacheStatus::Negative {
            consecutive_failures,
            last_failure_at,
            previously_worked,
            ..
        } => {
            let ttl = calculate_negative_ttl(*consecutive_failures, *previously_worked);
            let age = now - last_failure_at;
            age >= ttl.as_secs() as i64
        }
        _ => true, // non-negative entries are always "expired"
    }
}

/// Check whether a positive entry is stale (needs background refresh).
pub fn is_positive_stale(tested_at: i64, now: i64) -> bool {
    now - tested_at > POSITIVE_STALE_SECS
}

// ── ServerCache ─────────────────────────────────────────────────────────────

/// Dual-layer server cache with SWR, persistence, and request coalescing.
#[derive(Clone)]
pub struct ServerCache {
    /// Source-of-truth store. Serializable, iterable.
    store: Arc<RwLock<HashMap<String, CacheEntry>>>,
    /// Hot read-through cache with request coalescing.
    hot: Cache<String, Arc<HotEntry>>,
    /// Channel for batch testing requests.
    batch_tx: tokio::sync::mpsc::UnboundedSender<BatchRequest>,
    /// Receiver — stored until `start_batcher()` is called.
    /// This allows `get_or_test()` to queue requests even before the batcher starts.
    pending_rx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<BatchRequest>>>>,
    /// Path to data directory for persistence.
    data_dir: Arc<Path>,
    /// Whether the batcher has been started.
    batcher_started: Arc<std::sync::atomic::AtomicBool>,
}

/// A request sent to the batch worker.
pub struct BatchRequest {
    pub server: ServerProfile,
    pub profile_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<PositiveResult, String>>,
}

impl ServerCache {
    /// Create a new cache, loading persisted data from disk.
    ///
    /// Does NOT start the batch worker — call `start_batcher()` after app setup.
    /// This allows the cache to be created early and queue requests before the
    /// batcher is fully initialized.
    pub fn new(data_dir: &Path) -> Self {
        // Load persisted entries from disk
        let persisted = persistence::load_cache(data_dir);
        let persisted_count = persisted.len();

        let store = Arc::new(RwLock::new(persisted));
        let hot: Cache<String, Arc<HotEntry>> = Cache::builder()
            .time_to_live(Duration::from_secs(POSITIVE_TTL_SECS))
            .time_to_idle(Duration::from_secs(POSITIVE_IDLE_SECS))
            .build();

        let (batch_tx, batch_rx) = tokio::sync::mpsc::unbounded_channel();

        log::info!(
            "[ServerCache] initialized with {persisted_count} persisted entries"
        );
        log::info!(
            "[ServerCache] hot cache TTL={POSITIVE_TTL_SECS}s, idle={POSITIVE_IDLE_SECS}s, SWR stale={POSITIVE_STALE_SECS}s"
        );
        log::info!(
            "[ServerCache] batch channel created (not started — call start_batcher)"
        );

        Self {
            store,
            hot,
            batch_tx,
            pending_rx: Arc::new(Mutex::new(Some(batch_rx))),
            data_dir: Arc::from(data_dir.to_path_buf()),
            batcher_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Start the background batch worker (BatchingEngine).
    ///
    /// Should be called once during app setup, after the cache is created.
    /// Takes ownership of the batch receiver channel.
    pub fn start_batcher(
        &self,
        port_pool: Arc<PortPool>,
        app_handle: AppHandle,
        cancel: CancellationToken,
    ) {
        if self
            .batcher_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            log::warn!("[ServerCache] start_batcher called twice — ignoring");
            return;
        }

        let cache = self.clone();
        tauri::async_runtime::spawn(async move {
            let rx = {
                let mut guard = cache.pending_rx.lock().await;
                guard
                    .take()
                    .expect("start_batcher: batch_rx already consumed")
            };
            crate::vpn::batcher::run(rx, cache, port_pool, app_handle, cancel).await;
        });

        log::info!("[ServerCache] batch worker started");
    }

    /// Get a server's cached positive result, or initiate a test.
    ///
    /// Returns:
    /// - `Ok(PositiveResult)` from cache (fresh or stale SWR)
    /// - `Err("Cached negative: ...")` if the server is in the negative cache
    ///   and hasn't expired yet
    /// - `Err("...")` if the test failed
    pub async fn get_or_test(&self, server: &ServerProfile) -> Result<PositiveResult, String> {
        let server_id = &server.id;
        let now = chrono::Utc::now().timestamp();

        // 1. Hot cache check (fastest path)
        if let Some(hot) = self.hot.get(server_id).await {
            if !is_positive_stale(hot.tested_at, now) {
                // Fresh in hot cache — return from store
                let store = self.store.read().await;
                if let Some(entry) = store.get(server_id) {
                    if let Some(result) = entry.to_positive() {
                        return Ok(result);
                    }
                }
            }
            // Stale — fall through to store check + SWR
        }

        // 2. Store check (disk-backed HashMap)
        {
            let store = self.store.read().await;
            if let Some(entry) = store.get(server_id) {
                match &entry.status {
                    CacheStatus::Positive { .. } => {
                        if is_positive_stale(entry.last_updated, now) {
                            // SWR: return stale value, spawn background refresh
                            if let Some(result) = entry.to_positive() {
                                let cache = self.clone();
                                let srv = server.clone();
                                tokio::spawn(async move {
                                    let rx = cache.queue_test(&srv, "");
                                    let _ = rx.await;
                                });
                                return Ok(result);
                            }
                        } else {
                            // Fresh — promote to hot cache
                            if let Some(result) = entry.to_positive() {
                                let tested_at = entry.last_updated;
                                let ping_ms = result.ping_ms;
                                let sid = server_id.to_string();
                                drop(store);
                                self.hot
                                    .insert(
                                        sid,
                                        Arc::new(HotEntry {
                                            ping_ms,
                                            tested_at,
                                        }),
                                    )
                                    .await;
                                return Ok(result);
                            }
                        }
                    }
                    CacheStatus::Negative { .. } => {
                        if !is_negative_expired(entry, now) {
                            // Still within backoff period
                            if let CacheStatus::Negative { reason, .. } = &entry.status {
                                return Err(format!("Cached negative: {reason}"));
                            }
                        }
                        // Expired — fall through to fresh test
                    }
                }
            }
        }

        // 3. Queue a fresh test via the batch worker and wait
        let rx = self.queue_test(server, "");
        rx.await
            .map_err(|_| "Batch worker dropped the request".to_string())?
    }

    /// Update the store and hot cache with a positive result.
    pub(crate) async fn update_positive(&self, server_id: &str, result: PositiveResult) {
        let entry = CacheEntry {
            server_id: server_id.to_string(),
            last_updated: chrono::Utc::now().timestamp(),
            status: CacheStatus::Positive {
                ping_ms: result.ping_ms,
                exit_ip: result.exit_ip.clone(),
                exit_flag: result.exit_flag.clone(),
                exit_country: result.exit_country.clone(),
                exit_city: result.exit_city.clone(),
                behind_cdn: result.behind_cdn,
                consecutive_successes: 1,
            },
        };

        // Update store
        {
            let mut store = self.store.write().await;
            store.insert(server_id.to_string(), entry);
        }

        // Update hot cache
        self.hot
            .insert(
                server_id.to_string(),
                Arc::new(HotEntry {
                    ping_ms: result.ping_ms,
                    tested_at: chrono::Utc::now().timestamp(),
                }),
            )
            .await;
    }

    /// Queue a server test in the batch channel and return a oneshot receiver.
    /// Does NOT block — caller must await the receiver separately.
    fn queue_test(&self, server: &ServerProfile, profile_id: &str) -> tokio::sync::oneshot::Receiver<Result<PositiveResult, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.batch_tx.send(BatchRequest {
            server: server.clone(),
            profile_id: profile_id.to_string(),
            reply: tx,
        });
        rx
    }

    /// Update the store with a negative result.
    pub(crate) async fn update_negative(&self, server_id: &str, reason: String) {
        let now = chrono::Utc::now().timestamp();

        let mut store = self.store.write().await;
        let previous = store.get(server_id).cloned();

        let (consecutive_failures, previously_worked) = match previous {
            Some(CacheEntry {
                status: CacheStatus::Negative { consecutive_failures, .. },
                ..
            }) => (consecutive_failures + 1, false),
            Some(CacheEntry {
                status: CacheStatus::Positive { .. },
                ..
            }) => (1, true),
            _ => (1, false),
        };

        let entry = CacheEntry {
            server_id: server_id.to_string(),
            last_updated: now,
            status: CacheStatus::Negative {
                first_failure_at: previous
                    .map(|e| match &e.status {
                        CacheStatus::Negative {
                            first_failure_at, ..
                        } => *first_failure_at,
                        _ => now,
                    })
                    .unwrap_or(now),
                last_failure_at: now,
                consecutive_failures,
                previously_worked,
                reason,
            },
        };
        store.insert(server_id.to_string(), entry);
    }

    /// Batch-test all servers that passed TCP ping. Fills the cache
    /// and returns results.
    ///
    /// Two-phase algorithm to avoid sequential batching:
    ///   Phase 1: check cache for all servers, queue uncached ones
    ///   Phase 2: await all queued results in parallel
    pub async fn batch_test(
        &self,
        app: &AppHandle,
        servers: &[crate::vpn::ServerProfile],
        _cancel: Option<tokio_util::sync::CancellationToken>,
        profile_id: &str,
    ) -> Vec<crate::vpn::ServerProfile> {
        let logger = app.state::<std::sync::Mutex<Logger>>();
        let total = servers.len();
        let mut cache_hits = 0usize;
        let mut cdn_skip_count = 0usize;
        let mut queued = 0usize;

        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Cache,
            format!("[Cache] batch_test: {total} server(s)"),
        );

        // Phase 1: Check cache for all servers. Queue uncached ones.
        // Use a simple cache-only check to avoid blocking on the batch worker.
        let mut pending = Vec::new();
        let mut result_slots: Vec<Option<ServerProfile>> = Vec::with_capacity(total);

        for server in servers.iter() {
            let server_id = &server.id;
            let now = chrono::Utc::now().timestamp();

            // 1a. Hot cache
            let mut cached: Option<PositiveResult> = None;
            if let Some(hot) = self.hot.get(server_id).await {
                if !is_positive_stale(hot.tested_at, now) {
                    let store = self.store.read().await;
                    if let Some(entry) = store.get(server_id) {
                        cached = entry.to_positive();
                    }
                }
            }

            // 1b. Store (disk-backed HashMap)
            if cached.is_none() {
                let store = self.store.read().await;
                if let Some(entry) = store.get(server_id) {
                    match &entry.status {
                        CacheStatus::Positive { .. } => {
                            if is_positive_stale(entry.last_updated, now) {
                                // Stale — don't return from cache in batch_test, force re-test
                            } else {
                                // Fresh — promote to hot
                                cached = entry.to_positive();
                                if let Some(ref p) = cached {
                                    let hot_entry = Arc::new(HotEntry {
                                        ping_ms: p.ping_ms,
                                        tested_at: entry.last_updated,
                                    });
                                    drop(store);
                                    self.hot.insert(server_id.to_string(), hot_entry).await;
                                }
                            }
                        }
                        CacheStatus::Negative { .. } => {
                            if !is_negative_expired(entry, now) {
                                // Still negative — skip
                            }
                        }
                    }
                }
            }

            if let Some(positive) = cached {
                cache_hits += 1;
                let mut s = server.clone();
                s.ping_ms = Some(positive.ping_ms);
                // Preserve live behind_cdn flag — entry IP check (step 1c in process_servers)
                // is authoritative. Cache may have stale false from before CDN detection.
                if !s.behind_cdn {
                    s.behind_cdn = positive.behind_cdn;
                }
                // CDN servers must not retain outlet IP data — clear it
                if s.behind_cdn {
                    s.exit_server = None;
                    s.exit_flag = None;
                    s.exit_country_name = None;
                    s.exit_city_name = None;
                } else {
                    s.exit_server = positive.exit_ip;
                    s.exit_flag = positive.exit_flag;
                    s.exit_country_name = positive.exit_country;
                    s.exit_city_name = positive.exit_city;
                }
                result_slots.push(Some(s));
            } else if server.behind_cdn {
                // CDN servers can't be batch-tested reliably (Fastly rejects TLS on port 80).
                // Keep TCP ping result as-is.
                cdn_skip_count += 1;
                log(
                    app,
                    &logger,
                    LogLevel::Info,
                    LogSource::Cache,
                    format!("[Cache] server {} behind CDN — skipping batch test, using TCP ping", server.id),
                );
                result_slots.push(Some(server.clone()));
            } else {
                // Queue for batch test (don't await yet!)
                let rx = self.queue_test(server, profile_id);
                pending.push((server.clone(), rx));
                result_slots.push(None);
                queued += 1;
            }
        }

        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Cache,
            format!("[Cache] batch_test: {total} total, {cache_hits} cache hits, {cdn_skip_count} CDN skip, {queued} queued"),
        );

        if queued == 0 {
            return result_slots.into_iter().map(|s| s.unwrap()).collect();
        }

        // Phase 2: Await all queued results using JoinSet with progress reporting
        let mut set = tokio::task::JoinSet::new();
        for (server, rx) in pending {
            set.spawn(async move {
                let res = rx.await;
                (server, res)
            });
        }

        let mut completed = 0usize;
        let mut failed = 0usize;
        let mut successful = 0usize;

        while let Some(res) = set.join_next().await {
            if let Some(ref token) = _cancel {
                if token.is_cancelled() {
                    log(
                        app,
                        &logger,
                        LogLevel::Info,
                        LogSource::Cache,
                        "[Cache] batch_test cancelled".into(),
                    );
                    break;
                }
            }

            if let Ok((server, rx_res)) = res {
                completed += 1;
                let mut s = server.clone();
                match rx_res {
                    Ok(Ok(positive)) => {
                        successful += 1;
                        s.ping_ms = Some(positive.ping_ms);
                        s.behind_cdn = positive.behind_cdn;
                        s.exit_server = positive.exit_ip;
                        s.exit_flag = positive.exit_flag;
                        s.exit_country_name = positive.exit_country;
                        s.exit_city_name = positive.exit_city;
                    }
                    Ok(Err(e)) => {
                        failed += 1;
                        log(
                            app,
                            &logger,
                            LogLevel::Warning,
                            LogSource::Cache,
                            format!("[Cache] server {} batch test failed: {e}", server.id),
                        );
                    }
                    Err(_) => {
                        failed += 1;
                        log(
                            app,
                            &logger,
                            LogLevel::Warning,
                            LogSource::Cache,
                            format!("[Cache] server {} batch worker dropped", server.id),
                        );
                    }
                }

                // Find and fill the right slot
                for slot in &mut result_slots {
                    if slot.is_none() {
                        *slot = Some(s);
                        break;
                    }
                }

                crate::events::emit_refresh_progress(
                    app,
                    crate::events::RefreshProgress {
                        profile_id: profile_id.to_string(),
                        total,
                        tested: cache_hits + completed,
                        cache_hits,
                        successful: cache_hits + successful,
                        failed,
                        phase: "testing".into(),
                    },
                );
            }
        }

        log(
            app,
            &logger,
            LogLevel::Info,
            LogSource::Cache,
            format!("[Cache] batch_test done: {completed}/{queued} completed, {failed} failed"),
        );

        result_slots.into_iter().map(|s| s.unwrap()).collect()
    }

    /// Remove entries that no longer exist in a subscription.
    pub async fn handle_subscription_refresh(&self, new_ids: &std::collections::HashSet<String>) {
        let mut store = self.store.write().await;

        // Remove servers that are no longer in the subscription
        store.retain(|key, _| new_ids.contains(key));

        // Resurrection: clear negative cache for surviving servers
        // so they can be tested immediately
        let mut changed = false;
        for key in new_ids {
            if let Some(entry) = store.get(key) {
                if matches!(entry.status, CacheStatus::Negative { .. }) {
                    store.remove(key);
                    changed = true;
                }
            }
        }

        if changed {
            log::info!(
                "[Cache] subscription refresh: resurrected {} server(s)",
                new_ids.len()
            );
        }

        // Also clear hot cache entries that are no longer valid
        // (moka doesn't support selective remove by key pattern,
        // but we can invalidate specific keys)
        // For now, this is sufficient — hot cache TTL will handle stale entries.
    }

    /// Persist the current store to disk.
    pub async fn save_to_disk(&self) {
        let store = self.store.read().await;
        persistence::save_cache(&self.data_dir, &store);
    }

    /// Load persisted cache from disk into the store.
    pub async fn load_from_disk(&self) {
        let persisted = persistence::load_cache(&self.data_dir);
        let count = persisted.len();
        let mut store = self.store.write().await;
        *store = persisted;
        log::info!("[Cache] loaded {count} entries from disk");
    }

    /// Get the number of positive entries in the store.
    pub async fn positive_count(&self) -> usize {
        let store = self.store.read().await;
        store
            .values()
            .filter(|e| matches!(e.status, CacheStatus::Positive { .. }))
            .count()
    }

    /// Get the number of negative entries in the store.
    pub async fn negative_count(&self) -> usize {
        let store = self.store.read().await;
        store
            .values()
            .filter(|e| matches!(e.status, CacheStatus::Negative { .. }))
            .count()
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vpn::cache;

    fn make_entry(server_id: &str, ping_ms: u64) -> CacheEntry {
        CacheEntry {
            server_id: server_id.to_string(),
            last_updated: 1_000_000,
            status: CacheStatus::Positive {
                ping_ms,
                exit_ip: None,
                exit_flag: None,
                exit_country: None,
                exit_city: None,
                behind_cdn: false,
                consecutive_successes: 1,
            },
        }
    }

    fn make_neg(server_id: &str, failures: u32, worked: bool, reason: &str) -> CacheEntry {
        CacheEntry {
            server_id: server_id.to_string(),
            last_updated: 1_000_000,
            status: CacheStatus::Negative {
                first_failure_at: 1_000_000,
                last_failure_at: 1_000_000,
                consecutive_failures: failures,
                previously_worked: worked,
                reason: reason.to_string(),
            },
        }
    }

    // ── calculate_negative_ttl ──────────────────────────────────────────────

    #[test]
    fn test_negative_ttl_previously_worked_base_60() {
        let ttl = calculate_negative_ttl(1, true);
        assert_eq!(ttl.as_secs(), 60);
    }

    #[test]
    fn test_negative_ttl_never_worked_base_300() {
        let ttl = calculate_negative_ttl(1, false);
        assert_eq!(ttl.as_secs(), 300);
    }

    #[test]
    fn test_negative_ttl_exponential_backoff() {
        // failure=1: base * 2^0 = base
        // failure=2: base * 2^1 = base*2
        // failure=3: base * 2^2 = base*4
        let ttl1 = calculate_negative_ttl(1, true).as_secs();
        let ttl2 = calculate_negative_ttl(2, true).as_secs();
        let ttl3 = calculate_negative_ttl(3, true).as_secs();
        assert_eq!(ttl1, 60);
        assert_eq!(ttl2, 120);
        assert_eq!(ttl3, 240);
    }

    #[test]
    fn test_negative_ttl_capped_at_7200() {
        // failure=7: clamp(7,6)->5 -> 2^5=32 -> 300*32=9600 -> min(9600,7200)=7200
        // failure=99: clamp(99,6)->5 -> same
        let ttl = calculate_negative_ttl(99, false);
        assert_eq!(ttl.as_secs(), 7200);
    }

    #[test]
    fn test_negative_ttl_zero_failures_no_underflow() {
        // ⚠️ This is the case that would underflow in the original TZ.
        // consecutive_failures=0: max(1,0)=1 -> 1-1=0 -> 2^0=1 -> 60*1=60
        let ttl = calculate_negative_ttl(0, true);
        assert_eq!(ttl.as_secs(), 60);
    }

    #[test]
    fn test_negative_ttl_one_failure() {
        let ttl = calculate_negative_ttl(1, false);
        assert_eq!(ttl.as_secs(), 300);
    }

    #[test]
    fn test_negative_ttl_six_failures() {
        // failure=6: max(1,6)->6 -> 6-1=5 -> 2^5=32 -> 60*32=1920
        let ttl = calculate_negative_ttl(6, true);
        assert_eq!(ttl.as_secs(), 1920);
    }

    // ── is_negative_expired ─────────────────────────────────────────────────

    #[test]
    fn test_negative_unexpired() {
        let entry = make_neg("s1", 1, false, "timeout");
        let now = 1_000_000 + 100; // 100s ago (TTL=300s)
        assert!(!is_negative_expired(&entry, now));
    }

    #[test]
    fn test_negative_expired() {
        let entry = make_neg("s1", 1, false, "timeout");
        let now = 1_000_000 + 400; // 400s ago (TTL=300s)
        assert!(is_negative_expired(&entry, now));
    }

    #[test]
    fn test_positive_entry_not_expired() {
        let entry = make_entry("s1", 42);
        let now = 1_000_000 + 1000;
        // Positive entries are not subject to negative TTL — always "expired"
        assert!(is_negative_expired(&entry, now));
    }

    // ── is_positive_stale ───────────────────────────────────────────────────

    #[test]
    fn test_positive_fresh() {
        let now = 1000;
        assert!(!is_positive_stale(now - 300, now)); // 5 min ago
    }

    #[test]
    fn test_positive_stale() {
        let now = 1000;
        assert!(is_positive_stale(now - 601, now)); // >10 min ago
    }

    #[test]
    fn test_positive_exact_boundary() {
        let now = 1000;
        assert!(!is_positive_stale(now - 600, now)); // exactly 10 min — still fresh
    }

    // ── CacheEntry.to_positive ───────────────────────────────────────────────

    #[test]
    fn test_positive_to_positive_returns_some() {
        let entry = make_entry("s1", 42);
        let result = entry.to_positive();
        assert!(result.is_some());
        assert_eq!(result.unwrap().ping_ms, 42);
    }

    #[test]
    fn test_negative_to_positive_returns_none() {
        let entry = make_neg("s1", 1, false, "err");
        assert!(entry.to_positive().is_none());
    }

    // ── handle_subscription_refresh ─────────────────────────────────────────

    #[tokio::test]
    async fn test_refresh_removes_deleted_servers() {
        let dir = std::env::temp_dir().join(format!(
            "cache-test-refresh-remove-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cache = ServerCache::new(&dir);
        {
            let mut store = cache.store.write().await;
            store.insert("s1".into(), make_entry("s1", 10));
            store.insert("s2".into(), make_entry("s2", 20));
            store.insert("s3".into(), make_entry("s3", 30));
        }

        let new_ids: std::collections::HashSet<String> =
            ["s1".into(), "s3".into()].into_iter().collect();
        cache.handle_subscription_refresh(&new_ids).await;

        let store = cache.store.read().await;
        assert_eq!(store.len(), 2);
        assert!(store.contains_key("s1"));
        assert!(store.contains_key("s3"));
        assert!(!store.contains_key("s2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_refresh_resurrects_surviving_negative() {
        let dir = std::env::temp_dir().join(format!(
            "cache-test-resurrect-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cache = ServerCache::new(&dir);
        {
            let mut store = cache.store.write().await;
            store.insert("alive".into(), make_entry("alive", 10));
            store.insert("dead".into(), make_neg("dead", 3, true, "timeout"));
        }

        let new_ids: std::collections::HashSet<String> =
            ["alive".into(), "dead".into()].into_iter().collect();
        cache.handle_subscription_refresh(&new_ids).await;

        // "dead" was negative — should be resurrected (removed from cache)
        let store = cache.store.read().await;
        assert_eq!(store.len(), 1);
        assert!(store.contains_key("alive"));
        assert!(!store.contains_key("dead"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── PositiveCount / NegativeCount ───────────────────────────────────────

    #[tokio::test]
    async fn test_counts() {
        let dir = std::env::temp_dir().join(format!(
            "cache-test-counts-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cache = ServerCache::new(&dir);
        {
            let mut store = cache.store.write().await;
            store.insert("p1".into(), make_entry("p1", 10));
            store.insert("p2".into(), make_entry("p2", 20));
            store.insert("n1".into(), make_neg("n1", 1, false, "err"));
        }

        assert_eq!(cache.positive_count().await, 2);
        assert_eq!(cache.negative_count().await, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
