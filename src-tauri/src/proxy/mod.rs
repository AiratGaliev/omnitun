pub mod batcher;
pub mod cache;
pub mod config;
pub mod geoip;
pub mod installer;
pub mod persistence;
pub mod port_pool;
mod process;

use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::settings::AppSettings;
use installer::Installer;
use process::ProcessManager;
pub use process::{cleanup_stale_pid, remove_pid_file, set_pid_file_path, write_pid_file};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use thiserror::Error;

/// Current Clash API port (0 = not running). Set by config.rs on each start.
pub(crate) static CLASH_API_PORT: AtomicU16 = AtomicU16::new(0);
pub(crate) static PROXY_PID: AtomicU32 = AtomicU32::new(0);

/// Last connected profile — stored for auto-reconnect on watchdog exit.
pub(crate) static LAST_PROFILE: std::sync::Mutex<Option<LastConnection>> =
    std::sync::Mutex::new(None);

/// ID of the currently active (connected) server, for exit IP enrichment.
pub(crate) static ACTIVE_SERVER_ID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Data needed to re-establish a connection after unexpected exit.
#[derive(Clone)]
pub struct LastConnection {
    pub all_servers: Vec<ServerProfile>,
    pub routing_mode: String,
    pub proxy_port: u16,
}

use serde::ser::SerializeStruct;

// ─── Structured errors ─────────────────────────────────────────────────────
//
// DESIGN:
//   - ProxyError variants carry ErrorDetail { code, message }.
//   - ErrorCode is serialized as camelCase (via #[serde(rename_all)]),
//     so the serialized code (e.g. "subscriptionDuplicateUrl") matches
//     i18n keys in src/i18n/{en,ru}.json directly.
//   - The frontend calls t(err.code, { defaultValue: err.message }) — no
//     intermediate mapping table needed.
//   - Io variant wraps std::io::Error via #[from] for ergonomic `?` propagation.
//     Serialized with code "ioError" + io::Error's display string.
//
// LOGGING PRESERVATION:
//   - Display for ErrorDetail returns only `message` (no variant prefix).
//   - Display for ProxyError delegates to ErrorDetail via #[error("{0}")].
//   - Result: format!("Failed to add subscription: {e}") produces
//     "Failed to add subscription: Subscription with URL '...' already exists"
//     — identical to pre-refactor output.
//
// MIGRATION SAFETY:
//   - All existing call sites use constructor pattern ProxyError::Variant(format!(...)).
//   - None pattern-match on variants, so changing inner type String → ErrorDetail
//     is mechanical and compiler-verified.
//   - Helper constructors ProxyError::{config,process,subscription,install}
//     keep call sites terse.

/// Machine-readable error codes.
///
/// SERIALIZATION: `#[serde(rename_all = "camelCase")]` converts Rust variants
/// like `SubscriptionDuplicateUrl` to JSON `"subscriptionDuplicateUrl"`.
/// This string IS the i18n key — the frontend calls `t(err.code)` directly.
///
/// NAMING CONVENTION:
///   - Variant name = `<Category><Specifics>` in PascalCase.
///   - Serialized key = `<category><Specifics>` in camelCase.
///   - Each variant MUST have a matching key in src/i18n/en.json AND ru.json.
///     If a key is missing, the frontend falls back to err.message (English dev text).
///
/// When adding a new variant:
///   1. Add it here (in the appropriate category section).
///   2. Use it at the relevant error site in Rust.
///   3. Add the camelCase key to src/i18n/en.json AND src/i18n/ru.json.
///   4. (Optional) Verify by triggering the error and checking the toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    // ── Subscription ─────────────────────────────────────────────────────
    /// URL already exists in profiles list (add_subscription).
    SubscriptionDuplicateUrl,
    /// Profile ID not found (remove, refresh).
    SubscriptionNotFound,
    /// HTTP fetch failed (network error, timeout, non-200).
    SubscriptionFetchFailed,
    /// Parser failed to decode base64 / parse URI scheme.
    SubscriptionParseFailed,
    /// Subscription content parsed but yielded 0 servers.
    SubscriptionNoServers,
    /// profiles.json parse/serialize/write/rename failed (persistence).
    SubscriptionSaveFailed,

    // ── Config ───────────────────────────────────────────────────────────
    /// sing-box config generation failed (template parse, JSON build).
    ConfigGenerationFailed,
    /// Server uses a protocol not supported by build_outbound.
    ConfigUnsupportedProtocol,
    /// No server profiles provided to generate_config.
    ConfigNoServers,

    // ── Process ──────────────────────────────────────────────────────────
    /// sing-box process already running when start was called.
    ProcessAlreadyRunning,
    /// Failed to spawn sing-box binary.
    ProcessStartFailed,
    /// Failed to kill an existing sing-box process.
    ProcessKillFailed,
    /// Process started but proxy port not reachable (health check failed).
    ProcessNotReachable,

    // ── Install ──────────────────────────────────────────────────────────
    /// Failed to download sing-box binary (network error, HTTP error).
    InstallDownloadFailed,
    /// Failed to extract zip / tar.gz archive.
    InstallExtractFailed,
    /// Binary not found inside the archive.
    InstallBinaryNotFound,
    /// Failed to determine latest version from GitHub API.
    InstallLatestVersionFailed,

    // ── IO ───────────────────────────────────────────────────────────────
    /// Wraps std::io::Error (from #[from]). Generic filesystem error.
    IoError,
}

