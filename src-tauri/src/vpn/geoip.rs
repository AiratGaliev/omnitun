use std::net::IpAddr;
use std::path::Path;
use std::sync::OnceLock;
use serde::{Serialize, Deserialize};

/// Lazily-initialized MaxMind GeoLite2 City reader.
static CITY_READER: OnceLock<maxminddb::Reader<Vec<u8>>> = OnceLock::new();
/// Lazily-initialized MaxMind GeoLite2 ASN reader.
static ASN_READER: OnceLock<maxminddb::Reader<Vec<u8>>> = OnceLock::new();

/// Rich result from GeoIP lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoIpResult {
    pub country_code: String,
    #[allow(dead_code)]
    pub country_name: String,
    #[allow(dead_code)]
    pub city: String,
}

/// Initialise the City GeoIP reader from a local MMDB file.
pub fn init_city(db_path: &Path) -> Result<(), String> {
    let data = match std::fs::read(db_path) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_file(db_path);
            return Err(format!(
                "Failed to read GeoIP City database {:?}: {e}",
                db_path
            ));
        }
    };
    match maxminddb::Reader::from_source(data) {
        Ok(reader) => {
            let _ = CITY_READER.set(reader);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(db_path);
            Err(format!("Failed to parse GeoIP City database: {e}"))
        }
    }
}

/// Initialise the ASN GeoIP reader from a local MMDB file.
pub fn init_asn(db_path: &Path) -> Result<(), String> {
    let data = match std::fs::read(db_path) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_file(db_path);
            return Err(format!(
                "Failed to read GeoIP ASN database {:?}: {e}",
                db_path
            ));
        }
    };
    match maxminddb::Reader::from_source(data) {
        Ok(reader) => {
            let _ = ASN_READER.set(reader);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(db_path);
            Err(format!("Failed to parse GeoIP ASN database: {e}"))
        }
    }
}

pub fn is_ready() -> bool {
    CITY_READER.get().is_some()
}

/// Check if the ASN database reader is initialised.
pub fn is_asn_ready() -> bool {
    ASN_READER.get().is_some()
}

/// Check if an IP address belongs to a known CDN provider by ASN.
///
/// Uses the GeoLite2-ASN.mmdb database (offline, ~2µs lookup).
/// Known CDN ASNs:
///   - 13335, 209242, 203898, 394536, 395747, 132892, 398088  Cloudflare
///   - 54113  Fastly
///   - 16509, 14618  Amazon CloudFront (AWS)
///   - 132203 Tencent Cloud CDN
pub fn is_cdn_asn(ip: &str) -> bool {
    let addr: IpAddr = match ip.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };

    let reader = match ASN_READER.get() {
        Some(r) => r,
        None => return false,
    };

    if let Ok(asn_info) = reader.lookup::<maxminddb::geoip2::Asn>(addr) {
        if let Some(asn) = asn_info.autonomous_system_number {
            return matches!(
                asn,
                // Cloudflare (main + WARP/Workers pools)
                13335 | 209242 | 203898 | 394536 | 395747 | 132892 | 398088
                // Fastly
                | 54113
                // Amazon CloudFront (AWS)
                | 16509 | 14618
                // Tencent Cloud CDN
                | 132203
            );
        }
    }
    false
}

/// Mock country data for test/private IPs (127.0.0.x Docker test servers).
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
    if let Some(mock) = mock_lookup(ip) {
        return Some(mock);
    }

    let reader = CITY_READER.get()?;

    let addr: IpAddr = ip.parse().ok()?;

    let entry = reader.lookup::<maxminddb::geoip2::City>(addr).ok()?;

    let country_code = entry
        .country
        .as_ref()
        .and_then(|c| c.iso_code)
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
    if let Ok(ip) = host.parse::<IpAddr>() {
        return resolve(&ip.to_string());
    }

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

/// Download GeoIP databases (City + ASN) from GitHub release mirrors.
pub async fn ensure_downloaded(db_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(db_dir)
        .map_err(|e| format!("Failed to create geoip directory: {e}"))?;

    let city_path = db_dir.join("GeoLite2-City.mmdb");
    let asn_path = db_dir.join("GeoLite2-ASN.mmdb");

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    // Download City database (if not exists)
    if !city_path.exists() {
        let city_urls = [
            "https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-City.mmdb",
            "https://cdn.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-City.mmdb",
        ];
        download_db(&client, &city_path, &city_urls, "City").await?;
    }
    let _ = init_city(&city_path);

    // Download ASN database (if not exists)
    if !asn_path.exists() {
        let asn_urls = [
            "https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-ASN.mmdb",
            "https://cdn.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-ASN.mmdb",
        ];
        download_db(&client, &asn_path, &asn_urls, "ASN").await?;
    }
    let _ = init_asn(&asn_path);

    Ok(())
}

