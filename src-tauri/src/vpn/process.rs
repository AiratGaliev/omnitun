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

    pub async fn start(&self, config_path: &Path, elevated: bool) -> Result<(), VpnError> {
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

        let new_pid = self.launch(config_str, elevated).await?;
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

        // Windows: fire-and-forget kill by PID only (no /im — avoids nuking batch test instances)
        #[cfg(target_os = "windows")]
        {
            let p = pid.unwrap();
            // Fire both non-elevated and elevated kill in parallel, then return
            let _ = std::process::Command::new("taskkill")
                .args(["/pid", &p.to_string(), "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            // Elevated kill via PowerShell (handles UAC-elevated processes)
            let _ = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command",
                    &format!("Start-Process 'taskkill' -Verb RunAs -WindowStyle Hidden -ArgumentList '/pid','{}','/f' | Out-Null", p)])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            crate::vpn::VPN_PID.store(0, std::sync::atomic::Ordering::Relaxed);
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

    async fn launch(&self, config_path: &str, elevated: bool) -> Result<u32, VpnError> {
        let binary = self
            .binary_path
            .to_str()
            .ok_or_else(|| VpnError::Process("Invalid binary path".into()))?;

        #[cfg(target_os = "windows")]
        {
            // Compute log file paths for output redirection (non-elevated mode)
            let config_dir = std::path::Path::new(config_path)
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let log_path_stdout = config_dir.join("sing-box-stdout.log");
            let log_path_stderr = config_dir.join("sing-box-stderr.log");

            if elevated {
                // FIX: Launch sing-box elevated with proper PID tracking.
                // Writes a .ps1 launcher script to disk, then runs it elevated.
                // The script captures sing-box.exe PID via Start-Process -PassThru
                // and writes it to a known file for the app to read.
                let pid_file = config_dir.join("sing-box.pid");
                let pid_file_str = pid_file.to_str().unwrap_or("sing-box.pid");
                let launcher_path = std::env::temp_dir().join("omnitun-singbox-launcher.ps1");

                let launcher_content = format!(
                    r#"$p = Start-Process -FilePath '{}' -ArgumentList 'run','-c','{}' -PassThru -WindowStyle Hidden -RedirectStandardError '{}'; $p.Id | Out-File -FilePath '{}' -Encoding ASCII"#,
                    binary,
                    config_path,
                    log_path_stderr.to_str().unwrap_or("NUL"),
                    pid_file_str,
                );

                std::fs::write(&launcher_path, &launcher_content).map_err(|e| {
                    VpnError::Process(format!("Failed to write launcher script: {e}"))
                })?;

                let output = std::process::Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-ExecutionPolicy", "Bypass",
                        "-Command",
                        &format!(
                            "Start-Process 'powershell' -Verb RunAs -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'",
                            launcher_path.to_str().unwrap_or(""),
                        ),
                    ])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()
                    .map_err(|e| VpnError::Process(format!("Elevation launch failed: {e}")))?;

                if !output.status.success() {
                    let err = String::from_utf8_lossy(&output.stderr);
                    return Err(VpnError::Process(format!(
                        "User denied elevation or elevation failed: {err}"
                    )));
                }

                // Read PID from the file written by the elevated launcher.
                // Poll up to ~15 seconds to allow for UAC prompt response time.
                let pid = {
                    let mut found_pid = None;
                    // Cumulative wait: 500+1000+2000+3000+4000+5000 = 15500ms
                    for delay_ms in [500u64, 1000, 2000, 3000, 4000, 5000] {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        if pid_file.exists() {
                            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                                if let Ok(p) = content.trim().parse::<u32>() {
                                    if p > 0 {
                                        found_pid = Some(p);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    found_pid.ok_or_else(|| VpnError::Process(
                        "Could not obtain sing-box PID after 15 seconds. The UAC prompt may have been denied, \
                         or the elevated PowerShell could not launch sing-box. Try running OmniTun as \
                         Administrator, or check Task Manager for hung sing-box.exe processes."
                            .into(),
                    ))?
                };
                // Clean up temporary files
                let _ = std::fs::remove_file(&pid_file);
                let _ = std::fs::remove_file(&launcher_path);

                Ok(pid)
            } else {
                // Non-elevated: launch sing-box directly with output redirection
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
        }

        #[cfg(not(target_os = "windows"))]
        {
            // Compute log file path
            let config_dir = std::path::Path::new(config_path)
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let log_path = config_dir.join("sing-box.log");

            if elevated {
                let child = std::process::Command::new("pkexec")
                    .args([binary, "run", "-c", config_path])
                    .stdout(std::fs::File::create(&log_path).ok())
                    .stderr(std::process::Stdio::from(
                        std::fs::File::create(&log_path).ok().unwrap(),
                    ))
                    .spawn()
                    .or_else(|_| {
                        std::process::Command::new("sudo")
                            .args(["-n", binary, "run", "-c", config_path])
                            .stdout(std::fs::File::create(&log_path).ok())
                            .stderr(std::process::Stdio::from(
                                std::fs::File::create(&log_path).ok().unwrap(),
                            ))
                            .spawn()
                    })
                    .map_err(|e| {
                        VpnError::Process(format!("Failed to start sing-box elevated: {e}"))
                    })?;

                Ok(child.id())
            } else {
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
}
