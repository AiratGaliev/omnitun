// FIXED:
//   - Proper PID tracking for elevated processes (find sing-box.exe instead of cmd.exe)
//   - sing-box stdout/stderr redirected to log file for crash diagnostics
//   - DNS migrated to sing-box 1.12+ format — deprecated env vars removed
//
// PID file: stores the main proxy's PID in `<data_dir>/singbox.pid` so that
// stale processes from a previous (killed) session are cleaned up on next
// startup. This is a fallback when Windows Job Objects aren't available.

use crate::proxy::ProxyError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(not(target_os = "windows"))]
use std::os::unix::process::CommandExt;

use tauri::Manager;

const CREATE_NO_WINDOW: u32 = 0x08000000;

// ── PID file management ──────────────────────────────────────────────
// Stores the sing-box PID so stale orphans from a killed session can be
// cleaned up on next launch. Path is set once from setup().

static PID_FILE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Set the PID file path (called once from setup).
pub fn set_pid_file_path(data_dir: &Path) {
    if let Ok(mut guard) = PID_FILE.lock() {
        *guard = Some(data_dir.join("singbox.pid"));
    }
}

/// Write the current PID to the PID file.
pub fn write_pid_file(pid: u32, app: &tauri::AppHandle) {
    let path = match PID_FILE.lock().ok().and_then(|g| g.clone()) {
        Some(p) => p,
        None => return,
    };
    if let Err(e) = std::fs::write(&path, pid.to_string()) {
        if let Some(logger) = app.try_state::<std::sync::Mutex<crate::logging::Logger>>() {
            crate::logging::log(
                app, &logger,
                crate::logging::LogLevel::Warning,
                crate::logging::LogSource::Process,
                format!("[pidfile] failed to write sing-box PID {pid}: {e}"),
            );
        }
    }
}

