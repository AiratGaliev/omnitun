// FIXED:
//   - Proper PID tracking for elevated processes (find sing-box.exe instead of cmd.exe)
//   - sing-box stdout/stderr redirected to log file for crash diagnostics
//   - DNS migrated to sing-box 1.12+ format — deprecated env vars removed
//
// PID file: stores the main proxy's PID in `<data_dir>/singbox.pid` so that
// stale processes from a previous (killed) session are cleaned up on next
// startup. This is a fallback when Windows Job Objects aren't available.
//
// ORPHAN GUARD:
//   Windows — RAII JobObject with KILL_ON_JOB_CLOSE. When the JobObject is
//   dropped (either via stop() or on crash/process exit), the OS kernel
//   immediately terminates sing-box. No handles leaked.
//   Linux   — prctl(PR_SET_PDEATHSIG, SIGKILL) inside pre_exec.
//   macOS   — process group kill via process_group(0).

use crate::proxy::{ErrorCode, ProxyError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(not(windows))]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

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

/// Write PID file without an AppHandle (for headless / MCP mode).
/// Same as write_pid_file but takes no _app parameter.
pub fn write_pid_file_headless(pid: u32) {
    let path = match PID_FILE.lock().ok().and_then(|g| g.clone()) {
        Some(p) => p,
        None => return,
    };
    if let Err(e) = std::fs::write(&path, pid.to_string()) {
        tracing::warn!(target: "process", "[pidfile] failed to write sing-box PID {pid}: {e}");
    }
}

/// Write the current PID to the PID file.
pub fn write_pid_file(pid: u32, _app: &tauri::AppHandle) {
    let path = match PID_FILE.lock().ok().and_then(|g| g.clone()) {
        Some(p) => p,
        None => return,
    };
    if let Err(e) = std::fs::write(&path, pid.to_string()) {
        tracing::warn!(target: "process", "[pidfile] failed to write sing-box PID {pid}: {e}");
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
pub fn cleanup_stale_pid(_app: &tauri::AppHandle) {
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
        tracing::info!(target: "process", "[pidfile] killing stale sing-box (PID {pid}) from previous session");
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

// ── Windows orphan guard ─────────────────────────────────────────────
// RAII JobObject: when dropped (normal stop or process crash), calls
// CloseHandle on the job, which triggers KILL_ON_JOB_CLOSE and the OS
// kernel immediately terminates all assigned processes (sing-box).

#[cfg(windows)]
pub(crate) mod orphan_guard {
    use std::os::windows::io::RawHandle;

    type Handle = *mut std::ffi::c_void;

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x00002000;

    #[repr(C)]
    #[derive(Default)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IO_COUNTERS {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        basic_limit_information: JOBOBJECT_BASIC_LIMIT_INFORMATION,
        io_info: IO_COUNTERS,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    /// RAII wrapper: when dropped, calls CloseHandle which triggers
    /// KILL_ON_JOB_CLOSE and the OS terminates all processes in the job.
    pub struct JobObject(pub RawHandle);
    // RawHandle = *mut c_void which is !Send + !Sync.
    // Safe because CloseHandle is thread-safe (just decrements kernel refcount).
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0 as Handle);
            }
        }
    }

    /// Create a job object with KILL_ON_JOB_CLOSE.
    /// Returns None on failure (fallback to taskkill/kill).
    pub(crate) fn create() -> Option<JobObject> {
        unsafe {
            let log_it = |msg: &str| {
                tracing::warn!(target: "process", "{msg}");
            };

            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                log_it(&format!(
                    "[orphan_guard] CreateJobObjectW failed: err={}",
                    GetLastError()
                ));
                return None;
            }

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let res = SetInformationJobObject(
                job,
                9, // JobObjectExtendedLimitInformation
                &mut info as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );

            if res == 0 {
                log_it(&format!(
                    "[orphan_guard] SetInformationJobObject(KILL_ON_JOB_CLOSE) failed: err={}",
                    GetLastError()
                ));
                CloseHandle(job);
                return None;
            }

            Some(JobObject(job as RawHandle))
        }
    }

    /// Assign a process to the job object. Returns true on success.
    pub(crate) fn assign(job: &JobObject, process_handle: RawHandle) -> bool {
        unsafe { AssignProcessToJobObject(job.0 as Handle, process_handle as Handle) != 0 }
    }

    extern "system" {
        fn CreateJobObjectW(lpJobAttributes: *const std::ffi::c_void, lpName: *const u16)
            -> Handle;
        fn CloseHandle(hObject: Handle) -> i32;
        fn GetLastError() -> u32;
        fn SetInformationJobObject(
            hJob: Handle,
            JobObjectInformationClass: i32,
            lpJobObjectInformation: *mut std::ffi::c_void,
            cbJobObjectInformationLength: u32,
        ) -> i32;
        fn AssignProcessToJobObject(hJob: Handle, hProcess: Handle) -> i32;
    }
}

