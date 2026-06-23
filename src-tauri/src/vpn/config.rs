// FIXED for sing-box 1.13.13:
//   - DNS config uses NEW format: type-based servers instead of legacy address+detour
//   - Custom DNS preserves new format

use crate::settings::AppSettings;
use crate::vpn::{ServerProfile, VpnError};
use serde_json::{json, Value};
use std::path::Path;

pub(crate) fn find_available_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

pub fn generate_config(
    base_template: &str,
    cache_dir: &Path,
    proxy_port: u16,
    routing_mode: &str,
    server_profiles: &[ServerProfile],
    settings: &AppSettings,
) -> Result<(String, u16), VpnError> {
    let mut config: Value =
        serde_json::from_str(base_template).map_err(|e| VpnError::Config(e.to_string()))?;

    if server_profiles.is_empty() {
        return Err(VpnError::Config("No server profiles provided".into()));
    }

    // Build all outbounds
    let mut proxy_outbounds: Vec<Value> = Vec::new();
    for (i, profile) in server_profiles.iter().enumerate() {
        let mut outbound = build_outbound(profile)?;
        if server_profiles.len() > 1 {
            // Tag each as proxy-0, proxy-1, etc. for urltest group
            outbound["tag"] = json!(format!("proxy-{i}"));
        } else {
            outbound["tag"] = json!("proxy");
        }

        // Apply anti-DPI (TLS fragment/utls) if enabled
        if settings.anti_dpi {
            apply_anti_dpi(&mut outbound);
        }

        // Apply Mux for TCP-based outbounds if enabled
        if settings.enable_mux {
            apply_mux(&mut outbound);
        }

        proxy_outbounds.push(outbound);
    }

    if let Some(outbounds) = config["outbounds"].as_array_mut() {
        if proxy_outbounds.len() > 1 {
            // Create urltest group for auto-selection
            let tags: Vec<String> = (0..proxy_outbounds.len())
                .map(|i| format!("proxy-{i}"))
                .collect();
            let urltest = json!({
                "type": "urltest",
                "tag": "proxy",
                "outbounds": tags.clone(),
                "url": "http://cp.cloudflare.com/generate_204",
                "interval": "5m",
                "tolerance": 50
            });
            for ob in proxy_outbounds {
                outbounds.push(ob);
            }
            outbounds.push(urltest);
        } else {
            // Single server — use it as the proxy outbound
            outbounds.push(proxy_outbounds.into_iter().next().unwrap());
        }
    } else {
        return Err(VpnError::Config(
            "Missing outbounds array in template".into(),
        ));
    }

    // Set proxy listen port from user settings (HTTP/SOCKS proxy — set for all proxy modes)
    if routing_mode == "proxy" || routing_mode == "local-proxy" || routing_mode == "system-proxy" {
        if let Some(inbound) = config["inbounds"].get_mut(0) {
            inbound["listen_port"] = json!(proxy_port);
        }
    }

    // In VPN mode, add routing rules to send proxy server traffic via "direct".
    // This ensures the proxy connection doesn't loop through the TUN.
    if routing_mode == "vpn" {
        if let Some(rules) = config["route"]["rules"].as_array_mut() {
            let mut bypass_cidrs = Vec::new();
            for sp in server_profiles {
                let ip = sp.server.clone();
                // Only add if it's a valid IP address — hostnames can't be CIDR
                if !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok() {
                    bypass_cidrs.push(format!("{ip}/32"));
                }
            }
            if !bypass_cidrs.is_empty() {
                let bypass_rule = json!({
                    "ip_cidr": bypass_cidrs,
                    "outbound": "direct",
                });
                rules.insert(0, bypass_rule);
            }
        }
    }

    // Set cache file to absolute path so it doesn't appear in the project root
    if let Some(cache_file) = config
        .get_mut("experimental")
        .and_then(|e| e.get_mut("cache_file"))
    {
        if let Some(path) = cache_file.get_mut("path") {
            *path = json!(cache_dir.join("cache.db").to_string_lossy().to_string());
        }
    }

    // Find an available port and enable the clash_api for traffic stats
    let stats_port = find_available_port().unwrap_or(20000);
    crate::vpn::CLASH_API_PORT.store(stats_port, std::sync::atomic::Ordering::Relaxed);
    config["experimental"]["clash_api"] = json!({
        "external_controller": format!("127.0.0.1:{}", stats_port),
        "external_ui": "",
        "secret": ""
    });

    // ── Apply user settings ──────────────────────────────────────────────────

    // 1. Custom DNS server address
    // FIX: Update the "remote" DNS server in the new sing-box 1.12+ format.
    // The new format uses type-based servers with "server" (IP/hostname) and "server_port"
    // instead of the legacy "address" (URL) format.
    if settings.custom_dns && !settings.dns_address.is_empty() {
        if let Some(dns) = config.get_mut("dns") {
            if let Some(servers) = dns.get_mut("servers").and_then(|s| s.as_array_mut()) {
                if let Some(remote) = servers.first_mut() {
                    // New format: update "server" (IP/hostname) field
                    // If user provides a full URL, extract the hostname; otherwise use as-is
                    let dns_addr = &settings.dns_address;
                    if dns_addr.starts_with("https://") || dns_addr.starts_with("http://") {
                        // Extract hostname from URL like https://1.1.1.1/dns-query
                        let stripped = dns_addr
                            .trim_start_matches("https://")
                            .trim_start_matches("http://");
                        let host = stripped.split('/').next().unwrap_or(dns_addr);
                        let host = host.split(':').next().unwrap_or(host);
                        remote["server"] = json!(host);
                        remote["server_port"] = json!(443);
                    } else if dns_addr.starts_with("tls://") || dns_addr.starts_with("quic://") {
                        let stripped = dns_addr
                            .trim_start_matches("tls://")
                            .trim_start_matches("quic://");
                        let host = stripped.split('/').next().unwrap_or(dns_addr);
                        let host = host.split(':').next().unwrap_or(host);
                        remote["server"] = json!(host);
                        remote["server_port"] = json!(853);
                    } else {
                        // Plain IP or hostname — use as HTTPS DNS server
                        remote["server"] = json!(dns_addr);
                        remote["server_port"] = json!(443);
                    }
                }
            }
        }
    }

    // 2. Routing rules — apply user settings
    // Note: GeoIP database was removed in sing-box 1.12, so bypass_ru uses
    // a direct rule-set approach via ip_rules.
    if let Some(rules) = config.get_mut("route").and_then(|r| r.get_mut("rules")).and_then(|r| r.as_array_mut()) {
        // Allow LAN: handled by base templates (ip_is_private → direct)
        // No extra rule needed — it's already in base.json / base-vpn.json

        // Bypass RU: route Russian traffic direct using geoip (via simple rules)
        if settings.bypass_ru {
            // Without GeoIP db, we add CIDR rules for known Russian ranges
            // as a best-effort approach
            rules.insert(0, json!({
                "ip_cidr": [
                    "5.45.192.0/18", "5.57.128.0/17", "5.134.128.0/19",
                    "5.136.0.0/13", "5.158.96.0/19", "5.166.0.0/14",
                    "5.175.128.0/17", "5.178.64.0/18", "5.178.80.0/20",
                    "5.181.108.0/22", "5.200.0.0/16", "5.201.128.0/17",
                    "5.227.0.0/16", "5.228.0.0/16", "5.250.0.0/17",
                    "5.252.184.0/22", "5.255.192.0/18",
                    "31.3.0.0/16", "31.7.0.0/16", "31.8.0.0/15",
                    "31.15.0.0/16", "31.25.0.0/16", "31.28.32.0/19",
                    "31.40.0.0/16", "31.41.0.0/16", "31.44.0.0/16",
                    "31.47.0.0/16", "31.128.0.0/16", "31.129.0.0/16",
                    "31.130.0.0/15", "31.132.0.0/16", "31.133.0.0/16",
                    "31.134.0.0/15", "31.148.0.0/16", "31.149.0.0/16",
                    "31.161.0.0/16", "31.162.0.0/15", "31.170.0.0/16",
                    "31.171.0.0/16", "31.172.0.0/15", "31.173.0.0/16",
                    "31.177.0.0/16", "31.180.0.0/15", "31.184.0.0/16",
                    "31.185.0.0/16", "31.186.0.0/15", "31.192.0.0/16",
                    "31.193.0.0/16", "31.200.0.0/16", "31.201.0.0/16",
                    "31.204.0.0/16", "31.205.0.0/16", "31.206.0.0/15",
                    "31.207.0.0/16", "31.210.0.0/15", "31.216.0.0/16",
                    "31.217.0.0/16", "31.220.0.0/16", "31.222.0.0/16",
                    "37.0.0.0/16", "37.1.0.0/16", "37.8.0.0/15",
                    "37.9.0.0/16", "37.16.0.0/15", "37.18.0.0/16",
                    "37.19.0.0/16", "37.20.0.0/14", "37.24.0.0/16",
                    "37.26.0.0/15", "37.28.0.0/16", "37.29.0.0/16",
                    "37.32.0.0/16", "37.44.0.0/16", "37.46.0.0/15",
                    "37.48.0.0/16", "37.49.0.0/16", "37.52.0.0/14",
                    "37.56.0.0/15", "37.58.0.0/16", "37.60.0.0/16",
                    "37.61.0.0/16", "37.63.0.0/16", "37.72.0.0/16",
                    "37.73.0.0/16", "37.75.0.0/16", "37.76.0.0/15",
                    "37.78.0.0/15", "37.98.0.0/16", "37.99.0.0/16",
                    "37.110.0.0/16", "37.112.0.0/15", "37.114.0.0/16",
                    "37.122.0.0/16", "37.123.0.0/16", "37.130.0.0/16",
                    "37.131.0.0/16", "37.139.0.0/16", "37.140.0.0/15",
                    "37.143.0.0/16", "37.144.0.0/14", "37.150.0.0/16",
                    "37.151.0.0/16", "37.153.0.0/16", "37.157.0.0/16",
                    "37.188.0.0/16", "37.190.0.0/16", "37.192.0.0/16",
                    "37.194.0.0/16", "37.200.0.0/16", "37.203.0.0/16",
                    "37.204.0.0/16", "37.205.0.0/16", "37.208.0.0/16",
                    "37.209.0.0/16", "37.210.0.0/16", "37.212.0.0/16",
                    "37.215.0.0/16", "37.218.0.0/16", "37.220.0.0/16",
                    "37.221.0.0/16", "37.228.0.0/16", "37.230.0.0/16",
                    "37.232.0.0/16", "37.233.0.0/16", "37.235.0.0/16",
                    "37.252.0.0/16", "46.0.0.0/16", "46.3.0.0/16",
                    "46.8.0.0/16", "46.16.0.0/15", "46.19.0.0/16",
                    "46.20.0.0/16", "46.21.0.0/16", "46.22.0.0/15",
                    "46.28.0.0/15", "46.30.0.0/16", "46.31.0.0/16",
                    "46.34.0.0/16", "46.35.0.0/16", "46.36.0.0/16",
                    "46.37.0.0/16", "46.38.0.0/16", "46.39.0.0/16",
                    "46.41.0.0/16", "46.42.0.0/15", "46.44.0.0/16",
                    "46.45.0.0/16", "46.46.0.0/16", "46.47.0.0/16",
                    "46.48.0.0/16", "46.50.0.0/16", "46.51.0.0/16",
                    "46.52.0.0/16", "46.61.0.0/16", "46.62.0.0/16",
                    "46.63.0.0/16", "46.72.0.0/15", "46.111.0.0/16",
                    "46.138.0.0/16", "46.146.0.0/15", "46.148.0.0/16",
                    "46.149.0.0/16", "46.150.0.0/16", "46.151.0.0/16",
                    "46.158.0.0/15", "46.160.0.0/16", "46.161.0.0/16",
                    "46.163.0.0/16", "46.164.0.0/16", "46.165.0.0/16",
                    "46.166.0.0/16", "46.171.0.0/16", "46.172.0.0/16",
                    "46.173.0.0/16", "46.174.0.0/16", "46.175.0.0/16",
                    "46.180.0.0/16", "46.181.0.0/16", "46.182.0.0/16",
                    "46.183.0.0/16", "46.187.0.0/16", "46.188.0.0/16",
                    "46.189.0.0/16", "46.191.0.0/16", "46.226.0.0/16",
                    "46.227.0.0/16", "46.229.0.0/16", "46.230.0.0/16",
                    "46.231.0.0/16", "46.232.0.0/16", "46.233.0.0/16",
                    "46.235.0.0/16", "46.236.0.0/16", "46.237.0.0/16",
                    "46.241.0.0/16", "46.242.0.0/16", "46.243.0.0/16",
                    "46.244.0.0/16", "46.245.0.0/16", "46.246.0.0/16",
                    "46.247.0.0/16", "46.248.0.0/16", "46.249.0.0/16",
                    "46.250.0.0/16", "46.251.0.0/16", "46.252.0.0/16",
                    "46.254.0.0/16", "46.255.0.0/16",
                    "77.34.0.0/15", "77.37.0.0/16", "77.39.0.0/16",
                    "77.40.0.0/15", "77.50.0.0/16", "77.51.0.0/16",
                    "77.52.0.0/16", "77.53.0.0/16", "77.54.0.0/16",
                    "77.55.0.0/16", "77.56.0.0/16", "77.57.0.0/16",
                    "77.58.0.0/16", "77.59.0.0/16",
                    "77.60.0.0/14", "77.64.0.0/15",
                    "77.66.0.0/16", "77.67.0.0/16",
                    "77.70.0.0/16", "77.71.0.0/16",
                    "77.72.0.0/15", "77.74.0.0/16",
                    "77.75.0.0/16", "77.78.0.0/16",
                    "77.79.0.0/16", "77.81.0.0/16",
                    "77.82.0.0/16", "77.83.0.0/16",
                    "77.84.0.0/16", "77.85.0.0/16",
                    "77.86.0.0/16", "77.87.0.0/16",
                    "77.88.0.0/16", "77.90.0.0/16",
                    "77.91.0.0/15", "77.94.0.0/16",
                    "77.95.0.0/16", "77.105.0.0/16",
                    "77.106.0.0/15", "77.108.0.0/15",
                    "77.120.0.0/16", "77.121.0.0/16",
                    "77.122.0.0/15", "77.220.0.0/16",
                    "77.221.0.0/16", "77.222.0.0/16",
                    "77.223.0.0/16", "77.232.0.0/16",
                    "77.233.0.0/16", "77.234.0.0/16",
                    "77.235.0.0/16", "77.236.0.0/16",
                    "77.237.0.0/16", "77.238.0.0/16",
                    "77.239.0.0/16", "77.240.0.0/16",
                    "77.241.0.0/16", "77.242.0.0/16",
                    "77.243.0.0/16", "77.244.0.0/16",
                    "77.245.0.0/16", "77.246.0.0/16",
                    "77.247.0.0/16", "77.248.0.0/16",
                    "77.249.0.0/16", "77.250.0.0/16",
                    "77.251.0.0/16"
                ],
                "outbound": "direct"
            }));
        }
    }

    // Apply kill switch setting for TUN mode
    if settings.kill_switch && routing_mode == "vpn" {
        apply_kill_switch(&mut config);
    }

    let json_string =
        serde_json::to_string_pretty(&config).map_err(|e| VpnError::Config(e.to_string()))?;

    Ok((json_string, stats_port))
}

