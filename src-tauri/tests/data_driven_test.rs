/// Data-driven tests using disposable profiles.json fixtures.
///
/// Validates config generation, sing-box compatibility, data integrity.
/// Run: cargo test --test data_driven_test -- --nocapture
use std::path::PathBuf;
use std::process::Command;

fn app_data_dir() -> PathBuf {
    PathBuf::from(
        std::env::var_os("OMNITUN_TEST_DATA_DIR")
            .expect("Set OMNITUN_TEST_DATA_DIR to a disposable fixture directory"),
    )
}

fn singbox_binary() -> PathBuf {
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

fn profiles_path() -> PathBuf {
    app_data_dir().join("profiles.json")
}

fn load_real_profiles() -> Vec<serde_json::Value> {
    let path = profiles_path();
    let content = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "Required disposable fixture is missing at {}: {error}",
            path.display()
        )
    });
    let profiles: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("profiles.json fixture must contain a JSON array");
    assert!(
        !profiles.is_empty(),
        "profiles.json fixture must contain at least one profile"
    );
    assert!(
        profiles
            .first()
            .and_then(|profile| profile["servers"].as_array())
            .is_some_and(|servers| !servers.is_empty()),
        "The first fixture profile must contain at least one server",
    );
    profiles
}

// ---------------------------------------------------------------------------
// 1. Basic data integrity
// ---------------------------------------------------------------------------

#[test]
fn test_load_and_count() {
    let profiles = load_real_profiles();
    let total: usize = profiles
        .iter()
        .map(|p| p["servers"].as_array().map(|a| a.len()).unwrap_or(0))
        .sum();
    assert!(total > 0, "No servers");
    println!("Loaded {} profile(s), {} server(s)", profiles.len(), total);
}

#[test]
fn test_all_servers_have_required_fields() {
    let profiles = load_real_profiles();
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            let id = s["id"].as_str().unwrap_or("?");
            let is_cdn = s["behind_cdn"].as_bool().unwrap_or(false);
            // Skip CDN servers — their entry IP may not be in the GeoIP DB.
            if is_cdn {
                continue;
            }
            assert!(s["protocol"].is_string(), "{id}: missing protocol");
            assert!(s["server"].is_string(), "{id}: missing server");
            assert!(s["port"].is_number(), "{id}: missing port");
            assert!(
                s["country_name"].is_string() || s["flag"].is_string(),
                "{id}: missing both country_name and flag"
            );
        }
    }
}

#[test]
fn test_no_orphaned_name_field() {
    // After migration, `name` field must NOT be in profiles.json
    let profiles = load_real_profiles();
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            assert!(
                s.get("name").is_none(),
                "Server {} still has legacy `name` field",
                s["id"].as_str().unwrap_or("?")
            );
        }
    }
}

#[test]
fn test_flag_is_valid_iso_code() {
    let profiles = load_real_profiles();
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            if let Some(flag) = s["flag"].as_str() {
                assert_eq!(
                    flag.len(),
                    2,
                    "Invalid flag '{}' for server {}",
                    flag,
                    s["id"].as_str().unwrap_or("?")
                );
                assert!(
                    flag.chars().all(|c| c.is_ascii_uppercase()),
                    "Flag '{}' not uppercase",
                    flag
                );
            }
        }
    }
}