/// Structured error detail: machine-readable code + human-readable message.
///
/// `message` is the English developer-facing text (used for logs and as
/// frontend fallback if no i18n translation exists for `code`).
#[derive(Debug, Clone)]
pub struct ErrorDetail {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorDetail {
    #[inline]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Display only the human message — variant prefix is irrelevant for logs.
/// This preserves the existing log format after migration:
/// `format!("Failed to add subscription: {e}")` →
/// `"Failed to add subscription: Subscription with URL '...' already exists"`
impl std::fmt::Display for ErrorDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Proxy error enum. Each variant wraps ErrorDetail except Io (which wraps
/// std::io::Error directly via #[from] for ergonomic `?` propagation).
///
/// Display delegates to ErrorDetail / io::Error — no variant prefix in output.
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("{0}")]
    Config(ErrorDetail),

    #[error("{0}")]
    Process(ErrorDetail),

    #[error("{0}")]
    Subscription(ErrorDetail),

    #[error("{0}")]
    Install(ErrorDetail),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl ProxyError {
    // ── Typed constructors ──────────────────────────────────────────────
    // Keep call sites terse: `ProxyError::subscription(ErrorCode::..., "msg")`
    // instead of `ProxyError::Subscription(ErrorDetail::new(ErrorCode::..., "msg"))`.

    #[inline]
    pub fn config(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Config(ErrorDetail::new(code, msg))
    }

    #[inline]
    pub fn process(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Process(ErrorDetail::new(code, msg))
    }

    #[inline]
    pub fn subscription(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Subscription(ErrorDetail::new(code, msg))
    }

    #[inline]
    pub fn install(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Install(ErrorDetail::new(code, msg))
    }
}

/// Serialize as `{ "code": "camelCaseCode", "message": "..." }`.
///
/// IPC: when a Tauri command returns `Err(ProxyError)`, the frontend receives
/// this object as the rejection payload. The frontend's `isRustError` type guard
/// recognizes it, and `t(err.code, { defaultValue: err.message })` does
/// the translation lookup.
///
/// IMPORTANT: this changes the IPC payload shape from a plain string to an
/// object for migrated commands. Commands still returning `Result<_, String>`
/// (outlet_ip, get_real_ip, speed_test, download_ipinfo_db, update_settings,
/// get_connections) keep the old shape — `isRustError` returns false for
/// strings, and the frontend falls back to a generic message.
impl serde::Serialize for ProxyError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut st = serializer.serialize_struct("ProxyError", 2)?;
        match self {
            Self::Config(d)
            | Self::Process(d)
            | Self::Subscription(d)
            | Self::Install(d) => {
                st.serialize_field("code", &d.code)?;
                st.serialize_field("message", &d.message)?;
            }
            Self::Io(e) => {
                st.serialize_field("code", &ErrorCode::IoError)?;
                st.serialize_field("message", &e.to_string())?;
            }
        }
        st.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerProfile {
    pub id: String,
    pub protocol: String,
    pub server: String,
    pub port: u16,
    pub uuid: Option<String>,
    pub password: Option<String>,
    pub method: Option<String>,
    pub sni: Option<String>,
    pub flow: Option<String>,
    pub public_key: Option<String>,
    pub private_key: Option<String>,
    // Transport and Reality for modern VLESS/Trojan
    pub transport_type: Option<String>,
    pub transport_path: Option<String>,
    pub transport_host: Option<String>,
    pub reality_pbk: Option<String>,
    pub reality_sid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ping_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city_name: Option<String>,
    // WireGuard-specific fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_local_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_allowed_ips: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_dns: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_mtu: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wg_keepalive: Option<u16>,
    pub username: Option<String>,
    /// Original subscription-provided name (for display fallback). Not serialized.
    #[serde(skip)]
    pub original_name: String,
    /// Whether this server routes through a CDN (unstable outlet IP).
    /// CDN-routed servers have unstable outlet IPs (different per request).
    #[serde(default, skip_serializing_if = "is_false")]
    pub behind_cdn: bool,
    /// Exit IP geoip — last known exit IP address when connected through this server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_country_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_city_name: Option<String>,
}

/// Serde helper: skip serializing `behind_cdn` when false.
fn is_false(v: &bool) -> bool {
    !v
}

// ─── CDN detection ─────────────────────────────────────────────────────────

/// Keyword-based CDN detection — checks if the domain contains known CDN
/// provider names or CDN-related patterns.
///
/// This catches servers routed through Cloudflare/Fastly/CloudFront CDNs
/// (like `cdn.gamingdll.ir`, `global.ssl.fastly.net`) where the transport_host
/// or sni indicates CDN routing. These servers typically have unstable outlet IPs.
///
/// In addition to keywords, the ASN database (GeoLite2-ASN.mmdb) is checked
/// during batch latency testing for servers whose outlet IP IS reachable
/// through the proxy (covers Cloudflare Workers/WARP where ASN = 13335 etc.).
fn keyword_cdn_check(host: &str) -> bool {
    let lower = host.to_lowercase();
    lower.contains("fastly")
        || lower.contains("cloudflare")
        || lower.contains("cloudfront")
        || lower.contains("akamai")
        || lower.contains("cdn.")
        || lower.ends_with(".workers.dev")
        || lower.ends_with(".pages.dev")
        || lower.ends_with(".cfargotunnel.com")
        || lower.contains(".worker.")
}

/// Lightweight initial CDN detection via keyword check on transport_host, sni.
///
/// The real detection happens during batch testing via GeoLite2-ASN.mmdb
/// (`geoip::is_cdn_entry_ip()`), which examines the actual entry IP.
/// is a fast pre-check for obviously serverless domains, so the UI can show
/// the CDN badge even before batch testing runs.
///
/// Previously this function also did DNS CIDR resolution on entry domains,
/// but that was incorrect — a VPS behind Cloudflare CDN has a stable outlet IP
/// and is NOT a CDN node in the problematic sense. Only Workers/WARP/Pages
/// have unstable outlet IPs.
pub async fn is_cdn_server(s: &ServerProfile) -> bool {
    if let Some(ref host) = s.transport_host {
        if keyword_cdn_check(host) {
            return true;
        }
    }
    if let Some(ref sni) = s.sni {
        if keyword_cdn_check(sni) {
            return true;
        }
    }
    if keyword_cdn_check(&s.server) {
        return true;
    }
    false
}

// ─── ServerProfile impl ────────────────────────────────────────────────────

impl ServerProfile {
    /// Construct a human-readable display name from GeoIP data or fallback to original_name.
    pub fn display_name(&self) -> String {
        if let (Some(country), Some(city)) = (&self.country_name, &self.city_name) {
            format!("{country}, {city}")
        } else if let Some(country) = &self.country_name {
            country.clone()
        } else if !self.original_name.is_empty() {
            self.original_name.clone()
        } else {
            format!("{}:{}", self.server, self.port)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

pub struct ProxyManager {
    process_mgr: ProcessManager,
    bin_dir: PathBuf,
    config_dir: PathBuf,
    installed: Arc<AtomicBool>,
}

impl ProxyManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let bin_dir = data_dir.join("binaries");
        let config_dir = data_dir.join("config");
        let binary_path = bin_dir.join(Installer::binary_name());

        Self {
            process_mgr: ProcessManager::new(binary_path),
            bin_dir,
            config_dir,
            installed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn ensure_installed(&self, app_handle: &AppHandle) -> Result<PathBuf, ProxyError> {
        if self.installed.load(Ordering::SeqCst) || Installer::is_installed(&self.bin_dir) {
            self.installed.store(true, Ordering::SeqCst);
            return Ok(self.bin_dir.join(Installer::binary_name()));
        }

        let logger = app_handle.state::<Mutex<Logger>>();
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Installer,
            "Looking for sing-box updates...".into(),
        );

        let version = Installer::latest_version(app_handle).await?;
        let binary_path = Installer::install(app_handle, &self.bin_dir, &version).await?;
        self.installed.store(true, Ordering::SeqCst);
        Ok(binary_path)
    }

    pub async fn start(
        &self,
        app_handle: &AppHandle,
        proxy_port: u16,
        routing_mode: &str,
        profiles: &[ServerProfile],
        settings: &AppSettings,
    ) -> Result<(), ProxyError> {
        let logger = app_handle.state::<Mutex<Logger>>();

        // ── Idempotent start: clean up any existing process (proxy or Kill Switch) ──
        // Frontend must never manage process lifecycle. This is backend's job.
        if self.process_mgr.is_running() {
            log(
                app_handle,
                &logger,
                LogLevel::Info,
                LogSource::Proxy,
                "Cleaning up existing process (Kill Switch or stale proxy) before starting..."
                    .into(),
            );
            let _ = self.stop(app_handle).await;
        }
        // ────────────────────────────────────────────────────────────────────────────

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            format!(
                "Starting in {routing_mode} mode with {} server(s)...",
                profiles.len()
            ),
        );
        self.ensure_installed(app_handle).await?;

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            "Generating sing-box config...".into(),
        );
        let (config_json, _stats_port) = config::generate_config(
            &self.config_dir,
            proxy_port,
            routing_mode,
            profiles,
            settings,
        )?;
        let config_path = config::write_config(&config_json, &self.config_dir)?;
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            format!("Config written to {}", config_path.display()),
        );

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Process,
            "Starting sing-box process...".into(),
        );
        self.process_mgr.start(&config_path, app_handle).await?;