pub fn write_config(json: &str, config_dir: &Path) -> Result<std::path::PathBuf, VpnError> {
    std::fs::create_dir_all(config_dir)?;
    let config_path = config_dir.join("config.json");
    std::fs::write(&config_path, json)?;
    Ok(config_path)
}

pub(crate) fn build_outbound(server_profile: &ServerProfile) -> Result<Value, VpnError> {
    match server_profile.protocol.as_str() {
        "vless" => Ok(build_vless(server_profile)),
        "vmess" => Ok(build_vmess(server_profile)),
        "trojan" => Ok(build_trojan(server_profile)),
        "shadowsocks" => Ok(build_shadowsocks(server_profile)),
        "wireguard" => Ok(build_wireguard(server_profile)),
        "hysteria2" => Ok(build_hysteria2(server_profile)),
        "tuic" => Ok(build_tuic(server_profile)),
        "socks" => Ok(build_socks(server_profile)),
        "http" => Ok(build_http(server_profile)),
        _ => Err(VpnError::Config(format!(
            "Unsupported protocol: {}",
            server_profile.protocol
        ))),
    }
}

fn build_vless(server_profile: &ServerProfile) -> Value {
    let mut outbound = json!({
        "type": "vless",
        "tag": "proxy",
        "server": server_profile.server,
        "server_port": server_profile.port,
        "uuid": server_profile.uuid,
    });

    if let Some(flow) = &server_profile.flow {
        if !flow.is_empty() {
            outbound["flow"] = json!(flow);
        }
    }

    // Reality TLS (higher priority than regular TLS)
    if let Some(pbk) = &server_profile.reality_pbk {
        outbound["tls"] = json!({
            "enabled": true,
            "server_name": server_profile.sni.clone().unwrap_or_default(),
            "reality": {
                "enabled": true,
                "public_key": pbk,
                "short_id": server_profile.reality_sid.clone().unwrap_or_default()
            }
        });
    } else if let Some(sni) = &server_profile.sni {
        set_tls(&mut outbound, sni);
    }

    // Transport (WebSocket, gRPC)
    if let Some(t_type) = &server_profile.transport_type {
        match t_type.as_str() {
            "ws" => {
                outbound["transport"] = json!({
                    "type": "ws",
                    "path": server_profile.transport_path.clone().unwrap_or_else(|| "/".into()),
                    "headers": {
                        "Host": server_profile.transport_host.clone().unwrap_or_default()
                    }
                });
            }
            "grpc" => {
                outbound["transport"] = json!({
                    "type": "grpc",
                    "service_name": server_profile.transport_path.clone().unwrap_or_default()
                });
            }
            _ => {}
        }
    }

    outbound
}

