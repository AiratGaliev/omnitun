//! Database layer for OmniTun.
//!
//! Uses SQLite via `tokio-rusqlite` (wraps `rusqlite::Connection` in a dedicated
//! thread, solving the `!Send` problem without a custom flume pool).
//!
//! # Architecture
//!
//! - `DbPool = Arc<tokio_rusqlite::Connection>` — Send + Sync, stored in
//!   Tauri State (`app.manage()`).
//! - All operations go through `db.call(|conn| { ... }).await` — rusqlite
//!   runs on a dedicated OS thread, never blocking the async runtime.
//! - WAL mode + `synchronous=NORMAL` — concurrent readers never block.
//!
//! # Schema
//!
//! See [`migrations`] for the full schema. Versioned via `PRAGMA user_version`.

pub mod migrations;

use std::path::Path;
use std::sync::Arc;

/// Type alias for the shared database connection.
///
/// `tokio_rusqlite::Connection` uses interior mutability — all SQL runs
/// through a dedicated OS thread. Wrapping in `Arc` makes it shareable
/// with Tauri State, spawned tasks, and the MCP server.
pub type DbPool = Arc<tokio_rusqlite::Connection>;

/// Initialize (or open) the SQLite database at `data_dir/omnitun.db`.
///
/// 1. Opens (or creates) the database file.
/// 2. Applies PRAGMAs (WAL, synchronous, busy_timeout, foreign_keys).
/// 3. Runs any pending migrations.
///
/// # Errors
///
/// Returns `Err(String)` if the database cannot be opened or migrations fail.
pub async fn init_db(data_dir: &Path) -> Result<DbPool, String> {
    let db_path = data_dir.join("omnitun.db");
    let conn = tokio_rusqlite::Connection::open(&db_path)
        .await
        .map_err(|e| format!("Failed to open database at {db_path:?}: {e}"))?;

    // Apply PRAGMAs + run migrations in one synchronous call.
    conn.call(move |inner| -> Result<(), rusqlite::Error> {
        // WAL + performance PRAGMAs (must be set before any other operation).
        inner.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;",
        )?;

        // Check current schema version and run pending migrations.
        let current_version: u32 = inner
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);

        if current_version < 1 {
            inner.execute_batch(migrations::CREATE_TABLES_SQL)?;
            inner.pragma_update(None, "user_version", 1)?;
            tracing::info!(target: "db", "Database migrated to schema v1");
        }

        // Schema v2: enforce NOT NULL on servers.ping_ms, servers.flag, servers.country_name
        //
        // ── ANTI-REGRESSION ─────────────────────────────────────────────────
        // ping_ms, flag, country_name must NEVER contain default/dummy values
        // (0, 'UN', 'Unknown'). If they are NULL, the pipeline has a REAL bug —
        // a server reached the servers table without batch test or GeoIP.
        // Do NOT add defaults or unwrap_or — let the NOT NULL constraint fail
        // and surface the bug immediately. Every server in this table MUST have
        // been through batch_test_incremental (for ping_ms) and GeoIP resolution
        // (for flag, country_name).
        // ────────────────────────────────────────────────────────────────────
        //
        if current_version < 2 {
            tracing::info!(target: "db", "Migrating to schema v2: adding NOT NULL constraints");

            // Remove rows with NULL ping_ms — they indicate pipeline failure
            let deleted_ping = inner.execute(
                "DELETE FROM servers WHERE ping_ms IS NULL",
                [],
            )?;
            if deleted_ping > 0 {
                tracing::warn!(
                    target: "db",
                    "v2 migration: removed {} server(s) with NULL ping_ms (pipeline defect)",
                    deleted_ping,
                );
            }

            // Drop stale intermediate table if it exists from a previous run.
            inner.execute("DROP TABLE IF EXISTS servers_v2", [])?;
            inner.execute_batch(

                "CREATE TABLE IF NOT EXISTS servers_v2 (
                    profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                    server_hash TEXT NOT NULL,
                    protocol TEXT NOT NULL,
                    server TEXT NOT NULL,
                    port INTEGER NOT NULL,
                    uuid TEXT,
                    password TEXT,
                    method TEXT,
                    sni TEXT,
                    flow TEXT,
                    public_key TEXT,
                    private_key TEXT,
                    transport_type TEXT,
                    transport_path TEXT,
                    transport_host TEXT,
                    reality_pbk TEXT,
                    reality_sid TEXT,
                    wg_local_address TEXT,
                    wg_allowed_ips TEXT,
                    wg_dns TEXT,
                    wg_mtu INTEGER,
                    wg_keepalive INTEGER,
                    username TEXT,
                    ping_ms INTEGER NOT NULL,
                    behind_cdn INTEGER DEFAULT 0,
                    flag TEXT NOT NULL,
                    country_name TEXT NOT NULL,
                    city_name TEXT,
                    exit_server TEXT,
                    exit_flag TEXT,
                    exit_country_name TEXT,
                    exit_city_name TEXT,
                    original_name TEXT,
                    sort_order INTEGER DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (profile_id, server_hash)
                );

                INSERT INTO servers_v2
                    SELECT * FROM servers;

                DROP TABLE servers;

                ALTER TABLE servers_v2 RENAME TO servers;

                CREATE INDEX IF NOT EXISTS idx_servers_profile_ping
                    ON servers(profile_id, ping_ms);
                CREATE INDEX IF NOT EXISTS idx_servers_exit_flag
                    ON servers(exit_flag) WHERE exit_flag IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_servers_protocol
                    ON servers(protocol);
            ")?;

            inner.pragma_update(None, "user_version", 2)?;
            tracing::info!(target: "db", "Database migrated to schema v2");
        }

        // Schema v3: CDN servers must not have exit data
        //
        // ── ANTI-REGRESSION ─────────────────────────────────────────────────
        // If behind_cdn = 1, then exit_server, exit_flag, exit_country_name
        // and exit_city_name MUST be NULL. If the pipeline writes exit data
        // for a CDN server, the CHECK constraint will fail immediately.
        // If behind_cdn = 0, exit_server must be set (either from batch test
        // or fallback to entry server).
        // ────────────────────────────────────────────────────────────────────
        if current_version < 3 {
            tracing::info!(target: "db", "Migrating to schema v3: CDN exit data constraint");

            inner.execute("DROP TABLE IF EXISTS servers_v3", [])?;
            inner.execute_batch(
                "CREATE TABLE IF NOT EXISTS servers_v3 (
                    profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                    server_hash TEXT NOT NULL,
                    protocol TEXT NOT NULL,
                    server TEXT NOT NULL,
                    port INTEGER NOT NULL,
                    uuid TEXT,
                    password TEXT,
                    method TEXT,
                    sni TEXT,
                    flow TEXT,
                    public_key TEXT,
                    private_key TEXT,
                    transport_type TEXT,
                    transport_path TEXT,
                    transport_host TEXT,
                    reality_pbk TEXT,
                    reality_sid TEXT,
                    wg_local_address TEXT,
                    wg_allowed_ips TEXT,
                    wg_dns TEXT,
                    wg_mtu INTEGER,
                    wg_keepalive INTEGER,
                    username TEXT,
                    ping_ms INTEGER NOT NULL,
                    behind_cdn INTEGER DEFAULT 0,
                    flag TEXT NOT NULL,
                    country_name TEXT NOT NULL,
                    city_name TEXT,
                    exit_server TEXT,
                    exit_flag TEXT,
                    exit_country_name TEXT,
                    exit_city_name TEXT,
                    original_name TEXT,
                    sort_order INTEGER DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (profile_id, server_hash),
                    CHECK(
                        (behind_cdn = 1 AND exit_server IS NULL AND exit_flag IS NULL
                                         AND exit_country_name IS NULL AND exit_city_name IS NULL)
                        OR
                        (behind_cdn = 0 AND exit_server IS NOT NULL)
                    )
                );

                INSERT INTO servers_v3 SELECT * FROM servers;

                DROP TABLE servers;

                ALTER TABLE servers_v3 RENAME TO servers;

                CREATE INDEX IF NOT EXISTS idx_servers_profile_ping
                    ON servers(profile_id, ping_ms);
                CREATE INDEX IF NOT EXISTS idx_servers_exit_flag
                    ON servers(exit_flag) WHERE exit_flag IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_servers_protocol
                    ON servers(protocol);
            ")?;

            inner.pragma_update(None, "user_version", 3)?;
            tracing::info!(target: "db", "Database migrated to schema v3");
        }

        // Schema v4: NOT NULL on behind_cdn, sort_order, original_name
        //
        // behind_cdn is a boolean — NULL is meaningless. Default 0 (not CDN)
        // is semantically correct for the common case, unlike ping_ms/flag/
        // country_name where defaults would mask pipeline bugs.
        // sort_order and original_name always have values from the parser.
        if current_version < 4 {
            tracing::info!(target: "db", "Migrating to schema v4: NOT NULL on behind_cdn, sort_order, original_name");

            // Fix any existing NULLs before the constraint
            inner.execute("UPDATE servers SET behind_cdn = 0 WHERE behind_cdn IS NULL", [])?;
            inner.execute("UPDATE servers SET sort_order = 0 WHERE sort_order IS NULL", [])?;
            inner.execute("UPDATE servers SET original_name = '' WHERE original_name IS NULL", [])?;

            inner.execute("DROP TABLE IF EXISTS servers_v4", [])?;
            inner.execute_batch(
                "CREATE TABLE IF NOT EXISTS servers_v4 (
                    profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                    server_hash TEXT NOT NULL,
                    protocol TEXT NOT NULL,
                    server TEXT NOT NULL,
                    port INTEGER NOT NULL,
                    uuid TEXT,
                    password TEXT,
                    method TEXT,
                    sni TEXT,
                    flow TEXT,
                    public_key TEXT,
                    private_key TEXT,
                    transport_type TEXT,
                    transport_path TEXT,
                    transport_host TEXT,
                    reality_pbk TEXT,
                    reality_sid TEXT,
                    wg_local_address TEXT,
                    wg_allowed_ips TEXT,
                    wg_dns TEXT,
                    wg_mtu INTEGER,
                    wg_keepalive INTEGER,
                    username TEXT,
                    ping_ms INTEGER NOT NULL,
                    behind_cdn INTEGER NOT NULL DEFAULT 0,
                    flag TEXT NOT NULL,
                    country_name TEXT NOT NULL,
                    city_name TEXT,
                    exit_server TEXT,
                    exit_flag TEXT,
                    exit_country_name TEXT,
                    exit_city_name TEXT,
                    original_name TEXT NOT NULL DEFAULT '',
                    sort_order INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (profile_id, server_hash),
                    CHECK(
                        (behind_cdn = 1 AND exit_server IS NULL AND exit_flag IS NULL
                                         AND exit_country_name IS NULL AND exit_city_name IS NULL)
                        OR
                        (behind_cdn = 0 AND exit_server IS NOT NULL)
                    )
                );

                INSERT INTO servers_v4 SELECT * FROM servers;

                DROP TABLE servers;

                ALTER TABLE servers_v4 RENAME TO servers;

                CREATE INDEX IF NOT EXISTS idx_servers_profile_ping
                    ON servers(profile_id, ping_ms);
                CREATE INDEX IF NOT EXISTS idx_servers_exit_flag
                    ON servers(exit_flag) WHERE exit_flag IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_servers_protocol
                    ON servers(protocol);
            ")?;

            inner.pragma_update(None, "user_version", 4)?;
            tracing::info!(target: "db", "Database migrated to schema v4");
        }

        // Schema v5: ensure NOT NULL constraints are really enforced.
        // Previous migrations used IF NOT EXISTS on intermediate tables which
        // silently skipped schema changes when a stale table existed.
        if current_version < 5 {
            tracing::info!(target: "db", "Migrating to schema v5: force NOT NULL constraints");

            // Clean up any NULL-ping rows that slipped through
            let deleted = inner.execute("DELETE FROM servers WHERE ping_ms IS NULL", [])?;
            if deleted > 0 {
                tracing::warn!(
                    target: "db",
                    "v5 migration: removed {} server(s) with NULL ping_ms",
                    deleted,
                );
            }

            inner.execute("DROP TABLE IF EXISTS servers_v5", [])?;
            inner.execute_batch(
                "CREATE TABLE IF NOT EXISTS servers_v5 (
                    profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                    server_hash TEXT NOT NULL,
                    protocol TEXT NOT NULL,
                    server TEXT NOT NULL,
                    port INTEGER NOT NULL,
                    uuid TEXT,
                    password TEXT,
                    method TEXT,
                    sni TEXT,
                    flow TEXT,
                    public_key TEXT,
                    private_key TEXT,
                    transport_type TEXT,
                    transport_path TEXT,
                    transport_host TEXT,
                    reality_pbk TEXT,
                    reality_sid TEXT,
                    wg_local_address TEXT,
                    wg_allowed_ips TEXT,
                    wg_dns TEXT,
                    wg_mtu INTEGER,
                    wg_keepalive INTEGER,
                    username TEXT,
                    ping_ms INTEGER NOT NULL,
                    behind_cdn INTEGER NOT NULL DEFAULT 0,
                    flag TEXT NOT NULL,
                    country_name TEXT NOT NULL,
                    city_name TEXT,
                    exit_server TEXT,
                    exit_flag TEXT,
                    exit_country_name TEXT,
                    exit_city_name TEXT,
                    original_name TEXT NOT NULL DEFAULT '',
                    sort_order INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (profile_id, server_hash),
                    CHECK(
                        (behind_cdn = 1 AND exit_server IS NULL AND exit_flag IS NULL
                                         AND exit_country_name IS NULL AND exit_city_name IS NULL)
                        OR
                        (behind_cdn = 0 AND exit_server IS NOT NULL)
                    )
                );

                INSERT INTO servers_v5 SELECT * FROM servers;

                DROP TABLE servers;

                ALTER TABLE servers_v5 RENAME TO servers;

                CREATE INDEX IF NOT EXISTS idx_servers_profile_ping
                    ON servers(profile_id, ping_ms);
                CREATE INDEX IF NOT EXISTS idx_servers_exit_flag
                    ON servers(exit_flag) WHERE exit_flag IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_servers_protocol
                    ON servers(protocol);
            ")?;

            inner.pragma_update(None, "user_version", 5)?;
            tracing::info!(target: "db", "Database migrated to schema v5");
        }

        // v6 → UNIQUE server_hash (cross-profile dedup enforcement)
        if current_version < 6 {
            // Delete duplicate server_hashes before creating the UNIQUE index
            inner.execute_batch(
                "DELETE FROM servers WHERE rowid NOT IN (
                    SELECT MIN(rowid) FROM servers GROUP BY server_hash
                );"
            )?;
            inner.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_servers_server_hash_unique
                 ON servers(server_hash);"
            )?;
            inner.pragma_update(None, "user_version", 6)?;
            tracing::info!(target: "db", "Database migrated to schema v6 (UNIQUE server_hash)");
        }

        Ok(())
    })
    .await
    .map_err(|e: tokio_rusqlite::Error<_>| format!("Database migration failed: {e}"))?;

    tracing::info!(target: "db", "Database ready at {:?}", db_path);
    Ok(Arc::new(conn))
}

