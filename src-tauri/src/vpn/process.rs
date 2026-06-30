// FIXED:
//   - Proper PID tracking for elevated processes (find sing-box.exe instead of cmd.exe)
//   - sing-box stdout/stderr redirected to log file for crash diagnostics
//   - DNS migrated to sing-box 1.12+ format — deprecated env vars removed

use crate::vpn::VpnError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
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

    pub fn pid(&self) -> Option<u32> {
        *self.pid.lock().unwrap()
    }

    /// Take the PID from the guard and return it, leaving None.
    /// Used by stop_vpn to atomically get+clear the PID under the outer lock.
    pub fn clear_pid(&self) -> Option<u32> {
        self.pid.lock().unwrap().take()
    }

    /// Check if a PID is tracked (process may be stale — use status() for real liveness).
    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.pid.lock().unwrap().is_some()
    }

    pub async fn start(&self, config_path: &Path) -> Result<(), VpnError> {
        let existing_pid = *self.pid.lock().unwrap();
        if let Some(pid) = existing_pid {
            // Process crash recovery: stale PID that no longer exists
            if !crate::is_process_alive(pid) {
                *self.pid.lock().unwrap() = None; // clear stale PID
            } else {
                return Err(VpnError::Process("VPN is already running".into()));
            }
        }

        let config_str = config_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid config path".into()))?;

        let new_pid = self.launch(config_str).await?;
        *self.pid.lock().unwrap() = Some(new_pid);
        crate::vpn::VPN_PID.store(new_pid, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), VpnError> {
        let pid = {
            let mut guard = self.pid.lock().unwrap();
            guard.take()
        };

        if pid.is_none() {
            return Ok(());
        }

        let p = pid.unwrap();

        // Windows: fire-and-forget kill by PID only (no /im — avoids nuking batch test instances).
        // Then poll for process exit with timeout so callers (e.g. reconnect) can safely
        // start a new instance without port/config conflicts.
        #[cfg(target_os = "windows")]
        {
            // Fire both non-elevated and elevated kill
            let _ = std::process::Command::new("taskkill")
                .args(["/pid", &p.to_string(), "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            let _ = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command",
                    &format!("Start-Process 'taskkill' -Verb RunAs -WindowStyle Hidden -ArgumentList '/pid','{}','/f' | Out-Null", p)])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            crate::vpn::VPN_PID.store(0, std::sync::atomic::Ordering::Relaxed);

            // Poll for process exit — cumulative ~3s max
            for delay_ms in [200u64, 300, 500, 1000, 1000] {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                if !crate::is_process_alive(p) {
                    return Ok(());
                }
            }

            // Process still alive after 3s — log warning but don't block
            eprintln!("[warn] sing-box PID {p} still alive 3s after taskkill");
            Ok(())
        }

        // Unix: SIGTERM → wait → SIGKILL
        #[cfg(not(target_os = "windows"))]
        {
            let pid = pid.unwrap();
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .output();
            tokio::time::sleep(Duration::from_secs(3)).await;

            if !crate::is_process_alive(pid) {
                return Ok(());
            }

            std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output()
                .map_err(|e| VpnError::Process(format!("Failed to kill process {pid}: {e}")))?;

            tokio::time::sleep(Duration::from_millis(500)).await;
            if crate::is_process_alive(pid) {
                return Err(VpnError::Process(format!(
                    "Could not kill sing-box (PID {pid}) even with SIGKILL"
                )));
            }

            Ok(())
        }
    }

    async fn launch(&self, config_path: &str) -> Result<u32, VpnError> {
        let binary = self
            .binary_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid binary path".into()))?;

        #[cfg(target_os = "windows")]
        {
            // Compute log file paths for output redirection
            let config_dir = std::path::Path::new(config_path)
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let log_path_stdout = config_dir.join("sing-box-stdout.log");
            let log_path_stderr = config_dir.join("sing-box-stderr.log");

            // Launch sing-box directly with output redirection
            let child = std::process::Command::new(binary)
                .args(["run", "-c", config_path])
                // Redirect stdout/stderr to separate log files for crash diagnostics
                .stdout(match std::fs::File::create(&log_path_stdout) {
                    Ok(f) => f.into(),
                    Err(_) => std::process::Stdio::null(),
                })
                .stderr(match std::fs::File::create(&log_path_stderr) {
                    Ok(f) => std::process::Stdio::from(f),
                    Err(_) => std::process::Stdio::null(),
                })
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| VpnError::Process(format!("Failed to start sing-box: {e}")))?;

            Ok(child.id())
        }

        #[cfg(not(target_os = "windows"))]
        {
            // Compute log file path
            let config_dir = std::path::Path::new(config_path)
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let log_path = config_dir.join("sing-box.log");

            let child = std::process::Command::new(binary)
                .args(["run", "-c", config_path])
                .stdout(std::fs::File::create(&log_path).ok())
                .stderr(std::process::Stdio::from(
                    std::fs::File::create(&log_path).ok().unwrap(),
                ))
                .spawn()
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

    #[test]
    fn test_new_stores_binary_path() {
        let mgr = ProcessManager::new(PathBuf::from("/custom/path/sing-box"));
        // binary_path is not pub, but we can verify pid starts None
        assert!(mgr.pid().is_none());
    }

    #[test]
    fn test_pid_returns_none_initially() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        assert!(mgr.pid().is_none());
    }

    #[test]
    fn test_clear_pid_returns_none_when_empty() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        assert!(mgr.clear_pid().is_none());
        assert!(mgr.pid().is_none());
    }

    #[test]
    fn test_clear_pid_atomic_take() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        // Simulate setting a PID via the internal Arc<Mutex<Option<u32>>>
        {
            let mut guard = mgr.pid.lock().unwrap();
            *guard = Some(12345);
        }
        assert_eq!(mgr.pid(), Some(12345));

        // clear_pid should take it
        let taken = mgr.clear_pid();
        assert_eq!(taken, Some(12345));
        assert!(mgr.pid().is_none(), "PID should be None after clear_pid");

        // Second clear_pid returns None
        assert!(mgr.clear_pid().is_none());
    }

    #[tokio::test]
    async fn test_stop_clears_pid() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        // Set a fake PID (won't actually kill anything since it's 999999999)
        {
            let mut guard = mgr.pid.lock().unwrap();
            *guard = Some(999999999);
        }
        assert!(mgr.is_running());

        // stop should take the PID and try to kill (won't find process, but shouldn't crash)
        let result = mgr.stop().await;
        assert!(result.is_ok());
        assert!(!mgr.is_running());
    }
}
