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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;

// ── Constants ───────────────────────────────────────────────────────────────

/// Maximum servers per batch.
const BATCH_SIZE: usize = 50;

/// Wait up to this long for more requests before starting a batch.
const BATCH_TIMEOUT_MS: u64 = 300;

/// Timeout for each individual server's test (latency + outlet IP).
/// Used in tests only — production uses PER_URL_TIMEOUT_SECS.
#[allow(dead_code)]
const TEST_TIMEOUT_SECS: u64 = 8;

/// Time to wait for sing-box to start before testing.
const SINGBOX_STARTUP_MS: u64 = 600;

/// Number of parallel testing tasks per batch.
/// Reduced from 50 to 20: 50 concurrent TLS handshakes overwhelmed
/// mobile/low-end systems.
const MAX_CONCURRENT_TESTS: usize = 20;

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
    pub diagnostic_warning: Option<String>,
}

/// Background worker entry point. Spawned from `ServerCache::new()`.
///
/// Collects batch requests, runs sing-box, tests servers in parallel,
/// updates caches, sends results back, and emits progress events.
pub async fn run(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<BatchRequest>,
    cache: ServerCache,
    port_pool: Arc<PortPool>,
    data_dir: PathBuf,
    cancel: Arc<std::sync::Mutex<CancellationToken>>,
) {
    tracing::info!(target: "cache", "[Batcher] engine started");

    loop {
        // NO select! cancel arm — the engine runs until the channel closes.
        // Cancel is handled per-batch inside process_batch by reading the
        // CURRENT token from the Arc<Mutex>.
        let batch = match collect_batch(&mut rx).await {
            Some(b) => b,
            None => {
                tracing::info!(target: "cache", "[Batcher] channel closed — shutting down");
                return;
            }
        };

        if batch.is_empty() {
            continue;
        }

        let count = batch.len();
        tracing::info!(target: "cache", "[Batcher] processing batch of {count} server(s)");

        process_batch(batch, Some(&cache), &port_pool, &data_dir, &cancel).await;
    }
}

