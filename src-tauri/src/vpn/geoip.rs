use std::net::IpAddr;
use std::path::Path;
use std::sync::OnceLock;

/// Lazily-initialized MaxMind GeoLite2 City reader.
static READER: OnceLock<maxminddb::Reader<Vec<u8>>> = OnceLock::new();

/// Rich result from GeoIP lookup.
#[derive(Debug, Clone)]
pub struct GeoIpResult {
    pub country_code: String,
    #[allow(dead_code)]
    pub country_name: String,
    #[allow(dead_code)]
    pub city: String,
}

/// Initialise the GeoIP reader from a local City MMDB file.
pub fn init(db_path: &Path) -> Result<(), String> {
    // If the file exists but is corrupt, remove it so it gets re-downloaded later
    let data = match std::fs::read(db_path) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_file(db_path);
            return Err(format!("Failed to read GeoIP database {:?}: {e}", db_path));
        }
    };
    match maxminddb::Reader::from_source(data) {
        Ok(reader) => {
            let _ = READER.set(reader);
            Ok(())
        }
        Err(e) => {
            // Corrupt file — delete so ensure_downloaded retries
            let _ = std::fs::remove_file(db_path);
            Err(format!("Failed to parse GeoIP database: {e}"))
        }
    }
}

pub fn is_ready() -> bool {
    READER.get().is_some()
}

/// Mock country data for test/private IPs (127.0.0.x Docker test servers).
/// These IPs are not in the real GeoIP database, but we need them for local testing.
fn mock_lookup(ip: &str) -> Option<GeoIpResult> {
    match ip {
        "127.0.0.1" => Some(GeoIpResult {
            country_code: "US".into(),
            country_name: "United States".into(),
            city: "Test City".into(),
        }),
        "127.0.0.2" => Some(GeoIpResult {
            country_code: "DE".into(),
            country_name: "Germany".into(),
            city: "Frankfurt".into(),
        }),
        "127.0.0.3" => Some(GeoIpResult {
            country_code: "JP".into(),
            country_name: "Japan".into(),
            city: "Tokyo".into(),
        }),
        "127.0.0.4" => Some(GeoIpResult {
            country_code: "AU".into(),
            country_name: "Australia".into(),
            city: "Sydney".into(),
        }),
        "127.0.0.5" => Some(GeoIpResult {
            country_code: "GB".into(),
            country_name: "United Kingdom".into(),
            city: "London".into(),
        }),
        _ => None,
    }
}

/// Resolve GeoIP info for the given IP address.
/// Returns None when the reader isn't ready, the input is a hostname, or the IP isn't in the DB.
pub fn resolve(ip: &str) -> Option<GeoIpResult> {
    // 1. Check mock entries for test/private IPs first
    if let Some(mock) = mock_lookup(ip) {
        return Some(mock);
    }

    // 2. Check if reader is ready
    let reader = READER.get()?;

    // 3. Parse IP
    let addr: IpAddr = ip.parse().ok()?;

    let entry = reader.lookup::<maxminddb::geoip2::City>(addr).ok()?;

    let country_code = entry
        .country
        .as_ref()
        .and_then(|c| c.iso_code.as_deref())
        .unwrap_or("")
        .to_string();

    let country_name = entry
        .country
        .as_ref()
        .and_then(|c| c.names.as_ref())
        .and_then(|n| n.get("en").map(|s| s.to_string()))
        .unwrap_or_default();

    let city = entry
        .city
        .as_ref()
        .and_then(|c| c.names.as_ref())
        .and_then(|n| n.get("en").map(|s| s.to_string()))
        .unwrap_or_default();

    if country_code.is_empty() {
        return None;
    }

    Some(GeoIpResult {
        country_code,
        country_name,
        city,
    })
}

/// Try to resolve a hostname to an IP address, then do GeoIP lookup.
pub async fn resolve_hostname(host: &str) -> Option<GeoIpResult> {
    // Try parsing as IP first
    if let Ok(ip) = host.parse::<IpAddr>() {
        return resolve(&ip.to_string());
    }

    // Try DNS resolution
    let addr_str = format!("{host}:0");
    let result = tokio::net::lookup_host(&addr_str).await;
    match result {
        Ok(mut addrs) => {
            if let Some(addr) = addrs.next() {
                resolve(&addr.ip().to_string())
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// Download and initialise the City database.
pub async fn ensure_downloaded(db_dir: &Path) -> Result<(), String> {
    let db_path = db_dir.join("GeoLite2-City.mmdb");

    if db_path.exists() {
        return init(&db_path);
    }

    // Try multiple mirrors — use GitHub release assets (not raw files) which work in more regions
    let urls = [
        "https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-City.mmdb",
        "https://cdn.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-City.mmdb",
    ];

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let mut last_err = String::new();
    for url in &urls {
        log::info!("Downloading GeoIP City database from {} ...", url);
        match client.get(*url).send().await {
            Ok(resp) => {
                let bytes = match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => { last_err = format!("{e}"); continue; }
                };
                if bytes.len() < 100_000 {
                    last_err = format!("response too small ({} bytes) — likely an error page", bytes.len());
                    continue;
                }
                std::fs::create_dir_all(db_dir)
                    .map_err(|e| format!("Failed to create geoip directory: {e}"))?;
                let tmp_path = db_dir.join("GeoLite2-City.mmdb.tmp");
                std::fs::write(&tmp_path, &bytes)
                    .map_err(|e| format!("Failed to write GeoIP database: {e}"))?;
                std::fs::rename(&tmp_path, &db_path)
                    .map_err(|e| format!("Failed to finalize GeoIP database: {e}"))?;
                log::info!("GeoIP City database downloaded ({} bytes)", bytes.len());
                return init(&db_path);
            }
            Err(e) => { last_err = format!("{e}"); }
        }
    }
    Err(format!("All GeoIP mirrors failed: {last_err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unknown_hostname() {
        assert!(resolve("fta.fontanvpn.com").is_none());
    }

    #[test]
    fn test_not_initialised_returns_none() {
        assert!(resolve("81.255.155.10").is_none());
    }

    #[test]
    fn test_empty_string() {
        assert!(resolve("").is_none());
    }

    #[test]
    fn test_mock_ip() {
        // Mock entries for test IPs
        let us = resolve("127.0.0.1");
        assert!(us.is_some());
        assert_eq!(us.unwrap().country_code, "US");

        let de = resolve("127.0.0.2");
        assert!(de.is_some());
        assert_eq!(de.unwrap().country_code, "DE");
    }

    #[test]
    fn test_private_ip_no_mock() {
        // Private IP without mock entry
        assert!(resolve("192.168.1.1").is_none());
        assert!(resolve("10.0.0.1").is_none());
    }
}
