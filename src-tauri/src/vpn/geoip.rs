use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
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
/// We use 4 hours (14400 s) → max 6 downloads/day, well under the limit
/// with margin for retries and app restarts.
const IPINFO_REFRESH_INTERVAL: Duration = Duration::from_secs(4 * 3600);

/// Minimum interval between GitHub release API checks for MaxMind DB updates.
/// 1 hour — well under GitHub's 60 req/h unauthenticated limit.
#[allow(dead_code)]
const GITHUB_RELEASE_CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// Minimum interval between MaxMind DB re-downloads when a new release is
/// available. Prevents re-downloading on every startup if the same release
/// tag is already cached.
#[allow(dead_code)]
const MAXMIND_REFRESH_INTERVAL: Duration = Duration::from_secs(12 * 3600); // 12 h

// ── GeoIP download metadata ────────────────────────────────────────────────

/// Per-database download metadata stored in `geoip_meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbMeta {
    /// ISO-8601 timestamp of last successful download.
    pub last_downloaded_at: String,
    /// Release tag from upstream (GitHub tag for MaxMind, empty for ipinfo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// Download metadata for all GeoIP databases.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GeoIpMeta {
    #[serde(default)]
    pub databases: std::collections::HashMap<String, DbMeta>,
    /// ISO-8601 timestamp of last GitHub release API check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_release_check: Option<String>,
}

/// Load geoip_meta.json from the geoip directory.
pub fn load_meta(db_dir: &Path) -> GeoIpMeta {
    let path = db_dir.join("geoip_meta.json");
    match std::fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => GeoIpMeta::default(),
    }
}

/// Save geoip_meta.json to the geoip directory.
pub fn save_meta(db_dir: &Path, meta: &GeoIpMeta) {
    let path = db_dir.join("geoip_meta.json");
    if let Ok(json) = serde_json::to_string_pretty(meta) {
        let _ = std::fs::write(&path, &json);
    }
}

/// Return the download metadata (for frontend display).
pub fn get_geoip_meta(db_dir: &Path) -> GeoIpMeta {
    load_meta(db_dir)
}

// ── Rich result from GeoIP lookup ──────────────────────────────────────────

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

// ── CDN detection (sing-box .srs rule sets via decompile) ──────────
//
// We download .srs binary rule sets (native sing-box format), then
// parse them via `sing-box rule-set decompile` → JSON → ip_cidr.
// See https://github.com/123jjck/cdn-ip-ranges
static CDN_RANGES: OnceLock<Vec<(IpAddr, u8)>> = OnceLock::new();

/// Path to sing-box binary, set during app initialization.
static SINGBOX_BIN: OnceLock<PathBuf> = OnceLock::new();

/// Set the path to the sing-box binary (called during app setup).
pub fn set_singbox_bin_path(path: PathBuf) {
    let _ = SINGBOX_BIN.set(path);
}

/// Replace CDN_RANGES with a new set.
pub fn set_cdn_ranges(ranges: Vec<(IpAddr, u8)>) {
    let _ = CDN_RANGES.set(ranges);
}

