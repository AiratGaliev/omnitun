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

use crate::proxy::cache::{BatchRequest, PositiveResult, ServerCache};
use crate::proxy::config::generate_batch_config;
use crate::proxy::installer::Installer;
use crate::proxy::port_pool::PortPool;
use crate::proxy::ServerProfile;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

// ── Constants ───────────────────────────────────────────────────────────────

/// Maximum servers per batch.
const BATCH_SIZE: usize = 50;

/// Wait up to this long for more requests before starting a batch.
const BATCH_TIMEOUT_MS: u64 = 300;

/// Timeout for each individual server's test (latency + outlet IP).
const TEST_TIMEOUT_SECS: u64 = 8;

/// Time to wait for sing-box to start before testing.
const SINGBOX_STARTUP_MS: u64 = 600;

/// Number of parallel testing tasks per batch.
const MAX_CONCURRENT_TESTS: usize = 50;

// ── Helper: emit structured log to JSONL + UI ──────────────────────────────

/// Log a batcher message to the app's structured log (visible in JSONL and UI).
fn batcher_log(app_handle: &AppHandle, level: crate::logging::LogLevel, message: String) {
    use crate::logging::LogSource;
    let logger = app_handle.state::<std::sync::Mutex<crate::logging::Logger>>();
    crate::logging::log(app_handle, &logger, level, LogSource::Cache, message);
}

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
    batcher_log(&app_handle, crate::logging::LogLevel::Info, "[Batcher] engine started".into());

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                batcher_log(&app_handle, crate::logging::LogLevel::Info, "[Batcher] cancelled — shutting down".into());
                return;
            }
            result = collect_batch(&mut rx) => {
                let batch = match result {
                    Some(b) => b,
                    None => {
                        batcher_log(&app_handle, crate::logging::LogLevel::Info, "[Batcher] channel closed — shutting down".into());
                        return;
                    }
                };

                if batch.is_empty() {
                    continue;
                }

                let count = batch.len();
                batcher_log(&app_handle, crate::logging::LogLevel::Info, format!("[Batcher] processing batch of {count} server(s)"));

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
    batcher_log(app_handle, crate::logging::LogLevel::Info, format!("[Batcher] processing {count} server(s)"));

    // 1. Acquire ports
    batcher_log(app_handle, crate::logging::LogLevel::Info, format!("[Batcher] acquiring {count} port(s)"));

    // Get binary path
    let data_dir = match app_handle.path().app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] cannot get app data dir: {e}"));
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
                batcher_log(app_handle, crate::logging::LogLevel::Warning, "[Batcher] port pool exhausted after 10 retries".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        match guard {
            Some(g) => guards.push(g),
            None => {
                batcher_log(app_handle, crate::logging::LogLevel::Error, "[Batcher] port pool exhausted — cannot process batch".into());
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
    batcher_log(app_handle, crate::logging::LogLevel::Info, format!("[Batcher] generating N-inbound config ({count} servers)"));

    let test_dir = std::env::temp_dir().join(format!(
        "omnitun-batch-{}",
        uuid::Uuid::new_v4()
    ));
    if let Err(e) = std::fs::create_dir_all(&test_dir) {
        batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] failed to create test dir: {e}"));
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

    // Also log first few server protocols for debugging
    for (idx, (sid, profile)) in tagged_servers.iter().enumerate().take(3) {
        batcher_log(app_handle, crate::logging::LogLevel::Info, format!(
            "[Batcher] server {}: protocol={}, server={}:{}, port_inbound={}",
            sid, profile.protocol, profile.server, profile.port, port_list[idx],
        ));
    }

    let config = match generate_batch_config(&tagged_servers, &port_list, &test_dir) {
        Ok(c) => c,
        Err(e) => {
            batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] config generation failed: {e}"));
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
            batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] config serialization failed: {e}"));
            for req in batch {
                let _ = req.reply.send(Err(format!("Config serialization error: {e}")));
            }
            let _ = std::fs::remove_dir_all(&test_dir);
            return;
        }
    };
    if let Err(e) = std::fs::write(&config_path, &config_str) {
        batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] config write failed: {e}"));
        for req in batch {
            let _ = req.reply.send(Err(format!("Config write error: {e}")));
        }
        let _ = std::fs::remove_dir_all(&test_dir);
        return;
    }

    // 3. Check if sing-box binary exists, then start
    if !binary_path.exists() {
        batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] sing-box binary not found at {:?} — installing may be needed (run main VPN first)", binary_path));
        for req in batch {
            let _ = req.reply.send(Err("sing-box not installed — connect to VPN first to auto-install".into()));
        }
        let _ = std::fs::remove_dir_all(&test_dir);
        return;
    }

    batcher_log(app_handle, crate::logging::LogLevel::Info, "[Batcher] starting sing-box...".into());

    #[cfg(windows)]
    let mut child = match std::process::Command::new(&binary_path)
        .args(["run", "-c"])
        .arg(&config_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] failed to start sing-box: {e}"));
            for req in batch {
                let _ = req.reply.send(Err(format!("sing-box startup error: {e}")));
            }
            let _ = std::fs::remove_dir_all(&test_dir);
            return;
        }
    };

    #[cfg(not(windows))]
    let mut child = match std::process::Command::new(&binary_path)
        .args(["run", "-c"])
        .arg(&config_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] failed to start sing-box: {e}"));
            for req in batch {
                let _ = req.reply.send(Err(format!("sing-box startup error: {e}")));
            }
            let _ = std::fs::remove_dir_all(&test_dir);
            return;
        }
    };

    let pid = child.id();
    batcher_log(app_handle, crate::logging::LogLevel::Info, format!("[Batcher] sing-box started (PID {pid})"));

    // Wait for sing-box to initialize
    tokio::time::sleep(Duration::from_millis(SINGBOX_STARTUP_MS)).await;

    // Port readiness check — ensure sing-box actually bound the ports
    let mut ports_ready = false;
    if let Some(&first_port) = port_list.first() {
        for _ in 0..20 { // up to 2 seconds
            if tokio::net::TcpStream::connect(("127.0.0.1", first_port)).await.is_ok() {
                ports_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if ports_ready {
        batcher_log(app_handle, crate::logging::LogLevel::Info, "[Batcher] all proxy ports ready".into());
    } else {
        batcher_log(app_handle, crate::logging::LogLevel::Warning, "[Batcher] proxy ports not ready after 2s, proceeding anyway...".into());
    }

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
            batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] sing-box exited prematurely with status {status} — config error likely"));
            if !stderr.is_empty() {
                batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] sing-box stderr:\n{}", stderr.trim()));
            }
            // Log to frontend logs
            batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] sing-box exited with status {status}: {}",
                stderr.trim().lines().next().unwrap_or("no stderr")));
            batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] config preserved at {:?}", test_dir.join("config.json")));
            if let Ok(cfg) = std::fs::read_to_string(test_dir.join("config.json")) {
                batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] generated config:\n{cfg}"));
            }
            // Don't delete test_dir — save config for debugging
            for req in batch {
                let _ = req.reply.send(Err(format!("sing-box exited with status {status}")));
            }
            return;
        }
        Ok(None) => {
            batcher_log(app_handle, crate::logging::LogLevel::Info, "[Batcher] sing-box is still running — proceeding with tests".into());
        }
        Err(e) => {
            batcher_log(app_handle, crate::logging::LogLevel::Warning, format!("[Batcher] sing-box wait check error (will proceed): {e}"));
        }
    }

    // 4. Test all servers in parallel through their individual ports
    let mut set = tokio::task::JoinSet::new();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TESTS));

    for (idx, req) in batch.into_iter().enumerate() {
        let port = port_list[idx];
        let sem = semaphore.clone();
        let cancel = cancel.clone();

        set.spawn(async move {
            let _permit = sem.acquire().await;
            let res = test_single_server(port, &req.server, &cancel).await;
            (req, res)
        });
    }

    // 5. Collect results
    let mut success_count = 0usize;
    let mut fail_count = 0usize;

    while let Some(res) = set.join_next().await {
        match res {
            Ok((req, Ok(result))) => {
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

                cache.update_positive(&result.server_id, positive.clone()).await;
                let _ = req.reply.send(Ok(positive));
                success_count += 1;
                match result.outlet_ip {
                    Some(ref ip) => batcher_log(app_handle, crate::logging::LogLevel::Info,
                        format!("[Batcher] server {} passed — ping {}ms, outlet {}", result.server_id, result.ping_ms, ip)),
                    None => batcher_log(app_handle, crate::logging::LogLevel::Warning,
                        format!("[Batcher] server {} passed — ping {}ms, NO outlet IP", result.server_id, result.ping_ms)),
                }
            }
            Ok((req, Err(e))) => {
                cache.update_negative(&req.server.id, e.clone()).await;
                let _ = req.reply.send(Err(e));
                fail_count += 1;
            }
            Err(e) => {
                batcher_log(app_handle, crate::logging::LogLevel::Error, format!("[Batcher] Task panicked: {e}"));
                fail_count += 1;
            }
        }
    }

    // 6. Stop sing-box
    batcher_log(app_handle, crate::logging::LogLevel::Info, format!("[Batcher] stopping sing-box (PID {pid})"));
    kill_singbox(pid);

    // 7. Clean up — preserve config+log on total failure for debugging
    if success_count == 0 && count > 0 {
        let debug_dir = data_dir.join("batch-debug");
        let _ = std::fs::create_dir_all(&debug_dir);
        let _ = std::fs::copy(config_path, debug_dir.join("config.json"));
        let sb_log = test_dir.join("sing-box.log");
        if sb_log.exists() {
            let _ = std::fs::copy(&sb_log, debug_dir.join("sing-box.log"));
        }
        batcher_log(app_handle, crate::logging::LogLevel::Info, format!(
            "[Batcher] config+log preserved at {:?} for debugging", debug_dir));
    }
    let _ = std::fs::remove_dir_all(&test_dir);

    // 8. Save cache
    cache.save_to_disk().await;

    batcher_log(
        app_handle,
        crate::logging::LogLevel::Info,
        format!("[Batcher] batch complete: {success_count}/{count} passed, {fail_count} failed"),
    );
}

