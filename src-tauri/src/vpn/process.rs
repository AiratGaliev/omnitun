use crate::vpn::VpnError;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct ProcessManager {
    binary_path: PathBuf,
    running: Arc<AtomicBool>,
}

impl ProcessManager {
    pub fn new(binary_path: PathBuf) -> Self {
        Self {
            binary_path,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn start(&self, config_path: &PathBuf) -> Result<(), VpnError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(VpnError::Process("VPN is already running".into()));
        }

        let config_str = config_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid config path".into()))?;

        #[cfg(target_os = "windows")]
        {
            self.start_windows(config_str).await?;
        }

        #[cfg(not(target_os = "windows"))]
        {
            self.start_unix(config_str).await?;
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

    #[cfg(target_os = "windows")]
    async fn start_windows(&self, config_path: &str) -> Result<(), VpnError> {
        let binary = self
            .binary_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid binary path".into()))?;

        let ps_command = format!(
            "Start-Process -FilePath '{}' -ArgumentList 'run','-c','{}' -Verb RunAs -WindowStyle Hidden",
            binary.replace("\'", "''"),
            config_path.replace("\'", "''")
        );

        let output = std::process::Command::new("powershell")
            .args(["-Command", &ps_command])
            .output()
            .map_err(|e| VpnError::Process(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(VpnError::Process(format!("UAC elevation failed: {stderr}")));
        }

        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    async fn start_unix(&self, config_path: &str) -> Result<(), VpnError> {
        let binary = self
            .binary_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid binary path".into()))?;

        let status = std::process::Command::new("pkexec")
            .args([binary, "run", "-c", config_path])
            .spawn()
            .or_else(|_| {
                std::process::Command::new("sudo")
                    .args(["-n", binary, "run", "-c", config_path])
                    .spawn()
            })
            .map_err(|e| VpnError::Process(format!("Failed to start sing-box: {e}")))?;

        drop(status);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn test_initial_state_not_running() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        assert!(!mgr.is_running());
    }

    #[tokio::test]
    async fn test_stop_when_not_running_is_noop() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        let result = mgr.stop().await;
        assert!(result.is_ok());
        assert!(!mgr.is_running());
    }

    #[tokio::test]
    async fn test_double_start_returns_error() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        mgr.running.store(true, Ordering::SeqCst);
        let result = mgr.start(&PathBuf::from("config.json")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("VPN is already running"));
    }

    #[tokio::test]
    async fn test_running_flag_persists_after_forced_set() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        assert!(!mgr.is_running());
        mgr.running.store(true, Ordering::SeqCst);
        assert!(mgr.is_running());
        mgr.running.store(false, Ordering::SeqCst);
        assert!(!mgr.is_running());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_invalid_config_path_returns_error() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        let bad_path = PathBuf::from(OsStr::from_bytes(&[0xFF]));
        let result = mgr.start(&bad_path).await;
        assert!(result.is_err());
    }
}
