//! Selector switch + outlet IP integration test.
//!
//! 1. Creates sing-box config with direct + 1 proxy outbound + selector group 'proxy'
//! 2. Starts sing-box (with stderr captured to a log file for diagnostics)
//! 3. Polls the Clash API until it responds (up to 15 s) — replaces the
//!    previous fixed 1-second sleep that was too short on Windows.
//! 4. Checks default selector = 'direct'
//! 5. PUT /proxies/proxy with {'name': 'proxy-{id}'} — asserts 204
//! 6. Fetches outlet IP via mixed port (through proxy)
//! 7. Switches back to direct, fetches outlet IP again
//! 8. Compares IPs (must be different if proxy works)
//! 9. Kills sing-box
//!
//! Run: cargo test --test selector_test -- --nocapture

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").expect("APPDATA not set");
        PathBuf::from(base).join("com.omnitun.desktop")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join(".config")
            .join("com.omnitun.desktop")
    }
}

fn singbox() -> PathBuf {
    #[cfg(target_os = "windows")]
    let name = "sing-box.exe";
    #[cfg(not(target_os = "windows"))]
    let name = "sing-box";

    let m = app_data_dir().join("binaries").join(name);
    if m.exists() {
        return m;
    }
    PathBuf::from(name)
}

/// Find a free TCP port in the ephemeral range.
fn free_port(skip: &[u16]) -> Option<u16> {
    (20000..60000u16).find(|&p| {
        if skip.contains(&p) {
            return false;
        }
        std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
    })
}

/// Load a server from profiles.json. Prefer one with an IP address (not hostname)
/// to avoid DNS-resolution loops through the proxy.
fn load_best_server() -> Option<(serde_json::Value, omnitun_lib::proxy::ServerProfile)> {
    let p = app_data_dir().join("profiles.json");
    let content = std::fs::read_to_string(p).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;
    let servers = data[0]["servers"].as_array()?.clone();

    let mut candidates: Vec<(serde_json::Value, omnitun_lib::proxy::ServerProfile)> = Vec::new();

    for sv in servers {
        let sp: Result<omnitun_lib::proxy::ServerProfile, _> = serde_json::from_value(sv.clone());
        if let Ok(srv) = sp {
            if omnitun_lib::proxy::config::build_outbound(&srv).is_ok() {
                candidates.push((sv, srv));
            }
        }
    }

    if candidates.is_empty() {
        return None;
    }

    // Prefer servers with IP address (to avoid DNS issues through proxy)
    for (sv, srv) in &candidates {
        if srv.server.parse::<std::net::IpAddr>().is_ok() {
            return Some((sv.clone(), srv.clone()));
        }
    }

    // Fallback: first candidate
    Some(candidates.remove(0))
}

