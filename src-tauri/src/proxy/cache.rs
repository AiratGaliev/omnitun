//! Dual-layer server cache with Stale-While-Revalidate (SWR).
//!
//! Architecture:
//! ```text
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

use crate::proxy::port_pool::PortPool;
use crate::proxy::ServerProfile;
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, RwLock};
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
/// 1 мин, экспоненциальный backoff: 1м → 2м → 4м → 8м → 16м → 32м (plateau).
/// Быстро перетестировать — сервер мог ожить.
const NEGATIVE_BASE_WORKED_SECS: u64 = 60; // 1 min

/// Negative cache base TTL for servers that never worked.
/// Тоже 1 день — нет смысла тестировать чаще.
const NEGATIVE_BASE_NEVER_WORKED_SECS: u64 = 86400; // 1 day

/// Maximum negative cache TTL — 32 дня.
/// Экспоненциальный backoff доходит до multiplier=32, и логично
/// чтобы cap совпадал с пиковым значением.
const NEGATIVE_MAX_TTL_SECS: u64 = 2764800; // 32 days

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
    let clamped = consecutive_failures.clamp(1, 6) - 1;
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
    #[allow(dead_code)]
    data_dir: Arc<Path>,
    /// Synchronous SQLite connection for cache persistence.
    /// All cache operations write through to the `server_cache` table.
    sync_conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    /// Whether the batcher has been started.
    batcher_started: Arc<std::sync::atomic::AtomicBool>,
}

/// A request sent to the batch worker.
pub struct BatchRequest {
    pub server: ServerProfile,
    pub profile_id: String,
    pub cancel: CancellationToken,
    pub reply: tokio::sync::oneshot::Sender<Result<PositiveResult, String>>,
}

impl ServerCache {
    /// Create a new cache, loading persisted data from disk.
    ///
    /// Does NOT start the batch worker — call `start_batcher()` after app setup.
    /// This allows the cache to be created early and queue requests before the
    /// batcher is fully initialized.
    pub fn new(data_dir: &Path) -> Self {
        // Open synchronous SQLite connection for cache persistence
        let db_path = data_dir.join("omnitun.db");
        let sync_conn = match rusqlite::Connection::open(&db_path) {
            Ok(c) => {
                let _ = c.execute_batch(
                    "PRAGMA journal_mode=WAL;
                     PRAGMA synchronous=NORMAL;
                     PRAGMA busy_timeout=5000;
                     PRAGMA foreign_keys=ON;",
                );
                let _ = c.execute_batch(crate::db::migrations::CREATE_TABLES_SQL);
                c
            }
            Err(e) => {
                tracing::error!(target: "cache", "Failed to open cache DB: {e} — using in-memory");
                let c = rusqlite::Connection::open_in_memory().expect("in-memory DB failed");
                let _ = c.execute_batch(crate::db::migrations::CREATE_TABLES_SQL);
                c
            }
        };
        let sync_conn = Arc::new(std::sync::Mutex::new(sync_conn));

        // Load persisted entries from SQLite
        let persisted = Self::load_from_sqlite(&sync_conn);
        let persisted_count = persisted.len();

        let store = Arc::new(RwLock::new(persisted));
        let hot: Cache<String, Arc<HotEntry>> = Cache::builder()
            .time_to_live(Duration::from_secs(POSITIVE_TTL_SECS))
            .time_to_idle(Duration::from_secs(POSITIVE_IDLE_SECS))
            .build();

        let (batch_tx, batch_rx) = tokio::sync::mpsc::unbounded_channel();

        tracing::info!(target: "cache", "[ServerCache] initialized with {persisted_count} persisted entries");
        tracing::info!(target: "cache",
            "[ServerCache] hot cache TTL={POSITIVE_TTL_SECS}s, idle={POSITIVE_IDLE_SECS}s, SWR stale={POSITIVE_STALE_SECS}s"
        );
        tracing::info!(target: "cache", "[ServerCache] batch channel created (not started — call start_batcher)");

        Self {
            store,
            hot,
            batch_tx,
            pending_rx: Arc::new(Mutex::new(Some(batch_rx))),
            data_dir: Arc::from(data_dir.to_path_buf()),
            sync_conn,
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
        data_dir: PathBuf,
        cancel: Arc<std::sync::Mutex<CancellationToken>>,
    ) {
        if self
            .batcher_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            tracing::warn!(target: "cache", "[ServerCache] start_batcher called twice — ignoring");
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
            crate::proxy::batcher::run(rx, cache, port_pool, data_dir, cancel).await;
        });