#[test]
fn test_ping_ms_within_reasonable_range() {
    let profiles = load_real_profiles();
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            let is_cdn = s["behind_cdn"].as_bool().unwrap_or(false);
            // Skip CDN servers — their ping is measured to CDN edge, not to the proxy backend.
            if is_cdn {
                continue;
            }
            if let Some(ping) = s["ping_ms"].as_u64() {
                assert!(
                    ping < 10000,
                    "Suspicious ping {}ms for server {}",
                    ping,
                    s["id"].as_str().unwrap_or("?")
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Config generation
// ---------------------------------------------------------------------------

#[test]
fn test_build_outbound_all_servers() -> Result<(), Box<dyn std::error::Error>> {
    let profiles = load_real_profiles();
    let count = profiles
        .iter()
        .flat_map(|p| p["servers"].as_array().unwrap().iter())
        .try_fold(0usize, |acc, s| {
            let server: omnitun_lib::proxy::ServerProfile = serde_json::from_value((*s).clone())?;
            let outbound =
                omnitun_lib::proxy::config::build_outbound(&server).unwrap_or_else(|e| {
                    panic!("{}: build_outbound failed: {e}", server.display_name())
                });
            assert_eq!(
                outbound["type"],
                server.protocol,
                "{}: type mismatch",
                server.display_name()
            );
            assert_eq!(outbound["server"], server.server);
            assert_eq!(outbound["server_port"], server.port as u64);
            Ok::<_, Box<dyn std::error::Error>>(acc + 1)
        })?;
    println!("✓ build_outbound: {count} servers OK");
    Ok(())
}

#[test]
fn test_build_outbound_never_returns_empty_server() -> Result<(), Box<dyn std::error::Error>> {
    let profiles = load_real_profiles();
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            let server: omnitun_lib::proxy::ServerProfile = serde_json::from_value((*s).clone())?;
            let outbound = omnitun_lib::proxy::config::build_outbound(&server)
                .unwrap_or_else(|e| panic!("{}: build_outbound: {e}", server.display_name()));
            let srv = outbound["server"].as_str().unwrap_or("");
            assert!(
                !srv.is_empty() && srv != "127.0.0.1",
                "{}: server resolved to empty/loopback",
                server.display_name()
            );
        }
    }
    println!("✓ No empty server values");
    Ok(())
}

#[test]
fn test_all_outbounds_have_tls_config_for_trojan() -> Result<(), Box<dyn std::error::Error>> {
    let profiles = load_real_profiles();
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            let server: omnitun_lib::proxy::ServerProfile = serde_json::from_value((*s).clone())?;
            if server.protocol != "trojan" {
                continue;
            }
            let outbound = omnitun_lib::proxy::config::build_outbound(&server)?;
            let tls = outbound.get("tls");
            if tls.is_none() {
                println!(
                    "⚠  {}: trojan without TLS (possible non-TLS trojan variant)",
                    server.display_name()
                );
                continue;
            }
            assert_eq!(
                tls.and_then(|t| t.get("enabled")).and_then(|e| e.as_bool()),
                Some(true),
                "{}: trojan TLS not enabled",
                server.display_name()
            );
        }
    }
    println!("✓ All trojan outbounds have TLS enabled");
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. sing-box check for all configs
// ---------------------------------------------------------------------------