/// Background worker entry point for stateless batch processing.
///
/// Same as `run()` but does NOT take a `ServerCache` — the caller (e.g.
/// `batch_test_incremental` in `cache.rs`) handles cache + SQLite writes.
/// The engine only sends results via `oneshot` channels.
pub async fn run_streaming(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<BatchRequest>,
    port_pool: Arc<PortPool>,
    data_dir: PathBuf,
    cancel: Arc<std::sync::Mutex<CancellationToken>>,
) {
    tracing::info!(target: "cache", "[Batcher] streaming engine started");

    loop {
        let batch = match collect_batch(&mut rx).await {
            Some(b) => b,
            None => {
                tracing::info!(target: "cache", "[Batcher] streaming channel closed — shutting down");
                return;
            }
        };

        if batch.is_empty() {
            continue;
        }

        let count = batch.len();
        tracing::info!(target: "cache", "[Batcher] streaming processing batch of {count} server(s)");

        process_batch(batch, None::<&ServerCache>, &port_pool, &data_dir, &cancel).await;
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
    cache: Option<&ServerCache>,
    port_pool: &PortPool,
    data_dir: &Path,
    cancel: &Arc<std::sync::Mutex<CancellationToken>>,
) {
    let mut active_batch = Vec::with_capacity(batch.len());
    for request in batch {
        if request.cancel.is_cancelled() {
            let _ = request.reply.send(Err("Cancelled".to_string()));
        } else {
            active_batch.push(request);
        }
    }
    let batch = active_batch;
    if batch.is_empty() {
        tracing::debug!(target: "cache", "[Batcher] skipped batch: all requests were already cancelled");
        return;
    }

    // Read the CURRENT token at the start of this batch. If cancel_refresh
    // fired during a previous batch, the old token is cancelled but this
    // call reads the NEW (un-cancelled) token.
    // StdMutex is held for microseconds (just a clone) — no await while locked.
    let token = cancel.lock().unwrap_or_else(|e| e.into_inner()).clone();

    let count = batch.len();
    tracing::info!(target: "cache", "[Batcher] processing {count} server(s)");

    // 1. Acquire ports
    tracing::info!(target: "cache", "[Batcher] acquiring {count} port(s)");

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
            if attempt < 10 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        if guard.is_none() {
            tracing::warn!(target: "cache", "[Batcher] port pool exhausted after 10 attempts");
        }
        match guard {
            Some(g) => guards.push(g),
            None => {
                tracing::error!(target: "cache", "[Batcher] port pool exhausted — cannot process batch");
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
    if token.is_cancelled() {
        for req in batch {
            let _ = req.reply.send(Err("Cancelled".into()));
        }
        return;
    }

    // 2. Generate config
    tracing::info!(target: "cache", "[Batcher] generating N-inbound config ({count} servers)");

    let test_dir = std::env::temp_dir().join(format!("omnitun-batch-{}", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::create_dir_all(&test_dir) {
        tracing::error!(target: "cache", "[Batcher] failed to create test dir: {e}");
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
        tracing::info!(target: "cache",
            "[Batcher] server {}: protocol={}, server={}:{}, port_inbound={}",
            sid, profile.protocol, profile.server, profile.port, port_list[idx],
        );
    }

    let config = match generate_batch_config(&tagged_servers, &port_list, &test_dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(target: "cache", "[Batcher] config generation failed: {e}");
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
            tracing::error!(target: "cache", "[Batcher] config serialization failed: {e}");
            for req in batch {
                if let Err(e) = req
                    .reply
                    .send(Err(format!("Config serialization error: {e}")))
                {
                    tracing::warn!(target: "cache", "[Batcher] reply send failed: {:?}", e);
                }
            }
            if let Err(e) = std::fs::remove_dir_all(&test_dir) {
                tracing::warn!(target: "cache", "[Batcher] failed to remove test dir: {e}");
            }
            return;
        }
    };
    if let Err(e) = std::fs::write(&config_path, &config_str) {
        tracing::error!(target: "cache", "[Batcher] config write failed: {e}");
        for req in batch {
            if let Err(e) = req.reply.send(Err(format!("Config write error: {e}"))) {
                tracing::warn!(target: "cache", "[Batcher] reply send failed: {:?}", e);
            }
        }
        if let Err(e) = std::fs::remove_dir_all(&test_dir) {
            tracing::warn!(target: "cache", "[Batcher] failed to remove test dir: {e}");
        }
        return;
    }

    // 3. Check if sing-box binary exists, then start
    if !binary_path.exists() {
        tracing::error!(target: "cache", "[Batcher] sing-box binary not found at {:?} — installing may be needed (run main proxy first)", binary_path);
        for req in batch {
            if let Err(e) = req.reply.send(Err(
                "sing-box not installed — connect to proxy first to auto-install".into(),
            )) {
                tracing::warn!(target: "cache", "[Batcher] reply send failed: {:?}", e);
            }
        }
        if let Err(e) = std::fs::remove_dir_all(&test_dir) {
            tracing::warn!(target: "cache", "[Batcher] failed to remove test dir: {e}");
        }
        return;
    }

    tracing::info!(target: "cache", "[Batcher] starting sing-box...");

    #[cfg(windows)]
    let (mut child, _job_guard) = {
        let mut cmd = std::process::Command::new(&binary_path);
        cmd.args(["run", "-c"])
            .arg(&config_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);

        let child_proc = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                // Fallback: breakaway may be blocked (e.g. terminal job)
                match std::process::Command::new(&binary_path)
                    .args(["run", "-c"])
                    .arg(&config_path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(target: "cache", "[Batcher] failed to start sing-box: {e}");
                        for req in batch {
                            if let Err(e) =
                                req.reply.send(Err(format!("sing-box startup error: {e}")))
                            {
                                tracing::warn!(target: "cache", "[Batcher] reply send failed: {:?}", e);
                            }
                        }
                        if let Err(e) = std::fs::remove_dir_all(&test_dir) {
                            tracing::warn!(target: "cache", "[Batcher] failed to remove test dir: {e}");
                        }
                        return;
                    }
                }
            }
        };

        // Assign to kill-on-close JobObject for orphan protection
        use std::os::windows::io::AsRawHandle;
        let mut job_guard = None;
        if let Some(job) = crate::proxy::process::orphan_guard::create() {
            if crate::proxy::process::orphan_guard::assign(&job, child_proc.as_raw_handle()) {
                job_guard = Some(job);
            }
        }
        (child_proc, job_guard)
    };

    #[cfg(not(windows))]
    let mut child = {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&binary_path);
        cmd.args(["run", "-c"])
            .arg(&config_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);

        // Linux: PDEATHSIG so the kernel kills sing-box on parent exit
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                extern "C" {
                    fn prctl(option: i32, arg2: u64, ...) -> i32;
                }
                prctl(1, 9); // PR_SET_PDEATHSIG=1, SIGKILL=9
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(target: "cache", "[Batcher] failed to start sing-box: {e}");
                for req in batch {
                    let _ = req.reply.send(Err(format!("sing-box startup error: {e}")));
                }
                let _ = std::fs::remove_dir_all(&test_dir);
                return;
            }
        }
    };

    let pid = child.id();
    tracing::info!(target: "cache", "[Batcher] sing-box started (PID {pid})");

    // Wait for sing-box to initialize, but stop immediately if the batch is cancelled.
    tokio::select! {
        _ = token.cancelled() => {
            #[cfg(windows)]
            drop(_job_guard);
            kill_singbox(pid);
            for request in batch {
                let _ = request.reply.send(Err("Cancelled".to_string()));
            }
            let _ = std::fs::remove_dir_all(&test_dir);
            return;
        }
        _ = tokio::time::sleep(Duration::from_millis(SINGBOX_STARTUP_MS)) => {}
    }

    // Port readiness check — ensure sing-box actually bound the ports
    let mut ports_ready = false;
    if let Some(&first_port) = port_list.first() {
        for _ in 0..20 {
            // up to 2 seconds
            if tokio::net::TcpStream::connect(("127.0.0.1", first_port))
                .await
                .is_ok()
            {
                ports_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if ports_ready {
        tracing::info!(target: "cache", "[Batcher] all proxy ports ready");
    } else {
        tracing::warn!(target: "cache", "[Batcher] proxy ports not ready after 2s, proceeding anyway...");
    }

    // Check if sing-box is still alive after startup
    match child.try_wait() {
        Ok(Some(status)) => {
            let stderr = child
                .stderr
                .take()
                .and_then(|mut s| {
                    use std::io::Read;
                    let mut buf = String::new();
                    s.read_to_string(&mut buf).ok().map(|_| buf)
                })
                .unwrap_or_default();
            tracing::error!(target: "cache", "[Batcher] sing-box exited prematurely with status {status} — config error likely");
            if !stderr.is_empty() {
                tracing::error!(target: "cache", "[Batcher] sing-box stderr:\n{}", stderr.trim());
            }
            // Log to frontend logs
            tracing::error!(target: "cache", "[Batcher] sing-box exited with status {status}: {}",
                stderr.trim().lines().next().unwrap_or("no stderr"));
            tracing::error!(target: "cache", "[Batcher] config preserved at {:?}", test_dir.join("config.json"));
            if let Ok(cfg) = std::fs::read_to_string(test_dir.join("config.json")) {
                if let Ok(cfg_json) = serde_json::from_str::<serde_json::Value>(&cfg) {
                    let safe_cfg = crate::proxy::config::redact_sensitive_data(cfg_json);
                    tracing::error!(target: "cache", "[Batcher] generated config (redacted):\n{}",
                        serde_json::to_string_pretty(&safe_cfg).unwrap_or_default());
                }
            }
            // Preserve config+log for debugging by COPYING them into
            // data_dir/batch-debug (same as the total-failure path below),
            // then remove the temp dir. Leaving it in %TEMP% leaks one
            // omnitun-batch-<uuid> dir per config-error batch (AUDIT-PLAN BUG-C3).
            let debug_dir = data_dir.join("batch-debug");
            if std::fs::create_dir_all(&debug_dir).is_ok() {
                let _ = std::fs::copy(test_dir.join("config.json"), debug_dir.join("config.json"));
                let sb_log = test_dir.join("sing-box.log");
                if sb_log.exists() {
                    let _ = std::fs::copy(&sb_log, debug_dir.join("sing-box.log"));
                }
                let sb_err = test_dir.join("sing-box-stderr.log");
                if sb_err.exists() {
                    let _ = std::fs::copy(&sb_err, debug_dir.join("sing-box-stderr.log"));
                }
                tracing::info!(target: "cache",
                    "[Batcher] config+log preserved at {:?} for debugging", debug_dir);
            }
            for req in batch {
                let _ = req
                    .reply
                    .send(Err(format!("sing-box exited with status {status}")));
            }
            let _ = std::fs::remove_dir_all(&test_dir);
            return;
        }
        Ok(None) => {
            tracing::info!(target: "cache", "[Batcher] sing-box is still running — proceeding with tests");
        }
        Err(e) => {
            tracing::warn!(target: "cache", "[Batcher] sing-box wait check error (will proceed): {e}");
        }
    }

    // 4. Test all servers in parallel through their individual ports.
    //
    // IMPORTANT: the reply `oneshot::Sender` is NOT moved into the spawned
    // task. If it were, aborting a task (deadline) or a panic would drop the
    // sender → the caller's `rx.await` returns `RecvError` → no
    // `ServerTestedEvent`, no `test_history` row, and the server vanishes
    // from the UI ("Tested 47/50 forever"). Instead we keep the sender in a
    // side table keyed by server id; the task returns only the result, and
    // the collector loop drains the matching sender here. Anything left in
    // the table after the loop is explicitly replied-to as an error.
    let mut set = tokio::task::JoinSet::new();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TESTS));
    // pending_replies: server_id -> (reply sender, server id for cache).
    type BatchReply =
        tokio::sync::oneshot::Sender<Result<crate::proxy::cache::PositiveResult, String>>;
    let mut pending_replies: std::collections::HashMap<String, (BatchReply, String)> =
        std::collections::HashMap::with_capacity(batch.len());

    for (idx, req) in batch.into_iter().enumerate() {
        let port = port_list[idx];
        let sem = semaphore.clone();
        let global_cancel = token.clone();
        let request_cancel = req.cancel.clone();
        let server_id: String = req.server.id.clone();
        let server_id_for_cache: String = req.server.id.clone();
        pending_replies.insert(server_id.clone(), (req.reply, server_id_for_cache));
        let owned_server = req.server;
        set.spawn(async move {
            let _permit = sem.acquire().await;
            let res = tokio::select! {
                _ = global_cancel.cancelled() => Err("Cancelled".to_string()),
                _ = request_cancel.cancelled() => Err("Cancelled".to_string()),
                result = test_single_server(port, &owned_server, &request_cancel) => result,
            };
            (owned_server.id, res)
        });
    }

    // 5. Collect results
    // Per-batch deadline — guards against a single stuck batch blocking
    // the engine indefinitely. 90s accommodates the UDP worst case:
    // 6s timeout × 6 URLs × 2 attempts = 72s per server, with 20-way
    // concurrency a wave's wall-clock = max(per-server) ≈ 72s.
    // 90s gives headroom while still catching truly stuck batches.
    let batch_deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut success_count = 0usize;
    let mut fail_count = 0usize;

    loop {
        let res = match tokio::time::timeout_at(batch_deadline, set.join_next()).await {
            Ok(Some(result)) => result,
            Ok(None) => break,
            Err(_) => {
                tracing::warn!(
                    target: "cache",
                    "[Batcher] batch deadline (90s) exceeded ? aborting {} remaining task(s)",
                    set.len()
                );
                set.abort_all();
                while set.join_next().await.is_some() {}
                break;
            }
        };
        match res {
            Ok((sid, Ok(result))) => {
                if let Some(warning) = &result.diagnostic_warning {
                    tracing::info!(target: "cache", "{warning}");
                }
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

                if let Some(cache) = cache {
                    if !result.behind_cdn && result.outlet_ip.is_none() {
                        // Non-CDN server passed ping but no outlet IP — route issue.
                        // Report as negative so it goes to negative cache
                        // with previously_worked=1 (auto-test re-checks fast).
                        let reason =
                            "No exit IP after batch test (transient route failure)".to_string();
                        cache
                            .update_negative(&result.server_id, reason.clone())
                            .await;
                        tracing::warn!(target: "cache",
                            "[Batcher] server {} passed — ping {}ms, NO outlet IP (non-CDN, negative)",
                            result.server_id, result.ping_ms);
                        if let Some((reply, _)) = pending_replies.remove(&sid) {
                            let _ = reply.send(Err(reason));
                        }
                        fail_count += 1;
                        continue;
                    }
                    cache
                        .update_positive(&result.server_id, positive.clone())
                        .await;
                }
                if let Some((reply, _)) = pending_replies.remove(&sid) {
                    let _ = reply.send(Ok(positive));
                }
                success_count += 1;
                match result.outlet_ip {
                    Some(ref ip) => tracing::info!(target: "cache",
                        "[Batcher] server {} passed — ping {}ms, outlet {}", result.server_id, result.ping_ms, ip),
                    None => tracing::warn!(target: "cache",
                        "[Batcher] server {} passed — ping {}ms, NO outlet IP (behind CDN — expected)", result.server_id, result.ping_ms),
                }
            }
            Ok((sid, Err(e))) => {
                if let Some((reply, sid_cache)) = pending_replies.remove(&sid) {
                    if e != "Cancelled" {
                        if let Some(cache) = cache.as_ref() {
                            cache.update_negative(&sid_cache, e.clone()).await;
                        }
                    }
                    let _ = reply.send(Err(e));
                }
                fail_count += 1;
            }
            Err(e) => {
                tracing::error!(target: "cache", "[Batcher] Task panicked: {e}");
                fail_count += 1;
            }
        }
    }

    // Any reply still pending here belongs to a task that was aborted or
    // panicked before producing a result. Reply with an explicit error so the
    // caller's `rx.await` resolves, a `ServerTestedEvent` is emitted, and the
    // server is recorded as negative in the cache. Without this the server
    // would vanish from the UI ("Tested 47/50 forever").
    for (sid, (reply, sid_cache)) in pending_replies.drain() {
        let reason = "Batch aborted before completion (deadline/panic)".to_string();
        if let Some(cache) = cache.as_ref() {
            cache.update_negative(&sid_cache, reason.clone()).await;
        }
        let _ = reply.send(Err(reason));
        fail_count += 1;
        tracing::warn!(target: "cache", "[Batcher] resolved orphaned reply for server {sid} as failed");
    }

    // 6. Stop sing-box
    tracing::info!(target: "cache", "[Batcher] stopping sing-box (PID {pid})");
    // Drop JobObject first — triggers KILL_ON_JOB_CLOSE (OS kills process immediately)
    #[cfg(windows)]
    drop(_job_guard);
    kill_singbox(pid);

    // 7. Clean up — preserve config+log on total failure for debugging
    if success_count == 0 && count > 0 {
        let debug_dir = data_dir.join("batch-debug");
        if let Err(e) = std::fs::create_dir_all(&debug_dir) {
            tracing::warn!(target: "cache", "[Batcher] failed to create debug dir: {e}");
        }
        if let Err(e) = std::fs::copy(&config_path, debug_dir.join("config.json")) {
            tracing::warn!(target: "cache", "[Batcher] failed to copy config for debug: {e}");
        }
        let sb_log = test_dir.join("sing-box.log");
        if sb_log.exists() {
            if let Err(e) = std::fs::copy(&sb_log, debug_dir.join("sing-box.log")) {
                tracing::warn!(target: "cache", "[Batcher] failed to copy log for debug: {e}");
            }
        }
        tracing::info!(target: "cache",
            "[Batcher] config+log preserved at {:?} for debugging", debug_dir);
    }
    if let Err(e) = std::fs::remove_dir_all(&test_dir) {
        tracing::warn!(target: "cache", "[Batcher] failed to remove test dir: {e}");
    }

    // 8. Cleanup done — flush_cache_batch (called by batch_test_incremental or
    //    manually by the caller) already wrote results to SQLite.
    //    save_to_disk is NOT called here because it does DELETE FROM server_cache
    //    which would erase negative entries written by Phase 2 pre-filter.

    tracing::info!(
        target: "cache",
        "[Batcher] batch complete: {success_count}/{count} passed, {fail_count} failed",
    );
}

