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

    // Set proxy listen port from user settings (HTTP proxy only — TUN doesn't use listen_port)
    if routing_mode == "proxy" {
        if let Some(inbound) = config["inbounds"].get_mut(0) {
            inbound["listen_port"] = json!(proxy_port);
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
    if settings.custom_dns && !settings.dns_address.is_empty() {
        if let Some(dns) = config.get_mut("dns") {
            if let Some(servers) = dns.get_mut("servers").and_then(|s| s.as_array_mut()) {
                if let Some(remote) = servers.first_mut() {
                    remote["address"] = json!(settings.dns_address);
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
            _ => {} // tcp or unknown — no transport block needed
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
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_settings() -> AppSettings {
        AppSettings::default()
    }

    /// Шаблон клиентского конфига — mixed inbound (HTTP + SOCKS5),
    /// outbounds direct + block + dns-out, route final "proxy".
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

    /// Шаблон TUN-конфига для VPN-режима — создаёт сетевой интерфейс,
    /// outbounds direct + block, DNS сервера с HTTPS-форматом.
    fn vpn_template() -> &'static str {
        r#"{
            "log": { "level": "info", "timestamp": true },
            "dns": {
                "servers": [
                    { "tag": "remote", "address": "https://1.1.1.1/dns-query", "detour": "proxy" },
                    { "tag": "local", "address": "https://8.8.8.8/dns-query", "detour": "direct" }
                ],
                "rules": [
                    { "outbound": "any", "server": "remote" }
                ]
            },
            "inbounds": [{
                "type": "tun",
                "tag": "tun-in",
                "interface_name": "omnitun",
                "address": "172.19.0.1/24",
                "mtu": 1500,
                "stack": "mixed"
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
            },
            "experimental": {
                "cache_file": { "enabled": true, "path": "cache.db" }
            }
        }"#
    }

    /// VLESS без TLS — порт 8443 тестового стенда
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

    /// VLESS+TLS — порт 8442 тестового стенда
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

    /// VLESS+Reality+WebSocket — порт 443, тип ws, pbk Reality
    fn vless_reality_ws_profile() -> ServerProfile {
        ServerProfile {
            id: "test-3".into(),
            name: "VLESS Reality WS".into(),
            protocol: "vless".into(),
            server: "server.example.com".into(),
            port: 443,
            uuid: Some("b831381d-6324-4d53-ad4f-8cda48b30811".into()),
            sni: Some("example.com".into()),
            flow: Some("xtls-rprx-vision".into()),
            transport_type: Some("ws".into()),
            transport_path: Some("/ws".into()),
            transport_host: Some("ws.example.com".into()),
            reality_pbk: Some("abcdef123456".into()),
            reality_sid: Some("1".into()),
            ..Default::default()
        }
    }

    fn cache_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("omnitun-cfg-test-{}", std::process::id()))
    }

    #[test]
    fn test_vless_plain() {
        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &vless_profile(), &default_settings());
        assert!(
            result.is_ok(),
            "Config generation failed: {:?}",
            result.err()
        );

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let outbounds = config["outbounds"].as_array().unwrap();
        let proxy = &outbounds[2];

        assert_eq!(proxy["tag"], "proxy");
        assert_eq!(proxy["type"], "vless");
        assert_eq!(proxy["server"], "127.0.0.2");
        assert_eq!(proxy["server_port"], 8443);
        assert_eq!(proxy["uuid"], "b831381d-6324-4d53-ad4f-8cda48b30811");

        // Без TLS — поле tls отсутствует
        assert!(proxy.get("tls").is_none(), "TLS disabled: no tls block");

        // Без flow — поле flow отсутствует
        assert!(proxy.get("flow").is_none(), "No flow when flow is None");
    }

    #[test]
    fn test_vless_tls() {
        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &vless_tls_profile(), &default_settings());
        assert!(
            result.is_ok(),
            "Config generation failed: {:?}",
            result.err()
        );

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let outbounds = config["outbounds"].as_array().unwrap();
        let proxy = &outbounds[2];

        assert_eq!(proxy["type"], "vless");
        assert_eq!(proxy["server"], "127.0.0.2");
        assert_eq!(proxy["server_port"], 8442);

        // TLS включён, server_name = localhost
        let tls = &proxy["tls"];
        assert!(tls["enabled"].as_bool().unwrap());
        assert_eq!(tls["server_name"], "localhost");
    }

    #[test]
    fn test_vless_reality_ws() {
        let result = generate_config(
            base_template(), &cache_dir(), 10809, "proxy", &vless_reality_ws_profile(), &default_settings()
        );
        assert!(result.is_ok(), "VLESS Reality+WS config failed: {:?}", result.err());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let proxy = &config["outbounds"][2];

        assert_eq!(proxy["type"], "vless");
        assert_eq!(proxy["server"], "server.example.com");
        assert_eq!(proxy["server_port"], 443);

        // Reality TLS block
        let tls = &proxy["tls"];
        assert_eq!(tls["enabled"], true);
        assert_eq!(tls["server_name"], "example.com");
        assert_eq!(tls["reality"]["enabled"], true);
        assert_eq!(tls["reality"]["public_key"], "abcdef123456");
        assert_eq!(tls["reality"]["short_id"], "1");

        // WebSocket transport
        let transport = &proxy["transport"];
        assert_eq!(transport["type"], "ws");
        assert_eq!(transport["path"], "/ws");
        assert_eq!(transport["headers"]["Host"], "ws.example.com");

        // Flow
        assert_eq!(proxy["flow"], "xtls-rprx-vision");
    }

    // ── VPN mode (TUN) tests ─────────────────────────────────────────────────

    /// VPN-режим: генерирует TUN inbound, а не HTTP.
    #[test]
    fn test_vpn_tun_inbound() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_profile(), &default_settings());
        assert!(result.is_ok(), "VPN config generation failed: {:?}", result.err());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let inbounds = config["inbounds"].as_array().unwrap();

        assert_eq!(inbounds.len(), 1, "VPN mode: exactly one inbound");
        assert_eq!(inbounds[0]["type"], "tun", "VPN mode: inbound type is tun");
        assert_eq!(inbounds[0]["tag"], "tun-in");
        assert_eq!(inbounds[0]["interface_name"], "omnitun");
        assert_eq!(inbounds[0]["address"], "172.19.0.1/24");

        // TUN inbound does NOT get a listen_port — that's only for HTTP proxy
        assert!(
            inbounds[0].get("listen_port").is_none(),
            "VPN mode: TUN inbound must NOT have listen_port"
        );
    }

    /// VPN-режим: outbound server добавляется с tag "proxy", final маршрут совпадает.
    #[test]
    fn test_vpn_outbound_added() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_tls_profile(), &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let outbounds = config["outbounds"].as_array().unwrap();

        assert_eq!(outbounds.len(), 3, "VPN mode: direct + block + proxy");
        assert_eq!(outbounds[0]["tag"], "direct");
        assert_eq!(outbounds[1]["tag"], "block");
        assert_eq!(outbounds[2]["tag"], "proxy");
        assert_eq!(outbounds[2]["type"], "vless");

        // Route final должен указывать на tag "proxy"
        assert_eq!(config["route"]["final"], "proxy");
    }

    /// VPN-режим: clash_api добавляется для сбора статистики трафика.
    #[test]
    fn test_vpn_clash_api_enabled() {
        let result = generate_config(vpn_template(), &cache_dir(), 1080, "vpn", &vless_profile(), &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let clash_api = &config["experimental"]["clash_api"];

        assert!(clash_api.get("external_controller").is_some());
        let addr = clash_api["external_controller"].as_str().unwrap();
        assert!(addr.starts_with("127.0.0.1:"), "clash_api binds to 127.0.0.1:<port>");
    }

    /// VPN-режим: cache_file path превращается в абсолютный.
    #[test]
    fn test_vpn_absolute_cache_path() {
        let cache = cache_dir();
        let result = generate_config(vpn_template(), &cache, 1080, "vpn", &vless_profile(), &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let cache_path = config["experimental"]["cache_file"]["path"].as_str().unwrap();

        // Должен быть абсолютным
        let path = std::path::Path::new(cache_path);
        assert!(path.is_absolute(), "Cache path must be absolute: {cache_path}");
        assert!(cache_path.contains("cache.db"), "Cache path must end with cache.db");
    }

    // ── Proxy mode regression tests ──────────────────────────────────────────

    /// Regression: proxy-режим ДОЛЖЕН устанавливать listen_port на inbound.
    #[test]
    fn test_proxy_listen_port_set() {
        let result = generate_config(base_template(), &cache_dir(), 9999, "proxy", &vless_profile(), &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        let inbounds = config["inbounds"].as_array().unwrap();

        assert_eq!(inbounds[0]["type"], "mixed");
        assert_eq!(inbounds[0]["listen_port"], 9999, "Proxy mode: listen_port must be set");
    }

    /// Regression: proxy-режим с портом по умолчанию (1080).
    #[test]
    fn test_proxy_default_port() {
        let result = generate_config(base_template(), &cache_dir(), 1080, "proxy", &vless_profile(), &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        assert_eq!(config["inbounds"][0]["listen_port"], 1080);
    }

    /// Regression: proxy-режим — clash_api тоже добавляется.
    #[test]
    fn test_proxy_clash_api_enabled() {
        let result = generate_config(base_template(), &cache_dir(), 1080, "proxy", &vless_profile(), &default_settings());
        assert!(result.is_ok());

        let config: Value = serde_json::from_str(&result.unwrap().0).unwrap();
        assert!(
            config["experimental"]["clash_api"].is_object(),
            "Proxy mode must have clash_api"
        );
    }

    #[test]
    fn test_unsupported_protocol() {
        let profile = ServerProfile {
            protocol: "unknown".into(),
            ..vless_profile()
        };

        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &profile, &default_settings());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported protocol"));
    }

    #[test]
    fn test_write_config() {
        let dir = std::env::temp_dir().join(format!("omnitun-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let path = write_config(r#"{"test": true}"#, &dir).unwrap();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, r#"{"test": true}"#);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn proto_profile(protocol: &str) -> ServerProfile {
        ServerProfile {
            protocol: protocol.into(),
            server: "server.example.com".into(),
            port: 443,
            uuid: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            password: Some("pass123".into()),
            method: Some("chacha20-ietf-poly1305".into()),
            sni: Some("example.com".into()),
            ..Default::default()
        }
    }

    #[test]
    fn test_vmess() {
        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &proto_profile("vmess"), &default_settings());
        assert!(result.is_ok());
        let proxy = &serde_json::from_str::<Value>(&result.unwrap().0).unwrap()["outbounds"][2];
        assert_eq!(proxy["type"], "vmess");
        assert_eq!(proxy["uuid"], "550e8400-e29b-41d4-a716-446655440000");
        assert!(proxy["tls"]["enabled"].as_bool().unwrap());
    }

    #[test]
    fn test_trojan() {
        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &proto_profile("trojan"), &default_settings());
        assert!(result.is_ok());
        let proxy = &serde_json::from_str::<Value>(&result.unwrap().0).unwrap()["outbounds"][2];
        assert_eq!(proxy["type"], "trojan");
        assert_eq!(proxy["password"], "pass123");
        assert!(proxy["tls"]["enabled"].as_bool().unwrap());
    }

    #[test]
    fn test_shadowsocks() {
        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &proto_profile("shadowsocks"), &default_settings());
        assert!(result.is_ok());
        let proxy = &serde_json::from_str::<Value>(&result.unwrap().0).unwrap()["outbounds"][2];
        assert_eq!(proxy["type"], "shadowsocks");
        assert_eq!(proxy["method"], "chacha20-ietf-poly1305");
        assert_eq!(proxy["password"], "pass123");
    }

    #[test]
    fn test_wireguard() {
        let profile = ServerProfile {
            protocol: "wireguard".into(),
            server: "10.0.0.1".into(),
            port: 51820,
            private_key: Some("some-private-key".into()),
            public_key: Some("some-public-key".into()),
            ..Default::default()
        };
        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &profile, &default_settings());
        assert!(result.is_ok());
        let proxy = &serde_json::from_str::<Value>(&result.unwrap().0).unwrap()["outbounds"][2];
        assert_eq!(proxy["type"], "wireguard");
        assert_eq!(proxy["peers"][0]["public_key"], "some-public-key");
    }

    #[test]
    fn test_hysteria2() {
        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &proto_profile("hysteria2"), &default_settings());
        assert!(result.is_ok());
        let proxy = &serde_json::from_str::<Value>(&result.unwrap().0).unwrap()["outbounds"][2];
        assert_eq!(proxy["type"], "hysteria2");
        assert_eq!(proxy["password"], "pass123");
        assert!(proxy["tls"]["enabled"].as_bool().unwrap());
    }

    #[test]
    fn test_tuic() {
        let result = generate_config(base_template(), &cache_dir(), 10809, "proxy", &proto_profile("tuic"), &default_settings());
        assert!(result.is_ok());
        let proxy = &serde_json::from_str::<Value>(&result.unwrap().0).unwrap()["outbounds"][2];
        assert_eq!(proxy["type"], "tuic");
        assert_eq!(proxy["uuid"], "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(proxy["password"], "pass123");
        assert!(proxy["tls"]["enabled"].as_bool().unwrap());
    }
}
