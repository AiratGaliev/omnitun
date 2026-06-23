pub mod config;
pub mod geoip;
pub mod installer;
mod process;

use crate::logging::{log, LogLevel, Logger, LogSource};
use crate::settings::AppSettings;
use installer::Installer;
use process::ProcessManager;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

/// Current Clash API port (0 = not running). Set by config.rs on each start.
pub(crate) static CLASH_API_PORT: AtomicU16 = AtomicU16::new(0);

/// Last connected profile — stored for auto-reconnect on watchdog exit.
pub(crate) static LAST_PROFILE: std::sync::Mutex<Option<LastConnection>> = std::sync::Mutex::new(None);

/// Data needed to re-establish a connection after unexpected exit.
#[derive(Clone)]
pub struct LastConnection {
    pub all_servers: Vec<ServerProfile>,
    pub routing_mode: String,
    pub proxy_port: u16,
}
use thiserror::Error;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
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
            format!("Starting in {routing_mode} mode with {} server(s)...", profiles.len()),
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
        let (config_json, _stats_port) =
            config::generate_config(base_template, &self.config_dir, proxy_port, routing_mode, profiles, settings)?;
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

        // Disable system proxy
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
        *self.process_mgr.pid()
    }

    pub fn status(&self) -> VpnStatus {
        if self.process_mgr.is_running() {
            VpnStatus::Connected
        } else {
            VpnStatus::Disconnected
        }
    }
}