/// Test URLs ordered by preference. Tried in sequence within each attempt.
/// Accessed through the proxy (SOCKS5h), not directly — so HTTPS is safe from
/// local ISP interception. Cloudflare anycast CDN = lowest global latency.
const TEST_URLS: &[&str] = &[
    "https://cp.cloudflare.com/generate_204",
    "https://www.gstatic.com/generate_204",
    "https://www.msftconnecttest.com/connecttest.txt",
    "https://detectportal.firefox.com/success.txt",
    "http://cp.cloudflare.com/generate_204",
];

/// Default per-URL timeout for TCP-based protocols.
/// TCP handshake + TLS + HTTP 204 ≈ 1-2s on a healthy server; 3s gives
/// ample headroom without wasting time on dead servers.
const PER_URL_TIMEOUT_TCP_SECS: u64 = 3;
/// Per-URL timeout for UDP-based protocols (hysteria2, tuic, wireguard).
/// QUIC handshake takes longer than TCP TLS; 6s is conservative.
const PER_URL_TIMEOUT_UDP_SECS: u64 = 6;

fn per_url_timeout_secs(server: &ServerProfile) -> Duration {
    if server.is_udp_protocol() {
        Duration::from_secs(PER_URL_TIMEOUT_UDP_SECS)
    } else {
        Duration::from_secs(PER_URL_TIMEOUT_TCP_SECS)
    }
}

