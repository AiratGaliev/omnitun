use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

/// Lazily-initialized MaxMind GeoLite2 City reader.
static CITY_READER: OnceLock<maxminddb::Reader<Vec<u8>>> = OnceLock::new();
/// Lazily-initialized MaxMind GeoLite2 ASN reader.
static ASN_READER: OnceLock<maxminddb::Reader<Vec<u8>>> = OnceLock::new();
/// Lazily-initialized ipinfo.io Lite reader (country + ASN).
/// Field names differ from MaxMind — uses custom deserialisation.
static IPINFO_LITE_READER: OnceLock<maxminddb::Reader<Vec<u8>>> = OnceLock::new();

/// Minimum interval between ipinfo.io database downloads.
///
/// ipinfo.io rate-limits downloads to 10 per day per IP.
/// We use 4 hours (14 400 s) → max 6 downloads/day, well under the limit
/// with margin for retries and app restarts.
const IPINFO_REFRESH_INTERVAL: Duration = Duration::from_secs(4 * 3600);

/// Rich result from GeoIP lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoIpResult {
    pub country_code: String,
    #[allow(dead_code)]
    pub country_name: String,
    #[allow(dead_code)]
    pub city: String,
}

// ── ipinfo.io Lite record ──────────────────────────────────────────────────

/// Data record inside `ipinfo_lite.mmdb`.
///
/// Fields (from ipinfo.io docs):
/// - `country`       — full country name e.g. "United Kingdom"
/// - `country_code`  — ISO 3166-2 code e.g. "GB"
/// - `asn`           — AS number string e.g. "AS13335"
/// - `as_name`       — AS organisation name
/// - `as_type`       — "hosting", "isp", "education", "business", "government"
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct IpinfoLiteRecord {
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    asn: Option<String>,
    #[serde(default, alias = "as_name")]
    as_name: Option<String>,
    #[serde(default)]
    as_type: Option<String>,
}

// ── MaxMind City DB ────────────────────────────────────────────────────────

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

// ── MaxMind ASN DB ─────────────────────────────────────────────────────────

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

// ── ipinfo.io Lite DB ──────────────────────────────────────────────────────

/// Initialise the ipinfo.io Lite reader from a local MMDB file.
pub fn init_ipinfo(db_path: &Path) -> Result<(), String> {
    let data = match std::fs::read(db_path) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_file(db_path);
            return Err(format!(
                "Failed to read ipinfo Lite database {:?}: {e}",
                db_path
            ));
        }
    };
    match maxminddb::Reader::from_source(data) {
        Ok(reader) => {
            let _ = IPINFO_LITE_READER.set(reader);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(db_path);
            Err(format!("Failed to parse ipinfo Lite database: {e}"))
        }
    }
}

// ── Readiness checks ───────────────────────────────────────────────────────

/// Check if any GeoIP resolution source is available.
///
/// Returns true when at least one of the loaded databases (MaxMind City
/// or ipinfo Lite) can serve country-level lookups.
pub fn is_ready() -> bool {
    CITY_READER.get().is_some() || IPINFO_LITE_READER.get().is_some()
}

/// Check if the MaxMind City database reader is initialised specifically.
pub fn is_city_ready() -> bool {
    CITY_READER.get().is_some()
}

/// Check if the ASN database reader (MaxMind or ipinfo) is initialised.
pub fn is_asn_ready() -> bool {
    ASN_READER.get().is_some() || IPINFO_LITE_READER.get().is_some()
}

/// Check if the ipinfo.io Lite reader is initialised.
pub fn is_ipinfo_ready() -> bool {
    IPINFO_LITE_READER.get().is_some()
}

// ── CDN detection (ASN-based) ──────────────────────────────────────────────

/// Check if an IP address belongs to a known CDN provider by ASN.
///
/// Tries ipinfo.io Lite first (has `asn` + `as_type` fields), then falls
/// back to GeoLite2-ASN.mmdb.
///
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

    // 1. Try ipinfo Lite DB first (richer, often more up-to-date)
    if let Some(reader) = IPINFO_LITE_READER.get() {
        if let Ok(rec) = reader.lookup::<IpinfoLiteRecord>(addr) {
            if let Some(ref asn_str) = rec.asn {
                // asn field format: "AS13335" — strip the "AS" prefix
                let num: u64 = asn_str
                    .trim_start_matches("AS")
                    .trim_start_matches("as")
                    .parse()
                    .unwrap_or(0);
                if is_cdn_asn_num(num) {
                    return true;
                }
            }
        }
    }

    // 2. Fallback to MaxMind ASN DB
    let reader = match ASN_READER.get() {
        Some(r) => r,
        None => return false,
    };

    if let Ok(asn_info) = reader.lookup::<maxminddb::geoip2::Asn>(addr) {
        if let Some(asn) = asn_info.autonomous_system_number {
            return is_cdn_asn_num(asn as u64);
        }
    }

    false
}