        // In system-proxy mode, sing-box sets OS proxy automatically
        // via mixed inbound `set_system_proxy: true`.
        if routing_mode == "system-proxy" {
            log(
                app_handle,
                &logger,
                LogLevel::Info,
                LogSource::Proxy,
                "System proxy will be managed by sing-box (set_system_proxy)".into(),
            );
        }

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            format!(
                "sing-box proxy process started (PID {})",
                crate::proxy::PROXY_PID.load(std::sync::atomic::Ordering::Acquire),
            ),
        );

        Ok(())
    }

    pub async fn stop(&self, app_handle: &AppHandle) -> Result<(), ProxyError> {
        // Reset Clash API port so fetcher goes direct
        crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Relaxed);

        let logger = app_handle.state::<Mutex<Logger>>();
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Process,
            "Terminating sing-box process...".into(),
        );
        self.process_mgr.stop().await
    }

    pub fn process_pid(&self) -> Option<u32> {
        self.process_mgr.pid()
    }

    /// Atomically get + clear the tracked PID. Used by stop_proxy under the outer lock.
    pub fn clear_pid(&self) -> Option<u32> {
        self.process_mgr.clear_pid()
    }

    pub fn status(&self) -> ConnectionStatus {
        if let Some(pid) = self.process_mgr.pid() {
            if crate::is_process_alive(pid) {
                return ConnectionStatus::Connected;
            }
            // Process died but PID wasn't cleared yet — clean up
            self.process_mgr.clear_pid();
        }
        ConnectionStatus::Disconnected
    }

    /// Activates Kill Switch: stops any running process and starts a block-only
    /// sing-box config that holds the port open but rejects all traffic.
    /// This prevents OS/apps from bypassing the proxy when it's down.
    pub async fn activate_kill_switch(
        &self,
        app_handle: &AppHandle,
        proxy_port: u16,
        routing_mode: &str,
    ) -> Result<(), ProxyError> {
        // Stop any existing process (dead proxy or previous kill switch)
        let _ = self.stop(app_handle).await;

        // Ensure sing-box binary exists (it should from initial start, but guard against edge cases)
        self.ensure_installed(app_handle).await?;

        let ks_config_json = config::generate_kill_switch_config(proxy_port, routing_mode)?;
        let config_path = config::write_config(&ks_config_json, &self.config_dir)?;

        self.process_mgr.start(&config_path, app_handle).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keyword_cdn_workers_dev() {
        assert!(keyword_cdn_check("my-worker.workers.dev"));
    }

    #[test]
    fn test_keyword_cdn_pages_dev() {
        assert!(keyword_cdn_check("my-site.pages.dev"));
    }

    #[test]
    fn test_keyword_cdn_cfargotunnel() {
        assert!(keyword_cdn_check("tunnel.cfargotunnel.com"));
    }

    #[test]
    fn test_keyword_cdn_worker_subdomain() {
        assert!(keyword_cdn_check("test.worker.example.com"));
    }

    #[test]
    fn test_keyword_cdn_fastly() {
        assert!(keyword_cdn_check("global.ssl.fastly.net"));
    }

    #[test]
    fn test_keyword_cdn_cloudflare() {
        assert!(keyword_cdn_check("cdn.cloudflare.net"));
    }

    #[test]
    fn test_keyword_cdn_cdn_prefix() {
        assert!(keyword_cdn_check("cdn.gamingdll.ir"));
    }

    #[test]
    fn test_keyword_cdn_akamai() {
        assert!(keyword_cdn_check("a1245.g.akamai.net"));
    }

    #[test]
    fn test_keyword_cdn_not_cdn() {
        assert!(!keyword_cdn_check("example.com"));
        assert!(!keyword_cdn_check("my-server.io"));
    }
}