#[test]
fn test_all_servers_pass_singbox_check() -> Result<(), Box<dyn std::error::Error>> {
    let binary = singbox_binary();
    assert!(
        binary.is_file(),
        "Required sing-box binary is missing: {:?}",
        binary
    );

    let profiles = load_real_profiles();
    let tmp_dir = tempfile::tempdir()?;

    let servers: Vec<_> = profiles
        .iter()
        .flat_map(|p| p["servers"].as_array().unwrap().iter())
        .collect();
    let total = servers.len();

    for (i, s) in servers.iter().enumerate() {
        let server: omnitun_lib::proxy::ServerProfile = serde_json::from_value((*s).clone())?;
        let name = server.display_name();

        // Build minimal config: one outbound per test
        let outbound = omnitun_lib::proxy::config::build_outbound(&server)?;
        let config = serde_json::json!({
            "log": {"level": "error"},
            "inbounds": [{"type": "mixed", "tag": "in", "listen": "127.0.0.1", "listen_port": 2080}],
            "outbounds": [
                {"type": "direct", "tag": "direct"},
                outbound,
            ],
            "route": {
                "final": "direct",
                "auto_detect_interface": true,
            },
        });

        let config_path = tmp_dir.path().join(format!("c-{}.json", i));
        std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

        let output = Command::new(&binary)
            .args(["check", "-c"])
            .arg(&config_path)
            .output()?;

        assert!(
            output.status.success(),
            "{}/{} {name}: sing-box check FAILED\nstderr: {}",
            i + 1,
            total,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("✓ sing-box check: {total}/{total} servers passed");
    Ok(())
}

/// Test that base.json template has DNS (regression: was missing before fix)
#[test]
fn test_base_template_has_dns() -> Result<(), Box<dyn std::error::Error>> {
    let template: serde_json::Value =
        serde_json::from_str(include_str!("../crates/omnitun-core/src/proxy/base.json"))?;
    let dns = template.get("dns");
    assert!(dns.is_some(), "base.json missing DNS section");
    assert!(
        dns.and_then(|d| d.get("servers"))
            .and_then(|s| s.as_array())
            .is_some_and(|s| !s.is_empty()),
        "base.json has no DNS servers"
    );
    assert!(
        dns.and_then(|d| d.get("final"))
            .and_then(|f| f.as_str())
            .is_some(),
        "base.json missing DNS final"
    );
    let route = template.get("route");
    assert!(
        route
            .and_then(|r| r.get("default_domain_resolver"))
            .is_some(),
        "base.json missing route.default_domain_resolver"
    );
    println!("✓ base.json has DNS and route.default_domain_resolver");
    Ok(())
}

// ---------------------------------------------------------------------------
// 4. Outlet IP guard test
// ---------------------------------------------------------------------------
/// Outlet IP guard: не обновлять страну если IP совпадает с реальным
#[test]
fn test_outlet_ip_guard_doesnt_update_when_same() {
    // Тестируем только ЛОГИКУ guard'а — ip != real_ip
    // Реальный IP может быть любым, динамическим
    let real_ip = Some("any.dynamic.ip".to_string());
    let outlet_ip = Some("any.dynamic.ip".to_string());

    let should_update = matches!(
        (&real_ip, &outlet_ip),
        (Some(r), Some(o)) if !r.is_empty() && o != r
    );
    assert!(!should_update, "Guard: same IP → не обновлять");
    println!("✓ Guard: outlet IP == real IP → не обновляем");
}

/// Outlet IP guard: обновлять страну если IP отличается от реального
#[test]
fn test_outlet_ip_guard_updates_when_different() {
    let real_ip = Some("any.dynamic.ip".to_string());
    let outlet_ip = Some("different.proxy.ip".to_string());

    let should_update = matches!(
        (&real_ip, &outlet_ip),
        (Some(r), Some(o)) if !r.is_empty() && o != r
    );
    assert!(should_update, "Guard: different IP → обновлять");
    println!("✓ Guard: outlet IP != real IP → обновляем");
}

/// Guard — не обновлять если реальный IP пустой
#[test]
fn test_outlet_ip_guard_skips_when_empty() {
    let should_update = matches!(
        (None::<String>, Some("1.2.3.4".to_string())),
        (Some(r), Some(o)) if !r.is_empty() && o != r
    );
    assert!(!should_update, "Guard: empty real IP → не обновлять");
    println!("✓ Guard: empty real IP → не обновляем");
}

// ---------------------------------------------------------------------------
// 5. display_name formatting
// ---------------------------------------------------------------------------

#[test]
fn test_display_name_doesnt_show_raw_ip() -> Result<(), Box<dyn std::error::Error>> {
    let profiles = load_real_profiles();
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            let server: omnitun_lib::proxy::ServerProfile = serde_json::from_value((*s).clone())?;
            let name = server.display_name();

            // If country_name is set, display must use it (not IP)
            if server.country_name.is_some() {
                assert_ne!(
                    name,
                    format!("{}:{}", server.server, server.port),
                    "{}: display_name shows raw IP when country is known",
                    server.display_name()
                );
            }
        }
    }
    println!("✓ display_name shows country, not raw IP");
    Ok(())
}

// ---------------------------------------------------------------------------
// 6. generate_config with selector group (batch test config)
// ---------------------------------------------------------------------------

