//! BatchingEngine — background worker for parallel server testing.
//!
//! Collects batch requests from `ServerCache`, generates an N-inbound
//! sing-box config, starts one sing-box process, tests all servers in
//! parallel through their individual proxy ports, then sends results back
//! via oneshot channels.
//!
//! Key advantage over the old Clash API approach:
//! - No selector switching (race condition source)
//! - No sequential outlet IP checks (old code: 100ms × N servers)
//! - Latency + outlet IP + CDN check all in one parallel pass
//! - Every server tested through its own inbound port

use crate::events::{emit_refresh_progress, RefreshProgress};
use crate::vpn::cache::{BatchRequest, PositiveResult, ServerCache};
use crate::vpn::config::generate_batch_config;
use crate::vpn::installer::Installer;
use crate::vpn::port_pool::PortPool;
use crate::vpn::ServerProfile;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

// ── Constants ───────────────────────────────────────────────────────────────

/// Maximum servers per batch.
const BATCH_SIZE: usize = 50;

/// Wait up to this long for more requests before starting a batch.
const BATCH_TIMEOUT_MS: u64 = 300;

/// Timeout for each individual server's test (latency + outlet IP).
const TEST_TIMEOUT_SECS: u64 = 5;

/// Time to wait for sing-box to start before testing.
const SINGBOX_STARTUP_MS: u64 = 600;

/// Number of parallel testing tasks per batch.
const MAX_CONCURRENT_TESTS: usize = 50;

// ── BatchingEngine ──────────────────────────────────────────────────────────

/// Results from testing a single server through its proxy port.
#[derive(Debug, Clone)]
pub struct ServerTestResult {
    pub server_id: String,
    pub ping_ms: u64,
    pub outlet_ip: Option<String>,
    pub behind_cdn: bool,
    pub exit_flag: Option<String>,
    pub exit_country: Option<String>,
    pub exit_city: Option<String>,
}

/// Background worker entry point. Spawned from `ServerCache::new()`.
///
/// Collects batch requests, runs sing-box, tests servers in parallel,
/// updates caches, sends results back, and emits progress events.
pub async fn run(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<BatchRequest>,
    cache: ServerCache,
    port_pool: Arc<PortPool>,
    app_handle: AppHandle,
    cancel: CancellationToken,
) {
    log::info!("[Batcher] engine started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                log::info!("[Batcher] cancelled — shutting down");
                return;
            }
            result = collect_batch(&mut rx) => {
                let batch = match result {
                    Some(b) => b,
                    None => {
                        log::info!("[Batcher] channel closed — shutting down");
                        return;
                    }
                };

                if batch.is_empty() {
                    continue;
                }

                let count = batch.len();
                log::info!("[Batcher] processing batch of {count} server(s)");

                // Emit: queueing phase
                emit_refresh_progress(&app_handle, RefreshProgress {
                    profile_id: String::new(),
                    total: count,
                    tested: 0,
                    cache_hits: 0,
                    successful: 0,
                    failed: 0,
                    phase: "acquiring_ports".into(),
                });

                process_batch(batch, &cache, &port_pool, &app_handle, &cancel).await;
            }
        }
    }
}

/// Collect requests from the channel until BATCH_SIZE or timeout.
async fn collect_batch(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<BatchRequest>,
) -> Option<Vec<BatchRequest>> {
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let timer = tokio::time::sleep(Duration::from_millis(BATCH_TIMEOUT_MS));
    tokio::pin!(timer);

    loop {
        tokio::select! {
            req = rx.recv() => {
                match req {
                    Some(r) => {
                        batch.push(r);
                        if batch.len() >= BATCH_SIZE {
                            break;
                        }
                    }
                    None => return if batch.is_empty() { None } else { Some(batch) },
                }
            }
            _ = &mut timer => break,
        }
    }

    Some(batch)
}