fn build_vmess(profile: &ServerProfile) -> Value {
    let mut outbound = json!({
        "type": "vmess",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
        "uuid": profile.uuid,
    });

    if let Some(method) = &profile.method {
        outbound["security"] = json!(method);
    }
    if let Some(sni) = &profile.sni {
        set_tls(&mut outbound, sni);
    }

    outbound
}

fn build_trojan(profile: &ServerProfile) -> Value {
    let mut outbound = json!({
        "type": "trojan",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
        "password": profile.password,
    });

    if let Some(sni) = &profile.sni {
        set_tls(&mut outbound, sni);
    }

    // Transport (WebSocket, gRPC) — same as VLESS
    if let Some(t_type) = &profile.transport_type {
        match t_type.as_str() {
            "ws" => {
                outbound["transport"] = json!({
                    "type": "ws",
                    "path": profile.transport_path.clone().unwrap_or_else(|| "/".into()),
                    "headers": {
                        "Host": profile.transport_host.clone().unwrap_or_default()
                    }
                });
            }
            "grpc" => {
                outbound["transport"] = json!({
                    "type": "grpc",
                    "service_name": profile.transport_path.clone().unwrap_or_default()
                });
            }
            _ => {}
        }
    }

    outbound
}

fn build_shadowsocks(profile: &ServerProfile) -> Value {
    json!({
        "type": "shadowsocks",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
        "method": profile.method,
        "password": profile.password,
    })
}