// ── ProcessManager ───────────────────────────────────────────────────

pub struct ProcessManager {
    binary_path: PathBuf,
    pid: Arc<Mutex<Option<u32>>>,
    #[cfg(windows)]
    job: Arc<Mutex<Option<orphan_guard::JobObject>>>,
}

impl ProcessManager {
    pub fn new(binary_path: PathBuf) -> Self {
        Self {
            binary_path,
            pid: Arc::new(Mutex::new(None)),
            #[cfg(windows)]
            job: Arc::new(Mutex::new(None)),
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

    /// Set the PID (used by MCP server).
    pub fn set_pid(&self, pid: u32) {
        *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        crate::proxy::PROXY_PID.store(pid, std::sync::atomic::Ordering::Release);
    }

    /// Check if a PID is tracked (process may be stale — use status() for real liveness).
    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.pid.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    pub async fn start(
        &self,
        config_path: &Path,
        app: &tauri::AppHandle,
    ) -> Result<(), ProxyError> {
        let existing_pid = *self.pid.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pid) = existing_pid {
            // Process crash recovery: stale PID that no longer exists
            if !crate::is_process_alive(pid) {
                *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = None; // clear stale PID
            } else {
                return Err(ProxyError::process(
                    ErrorCode::ProcessAlreadyRunning,
                    "Proxy is already running",
                ));
            }
        }

        let config_str = config_path.to_str().ok_or_else(|| {
            ProxyError::process(ErrorCode::ProcessStartFailed, "Invalid config path")
        })?;

        let new_pid = self.launch(config_str, app).await?;
        *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(new_pid);
        crate::proxy::PROXY_PID.store(new_pid, std::sync::atomic::Ordering::Release);
        write_pid_file(new_pid, app);
        Ok(())
    }

    // Headless spawn — same as start() but no AppHandle.
    // Used by the MCP server. Does NOT write the PID file (the caller
    // should call write_pid_file_headless separately if needed).
    pub async fn start_headless(&self, config_path: &Path) -> Result<u32, ProxyError> {
        let existing_pid = *self.pid.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pid) = existing_pid {
            if !crate::is_process_alive(pid) {
                *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = None;
            } else {
                return Err(ProxyError::process(
                    ErrorCode::ProcessAlreadyRunning,
                    "Proxy is already running",
                ));
            }
        }

        let config_str = config_path.to_str().ok_or_else(|| {
            ProxyError::process(ErrorCode::ProcessStartFailed, "Invalid config path")
        })?;

        let binary = self.binary_path.to_str().ok_or_else(|| {
            ProxyError::process(ErrorCode::ProcessStartFailed, "Invalid binary path")
        })?;

        let stderr_path = std::path::Path::new(config_str)
            .parent()
            .map(|p| p.join("sing-box-stderr.log"))
            .unwrap_or_else(|| std::path::PathBuf::from("sing-box-stderr.log"));
        let stderr_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)
            .ok();
        let stderr_stdio = match stderr_file {
            Some(ref file) => std::process::Stdio::from(file.try_clone().unwrap_or_else(|_| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&stderr_path)
                    .expect("Failed to reopen stderr log file")
            })),
            None => std::process::Stdio::null(),
        };

        #[cfg(windows)]
        {
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            let mut cmd_breakaway = std::process::Command::new(binary);
            cmd_breakaway
                .args(["run", "-c", config_str])
                .stdout(std::process::Stdio::null())
                .stderr(stderr_stdio)
                .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);

            let child = match cmd_breakaway.spawn() {
                Ok(c) => c,
                Err(_) => std::process::Command::new(binary)
                    .args(["run", "-c", config_str])
                    .stdout(std::process::Stdio::null())
                    .stderr({
                        match std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&stderr_path)
                        {
                            Ok(f) => std::process::Stdio::from(f),
                            Err(_) => std::process::Stdio::null(),
                        }
                    })
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                    .map_err(|e| {
                        ProxyError::process(
                            ErrorCode::ProcessStartFailed,
                            format!("Failed to start sing-box: {e}"),
                        )
                    })?,
            };

            use std::os::windows::io::AsRawHandle;
            let process_handle = child.as_raw_handle();
            if let Some(job) = orphan_guard::create() {
                if orphan_guard::assign(&job, process_handle) {
                    *self.job.lock().unwrap_or_else(|e| e.into_inner()) = Some(job);
                }
            }

            let new_pid = child.id();
            *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(new_pid);
            crate::proxy::PROXY_PID.store(new_pid, std::sync::atomic::Ordering::Release);
            write_pid_file_headless(new_pid);
            Ok(new_pid)
        }

        #[cfg(not(windows))]
        {
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new(binary);
            cmd.args(["run", "-c", config_str])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0);

            #[cfg(target_os = "linux")]
            unsafe {
                cmd.pre_exec(|| {
                    extern "C" {
                        fn prctl(option: i32, arg2: u64, ...) -> i32;
                    }
                    prctl(1, 9);
                    Ok(())
                });
            }

            let child = cmd.spawn().map_err(|e| {
                ProxyError::process(
                    ErrorCode::ProcessStartFailed,
                    format!("Failed to start sing-box: {e}"),
                )
            })?;

            let new_pid = child.id();
            *self.pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(new_pid);
            crate::proxy::PROXY_PID.store(new_pid, std::sync::atomic::Ordering::Release);
            write_pid_file_headless(new_pid);
            Ok(new_pid)
        }
    }

    pub async fn stop(&self) -> Result<(), ProxyError> {
        remove_pid_file();

        // Windows: drop the JobObject first — this calls CloseHandle on the
        // job handle, which triggers KILL_ON_JOB_CLOSE and the OS kernel
        // immediately terminates sing-box. No need to wait.
        #[cfg(windows)]
        {
            let _ = self.job.lock().unwrap_or_else(|e| e.into_inner()).take();
        }

        let pid = {
            let mut guard = self.pid.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };

        if pid.is_none() {
            return Ok(());
        }

        let p = pid.unwrap();

        // Windows: fire-and-forget kill by PID as fallback (the JobObject
        // drop above already killed it, but this covers the case where
        // the job object wasn't created or assign failed).
        // Then poll for process exit with timeout so callers can safely
        // start a new instance without port/config conflicts.
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/pid", &p.to_string(), "/f"])
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
            tracing::warn!(target: "process", "[process] sing-box PID {p} still alive 3s after taskkill");
            Ok(())
        }

        // Unix: SIGTERM → wait → SIGKILL
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .arg(p.to_string())
                .output();
            tokio::time::sleep(Duration::from_secs(3)).await;

            if !crate::is_process_alive(p) {
                return Ok(());
            }

            std::process::Command::new("kill")
                .args(["-9", &p.to_string()])
                .output()
                .map_err(|e| {
                    ProxyError::process(
                        ErrorCode::ProcessKillFailed,
                        format!("Failed to kill process {p}: {e}"),
                    )
                })?;

            tokio::time::sleep(Duration::from_millis(500)).await;
            if crate::is_process_alive(p) {
                return Err(ProxyError::process(
                    ErrorCode::ProcessKillFailed,
                    format!("Could not kill sing-box (PID {p}) even with SIGKILL"),
                ));
            }

            Ok(())
        }
    }

    async fn launch(&self, config_path: &str, _app: &tauri::AppHandle) -> Result<u32, ProxyError> {
        let binary = self.binary_path.to_str().ok_or_else(|| {
            ProxyError::process(ErrorCode::ProcessStartFailed, "Invalid binary path")
        })?;

        // Open stderr log file next to config.json for diagnostics.
        let stderr_path = std::path::Path::new(config_path)
            .parent()
            .map(|p| p.join("sing-box-stderr.log"))
            .unwrap_or_else(|| std::path::PathBuf::from("sing-box-stderr.log"));
        let stderr_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)
            .ok();

        // Pre-compute stderr Stdio: use the log file if available, null otherwise.
        let stderr_stdio = match stderr_file {
            Some(ref file) => std::process::Stdio::from(file.try_clone().unwrap_or_else(|_| {
                // Fallback: create a new file handle. This should never fail
                // since we just opened it, but be defensive.
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&stderr_path)
                    .expect("Failed to reopen stderr log file")
            })),
            None => std::process::Stdio::null(),
        };

        #[cfg(windows)]
        {
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;

            // First try: spawn with BREAKAWAY so the process leaves the
            // parent job (if any — e.g. running under a terminal's job).
            // This lets us assign our own job object with KILL_ON_JOB_CLOSE.
            let mut cmd_breakaway = std::process::Command::new(binary);
            cmd_breakaway
                .args(["run", "-c", config_path])
                .stdout(std::process::Stdio::null())
                .stderr(stderr_stdio)
                .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);

            let child = match cmd_breakaway.spawn() {
                Ok(c) => c,
                Err(_) => {
                    // Fallback: some systems/environments block breakaway.
                    // Spawn without it — AssignProcessToJobObject may still
                    // succeed, and if not, we fall back to taskkill on stop.
                    std::process::Command::new(binary)
                        .args(["run", "-c", config_path])
                        .stdout(std::process::Stdio::null())
                        .stderr({
                            // Re-open stderr log for the fallback process
                            match std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&stderr_path)
                            {
                                Ok(f) => std::process::Stdio::from(f),
                                Err(_) => std::process::Stdio::null(),
                            }
                        })
                        .creation_flags(CREATE_NO_WINDOW)
                        .spawn()
                        .map_err(|e| {
                            ProxyError::process(
                                ErrorCode::ProcessStartFailed,
                                format!("Failed to start sing-box: {e}"),
                            )
                        })?
                }
            };

            // Assign the child process to a kill-on-close job object.
            use std::os::windows::io::AsRawHandle;
            let process_handle = child.as_raw_handle();
            if let Some(job) = orphan_guard::create() {
                if orphan_guard::assign(&job, process_handle) {
                    *self.job.lock().unwrap_or_else(|e| e.into_inner()) = Some(job);
                }
                // If assign fails, the JobObject is dropped immediately,
                // which calls CloseHandle but the job has no processes
                // assigned, so nothing happens. We fall back to taskkill.
            }

            Ok(child.id())
        }

        #[cfg(not(windows))]
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

            let child = cmd.spawn().map_err(|e| {
                ProxyError::process(
                    ErrorCode::ProcessStartFailed,
                    format!("Failed to start sing-box: {e}"),
                )
            })?;

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
        {
            let mut guard = mgr.pid.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(12345);
        }
        assert_eq!(mgr.pid(), Some(12345));

        let taken = mgr.clear_pid();
        assert_eq!(taken, Some(12345));
        assert!(mgr.pid().is_none(), "PID should be None after clear_pid");

        assert!(mgr.clear_pid().is_none());
    }

    #[tokio::test]
    async fn test_stop_clears_pid() {
        let mgr = ProcessManager::new(PathBuf::from("sing-box"));
        {
            let mut guard = mgr.pid.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(999999999);
        }
        assert!(mgr.is_running());

        let result = mgr.stop().await;
        assert!(result.is_ok());
        assert!(!mgr.is_running());
    }
}