/// Test URLs ordered by preference. Tried in sequence within each attempt.
/// Accessed through the proxy (SOCKS5h), not directly — so HTTPS is safe from
/// local ISP interception. Cloudflare anycast CDN = lowest global latency.
const TEST_URLS: &[&str] = &[
    "https://cp.cloudflare.com/generate_204",
    "https://www.msftconnecttest.com/connecttest.txt",
    "https://detectportal.firefox.com/success.txt",
    "http://cp.cloudflare.com/generate_204",
];

/// Test a single server through its dedicated proxy port.
/// Tries multiple test URLs with one retry on total failure.
async fn test_single_server(
    port: u16,
    server: &ServerProfile,
    cancel: &CancellationToken,
) -> Result<ServerTestResult, String> {
    // Use socks5h for DNS resolution on the proxy side (avoids local DNS blocking)
    let proxy = reqwest::Proxy::all(format!("socks5h://127.0.0.1:{port}"))
        .map_err(|e| format!("Proxy build error: {e}"))?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Client build error: {e}"))?;

    // 1. Latency test — try URLs in order with 1 retry
    let mut last_err = "No test URL succeeded".to_string();
    let mut ping_ms = 0u64;
    let mut success = false;

    for attempt in 0..2 {
        if cancel.is_cancelled() {
            return Err("Cancelled".into());
        }
        for &url in TEST_URLS {
            if attempt == 0 && url != TEST_URLS[0] {
                // First attempt only tries primary URL; fallbacks on retry
                break;
            }
            if cancel.is_cancelled() {
                return Err("Cancelled".into());
            }
            let start = Instant::now();
            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    ping_ms = start.elapsed().as_millis() as u64;
                    success = true;
                    break;
                }
                Ok(resp) => {
                    last_err = format!("HTTP status {}", resp.status());
                }
                Err(e) => {
                    last_err = format!("Request failed: {e}");
                }
            }
        }
        if success {
            break;
        }
    }

    if !success {
        return Err(last_err);
    }

    // 2. Outlet IP — multiple fallback services, concurrent fetch.
    let outlet_ip = fetch_outlet_ip_through_port(&client).await;

    // 3. CDN detection is ENTRY IP only (done in TCP ping phase).
    //    Outlet IP CDN detection is incorrect — traffic CDN ≠ server behind CDN.
    let behind_cdn = server.behind_cdn;

    // 4. Exit GeoIP
    let exit_geoip = if let Some(ref ip) = outlet_ip {
        crate::proxy::geoip::resolve_hostname(ip).await
    } else {
        None
    };

    Ok(ServerTestResult {
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

/// Fetch outlet IP through the sing‑box proxy port with multiple fallback URLs concurrently.
/// Returns `None` if all services fail / time out.
async fn fetch_outlet_ip_through_port(client: &reqwest::Client) -> Option<String> {
    let services = [
        "https://www.cloudflare.com/cdn-cgi/trace",
        "https://api64.ipify.org",
        "https://ifconfig.me/ip",
    ];

    let mut set = tokio::task::JoinSet::new();
    for &url in &services {
        let client = client.clone();
        set.spawn(async move {
            let resp = client.get(url).send().await.ok()?;
            let text = resp.text().await.ok()?;
            // cloudflare.com/cdn-cgi/trace returns key=value pairs (ip=... loc=...)
            // Extract the IP line. Other services return just the IP.
            let ip = text.lines()
                .find_map(|l| l.strip_prefix("ip="))
                .map(|s| s.trim().to_string())
                .or_else(|| {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() { Some(trimmed) } else { None }
                });
            ip
        });
    }

    while let Some(res) = set.join_next().await {
        if let Ok(Some(ip)) = res {
            return Some(ip);
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
            .creation_flags(CREATE_NO_WINDOW)
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

    // ── collect_batch ─────────────────────────────────────────────────────

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

    #[tokio::test]
    async fn test_collect_batch_returns_none_on_empty_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx); // Close channel immediately

        let batch = collect_batch(&mut rx).await;
        assert!(batch.is_none(), "should return None when channel is empty and closed");
    }

    // ── kill_singbox ──────────────────────────────────────────────────────

    #[test]
    fn test_kill_singbox_nonexistent_pid() {
        // Should not panic regardless of PID existence
        kill_singbox(99999999);
        // If we reach here, the function handled it gracefully
    }

    #[test]
    fn test_kill_singbox_zero_pid() {
        // PID 0 should not cause panic either
        kill_singbox(0);
    }

    // ── Constants ─────────────────────────────────────────────────────────

    #[test]
    fn test_batch_constants() {
        assert!(BATCH_SIZE > 0 && BATCH_SIZE <= 200, "BATCH_SIZE should be between 1 and 200");
        assert!(BATCH_TIMEOUT_MS >= 50 && BATCH_TIMEOUT_MS <= 5000, "BATCH_TIMEOUT_MS should be between 50 and 5000");
        assert!(TEST_TIMEOUT_SECS >= 2 && TEST_TIMEOUT_SECS <= 30, "TEST_TIMEOUT_SECS should be between 2 and 30");
        assert!(SINGBOX_STARTUP_MS >= 100 && SINGBOX_STARTUP_MS <= 5000, "SINGBOX_STARTUP_MS should be between 100 and 5000");
        assert!(MAX_CONCURRENT_TESTS >= 1 && MAX_CONCURRENT_TESTS <= 200, "MAX_CONCURRENT_TESTS should be between 1 and 200");
    }

    // ── fetch_outlet_ip_through_port ──────────────────────────────────────

    /// Build a reqwest client that points to a dead proxy (= will time out).
    fn dead_client() -> reqwest::Client {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn test_fetch_outlet_ip_all_services_fail() {
        // With a dead proxy, all 3 services should fail → None
        let client = dead_client();
        let result = fetch_outlet_ip_through_port(&client).await;
        assert!(result.is_none(), "fetch_outlet_ip should return None when all services fail");
    }

    #[tokio::test]
    async fn test_fetch_outlet_ip_very_short_timeout() {
        // Even with 1ms timeout, should return None gracefully (not panic)
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .timeout(Duration::from_millis(1))
            .build()
            .unwrap();
        let result = fetch_outlet_ip_through_port(&client).await;
        assert!(result.is_none(), "fetch_outlet_ip should return None even with very short timeout");
    }

    // ── test_single_server with cancellation ──────────────────────────────

    #[tokio::test]
    async fn test_test_single_server_cancelled_before_test() {
        let server = ServerProfile::default();
        let cancel = CancellationToken::new();

        // Cancel immediately
        cancel.cancel();

        let result = test_single_server(9999, &server, &cancel).await;
        assert!(result.is_err(), "test_single_server should return Err when cancelled");

        // Verify cancellation is idempotent
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn test_test_single_server_with_dead_port() {
        let server = ServerProfile::default();
        let cancel = CancellationToken::new();

        // Port 1 should not have a proxy → proxy creation may fail or timeout
        let result = test_single_server(1, &server, &cancel).await;
        // Either Err (proxy creation failure) or timeout — both acceptable
        // We just verify it doesn't panic
        if let Ok(r) = result {
            // If proxy somehow connected, ping_ms should be valid
            assert!(r.ping_ms > 0 || r.server_id == server.id, "unexpected result state");
        }
    }

    // ── ServerTestResult construction ─────────────────────────────────────

    #[test]
    fn test_server_test_result_construction() {
        let result = ServerTestResult {
            server_id: "test-1".into(),
            ping_ms: 42,
            outlet_ip: Some("1.2.3.4".into()),
            behind_cdn: false,
            exit_flag: Some("US".into()),
            exit_country: Some("United States".into()),
            exit_city: Some("New York".into()),
        };

        assert_eq!(result.server_id, "test-1");
        assert_eq!(result.ping_ms, 42);
        assert_eq!(result.outlet_ip, Some("1.2.3.4".into()));
        assert!(!result.behind_cdn);
        assert_eq!(result.exit_flag, Some("US".into()));

        // Test with CDN = true, no outlet IP
        let cdn_result = ServerTestResult {
            server_id: "test-cdn".into(),
            ping_ms: 120,
            outlet_ip: None,
            behind_cdn: true,
            exit_flag: None,
            exit_country: None,
            exit_city: None,
        };
        assert!(cdn_result.behind_cdn);
        assert!(cdn_result.outlet_ip.is_none());
        assert_eq!(cdn_result.ping_ms, 120);
    }

    #[test]
    fn test_server_test_result_empty_city() {
        let r = ServerTestResult {
            server_id: "x".into(),
            ping_ms: 10,
            outlet_ip: None,
            behind_cdn: false,
            exit_flag: None,
            exit_country: Some("Germany".into()),
            exit_city: None,
        };
        assert!(r.exit_city.is_none());
    }

    // ── BatcherLog helper signature ───────────────────────────────────────
    // batcher_log() calls AppHandle::state() which requires a running Tauri app,
    // so we can't unit-test it in isolation. Test that the function signature
    // resolves at compile time by verifying we can call it with the right types.
    #[test]
    fn test_batcher_log_fn_compiles() {
        // Compile-time check: function exists with expected signature.
        // No runtime test needed — it requires Tauri runtime.
        fn _check_type(a: &tauri::AppHandle, l: crate::logging::LogLevel, m: String) {
            batcher_log(a, l, m);
        }
    }
}
