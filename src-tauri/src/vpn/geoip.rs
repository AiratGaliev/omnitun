use std::net::IpAddr;
use std::path::Path;
use std::sync::OnceLock;

/// Lazily-initialized MaxMind GeoLite2 reader (Country database).
/// Loaded once at app startup or on first profile refresh.
static READER: OnceLock<maxminddb::Reader<Vec<u8>>> = OnceLock::new();

/// Path to the MMDB file on disk — used by `init()` and `auto_download()`.
static DB_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Initialise the GeoIP reader from a local MMDB file.
/// If the file doesn't exist yet, call `ensure_downloaded()` first.
pub fn init(db_path: &Path) -> Result<(), String> {
    let _ = DB_PATH.set(db_path.to_path_buf());
    let data = std::fs::read(db_path)
        .map_err(|e| format!("Failed to read GeoIP database {:?}: {e}", db_path))?;
    let reader = maxminddb::Reader::from_source(data)
        .map_err(|e| format!("Failed to parse GeoIP database: {e}"))?;
    let _ = READER.set(reader);
    Ok(())
}

/// Returns true if the MMDB has been loaded and is usable.
pub fn is_ready() -> bool {
    READER.get().is_some()
}

/// Resolve a two-letter country code (ISO 3166-1 alpha-2) for the given IP.
///
/// Returns `"Unknown"` when:
/// - The reader hasn't been initialised yet
/// - The input is a hostname, not an IP
/// - The IP is not in the database (private / reserved ranges)
pub fn resolve(ip: &str) -> String {
    let reader = match READER.get() {
        Some(r) => r,
        None => return "Unknown".into(),
    };

    let addr: IpAddr = match ip.parse() {
        Ok(a) => a,
        Err(_) => return "Unknown".into(), // hostname, not an IP
    };

    match reader.lookup::<maxminddb::geoip2::Country>(addr) {
        Ok(entry) => entry
            .country
            .and_then(|c| c.iso_code)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Unknown".into()),
        Err(_) => "Unknown".into(),
    }
}

/// Try to initialise the database; if the file doesn't exist, download it.
pub async fn ensure_downloaded(db_dir: &Path) -> Result<(), String> {
    let db_path = db_dir.join("GeoLite2-Country.mmdb");

    // Check if already on disk
    if db_path.exists() {
        return init(&db_path);
    }

    // Download from P3TERX mirror
    let url = "https://github.com/P3TERX/GeoLite.mmdb/raw/main/GeoLite2-Country.mmdb";

    log::info!("Downloading GeoIP database from {url} ...");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to download GeoIP database: {e}"))?;

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read GeoIP database response: {e}"))?;

    // Ensure directory exists
    std::fs::create_dir_all(db_dir)
        .map_err(|e| format!("Failed to create geoip directory: {e}"))?;

    // Write atomically via temp file
    let tmp_path = db_dir.join("GeoLite2-Country.mmdb.tmp");
    std::fs::write(&tmp_path, &bytes)
        .map_err(|e| format!("Failed to write GeoIP database: {e}"))?;
    std::fs::rename(&tmp_path, &db_path)
        .map_err(|e| format!("Failed to finalize GeoIP database: {e}"))?;

    log::info!("GeoIP database downloaded ({} bytes)", bytes.len());

    init(&db_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unknown_hostname() {
        // Hostnames should return Unknown without trying to parse
        assert_eq!(resolve("fta.fontanvpn.com"), "Unknown");
    }

    #[test]
    fn test_not_initialised_returns_unknown() {
        // Before init() is called, everything is Unknown
        assert_eq!(resolve("81.255.155.10"), "Unknown");
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(resolve(""), "Unknown");
    }

    #[test]
    fn test_reserved_ip() {
        // Private/reserved IPs won't be in the DB
        assert_eq!(resolve("127.0.0.1"), "Unknown");
        assert_eq!(resolve("192.168.1.1"), "Unknown");
    }
}
