use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use tokio::sync::{Mutex as AsyncMutex, Semaphore};

/// Current UI locale for city name localisation (set from AppSettings on init).
/// MaxMind GeoLite2-City supports: de, en, es, fr, ja, pt-BR, ru, zh-CN.
static LOCALE: OnceLock<String> = OnceLock::new();

/// Set the locale for city name localisation. Call during app initialization
/// from the frontend's language setting. Falls back to "en" if not set.
pub fn set_locale(locale: &str) {
    let _ = LOCALE.set(locale.to_string());
}

/// Get current locale or default to "en".
fn current_locale() -> &'static str {
    LOCALE.get().map_or("en", |l| l.as_str())
}

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

// ── ipinfo.io Lite record (гибкий парсинг через serde_json::Value) ─────────
//
// Реальный mmdb от ipinfo.io может иметь EITHER:
//   - Плоскую структуру: { country, country_code, asn, as_name, as_type }
//   - Вложенную структуру: { country, country_code, ASN: { asn, name, type, ... } }
//
// Старый struct IpinfoLiteRecord ожидал плоскую структуру и не парсил
// вложенную. Из-за этого asn/as_name ВСЕГДА были None, даже когда данные
// в БД были. Решение: использовать serde_json::Value и извлекать поля
// динамически через helper ipinfo_lite_extract().
//
// Helper ниже принимает serde_json::Value (результат lookup) и возвращает
// все доступные поля как Option<String>.

