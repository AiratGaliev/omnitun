pub mod config;
pub mod geoip;
pub mod installer;
mod process;

use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::settings::AppSettings;
use installer::Installer;
use process::ProcessManager;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use thiserror::Error;

/// Current Clash API port (0 = not running). Set by config.rs on each start.
pub(crate) static CLASH_API_PORT: AtomicU16 = AtomicU16::new(0);
pub(crate) static VPN_PID: AtomicU32 = AtomicU32::new(0);

/// Last connected profile — stored for auto-reconnect on watchdog exit.
pub(crate) static LAST_PROFILE: std::sync::Mutex<Option<LastConnection>> =
    std::sync::Mutex::new(None);

/// Data needed to re-establish a connection after unexpected exit.
#[derive(Clone)]
pub struct LastConnection {
    pub all_servers: Vec<ServerProfile>,
    pub routing_mode: String,
    pub proxy_port: u16,
}

#[derive(Debug, Error)]
pub enum VpnError {
    #[error("Config generation failed: {0}")]
    Config(String),
    #[error("Process error: {0}")]
    Process(String),
    #[error("Subscription error: {0}")]
    Subscription(String),
    #[error("Installation error: {0}")]
    Install(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<VpnError> for String {
    fn from(e: VpnError) -> Self {
        e.to_string()
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
}

/// Serde helper: skip serializing `behind_cdn` when false.
fn is_false(v: &bool) -> bool {
    !v
}

// ─── CDN detection ─────────────────────────────────────────────────────────

/// Known CDN provider CIDR ranges (IPv4). Used by `is_cdn_server` to check if a
/// resolved server IP belongs to a CDN network.
const CDN_CIDRS: &[(&str, u8)] = &[
    // Cloudflare — https://www.cloudflare.com/ips-v4
    ("173.245.48.0", 20),
    ("103.21.244.0", 22),
    ("103.22.200.0", 22),
    ("103.31.4.0", 22),
    ("141.101.64.0", 18),
    ("108.162.192.0", 18),
    ("190.93.240.0", 20),
    ("188.114.96.0", 20),
    ("197.234.240.0", 22),
    ("198.41.128.0", 17),
    ("162.158.0.0", 15),
    ("104.16.0.0", 13),
    ("104.24.0.0", 14),
    ("172.64.0.0", 13),
    ("131.0.72.0", 22),
    // Fastly — https://api.fastly.com/public-ip-list
    ("23.235.32.0", 20),
    ("104.156.80.0", 20),
    ("151.101.0.0", 16),
    ("199.27.72.0", 21),
    // Amazon CloudFront
    ("13.32.0.0", 15),
    ("13.224.0.0", 14),
    ("54.182.0.0", 16),
    ("52.84.0.0", 15),
    ("143.204.0.0", 16),
    ("130.176.0.0", 16),
    // Bunny CDN
    ("185.75.40.0", 24),
    ("185.93.180.0", 22),
    ("212.71.250.0", 24),
];

/// Fast keyword-based CDN check — returns true if the hostname contains known
/// CDN provider names or a `cdn.` subdomain prefix.
fn keyword_cdn_check(host: &str) -> bool {
    let lower = host.to_lowercase();
    lower.contains("fastly")
        || lower.contains("cloudflare")
        || lower.contains("cloudfront")
        || lower.contains("akamai")
        || lower.contains("cdn.")
}

/// Check if an IP address falls within any of the known CDN CIDR ranges.
fn ip_in_cdn_range(ip: &IpAddr) -> bool {
    let ip_u32 = match ip {
        IpAddr::V4(v4) => u32::from_be_bytes(v4.octets()),
        IpAddr::V6(_) => return false,
    };
    for &(base_str, prefix_len) in CDN_CIDRS {
        let base: u32 = base_str
            .parse::<std::net::Ipv4Addr>()
            .map(|a| u32::from_be_bytes(a.octets()))
            .unwrap_or(0);
        let mask = if prefix_len == 0 {
            0
        } else {
            !0u32 << (32 - prefix_len)
        };
        if (ip_u32 & mask) == (base & mask) {
            return true;
        }
    }
    false
}

/// Multi-layer CDN detection:
///   1. Keyword check on `transport_host` (fast, static).
///   2. DNS-resolve `transport_host` / `sni` / `server` and check resolved IPs
///      against known CDN CIDR ranges.
pub async fn is_cdn_server(s: &ServerProfile) -> bool {
    // Layer 1: keyword check on transport_host and sni
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

    // Layer 2: DNS-based CIDR check
    // Collect all candidate domains to resolve
    let mut domains: Vec<&str> = Vec::new();
    if let Some(ref host) = s.transport_host {
        // Skip raw IP addresses
        if host.parse::<IpAddr>().is_err() && !domains.contains(&host.as_str()) {
            domains.push(host);
        }
    }
    if let Some(ref sni) = s.sni {
        if sni.parse::<IpAddr>().is_err()
            && !domains.contains(&sni.as_str())
        {
            domains.push(sni);
        }
    }
    // Check server field too if it's a domain (not IP)
    if s.server.parse::<IpAddr>().is_err() && !domains.contains(&s.server.as_str()) {
        domains.push(&s.server);
    }

    for domain in domains {
        if let Ok(addrs) = tokio::net::lookup_host(format!("{}:0", domain)).await {
            for addr in addrs {
                if ip_in_cdn_range(&addr.ip()) {
                    return true;
                }
            }
        }
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
pub enum VpnStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

pub struct VpnManager {
    process_mgr: ProcessManager,
    bin_dir: PathBuf,
    config_dir: PathBuf,
    installed: Arc<AtomicBool>,
}

impl VpnManager {
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

    pub async fn ensure_installed(&self, app_handle: &AppHandle) -> Result<PathBuf, VpnError> {
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
    ) -> Result<(), VpnError> {
        let logger = app_handle.state::<Mutex<Logger>>();
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Vpn,
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
            LogSource::Vpn,
            "Generating sing-box config...".into(),
        );
        let base_template = if routing_mode == "vpn" {
            include_str!("../../config/base-vpn.json")
        } else {
            include_str!("../../config/base.json")
        };
        let (config_json, _stats_port) = config::generate_config(
            base_template,
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
            LogSource::Vpn,
            format!("Config written to {}", config_path.display()),
        );

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Process,
            "Starting sing-box process...".into(),
        );
        let elevated = routing_mode == "vpn";
        self.process_mgr.start(&config_path, elevated).await?;

        // Set system proxy for system-proxy mode (TCP via system proxy, UDP via TUN if present)
        // VPN mode relies solely on TUN (auto_route + gVisor) for all traffic.
        // Local proxy mode (local-proxy) starts a mixed inbound on localhost without system proxy.
        if routing_mode == "proxy" || routing_mode == "system-proxy" {
            log(
                app_handle,
                &logger,
                LogLevel::Info,
                LogSource::Vpn,
                "Setting system proxy...".into(),
            );
            let sysproxy_cfg = sysproxy::Sysproxy {
                enable: true,
                host: String::from("127.0.0.1"),
                port: proxy_port,
                bypass: String::from("localhost;127.*;10.*;172.16.*;192.168.*"),
            };
            if let Err(e) = sysproxy_cfg.set_system_proxy() {
                log(
                    app_handle,
                    &logger,
                    LogLevel::Warning,
                    LogSource::Vpn,
                    format!("Failed to set system proxy: {e}"),
                );
            }
        }

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Vpn,
            "sing-box process started, traffic monitored from lib.rs".into(),
        );

        Ok(())
    }

    pub async fn stop(&self, app_handle: &AppHandle) -> Result<(), VpnError> {
        // Reset Clash API port so fetcher goes direct
        crate::vpn::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Relaxed);

        // Disable system proxy — best effort, don't block on it
        if let Ok(mut sysproxy) = sysproxy::Sysproxy::get_system_proxy() {
            if sysproxy.enable {
                sysproxy.enable = false;
                let _ = sysproxy.set_system_proxy();
            }
        }

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

    /// Atomically get + clear the tracked PID. Used by stop_vpn under the outer lock.
    pub fn clear_pid(&self) -> Option<u32> {
        self.process_mgr.clear_pid()
    }

    pub fn status(&self) -> VpnStatus {
        if let Some(pid) = self.process_mgr.pid() {
            if crate::is_process_alive(pid) {
                return VpnStatus::Connected;
            }
            // Process died but PID wasn't cleared yet — clean up
            self.process_mgr.clear_pid();
        }
        VpnStatus::Disconnected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_ip_in_cdn_cloudflare() {
        let ip = IpAddr::V4(Ipv4Addr::new(104, 16, 0, 1));
        assert!(ip_in_cdn_range(&ip), "104.16.0.1 should be Cloudflare");
    }

    #[test]
    fn test_ip_in_cdn_fastly() {
        let ip = IpAddr::V4(Ipv4Addr::new(151, 101, 1, 1));
        assert!(ip_in_cdn_range(&ip), "151.101.1.1 should be Fastly");
    }

    #[test]
    fn test_ip_in_cdn_cloudfront() {
        let ip = IpAddr::V4(Ipv4Addr::new(13, 32, 0, 1));
        assert!(ip_in_cdn_range(&ip), "13.32.0.1 should be CloudFront");
    }

    #[test]
    fn test_ip_not_in_cdn_range() {
        let ip = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        assert!(!ip_in_cdn_range(&ip), "8.8.8.8 is not a CDN");
    }

    #[test]
    fn test_ip_in_cdn_bunny() {
        let ip = IpAddr::V4(Ipv4Addr::new(185, 75, 40, 10));
        assert!(ip_in_cdn_range(&ip), "185.75.40.10 should be Bunny CDN");
    }

    #[test]
    fn test_ip_in_cdn_range_edge() {
        let ip = IpAddr::V4(Ipv4Addr::new(173, 245, 63, 255));
        assert!(
            ip_in_cdn_range(&ip),
            "173.245.63.255 should be Cloudflare (last in 173.245.48.0/20)"
        );
    }

    #[test]
    fn test_ip_not_in_cdn_range_outside() {
        let ip = IpAddr::V4(Ipv4Addr::new(173, 245, 64, 0));
        assert!(
            !ip_in_cdn_range(&ip),
            "173.245.64.0 should be outside Cloudflare 173.245.48.0/20"
        );
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
    fn test_keyword_cdn_not_cdn() {
        assert!(!keyword_cdn_check("example.com"));
        assert!(!keyword_cdn_check("my-server.io"));
    }

    #[test]
    fn test_keyword_cdn_akamai() {
        assert!(keyword_cdn_check("a1245.g.akamai.net"));
    }
}