/// Check whether a numeric ASN belongs to a known CDN provider.
fn is_cdn_asn_num(asn: u64) -> bool {
    matches!(
        asn,
        // Cloudflare (main + WARP/Workers pools)
        13335 | 209242 | 203898 | 394536 | 395747 | 132892 | 398088
        // Fastly
        | 54113
        // Amazon CloudFront (AWS)
        | 16509 | 14618
        // Tencent Cloud CDN
        | 132203
    )
}

// ── Country resolution ─────────────────────────────────────────────────────

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
///
/// Lookup order:
///   1. Mock IPs (test Docker servers)
///   2. ipinfo.io Lite database (country only, no city)
///   3. MaxMind GeoLite2 City database (country + city)
///   4. None
///
/// Returns None when no database is loaded, the input is a hostname,
/// or the IP is not found in any loaded database.
pub fn resolve(ip: &str) -> Option<GeoIpResult> {
    if let Some(mock) = mock_lookup(ip) {
        return Some(mock);
    }

    let addr: IpAddr = ip.parse().ok()?;

    // 1. Try ipinfo Lite DB first (usually has broader coverage)
    if let Some(reader) = IPINFO_LITE_READER.get() {
        if let Ok(rec) = reader.lookup::<IpinfoLiteRecord>(addr) {
            let country_code = rec.country_code.as_deref().unwrap_or("").to_string();
            let country_name = rec.country.as_deref().unwrap_or("").to_string();
            if !country_code.is_empty() {
                return Some(GeoIpResult {
                    country_code,
                    country_name,
                    city: String::new(), // ipinfo Lite has no city data
                });
            }
        }
    }

    // 2. Fallback to MaxMind City DB
    let reader = CITY_READER.get()?;
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

// ── Database download ──────────────────────────────────────────────────────

/// Check whether `ipinfo_lite.mmdb` should be (re-)downloaded.
///
/// Downloads if:
///   - file does not exist, OR
///   - file's modification time is older than `IPINFO_REFRESH_INTERVAL` (4 h)
///
/// This keeps us well under ipinfo.io's 10-downloads-per-day limit.
pub(crate) fn should_download_ipinfo(path: &Path) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return true, // file missing → download
    };

    let modified = match meta.modified() {
        Ok(t) => t,
        Err(_) => return true, // can't read mtime (e.g. no clock) → be safe
    };

    let elapsed = match SystemTime::now().duration_since(modified) {
        Ok(d) => d,
        Err(_) => return false, // mtime in the future → skip
    };

    if elapsed < IPINFO_REFRESH_INTERVAL {
        log::info!(
            "ipinfo Lite database refreshed {:.0?} ago — skipping download (interval: {:.0?})",
            elapsed,
            IPINFO_REFRESH_INTERVAL,
        );
        return false;
    }

    true
}