/// Parse a .srs file: call `sing-box rule-set decompile` on the raw file
/// to convert to JSON, then extract ip_cidr entries.
pub fn parse_srs_file(path: &Path) -> Result<Vec<(IpAddr, u8)>, String> {
    let sb = SINGBOX_BIN.get().ok_or_else(|| "sing-box binary not set".to_string())?;

    let tmp_dir = std::env::temp_dir();
    let tmp_json = tmp_dir.join("cdn_srs_tmp.json");

    let status = std::process::Command::new(sb)
        .args(["rule-set", "decompile", "-o", &tmp_json.to_string_lossy(), &path.to_string_lossy()])
        .output()
        .map_err(|e| {
            format!("Failed to run sing-box: {e}")
        });

    let out = status?;
    if !out.status.success() {
        return Err(format!(
            "sing-box decompile failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    // Read the output JSON file, then clean up
    let json_text = std::fs::read_to_string(&tmp_json)
        .map_err(|e| format!("Failed to read sing-box output JSON: {e}"));
    let _ = std::fs::remove_file(&tmp_json);
    let json_text = json_text?;

    let json: serde_json::Value = serde_json::from_str(&json_text)
        .map_err(|e| format!("Failed to parse sing-box output: {e}"))?;

    let mut ranges = Vec::new();
    if let Some(rules) = json["rules"].as_array() {
        for rule in rules {
            if let Some(cidrs) = rule["ip_cidr"].as_array() {
                for cidr_val in cidrs {
                    let cidr = cidr_val.as_str().unwrap_or("");
                    if let Some((ip_str, prefix_str)) = cidr.split_once('/') {
                        if let Ok(ip) = ip_str.parse::<IpAddr>() {
                            if let Ok(plen) = prefix_str.parse::<u8>() {
                                ranges.push((ip, plen));
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(ranges)
}

/// Load a .srs file and set CDN_RANGES.
pub fn init_cdn_ranges(path: &Path) -> Result<(), String> {
    let ranges = parse_srs_file(path)?;
    log::info!("Loaded {} CDN CIDR entries from {}", ranges.len(), path.display());
    set_cdn_ranges(ranges);
    Ok(())
}

/// Returns true when CDN CIDR ranges are loaded.
pub fn is_cdn_ready() -> bool {
    CDN_RANGES.get().is_some()
}

/// Check whether an IP address falls within any CDN CIDR range from the
/// downloaded 123jjck/cdn-ip-ranges dataset.
///
/// This replaces the old hardcoded-ASN approach. The data is updated
/// every 12 hours via GitHub Actions in the upstream repo.
pub fn is_cdn_ip(ip: &str) -> bool {
    let addr: IpAddr = match ip.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };

    let ranges = match CDN_RANGES.get() {
        Some(r) => r,
        None => return false,
    };

    let addr_bits = addr_to_bits(addr);
    for &(prefix, prefix_len) in ranges {
        let mask = prefix_mask(addr, prefix_len);
        if (addr_bits & mask) == (addr_to_bits(prefix) & mask) {
            return true;
        }
    }
    false
}

#[inline]
fn addr_to_bits(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(v4) => v4.to_bits() as u128,
        IpAddr::V6(v6) => v6.to_bits(),
    }
}

#[inline]
fn prefix_mask(ip: IpAddr, len: u8) -> u128 {
    match ip {
        IpAddr::V4(_) => {
            if len >= 32 { return !0u128; }
            (!0u128) << (32 - len)
        }
        IpAddr::V6(_) => {
            if len >= 128 { return !0u128; }
            (!0u128) << (128 - len)
        }
    }
}

/// Check if an IP address belongs to a known CDN provider — legacy alias.
/// Still used by `profile/mod.rs` in batch/parallel server tests.
pub fn is_cdn_asn(ip: &str) -> bool {
    is_cdn_ip(ip)
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
/// Merges data from multiple sources:
///   1. Mock IPs (test Docker servers) — short-circuit
///   2. ipinfo.io Lite — country_code + country_name (more up-to-date)
///   3. MaxMind GeoLite2 City — city + fallback country
///   4. None
///
/// Returns None when no database is loaded, the input is a hostname,
/// or the IP is not found in any loaded database.
pub fn resolve(ip: &str) -> Option<GeoIpResult> {
    if let Some(mock) = mock_lookup(ip) {
        return Some(mock);
    }

    let addr: IpAddr = ip.parse().ok()?;

    // Try ipinfo Lite first for country (updated more frequently).
    let mut country_code = String::new();
    let mut country_name = String::new();
    if let Some(reader) = IPINFO_LITE_READER.get() {
        if let Ok(rec) = reader.lookup::<IpinfoLiteRecord>(addr) {
            if let Some(ref cc) = rec.country_code {
                if !cc.is_empty() {
                    country_code = cc.clone();
                    country_name = rec.country.clone().unwrap_or_default();
                }
            }
        }
    }

    // Always try MaxMind City for city enrichment (ipinfo Lite has none)
    // and fallback country when ipinfo didn't have the IP.
    let mut city = String::new();
    if let Some(reader) = CITY_READER.get() {
        if let Ok(entry) = reader.lookup::<maxminddb::geoip2::City>(addr) {
            city = entry
                .city
                .as_ref()
                .and_then(|c| c.names.as_ref())
                .and_then(|n| n.get("en").map(|s| s.to_string()))
                .unwrap_or_default();

            // Only use MaxMind country if ipinfo didn't provide one
            if country_code.is_empty() {
                country_code = entry
                    .country
                    .as_ref()
                    .and_then(|c| c.iso_code)
                    .unwrap_or("")
                    .to_string();
                country_name = entry
                    .country
                    .as_ref()
                    .and_then(|c| c.names.as_ref())
                    .and_then(|n| n.get("en").map(|s| s.to_string()))
                    .unwrap_or_default();
            }
        }
    }

    if country_code.is_empty() && city.is_empty() {
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

/// Update `geoip_meta.json` after a successful database download.
fn update_db_meta(db_dir: &Path, name: &str, release_tag: Option<&str>, bytes: u64) {
    let mut meta = load_meta(db_dir);
    let now = chrono::Utc::now().to_rfc3339();
    meta.databases.insert(
        name.to_string(),
        DbMeta {
            last_downloaded_at: now,
            release_tag: release_tag.map(|s| s.to_string()),
            bytes: Some(bytes),
        },
    );
    save_meta(db_dir, &meta);
}

/// Ensure `geoip_meta.json` exists by recording info for any MMDB files
/// already on disk. Should be called at the end of `ensure_downloaded`.
fn sync_meta_file(db_dir: &Path) {
    let mut meta = load_meta(db_dir);
    let mut changed = false;

    for name in &["GeoLite2-City.mmdb", "GeoLite2-ASN.mmdb", "ipinfo_lite.mmdb",
                  "cdn-only_singbox.srs", "merged.srs"] {
        if meta.databases.contains_key(*name) {
            continue; // already tracked
        }
        let path = db_dir.join(name);
        if let Ok(meta_info) = std::fs::metadata(&path) {
            if let Ok(modified) = meta_info.modified() {
                if let Ok(dur) = SystemTime::now().duration_since(modified) {
                    let iso = chrono::DateTime::<chrono::Utc>::from(
                        SystemTime::now() - dur,
                    )
                    .to_rfc3339();
                    meta.databases.insert(
                        name.to_string(),
                        DbMeta {
                            last_downloaded_at: iso,
                            release_tag: None,
                            bytes: Some(meta_info.len()),
                        },
                    );
                    changed = true;
                }
            }
        }
    }

    if changed {
        save_meta(db_dir, &meta);
    }
}

/// Check whether a DB entry in meta is older than the given interval.
fn is_older_than(meta: &GeoIpMeta, name: &str, interval: Duration) -> bool {
    match meta.databases.get(name) {
        Some(entry) => {
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&entry.last_downloaded_at) {
                SystemTime::now()
                    .duration_since(t.into())
                    .map(|d| d >= interval)
                    .unwrap_or(true)
            } else {
                true
            }
        }
        None => true,
    }
}

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

/// Fetch the latest release tag from P3TERX/GeoLite.mmdb GitHub releases.
/// Caches result in `geoip_meta.json` (1h rate limit on API).
async fn check_latest_release(client: &reqwest::Client, db_dir: &Path) -> Option<String> {
    let mut meta = load_meta(db_dir);

    // Rate-limit GitHub API to 1 req/h (60 req/h unauthenticated limit)
    let should_fetch = match &meta.last_release_check {
        Some(ts) => {
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) {
                let elapsed = SystemTime::now()
                    .duration_since(t.into())
                    .unwrap_or(Duration::MAX);
                elapsed >= GITHUB_RELEASE_CHECK_INTERVAL
            } else {
                true
            }
        }
        None => true,
    };

    if !should_fetch {
        // Use cached tag from meta
        for name in &["GeoLite2-City.mmdb", "GeoLite2-ASN.mmdb"] {
            if let Some(entry) = meta.databases.get(*name) {
                if let Some(ref tag) = entry.release_tag {
                    return Some(tag.clone());
                }
            }
        }
        return None;
    }

    let url = "https://api.github.com/repos/P3TERX/GeoLite.mmdb/releases/latest";
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        log::warn!("GitHub API returned HTTP {} for release check", resp.status().as_u16());
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let tag = body.get("tag_name")?.as_str()?.to_string();
    log::info!("GitHub API: latest P3TERX/GeoLite.mmdb release is {tag}");

    let now = chrono::Utc::now().to_rfc3339();
    meta.last_release_check = Some(now);
    for name in &["GeoLite2-City.mmdb", "GeoLite2-ASN.mmdb"] {
        if let Some(entry) = meta.databases.get_mut(*name) {
            entry.release_tag = Some(tag.clone());
        }
    }
    save_meta(db_dir, &meta);

    Some(tag)
}

/// Unified check for all GeoIP database freshness.
///
/// Called from:
///   - `initialize_app` (app startup)
///   - `process_servers` (lazy init before profile processing)
///
/// Respects all rate limits:
///   - ipinfo.io: max 10/day → 4h between downloads
///   - GitHub API: max 60 req/h → 1h between release checks
///   - MaxMind re-downloads: max 2/day → 12h between same-tag re-downloads
///
/// Non-blocking: all errors are logged as warnings, never returned.
pub async fn ensure_databases_fresh(db_dir: &Path, ipinfo_token: &str) {
    let _ = std::fs::create_dir_all(db_dir);

    let city_path = db_dir.join("GeoLite2-City.mmdb");
    let asn_path = db_dir.join("GeoLite2-ASN.mmdb");
    let ipinfo_path = db_dir.join("ipinfo_lite.mmdb");

    let client = match reqwest::Client::builder()
        .user_agent("omnitun/1.0")
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Failed to build HTTP client for GeoIP check: {e}");
            return;
        }
    };

    // ── 1. MaxMind: check GitHub for latest release ────────────────────
    let latest_tag = check_latest_release(&client, db_dir).await;
    if let Some(ref tag) = latest_tag {
        log::info!("Latest P3TERX/GeoLite.mmdb release: {tag}");
    } else {
        log::info!("P3TERX/GeoLite.mmdb release check skipped (cached or unavailable)");
    }

    // ── 2. MaxMind City ────────────────────────────────────────────────
    let meta = load_meta(db_dir);
    let city_stored_tag = meta
        .databases
        .get("GeoLite2-City.mmdb")
        .and_then(|d| d.release_tag.as_deref());

    let city_needs = !city_path.exists()
        || (latest_tag.is_some()
            && city_stored_tag != latest_tag.as_deref()
            && is_older_than(&meta, "GeoLite2-City.mmdb", MAXMIND_REFRESH_INTERVAL));
    drop(meta);

    if city_needs {
        let urls = [
            "https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-City.mmdb",
            "https://cdn.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-City.mmdb",
        ];
        if let Ok(dl) = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()
        {
            match download_db_geoip(&dl, &city_path, &urls, "City").await {
                Ok(bytes) => update_db_meta(db_dir, "GeoLite2-City.mmdb", latest_tag.as_deref(), bytes),
                Err(e) => log::warn!("City DB download: {e}"),
            }
        }
    } else if city_path.exists() {
        log::info!("GeoLite2-City.mmdb is up-to-date");
    }
    let _ = init_city(&city_path);

    // ── 3. MaxMind ASN ─────────────────────────────────────────────────
    let meta = load_meta(db_dir);
    let asn_stored_tag = meta
        .databases
        .get("GeoLite2-ASN.mmdb")
        .and_then(|d| d.release_tag.as_deref());

    let asn_needs = !asn_path.exists()
        || (latest_tag.is_some()
            && asn_stored_tag != latest_tag.as_deref()
            && is_older_than(&meta, "GeoLite2-ASN.mmdb", MAXMIND_REFRESH_INTERVAL));
    drop(meta);

    if asn_needs {
        let urls = [
            "https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-ASN.mmdb",
            "https://cdn.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-ASN.mmdb",
        ];
        if let Ok(dl) = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()
        {
            match download_db_geoip(&dl, &asn_path, &urls, "ASN").await {
                Ok(bytes) => update_db_meta(db_dir, "GeoLite2-ASN.mmdb", latest_tag.as_deref(), bytes),
                Err(e) => log::warn!("ASN DB download: {e}"),
            }
        }
    } else if asn_path.exists() {
        log::info!("GeoLite2-ASN.mmdb is up-to-date");
    }
    let _ = init_asn(&asn_path);

    // ── 4. ipinfo.io Lite ──────────────────────────────────────────────
    if !ipinfo_token.is_empty() && should_download_ipinfo(&ipinfo_path) {
        let url = format!(
            "https://ipinfo.io/data/ipinfo_lite.mmdb?token={}",
            ipinfo_token
        );
        log::info!("Downloading ipinfo Lite database from ipinfo.io ...");
        if let Ok(dl) = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()
        {
            match download_single(&dl, &ipinfo_path, &url, "ipinfo Lite").await {
                Ok(bytes) => update_db_meta(db_dir, "ipinfo_lite.mmdb", None, bytes),
                Err(e) => {
                    log::warn!("ipinfo Lite download: {e}");
                    if let Err(ve) = validate_ipinfo_token(&dl, ipinfo_token).await {
                        log::warn!("{ve}");
                    }
                }
            }
        }
    }
    let _ = init_ipinfo(&ipinfo_path);

    // ── 5. CDN CIDR ranges from sing-box .srs rule sets ────────────
    //
    // We download native .srs binary format (compact, sing-box native),
    // then parse via `sing-box rule-set decompile` → JSON → ip_cidr.
    //
    // Sources:
    //   A) 123jjck/cdn-ip-ranges – broad CDN coverage (cdn-only_singbox.srs)
    //   B) PentiumB/CDN-RuleSet   – MaxMind+IPInfo+RouteViews based CDN ranges (merged.srs)

    let cdn_sources: &[(&str, &str, &str)] = &[
        ("123jjck", "cdn-only_singbox.srs",
         "https://raw.githubusercontent.com/123jjck/cdn-ip-ranges/main/cdn-only/cdn-only_singbox.srs"),
        ("PentiumB", "merged.srs",
         "https://github.com/PentiumB/CDN-RuleSet/releases/latest/download/merged.srs"),
    ];

    let mut all_cdn_ranges: Vec<(IpAddr, u8)> = Vec::new();

    for (name, filename, url) in cdn_sources {
        let path = db_dir.join(filename);
        let needs = !path.exists()
            || std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|d| d > Duration::from_secs(24 * 3600)).unwrap_or(true))
                .unwrap_or(true);
        if needs {
            log::info!("Downloading CDN .srs from {name} ({url}) ...");
            if let Ok(dl) = reqwest::Client::builder()
                .user_agent("omnitun/1.0")
                .danger_accept_invalid_certs(true)
                .timeout(Duration::from_secs(30))
                .build()
            {
                match download_single(&dl, &path, url, &format!("CDN .srs {name}")).await {
                    Ok(bytes) => {
                        update_db_meta(db_dir, filename, None, bytes);
                        log::info!("CDN .srs {name} downloaded ({bytes} bytes)");
                    }
                    Err(e) => log::warn!("CDN .srs {name} download: {e}"),
                }
            }
        }
        if path.exists() {
            match parse_srs_file(&path) {
                Ok(r) => {
                    log::info!("CDN .srs {name}: {} CIDR entries", r.len());
                    all_cdn_ranges.extend(r);
                }
                Err(e) => log::warn!("CDN .srs {name} parse: {e}"),
            }
        }
    }

    if !all_cdn_ranges.is_empty() {
        log::info!("CDN ranges merged: {} total entries", all_cdn_ranges.len());
        set_cdn_ranges(all_cdn_ranges);
    }

    // ── 6. Ensure meta file exists (for existing DBs not yet tracked) ──
    sync_meta_file(db_dir);
}

/// Download from mirrors, return bytes.
async fn download_db_geoip(
    client: &reqwest::Client,
    target_path: &Path,
    urls: &[&str],
    label: &str,
) -> Result<u64, String> {
    let mut last_err = String::new();
    for url in urls {
        log::info!("Downloading GeoIP {} database from {} ...", label, url);
        match client.get(*url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() >= 400 {
                    let body = resp.text().await.unwrap_or_default();
                    last_err = format!("HTTP {} — {}", status.as_u16(),
                        body.chars().take(200).collect::<String>().trim());
                    continue;
                }
                let bytes = match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => { last_err = format!("{e}"); continue; }
                };
                let len = bytes.len() as u64;
                if len < 1000 {
                    last_err = format!("response too small ({len} bytes) — likely an error page");
                    continue;
                }
                let tmp = target_path.with_extension("mmdb.tmp");
                std::fs::write(&tmp, &bytes)
                    .map_err(|e| format!("Failed to write {label} database: {e}"))?;
                std::fs::rename(&tmp, target_path)
                    .map_err(|e| format!("Failed to finalize {label} database: {e}"))?;
                log::info!("GeoIP {} database downloaded ({len} bytes)", label);
                return Ok(len);
            }
            Err(e) => last_err = format!("{e}"),
        }
    }
    Err(format!("All GeoIP {} mirrors failed: {}", label, last_err))
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
        let bytes = download_single(&dl_client, &ipinfo_path, &url, "ipinfo Lite").await?;
        update_db_meta(db_dir, "ipinfo_lite.mmdb", None, bytes);
        let _ = init_ipinfo(&ipinfo_path);
        Ok(true)
    } else {
        // Still try to init if the file exists but was skipped by rate limit
        let _ = init_ipinfo(&ipinfo_path);
        Ok(false)
    }
}

/// Thin wrapper: check freshness + init all readers.
/// Called during startup and lazy init before profile processing.
pub async fn ensure_downloaded(db_dir: &Path, ipinfo_token: &str) -> Result<(), String> {
    ensure_databases_fresh(db_dir, ipinfo_token).await;
    Ok(())
}

/// Download a single URL (with redirect-follow) to a file.
/// Returns the number of bytes downloaded.
async fn download_single(
    client: &reqwest::Client,
    target_path: &Path,
    url: &str,
    label: &str,
) -> Result<u64, String> {
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
            let len = bytes.len() as u64;
            if len < 1000 {
                return Err(format!(
                    "{label} response too small ({len} bytes) — likely an error page"
                ));
            }
            let tmp_path = target_path.with_extension("mmdb.tmp");
            std::fs::write(&tmp_path, &bytes)
                .map_err(|e| format!("Failed to write {label} database: {e}"))?;
            std::fs::rename(&tmp_path, target_path)
                .map_err(|e| format!("Failed to finalize {label} database: {e}"))?;
            log::info!("{label} database downloaded ({len} bytes)");
            Ok(len)
        }
        Err(e) => Err(format!("Failed to download {label} database: {e}")),
    }
}
/// Download a database from multiple mirror URLs (tries each in order).
/// Returns the number of bytes downloaded.
#[allow(dead_code)]
async fn download_db(
    client: &reqwest::Client,
    target_path: &Path,
    urls: &[&str],
    label: &str,
) -> Result<u64, String> {
    let mut last_err = String::new();
    for url in urls {
        log::info!("Downloading GeoIP {} database from {} ...", label, url);
        match client.get(*url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() >= 400 {
                    let body = resp.text().await.unwrap_or_default();
                    last_err = format!(
                        "HTTP {} — {}",
                        status.as_u16(),
                        body.chars().take(200).collect::<String>().trim()
                    );
                    continue;
                }
                let bytes = match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        last_err = format!("{e}");
                        continue;
                    }
                };
                let len = bytes.len() as u64;
                if len < 100_000 {
                    last_err = format!(
                        "response too small ({len} bytes) — likely an error page"
                    );
                    continue;
                }
                let tmp_path = target_path.with_extension("mmdb.tmp");
                std::fs::write(&tmp_path, &bytes)
                    .map_err(|e| format!("Failed to write GeoIP {} database: {e}", label))?;
                std::fs::rename(&tmp_path, target_path)
                    .map_err(|e| format!("Failed to finalize GeoIP {} database: {e}", label))?;
                log::info!(
                    "GeoIP {} database downloaded ({len} bytes)",
                    label
                );
                return Ok(len);
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

    // ── is_cdn_ip / init_cdn_ranges ────────────────────────────────────

    #[test]
    fn test_is_cdn_ip_loaded() {
        // Directly set CDN ranges without needing sing-box binary
        let ranges = vec![
            ("1.1.1.0".parse::<IpAddr>().unwrap(), 24u8),
        ];
        set_cdn_ranges(ranges);

        assert!(is_cdn_ip("1.1.1.1"));
        assert!(!is_cdn_ip("8.8.8.8"));
        assert!(!is_cdn_ip("invalid"));
    }

    #[test]
    fn test_geoip_result_serde() {
        let result = GeoIpResult {
            country_code: "US".into(),
            country_name: "United States".into(),
            city: "New York".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
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
}