/// Process a batch of servers through sing-box.
async fn process_batch(
    batch: Vec<BatchRequest>,
    cache: &ServerCache,
    port_pool: &PortPool,
    app_handle: &AppHandle,
    cancel: &CancellationToken,
) {
    let count = batch.len();
    log::info!("[Batcher] processing {count} server(s)");

    let pid = batch.first().map(|r| r.profile_id.as_str()).unwrap_or("");
    emit_refresh_progress(app_handle, RefreshProgress {
        profile_id: pid.to_string(),
        total: count,
        tested: 0,
        cache_hits: 0,
        successful: 0,
        failed: 0,
        phase: "acquiring_ports".into(),
    });

    // 1. Acquire ports
    log::info!("[Batcher] acquiring {count} port(s)");

    // Get binary path
    let data_dir = match app_handle.path().app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            log::error!("[Batcher] cannot get app data dir: {e}");
            for req in batch {
                let _ = req.reply.send(Err(format!("App data dir error: {e}")));
            }
            return;
        }
    };

    let binary_path = data_dir.join("binaries").join(Installer::binary_name());

    // Acquire ports with retry
    let mut guards = Vec::with_capacity(count);
    for _ in 0..count {
        let mut guard = None;
        for attempt in 1..=10 {
            if let Some(g) = port_pool.acquire() {
                guard = Some(g);
                break;
            }
            if attempt == 10 {
                log::warn!("[Batcher] port pool exhausted after 10 retries");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        match guard {
            Some(g) => guards.push(g),
            None => {
                log::error!("[Batcher] port pool exhausted — cannot process batch");
                for req in batch {
                    let _ = req.reply.send(Err("Port pool exhausted".into()));
                }
                return;
            }
        }
    }
    // (guard keeps ports alive until end of scope)

    let port_list: Vec<u16> = guards.iter().map(|g| g.port).collect();

    // Check cancellation
    if cancel.is_cancelled() {
        for req in batch {
            let _ = req.reply.send(Err("Cancelled".into()));
        }
        return;
    }

    // 2. Generate config
    log::info!("[Batcher] generating N-inbound config ({count} servers)");

    let test_dir = std::env::temp_dir().join(format!(
        "omnitun-batch-{}",
        uuid::Uuid::new_v4()
    ));
    if let Err(e) = std::fs::create_dir_all(&test_dir) {
        log::error!("[Batcher] failed to create test dir: {e}");
        for req in batch {
            let _ = req.reply.send(Err(format!("Temp dir error: {e}")));
        }
        return;
    }

    // Build tagged server list from BatchRequest (each carries the full ServerProfile)
    let tagged_servers: Vec<(String, ServerProfile)> = batch
        .iter()
        .map(|req| (req.server.id.clone(), req.server.clone()))
        .collect();

    let config = match generate_batch_config(&tagged_servers, &port_list, &test_dir) {
        Ok(c) => c,
        Err(e) => {
            log::error!("[Batcher] config generation failed: {e}");
            for req in batch {
                let _ = req.reply.send(Err(format!("Config error: {e}")));
            }
            let _ = std::fs::remove_dir_all(&test_dir);
            return;
        }
    };

    let config_path = test_dir.join("config.json");
    let config_str = match serde_json::to_string_pretty(&config) {
        Ok(s) => s,
        Err(e) => {
            log::error!("[Batcher] config serialization failed: {e}");
            for req in batch {
                let _ = req.reply.send(Err(format!("Config serialization error: {e}")));
            }
            let _ = std::fs::remove_dir_all(&test_dir);
            return;
        }
    };
    if let Err(e) = std::fs::write(&config_path, &config_str) {
        log::error!("[Batcher] config write failed: {e}");
        for req in batch {
            let _ = req.reply.send(Err(format!("Config write error: {e}")));
        }
        let _ = std::fs::remove_dir_all(&test_dir);
        return;
    }

    // 3. Check if sing-box binary exists, then start
    if !binary_path.exists() {
        log::error!("[Batcher] sing-box binary not found at {:?} — installing may be needed (run main VPN first)", binary_path);
        for req in batch {
            let _ = req.reply.send(Err("sing-box not installed — connect to VPN first to auto-install".into()));
        }
        let _ = std::fs::remove_dir_all(&test_dir);
        return;
    }

    log::info!("[Batcher] starting sing-box...");

    let mut child = match std::process::Command::new(&binary_path)
        .args(["run", "-c"])
        .arg(&config_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log::error!("[Batcher] failed to start sing-box: {e}");
            for req in batch {
                let _ = req.reply.send(Err(format!("sing-box startup error: {e}")));
            }
            let _ = std::fs::remove_dir_all(&test_dir);
            return;
        }
    };
    let pid = child.id();
    log::info!("[Batcher] sing-box started (PID {pid})");

    // Wait for sing-box to initialize
    tokio::time::sleep(Duration::from_millis(SINGBOX_STARTUP_MS)).await;

    // Check if sing-box is still alive after startup
    match child.try_wait() {
        Ok(Some(status)) => {
            let stderr = child.stderr.take()
                .and_then(|mut s| {
                    use std::io::Read;
                    let mut buf = String::new();
                    s.read_to_string(&mut buf).ok().map(|_| buf)
                })
                .unwrap_or_default();
            log::error!("[Batcher] sing-box exited prematurely with status {status} — config error likely");
            if !stderr.is_empty() {
                log::error!("[Batcher] sing-box stderr:\n{}", stderr.trim());
            }
            // Also log to frontend logs via the custom log() function
            let _ = crate::logging::log(
                app_handle,
                &app_handle.state::<std::sync::Mutex<crate::logging::Logger>>(),
                crate::logging::LogLevel::Error,
                crate::logging::LogSource::Cache,
                format!("[Batcher] sing-box exited with status {status}: {}",
                    stderr.trim().lines().next().unwrap_or("no stderr")),
            );
            let _ = std::fs::read_to_string(test_dir.join("config.json"))
                .map(|cfg| log::error!("[Batcher] generated config:\n{cfg}"));
            // Don't delete test_dir — save config for debugging
            log::error!("[Batcher] config preserved at {:?}", test_dir.join("config.json"));
            for req in batch {
                let _ = req.reply.send(Err(format!("sing-box exited with status {status}")));
            }
            return;
        }
        Ok(None) => {
            log::info!("[Batcher] sing-box is still running — proceeding with tests");
        }
        Err(e) => {
            log::warn!("[Batcher] sing-box wait check error (will proceed): {e}");
        }
    }

    emit_refresh_progress(app_handle, RefreshProgress {
        profile_id: pid.to_string(),
        total: count,
        tested: 0,
        cache_hits: 0,
        successful: 0,
        failed: 0,
        phase: "singbox_starting".into(),
    });

    // 4. Test all servers in parallel through their individual ports
    let mut handles = Vec::with_capacity(count);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TESTS));

    for (idx, req) in batch.iter().enumerate() {
        let port = port_list[idx];
        let req_server = req.server.clone();
        let sem = semaphore.clone();
        let cancel = cancel.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await;
            test_single_server(port, &req_server, &cancel).await
        }));
    }

    // 5. Collect results
    let mut success_count = 0usize;
    let mut fail_count = 0usize;

    for (handle, req) in handles.into_iter().zip(batch) {
        match handle.await {
            Ok(Some(result)) => {
                // Create PositiveResult from test result
                let positive = PositiveResult {
                    server_id: result.server_id.clone(),
                    ping_ms: result.ping_ms,
                    exit_ip: result.outlet_ip.clone(),
                    exit_flag: result.exit_flag.clone(),
                    exit_country: result.exit_country.clone(),
                    exit_city: result.exit_city.clone(),
                    behind_cdn: result.behind_cdn,
                    tested_at: chrono::Utc::now().timestamp(),
                };

                // Update caches
                cache.update_positive(&result.server_id, positive.clone()).await;
                let _ = req.reply.send(Ok(positive));
                success_count += 1;
                log::info!(
                    "[Batcher] server {} passed — ping {}ms, outlet_ip: {}",
                    result.server_id,
                    result.ping_ms,
                    result.outlet_ip.as_deref().unwrap_or("N/A"),
                );
            }
            Ok(None) => {
                cache.update_negative(&req.server.id, "Test failed — no response".into()).await;
                let _ = req.reply.send(Err("Test failed — no response".into()));
                fail_count += 1;
            }
            Err(e) => {
                cache.update_negative(&req.server.id, format!("Task panicked: {e}")).await;
                let _ = req.reply.send(Err(format!("Test panicked: {e}")));
                fail_count += 1;
            }
        }
    }

    // 6. Stop sing-box
    log::info!("[Batcher] stopping sing-box (PID {pid})");
    kill_singbox(pid);

    // 7. Clean up
    let _ = std::fs::remove_dir_all(&test_dir);

    // 8. Save cache
    cache.save_to_disk().await;

    // 9. Emit completion
    emit_refresh_progress(app_handle, RefreshProgress {
        profile_id: String::new(),
        total: count,
        tested: count,
        cache_hits: 0,
        successful: success_count,
        failed: fail_count,
        phase: "done".into(),
    });

    log::info!(
        "[Batcher] batch complete: {success_count}/{count} passed, {fail_count} failed"
    );
}