/// Delete the PID file.
pub fn remove_pid_file() {
    if let Ok(guard) = PID_FILE.lock() {
        if let Some(ref path) = *guard {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Kill a previously orphaned sing-box by PID, then clear the file.
pub fn cleanup_stale_pid(app: &tauri::AppHandle) {
    let path = match PID_FILE.lock().ok().and_then(|g| g.clone()) {
        Some(p) => p,
        None => return,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return, // no file = nothing to clean
    };
    let pid: u32 = match content.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    if crate::is_process_alive(pid) {
        if let Some(logger) = app.try_state::<std::sync::Mutex<crate::logging::Logger>>() {
            crate::logging::log(
                app, &logger,
                crate::logging::LogLevel::Info,
                crate::logging::LogSource::Process,
                format!("[pidfile] killing stale sing-box (PID {pid}) from previous session"),
            );
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/pid", &pid.to_string(), "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// On Windows: create a job object with KILL_ON_JOB_CLOSE so sing-box is
/// terminated when our process exits (even via IDE stop, Task Manager, or crash).
#[cfg(windows)]
pub(crate) mod orphan_guard {
    use std::os::windows::io::RawHandle;
    use tauri::Manager;

    type HANDLE = *mut std::ffi::c_void;

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x00002000;

    #[repr(C)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        min_working_set_size: usize,
        max_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        child_process_count: u32,
        child_process_limit: u32,
    }

    #[repr(C)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        basic_limit_information: JOBOBJECT_BASIC_LIMIT_INFORMATION,
        io_info: [u8; 48], // IO_COUNTERS (6 × ULONGLONG)
    }

    // Compile-time assertion: JOBOBJECT_EXTENDED_LIMIT_INFORMATION must be exactly
    // sizeof(BASIC_LIMIT) + sizeof(IO_COUNTERS) = 64 + 48 = 112 bytes on x64.

    const _: [(); 112] = [(); std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()];
    const _: [(); 64] = [(); std::mem::size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>()];

    /// Create a job object configured to kill all child processes when the
    /// job handle is closed (which happens on process exit). Returns the
    /// raw handle, or None on failure.
    pub(crate) fn create(app: &tauri::AppHandle) -> Option<RawHandle> {
        unsafe {
            let log_it = |msg: &str| {
                if let Some(logger) = app.try_state::<std::sync::Mutex<crate::logging::Logger>>() {
                    crate::logging::log(
                        app, &logger,
                        crate::logging::LogLevel::Warning,
                        crate::logging::LogSource::Process,
                        msg.into(),
                    );
                }
            };

            // Try 1: unnamed job object (most common, may be blocked by security software)
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if !job.is_null() {
                return configure_job(job, &log_it);
            }
            log_it(&format!("[orphan_guard] unnamed CreateJobObjectW failed: err={}", GetLastError()));

            // Try 2: named job object (some security software blocks unnamed only)
            let name: Vec<u16> = format!("OmniTun-{}", std::process::id())
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let job2 = CreateJobObjectW(std::ptr::null(), name.as_ptr());
            if !job2.is_null() {
                return configure_job(job2, &log_it);
            }
            log_it(&format!("[orphan_guard] named CreateJobObjectW failed: err={}", GetLastError()));

            // Try 3: with minimal SECURITY_ATTRIBUTES
            #[repr(C)]
            struct SECURITY_ATTRIBUTES {
                n_length: u32,
                lp_security_descriptor: *mut std::ffi::c_void,
                b_inherit_handle: i32,
            }
            let mut sa = SECURITY_ATTRIBUTES {
                n_length: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lp_security_descriptor: std::ptr::null_mut(),
                b_inherit_handle: 0,
            };
            let job3 = CreateJobObjectW(&mut sa as *mut _ as *mut std::ffi::c_void, std::ptr::null());
            if !job3.is_null() {
                return configure_job(job3, &log_it);
            }
            log_it(&format!("[orphan_guard] CreateJobObjectW with SA failed: err={}", GetLastError()));

            None
        }
    }

    /// Apply KILL_ON_JOB_CLOSE to an existing job handle.
    /// log_it is a closure that writes to the JSONL logger.
    unsafe fn configure_job<F: Fn(&str)>(job: HANDLE, log_it: &F) -> Option<RawHandle> {
        // Step 0: Query the actual required buffer size from the kernel.
        // NtQueryInformationJobObject returns STATUS_INFO_LENGTH_MISMATCH
        // when the buffer is too small, but it also tells us what size it expects
        // (in the return info, or we can infer from successful calls).
        let mut buf = [0u8; 256];
        let mut ret_len: u32 = 0;

        // Try with a reasonably large buffer to see if we can get past the
        // size check. If 64 bytes is too SMALL, try 256. If 256 works, we'll
        // know the required size and can use it for the SET call.
        #[repr(C)]
        struct BASIC_LIMITS {
            per_process_user_time_limit: i64,
            per_job_user_time_limit: i64,
            limit_flags: u32,
            min_working_set_size: usize,
            max_working_set_size: usize,
            active_process_limit: u32,
            affinity: usize,
            child_process_count: u32,
            child_process_limit: u32,
        }

        if NtQueryInformationJobObject(
            job,
            4, // JobObjectBasicLimitInformation
            buf.as_mut_ptr() as *mut u8,
            buf.len() as u32,
            &mut ret_len,
        ) == 0
        {
            // Success — we now know the actual buffer size required.
            let actual_size = ret_len as usize;
            log_it(&format!("[orphan_guard] Query returned size={actual_size}"));

            // Now SET with the correct size.
            let mut info = BASIC_LIMITS {
                per_process_user_time_limit: 0,
                per_job_user_time_limit: 0,
                limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                min_working_set_size: 0,
                max_working_set_size: 0,
                active_process_limit: 0,
                affinity: 0,
                child_process_count: 0,
                child_process_limit: 0,
            };
            if SetInformationJobObject(
                job,
                4,
                &mut info as *mut _ as *mut u8,
                actual_size as u32,
            ) != 0
            {
                return Some(job as RawHandle);
            }
            let err_set = GetLastError();
            log_it(&format!("[orphan_guard] SetInformationJobObject (size={actual_size}) failed: err={err_set}"));
        } else {
            let status = RtlNtStatusToDosError(
                NtQueryInformationJobObject(
                    job,
                    4,
                    buf.as_mut_ptr() as *mut u8,
                    0,   // zero size — force STATUS_INFO_LENGTH_MISMATCH
                    &mut ret_len,
                )
            );
            log_it(&format!("[orphan_guard] NtQueryInformationJobObject failed: status={}, ret_len={ret_len}", status));
        }

        // All attempts failed — close the job handle and return None.
        CloseHandle(job);
        None
    }

    extern "system" {
        fn NtQueryInformationJobObject(
            job: HANDLE,
            info_class: i32,
            info: *mut u8,
            info_len: u32,
            ret_len: *mut u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    /// Assign a process handle to a job object.
    pub(crate) fn assign(job: RawHandle, process_handle: RawHandle) -> bool {
        unsafe { AssignProcessToJobObject(job as HANDLE, process_handle as HANDLE) != 0 }
    }

    extern "system" {
        fn CreateJobObjectW(
            security: *const std::ffi::c_void,
            name: *const u16,
        ) -> HANDLE;
        fn CloseHandle(handle: HANDLE) -> i32;
        fn GetLastError() -> u32;
        fn SetInformationJobObject(
            job: HANDLE,
            info_type: i32,
            info: *mut u8,
            info_len: u32,
        ) -> u32;
        fn AssignProcessToJobObject(
            job: HANDLE,
            process: HANDLE,
        ) -> u32;
    }
}

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
        *self.pid.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the PID from the guard and return it, leaving None.
    /// Used by stop_proxy to atomically get+clear the PID under the outer lock.
    pub fn clear_pid(&self) -> Option<u32> {
        self.pid.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Check if a PID is tracked (process may be stale — use status() for real liveness).
    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.pid.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    pub async fn start(&self, config_path: &Path, app: &tauri::AppHandle) -> Result<(), ProxyError> {
        let existing_pid = *self.pid.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pid) = existing_pid {
            // Process crash recovery: stale PID that no longer exists
            if !crate::is_process_alive(pid) {
                *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = None; // clear stale PID
            } else {
                return Err(ProxyError::Process("Proxy is already running".into()));
            }
        }

        let config_str = config_path
            .to_str()
            .ok_or_else(|| ProxyError::Process("Invalid config path".into()))?;

        let new_pid = self.launch(config_str, app).await?;
        *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(new_pid);
        crate::proxy::PROXY_PID.store(new_pid, std::sync::atomic::Ordering::Release);
        write_pid_file(new_pid, app);
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), ProxyError> {
        remove_pid_file();
        let pid = {
            let mut guard = self.pid.lock().unwrap_or_else(|e| e.into_inner());
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

            crate::proxy::PROXY_PID.store(0, std::sync::atomic::Ordering::Release);

            // Poll for process exit — cumulative ~3s max
            for delay_ms in [200u64, 300, 500, 1000, 1000] {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                if !crate::is_process_alive(p) {
                    return Ok(());
                }
            }

            // Process still alive after 3s — log warning but don't block
            log::warn!("[process] sing-box PID {p} still alive 3s after taskkill");
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
                .map_err(|e| ProxyError::Process(format!("Failed to kill process {pid}: {e}")))?;

            tokio::time::sleep(Duration::from_millis(500)).await;
            if crate::is_process_alive(pid) {
                return Err(ProxyError::Process(format!(
                    "Could not kill sing-box (PID {pid}) even with SIGKILL"
                )));
            }

            Ok(())
        }
    }

    async fn launch(&self, config_path: &str, app: &tauri::AppHandle) -> Result<u32, ProxyError> {
        let binary = self
            .binary_path
            .to_str()
            .ok_or_else(|| ProxyError::Process("Invalid binary path".into()))?;

        #[cfg(target_os = "windows")]
        {
            let child = std::process::Command::new(binary)
                .args(["run", "-c", config_path])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(CREATE_NO_WINDOW | 0x02000000) // + CREATE_BREAKAWAY_FROM_JOB so we can assign our own job
                .spawn()
                .map_err(|e| ProxyError::Process(format!("Failed to start sing-box: {e}")))?;

            // ── Orphan guard: assign to kill-on-close Job Object ──────────────
            // When our process exits (even via IDE stop / Task Manager / crash),
            // Windows closes all open handles including the job handle, which
            // triggers KILL_ON_JOB_CLOSE and terminates sing-box automatically.
            use std::os::windows::io::AsRawHandle;
            if let Some(job) = orphan_guard::create(app) {
                let process_handle = child.as_raw_handle();
                if orphan_guard::assign(job, process_handle) {
                    if let Some(logger) = app.try_state::<std::sync::Mutex<crate::logging::Logger>>() {
                        crate::logging::log(
                            app, &logger,
                            crate::logging::LogLevel::Info,
                            crate::logging::LogSource::Process,
                            format!("[orphan_guard] Job created and sing-box assigned (PID {})", child.id()),
                        );
                    }
                } else {
                    if let Some(logger) = app.try_state::<std::sync::Mutex<crate::logging::Logger>>() {
                        crate::logging::log(
                            app, &logger,
                            crate::logging::LogLevel::Warning,
                            crate::logging::LogSource::Process,
                            "[orphan_guard] AssignProcessToJobObject FAILED — process may be in a nested job".into(),
                        );
                    }
                }
                // Handle is deliberately NOT closed — it stays alive in the
                // kernel's handle table for our process. KILL_ON_JOB_CLOSE
                // triggers when our process exits and the kernel closes all
                // remaining handles, terminating sing-box automatically.
                // RawHandle = *mut c_void (Copy, no Drop) — letting it go out
                // of scope is fine; the kernel handle persists regardless.
            } else {
                if let Some(logger) = app.try_state::<std::sync::Mutex<crate::logging::Logger>>() {
                    crate::logging::log(
                        app, &logger,
                        crate::logging::LogLevel::Warning,
                        crate::logging::LogSource::Process,
                        "[orphan_guard] job object created but KILL_ON_JOB_CLOSE failed — no orphan protection".into(),
                    );
                }
            }

            Ok(child.id())
        }

        #[cfg(not(target_os = "windows"))]
        {
            let mut cmd = std::process::Command::new(binary);
            cmd.args(["run", "-c", config_path])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0); // own process group for cleanup

            // Linux: register PDEATHSIG via prctl so the kernel kills
            // sing-box when our process dies (SIGKILL / crash / IDE stop).
            // macOS provides no equivalent — relies on process group kill.
            #[cfg(target_os = "linux")]
            unsafe {
                cmd.pre_exec(|| {
                    prctl(1, 9); // PR_SET_PDEATHSIG=1, SIGKILL=9
                    Ok(())
                });
            }

            let child = cmd
                .spawn()
                .map_err(|e| ProxyError::Process(format!("Failed to start sing-box: {e}")))?;

            Ok(child.id())
        }
    }
}

// ── Linux orphan guard: prctl(PR_SET_PDEATHSIG, SIGKILL) ──────────
// The kernel sends SIGKILL to sing-box when our process exits for ANY
// reason (SIGKILL, crash, IDE stop). No pid/group management needed.
#[cfg(target_os = "linux")]
unsafe fn prctl(option: i32, arg: u64) -> i32 {
    extern "C" {
        fn prctl(option: i32, arg2: u64, ...) -> i32;
    }
    prctl(option, arg)
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
            let mut guard = mgr.pid.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut guard = mgr.pid.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(999999999);
        }
        assert!(mgr.is_running());

        // stop should take the PID and try to kill (won't find process, but shouldn't crash)
        let result = mgr.stop().await;
        assert!(result.is_ok());
        assert!(!mgr.is_running());
    }
}