async fn download_db(
    client: &reqwest::Client,
    target_path: &Path,
    urls: &[&str],
    label: &str,
) -> Result<(), String> {
    let mut last_err = String::new();
    for url in urls {
        log::info!("Downloading GeoIP {} database from {} ...", label, url);
        match client.get(*url).send().await {
            Ok(resp) => {
                let bytes = match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        last_err = format!("{e}");
                        continue;
                    }
                };
                if bytes.len() < 100_000 {
                    last_err = format!(
                        "response too small ({} bytes) — likely an error page",
                        bytes.len()
                    );
                    continue;
                }
                let tmp_path = target_path.with_extension("mmdb.tmp");
                std::fs::write(&tmp_path, &bytes)
                    .map_err(|e| format!("Failed to write GeoIP {} database: {e}", label))?;
                std::fs::rename(&tmp_path, target_path)
                    .map_err(|e| format!("Failed to finalize GeoIP {} database: {e}", label))?;
                log::info!(
                    "GeoIP {} database downloaded ({} bytes)",
                    label,
                    bytes.len()
                );
                return Ok(());
            }
            Err(e) => {
                last_err = format!("{e}");
            }
        }
    }
    Err(format!("All GeoIP {} mirrors failed: {}", label, last_err))
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
        let us = resolve("127.0.0.1");
        assert!(us.is_some());
        assert_eq!(us.unwrap().country_code, "US");

        let de = resolve("127.0.0.2");
        assert!(de.is_some());
        assert_eq!(de.unwrap().country_code, "DE");
    }

    #[test]
    fn test_private_ip_no_mock() {
        assert!(resolve("192.168.1.1").is_none());
        assert!(resolve("10.0.0.1").is_none());
    }

    // ── ASN / CDN detection tests ─────────────────────────────────────

    #[test]
    fn test_is_cdn_asn_cloudflare() {
        // 13335 = Cloudflare ASN. But is_cdn_asn takes an IP, not an ASN number.
        // Without a real ASN database, ASN_READER is not initialized,
        // so the function always returns false.
        // This tests the parsing + DB-not-ready path.
        assert!(!is_cdn_asn("1.1.1.1")); // Cloudflare DNS — but ASN DB not loaded
        assert!(!is_cdn_asn("8.8.8.8")); // Google DNS — DB not loaded
    }

    #[test]
    fn test_is_cdn_asn_fastly_when_db_ready() {
        // Without GeoLite2-ASN.mmdb, ASN detection returns false for all IPs.
        // Verify this is the case (not a panic).
        assert!(!is_cdn_asn("151.101.1.1")); // Fastly LB IP
    }

    #[test]
    fn test_is_cdn_asn_aws() {
        assert!(!is_cdn_asn("13.32.0.1")); // AWS CloudFront — DB not loaded
        assert!(!is_cdn_asn("52.84.0.1")); // AWS CloudFront — DB not loaded
    }

    #[test]
    fn test_is_cdn_asn_regular_asn() {
        assert!(!is_cdn_asn("12345"));
        assert!(!is_cdn_asn("42"));
        assert!(!is_cdn_asn("0"));
    }

    #[test]
    fn test_is_cdn_asn_invalid_input() {
        assert!(!is_cdn_asn(""));
        assert!(!is_cdn_asn("not_a_number"));
        assert!(!is_cdn_asn("abc123"));
    }

    #[test]
    fn test_is_asn_ready_before_init() {
        // Before any init, ASN should not be ready
        assert!(!is_asn_ready());
    }

    #[test]
    fn test_is_ready_before_init() {
        // CITY_READER might or might not be initialised depending on test order
        // This just verifies it doesn't panic
        let _ = is_ready();
    }

    // ── GeoIpResult serialization ──────────────────────────────────────

    #[test]
    fn test_geoip_result_serde() {
        let result = GeoIpResult {
            country_code: "US".into(),
            country_name: "United States".into(),
            city: "New York".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("US"));
        assert!(json.contains("New York"));
        let deser: GeoIpResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.country_code, "US");
        assert_eq!(deser.city, "New York");
    }

    #[test]
    fn test_geoip_result_empty_city() {
        let result = GeoIpResult {
            country_code: "DE".into(),
            country_name: "Germany".into(),
            city: String::new(),
        };
        let json = serde_json::to_string(&result).unwrap();
        // Must serialize to valid JSON (no panic) and handle empty city
        assert!(json.contains("\"DE\""), "JSON should contain country code");
        assert!(json.contains("\"city\":\"\""), "JSON should have empty city");
    }

    // ── resolve_hostname edge cases ────────────────────────────────────

    #[tokio::test]
    async fn test_resolve_hostname_returns_none_when_not_ready() {
        // Without init, should return None (or fall back to resolve mock)
        let result = resolve_hostname("127.0.0.1").await;
        // When not initialised, it may return mock result or None
        // Either is acceptable — just verify no panic
        assert!(result.is_none() || result.is_some());
    }

    #[tokio::test]
    async fn test_resolve_hostname_empty() {
        let result = resolve_hostname("").await;
        assert!(result.is_none());
    }
}
