//! Shared proxy lifecycle logic for Tauri and MCP.
//!
//! This module extracts the common parts of `start_proxy` (Tauri) and
//! `cmd_start` (MCP) into a single function `start_proxy_core`. Both
//! callers pass their parameters; the function does the shared work
//! (port fallback, config generation, process launch, wait-for-ready,
//! ACTIVE_SERVER_ID, LAST_PROFILE) and returns a `StartedProxy`.
//!
//! The caller is responsible for:
//!   - Spawning the watchdog (needs Tauri events or MCP notifications)
//!   - Exit IP verification (needs Tauri events)
//!   - Emitting status/progress events (environment-specific)
//!
//! DESIGN: the Tauri path is NOT changed by this patch. Only the MCP
//! server calls start_proxy_core. A future refactor can migrate the
//! Tauri path once the MCP path is validated in production.

use crate::proxy::{
    config, process::ProcessManager, ErrorCode, ProxyError, ServerProfile,
    ACTIVE_SERVER_ID, LAST_PROFILE, LastConnection,
};
use crate::settings::AppSettings;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

/// Parameters for `start_proxy_core`. Both Tauri and MCP construct this.
pub struct StartProxyParams {
    pub data_dir: PathBuf,
    pub bin_path: PathBuf,
    pub config_dir: PathBuf,
    pub proxy_port: u16,
    pub routing_mode: String,
    pub all_servers: Vec<ServerProfile>,
    pub settings: AppSettings,
    pub process_mgr: Arc<TokioMutex<ProcessManager>>,
}

/// Result of a successful `start_proxy_core`.
pub struct StartedProxy {
    pub pid: u32,
    /// The actual bound port (may differ from requested if fallback occurred).
    pub actual_port: u16,
    /// The Clash API port (for traffic stats, health checks, etc.).
    pub stats_port: u16,
}

/// Start the proxy: port check → config → spawn → wait-for-ready → set globals.
///
/// This is the shared core. Callers (Tauri `start_proxy`, MCP `cmd_start`)
/// call this, then do their environment-specific work (events, watchdog).
///
/// # Errors
/// - `ConfigNoServers` if `all_servers` is empty
/// - `ConfigGenerationFailed` if config generation fails
/// - `ProcessStartFailed` if the spawn fails
/// - `ProcessNotReachable` if the port doesn't open within timeout
pub async fn start_proxy_core(params: StartProxyParams) -> Result<StartedProxy, ProxyError> {
    let StartProxyParams {
        data_dir: _,
        bin_path: _,
        config_dir,
        proxy_port: requested_port,
        routing_mode,
        all_servers,
        settings,
        process_mgr,
    } = params;

    if all_servers.is_empty() {
        return Err(ProxyError::config(
            ErrorCode::ConfigNoServers,
            "No servers provided to start_proxy_core",
        ));
    }

    // 1. Port availability check with fallback (port+1 .. port+100).
    let actual_port = resolve_available_port(requested_port);
    if actual_port != requested_port {
        tracing::warn!(
            target: "proxy",
            "Port {requested_port} is in use, using {actual_port} instead"
        );
    }

    // 2. Generate sing-box config with the USER'S actual settings.
    let (config_json, stats_port) = config::generate_config(
        &config_dir,
        actual_port,
        &routing_mode,
        &all_servers,
        &settings,
    )?;

    // 3. Write config to disk.
    let config_path = config::write_config(&config_json, &config_dir)?;
    tracing::info!(target: "proxy", "Config written to {}", config_path.display());

    // 4. Spawn process via ProcessManager (uses start_headless internally).
    let pid = {
        let mgr = process_mgr.lock().await;
        mgr.start_headless(&config_path).await?
    };
    tracing::info!(target: "proxy", "sing-box started (PID {pid}) on port {actual_port}");

    // 5. Wait for proxy ready (TCP connect to the bound port).
    wait_for_proxy_ready_local(pid, actual_port).await?;

    // 6. Set ACTIVE_SERVER_ID to the primary server (all_servers[0]).
    {
        let mut active = ACTIVE_SERVER_ID
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *active = Some(all_servers[0].id.clone());
    }

    // 7. Set LAST_PROFILE for watchdog auto-reconnect.
    {
        let mut last = LAST_PROFILE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *last = Some(LastConnection {
            all_servers: all_servers.clone(),
            routing_mode: routing_mode.clone(),
            proxy_port: actual_port,
        });
    }

    Ok(StartedProxy {
        pid,
        actual_port,
        stats_port,
    })
}

/// Find an available port: try `start`, then `start+1` .. `start+100`.
/// If none are available, return `start` anyway (sing-box will fail to
/// bind, and the caller will get a clear error).
fn resolve_available_port(start: u16) -> u16 {
    if config::port_available(start) {
        return start;
    }
    for offset in 1..=100u16 {
        let candidate = match start.checked_add(offset) {
            Some(c) => c,
            None => break,
        };
        if config::port_available(candidate) {
            return candidate;
        }
    }
    // No alternative found — return the original (will fail to bind).
    start
}

/// Wait until sing-box is accepting TCP connections on `proxy_port`.
///
/// This is a headless copy of `wait_for_proxy_ready` from lib.rs. We
/// duplicate it here to avoid a circular dependency (lib.rs depends on
/// proxy::lifecycle, lifecycle depends on lib.rs's wait_for_proxy_ready).
/// A future refactor should move wait_for_proxy_ready into this module
/// and have lib.rs call it.
const PROXY_TIMEOUT_SECS: u64 = 15;
const PROXY_POLL_MS: u64 = 300;

async fn wait_for_proxy_ready_local(pid: u32, proxy_port: u16) -> Result<u32, ProxyError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(PROXY_TIMEOUT_SECS);
    let mut attempts = 0u32;

    loop {
        attempts += 1;

        if crate::proxy::CONNECTION_CANCELLING
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ProxyError::process(
                ErrorCode::ProcessNotReachable,
                "Connection cancelled by user",
            ));
        }

        if !crate::is_process_alive(pid) {
            return Err(ProxyError::process(
                ErrorCode::ProcessNotReachable,
                format!(
                    "sing-box (PID {pid}) exited before the proxy port was ready (after {attempts} attempt(s))"
                ),
            ));
        }

        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::net::TcpStream::connect(format!("127.0.0.1:{proxy_port}")),
        )
        .await
        {
            Ok(Ok(_stream)) => {
                tracing::info!(target: "proxy", "Proxy ready after {attempts} attempt(s)");
                return Ok(attempts);
            }
            _ => {}
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(ProxyError::process(
                ErrorCode::ProcessNotReachable,
                format!(
                    "timed out after {attempts} attempt(s) waiting for proxy port {proxy_port} (PID {pid})"
                ),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(PROXY_POLL_MS)).await;
    }
}