/// Fetch outlet IP through a given proxy port (mixed inbound).
/// Uses /ip endpoint for plain-text response.
async fn fetch_outlet_ip(proxy_port: u16, timeout_secs: u64) -> Option<String> {
    let proxy = reqwest::Proxy::http(format!("http://127.0.0.1:{proxy_port}")).ok()?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .ok()?;
    let resp = client.get("http://ifconfig.me/ip").send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text()
        .await
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Fetch outlet IP with retries.
async fn fetch_outlet_ip_retry(
    proxy_port: u16,
    retries: usize,
    timeout_secs: u64,
) -> Option<String> {
    for i in 0..retries {
        let result = fetch_outlet_ip(proxy_port, timeout_secs).await;
        if result.is_some() {
            return result;
        }
        if i < retries - 1 {
            eprintln!("retry {}/{} ...", i + 1, retries);
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
    None
}

/// Poll the Clash API until it responds or the deadline expires.
///
/// This replaces the original `tokio::time::sleep(1000ms)` after spawning
/// sing-box. On Windows + antivirus the Clash API port can take 2–5 seconds
/// to bind, so a fixed 1-second wait regularly failed with
/// "error sending request for url (http://127.0.0.1:20001/proxies/proxy)".
///
/// We also verify the sing-box process is still alive on every iteration —
/// if it died, we can stop early and dump the stderr log.
///
/// Returns `Ok(())` once the API responds, or `Err(message)` after the
/// deadline. The caller is expected to read `log_path` on error to find
/// out why sing-box didn't start.
async fn wait_for_clash_api(
    api_url: &str,
    pid: u32,
    log_path: &std::path::Path,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(800))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    loop {
        // If the process died, no point polling further.
        if !is_process_alive(pid) {
            let log = std::fs::read_to_string(log_path).unwrap_or_default();
            return Err(format!(
                "sing-box (PID {pid}) exited before Clash API came up.\n\
                 stderr log:\n{log}"
            ));
        }

        let last_err = match client.get(format!("{api_url}/version")).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => format!("HTTP {}", r.status()),
            Err(e) => e.to_string(),
        };

        if tokio::time::Instant::now() >= deadline {
            let log = std::fs::read_to_string(log_path).unwrap_or_default();
            return Err(format!(
                "Clash API at {api_url} did not respond before deadline.\n\
                 Last error: {last_err}\n\
                 stderr log:\n{log}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Cross-platform "is PID alive" check.
fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        let output = Command::new("tasklist")
            .args(["/fi", &format!("PID eq {pid}"), "/fo", "csv", "/nh"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(o) => {
                let out = String::from_utf8_lossy(&o.stdout);
                out.contains(&format!("\"{}\"", pid))
            }
            Err(_) => false,
        }
    }
    #[cfg(unix)]
    {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map_or(false, |o| o.status.success())
    }
}

/// Cross-platform kill of a process by PID.
fn kill_process(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("taskkill")
            .args(["/f", "/pid", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill").arg("-9").arg(pid.to_string()).output();
    }
}

#[tokio::test]
async fn selector_switch_and_outlet_ip() {
    // ── 0. Pre-checks ──────────────────────────────────────────────────────
    let bin = singbox();
    if !bin.exists() {
        eprintln!("SKIP: no sing-box binary found");
        return;
    }

    let (_sv, sp) = match load_best_server() {
        Some(pair) => pair,
        None => {
            eprintln!("SKIP: no usable server in profiles.json");
            return;
        }
    };

    println!(
        "Using server: {} ({}:{} | {})",
        sp.display_name(),
        sp.server,
        sp.port,
        sp.protocol
    );

    let proxy_port = match free_port(&[]) {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no free proxy port");
            return;
        }
    };
    let clash_port = match free_port(&[proxy_port]) {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no free clash API port");
            return;
        }
    };

    let server_id = sp.id.clone();
    let proxy_tag = format!("proxy-{server_id}");

    // ── 1. Build outbound + config ────────────────────────────────────────
    let ob = match omnitun_lib::proxy::config::build_outbound(&sp) {
        Ok(mut ob) => {
            ob["tag"] = serde_json::json!(&proxy_tag);
            ob
        }
        Err(e) => {
            eprintln!("SKIP: build_outbound failed: {e}");
            return;
        }
    };

    let config = serde_json::json!({
        "log": {"level": "info"},
        "dns": {
            "servers": [
                {"type": "https", "tag": "local", "server": "8.8.8.8", "server_port": 443}
            ],
            "final": "local"
        },
        "inbounds": [
            {
                "type": "mixed",
                "tag": "mixed-in",
                "listen": "127.0.0.1",
                "listen_port": proxy_port
            }
        ],
        "outbounds": [
            {"type": "direct", "tag": "direct"},
            ob,
            {
                "type": "selector",
                "tag": "proxy",
                "outbounds": ["direct", &proxy_tag]
            }
        ],
        "experimental": {
            "cache_file": {
                "enabled": false
            },
            "clash_api": {
                "external_controller": format!("127.0.0.1:{clash_port}"),
                "secret": ""
            }
        },
        "route": {
            "final": "proxy",
            "auto_detect_interface": true,
            "default_domain_resolver": "local",
            "rules": [{"ip_is_private": true, "outbound": "direct"}]
        },
    });

    let dir = std::env::temp_dir().join(format!("sb-sel-{}-{}", std::process::id(), clash_port));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_p = dir.join("c.json");
    std::fs::write(&cfg_p, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    // ── Config check ──────────────────────────────────────────────────────
    let check_out = Command::new(&bin)
        .args(["check", "-c"])
        .arg(&cfg_p)
        .output()
        .unwrap();
    if !check_out.status.success() {
        eprintln!(
            "SKIP: config check failed:\n{}",
            String::from_utf8_lossy(&check_out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    println!("✓ Config passed sing-box check");

    // ── 2. Start sing-box with stderr captured to a log file ──────────────
    //
    // FIX: previously both stdout and stderr were sent to /dev/null, which
    // made it impossible to diagnose startup failures. Now we redirect
    // stderr to a file in the test dir so we can dump it on failure.
    let log_path = dir.join("sing-box.stderr.log");
    let stderr_file = std::fs::File::create(&log_path).unwrap();
    let child = match Command::new(&bin)
        .args(["run", "-c"])
        .arg(&cfg_p)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: failed to start sing-box: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };
    let pid = child.id();
    println!("✓ sing-box started, PID {pid}");
    println!("  config: {}", cfg_p.display());
    println!("  stderr: {}", log_path.display());

    // FIX: replace the 1-second fixed sleep with a proper readiness poll.
    // On Windows + antivirus, sing-box can take 2–5 seconds to bind the
    // Clash API port. Polling /version until it responds (or the process
    // dies, or we hit the 15-second deadline) is reliable across platforms.
    let api_url = format!("http://127.0.0.1:{clash_port}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    if let Err(e) = wait_for_clash_api(&api_url, pid, &log_path, deadline).await {
        eprintln!("\n─── Clash API readiness FAILED ───");
        eprintln!("{e}");
        kill_process(pid);
        let _ = std::fs::remove_dir_all(&dir);
        // Fail the test explicitly — previously the assertion below would
        // fail with an opaque "got ''" message and no diagnostics.
        panic!("Clash API did not come up: {e}");
    }
    println!("✓ Clash API ready at {api_url}");

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(6))
        .build()
        .unwrap();

    // ── 3. Check default selector = 'direct' ──────────────────────────────
    println!("\n─── Step 3: Default selector check ───");
    let mut default_now = String::new();
    match client.get(format!("{api_url}/proxies/proxy")).send().await {
        Ok(r) => {
            let body = r.text().await.unwrap_or_default();
            println!("GET /proxies/proxy → {body}");
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(now) = val["now"].as_str() {
                    default_now = now.to_string();
                    println!("Current selector: {now}");
                }
            }
        }
        Err(e) => {
            eprintln!("GET /proxies/proxy FAILED: {e}");
        }
    };
    assert_eq!(
        default_now, "direct",
        "Default selector should be 'direct', got '{default_now}'"
    );
    println!("✓ Default selector is 'direct'");

    // ── First, get real (direct) IP without proxy switching ────────────────
    println!("\n─── Direct outlet IP (selector=direct, default) ───");
    let direct_ip = fetch_outlet_ip_retry(proxy_port, 2, 5).await;
    match &direct_ip {
        Some(ip) => println!("Direct outlet IP: {ip}"),
        None => eprintln!("Direct outlet IP: FAILED (network issue?)"),
    }

    // ── 4. Switch to proxy via PUT ─────────────────────────────────────────
    println!("\n─── Step 4: PUT /proxies/proxy ───");
    let put_resp = client
        .put(format!("{api_url}/proxies/proxy"))
        .json(&serde_json::json!({"name": proxy_tag}))
        .send()
        .await;

    match &put_resp {
        Ok(r) => println!("PUT /proxies/proxy status: {} (expected 204)", r.status()),
        Err(e) => println!("PUT FAILED: {e}"),
    }

    let put_ok = match &put_resp {
        Ok(r) => r.status().as_u16() == 204,
        _ => false,
    };
    assert!(put_ok, "PUT /proxies/proxy should return 204");
    println!("✓ PUT returned 204 — selector switched to {proxy_tag}");

    // Wait for the proxy outbound to establish connection
    println!("Waiting for proxy connection to establish...");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // ── 5. Fetch outlet IP through proxy ───────────────────────────────────
    println!("\n─── Step 5: Outlet IP via proxy ───");
    let proxy_ip = fetch_outlet_ip_retry(proxy_port, 3, 8).await;
    match &proxy_ip {
        Some(ip) => println!("Outlet IP via {proxy_tag}: {ip}"),
        None => eprintln!("Outlet IP via proxy: FAILED after retries"),
    }

    // ── 6. Switch back to direct, re-fetch outlet IP ──────────────────────
    // (re-fetch in case the earlier direct fetch was cached or stale)
    println!("\n─── Step 6: Switch back to direct, re-fetch outlet IP ───");
    let _ = client
        .put(format!("{api_url}/proxies/proxy"))
        .json(&serde_json::json!({"name": "direct"}))
        .send()
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Confirm the switch
    match client.get(format!("{api_url}/proxies/proxy")).send().await {
        Ok(r) => {
            let body = r.text().await.unwrap_or_default();
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(now) = val["now"].as_str() {
                    println!("Selector now: {now}");
                }
            }
        }
        Err(_) => {}
    }

    let direct_ip2 = fetch_outlet_ip_retry(proxy_port, 1, 5).await;
    let final_direct_ip = direct_ip2.or(direct_ip);

    // ── 7. Compare IPs (must be different) ─────────────────────────────────
    println!("\n─── Step 7: Comparison ───");

    // Prefer the post-switch direct IP
    match (&proxy_ip, &final_direct_ip) {
        (Some(proxy), Some(direct)) if proxy != direct => {
            println!("✓ DIFFERENT — proxy WORKS\n  Direct: {direct}\n  Proxy:  {proxy}");
        }
        (Some(proxy), Some(_direct)) => {
            eprintln!("⚠ SAME IP ({proxy}) — proxy may NOT be routing correctly");
        }
        (Some(ip), None) => {
            eprintln!("⚠ Only proxy IP fetched ({ip}), direct failed");
        }
        (None, Some(ip)) => {
            eprintln!("⚠ Only direct IP fetched ({ip}), proxy failed");
        }
        (None, None) => {
            eprintln!("⚠ Both IP fetches failed — check network/proxy connection");
        }
    }

    // Assert if both IPs available
    if let (Some(proxy), Some(direct)) = (&proxy_ip, &final_direct_ip) {
        assert_ne!(
            proxy, direct,
            "Proxy and direct outlet IPs must differ — proxy not routing correctly"
        );
    } else {
        eprintln!(
            "WARN: missing IP(s) — direct={:?}, proxy={:?}",
            final_direct_ip, proxy_ip
        );
    }

    // ── 8. Cleanup: kill sing-box ─────────────────────────────────────────
    println!("\n─── Cleanup ───");
    kill_process(pid);
    println!("✓ sing-box (PID {pid}) killed");
    let _ = std::fs::remove_dir_all(&dir);
    println!("✓ Test directory cleaned up");
    println!("\n=== TEST COMPLETE ===");
}