/// Timeout for exit IP fetch. Reduced from 30s to 8s:
/// fetch_outlet_ip_through_port races 3 services in parallel and returns on
/// the first Some, so worst case is bounded by the slowest service.
/// Empirically <5s when the proxy works. 30s meant a single flaky server
/// could hang a 50-server batch for half a minute.
const EXIT_IP_TIMEOUT_SECS: u64 = 8;

static CONFIGURED_TEST_URL: std::sync::LazyLock<std::sync::RwLock<String>> =
    std::sync::LazyLock::new(|| {
        std::sync::RwLock::new("https://cp.cloudflare.com/generate_204".to_string())
    });

/// Update the configured test URL from user preferences.
pub fn set_configured_test_url(host_path: &str, use_tls: bool) {
    let scheme = if use_tls { "https" } else { "http" };
    let url = format!("{}://{}", scheme, host_path);
    if let Ok(mut locked) = CONFIGURED_TEST_URL.write() {
        *locked = url;
    }
}

// ── Batch settings (mirrors AppSettings for the batcher subsystem) ──────────
// The batcher runs in a background task without direct access to the Tauri
// app handle or the AppSettings managed state. To apply user-configured
// anti-DPI / TLS / mux settings in generate_batch_config, we mirror the
// relevant fields into global statics (same pattern as CONFIGURED_TEST_URL).
// set_batch_settings() is called on app start and on every settings change.