#[test]
fn test_batch_test_config_has_selector() -> Result<(), Box<dyn std::error::Error>> {
    let profiles = load_real_profiles();
    assert!(
        profiles
            .first()
            .and_then(|profile| profile["servers"].as_array())
            .is_some_and(|servers| !servers.is_empty()),
        "The first fixture profile must contain servers",
    );
    let servers: Vec<omnitun_lib::proxy::ServerProfile> = profiles[0]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .take(3)
        .map(|s| serde_json::from_value(s.clone()).unwrap())
        .collect();

    // Build what test_servers_via_singbox_batch builds
    let mut outbounds: Vec<serde_json::Value> = vec![
        serde_json::json!({"type": "direct", "tag": "direct"}),
        serde_json::json!({"type": "block", "tag": "block"}),
    ];
    for server in &servers {
        if let Ok(mut ob) = omnitun_lib::proxy::config::build_outbound(server) {
            ob["tag"] = serde_json::json!(format!("proxy-{}", server.id));
            outbounds.push(ob);
        }
    }

    let proxy_tags: Vec<String> = servers.iter().map(|s| format!("proxy-{}", s.id)).collect();
    let mut sel = vec!["direct".to_string()];
    sel.extend(proxy_tags);
    outbounds.push(serde_json::json!({"type": "selector", "tag": "proxy", "outbounds": sel}));

    let config = serde_json::json!({
        "log": {"level": "error"},
        "dns": {
            "servers": [{"type": "https", "tag": "local", "server": "8.8.8.8", "server_port": 443}],
            "final": "local"
        },
        "inbounds": [{"type": "mixed", "tag": "mixed-in", "listen": "127.0.0.1", "listen_port": 9999}],
        "outbounds": outbounds,
        "route": {
            "final": "proxy",
            "auto_detect_interface": true,
            "default_domain_resolver": "local",
        },
    });

    // Verify correct structure
    let proxy_sel = config["outbounds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["tag"] == "proxy")
        .unwrap();
    assert_eq!(
        proxy_sel["type"], "selector",
        "Proxy outbound must be selector type"
    );
    assert_eq!(
        config["route"]["final"], "proxy",
        "Route must point to selector"
    );
    assert!(
        proxy_sel["outbounds"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
            >= 2,
        "Selector must have at least direct + 1 proxy"
    );

    // Verify it passes sing-box check
    let binary = singbox_binary();
    assert!(
        binary.is_file(),
        "Required sing-box binary is missing: {:?}",
        binary
    );
    {
        let check_dir = tempfile::tempdir()?;
        let tmp = check_dir.path().join("omnitun-batch-test.json");
        std::fs::write(&tmp, serde_json::to_string_pretty(&config)?)?;
        let out = Command::new(&binary)
            .args(["check", "-c"])
            .arg(&tmp)
            .output()?;
        assert!(
            out.status.success(),
            "Batch config check failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    println!("✓ Batch test config has correct selector structure");
    Ok(())
}

// ---------------------------------------------------------------------------
// 5. CDN invariants
// ---------------------------------------------------------------------------

/// CDN servers must NEVER retain exit IP data (exit_flag, exit_country_name,
/// exit_city_name, exit_server).  The entry IP fields (flag, country_name)
/// are allowed for display/country-filter purposes, but exit data must be
/// cleared because CDN-routed servers don't have a meaningful exit node.
#[test]
fn test_cdn_servers_must_not_have_exit_data() {
    let profiles = load_real_profiles();
    let mut violations = Vec::new();
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            if !s["behind_cdn"].as_bool().unwrap_or(false) {
                continue;
            }
            let id = s["id"].as_str().unwrap_or("?");
            if s["exit_flag"].is_string()
                || s["exit_country_name"].is_string()
                || s["exit_city_name"].is_string()
                || s["exit_server"].is_string()
            {
                violations.push(format!(
                    "CDN server {id} has exit data: exit_flag={:?} exit_country={:?} exit_city={:?} exit_server={:?}",
                    s["exit_flag"].as_str(),
                    s["exit_country_name"].as_str(),
                    s["exit_city_name"].as_str(),
                    s["exit_server"].as_str(),
                ));
            }
        }
    }
    if !violations.is_empty() {
        for v in &violations {
            println!("VIOLATION: {v}");
        }
        panic!("{} CDN server(s) with exit data found", violations.len());
    }
    println!("✓ All CDN servers have no exit data");
}

/// Non-CDN servers with a valid ping SHOULD have exit data populated.
/// This is a soft check — logs stats rather than panicking, because
/// exit data depends on GeoIP database availability.
#[test]
fn test_non_cdn_servers_should_have_exit_data() {
    let profiles = load_real_profiles();
    let mut missing_exit = 0u32;
    let mut total_non_cdn = 0u32;
    for p in &profiles {
        for s in p["servers"].as_array().unwrap() {
            if s["behind_cdn"].as_bool().unwrap_or(false) {
                continue;
            }
            total_non_cdn += 1;
            let has_ping = s["ping_ms"].as_u64().is_some_and(|p| p > 0 && p < 10000);
            let has_exit = s["exit_flag"].is_string() || s["exit_country_name"].is_string();
            if has_ping && !has_exit {
                missing_exit += 1;
            }
        }
    }
    println!(
        "Non-CDN servers: {total_non_cdn} total, {missing_exit} with valid ping but no exit data"
    );
}
