// FIXED for sing-box 1.13.13:
//   - DNS config uses NEW format: type-based servers instead of legacy address+detour
//   - Custom DNS preserves new format
//   - Added read_singbox_log() for crash diagnostics

use crate::settings::AppSettings;
use crate::vpn::{ServerProfile, VpnError};
use serde_json::{json, Value};
use std::path::Path;

fn find_available_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

/// Read the sing-box log file and return its contents (for error diagnostics).
pub fn read_singbox_log(config_dir: &Path) -> Option<String> {
    let log_path = config_dir.join("sing-box.log");
    if log_path.exists() {
        std::fs::read_to_string(&log_path).ok()
    } else {
        None
    }
}

pub fn generate_config(
    base_template: &str,
    cache_dir: &Path,
    proxy_port: u16,
    routing_mode: &str,
    server_profile: &ServerProfile,
    settings: &AppSettings,
) -> Result<(String, u16), VpnError> {
    let mut config: Value =
        serde_json::from_str(base_template).map_err(|e| VpnError::Config(e.to_string()))?;

    let outbound = build_outbound(server_profile)?;

    if let Some(outbounds) = config["outbounds"].as_array_mut() {
        outbounds.push(outbound);
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

    // In VPN mode, add a routing rule to send proxy server traffic via "direct".
    // This ensures the proxy connection doesn't loop through the TUN.
    // Note: we use route.rules rather than TUN's `exclude_route` (not available in sing-box 1.13).
    if routing_mode == "vpn" {
        let proxy_ip = server_profile.server.clone();
        if let Some(rules) = config["route"]["rules"].as_array_mut() {
            let bypass_rule = json!({
                "ip_cidr": [format!("{proxy_ip}/32")],
                "outbound": "direct",
            });
            rules.insert(0, bypass_rule);
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

    // 2. Routing rules
    // Note: GeoIP rules (bypass_ru, allowed/blocked countries) are disabled because
    // GeoIP database was removed in sing-box 1.12. Feature requires rule-set migration.

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

fn build_outbound(server_profile: &ServerProfile) -> Result<Value, VpnError> {
    match server_profile.protocol.as_str() {
        "vless" => Ok(build_vless(server_profile)),
        "vmess" => Ok(build_vmess(server_profile)),
        "trojan" => Ok(build_trojan(server_profile)),
        "shadowsocks" => Ok(build_shadowsocks(server_profile)),
        "wireguard" => Ok(build_wireguard(server_profile)),
        "hysteria2" => Ok(build_hysteria2(server_profile)),
        "tuic" => Ok(build_tuic(server_profile)),
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
    json!({
        "type": "wireguard",
        "tag": "proxy",
        "server": profile.server,
        "server_port": profile.port,
        "system_interface": false,
        "local_address": ["172.16.0.2/32"],
        "private_key": profile.private_key,
        "peers": [{
            "address": profile.server,
            "port": profile.port,
            "public_key": profile.public_key,
            "allowed_ips": ["0.0.0.0/0"]
        }]
    })
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
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_profile(), &default_settings());
        assert!(result.is_ok(), "VPN config generation failed: {:?}", result.err());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let inbounds = config["inbounds"].as_array().unwrap();
        assert_eq!(inbounds[0]["type"], "tun");
        assert!(inbounds[0].get("listen_port").is_none());
    }

    #[test]
    fn test_vpn_dns_new_format() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_profile(), &default_settings());
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
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_tls_profile(), &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let outbounds = config["outbounds"].as_array().unwrap();
        assert_eq!(outbounds.len(), 3);
        assert_eq!(outbounds[2]["tag"], "proxy");
        assert_eq!(config["route"]["final"], "proxy");
    }

    #[test]
    fn test_proxy_listen_port_set() {
        let result = generate_config(base_template(), &cache_dir(), 9999, "proxy", &vless_profile(), &default_settings());
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

        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &profile, &default_settings());
        assert!(result.is_err());
    }

    #[test]
    fn test_vpn_config_full_structure() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_profile(), &default_settings());
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

        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_profile(), &settings);
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

        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_profile(), &settings);
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let servers = config["dns"]["servers"].as_array().unwrap();
        assert_eq!(servers[0]["server"], "1.0.0.1");
        assert_eq!(servers[0]["server_port"], 443);
    }
}