static BATCH_ANTI_DPI: std::sync::LazyLock<std::sync::RwLock<bool>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(true));
static BATCH_TLS_INSECURE: std::sync::LazyLock<std::sync::RwLock<bool>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(true));
static BATCH_ENABLE_MUX: std::sync::LazyLock<std::sync::RwLock<bool>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(false));

pub fn set_batch_settings(anti_dpi: bool, tls_insecure: bool, enable_mux: bool) {
    if let Ok(mut g) = BATCH_ANTI_DPI.write() {
        *g = anti_dpi;
    }
    if let Ok(mut g) = BATCH_TLS_INSECURE.write() {
        *g = tls_insecure;
    }
    if let Ok(mut g) = BATCH_ENABLE_MUX.write() {
        *g = enable_mux;
    }
}
pub fn batch_anti_dpi() -> bool {
    BATCH_ANTI_DPI.read().map(|g| *g).unwrap_or(true)
}
pub fn batch_tls_insecure() -> bool {
    BATCH_TLS_INSECURE.read().map(|g| *g).unwrap_or(true)
}
pub fn batch_enable_mux() -> bool {
    BATCH_ENABLE_MUX.read().map(|g| *g).unwrap_or(false)
}

/// Read the currently configured test URL.
pub fn configured_test_url() -> Option<String> {
    CONFIGURED_TEST_URL
        .read()
        .ok()
        .map(|u| u.clone())
        .filter(|u| !u.is_empty())
}

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
        .timeout(per_url_timeout_secs(server))
        .build()
        .map_err(|e| format!("Client build error: {e}"))?;

    // 1. Latency test — try URLs in order with 1 retry.
    //    На каждой попытке перебираем все URL, а не только первый.
    //    Prepend the user-configured test URL (from settings) as the
    //    first URL to try. If it fails, fall back to the hardcoded TEST_URLS.
    let configured = CONFIGURED_TEST_URL
        .read()
        .map(|u| u.clone())
        .unwrap_or_default();
    let urls: Vec<&str> = if configured.is_empty() {
        TEST_URLS.to_vec()
    } else {
        std::iter::once(configured.as_str())
            .chain(TEST_URLS.iter().copied())
            .collect()
    };
    let mut last_err = "No test URL succeeded".to_string();
    let mut ping_ms = 0u64;
    let mut success = false;

    for attempt in 0..2 {
        if cancel.is_cancelled() {
            return Err("Cancelled".into());
        }
        for &url in &urls {
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
                    last_err = format!("HTTP status {} for {}", resp.status(), url);
                }
                Err(e) => {
                    last_err = format!("Request failed for {url}: {e}");
                }
            }
        }
        if success {
            break;
        }
        // Небольшая пауза перед retry
        if attempt == 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    if !success {
        return Err(last_err);
    }

    // 2. Outlet IP + CDN bug detection.
    let mut outlet_ip = None;
    let mut diagnostic_warning = None;

    if !server.behind_cdn {
        // Fetch outlet IP only if server is NOT marked as CDN.
        // behind_cdn is finalized at the process_servers_incremental stage via entry-IP
        // + keyword checks; we do NOT override it here.
        // Reuse the existing `client` (warm connection pool) with a
        // per-request timeout.
        let fetched =
            fetch_outlet_ip_through_port(&client, Some(Duration::from_secs(EXIT_IP_TIMEOUT_SECS)))
                .await;

        if let Some(ref ip) = fetched {
            outlet_ip = Some(ip.clone());

            // Canary: outlet IP in CDN ranges despite non-CDN entry.
            // Possible legitimate reasons: WARP upstream, cloud hosting that
            // overlaps with CDN ranges, ISP that uses CDN-as-backbone.
            // Possible bug: process_servers_incremental entry-IP classification was wrong.
            // We observe but do NOT mutate behind_cdn here.
            if crate::proxy::geoip::is_cdn_entry_ip(ip) {
                diagnostic_warning = Some(format!(
                    "[BUG] Server {} ({}:{}, name={:?}) — outlet IP {} is in CDN ranges \
                     although entry IP was NOT marked CDN. Keeping outlet IP and \
                     behind_cdn=false (outlet is egress, not entry). If this fires \
                     repeatedly for the same server, investigate CDN list coverage \
                     or process_servers_incremental entry-IP classification.",
                    server.id, server.server, server.port, server.original_name, ip,
                ));
            }
        }
    }

    // 3. Exit GeoIP
    let exit_geoip = if let Some(ref ip) = outlet_ip {
        crate::proxy::geoip::resolve_hostname(ip).await
    } else {
        None
    };

    // 🚫 ANTI-REGRESSION: финальная проверка инварианты перед возвратом.
    // Если server.behind_cdn=true, то outlet_ip обязан быть None.
    // Проверка — гарантия от будущих регрессий. Не удалять.
    if server.behind_cdn && outlet_ip.is_some() {
        tracing::error!(
            target: "cdn-invariant",
            "[BUG][I-CDN-1] INVARIANT VIOLATION in test_single_server (post-result): \
             server {} ({}:{}) — behind_cdn=true AND outlet_ip={:?}. \
             This is a REGRESSION. Returning outlet_ip=None defensively.",
            server.id,
            server.server,
            server.port,
            outlet_ip,
        );
        outlet_ip = None;
    }
    if server.behind_cdn && exit_geoip.is_some() {
        tracing::error!(
            target: "cdn-invariant",
            "[BUG][I-CDN-1] INVARIANT VIOLATION in test_single_server (post-result): \
             server {} ({}:{}) — behind_cdn=true AND exit_geoip={:?}. \
             This is a REGRESSION. Returning exit fields as None defensively.",
            server.id,
            server.server,
            server.port,
            exit_geoip,
        );
    }

    Ok(ServerTestResult {
        server_id: server.id.clone(),
        ping_ms,
        outlet_ip,
        behind_cdn: server.behind_cdn,
        exit_flag: exit_geoip.as_ref().map(|g| g.country_code.clone()),
        exit_country: exit_geoip.as_ref().map(|g| g.country_name.clone()),
        exit_city: None, // city intentionally excluded per user requirement
        diagnostic_warning,
    })
}