        tracing::info!(target: "cache", "[ServerCache] batch worker started");
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
                                    let rx = cache.queue_test(&srv, "", CancellationToken::new());
                                    match rx.await {
                                        Ok(Ok(r)) => {
                                            tracing::info!(target: "cache", "SWR refresh ok for {}", r.server_id)
                                        }
                                        Ok(Err(e)) => {
                                            tracing::warn!(target: "cache", "SWR refresh failed: {e}")
                                        }
                                        Err(_) => {
                                            tracing::warn!(target: "cache", "SWR refresh cancelled")
                                        }
                                    }
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
                                    .insert(sid, Arc::new(HotEntry { ping_ms, tested_at }))
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
        let rx = self.queue_test(server, "", CancellationToken::new());
        rx.await
            .map_err(|_| "Batch worker dropped the request".to_string())?
    }

    /// Update the store and hot cache with a positive result.
    pub(crate) async fn update_positive(&self, server_id: &str, result: PositiveResult) {
        // CDN servers must never store exit IP data in the cache.
        let mut result = result;
        if result.behind_cdn {
            // 🚫 ANTI-REGRESSION: логируем нарушение ПЕРЕД занулением.
            // PositiveResult — это результат test_single_server. Если здесь
            // есть exit-данные при behind_cdn=true, значит test_single_server
            // отдал неконсистентный результат. Это REGRESSION фикса B3.
            // Не удалять этот блок "потому что exit и так зануляется ниже".
            let has_exit = result.exit_ip.is_some()
                || result.exit_flag.is_some()
                || result.exit_country.is_some()
                || result.exit_city.is_some();
            if has_exit {
                tracing::error!(
                    target: "cdn-invariant",
                    "[BUG][I-CDN-1] INVARIANT VIOLATION in update_positive: \
                     server {} got PositiveResult with behind_cdn=true AND exit data \
                     (exit_ip={:?} exit_flag={:?} exit_country={:?} exit_city={:?}). \
                     This is a REGRESSION of fix B3 in test_single_server — \
                     behind_cdn should NOT be true with non-empty exit data. \
                     Clearing exit fields defensively.",
                    server_id,
                    result.exit_ip,
                    result.exit_flag,
                    result.exit_country,
                    result.exit_city,
                );
            }
            result.exit_ip = None;
            result.exit_flag = None;
            result.exit_country = None;
            result.exit_city = None;
        }
        // Look up previous entry to properly increment consecutive_successes
        let prev_successes = {
            let store = self.store.read().await;
            match store.get(server_id) {
                Some(CacheEntry {
                    status:
                        CacheStatus::Positive {
                            consecutive_successes,
                            ..
                        },
                    ..
                }) => *consecutive_successes,
                _ => 0,
            }
        };
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
                consecutive_successes: prev_successes + 1,
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
    pub(crate) fn queue_test(
        &self,
        server: &ServerProfile,
        profile_id: &str,
        cancel: CancellationToken,
    ) -> tokio::sync::oneshot::Receiver<Result<PositiveResult, String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Err(e) = self.batch_tx.send(BatchRequest {
            server: server.clone(),
            profile_id: profile_id.to_string(),
            cancel,
            reply: tx,
        }) {
            tracing::error!(
                target: "cache",
                "Queue test failed — batch channel closed: {e}"
            );
        }
        rx
    }

    /// Public wrapper around `queue_test` — used by the MCP server
    /// to queue a single server for batch testing.
    pub fn queue_test_public(
        &self,
        server: &ServerProfile,
    ) -> tokio::sync::oneshot::Receiver<Result<PositiveResult, String>> {
        self.queue_test(server, "", CancellationToken::new())
    }

    /// Update the store with a negative result.
    ///
    /// ═══ update_negative — in-memory only, no SQLite ═══════════════════
    /// OWNERSHIP: this mutates the in-memory ServerCache store.
    /// Caller is responsible for persisting to SQLite via save_to_disk()
    /// or flush_cache_batch(). Called by:
    ///
    ///   - Batcher (process_batch in batcher.rs) for failed batch tests
    ///   - test_servers (lib.rs) for servers without Clash API ping
    ///
    /// This does NOT write to the `servers`, `profiles`, or `server_cache` tables.
    pub(crate) async fn update_negative(&self, server_id: &str, reason: String) {
        let now = chrono::Utc::now().timestamp();

        let mut store = self.store.write().await;
        let previous = store.get(server_id).cloned();

        // PRESERVE previously_worked across repeated failures: a server that
        // once worked must keep the fast-retry base TTL (60s) on every failure,
        // not drop to the never-worked base (86400s) after the 2nd failure.
        // This must match the SQL path in `flush_cache_batch` which uses
        // `ELSE server_cache.previously_worked` to avoid the same drift.
        let (consecutive_failures, previously_worked) = match previous {
            Some(CacheEntry {
                status:
                    CacheStatus::Negative {
                        consecutive_failures,
                        previously_worked,
                        ..
                    },
                ..
            }) => (consecutive_failures + 1, previously_worked),
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

    /// Check if a server is in the negative cache with active backoff.
    /// Returns `true` if the server has a recent negative entry that hasn't expired.
    pub async fn is_cached_negative(&self, server_id: &str) -> bool {
        let now = chrono::Utc::now().timestamp();
        let store = self.store.read().await;
        match store.get(server_id) {
            Some(entry) if matches!(entry.status, CacheStatus::Negative { .. }) => {
                !is_negative_expired(entry, now)
            }
            _ => false,
        }
    }

    /// Fast cache lookup for a positive result without triggering batch testing.
    /// Returns `Some(PositiveResult)` if the server has a fresh positive entry
    /// in either the hot cache or the persistent store. Returns `None` if the
    /// entry is stale, negative, or missing — caller should fall through to
    /// batch testing.
    pub async fn get_positive_if_fresh(&self, server_id: &str) -> Option<PositiveResult> {
        let now = chrono::Utc::now().timestamp();

        // Check hot cache first (fastest path)
        if let Some(hot) = self.hot.get(server_id).await {
            if !is_positive_stale(hot.tested_at, now) {
                let store = self.store.read().await;
                if let Some(entry) = store.get(server_id) {
                    if let Some(result) = entry.to_positive() {
                        return Some(result);
                    }
                }
            }
        }

        // Check persistent store
        let store = self.store.read().await;
        if let Some(entry) = store.get(server_id) {
            if let CacheStatus::Positive { .. } = &entry.status {
                if !is_positive_stale(entry.last_updated, now) {
                    return entry.to_positive();
                }
            }
        }

        None
    }

    /// Get any positive entry from the persistent store, regardless of freshness.
    /// Used as fallback when batch test fails — stale data is better than no data.
    pub async fn get_positive(&self, server_id: &str) -> Option<PositiveResult> {
        let store = self.store.read().await;
        match store.get(server_id) {
            Some(entry) if matches!(entry.status, CacheStatus::Positive { .. }) => {
                entry.to_positive()
            }
            _ => None,
        }
    }

    /// Remove entries that no longer exist in any profile, and resurrect
    /// surviving negative entries so they get re-tested.
    ///
    /// `all_active_ids` — all server IDs currently present across ALL subscriptions.
    /// Entries whose ID is not in this set are removed from the cache.
    pub async fn handle_subscription_refresh(
        &self,
        all_active_ids: &std::collections::HashSet<String>,
    ) {
        let mut store = self.store.write().await;
        let before = store.len();

        // Remove entries that are no longer in ANY subscription.
        // Preserve negative cache entries — their exponential backoff TTL
        // controls when a dead server is eligible for re-testing.
        // 🚫 ANTI-REGRESSION: removing negative entries here defeats the
        // entire negative cache optimization.
        store.retain(|key, entry| {
            all_active_ids.contains(key) || matches!(entry.status, CacheStatus::Negative { .. })
        });
        let removed = before - store.len();

        // NOTE: negative cache entries for surviving servers are NOT resurrected
        // here. The exponential backoff in `is_negative_expired` controls when a
        // dead server is eligible for re-testing (5 min → 10 min → 20 min → ...).
        // Resurrection would defeat the negative cache and force a re-test on
        // every subscription refresh, wasting resources on known-dead servers.

        if removed > 0 {
            tracing::info!(target: "cache",
                "[Cache] subscription refresh: removed {removed} stale server(s) ({} remaining)",
                store.len(),
            );
        }
    }

    /// Remove entries not in `active_ids` and older than `max_age`.
    /// Grace period: recently-used entries survive `max_age` even without a profile.
    pub async fn prune_old(
        &self,
        active_ids: &std::collections::HashSet<String>,
        max_age: std::time::Duration,
    ) -> usize {
        let now = chrono::Utc::now().timestamp();
        let max_age_secs = max_age.as_secs() as i64;
        let mut store = self.store.write().await;
        let before = store.len();
        store.retain(|id, entry| {
            active_ids.contains(id) || (now - entry.last_updated) < max_age_secs
        });
        let removed = before - store.len();
        if removed > 0 {
            tracing::info!(target: "cache",
                "[Cache] pruned {removed} old entries older than {}d ({} remaining)",
                max_age_secs / 86400,
                store.len(),
            );
        }
        removed
    }

    /// Clear negative-cache entries for the given server IDs.
    /// Used by test_servers(force=true) so the user can manually re-test
    /// servers that are in exponential-backoff.
    ///
    /// NOTE: Negative entries are stored in self.store (CacheEntry with
    /// status=Negative), NOT a separate field. This method removes them
    /// from the store entirely, resetting the failure counter.
    pub async fn clear_negative_for_ids(&self, ids: &[String]) {
        let mut store = self.store.write().await;
        let before = store.len();
        store.retain(|k, v| !(ids.contains(k) && matches!(v.status, CacheStatus::Negative { .. })));
        let removed = before - store.len();
        if removed > 0 {
            tracing::info!(target:"cache", "[Cache] cleared {} negative entries (force re-test)", removed);
        }
    }

    /// Persist the current store to the `server_cache` SQLite table.
    ///
    /// ═══ save_to_disk — writes ONLY to `server_cache` (secondary async path) ═══
    /// OWNERSHIP: this is the SECONDARY async persistence path for server_cache.
    /// The PRIMARY path is flush_cache_batch() which writes via the async DbPool.
    /// save_to_disk uses the sync connection and is called:
    ///
    ///   - After process_servers_incremental completes (add_subscription, refresh)
    ///   - After test_servers completes
    ///   - After cache.handle_subscription_refresh
    ///
    /// It does NOT write to servers, profiles, or test_history.
    pub async fn save_to_disk(&self) {
        let store = self.store.read().await;
        // Recover from poisoned mutex (consistent with the rest of the codebase):
        // a panic in another thread while holding this lock would otherwise crash
        // every subsequent save, taking down the command. The connection itself
        // is not corrupted by the poisoning — only the lock state is.
        let conn = self.sync_conn.lock().unwrap_or_else(|e| e.into_inner());
        for entry in store.values() {
            let (
                status,
                ping_ms,
                exit_ip,
                exit_flag,
                exit_country_name,
                exit_city_name,
                behind_cdn,
                consecutive_successes,
                consecutive_failures,
                previously_worked,
                first_failure_at,
                last_failure_at,
                reason,
            ) = match &entry.status {
                CacheStatus::Positive {
                    ping_ms,
                    exit_ip,
                    exit_flag,
                    exit_country,
                    exit_city,
                    behind_cdn,
                    consecutive_successes,
                } => (
                    "positive",
                    Some(*ping_ms as i64),
                    exit_ip.clone(),
                    exit_flag.clone(),
                    exit_country.clone(),
                    exit_city.clone(),
                    *behind_cdn as i32,
                    *consecutive_successes as i32,
                    0i32,
                    1i32,
                    None::<i64>,
                    None::<i64>,
                    None::<String>,
                ),
                CacheStatus::Negative {
                    first_failure_at,
                    last_failure_at,
                    consecutive_failures,
                    previously_worked,
                    reason,
                } => (
                    "negative",
                    None::<i64>,
                    None::<String>,
                    None::<String>,
                    None::<String>,
                    None::<String>,
                    0i32,
                    0i32,
                    *consecutive_failures as i32,
                    *previously_worked as i32,
                    Some(*first_failure_at),
                    Some(*last_failure_at),
                    Some(reason.clone()),
                ),
            };
            if let Err(e) = conn.execute(
                "INSERT OR REPLACE INTO server_cache
                 (server_hash, status, ping_ms, exit_ip, exit_flag,
                  exit_country_name, exit_city_name, behind_cdn,
                  consecutive_successes, consecutive_failures,
                  previously_worked, first_failure_at, last_failure_at,
                  reason, tested_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                         ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    entry.server_id,
                    status,
                    ping_ms,
                    exit_ip,
                    exit_flag,
                    exit_country_name,
                    exit_city_name,
                    behind_cdn,
                    consecutive_successes,
                    consecutive_failures,
                    previously_worked,
                    first_failure_at,
                    last_failure_at,
                    reason,
                    entry.last_updated,
                ],
            ) {
                tracing::error!(target: "cache", "Failed to save cache entry: {e}");
            }
        }
    }

    /// Clear persisted, source-of-truth, and hot cache entries.
    ///
    /// This is intentionally one operation so callers cannot observe an empty
    /// SQLite table while stale entries remain available through the hot cache.
    pub async fn clear_all(&self) -> Result<(), String> {
        {
            let conn = self
                .sync_conn
                .lock()
                .map_err(|_| "Cache database lock is poisoned".to_string())?;
            conn.execute("DELETE FROM server_cache", [])
                .map_err(|e| format!("Failed to clear persisted server cache: {e}"))?;
        }

        self.hot.invalidate_all();
        self.hot.run_pending_tasks().await;
        self.store.write().await.clear();
        Ok(())
    }

    /// Load server_cache entries from SQLite into the in-memory store.
    /// Replaces the current store contents with data from SQLite.
    pub async fn load_from_disk(&self) {
        let entries = Self::load_from_sqlite(&self.sync_conn);
        let count = entries.len();
        let mut store = self.store.write().await;
        *store = entries;
        tracing::info!(target: "cache", "[Cache] loaded {count} entries from SQLite");
    }

    /// Get a locked handle to the synchronous SQLite connection.
    /// Returns `None` if the mutex is poisoned.
    /// Used by MCP commands to perform direct SQLite operations.
    pub fn get_sync_conn(&self) -> Option<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.sync_conn.lock().ok()
    }

    /// Load all cache entries from SQLite.
    /// Used at startup to hydrate the in-memory store.
    fn load_from_sqlite(
        conn: &Arc<std::sync::Mutex<rusqlite::Connection>>,
    ) -> HashMap<String, CacheEntry> {
        let guard = match conn.lock() {
            Ok(g) => g,
            Err(_) => return HashMap::new(),
        };
        Self::load_from_connection(&guard)
    }

    fn load_from_connection(conn: &rusqlite::Connection) -> HashMap<String, CacheEntry> {
        let mut stmt = match conn.prepare(
            "SELECT server_hash, status, ping_ms, exit_ip, exit_flag,
                    exit_country_name, exit_city_name, behind_cdn,
                    consecutive_successes, consecutive_failures,
                    previously_worked, first_failure_at, last_failure_at,
                    reason, tested_at
             FROM server_cache",
        ) {
            Ok(s) => s,
            Err(_) => return HashMap::new(),
        };
        let rows = match stmt.query_map([], |row| {
            let status: String = row.get("status")?;
            Ok((
                row.get::<_, String>("server_hash")?,
                CacheEntry {
                    server_id: row.get("server_hash")?,
                    last_updated: row.get("tested_at")?,
                    status: match status.as_str() {
                        "positive" => CacheStatus::Positive {
                            ping_ms: row.get::<_, Option<i64>>("ping_ms")?.unwrap_or(0) as u64,
                            exit_ip: row.get("exit_ip")?,
                            exit_flag: row.get("exit_flag")?,
                            exit_country: row.get("exit_country_name")?,
                            exit_city: row.get("exit_city_name")?,
                            behind_cdn: row.get::<_, Option<i32>>("behind_cdn")?.unwrap_or(0) != 0,
                            consecutive_successes: row
                                .get::<_, Option<i32>>("consecutive_successes")?
                                .unwrap_or(0)
                                as u32,
                        },
                        _ => CacheStatus::Negative {
                            first_failure_at: row
                                .get::<_, Option<i64>>("first_failure_at")?
                                .unwrap_or(0),
                            last_failure_at: row
                                .get::<_, Option<i64>>("last_failure_at")?
                                .unwrap_or(0),
                            consecutive_failures: row
                                .get::<_, Option<i32>>("consecutive_failures")?
                                .unwrap_or(0)
                                as u32,
                            previously_worked: row
                                .get::<_, Option<i32>>("previously_worked")?
                                .unwrap_or(0)
                                != 0,
                            reason: row.get::<_, Option<String>>("reason")?.unwrap_or_default(),
                        },
                    },
                },
            ))
        }) {
            Ok(r) => r,
            Err(_) => return HashMap::new(),
        };
        let mut map = HashMap::new();
        for row in rows.flatten() {
            map.insert(row.0, row.1);
        }
        map
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

/// Flush a batch of positive/negative results to the SQLite `server_cache` table.
///
/// Uses a single transaction for the batch. `consecutive_successes` is
/// incremented on positive hit; `consecutive_failures` on negative hit.
/// This is the direct-to-SQLite version that replaces the old moka+store path.
///
/// When `cache` is provided, in-memory store is also updated so subsequent
/// `is_cached_negative` lookups reflect the new state immediately.
///
/// ═══ OWNERSHIP: only writes to `server_cache` ════════════════════════
/// This function is SHARED by multiple callers:
///
///   - Phase 2 (preflight filter via process_servers_incremental)
///   - Phase 3 (batch_test_incremental after BatchingEngine)
///   - No-exit filter (process_servers_incremental)
///   - test_servers (via cache.update_negative)
///
/// All of them write ONLY to `server_cache`. None write to `servers`,
/// `profiles`, or `test_history`. Those tables have their own dedicated
/// writers (Phase 3 POST for servers, save() for profiles,
/// batch_test_incremental for test_history).
pub async fn flush_cache_batch(
    db: &crate::db::DbPool,
    cache: Option<&ServerCache>,
    positives: &[PositiveResult],
    negatives: &[(String, String)], // (server_id, reason)
) {
    if positives.is_empty() && negatives.is_empty() {
        return;
    }
    let now = chrono::Utc::now().timestamp();

    let positives_owned: Vec<PositiveResult> = positives.to_vec();
    let negatives_owned: Vec<(String, String)> = negatives.to_vec();
    let sync_store = cache.is_some();

    let result: Result<
        Option<HashMap<String, CacheEntry>>,
        tokio_rusqlite::Error<rusqlite::Error>,
    > = db
        .call(move |conn| {
            let tx = conn.transaction()?;
            for p in &positives_owned {
                // On CONFLICT: increment consecutive_successes, reset failures
                tx.execute(
                    "INSERT INTO server_cache
                 (server_hash, status, ping_ms, exit_ip, exit_flag,
                  exit_country_name, exit_city_name, behind_cdn,
                  consecutive_successes, previously_worked, tested_at)
                 VALUES (?1, 'positive', ?2, ?3, ?4, ?5, ?6, ?7, 1, 1, ?8)
                 ON CONFLICT(server_hash) DO UPDATE SET
                     status = 'positive',
                     ping_ms = excluded.ping_ms,
                     exit_ip = excluded.exit_ip,
                     exit_flag = excluded.exit_flag,
                     exit_country_name = excluded.exit_country_name,
                     exit_city_name = excluded.exit_city_name,
                     behind_cdn = excluded.behind_cdn,
                     tested_at = excluded.tested_at,
                     consecutive_successes = server_cache.consecutive_successes + 1,
                     previously_worked = 1,
                     consecutive_failures = 0,
                     first_failure_at = NULL,
                     last_failure_at = NULL,
                     reason = NULL",
                    rusqlite::params![
                        p.server_id,
                        p.ping_ms,
                        p.exit_ip,
                        p.exit_flag,
                        p.exit_country,
                        p.exit_city,
                        p.behind_cdn,
                        now
                    ],
                )?;
            }
            for (sid, reason) in &negatives_owned {
                tx.execute(
                    "INSERT INTO server_cache
                 (server_hash, status, reason, tested_at,
                  consecutive_failures, previously_worked, first_failure_at, last_failure_at)
                 VALUES (?1, 'negative', ?2, ?3, 1, 0, ?3, ?3)
                 ON CONFLICT(server_hash) DO UPDATE SET
                     status = 'negative',
                     reason = excluded.reason,
                     tested_at = excluded.tested_at,
                     consecutive_failures = CASE
                         WHEN server_cache.status = 'negative'
                         THEN server_cache.consecutive_failures + 1
                         ELSE 1
                     END,
                     previously_worked = CASE
                         WHEN server_cache.status = 'positive' THEN 1
                         ELSE server_cache.previously_worked
                     END,
                     first_failure_at = CASE
                         WHEN server_cache.status = 'negative' THEN server_cache.first_failure_at
                         ELSE excluded.first_failure_at
                     END,
                     last_failure_at = excluded.last_failure_at,
                     consecutive_successes = 0,
                     ping_ms = NULL, exit_ip = NULL, exit_flag = NULL,
                     exit_country_name = NULL, exit_city_name = NULL, behind_cdn = 0",
                    rusqlite::params![sid, reason, now],
                )?;
            }
            tx.commit()?;
            Ok(sync_store.then(|| ServerCache::load_from_connection(conn)))
        })
        .await;
    let committed_store = match result {
        Ok(store) => store,
        Err(e) => {
            tracing::error!(target: "cache", "flush_cache_batch failed: {e}");
            return;
        }
    };

    // Keep memory identical to the rows committed on the async connection.
    // This avoids a second SQLite round trip and preserves exact counters when
    // the batch worker already updated memory before this transaction.
    if let (Some(cache), Some(store)) = (cache, committed_store) {
        *cache.store.write().await = store;
    }
}

/// Incremental batch test: sends servers to the BatchingEngine via cache,
/// writes results to SQLite, and emits per-server events through `event_tx`.
///
/// Unlike the old `batch_test()`, this function:
/// - Uses `cache.queue_test()` to send requests to the background worker
/// - Writes results to SQLite via `flush_cache_batch`
/// - Emits `ServerTestedEvent` via `event_tx` for streaming UI updates
///
/// ═══ batch_test_incremental — writes to server_cache + test_history ═══
/// OWNERSHIP:
///
///   - `server_cache`: INSERTS via flush_cache_batch (positives + negatives)
///   - `test_history`: INSERTS per-server test result (phase='batch')
///
/// Does NOT write to `servers` or `profiles` — those are owned by
/// Phase 3 POST (servers) and save() (profiles).
pub async fn batch_test_incremental(
    cache: &ServerCache,
    servers: &[ServerProfile],
    cancel: Option<CancellationToken>,
    profile_id: &str,
    event_tx: &mpsc::UnboundedSender<crate::events::ServerTestedEvent>,
    db: &crate::db::DbPool,
) {
    let total = servers.len();
    if total == 0 {
        return;
    }
    let now = chrono::Utc::now().timestamp();

    // Queue all servers to the BatchingEngine via cache
    let mut pending = Vec::with_capacity(total);
    for server in servers {
        let cancel = cancel.clone().unwrap_or_default();
        let rx = cache.queue_test(server, profile_id, cancel);
        pending.push((server.id.clone(), rx));
    }

    // Await results
    let mut set = tokio::task::JoinSet::new();
    for (server_id, rx) in pending {
        set.spawn(async move { (server_id, rx.await) });
    }

    let mut positives: Vec<PositiveResult> = Vec::new();
    let mut negatives: Vec<(String, String)> = Vec::new();

    loop {
        let next = if let Some(token) = cancel.as_ref() {
            tokio::select! {
                _ = token.cancelled() => {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    None
                }
                result = set.join_next() => result,
            }
        } else {
            set.join_next().await
        };

        let Some(res) = next else {
            break;
        };

        match res {
            Ok((server_id, Ok(Ok(positive)))) => {
                let p = positive.clone();
                // Emit event
                let _ = event_tx.send(crate::events::ServerTestedEvent {
                    profile_id: profile_id.to_string(),
                    phase: "batch".into(),
                    server_id: server_id.clone(),
                    static_data: None,
                    ping_ms: Some(p.ping_ms),
                    exit_server: p.exit_ip.clone(),
                    exit_flag: p.exit_flag.clone(),
                    exit_country_name: p.exit_country.clone(),
                    exit_city_name: p.exit_city.clone(),
                    behind_cdn: Some(p.behind_cdn),
                    error: None,
                });
                // Write test history
                let _ = db.call({
                    let sid = server_id.clone();
                    let eip = p.exit_ip.clone();
                    move |conn| {
                        conn.execute(
                            "INSERT INTO test_history (server_id, tested_at, ping_ms, exit_ip, success, phase)
                             VALUES (?1, ?2, ?3, ?4, 1, 'batch')",
                            rusqlite::params![sid, now, p.ping_ms as i64, eip],
                        )?;
                        Ok::<_, rusqlite::Error>(())
                    }
                }).await;
                positives.push(p);
            }
            Ok((server_id, Ok(Err(e)))) => {
                if e == "Cancelled" {
                    continue;
                }
                let _ = event_tx.send(crate::events::ServerTestedEvent {
                    profile_id: profile_id.to_string(),
                    phase: "error".into(),
                    server_id: server_id.clone(),
                    static_data: None,
                    ping_ms: None,
                    exit_server: None,
                    exit_flag: None,
                    exit_country_name: None,
                    exit_city_name: None,
                    behind_cdn: None,
                    error: Some(e.clone()),
                });
                // Write test history
                let _ = db
                    .call({
                        let sid = server_id.clone();
                        let err = e.clone();
                        move |conn| {
                            conn.execute(
                            "INSERT INTO test_history (server_id, tested_at, success, error, phase)
                             VALUES (?1, ?2, 0, ?3, 'batch')",
                            rusqlite::params![sid, now, err],
                        )?;
                            Ok::<_, rusqlite::Error>(())
                        }
                    })
                    .await;
                negatives.push((server_id, e));
            }
            Err(join_err) => {
                tracing::warn!(
                    target: "cache",
                    "[batch_test_incremental] JoinSet task panicked or JoinError: {join_err}",
                );
            }
            _ => {
                tracing::warn!(
                    target: "cache",
                    "[batch_test_incremental] oneshot receiver dropped — batch worker may have crashed",
                );
            }
        }

        // Flush every 50 results
        if positives.len() >= 50 || negatives.len() >= 50 {
            flush_cache_batch(db, Some(cache), &positives, &negatives).await;
            positives.clear();
            negatives.clear();
        }
    }

    // Final flush
    flush_cache_batch(db, Some(cache), &positives, &negatives).await;

    // Cleanup: keep as long as negative cache (NEGATIVE_MAX_TTL_SECS = 32 days)
    let cutoff = now - (NEGATIVE_MAX_TTL_SECS as i64); // keep as long as negative cache
    let _ = db
        .call(move |conn| {
            conn.execute(
                "DELETE FROM test_history WHERE tested_at < ?1",
                rusqlite::params![cutoff],
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .await;
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_negative_ttl_never_worked_base_86400() {
        let ttl = calculate_negative_ttl(1, false);
        assert_eq!(ttl.as_secs(), 86400);
    }

    #[test]
    fn test_negative_ttl_exponential_backoff() {
        let ttl1 = calculate_negative_ttl(1, true).as_secs();
        let ttl2 = calculate_negative_ttl(2, true).as_secs();
        let ttl3 = calculate_negative_ttl(3, true).as_secs();
        assert_eq!(ttl1, 60);
        assert_eq!(ttl2, 120);
        assert_eq!(ttl3, 240);
    }

    #[test]
    fn test_negative_ttl_plateau_previously_worked() {
        // failure=99: previously-worked → base=60 * 32 = 1920 → min(1920, 2764800)=1920
        let ttl = calculate_negative_ttl(99, true);
        assert_eq!(ttl.as_secs(), 1920);
    }

    #[test]
    fn test_negative_ttl_zero_failures_no_underflow() {
        // consecutive_failures=0: clamp to 1 → 2^0=1 → 60*1=60
        let ttl = calculate_negative_ttl(0, true);
        assert_eq!(ttl.as_secs(), 60);
    }

    #[test]
    fn test_negative_ttl_one_failure_never_worked() {
        let ttl = calculate_negative_ttl(1, false);
        assert_eq!(ttl.as_secs(), 86400);
    }

    #[test]
    fn test_negative_ttl_six_failures_previously_worked() {
        // failure=6: base=60 * 32 = 1920
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
        let now = 1_000_000 + 86500; // 86500s ago (TTL=86400s for never-worked)
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
        let dir =
            std::env::temp_dir().join(format!("cache-test-refresh-remove-{}", std::process::id()));
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
    async fn test_refresh_keeps_surviving_negative() {
        let dir = std::env::temp_dir().join(format!("cache-test-keep-neg-{}", std::process::id()));
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

        // Negative entries are NOT resurrected — they stay until backoff expires.
        // The exponential backoff controls when a dead server is re-tested.
        let store = cache.store.read().await;
        assert_eq!(store.len(), 2);
        assert!(store.contains_key("alive"));
        assert!(store.contains_key("dead"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── prune_old ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_prune_old_keeps_active_ids_regardless_of_age() {
        let dir =
            std::env::temp_dir().join(format!("cache-test-prune-active-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cache = ServerCache::new(&dir);
        let now = chrono::Utc::now().timestamp();
        {
            let mut store = cache.store.write().await;
            // Very old entry (100 days ago) but in active_ids — must survive
            store.insert(
                "old-active".into(),
                CacheEntry {
                    server_id: "old-active".into(),
                    last_updated: now - 86400 * 100,
                    status: CacheStatus::Positive {
                        ping_ms: 50,
                        exit_ip: None,
                        exit_flag: None,
                        exit_country: None,
                        exit_city: None,
                        behind_cdn: false,
                        consecutive_successes: 1,
                    },
                },
            );
            // Fresh entry in active_ids
            store.insert(
                "fresh-active".into(),
                CacheEntry {
                    server_id: "fresh-active".into(),
                    last_updated: now,
                    status: CacheStatus::Positive {
                        ping_ms: 30,
                        exit_ip: None,
                        exit_flag: None,
                        exit_country: None,
                        exit_city: None,
                        behind_cdn: false,
                        consecutive_successes: 1,
                    },
                },
            );
        }

        let active: std::collections::HashSet<String> =
            ["old-active".into(), "fresh-active".into()]
                .into_iter()
                .collect();
        let max_age = Duration::from_secs(86400 * 30); // 30 days
        let removed = cache.prune_old(&active, max_age).await;

        assert_eq!(removed, 0, "should not remove active IDs");
        let store = cache.store.read().await;
        assert_eq!(store.len(), 2);
        assert!(store.contains_key("old-active"));
        assert!(store.contains_key("fresh-active"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_prune_old_removes_orphans_older_than_max_age() {
        let dir =
            std::env::temp_dir().join(format!("cache-test-prune-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cache = ServerCache::new(&dir);
        let now = chrono::Utc::now().timestamp();
        {
            let mut store = cache.store.write().await;
            // Orphan (not in active_ids), very old — should be removed
            store.insert(
                "orphan-old".into(),
                CacheEntry {
                    server_id: "orphan-old".into(),
                    last_updated: now - 86400 * 35, // 35 days ago
                    status: CacheStatus::Negative {
                        first_failure_at: now - 86400 * 35,
                        last_failure_at: now - 86400 * 35,
                        consecutive_failures: 5,
                        previously_worked: true,
                        reason: "timeout".into(),
                    },
                },
            );
        }

        let active: std::collections::HashSet<String> = std::collections::HashSet::new();
        let max_age = Duration::from_secs(86400 * 30); // 30 days
        let removed = cache.prune_old(&active, max_age).await;

        assert_eq!(removed, 1, "should remove orphan older than max_age");
        let store = cache.store.read().await;
        assert!(store.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_prune_old_keeps_orphans_within_grace_period() {
        let dir =
            std::env::temp_dir().join(format!("cache-test-prune-grace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cache = ServerCache::new(&dir);
        let now = chrono::Utc::now().timestamp();
        {
            let mut store = cache.store.write().await;
            // Orphan but recent (within 30 days) — grace period keeps it alive
            store.insert(
                "orphan-recent".into(),
                CacheEntry {
                    server_id: "orphan-recent".into(),
                    last_updated: now - 86400 * 10, // 10 days ago
                    status: CacheStatus::Positive {
                        ping_ms: 100,
                        exit_ip: None,
                        exit_flag: None,
                        exit_country: None,
                        exit_city: None,
                        behind_cdn: false,
                        consecutive_successes: 1,
                    },
                },
            );
        }

        let active: std::collections::HashSet<String> = std::collections::HashSet::new();
        let max_age = Duration::from_secs(86400 * 30); // 30 days
        let removed = cache.prune_old(&active, max_age).await;

        assert_eq!(removed, 0, "orphans within grace period should survive");
        let store = cache.store.read().await;
        assert_eq!(store.len(), 1);
        assert!(store.contains_key("orphan-recent"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_counts() {
        let dir = std::env::temp_dir().join(format!("cache-test-counts-{}", std::process::id()));
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

    #[tokio::test]
    async fn test_clear_all_removes_persistent_store_and_hot_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = ServerCache::new(dir.path());
        {
            let mut store = cache.store.write().await;
            store.insert("positive".into(), make_entry("positive", 10));
            store.insert("negative".into(), make_neg("negative", 1, false, "timeout"));
        }
        cache
            .hot
            .insert(
                "positive".into(),
                Arc::new(HotEntry {
                    ping_ms: 10,
                    tested_at: 1_000_000,
                }),
            )
            .await;
        cache.save_to_disk().await;

        assert_eq!(cache.positive_count().await, 1);
        assert_eq!(cache.negative_count().await, 1);
        assert!(cache.hot.get("positive").await.is_some());

        cache.clear_all().await.unwrap();

        assert_eq!(cache.positive_count().await, 0);
        assert_eq!(cache.negative_count().await, 0);
        assert!(cache.hot.get("positive").await.is_none());
        let conn = cache.get_sync_conn().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM server_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_save_to_disk_preserves_entry_timestamp() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = ServerCache::new(dir.path());
        {
            let mut store = cache.store.write().await;
            store.insert("negative".into(), make_neg("negative", 1, false, "timeout"));
        }

        cache.save_to_disk().await;

        let conn = cache.get_sync_conn().unwrap();
        let tested_at: i64 = conn
            .query_row(
                "SELECT tested_at FROM server_cache WHERE server_hash = 'negative'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tested_at, 1_000_000);
    }

    #[tokio::test]
    async fn test_flush_cache_batch_syncs_positive_to_negative_in_memory() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init_db(dir.path()).await.unwrap();
        let cache = ServerCache::new(dir.path());
        cache
            .update_positive(
                "server",
                PositiveResult {
                    server_id: "server".into(),
                    ping_ms: 42,
                    exit_ip: Some("203.0.113.1".into()),
                    exit_flag: Some("US".into()),
                    exit_country: Some("United States".into()),
                    exit_city: None,
                    behind_cdn: false,
                    tested_at: chrono::Utc::now().timestamp(),
                },
            )
            .await;
        cache.save_to_disk().await;

        flush_cache_batch(
            &db,
            Some(&cache),
            &[],
            &[("server".into(), "preflight failed".into())],
        )
        .await;

        let store = cache.store.read().await;
        match &store.get("server").unwrap().status {
            CacheStatus::Negative {
                consecutive_failures,
                previously_worked,
                ..
            } => {
                assert_eq!(*consecutive_failures, 1);
                assert!(*previously_worked);
            }
            CacheStatus::Positive { .. } => panic!("positive entry was not replaced"),
        }
    }

    #[tokio::test]
    async fn test_flush_cache_batch_syncs_incremented_negative_in_memory() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init_db(dir.path()).await.unwrap();
        let cache = ServerCache::new(dir.path());
        cache.update_negative("server", "first".into()).await;
        cache.save_to_disk().await;

        flush_cache_batch(
            &db,
            Some(&cache),
            &[],
            &[("server".into(), "second".into())],
        )
        .await;

        let store = cache.store.read().await;
        match &store.get("server").unwrap().status {
            CacheStatus::Negative {
                consecutive_failures,
                reason,
                ..
            } => {
                assert_eq!(*consecutive_failures, 2);
                assert_eq!(reason, "second");
            }
            CacheStatus::Positive { .. } => panic!("negative entry became positive"),
        }
    }

    #[tokio::test]
    async fn test_flush_cache_batch_does_not_double_count_worker_update() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init_db(dir.path()).await.unwrap();
        let cache = ServerCache::new(dir.path());

        cache.update_negative("server", "failure".into()).await;
        flush_cache_batch(
            &db,
            Some(&cache),
            &[],
            &[("server".into(), "failure".into())],
        )
        .await;

        let store = cache.store.read().await;
        match &store.get("server").unwrap().status {
            CacheStatus::Negative {
                consecutive_failures,
                ..
            } => assert_eq!(*consecutive_failures, 1),
            CacheStatus::Positive { .. } => panic!("negative entry became positive"),
        }
    }
}