/// Run a one-time migration from `profiles.json` / `cache.json` to SQLite.
///
/// This is called during app startup if the old JSON files exist and the
/// SQLite database is newly created (no `profiles` rows yet).
///
/// # Safety
///
/// Idempotent — safe to run multiple times. If JSON files are missing or
/// malformed, logs a warning and continues (no data loss).
pub async fn migrate_from_json(db: &DbPool, data_dir: &Path) -> Result<(), String> {
    // Check if we already have data — skip if profiles exist.

    let has_data: bool = db
        .call(|conn| -> Result<bool, rusqlite::Error> {
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
                .unwrap_or(0);
            Ok(count > 0)
        })
        .await
        .map_err(|e: tokio_rusqlite::Error<_>| format!("Failed to check existing data: {e}"))?;

    if has_data {
        tracing::info!(target: "db", "SQLite already has data — skipping JSON migration");
        return Ok(());
    }

    let profiles_path = data_dir.join("profiles.json");
    let cache_path = data_dir.join("cache.json");

    // ── Migrate profiles.json ─────────────────────────────────────────
    let profiles: Vec<crate::subscription::SubscriptionProfile> =
        match std::fs::read_to_string(&profiles_path) {
            Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
                tracing::warn!(target: "db", "Failed to parse profiles.json: {e} — starting fresh");
                Vec::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(target: "db", "No profiles.json found — starting fresh");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(target: "db", "Failed to read profiles.json: {e} — starting fresh");
                Vec::new()
            }
        };

    if !profiles.is_empty() {
        tracing::info!(target: "db", "Migrating {} profile(s) from profiles.json", profiles.len());

        let profile_count = profiles.len() as i64;
        let server_count: i64 = profiles.iter().map(|p| p.servers.len() as i64).sum();

        db.call(move |conn| -> Result<(), rusqlite::Error> {
            let tx = conn.transaction()?;

            for profile in &profiles {
                tx.execute(
                    "INSERT OR REPLACE INTO profiles
                     (id, name, url, updated_at, last_tested,
                      traffic_used, traffic_total, traffic_expiry)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    tokio_rusqlite::params![
                        profile.id,
                        profile.name,
                        profile.url,
                        profile.updated_at,
                        profile.last_tested,
                        profile.traffic_used,
                        profile.traffic_total,
                        profile.traffic_expiry,
                    ],
                )?;

                for server in &profile.servers {
                    tx.execute(
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
                        tokio_rusqlite::params![
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
                    )?;
                }
            }

            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e: tokio_rusqlite::Error<_>| format!("Failed to migrate profiles: {e}"))?;

        tracing::info!(target: "db",
            "Migrated {profile_count} profile(s) with {server_count} server(s) from profiles.json"
        );
    }

    // ── Migrate cache.json ─────────────────────────────────────────────
    let cache_entries: std::collections::HashMap<String, crate::proxy::cache::CacheEntry> =
        match std::fs::read_to_string(&cache_path) {
            Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
                tracing::warn!(target: "db", "Failed to parse cache.json: {e}");
                std::collections::HashMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::collections::HashMap::new(),
            Err(e) => {
                tracing::warn!(target: "db", "Failed to read cache.json: {e}");
                std::collections::HashMap::new()
            }
        };

    if !cache_entries.is_empty() {
        let count = cache_entries.len() as i64;
        db.call(move |conn| -> Result<(), rusqlite::Error> {
            let tx = conn.transaction()?;
            for entry in cache_entries.values() {
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
                    crate::proxy::cache::CacheStatus::Positive {
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
                    crate::proxy::cache::CacheStatus::Negative {
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

                tx.execute(
                    "INSERT OR REPLACE INTO server_cache
                     (server_hash, status, ping_ms, exit_ip, exit_flag,
                      exit_country_name, exit_city_name, behind_cdn,
                      consecutive_successes, consecutive_failures,
                      previously_worked, first_failure_at, last_failure_at,
                      reason, tested_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                             ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    tokio_rusqlite::params![
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
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e: tokio_rusqlite::Error<_>| format!("Failed to migrate cache: {e}"))?;

        tracing::info!(target: "db", "Migrated {count} cache entries from cache.json");
    }

    // ── Cleanup old JSON files ────────────────────────────────────
    // SQLite migration is stable — delete old JSON files that are no longer needed.
    let profiles_path = data_dir.join("profiles.json");
    let cache_path = data_dir.join("cache.json");
    for json_path in [&profiles_path, &cache_path] {
        if json_path.exists() {
            match std::fs::remove_file(json_path) {
                Ok(_) => tracing::info!(target: "db", "Deleted old JSON file {:?}", json_path),
                Err(e) => {
                    tracing::warn!(target: "db", "Failed to delete old JSON file {:?}: {e}", json_path)
                }
            }
        }
    }

    Ok(())
}

/// Helper: read a `SubscriptionProfile` row from the current query row.
/// Used by multiple read paths.
#[allow(dead_code)]
pub(crate) fn row_to_subscription_profile(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<crate::subscription::SubscriptionProfile> {
    Ok(crate::subscription::SubscriptionProfile {
        id: row.get("id")?,
        name: row.get("name")?,
        url: row.get("url")?,
        servers: Vec::new(), // filled separately
        updated_at: row.get("updated_at")?,
        last_tested: row.get("last_tested")?,
        traffic_used: row.get("traffic_used")?,
        traffic_total: row.get("traffic_total")?,
        traffic_expiry: row.get("traffic_expiry")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::SCHEMA_VERSION;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_init_db_creates_tables() {
        let dir = TempDir::new().unwrap();
        let db = init_db(dir.path()).await.unwrap();

        // Verify tables exist by querying each one
        db.call(|conn| -> Result<(), rusqlite::Error> {
            // profiles
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0, "profiles table should exist and be empty");

            // servers
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM servers", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0, "servers table should exist and be empty");

            // server_cache
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM server_cache", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0, "server_cache table should exist and be empty");

            // test_history
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM test_history", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0, "test_history table should exist and be empty");

            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_init_db_idempotent() {
        let dir = TempDir::new().unwrap();
        // Call twice — should not error
        let _ = init_db(dir.path()).await.unwrap();
        let _ = init_db(dir.path()).await.unwrap();
        // If we reach here, it's idempotent
    }

    #[tokio::test]
    async fn test_init_db_wal_mode() {
        let dir = TempDir::new().unwrap();
        let db = init_db(dir.path()).await.unwrap();

        db.call(|conn| -> Result<(), rusqlite::Error> {
            let journal_mode: String = conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap_or_default();
            assert_eq!(
                journal_mode.to_lowercase(),
                "wal",
                "Expected WAL journal mode, got {journal_mode}"
            );
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_migrate_from_json_empty() {
        let dir = TempDir::new().unwrap();
        let db = init_db(dir.path()).await.unwrap();

        // No JSON files — should succeed without error
        migrate_from_json(&db, dir.path()).await.unwrap();

        // Verify no data
        db.call(|conn| -> Result<(), rusqlite::Error> {
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_schema_version_persisted() {
        let dir = TempDir::new().unwrap();
        let _ = init_db(dir.path()).await.unwrap();

        // Re-open and check version
        let db_path = dir.path().join("omnitun.db");
        let conn = tokio_rusqlite::Connection::open(&db_path).await.unwrap();
        conn.call(|inner| -> Result<(), rusqlite::Error> {
            let version: u32 = inner
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap_or(0);
            assert_eq!(version, SCHEMA_VERSION, "Expected schema v{SCHEMA_VERSION}");
            Ok(())
        })
        .await
        .unwrap();
    }
}