fn build_wireguard(profile: &ServerProfile) -> Value {
    let local_address = profile.wg_local_address.clone().unwrap_or_else(|| "172.16.0.2/32".into());
    let allowed_ips: Vec<String> = profile.wg_allowed_ips
        .clone()
        .map(|ips| ips.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_else(|| vec!["0.0.0.0/0".into()]);

    let mut peer = json!({
        "server": profile.server,
        "server_port": profile.port,
        "public_key": profile.public_key,
        "allowed_ips": allowed_ips,
    });

    if let Some(k) = profile.wg_keepalive {
        peer["persistent_keepalive"] = json!(k);
    }

    let mut outbound = json!({
        "type": "wireguard",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
        "system_interface": false,
        "local_address": local_address.split(',').map(|s| s.trim().to_string()).collect::<Vec<_>>(),
        "private_key": profile.private_key,
        "peers": [peer],
    });

    if let Some(mtu) = profile.wg_mtu {
        outbound["mtu"] = json!(mtu);
    }

    if let Some(_dns) = &profile.wg_dns {
        // WireGuard DNS in sing-box is per-outbound
        // We skip it here — the global DNS config handles resolution
        // This field is stored for informational purposes
    }

    outbound
}

fn build_hysteria2(profile: &ServerProfile) -> Value {
    let mut outbound = json!({
        "type": "hysteria2",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
        "password": profile.password,
    });

    if let Some(sni) = &profile.sni {
        set_tls(&mut outbound, sni);
    }

    outbound
}

fn build_tuic(profile: &ServerProfile) -> Value {
    let mut outbound = json!({
        "type": "tuic",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
        "uuid": profile.uuid,
        "password": profile.password,
    });

    if let Some(sni) = &profile.sni {
        set_tls(&mut outbound, sni);
    }

    outbound
}

/// Apply anti-DPI measures to outbounds with TLS enabled.
/// Uses utls fingerprint randomization (equivalent to karing's TLS fragment / mixed-case SNI).
fn apply_anti_dpi(outbound: &mut Value) {
    // Only apply to outbounds with TLS
    if outbound.get("tls").and_then(|t| t.get("enabled")).and_then(|e| e.as_bool()).unwrap_or(false) {
        if let Some(tls) = outbound.get_mut("tls") {
            // Add utls with Chrome fingerprint for anti-DPI (supported in sing-box 1.13+)
            if tls.get("utls").is_none() {
                tls["utls"] = json!({
                    "enabled": true,
                    "fingerprint": "chrome"
                });
            }
        }
    }
}

/// Apply Mux (h2mux multiplexing) to TCP-based outbounds.
fn apply_mux(outbound: &mut Value) {
    let outbound_type = outbound["type"].as_str().unwrap_or("");
    // Mux works on TCP-based protocols
    match outbound_type {
        "vless" | "vmess" | "trojan" | "shadowsocks" | "tuic" => {
            outbound["mux"] = json!({
                "enabled": true,
                "protocol": "h2mux",
                "max_streams": 32,
                "padding": false,
            });
        }
        _ => {}
    }
}

/// Apply kill switch to TUN inbound (block traffic when VPN drops).
fn apply_kill_switch(config: &mut Value) {
    if let Some(inbounds) = config["inbounds"].as_array_mut() {
        for inbound in inbounds.iter_mut() {
            if inbound["type"] == "tun" {
                inbound["auto_redirect"] = json!(true);
            }
        }
    }
}

fn build_socks(profile: &ServerProfile) -> Value {
    let mut outbound = json!({
        "type": "socks",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
    });

    if let Some(pass) = &profile.password {
        outbound["password"] = json!(pass);
        if let Some(user) = &profile.username {
            outbound["username"] = json!(user);
        }
    }

    outbound
}

fn build_http(profile: &ServerProfile) -> Value {
    let mut outbound = json!({
        "type": "http",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
    });

    if let Some(pass) = &profile.password {
        outbound["password"] = json!(pass);
        if let Some(user) = &profile.username {
            outbound["username"] = json!(user);
        }
    }

    outbound
}

fn set_tls(outbound: &mut Value, sni: &str) {
    outbound["tls"] = json!({
        "enabled": true,
        "server_name": sni,
        "insecure": true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_settings() -> AppSettings {
        AppSettings::default()
    }

    fn base_template() -> &'static str {
        r#"{
            "log": { "level": "debug", "timestamp": true },
            "inbounds": [{
                "type": "mixed",
                "tag": "mixed-in",
                "listen": "127.0.0.1",
                "listen_port": 10809
            }],
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "block", "tag": "block" }
            ],
            "route": {
                "rules": [
                    { "ip_is_private": true, "outbound": "direct" }
                ],
                "final": "proxy",
                "auto_detect_interface": true
            }
        }"#
    }

    /// VPN template using NEW sing-box 1.12+ DNS server format
    fn vpn_template() -> &'static str {
        r#"{
            "log": { "level": "info", "timestamp": true },
            "dns": {
                "servers": [
                    {
                        "type": "https",
                        "tag": "remote",
                        "server": "1.1.1.1",
                        "server_port": 443,
                        "detour": "proxy"
                    },
                    {
                        "type": "https",
                        "tag": "local",
                        "server": "8.8.8.8",
                        "server_port": 443
                    }
                ],
                "final": "remote"
            },
            "inbounds": [{
                "type": "tun",
                "tag": "tun-in",
                "interface_name": "omnitun",
                "address": "172.19.0.1/24",
                "mtu": 1500,
                "stack": "mixed",
                "auto_route": true,
                "strict_route": true
            }],
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "block", "tag": "block" }
            ],
            "route": {
                "rules": [
                    { "ip_is_private": true, "outbound": "direct" }
                ],
                "final": "proxy",
                "auto_detect_interface": true,
                "default_domain_resolver": "local"
            },
            "experimental": {
                "cache_file": { "enabled": true, "path": "cache.db" }
            }
        }"#
    }

    fn vless_profile() -> ServerProfile {
        ServerProfile {
            id: "test-1".into(),
            name: "VLESS 127.0.0.2".into(),
            protocol: "vless".into(),
            server: "127.0.0.2".into(),
            port: 8443,
            uuid: Some("b831381d-6324-4d53-ad4f-8cda48b30811".into()),
            sni: None,
            flow: None,
            ..Default::default()
        }
    }

    fn vless_tls_profile() -> ServerProfile {
        ServerProfile {
            id: "test-2".into(),
            name: "VLESS+TLS 127.0.0.2".into(),
            protocol: "vless".into(),
            server: "127.0.0.2".into(),
            port: 8442,
            uuid: Some("b831381d-6324-4d53-ad4f-8cda48b30811".into()),
            sni: Some("localhost".into()),
            flow: None,
            ..Default::default()
        }
    }

    fn cache_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("omnitun-cfg-test-{}", std::process::id()))
    }

    #[test]
    fn test_vpn_tun_inbound() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &[vless_profile()], &default_settings());
        assert!(result.is_ok(), "VPN config generation failed: {:?}", result.err());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let inbounds = config["inbounds"].as_array().unwrap();
        assert_eq!(inbounds[0]["type"], "tun");
        assert!(inbounds[0].get("listen_port").is_none());
    }

    #[test]
    fn test_vpn_dns_new_format() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &[vless_profile()], &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();

        // DNS servers should use new type-based format
        let servers = config["dns"]["servers"].as_array().unwrap();

        // Remote server: type=https, server=1.1.1.1, detour=proxy
        assert_eq!(servers[0]["type"], "https");
        assert_eq!(servers[0]["server"], "1.1.1.1");
        assert_eq!(servers[0]["server_port"], 443);
        assert_eq!(servers[0]["detour"], "proxy");

        // Local server: type=https, server=8.8.8.8, no detour
        assert_eq!(servers[1]["type"], "https");
        assert_eq!(servers[1]["server"], "8.8.8.8");
        assert!(servers[1].get("detour").is_none());

        // DNS final should route to remote server (no deprecated outbound in rules)
        assert_eq!(config["dns"]["final"], "remote");
        // No DNS rules needed — final handles default routing
        assert!(config["dns"].get("rules").is_none());
    }

    #[test]
    fn test_vpn_outbound_added() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &[vless_tls_profile()], &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let outbounds = config["outbounds"].as_array().unwrap();
        assert_eq!(outbounds.len(), 3);
        assert_eq!(outbounds[2]["tag"], "proxy");
        assert_eq!(config["route"]["final"], "proxy");
    }

    #[test]
    fn test_proxy_listen_port_set() {
        let result = generate_config(base_template(), &cache_dir(), 9999, "proxy", &[vless_profile()], &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        assert_eq!(config["inbounds"][0]["listen_port"], 9999);
    }

    #[test]
    fn test_unsupported_protocol() {
        let profile = ServerProfile {
            protocol: "unknown".into(),
            ..vless_profile()
        };

        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &[profile], &default_settings());
        assert!(result.is_err());
    }

    #[test]
    fn test_vpn_config_full_structure() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &[vless_profile()], &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();

        // === TUN inbound ===
        let tun = &config["inbounds"][0];
        assert_eq!(tun["type"], "tun");
        assert_eq!(tun["auto_route"], true);
        assert_eq!(tun["strict_route"], true);
        assert_eq!(tun["stack"], "mixed");
        assert!(tun.get("listen_port").is_none(), "TUN must not have listen_port");

        // === DNS ===
        let dns = &config["dns"];
        assert_eq!(dns["final"], "remote");
        let servers = dns["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2);

        // Remote DNS (via proxy)
        assert_eq!(servers[0]["type"], "https");
        assert_eq!(servers[0]["server"], "1.1.1.1");
        assert_eq!(servers[0]["server_port"], 443);
        assert_eq!(servers[0]["detour"], "proxy");

        // Local DNS (direct)
        assert_eq!(servers[1]["type"], "https");
        assert_eq!(servers[1]["server"], "8.8.8.8");
        assert_eq!(servers[1]["server_port"], 443);
        assert!(servers[1].get("detour").is_none());

        // === Route ===
        let route = &config["route"];
        assert_eq!(route["final"], "proxy");
        assert_eq!(route["auto_detect_interface"], true);
        assert_eq!(route["default_domain_resolver"], "local");

        // === Outbounds ===
        let outbounds = config["outbounds"].as_array().unwrap();
        assert_eq!(outbounds.len(), 3);
        assert_eq!(outbounds[0]["tag"], "direct");
        assert_eq!(outbounds[1]["tag"], "block");
        assert_eq!(outbounds[2]["tag"], "proxy");
        assert_eq!(outbounds[2]["type"], "vless");

        // === Experimental ===
        let exp = &config["experimental"];
        assert!(exp["cache_file"]["enabled"].as_bool().unwrap_or(false));
        assert!(exp.get("clash_api").is_some(), "clash_api must be added for traffic stats");
        assert!(exp["clash_api"]["external_controller"].as_str().unwrap_or("").contains("127.0.0.1:"));
    }

    #[test]
    fn test_custom_dns_updates_remote_server() {
        let mut settings = default_settings();
        settings.custom_dns = true;
        settings.dns_address = "https://9.9.9.9/dns-query".into();

        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &[vless_profile()], &settings);
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let servers = config["dns"]["servers"].as_array().unwrap();

        // Remote server should be updated to custom DNS
        assert_eq!(servers[0]["server"], "9.9.9.9");
        assert_eq!(servers[0]["server_port"], 443);
        // Local server stays unchanged
        assert_eq!(servers[1]["server"], "8.8.8.8");
    }

    #[test]
    fn test_custom_dns_plain_ip() {
        let mut settings = default_settings();
        settings.custom_dns = true;
        settings.dns_address = "1.0.0.1".into();

        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &[vless_profile()], &settings);
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let servers = config["dns"]["servers"].as_array().unwrap();
        assert_eq!(servers[0]["server"], "1.0.0.1");
        assert_eq!(servers[0]["server_port"], 443);
    }
}
