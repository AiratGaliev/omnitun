use crate::core::Runtime;
use crate::proxy::{ConnectionStatus, ProxyError, ServerProfile};
use crate::settings::AppSettings;

/// Core proxy manager — delegates to ProxyManager with Runtime abstraction.
///
/// NOTE: ProxyManager::start/stop require `&AppHandle` (Tauri-specific).
/// CoreProxyManager provides stubs for now; MCP server manages process
/// lifecycle directly via ProcessManager.
pub struct CoreProxyManager;

impl CoreProxyManager {
    /// Start the proxy — stub (not wired to ProxyManager yet).
    pub async fn start(
        _rt: &dyn Runtime,
        _proxy_port: u16,
        _routing_mode: &str,
        _profiles: &[ServerProfile],
        _settings: &AppSettings,
    ) -> Result<(), ProxyError> {
        Err(ProxyError::process(
            crate::proxy::ErrorCode::ProcessStartFailed,
            "CoreProxyManager not wired - use direct process management".to_string(),
        ))
    }

    /// Stop the proxy — stub.
    pub async fn stop(_rt: &dyn Runtime) -> Result<(), ProxyError> {
        Err(ProxyError::process(
            crate::proxy::ErrorCode::ProcessKillFailed,
            "CoreProxyManager not wired - use direct process management".to_string(),
        ))
    }

    /// Cancel an in-flight connection attempt.
    pub fn cancel_connect(_rt: &dyn Runtime) {
        crate::proxy::CONNECTION_CANCELLING.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Get current proxy status from the runtime's proxy state.
    pub fn get_status(rt: &dyn Runtime) -> ConnectionStatus {
        let mgr = rt.proxy_state();
        // Use block_on-style synchronous check with try_lock.
        // tokio::sync::Mutex::try_lock returns a guard that borrows the mutex.
        // We avoid the borrow chain issue by extracting the PID directly via statics.
        drop(mgr);
        
        let pid = crate::proxy::PROXY_PID.load(std::sync::atomic::Ordering::Acquire);
        if pid > 0 && crate::is_process_alive(pid) {
            ConnectionStatus::Connected
        } else {
            ConnectionStatus::Disconnected
        }
    }
}