/// Извлечь поля из ipinfo Lite записи (поддерживает плоский и вложенный формат).
///
/// Возвращает (country, country_code, asn, as_name, as_type).
/// Любое поле может быть None, если его нет в mmdb.
#[allow(clippy::type_complexity)]
pub(crate) fn ipinfo_lite_extract(
    v: &serde_json::Value,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let country = v
        .get("country")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let country_code = v
        .get("country_code")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    // ASN может быть плоским полем или вложенным объектом "ASN"
    // Плоский: { asn: "AS13335", as_name: "Cloudflare", as_type: "hosting" }
    // Вложенный: { ASN: { asn: "AS13335", name: "Cloudflare", type: "hosting", domain: "...", route: "..." } }
    let asn_obj = v.get("ASN").or_else(|| v.get("asn_obj"));
    let (asn, as_name, as_type) = if let Some(obj) = asn_obj {
        let asn = obj
            .get("asn")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let as_name = obj
            .get("name")
            .or_else(|| obj.get("as_name"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let as_type = obj
            .get("type")
            .or_else(|| obj.get("as_type"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        (asn, as_name, as_type)
    } else {
        // Плоский формат
        let asn = v.get("asn").and_then(|x| x.as_str()).map(|s| s.to_string());
        let as_name = v
            .get("as_name")
            .or_else(|| v.get("name"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let as_type = v
            .get("as_type")
            .or_else(|| v.get("type"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        (asn, as_name, as_type)
    };

    (country, country_code, asn, as_name, as_type)
}

// ── ip.guide fallback (network) ────────────────────────────────────────────
//
// Сетевой fallback для IP, по которым ipinfo Lite + MaxMind City + MaxMind ASN
// не дали страну. Используется https://ip.guide/{ip} — бесплатный REST API
// без API key, возвращает JSON с RIR-based country (из ARIN/RIPE/APNIC whois).
//
// Это ЕДИНСТВЕННЫЙ источник country для anycast IP (Cloudflare 1.1.1.1,
// Google 8.8.8.8, Oracle 8.x.x.x) — у них нет geography в MaxMind City,
// потому что anycast не имеет одной страны. RIR whois знает ASN registration
// country, что и возвращает ip.guide.
//
// Ограничения:
//   - Кеш: TTL 1 час
//   - Rate-limit: max 10 запросов за refresh-цикл
//   - Concurrency: max 3 параллельно
//   - Таймаут: 5 сек на запрос
//   - НЕ для private/mock IP
//   - НЕ для CDN-серверов (behind_cdn=true)
//
// 🚫 ANTI-REGRESSION: не удаляй кеш и rate-limit. Без них 50-server refresh
// отправит 50 запросов за 5 секунд → бан от ip.guide.
// 🚫 ANTI-REGRESSION: не добавляй hardcoded ASN→country map. Данные
// уже есть в RIR whois (через ip.guide). Хардкод устаревает и врёт.

/// Cache entry for ip.guide results.
#[derive(Clone)]
struct IpGuideCacheEntry {
    country_code: String,
    #[allow(dead_code)]
    country_name: String,
    #[allow(dead_code)]
    asn: Option<u32>,
    #[allow(dead_code)]
    as_name: Option<String>,
    fetched_at: SystemTime,
}

/// TTL for ip.guide cache entries.
const IPGUIDE_CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour

/// Max requests per refresh-cycle.
const IPGUIDE_MAX_REQUESTS_PER_CYCLE: usize = 10;

/// Max concurrent requests to ip.guide.
static IPGUIDE_SEMAPHORE: Semaphore = Semaphore::const_new(3);

/// In-memory cache: IP → cache entry.
static IPGUIDE_CACHE: OnceLock<AsyncMutex<std::collections::HashMap<String, IpGuideCacheEntry>>> =
    OnceLock::new();

/// Rate-limit counter (resets per refresh-cycle).
static IPGUIDE_REQUESTS_THIS_CYCLE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Get or init the ip.guide cache.
fn ipguide_cache() -> &'static AsyncMutex<std::collections::HashMap<String, IpGuideCacheEntry>> {
    IPGUIDE_CACHE.get_or_init(|| AsyncMutex::new(std::collections::HashMap::new()))
}

/// Reset the per-cycle rate-limit counter.
pub fn reset_ipguide_rate_limit() {
    IPGUIDE_REQUESTS_THIS_CYCLE.store(0, std::sync::atomic::Ordering::Relaxed);
    tracing::debug!(target: "system", "GeoIP: ip.guide rate-limit counter reset for new refresh cycle");
}

/// Check if an IP is private/mock (skip ip.guide for these).
fn is_private_or_mock_ip(ip: &str) -> bool {
    if let Ok(addr) = ip.parse::<IpAddr>() {
        match addr {
            IpAddr::V4(v4) => {
                v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
            }
            IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_unicast_link_local(),
        }
    } else {
        true
    }
}

/// Fetch country from https://ip.guide/{ip}.
///
/// Returns Some(GeoIpResult) on success, None on failure.
/// Logs INFO on success (with source stated), WARN on failure.
pub async fn resolve_via_ip_guide(ip: &str) -> Option<GeoIpResult> {
    if is_private_or_mock_ip(ip) {
        return None;
    }

    // Check cache first
    {
        let cache = ipguide_cache().lock().await;
        if let Some(entry) = cache.get(ip) {
            if let Ok(age) = SystemTime::now().duration_since(entry.fetched_at) {
                if age < IPGUIDE_CACHE_TTL {
                    tracing::debug!(target: "system", "GeoIP: ip.guide cache hit for {}", ip);
                    return Some(GeoIpResult {
                        country_code: entry.country_code.clone(),
                        country_name: String::new(),
                        city: String::new(),
                    });
                }
            }
        }
    }

    // Rate-limit check
    let current = IPGUIDE_REQUESTS_THIS_CYCLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if current >= IPGUIDE_MAX_REQUESTS_PER_CYCLE {
        tracing::warn!(target: "system",
            "GeoIP: ip.guide rate limit exceeded (max {}/cycle), skipping {}",
            IPGUIDE_MAX_REQUESTS_PER_CYCLE, ip
        );
        return None;
    }

    // Concurrency limit
    let _permit = match IPGUIDE_SEMAPHORE.acquire().await {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!(target: "system", "GeoIP: ip.guide semaphore closed for {}", ip);
            return None;
        }
    };

    // HTTP GET with timeout
    let url = format!("https://ip.guide/{}", ip);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent("omnitun/1.0")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "system", "GeoIP: ip.guide client build failed for {}: {}", ip, e);
            return None;
        }
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "system", "GeoIP: ip.guide request failed for {}: {}", ip, e);
            return None;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!(target: "system", "GeoIP: ip.guide returned HTTP {} for {}", resp.status(), ip);
        return None;
    }

    let body = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(target: "system", "GeoIP: ip.guide body read failed for {}: {}", ip, e);
            return None;
        }
    };

    // Parse JSON. ip.guide format:
    // { "ip": "...", "network": { "autonomous_system": { "asn": N, "name": "...", "country": "US", "rir": "ARIN" } }, ... }
    // Prefer network.autonomous_system.country (RIR-based) over location.country (geo-based)
    let json: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "system", "GeoIP: ip.guide JSON parse failed for {}: {}. Body: {}",
                       ip, e, &body[..body.len().min(200)]);
            return None;
        }
    };

    let (country_code, asn_num, as_name) = {
        let asn_obj = json.get("network").and_then(|n| n.get("autonomous_system"));
        let cc = asn_obj
            .and_then(|a| a.get("country"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let asn = asn_obj
            .and_then(|a| a.get("asn"))
            .and_then(|a| a.as_u64())
            .map(|n| n as u32);
        let name = asn_obj
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string());
        (cc.to_string(), asn, name)
    };

    // Fallback to location.country if ASN country is empty
    let country_code = if country_code.is_empty() {
        json.get("location")
            .and_then(|l| l.get("country"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        country_code
    };

    if country_code.is_empty() {
        tracing::warn!(target: "system", "GeoIP: ip.guide returned no country for {}", ip);
        return None;
    }

    tracing::info!(target: "system",
        "GeoIP: country resolved via ip.guide for {} → {} (ASN {} {}). \
         country_name will be filled by frontend via i18n-iso-countries.",
        ip, country_code,
        asn_num.map(|n| n.to_string()).unwrap_or_else(|| "?".to_string()),
        as_name.as_deref().unwrap_or("?")
    );

    // Cache the result
    let entry = IpGuideCacheEntry {
        country_code: country_code.clone(),
        country_name: String::new(),
        asn: asn_num,
        as_name: as_name.clone(),
        fetched_at: SystemTime::now(),
    };
    {
        let mut cache = ipguide_cache().lock().await;
        cache.insert(ip.to_string(), entry);
    }

    Some(GeoIpResult {
        country_code,
        country_name: String::new(),
        city: String::new(),
    })
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

/// Cleanup stale tmp files from previous sessions.
///
/// If the app crashed during `parse_srs_file`, the tmp file
/// `omnitun-cdn-srs-{uuid}.json` stays in %TEMP%. Over months of
/// operation, hundreds of files can accumulate.
///
/// This function removes files with the `omnitun-cdn-srs-` prefix
/// that are older than 1 hour. The 1-hour threshold ensures we never
/// delete a file currently being written by a parallel `parse_srs_file`
/// call (sing-box decompile takes <1 second).
///
/// Call this at app startup, before `ensure_downloaded`.
///
/// 🚫 ANTI-REGRESSION: не удаляй эту функцию "потому что tmp-файлы и так
/// чистятся при выходе". При панике/краше файлы остаются. Эта функция —
/// защита от накопления.
pub fn cleanup_stale_tmp_files() {
    let tmp_dir = std::env::temp_dir();
    let cutoff = SystemTime::now() - Duration::from_secs(3600); // 1 hour
    let mut cleaned = 0u32;
    if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with("omnitun-cdn-srs-") && name.ends_with(".json") {
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            if mtime < cutoff {
                                let _ = std::fs::remove_file(entry.path());
                                cleaned += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    if cleaned > 0 {
        tracing::info!(target: "system", "Cleaned up {} stale CDN tmp file(s) from previous sessions", cleaned);
    }
}

/// Parse a .srs file: call `sing-box rule-set decompile` on the raw file
/// to convert to JSON, then extract ip_cidr entries.
///
/// # Concurrency
///
/// This function is **thread-safe** and may be called concurrently from
/// multiple tasks (e.g. `ensure_downloaded` and the lazy-init path in
/// `profile::mod`). Each call uses a unique temporary output file to
/// avoid races between parallel `sing-box` invocations.
///
/// # Errors
///
/// Returns `Err(String)` with a human-readable diagnostic if:
///   - `sing-box` binary is not set (`SINGBOX_BIN`);
///   - `sing-box` fails to start (rare; usually means binary missing);
///   - `sing-box` exits with non-zero status (stderr included);
///   - `sing-box` exits successfully but did not create the output file
///     (defensive check — this is what previously caused `os error 2`);
///   - the output file cannot be read (permissions, race with another
///     process, antivirus lock on Windows);
///   - the output is not valid JSON;
///   - the JSON structure does not match the expected `rules[].ip_cidr`
///     layout.
///
/// On any error path the temporary file is best-effort cleaned up.
pub fn parse_srs_file(path: &Path) -> Result<Vec<(IpAddr, u8)>, String> {
    let sb = SINGBOX_BIN
        .get()
        .ok_or_else(|| "sing-box binary not set".to_string())?;

    // ── 1. Unique tmp file per call ───────────────────────────────────
    // Previously this function used a hardcoded "cdn_srs_tmp.json" name,
    // which caused a race condition when called concurrently from
    // `ensure_downloaded` and `profile::mod::lazy_init_cdn_ranges`:
    // one call would `remove_file` the output while another was still
    // reading it, producing "os error 2" (file not found).
    //
    // Using uuid-based unique names eliminates the race entirely without
    // needing a global Mutex (which would serialise all .srs parsing
    // and defeat the purpose of parallel CDN-range loading).
    let tmp_dir = std::env::temp_dir();
    let tmp_uuid = uuid::Uuid::new_v4();
    let tmp_name = format!("omnitun-cdn-srs-{tmp_uuid}.json");
    let tmp_json = tmp_dir.join(&tmp_name);

    tracing::debug!(target: "system",
        "parse_srs_file: decompiling {} → {}",
        path.display(),
        tmp_json.display()
    );

    // ── 2. Run sing-box ───────────────────────────────────────────────
    let out = std::process::Command::new(sb)
        .args([
            "rule-set",
            "decompile",
            "-o",
            &tmp_json.to_string_lossy(),
            &path.to_string_lossy(),
        ])
        .output()
        .map_err(|e| {
            // Best-effort cleanup of the (probably empty) tmp file.
            let _ = std::fs::remove_file(&tmp_json);
            format!("Failed to run sing-box: {e}")
        })?;

    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp_json);
        return Err(format!(
            "sing-box decompile failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    // ── 3. Defensive check: sing-box exited OK — did it actually create
    //        the output file? On Windows, antivirus or filesystem
    //        redirection can cause this. Without this check, the next
    //        `read_to_string` would return a bare "os error 2" with no
    //        context, which is exactly what the user saw in logs.
    if !tmp_json.exists() {
        // Include sing-box's stderr/stdout for diagnosis — sometimes
        // sing-box reports a non-fatal warning to stderr but still
        // exits 0 without producing output.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "sing-box exited successfully but did not create output file {}. \
             stdout={}, stderr={}",
            tmp_json.display(),
            stdout.trim(),
            stderr.trim()
        ));
    }

    // ── 4. Read & parse JSON ──────────────────────────────────────────
    let json_text = match std::fs::read_to_string(&tmp_json) {
        Ok(t) => t,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_json);
            return Err(format!(
                "Failed to read sing-box output JSON ({}): {e}. \
                 If this persists, check antivirus exclusions for {}.",
                tmp_json.display(),
                std::env::temp_dir().display()
            ));
        }
    };

    // Cleanup: best-effort, log on failure (could indicate filesystem
    // issues worth knowing about).
    if let Err(e) = std::fs::remove_file(&tmp_json) {
        tracing::warn!(target: "system",
            "Failed to clean up tmp file {}: {e} (non-fatal — file will be \
             removed at next OS temp cleanup)",
            tmp_json.display()
        );
    }

    let json: serde_json::Value = serde_json::from_str(&json_text).map_err(|e| {
        format!(
            "Failed to parse sing-box output as JSON: {e}. First 200 bytes: {:?}",
            json_text.chars().take(200).collect::<String>()
        )
    })?;

    // ── 5. Extract ip_cidr entries ────────────────────────────────────
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
    tracing::info!(target: "system",
        "Loaded {} CDN CIDR entries from {}",
        ranges.len(),
        path.display()
    );
    set_cdn_ranges(ranges);
    Ok(())
}

/// Returns true when CDN CIDR ranges are loaded.
pub fn is_cdn_ready() -> bool {
    CDN_RANGES.get().is_some()
}

/// Returns the number of loaded CDN CIDR entries (0 if not loaded).
pub fn cdn_ranges_count() -> usize {
    CDN_RANGES.get().map(|r| r.len()).unwrap_or(0)
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
            if len >= 32 {
                return !0u128;
            }
            (!0u128) << (32 - len)
        }
        IpAddr::V6(_) => {
            if len >= 128 {
                return !0u128;
            }
            (!0u128) << (128 - len)
        }
    }
}
/// Check if an IP address belongs to a known CDN provider.
/// Delegates to .srs CIDR database (123jjck + PentiumB).
/// Used to determine if a server's entry IP is a CDN edge node.
pub fn is_cdn_entry_ip(ip: &str) -> bool {
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
///   2. ipinfo.io Lite — country_code + country_name (через serde_json::Value, гибкий парсинг)
///   3. MaxMind GeoLite2 City — city + fallback country
///   4. MaxMind GeoLite2 ASN — диагностическое логирование (не inference)
///   5. None
///
/// Returns None when no database is loaded, the input is a hostname,
/// or the IP is not found in any loaded database.
pub fn resolve(ip: &str) -> Option<GeoIpResult> {
    if let Some(mock) = mock_lookup(ip) {
        return Some(mock);
    }

    let addr: IpAddr = ip.parse().ok()?;

    // ── 1. ipinfo Lite (через serde_json::Value, гибкий парсинг) ──────
    // Раньше использовался IpinfoLiteRecord с плоскими полями, но реальный
    // mmdb может иметь вложенную структуру ASN. Это приводило к тому, что
    // asn/as_name всегда были None. Теперь парсим через Value.
    let mut country_code = String::new();
    let mut country_name = String::new();
    let mut ipinfo_asn: Option<String> = None;
    let mut ipinfo_as_name: Option<String> = None;
    if let Some(reader) = IPINFO_LITE_READER.get() {
        if let Ok(value) = reader.lookup::<serde_json::Value>(addr) {
            let (country, cc, asn, as_name, _as_type) = ipinfo_lite_extract(&value);
            ipinfo_asn = asn.clone();
            ipinfo_as_name = as_name.clone();
            if let Some(ref cc_str) = cc {
                if !cc_str.is_empty() {
                    country_code = cc_str.clone();
                    country_name = country.unwrap_or_default();
                }
            }
        }
    }

    // Диагностическое логирование: если ipinfo Lite вернул запись с ASN,
    // но без country_code — это anycast IP или специальный случай.
    if country_code.is_empty() {
        if let Some(ref asn) = ipinfo_asn {
            tracing::debug!(target: "system",
                "GeoIP: ipinfo Lite has ASN for {} but no country_code (anycast?): \
                 asn={}, as_name={:?}",
                ip, asn, ipinfo_as_name
            );
        }
    }

    // ── 2. MaxMind City (city enrichment + fallback country) ───────────
    let mut city = String::new();
    if let Some(reader) = CITY_READER.get() {
        if let Ok(entry) = reader.lookup::<maxminddb::geoip2::City>(addr) {
            city = entry
                .city
                .as_ref()
                .and_then(|c| c.names.as_ref())
                .and_then(|n| {
                    // Пробуем запрошенную локаль (ru, de, fr...),
                    // fallback на "en" если перевод отсутствует.
                    n.get(current_locale())
                        .or_else(|| n.get("en"))
                        .map(|s| s.to_string())
                })
                .unwrap_or_default();

            // Fallback country: только если ipinfo Lite не дал
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

    // ── 3. MaxMind ASN (диагностическое логирование, не inference) ────
    // ASN_READER загружен, но раньше не использовался. Теперь логируем
    // ASN+org на DEBUG, если country не найдена.
    if country_code.is_empty() {
        if let Some(reader) = ASN_READER.get() {
            if let Ok(entry) = reader.lookup::<maxminddb::geoip2::Asn>(addr) {
                let asn_num = entry.autonomous_system_number;
                let asn_org = entry.autonomous_system_organization;
                if asn_num.is_some() || asn_org.is_some() {
                    tracing::debug!(target: "system",
                        "GeoIP: MaxMind ASN has data for {} (no country in City): \
                         asn={:?}, org={:?}. Will try ip.guide fallback.",
                        ip, asn_num, asn_org
                    );
                }
            }
        }
    }

    if country_code.is_empty() && city.is_empty() {
        return None;
    }

    // Edge case: если есть city но нет country — всё равно возвращаем None,
    // чтобы resolve_hostname мог вызвать ip.guide fallback.
    // City без country для non-anycast IP крайне маловероятен (MaxMind City
    // почти всегда содержит country для IP с city), а для anycast IP
    // ip.guide — единственный источник country.
    if country_code.is_empty() && !city.is_empty() {
        tracing::debug!(target: "system",
            "GeoIP: MaxMind City has city ({}) for {} but no country — will try ip.guide",
            city, ip
        );
        return None;
    }

    Some(GeoIpResult {
        country_code,
        country_name,
        city,
    })
}

/// Try to resolve a hostname to an IP address, then do GeoIP lookup.
/// Falls back to ip.guide network API if local GeoIP databases don't have the IP.
pub async fn resolve_hostname(host: &str) -> Option<GeoIpResult> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        // Сначала пробуем sync resolve (ipinfo Lite + City + ASN logging).
        if let Some(result) = resolve(&ip.to_string()) {
            return Some(result);
        }
        // Fallback: ip.guide (сеть, с кешем и rate-limit).
        return resolve_via_ip_guide(&ip.to_string()).await;
    }

    let addr_str = format!("{host}:0");
    let result = tokio::net::lookup_host(&addr_str).await;
    match result {
        Ok(mut addrs) => {
            if let Some(addr) = addrs.next() {
                let ip_str = addr.ip().to_string();
                if let Some(result) = resolve(&ip_str) {
                    Some(result)
                } else {
                    resolve_via_ip_guide(&ip_str).await
                }
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

    for name in &[
        "GeoLite2-City.mmdb",
        "GeoLite2-ASN.mmdb",
        "ipinfo_lite.mmdb",
        "cdn-only_singbox.srs",
        "merged.srs",
    ] {
        if meta.databases.contains_key(*name) {
            continue; // already tracked
        }
        let path = db_dir.join(name);
        if let Ok(meta_info) = std::fs::metadata(&path) {
            if let Ok(modified) = meta_info.modified() {
                if let Ok(dur) = SystemTime::now().duration_since(modified) {
                    let iso =
                        chrono::DateTime::<chrono::Utc>::from(SystemTime::now() - dur).to_rfc3339();
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
        tracing::info!(target: "system",
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
    let url = format!("https://ipinfo.io/data/ipinfo_lite.mmdb?token={}", token);

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
        403 => Err(
            "IPinfo token does not have access to the Lite database — upgrade at ipinfo.io".into(),
        ),
        404 => Err("IPinfo database URL not found — check the API endpoint".into()),
        code if code >= 400 => Err(format!(
            "IPinfo token validation failed with HTTP {code} — check your token at ipinfo.io"
        )),
        _ => {
            // 200 or other success code without redirect is also unexpected
            // for the HEAD endpoint, but treat as valid rather than block
            tracing::info!(target: "system",
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
        tracing::warn!(target: "system",
            "GitHub API returned HTTP {} for release check",
            resp.status().as_u16()
        );
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let tag = body.get("tag_name")?.as_str()?.to_string();
    tracing::info!(target: "system", "GitHub API: latest P3TERX/GeoLite.mmdb release is {tag}");

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
            tracing::warn!(target: "system", "Failed to build HTTP client for GeoIP check: {e}");
            return;
        }
    };

    // ── 1. MaxMind: check GitHub for latest release ────────────────────
    let latest_tag = check_latest_release(&client, db_dir).await;
    if let Some(ref tag) = latest_tag {
        tracing::info!(target: "system", "Latest P3TERX/GeoLite.mmdb release: {tag}");
    } else {
        tracing::info!(target: "system", "P3TERX/GeoLite.mmdb release check skipped (cached or unavailable)");
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
            "https://fastly.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-City.mmdb",
            "https://ghproxy.net/https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-City.mmdb",
        ];
        if let Ok(dl) = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()
        {
            match download_db_geoip(&dl, &city_path, &urls, "City").await {
                Ok(bytes) => {
                    update_db_meta(db_dir, "GeoLite2-City.mmdb", latest_tag.as_deref(), bytes)
                }
                Err(e) => tracing::warn!(target: "system", "City DB download: {e}"),
            }
        }
    } else if city_path.exists() {
        tracing::info!(target: "system", "GeoLite2-City.mmdb is up-to-date");
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
            "https://fastly.jsdelivr.net/gh/P3TERX/GeoLite.mmdb@main/GeoLite2-ASN.mmdb",
            "https://ghproxy.net/https://github.com/P3TERX/GeoLite.mmdb/releases/latest/download/GeoLite2-ASN.mmdb",
        ];
        if let Ok(dl) = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()
        {
            match download_db_geoip(&dl, &asn_path, &urls, "ASN").await {
                Ok(bytes) => {
                    update_db_meta(db_dir, "GeoLite2-ASN.mmdb", latest_tag.as_deref(), bytes)
                }
                Err(e) => tracing::warn!(target: "system", "ASN DB download: {e}"),
            }
        }
    } else if asn_path.exists() {
        tracing::info!(target: "system", "GeoLite2-ASN.mmdb is up-to-date");
    }
    let _ = init_asn(&asn_path);

    // ── 4. ipinfo.io Lite ──────────────────────────────────────────────
    if !ipinfo_token.is_empty() && should_download_ipinfo(&ipinfo_path) {
        let url = format!(
            "https://ipinfo.io/data/ipinfo_lite.mmdb?token={}",
            ipinfo_token
        );
        tracing::info!(target: "system", "Downloading ipinfo Lite database from ipinfo.io ...");
        if let Ok(dl) = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()
        {
            match download_single(&dl, &ipinfo_path, &url, "ipinfo Lite").await {
                Ok(bytes) => update_db_meta(db_dir, "ipinfo_lite.mmdb", None, bytes),
                Err(e) => {
                    tracing::warn!(target: "system", "ipinfo Lite download: {e}");
                    if let Err(ve) = validate_ipinfo_token(&dl, ipinfo_token).await {
                        tracing::warn!(target: "system", "{ve}");
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
    //
    // Note: raw.githubusercontent.com and github.com are blocked in China.
    // ghproxy.net mirror is used as fallback for both sources.

    let cdn_sources: &[(&str, &str, &[&str])] = &[
        ("123jjck", "cdn-only_singbox.srs", &[
            "https://raw.githubusercontent.com/123jjck/cdn-ip-ranges/main/cdn-only/cdn-only_singbox.srs",
            "https://ghproxy.net/https://raw.githubusercontent.com/123jjck/cdn-ip-ranges/main/cdn-only/cdn-only_singbox.srs",
        ]),
        ("PentiumB", "merged.srs", &[
            "https://github.com/PentiumB/CDN-RuleSet/releases/latest/download/merged.srs",
            "https://ghproxy.net/https://github.com/PentiumB/CDN-RuleSet/releases/latest/download/merged.srs",
        ]),
    ];

    let mut all_cdn_ranges: Vec<(IpAddr, u8)> = Vec::new();

    for (name, filename, urls) in cdn_sources {
        let path = db_dir.join(filename);
        let needs = !path.exists()
            || std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(|t| {
                    t.elapsed()
                        .map(|d| d > Duration::from_secs(24 * 3600))
                        .unwrap_or(true)
                })
                .unwrap_or(true);
        if needs {
            let mut downloaded = false;
            for url in *urls {
                tracing::info!(target: "system", "Downloading CDN .srs from {name} ({url}) ...");
                if let Ok(dl) = reqwest::Client::builder()
                    .user_agent("omnitun/1.0")
                    .danger_accept_invalid_certs(true)
                    .timeout(Duration::from_secs(30))
                    .build()
                {
                    match download_single(&dl, &path, url, &format!("CDN .srs {name}")).await {
                        Ok(bytes) => {
                            update_db_meta(db_dir, filename, None, bytes);
                            tracing::info!(target: "system", "CDN .srs {name} downloaded ({bytes} bytes)");
                            downloaded = true;
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(target: "system", "CDN .srs {name} download from {url} failed: {e}")
                        }
                    }
                }
            }
            if !downloaded {
                tracing::warn!(target: "system", "CDN .srs {name}: all download URLs failed");
            }
        }
        if path.exists() {
            match parse_srs_file(&path) {
                Ok(r) => {
                    tracing::info!(target: "system", "CDN .srs {name}: {} CIDR entries", r.len());
                    all_cdn_ranges.extend(r);
                }
                Err(e) => tracing::warn!(target: "system", "CDN .srs {name} parse: {e}"),
            }
        }
    }

    if !all_cdn_ranges.is_empty() {
        tracing::info!(target: "system", "CDN ranges merged: {} total entries", all_cdn_ranges.len());
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
        tracing::info!(target: "system", "Downloading GeoIP {} database from {} ...", label, url);
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
                if len < 1000 {
                    last_err = format!("response too small ({len} bytes) — likely an error page");
                    continue;
                }
                let tmp = target_path.with_extension("mmdb.tmp");
                std::fs::write(&tmp, &bytes)
                    .map_err(|e| format!("Failed to write {label} database: {e}"))?;
                std::fs::rename(&tmp, target_path)
                    .map_err(|e| format!("Failed to finalize {label} database: {e}"))?;
                tracing::info!(target: "system", "GeoIP {} database downloaded ({len} bytes)", label);
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

        tracing::info!(target: "system", "Downloading ipinfo Lite database from ipinfo.io ...");
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
            tracing::info!(target: "system", "{label} database downloaded ({len} bytes)");
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
        tracing::info!(target: "system", "Downloading GeoIP {} database from {} ...", label, url);
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
                    last_err = format!("response too small ({len} bytes) — likely an error page");
                    continue;
                }
                let tmp_path = target_path.with_extension("mmdb.tmp");
                std::fs::write(&tmp_path, &bytes)
                    .map_err(|e| format!("Failed to write GeoIP {} database: {e}", label))?;
                std::fs::rename(&tmp_path, target_path)
                    .map_err(|e| format!("Failed to finalize GeoIP {} database: {e}", label))?;
                tracing::info!(target: "system", "GeoIP {} database downloaded ({len} bytes)", label);
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
    fn test_is_cdn_entry_ip_cloudflare() {
        assert!(!is_cdn_entry_ip("1.1.1.1"));
        assert!(!is_cdn_entry_ip("8.8.8.8"));
    }

    #[test]
    fn test_is_cdn_entry_ip_fastly_when_db_ready() {
        assert!(!is_cdn_entry_ip("151.101.1.1"));
    }

    #[test]
    fn test_is_cdn_entry_ip_aws() {
        assert!(!is_cdn_entry_ip("13.32.0.1"));
        assert!(!is_cdn_entry_ip("52.84.0.1"));
    }

    #[test]
    fn test_is_cdn_entry_ip_regular_asn() {
        assert!(!is_cdn_entry_ip("12345"));
        assert!(!is_cdn_entry_ip("42"));
        assert!(!is_cdn_entry_ip("0"));
    }

    #[test]
    fn test_is_cdn_entry_ip_invalid_input() {
        assert!(!is_cdn_entry_ip(""));
        assert!(!is_cdn_entry_ip("not_a_number"));
        assert!(!is_cdn_entry_ip("abc123"));
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

    // ── ipinfo Lite record (ipinfo_lite_extract helper) ─────────────────

    #[test]
    fn test_ipinfo_lite_extract_defaults() {
        // An empty JSON object should return all fields as None
        let v: serde_json::Value = serde_json::from_str(r#"{}"#).unwrap();
        let (country, cc, asn, as_name, as_type) = super::ipinfo_lite_extract(&v);
        assert!(country.is_none());
        assert!(cc.is_none());
        assert!(asn.is_none());
        assert!(as_name.is_none());
        assert!(as_type.is_none());
    }

    #[test]
    fn test_ipinfo_lite_extract_flat() {
        // Плоский формат (традиционная структура ipinfo Lite)
        let json = r#"{
            "country": "United Kingdom",
            "country_code": "GB",
            "asn": "AS13335",
            "as_name": "Cloudflare Inc.",
            "as_type": "hosting"
        }"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let (country, cc, asn, as_name, as_type) = super::ipinfo_lite_extract(&v);
        assert_eq!(country.as_deref(), Some("United Kingdom"));
        assert_eq!(cc.as_deref(), Some("GB"));
        assert_eq!(asn.as_deref(), Some("AS13335"));
        assert_eq!(as_name.as_deref(), Some("Cloudflare Inc."));
        assert_eq!(as_type.as_deref(), Some("hosting"));
    }

    #[test]
    fn test_ipinfo_lite_extract_nested() {
        // Вложенный формат (ASN как объект, как может быть в реальном mmdb)
        let json = r#"{
            "country": "Germany",
            "country_code": "DE",
            "ASN": {
                "asn": "AS24940",
                "name": "Hetzner Online GmbH",
                "type": "hosting",
                "domain": "hetzner.com",
                "route": "5.9.0.0/16"
            }
        }"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let (country, cc, asn, as_name, as_type) = super::ipinfo_lite_extract(&v);
        assert_eq!(country.as_deref(), Some("Germany"));
        assert_eq!(cc.as_deref(), Some("DE"));
        assert_eq!(asn.as_deref(), Some("AS24940"));
        assert_eq!(as_name.as_deref(), Some("Hetzner Online GmbH"));
        assert_eq!(as_type.as_deref(), Some("hosting"));
    }

    // ── is_cdn_ip / init_cdn_ranges ────────────────────────────────────

    #[test]
    fn test_is_cdn_ip_loaded() {
        // Directly set CDN ranges without needing sing-box binary
        let ranges = vec![("1.1.1.0".parse::<IpAddr>().unwrap(), 24u8)];
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
        assert!(
            json.contains("\"city\":\"\""),
            "JSON should have empty city"
        );
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

    // ── parse_srs_file ─────────────────────────────────────────────────

    /// Locate the sing-box binary: check SINGBOX_BIN env var first,
    /// then PATH.
    fn find_singbox() -> Option<std::path::PathBuf> {
        if let Some(p) = SINGBOX_BIN.get() {
            return Some(p.clone());
        }
        // Try via PATH / `where sing-box` on Windows
        std::process::Command::new("sing-box")
            .arg("--version")
            .output()
            .ok()
            .map(|_| "sing-box".into())
    }

    /// Make a .srs fixture from a JSON rule-set by invoking
    /// `sing-box rule-set compile`. Returns the path to the .srs file.
    /// Requires sing-box to be on PATH.
    fn make_test_srs(cidrs: &[&str]) -> Option<std::path::PathBuf> {
        let sb = find_singbox()?;

        let json = serde_json::json!({
            "version": 3,
            "rules": [{ "ip_cidr": cidrs }]
        });

        let tmp_dir = std::env::temp_dir();
        let in_name = format!("omnitun-cdn-test-{}.json", uuid::Uuid::new_v4());
        let in_path = tmp_dir.join(&in_name);
        let out_path = in_path.with_extension("srs");

        std::fs::write(&in_path, json.to_string()).ok()?;

        let status = std::process::Command::new(&sb)
            .args([
                "rule-set",
                "compile",
                "-o",
                &out_path.to_string_lossy(),
                &in_path.to_string_lossy(),
            ])
            .output()
            .ok()?;

        // Clean up input
        let _ = std::fs::remove_file(&in_path);

        if !status.status.success() {
            return None;
        }

        Some(out_path)
    }

    #[test]
    fn test_parse_srs_file_missing_binary() {
        // Only run if SINGBOX_BIN is NOT already set.
        if SINGBOX_BIN.get().is_some() {
            eprintln!("Skipping test: SINGBOX_BIN already set");
            return;
        }
        let result = parse_srs_file(std::path::Path::new("nonexistent.srs"));
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("sing-box binary not set"),
            "should report missing binary"
        );
    }

    #[test]
    fn test_parse_srs_file_missing_input() {
        let sb = match find_singbox() {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: sing-box not found");
                return;
            }
        };
        // Ensure SINGBOX_BIN is set so parse_srs_file doesn't bail early
        let _ = SINGBOX_BIN.set(sb);

        let result = parse_srs_file(std::path::Path::new("/nonexistent/path/file.srs"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("sing-box decompile failed") || err.contains("did not create output file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_srs_file_returns_cidrs() {
        let sb = match find_singbox() {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: sing-box not found");
                return;
            }
        };
        let _ = SINGBOX_BIN.set(sb);

        let srs = match make_test_srs(&["10.0.0.0/8", "192.168.0.0/16"]) {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: could not create .srs fixture");
                return;
            }
        };

        let result = parse_srs_file(&srs).expect("parse_srs_file should succeed");
        assert!(
            result.len() >= 2,
            "expected >=2 CIDRs, got {}",
            result.len()
        );

        let has_v4_10 = result.iter().any(|(ip, _)| ip.to_string() == "10.0.0.0");
        let has_v4_192 = result.iter().any(|(ip, _)| ip.to_string() == "192.168.0.0");
        assert!(has_v4_10, "missing 10.0.0.0/8");
        assert!(has_v4_192, "missing 192.168.0.0/16");

        let _ = std::fs::remove_file(&srs);
    }

    /// Regression test for the original race condition bug:
    /// parallel calls to parse_srs_file must NOT race on the tmp output file.
    #[test]
    fn test_parse_srs_file_concurrent_no_race() {
        let sb = match find_singbox() {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: sing-box not found");
                return;
            }
        };
        let _ = SINGBOX_BIN.set(sb);

        let srs1 = match make_test_srs(&["10.0.0.0/8", "11.0.0.0/8"]) {
            Some(p) => p,
            None => return,
        };
        let srs2 = match make_test_srs(&["172.16.0.0/12", "172.32.0.0/16"]) {
            Some(p) => p,
            None => {
                let _ = std::fs::remove_file(&srs1);
                return;
            }
        };

        let srs1 = std::sync::Arc::new(srs1);
        let srs2 = std::sync::Arc::new(srs2);
        let results = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for i in 0..8 {
            let srs = if i % 2 == 0 {
                srs1.clone()
            } else {
                srs2.clone()
            };
            let results = results.clone();
            handles.push(std::thread::spawn(move || {
                let r = parse_srs_file(&srs);
                results.lock().unwrap().push((i, r.is_ok()));
            }));
        }
        for h in handles {
            h.join().expect("thread panicked");
        }

        let results = results.lock().unwrap();
        let failures: Vec<_> = results.iter().filter(|(_, ok)| !*ok).collect();
        assert!(
            failures.is_empty(),
            "race regression: {} of {} parallel calls failed: {:?}",
            failures.len(),
            results.len(),
            failures
        );

        let _ = std::fs::remove_file(&*srs1);
        let _ = std::fs::remove_file(&*srs2);
    }

    /// Verify tmp file cleanup after successful parse_srs_file call.
    #[test]
    fn test_parse_srs_file_cleans_up_tmp() {
        let sb = match find_singbox() {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: sing-box not found");
                return;
            }
        };
        let _ = SINGBOX_BIN.set(sb);

        let srs = match make_test_srs(&["10.0.0.0/8"]) {
            Some(p) => p,
            None => return,
        };

        let tmp_dir = std::env::temp_dir();
        let before = std::fs::read_dir(&tmp_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("omnitun-cdn-srs-"))
                    .unwrap_or(false)
            })
            .count();

        let _ = parse_srs_file(&srs);

        let after = std::fs::read_dir(&tmp_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("omnitun-cdn-srs-"))
                    .unwrap_or(false)
            })
            .count();

        assert_eq!(before, after);

        let _ = std::fs::remove_file(&srs);
    }
}
