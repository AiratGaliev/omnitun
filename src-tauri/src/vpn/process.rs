use crate::vpn::VpnError;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::Arc;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x08000000;

pub struct ProcessManager {
    binary_path: PathBuf,
    pid: Arc<Mutex<Option<u32>>>,
}

impl ProcessManager {
    pub fn new(binary_path: PathBuf) -> Self {
        Self {
            binary_path,
            pid: Arc::new(Mutex::new(None)),
        }
    }

    pub fn pid(&self) -> std::sync::MutexGuard<'_, Option<u32>> {
        self.pid.lock().unwrap()
    }

    pub fn is_running(&self) -> bool {
        self.pid.lock().unwrap().is_some()
    }

    pub async fn start(&self, config_path: &PathBuf) -> Result<(), VpnError> {
        {
            let pid_guard = self.pid.lock().unwrap();
            if pid_guard.is_some() {
                return Err(VpnError::Process("VPN is already running".into()));
            }
        }

        let config_str = config_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid config path".into()))?;

        let new_pid = self.launch(config_str).await?;
        *self.pid.lock().unwrap() = Some(new_pid);
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), VpnError> {
        let pid = {
            let mut guard = self.pid.lock().unwrap();
            guard.take()
        };

        if let Some(pid) = pid {
            #[cfg(target_os = "windows")]
            {
                // Graceful shutdown first
                let _ = std::process::Command::new("taskkill")
                    .args(["/pid", &pid.to_string()])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();

                tokio::time::sleep(Duration::from_secs(3)).await;

                // Force kill if still alive
                std::process::Command::new("taskkill")
                    .args(["/pid", &pid.to_string(), "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()
                    .map_err(|e| VpnError::Process(format!("Failed to stop process: {e}")))?;
            }

            #[cfg(not(target_os = "windows"))]
            {
                // Send SIGTERM
                let _ = std::process::Command::new("kill").arg(pid.to_string()).output();
                tokio::time::sleep(Duration::from_secs(3)).await;
                // Force kill if still alive
                std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output()
                    .map_err(|e| VpnError::Process(format!("Failed to kill process: {e}")))?;
            }
        }

        Ok(())
    }

    async fn launch(&self, config_path: &str) -> Result<u32, VpnError> {
        let binary = self
            .binary_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid binary path".into()))?;

        #[cfg(target_os = "windows")]
        {
            let child = std::process::Command::new(binary)
                .args(["run", "-c", config_path])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| VpnError::Process(format!("Failed to start sing-box: {e}")))?;

            Ok(child.id())
        }

        #[cfg(not(target_os = "windows"))]
        {
            let child = std::process::Command::new("pkexec")
                .args([binary, "run", "-c", config_path])
                .spawn()
                .or_else(|_| {
                    std::process::Command::new("sudo")
                        .args(["-n", binary, "run", "-c", config_path])
                        .spawn()
                })
                .map_err(|e| VpnError::Process(format!("Failed to start sing-box: {e}")))?;

            Ok(child.id())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        *mgr.pid.lock().unwrap() = Some(1);
        let result = mgr.start(&PathBuf::from("config.json")).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("VPN is already running")
        );
    }

    #[tokio::test]
    async fn test_running_flag_is_not_set_by_default() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        assert!(!mgr.is_running());
        *mgr.pid.lock().unwrap() = Some(1);
        assert!(mgr.is_running());
        *mgr.pid.lock().unwrap() = None;
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