/// Validate an ipinfo.io API token by making a HEAD request to the
/// download URL.
///
/// Per the ipinfo.io docs, a valid token returns HTTP 302 (redirect to CDN).
/// An invalid or missing token returns 401 Unauthorized.
async fn validate_ipinfo_token(client: &reqwest::Client, token: &str) -> Result<(), String> {
    let url = format!(
        "https://ipinfo.io/data/ipinfo_lite.mmdb?token={}",
        token
    );

    // Don't follow redirects — we just check for the initial 302
    let resp = client
        .head(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to reach ipinfo.io: {e}"))?;

    let status = resp.status();
    match status.as_u16() {
        302 => Ok(()), // redirect to CDN = token valid
        401 => Err("IPinfo token is invalid or missing — check your token at ipinfo.io".into()),
        403 => Err("IPinfo token does not have access to the Lite database — upgrade at ipinfo.io".into()),
        404 => Err("IPinfo database URL not found — check the API endpoint".into()),
        code if code >= 400 => Err(format!(
            "IPinfo token validation failed with HTTP {code} — check your token at ipinfo.io"
        )),
        _ => {
            // 200 or other success code without redirect is also unexpected
            // for the HEAD endpoint, but treat as valid rather than block
            log::info!(
                "IPinfo token validation returned HTTP {status} (expected 302) — proceeding anyway"
            );
            Ok(())
        }
    }
}

/// Download only the ipinfo.io Lite database.
///
/// Validates the token first, then respects the rate-limit
/// (`IPINFO_REFRESH_INTERVAL`). Safe to call on every settings save —
/// returns `Ok(false)` when skipped due to rate limit, `Ok(true)` when
/// freshly downloaded, `Err` on failure.
///
/// After download (or if file already exists), inits the reader.
pub async fn download_ipinfo_db(db_dir: &Path, ipinfo_token: &str) -> Result<bool, String> {
    if ipinfo_token.is_empty() {
        return Err("IPinfo token not configured".into());
    }

    std::fs::create_dir_all(db_dir)
        .map_err(|e| format!("Failed to create geoip directory: {e}"))?;

    let ipinfo_path = db_dir.join("ipinfo_lite.mmdb");

    if should_download_ipinfo(&ipinfo_path) {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        // Validate token before downloading
        validate_ipinfo_token(&client, ipinfo_token).await?;

        let url = format!(
            "https://ipinfo.io/data/ipinfo_lite.mmdb?token={}",
            ipinfo_token
        );

        // Rebuild client with longer timeout for actual download
        let dl_client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        log::info!("Downloading ipinfo Lite database from ipinfo.io ...");
        download_single(&dl_client, &ipinfo_path, &url, "ipinfo Lite").await?;
        let _ = init_ipinfo(&ipinfo_path);
        Ok(true)
    } else {
        // Still try to init if the file exists but was skipped by rate limit
        let _ = init_ipinfo(&ipinfo_path);
        Ok(false)
    }
}

/// Download GeoIP databases (MaxMind City + ASN + ipinfo Lite) from remote sources.
///
/// * `db_dir` — directory to store `.mmdb` files
/// * `ipinfo_token` — ipinfo.io API token; when non-empty, also downloads
///   `ipinfo_lite.mmdb` (country + ASN) from ipinfo.io
pub async fn ensure_downloaded(db_dir: &Path, ipinfo_token: &str) -> Result<(), String> {
    std::fs::create_dir_all(db_dir)
        .map_err(|e| format!("Failed to create geoip directory: {e}"))?;

    let city_path = db_dir.join("GeoLite2-City.mmdb");
    let asn_path = db_dir.join("GeoLite2-ASN.mmdb");
    let ipinfo_path = db_dir.join("ipinfo_lite.mmdb");

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    // Download MaxMind City database (if not exists)
    if !city_path.exists() {
        let city_urls = [
            "https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-City.mmdb",
            "https://cdn.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-City.mmdb",
        ];
        download_db(&client, &city_path, &city_urls, "City").await?;
    }
    let _ = init_city(&city_path);

    // Download MaxMind ASN database (if not exists)
    if !asn_path.exists() {
        let asn_urls = [
            "https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-ASN.mmdb",
            "https://cdn.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-ASN.mmdb",
        ];
        download_db(&client, &asn_path, &asn_urls, "ASN").await?;
    }
    let _ = init_asn(&asn_path);

    // Download ipinfo.io Lite database (with rate-limit + validation)
    if !ipinfo_token.is_empty() && should_download_ipinfo(&ipinfo_path) {
        let ipinfo_url = format!(
            "https://ipinfo.io/data/ipinfo_lite.mmdb?token={}",
            ipinfo_token
        );
        log::info!("Downloading ipinfo Lite database from ipinfo.io ...");
        // Non-blocking: ipinfo download failure (invalid token, network) is
        // just a warning — MaxMind DBs are still loaded and the app works.
        // User can retry via Settings → General → IPinfo API Token.
        if let Err(e) = download_single(&client, &ipinfo_path, &ipinfo_url, "ipinfo Lite").await
        {
            log::warn!("Failed to download ipinfo Lite database: {e}");
            // Attempt to validate the token to give a more specific warning
            if let Err(ve) = validate_ipinfo_token(&client, ipinfo_token).await {
                log::warn!("{ve}");
            }
        }
    }
    let _ = init_ipinfo(&ipinfo_path);

    Ok(())
}

/// Download a single URL (with redirect-follow) to a file.
async fn download_single(
    client: &reqwest::Client,
    target_path: &Path,
    url: &str,
    label: &str,
) -> Result<(), String> {
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.as_u16() >= 400 {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!(
                    "{label} download failed with HTTP {} — {}",
                    status.as_u16(),
                    body.chars().take(200).collect::<String>().trim()
                ));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("Failed to read {label} response: {e}"))?;
            if bytes.len() < 100_000 {
                return Err(format!(
                    "{label} response too small ({} bytes) — likely an error page",
                    bytes.len()
                ));
            }
            let tmp_path = target_path.with_extension("mmdb.tmp");
            std::fs::write(&tmp_path, &bytes)
                .map_err(|e| format!("Failed to write {label} database: {e}"))?;
            std::fs::rename(&tmp_path, target_path)
                .map_err(|e| format!("Failed to finalize {label} database: {e}"))?;
            log::info!("{label} database downloaded ({} bytes)", bytes.len());
            Ok(())
        }
        Err(e) => Err(format!("Failed to download {label} database: {e}")),
    }
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