/// Test a single server through its dedicated proxy port.
async fn test_single_server(
    port: u16,
    server: &ServerProfile,
    cancel: &CancellationToken,
) -> Option<ServerTestResult> {
    let proxy = reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).ok()?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECS))
        .build()
        .ok()?;

    // 1. Latency test
    let start = Instant::now();
    let latency_result = client
        .get("http://cp.cloudflare.com/generate_204")
        .send()
        .await;

    let ping_ms = match latency_result {
        Ok(resp) if resp.status().is_success() => {
            start.elapsed().as_millis() as u64
        }
        Ok(resp) => {
            log::warn!("[test_single_server] latency HTTP {} returned status {}", server.id, resp.status());
            return None;
        }
        Err(e) => {
            log::warn!("[test_single_server] latency HTTP {} failed: {e}", server.id);
            return None;
        }
    };

    // Check cancellation after latency test
    if cancel.is_cancelled() {
        return None;
    }

    // 2. Outlet IP — multiple fallback services, non-fatal if all fail.
    //    Unlike the old Clash API approach (sequential per-server), this runs
    //    in parallel for all servers via individual sing‑box inbounds, so it
    //    doesn't bottleneck the batch.
    let outlet_ip = fetch_outlet_ip_through_port(&client).await;

    // 3. CDN detection via outlet IP
    let behind_cdn = outlet_ip
        .as_ref()
        .map(|ip| crate::vpn::geoip::is_cdn_ip(ip))
        .unwrap_or(false);

    // 4. Exit GeoIP
    let exit_geoip = if let Some(ref ip) = outlet_ip {
        crate::vpn::geoip::resolve_hostname(ip).await
    } else {
        None
    };

    Some(ServerTestResult {
        server_id: server.id.clone(),
        ping_ms,
        outlet_ip,
        behind_cdn,
        exit_flag: exit_geoip.as_ref().map(|g| g.country_code.clone()),
        exit_country: exit_geoip.as_ref().map(|g| g.country_name.clone()),
        exit_city: exit_geoip.as_ref().and_then(|g| {
            if g.city.is_empty() { None } else { Some(g.city.clone()) }
        }),
    })
}

