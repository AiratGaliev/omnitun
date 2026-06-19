mod config;
pub mod geoip;
pub mod installer;
mod process;

use crate::logging::{log, LogLevel, Logger, LogSource};
use installer::Installer;
use process::ProcessManager;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
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

    pub async fn start(&self, app_handle: &AppHandle, profile: &ServerProfile) -> Result<(), VpnError> {
        let logger = app_handle.state::<Mutex<Logger>>();
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Vpn,
            "Checking sing-box installation...".into(),
        );
        self.ensure_installed(app_handle).await?;

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Vpn,
            "Generating sing-box config...".into(),
        );
        let base_template = include_str!("../../config/base.json");
        let config_json = config::generate_config(base_template, &self.config_dir, profile)?;
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
        self.process_mgr.start(&config_path).await
    }

    pub async fn stop(&self, app_handle: &AppHandle) -> Result<(), VpnError> {
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
