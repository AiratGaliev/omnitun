//! SQLite schema migrations — embedded as `const &str` so they never get
//! lost during Tauri bundling.
//!
//! Versioned via `PRAGMA user_version`. Each migration is idempotent
//! (uses `CREATE TABLE IF NOT EXISTS` / `ALTER TABLE ... IF NOT EXISTS`
//! where supported).

/// Current schema version.
#[allow(dead_code)]
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Schema version 1 — initial tables.
pub(crate) const CREATE_TABLES_SQL: &str = "
    PRAGMA journal_mode=WAL;
    PRAGMA synchronous=NORMAL;
    PRAGMA busy_timeout=5000;
    PRAGMA foreign_keys=ON;

    CREATE TABLE IF NOT EXISTS profiles (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        url TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now')),
        last_tested TEXT,
        traffic_used INTEGER,
        traffic_total INTEGER,
        traffic_expiry TEXT,
        refresh_state TEXT NOT NULL DEFAULT 'idle'
            CHECK(refresh_state IN ('idle', 'refreshing', 'complete'))
    );

    CREATE TABLE IF NOT EXISTS servers (
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
        ping_ms INTEGER,
        behind_cdn INTEGER DEFAULT 0,
        flag TEXT,
        country_name TEXT,
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

    CREATE INDEX IF NOT EXISTS idx_servers_profile_ping
        ON servers(profile_id, ping_ms);
    CREATE INDEX IF NOT EXISTS idx_servers_exit_flag
        ON servers(exit_flag) WHERE exit_flag IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_servers_protocol
        ON servers(protocol);

    CREATE TABLE IF NOT EXISTS server_cache (
        server_hash TEXT PRIMARY KEY,
        status TEXT NOT NULL CHECK(status IN ('positive', 'negative')),
        ping_ms INTEGER,
        exit_ip TEXT,
        exit_flag TEXT,
        exit_country_name TEXT,
        exit_city_name TEXT,
        behind_cdn INTEGER DEFAULT 0,
        consecutive_successes INTEGER DEFAULT 0,
        consecutive_failures INTEGER DEFAULT 0,
        previously_worked INTEGER DEFAULT 0,
        first_failure_at INTEGER,
        last_failure_at INTEGER,
        reason TEXT,
        tested_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS test_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        server_id TEXT NOT NULL,
        tested_at INTEGER NOT NULL,
        ping_ms INTEGER,
        exit_ip TEXT,
        success INTEGER NOT NULL,
        error TEXT,
        phase TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_history_server
        ON test_history(server_id, tested_at);
    CREATE INDEX IF NOT EXISTS idx_history_cleanup
        ON test_history(tested_at);
";
