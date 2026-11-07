//! Outlet IP comparison test.
//! Tests: profiles.json entry country vs actual exit IP country.
//! Run: cargo test --test outlet_ip_test -- --nocapture
//!
//! For each server: start temp sing-box (all servers on separate inbound ports),
//! fetch exit IP through each, GeoIP, compare with stored entry data.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Library")
        .join("Application Support");

    #[cfg(target_os = "linux")]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let base = std::env::temp_dir();

    base.join("com.omnitun.desktop")
}

fn singbox() -> PathBuf {
    #[cfg(target_os = "windows")]
    let binary_name = "sing-box.exe";
    #[cfg(not(target_os = "windows"))]
    let binary_name = "sing-box";

    let bundled = app_data_dir().join("binaries").join(binary_name);
    if bundled.is_file() {
        return bundled;
    }

    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(binary_name))
            .find(|candidate| candidate.is_file())
    }) {
        return path;
    }

    PathBuf::from(binary_name)
}

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port(skip: &[u16]) -> Option<u16> {
    (20000..60000u16).find(|&p| {
        if skip.contains(&p) {
            return false;
        }
        std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
    })
}

fn load_servers() -> Vec<serde_json::Value> {
    let p = app_data_dir().join("profiles.json");
    let content = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: cannot read profiles.json at {}: {e}", p.display());
            return Vec::new();
        }
    };
    let d: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("SKIP: invalid JSON in profiles.json: {e}");
            return Vec::new();
        }
    };
    match d[0]["servers"].as_array() {
        Some(arr) => arr.clone(),
        None => {
            eprintln!("SKIP: no servers in profiles.json");
            Vec::new()
        }
    }
}

#[tokio::test]
async fn outlet_vs_entry_country() {
    let servers = load_servers();
    if servers.is_empty() {
        eprintln!("SKIP: no servers loaded");
        return;
    }
    let bin = singbox();
    if !bin.exists() {
        eprintln!("SKIP: no sing-box");
        return;
    }

    // Build config: each server receives its own inbound port
    let mut obs: Vec<serde_json::Value> = vec![
        serde_json::json!({"type": "direct", "tag": "direct"}),
        serde_json::json!({"type": "block", "tag": "block"}),
    ];
    let mut inbs = Vec::new();
    let mut routes = Vec::new();
    let mut ports = Vec::new();
    let mut labels: Vec<(usize, String, String)> = Vec::new();

    for (i, sv) in servers.iter().enumerate() {
        let sp: omnitun_lib::proxy::ServerProfile = match serde_json::from_value(sv.clone()) {
            Ok(s) => s,
            _ => continue,
        };
        let p = match free_port(&ports) {
            Some(x) => x,
            _ => continue,
        };
        let tag = format!("s{i}");
        if let Ok(mut ob) = omnitun_lib::proxy::config::build_outbound(&sp) {
            ob["tag"] = serde_json::json!(&tag);
            inbs.push(serde_json::json!({"type": "mixed","tag":format!("in{i}"),
                "listen":"127.0.0.1","listen_port":p}));
            ports.push(p);
            obs.push(ob);
            routes.push(serde_json::json!({"inbound_port":[p],"outbound":tag}));
            labels.push((
                i,
                sv["flag"].as_str().unwrap_or("?").to_string(),
                sv["country_name"].as_str().unwrap_or("?").to_string(),
            ));
        }
    }

    if ports.is_empty() {
        eprintln!("No servers");
        return;
    }
    println!("Testing {} servers via sing-box", ports.len());

    let cfg = serde_json::json!({
        "log":{"level":"error"},
        "dns":{"servers":[{"type":"https","tag":"l","server":"8.8.8.8","server_port":443}],"final":"l"},
        "inbounds": inbs, "outbounds": obs,
        "route":{"rules":routes,"final":"direct","auto_detect_interface":true,"default_domain_resolver":"local"},
    });

    let dir = std::env::temp_dir().join(format!("ot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_p = dir.join("c.json");
    std::fs::write(&cfg_p, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

    if !Command::new(&bin)
        .args(["check", "-c"])
        .arg(&cfg_p)
        .output()
        .unwrap()
        .status
        .success()
    {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let child = Command::new(&bin)
        .args(["run", "-c"])
        .arg(&cfg_p)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _child_guard = ChildGuard(child);
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Sequential test (simpler, avoids borrow issues)
    let _client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .unwrap();

    println!(
        "\n{:>3} | {:<8} {:<16} | {:<20} | {:<15} | Match",
        "#", "Entry", "Server IP", "Exit country", "Exit IP"
    );
    println!("{}", "-".repeat(90));

    let mut ok = 0u32;
    let mut fail = 0u32;
    for (idx, (&p, (orig, ref flag, ref _name))) in ports.iter().zip(labels.iter()).enumerate() {
        let sv = &servers[*orig];
        let srv_ip = sv["server"].as_str().unwrap_or("?");

        // Fetch exit IP through this server's port
        let proxy = match reqwest::Proxy::http(format!("http://127.0.0.1:{p}")) {
            Ok(pr) => pr,
            _ => {
                fail += 1;
                println!(
                    "{:>3} | {:<8} {:<16} | {:<20}",
                    idx + 1,
                    flag,
                    srv_ip,
                    "PROXY ERROR"
                );
                continue;
            }
        };
        let pc = match reqwest::Client::builder()
            .proxy(proxy)
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            _ => {
                fail += 1;
                println!(
                    "{:>3} | {:<8} {:<16} | {:<20}",
                    idx + 1,
                    flag,
                    srv_ip,
                    "CLIENT ERROR"
                );
                continue;
            }
        };

        let outlet = match pc.get("https://ifconfig.me/ip").send().await {
            Ok(r) if r.status().is_success() => match r.text().await {
                Ok(t) => {
                    let ip = t.trim().to_string();
                    if ip.is_empty() {
                        None
                    } else {
                        Some(ip)
                    }
                }
                _ => None,
            },
            _ => None,
        };

        let (exit_country, exit_ip, matched) = match outlet {
            Some(ref ip) => match omnitun_lib::proxy::geoip::resolve_hostname(ip).await {
                Some(g) => {
                    let m = g.country_code == *flag;
                    if m {
                        ok += 1;
                    } else {
                        fail += 1;
                    }
                    (
                        format!("{} {}", g.country_code, g.country_name),
                        ip.clone(),
                        m,
                    )
                }
                None => {
                    fail += 1;
                    ("NO GEOIP".into(), ip.clone(), false)
                }
            },
            None => {
                fail += 1;
                ("TIMEOUT".into(), String::new(), false)
            }
        };

        println!(
            "{:>3} | {:<8} {:<16} | {:<20} | {:<15} | {}",
            idx + 1,
            flag,
            srv_ip,
            exit_country,
            exit_ip,
            if matched { "✓" } else { "✗ MISMATCH" }
        );
    }

    println!(
        "\n✓ Matched: {}/{}  ✗ Differed/Error: {}/{}",
        ok,
        ok + fail,
        fail,
        ok + fail
    );

    drop(_child_guard);
    let _ = std::fs::remove_dir_all(&dir);
}
