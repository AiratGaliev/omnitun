mod config;
pub mod geoip;
pub mod installer;
mod process;

use crate::events::emit_traffic;
use crate::logging::{log, LogLevel, Logger, LogSource};
use installer::Installer;
use process::ProcessManager;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
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

    pub async fn start(&self, app_handle: &AppHandle, proxy_port: u16, profile: &ServerProfile) -> Result<(), VpnError> {
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
        let (config_json, stats_port) = config::generate_config(base_template, &self.config_dir, proxy_port, profile)?;
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

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Vpn,
            "sing-box process started, spawning traffic monitor...".into(),
        );

        // Spawn background task to poll sing-box clash_api for traffic stats
        let app_handle = app_handle.clone();
        let api_url = format!("http://127.0.0.1:{}/connections", stats_port);
        tokio::spawn(async move {
            let client = reqwest::Client::new();

            // Retry connecting — API starts slightly after sing-box
            for delay_ms in [200u64, 400, 800, 1500, 3000] {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                if client.get(&api_url).send().await.is_ok() {
                    break;
                }
            }

            let mut prev_up = 0u64;
            let mut prev_down = 0u64;
            let mut first = true;

            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;

                match client.get(&api_url).send().await {
                    Ok(resp) => match resp.json::<serde_json::Value>().await {
                        Ok(data) => {
                            let up = data["upload_total"].as_u64().unwrap_or(0);
                            let down = data["download_total"].as_u64().unwrap_or(0);

                            if !first && (up != prev_up || down != prev_down) {
                                let up_speed = up.saturating_sub(prev_up) as f64;
                                let down_speed = down.saturating_sub(prev_down) as f64;
                                emit_traffic(
                                    &app_handle,
                                    up_speed / 1_048_576.0,
                                    down_speed / 1_048_576.0,
                                );
                            }
                            first = false;
                            prev_up = up;
                            prev_down = down;
                        }
                        Err(_) => break, // JSON parse failed → sing-box stopped
                    },
                    Err(_) => break, // Connection refused → sing-box stopped
                }
            }
        });

        Ok(())
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
