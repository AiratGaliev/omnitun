use crate::vpn::VpnError;
use std::path::{Path, PathBuf};
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

    pub async fn start(&self, config_path: &Path, elevated: bool) -> Result<(), VpnError> {
        {
            let mut pid_guard = self.pid.lock().unwrap();
            if let Some(pid) = *pid_guard {
                // Process crash recovery: stale PID that no longer exists
                if !crate::is_process_alive(pid) {
                    *pid_guard = None; // clear stale PID
                } else {
                    return Err(VpnError::Process("VPN is already running".into()));
                }
            }
        }

        let config_str = config_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid config path".into()))?;

        let new_pid = self.launch(config_str, elevated).await?;
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
                // Graceful shutdown first (WM_CLOSE)
                let _ = std::process::Command::new("taskkill")
                    .args(["/pid", &pid.to_string()])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();

                tokio::time::sleep(Duration::from_secs(3)).await;

                let output = std::process::Command::new("taskkill")
                    .args(["/pid", &pid.to_string(), "/f"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()
                    .map_err(|e| VpnError::Process(format!("Failed to stop process: {e}")))?;

                if !output.status.success() {
                    // Direct taskkill failed (likely access denied for elevated process).
                    // Retry via PowerShell with UAC elevation.
                    let _stderr = String::from_utf8_lossy(&output.stderr);
                    let ps_script = format!(
                        "Start-Process 'taskkill' -Verb RunAs -ArgumentList '/f','/pid','{}' -WindowStyle Hidden -Wait | Out-Null",
                        pid
                    );
                    let _ = std::process::Command::new("powershell")
                        .args(["-NoProfile", "-Command", &ps_script])
                        .creation_flags(CREATE_NO_WINDOW)
                        .output();
                }
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

    async fn launch(&self, config_path: &str, elevated: bool) -> Result<u32, VpnError> {
        let binary = self
            .binary_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid binary path".into()))?;

        #[cfg(target_os = "windows")]
        {
            if elevated {
                // Launch with admin privileges via UAC prompt
                let ps_script = format!(
                    "Start-Process '{}' -Verb RunAs -ArgumentList 'run','-c','{}' -WindowStyle Hidden -PassThru | Select-Object -ExpandProperty Id",
                    binary.replace('\'', "''"),
                    config_path.replace('\'', "''"),
                );
                let output = std::process::Command::new("powershell")
                    .args(["-NoProfile", "-Command", &ps_script])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()
                    .map_err(|e| VpnError::Process(format!("Elevation launch failed: {e}")))?;

                if output.status.success() {
                    let pid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    pid_str.parse::<u32>().map_err(|_| {
                        VpnError::Process("Failed to parse elevated PID".into())
                    })
                } else {
                    let err = String::from_utf8_lossy(&output.stderr);
                    Err(VpnError::Process(format!(
                        "User denied elevation or elevation failed: {err}"
                    )))
                }
            } else {
                let child = std::process::Command::new(binary)
                    .args(["run", "-c", config_path])
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                    .map_err(|e| VpnError::Process(format!("Failed to start sing-box: {e}")))?;

                Ok(child.id())
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            if elevated {
                let child = std::process::Command::new("pkexec")
                    .args([binary, "run", "-c", config_path])
                    .spawn()
                    .or_else(|_| {
                        std::process::Command::new("sudo")
                            .args(["-n", binary, "run", "-c", config_path])
                            .spawn()
                    })
                    .map_err(|e| {
                        VpnError::Process(format!("Failed to start sing-box elevated: {e}"))
                    })?;

                Ok(child.id())
            } else {
                let child = std::process::Command::new(binary)
                    .args(["run", "-c", config_path])
                    .spawn()
                    .map_err(|e| VpnError::Process(format!("Failed to start sing-box: {e}")))?;

                Ok(child.id())
            }
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
        // Use a PID that IS alive (the current test process)
        *mgr.pid.lock().unwrap() = Some(std::process::id());
        let result = mgr.start(&PathBuf::from("config.json"), false).await;
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
        let result = mgr.start(&bad_path, false).await;
        assert!(result.is_err());
    }
}