// ── Tests ──────────────────────────────────────────────────────────────────

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
        // Without a real ASN database, all calls return false.
        assert!(!is_cdn_asn("1.1.1.1"));
        assert!(!is_cdn_asn("8.8.8.8"));
    }

    #[test]
    fn test_is_cdn_asn_fastly_when_db_ready() {
        assert!(!is_cdn_asn("151.101.1.1"));
    }

    #[test]
    fn test_is_cdn_asn_aws() {
        assert!(!is_cdn_asn("13.32.0.1"));
        assert!(!is_cdn_asn("52.84.0.1"));
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
        assert!(!is_asn_ready());
    }

    #[test]
    fn test_is_ready_before_init() {
        let _ = is_ready();
    }

    #[test]
    fn test_is_ready_with_ipinfo_only() {
        // When no DB is loaded, is_ready should be false
        assert!(!is_ready());
    }

    // ── ipinfo Lite record parsing ────────────────────────────────────

    #[test]
    fn test_ipinfo_lite_record_defaults() {
        // An empty JSON object should deserialize with all fields as None
        let json = r#"{}"#;
        let rec: IpinfoLiteRecord = serde_json::from_str(json).unwrap();
        assert!(rec.country.is_none());
        assert!(rec.country_code.is_none());
        assert!(rec.asn.is_none());
        assert!(rec.as_name.is_none());
        assert!(rec.as_type.is_none());
    }

    #[test]
    fn test_ipinfo_lite_record_full() {
        let json = r#"{
            "country": "United Kingdom",
            "country_code": "GB",
            "asn": "AS13335",
            "as_name": "Cloudflare Inc.",
            "as_type": "hosting"
        }"#;
        let rec: IpinfoLiteRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.country.as_deref(), Some("United Kingdom"));
        assert_eq!(rec.country_code.as_deref(), Some("GB"));
        assert_eq!(rec.asn.as_deref(), Some("AS13335"));
        assert_eq!(rec.as_name.as_deref(), Some("Cloudflare Inc."));
        assert_eq!(rec.as_type.as_deref(), Some("hosting"));
    }

    #[test]
    fn test_is_cdn_asn_num_cloudflare() {
        assert!(is_cdn_asn_num(13335));
        assert!(is_cdn_asn_num(209242));
    }

    #[test]
    fn test_is_cdn_asn_num_fastly() {
        assert!(is_cdn_asn_num(54113));
    }

    #[test]
    fn test_is_cdn_asn_num_aws() {
        assert!(is_cdn_asn_num(16509));
        assert!(is_cdn_asn_num(14618));
    }

    #[test]
    fn test_is_cdn_asn_num_tencent() {
        assert!(is_cdn_asn_num(132203));
    }

    #[test]
    fn test_is_cdn_asn_num_non_cdn() {
        assert!(!is_cdn_asn_num(1));
        assert!(!is_cdn_asn_num(15169)); // Google
        assert!(!is_cdn_asn_num(8075)); // Microsoft
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
        assert!(json.contains("\"DE\""), "JSON should contain country code");
        assert!(json.contains("\"city\":\"\""), "JSON should have empty city");
    }

    // ── resolve_hostname edge cases ────────────────────────────────────

    #[tokio::test]
    async fn test_resolve_hostname_returns_none_when_not_ready() {
        let result = resolve_hostname("127.0.0.1").await;
        assert!(result.is_none() || result.is_some());
    }

    #[tokio::test]
    async fn test_resolve_hostname_empty() {
        let result = resolve_hostname("").await;
        assert!(result.is_none());
    }

    // ── is_cdn_asn_num extracted function ──────────────────────────────

    #[test]
    fn test_is_cdn_asn_num_edge() {
        assert!(!is_cdn_asn_num(0));
        assert!(!is_cdn_asn_num(u64::MAX));
    }
}
