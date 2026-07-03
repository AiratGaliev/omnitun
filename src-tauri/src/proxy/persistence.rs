//! Persistent cache storage for `ServerCache`.
//!
//! Saves and loads the `HashMap<String, CacheEntry>` to/from a JSON file.
//! Uses atomic write (temp file + rename) so partial writes never corrupt
//! the cached data.
//!
//! This module persists the **source-of-truth** `HashMap`, NOT the moka hot
//! cache. The hot cache is reconstructed on demand.

use std::collections::HashMap;
use std::path::Path;

use crate::proxy::cache::CacheEntry;

/// File name for the persistent cache.
const CACHE_FILE: &str = "cache.json";

/// Load the cache from `geoip_meta.json`'s sibling directory.
/// Returns an empty map if the file does not exist or is corrupted.
pub fn load_cache(data_dir: &Path) -> HashMap<String, CacheEntry> {
    let path = data_dir.join(CACHE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(json) => match serde_json::from_str::<HashMap<String, CacheEntry>>(&json) {
            Ok(map) => {
                log::info!(
                    "[Cache Persistence] loaded {} entries from {}",
                    map.len(),
                    path.display()
                );
                map
            }
            Err(e) => {
                log::warn!(
                    "[Cache Persistence] failed to parse {}: {e} — starting fresh",
                    path.display()
                );
                HashMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::info!(
                "[Cache Persistence] no cache file at {} — starting fresh",
                path.display()
            );
            HashMap::new()
        }
        Err(e) => {
            log::warn!(
                "[Cache Persistence] error reading {}: {e} — starting fresh",
                path.display()
            );
            HashMap::new()
        }
    }
}

/// Save the cache to disk using atomic write (temp file + rename).
///
/// Writes to `<data_dir>/cache.json.tmp` first, then renames to
/// `<data_dir>/cache.json`. If the write fails, the original file is untouched.
pub fn save_cache(data_dir: &Path, entries: &HashMap<String, CacheEntry>) {
    let path = data_dir.join(CACHE_FILE);
    let tmp_path = data_dir.join("cache.json.tmp");

    match serde_json::to_string_pretty(entries) {
        Ok(json) => {
            match std::fs::write(&tmp_path, &json) {
                Ok(_) => {
                    match std::fs::rename(&tmp_path, &path) {
                        Ok(_) => {
                            log::info!(
                                "[Cache Persistence] saved {} entries to {}",
                                entries.len(),
                                path.display()
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "[Cache Persistence] atomic rename failed: {e} — cache NOT saved"
                            );
                            // Try to clean up the temp file
                            let _ = std::fs::remove_file(&tmp_path);
                        }
                    }
                }
                Err(e) => {
                    log::error!("[Cache Persistence] write failed: {e} — cache NOT saved");
                    let _ = std::fs::remove_file(&tmp_path);
                }
            }
        }
        Err(e) => {
            log::error!("[Cache Persistence] serialization failed: {e} — cache NOT saved");
        }
    }
}

/// Delete the cache file entirely (e.g., when all profiles are removed).
pub fn clear_cache(data_dir: &Path) {
    let path = data_dir.join(CACHE_FILE);
    match std::fs::remove_file(&path) {
        Ok(_) => log::info!("[Cache Persistence] deleted {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Already gone — nothing to do
        }
        Err(e) => {
            log::warn!(
                "[Cache Persistence] failed to delete {}: {e}",
                path.display()
            );
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::cache::{CacheEntry, CacheStatus};
    use std::path::PathBuf;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("omnitun-persistence-test-{}", name))
    }

    fn make_entry(server_id: &str, ping_ms: u64) -> (String, CacheEntry) {
        let entry = CacheEntry {
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
        };
        (server_id.to_string(), entry)
    }

    fn make_negative_entry(server_id: &str, reason: &str) -> (String, CacheEntry) {
        let entry = CacheEntry {
            server_id: server_id.to_string(),
            last_updated: 1_000_000,
            status: CacheStatus::Negative {
                first_failure_at: 1_000_000,
                last_failure_at: 1_000_000,
                consecutive_failures: 1,
                previously_worked: false,
                reason: reason.to_string(),
            },
        };
        (server_id.to_string(), entry)
    }

    #[test]
    fn test_load_missing_file_returns_empty() {
        let dir = test_dir("missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let map = load_cache(&dir);
        assert!(map.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = test_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut original = HashMap::new();
        let (k1, v1) = make_entry("server-1", 42);
        let (k2, v2) = make_entry("server-2", 105);
        let (k3, v3) = make_negative_entry("server-3", "Connection refused");
        original.insert(k1, v1);
        original.insert(k2, v2);
        original.insert(k3, v3);

        save_cache(&dir, &original);

        let loaded = load_cache(&dir);
        assert_eq!(loaded.len(), 3);

        let s1 = loaded.get("server-1").unwrap();
        assert_eq!(s1.server_id, "server-1");
        match &s1.status {
            CacheStatus::Positive { ping_ms, .. } => assert_eq!(*ping_ms, 42),
            _ => panic!("expected Positive"),
        }

        let s3 = loaded.get("server-3").unwrap();
        match &s3.status {
            CacheStatus::Negative { reason, .. } => assert_eq!(reason, "Connection refused"),
            _ => panic!("expected Negative"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_overwrites_existing() {
        let dir = test_dir("overwrite");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // First save with 2 entries
        let mut first = HashMap::new();
        let (k1, v1) = make_entry("s1", 10);
        let (k2, v2) = make_entry("s2", 20);
        first.insert(k1, v1);
        first.insert(k2, v2);
        save_cache(&dir, &first);
        assert_eq!(load_cache(&dir).len(), 2);

        // Overwrite with 1 entry
        let mut second = HashMap::new();
        let (k3, v3) = make_entry("s3", 30);
        second.insert(k3, v3);
        save_cache(&dir, &second);
        let loaded = load_cache(&dir);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("s3"));
        assert!(!loaded.contains_key("s1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_corrupted_file_returns_empty() {
        let dir = test_dir("corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("cache.json");
        std::fs::write(&path, "{this is not valid json").unwrap();

        let map = load_cache(&dir);
        assert!(map.is_empty(), "corrupted file should return empty map");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_clear_cache() {
        let dir = test_dir("clear");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut map = HashMap::new();
        let (key, val) = make_entry("s1", 5);
        map.insert(key, val);
        save_cache(&dir, &map);
        assert!(dir.join("cache.json").exists());

        clear_cache(&dir);
        assert!(!dir.join("cache.json").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_clear_cache_nonexistent_no_panic() {
        let dir = test_dir("clear-nonexistent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No cache.json exists — should not panic
        clear_cache(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_atomic_write_does_not_corrupt_on_failure() {
        // Simulate a failed write by making the data dir read-only after
        // writing the original.
        let dir = test_dir("atomic-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // First, save a valid cache
        let mut original = HashMap::new();
        let (k, v) = make_entry("keep-me", 99);
        original.insert(k, v);
        save_cache(&dir, &original);
        assert!(dir.join("cache.json").exists());

        // Now make the directory read-only
        let perms = std::fs::Permissions::from_mode(0o444);
        let _ = std::fs::set_permissions(&dir, perms);

        // Try to save — should fail gracefully
        let mut new = HashMap::new();
        let (k2, v2) = make_entry("new", 1);
        new.insert(k2, v2);
        save_cache(&dir, &new);

        // Original file should still exist (write was atomic)
        // and contain the old data
        let loaded = load_cache(&dir);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("keep-me"));

        // Restore permissions so cleanup works
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
