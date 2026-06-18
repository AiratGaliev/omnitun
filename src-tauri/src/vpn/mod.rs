mod config;
pub mod geoip;
pub mod installer;
mod process;

use installer::Installer;
use process::ProcessManager;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::AppHandle;
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

impl Default for ServerProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            protocol: String::new(),
            server: String::new(),
            port: 0,
            uuid: None,
            password: None,
            method: None,
            sni: None,
            flow: None,
            public_key: None,
            private_key: None,
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
    pub fn new(app_handle: AppHandle, data_dir: PathBuf) -> Self {
        let bin_dir = data_dir.join("binaries");
        let config_dir = data_dir.join("config");
        let binary_path = bin_dir.join(Installer::binary_name());

        Self {
            process_mgr: ProcessManager::new(app_handle.clone(), binary_path),
            bin_dir,
            config_dir,
            installed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn ensure_installed(&self) -> Result<PathBuf, VpnError> {
        if self.installed.load(Ordering::SeqCst) || Installer::is_installed(&self.bin_dir) {
            self.installed.store(true, Ordering::SeqCst);
            return Ok(self.bin_dir.join(Installer::binary_name()));
        }

        // Download and install the latest version
        let version = Installer::latest_version().await?;
        let binary_path = Installer::install(&self.bin_dir, &version).await?;
        self.installed.store(true, Ordering::SeqCst);
        Ok(binary_path)
    }

    pub async fn start(&self, profile: &ServerProfile) -> Result<(), VpnError> {
        self.ensure_installed().await?;

        let base_template = include_str!("../../config/base.json");
        let config_json = config::generate_config(base_template, profile)?;
        let config_path = config::write_config(&config_json, &self.config_dir)?;

        self.process_mgr.start(&config_path).await
    }

    pub async fn stop(&self) -> Result<(), VpnError> {
        self.process_mgr.stop().await
    }

    pub fn status(&self) -> VpnStatus {
        if self.process_mgr.is_running() {
            VpnStatus::Connected
        } else {
            VpnStatus::Disconnected
        }
    }
}