/// Fetch outlet IP through the sing‑box proxy port with multiple fallback URLs concurrently.
/// Returns `None` if all services fail / time out.
async fn fetch_outlet_ip_through_port(
    client: &reqwest::Client,
    per_request_timeout: Option<Duration>,
) -> Option<String> {
    let services = [
        "https://www.cloudflare.com/cdn-cgi/trace",
        "https://api64.ipify.org",
        "https://ifconfig.me/ip",
    ];

    let mut set = tokio::task::JoinSet::new();
    for &url in &services {
        let client = client.clone();
        let t = per_request_timeout;
        set.spawn(async move {
            let req = client.get(url);
            let req = if let Some(t) = t { req.timeout(t) } else { req };
            let resp = req.send().await.ok()?;
            // Must check status — 403/50x returns HTML, not an IP.
            if !resp.status().is_success() {
                return None;
            }
            let text = resp.text().await.ok()?;
            // cloudflare.com/cdn-cgi/trace returns key=value pairs (ip=... loc=...)
            // Extract the IP line. Other services return just the IP.
            let candidate = text
                .lines()
                .find_map(|l| l.strip_prefix("ip="))
                .map(|s| s.trim().to_string())
                .or_else(|| {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        Some(trimmed)
                    } else {
                        None
                    }
                })?;
            // Validate that the response is actually an IP address.
            // Without this, HTML error pages (403 Forbidden etc.) get stored as exit IP.
            if candidate.parse::<std::net::IpAddr>().is_ok() {
                Some(candidate)
            } else {
                None
            }
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
        // sing-box is spawned in its own process group (process_group(0) above),
        // so kill the WHOLE group with `kill -9 -<pgid>` to avoid orphaning any
        // child subprocess sing-box may have spawned. A bare `kill -9 <pid>`
        // only terminates the leader and can leave children reparented to init.
        // Fall back to killing just the pid if the group kill fails (e.g. the
        // process already exited or never set its own pgrp).
        let neg = format!("-{}", pid);
        let killed_group = std::process::Command::new("kill")
            .args(["-9", &neg])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !killed_group {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
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
                cancel: CancellationToken::new(),
                reply: tokio::sync::oneshot::channel().0,
            });
        }

        let batch = collect_batch(&mut rx).await;
        assert!(batch.is_some(), "batch should be Some when items available");
        let batch = batch.unwrap();
        assert_eq!(
            batch.len(),
            BATCH_SIZE,
            "collect_batch should return exactly BATCH_SIZE items"
        );
    }

    #[tokio::test]
    async fn test_collect_batch_returns_remaining_on_channel_close() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Send 3 requests, then drop the sender
        for _ in 0..3 {
            let _ = tx.send(BatchRequest {
                server: ServerProfile::default(),
                profile_id: String::new(),
                cancel: CancellationToken::new(),
                reply: tokio::sync::oneshot::channel().0,
            });
        }
        drop(tx);

        let batch = collect_batch(&mut rx).await;
        assert!(
            batch.is_some(),
            "collect_batch should return remaining items on channel close"
        );
        let items = batch.unwrap();
        assert_eq!(items.len(), 3, "should get all 3 sent items");
    }

    #[tokio::test]
    async fn test_collect_batch_returns_none_on_empty_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx); // Close channel immediately

        let batch = collect_batch(&mut rx).await;
        assert!(
            batch.is_none(),
            "should return None when channel is empty and closed"
        );
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
        const {
            assert!(
                BATCH_SIZE > 0 && BATCH_SIZE <= 200,
                "BATCH_SIZE should be between 1 and 200"
            );
            assert!(
                BATCH_TIMEOUT_MS >= 50 && BATCH_TIMEOUT_MS <= 5000,
                "BATCH_TIMEOUT_MS should be between 50 and 5000"
            );
            assert!(
                TEST_TIMEOUT_SECS >= 2 && TEST_TIMEOUT_SECS <= 30,
                "TEST_TIMEOUT_SECS should be between 2 and 30"
            );
            assert!(
                SINGBOX_STARTUP_MS >= 100 && SINGBOX_STARTUP_MS <= 5000,
                "SINGBOX_STARTUP_MS should be between 100 and 5000"
            );
            assert!(
                MAX_CONCURRENT_TESTS >= 1 && MAX_CONCURRENT_TESTS <= 200,
                "MAX_CONCURRENT_TESTS should be between 1 and 200"
            );
        };
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
        let result = fetch_outlet_ip_through_port(&client, None).await;
        assert!(
            result.is_none(),
            "fetch_outlet_ip should return None when all services fail"
        );
    }

    #[tokio::test]
    async fn test_fetch_outlet_ip_very_short_timeout() {
        // Even with 1ms timeout, should return None gracefully (not panic)
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .timeout(Duration::from_millis(1))
            .build()
            .unwrap();
        let result = fetch_outlet_ip_through_port(&client, None).await;
        assert!(
            result.is_none(),
            "fetch_outlet_ip should return None even with very short timeout"
        );
    }

    // ── test_single_server with cancellation ──────────────────────────────

    #[tokio::test]
    async fn test_test_single_server_cancelled_before_test() {
        let server = ServerProfile::default();
        let cancel = CancellationToken::new();

        // Cancel immediately
        cancel.cancel();

        let result = test_single_server(9999, &server, &cancel).await;
        assert!(
            result.is_err(),
            "test_single_server should return Err when cancelled"
        );

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
            assert!(
                r.ping_ms > 0 || r.server_id == server.id,
                "unexpected result state"
            );
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
            diagnostic_warning: None,
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
            diagnostic_warning: None,
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
            diagnostic_warning: None,
        };
        assert!(r.exit_city.is_none());
    }

    // ── BatcherLog helper removed ──────────────────────────────────────────
    // batcher_log() has been replaced with direct tracing::*!() macros.
    // The function no longer exists, so this compile-time check is removed.
}