/// Fetch outlet IP through the sing‑box proxy port with multiple fallback URLs.
/// Returns `None` if all services fail / time out.
async fn fetch_outlet_ip_through_port(client: &reqwest::Client) -> Option<String> {
    let services = [
        "https://ifconfig.me/ip",
        "https://api.ipify.org",
        "https://checkip.amazonaws.com",
    ];

    // Try all services concurrently — take the first success within a timeout
    for &url in &services {
        match tokio::time::timeout(Duration::from_secs(4), async {
            let resp = client.get(url).send().await.ok()?;
            let text = resp.text().await.ok()?;
            let ip = text.trim().to_string();
            if !ip.is_empty() { Some(ip) } else { None }
        }).await {
            Ok(Some(ip)) => return Some(ip),
            _ => continue,
        }
    }
    None
}

/// Kill a sing-box process by PID.
fn kill_singbox(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/f"])
            .output();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output();
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Test that collect_batch returns when a batch is ready (size reached).
    #[tokio::test]
    async fn test_collect_batch_returns_when_size_reached() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Send BATCH_SIZE + 2 requests (should return BATCH_SIZE first)
        for _ in 0..BATCH_SIZE + 2 {
            let _ = tx.send(BatchRequest {
                server: ServerProfile::default(),
                profile_id: String::new(),
                reply: tokio::sync::oneshot::channel().0,
            });
        }

        let batch = collect_batch(&mut rx).await;
        assert!(batch.is_some(), "batch should be Some when items available");
        let batch = batch.unwrap();
        assert_eq!(batch.len(), BATCH_SIZE, "collect_batch should return exactly BATCH_SIZE items");
    }

    /// Test that collect_batch returns whatever it has when the channel closes.
    #[tokio::test]
    async fn test_collect_batch_returns_remaining_on_channel_close() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Send 3 requests, then drop the sender
        for _ in 0..3 {
            let _ = tx.send(BatchRequest {
                server: ServerProfile::default(),
                profile_id: String::new(),
                reply: tokio::sync::oneshot::channel().0,
            });
        }
        drop(tx);

        let batch = collect_batch(&mut rx).await;
        assert!(batch.is_some(), "collect_batch should return remaining items on channel close");
        let items = batch.unwrap();
        assert_eq!(items.len(), 3, "should get all 3 sent items");
    }

    /// Test that collect_batch returns None when channel is already closed and empty.
    #[tokio::test]
    async fn test_collect_batch_returns_none_on_empty_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx); // Close channel immediately

        let batch = collect_batch(&mut rx).await;
        assert!(batch.is_none(), "should return None when channel is empty and closed");
    }

    /// Test that kill_singbox does not panic for a non-existent PID.
    #[test]
    fn test_kill_singbox_nonexistent_pid() {
        // Should not panic regardless of PID existence
        kill_singbox(99999999);
        // If we reach here, the function handled it gracefully
    }

    /// Verify constants are reasonable.
    #[test]
    fn test_batch_constants() {
        assert!(BATCH_SIZE > 0 && BATCH_SIZE <= 200, "BATCH_SIZE should be between 1 and 200");
        assert!(BATCH_TIMEOUT_MS >= 50 && BATCH_TIMEOUT_MS <= 5000, "BATCH_TIMEOUT_MS should be between 50 and 5000");
    }


}
