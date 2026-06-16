use crate::vpn::VpnError;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::AppHandle;

pub struct ProcessManager {
    app_handle: AppHandle,
    binary_path: PathBuf,
    running: Arc<AtomicBool>,
}

impl ProcessManager {
    pub fn new(app_handle: AppHandle, binary_path: PathBuf) -> Self {
        Self {
            app_handle,
            binary_path,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn start(&self, config_path: &PathBuf) -> Result<(), VpnError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(VpnError::Process("Vpn is already running".into()));
        }

        let config_str = config_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid config path".into()))?;

        let binary = self
            .binary_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid binary path".into()))?;

        #[cfg(target_os = "windows")]
        {
            let ps_command = format!(
                "Start-Process -FilePath '{}' -ArgumentList 'run','-c','{}' -Verb RunAs -WindowStyle Hidden",
                binary.replace("\'", "''"),
                config_str.replace("\'", "''")
            );
            std::process::Command::new("powershell")
                .args(["-Command", &ps_command])
                .output()
                .map_err(|e| VpnError::Process(e.to_string()))?;
        }

        #[cfg(not(target_os = "windows"))]
        {
            std::process::Command::new("pkexec")
                .args([binary, "run", "-c", config_str])
                .spawn()
                .or_else(|_| {
                    std::process::Command::new("sudo")
                        .args(["-n", binary, "run", "-c", config_str])
                        .spawn()
                })
                .map_err(|e| VpnError::Process(format!("Failed to start: {e}")))?;
        }

        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), VpnError> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("taskkill")
                .args(["/f", "/im", "sing-box.exe"])
                .output()
                .map_err(|e| VpnError::Process(e.to_string()))?;
        }

        #[cfg(not(target_os = "windows"))]
        {
            std::process::Command::new("pkill")
                .args(["-f", "sing-box"])
                .output()
                .map_err(|e| VpnError::Process(e.to_string()))?;
        }

        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }
}
