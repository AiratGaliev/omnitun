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
use serde::{Deserialize, Serialize};
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

/// ID of the currently active (connected) server, for exit IP enrichment.
pub(crate) static ACTIVE_SERVER_ID: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

/// Data needed to re-establish a connection after unexpected exit.
#[derive(Clone)]
pub struct LastConnection {
    pub all_servers: Vec<ServerProfile>,
    pub routing_mode: String,
    pub proxy_port: u16,
}

#[derive(Debug, Error)]
pub enum ProxyError {
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

impl From<ProxyError> for String {
    fn from(e: ProxyError) -> Self {
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
        || lower.contains("worker")
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
        self.process_mgr.start(&config_path).await?;

        // Set system proxy for system-proxy mode.
        // Local-proxy mode starts a mixed inbound on localhost without system proxy.
        if routing_mode == "system-proxy" {
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

    pub async fn stop(&self, app_handle: &AppHandle) -> Result<(), ProxyError> {
        // Reset Clash API port so fetcher goes direct
        crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Relaxed);

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
