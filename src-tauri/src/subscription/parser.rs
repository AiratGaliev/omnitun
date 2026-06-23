// src-tauri/src/subscription/parser.rs

use crate::vpn::{ServerProfile, VpnError};
use std::collections::HashMap;

pub fn parse_subscription(data: &str) -> Result<Vec<ServerProfile>, VpnError> {
    let decoded = try_base64_decode(data).unwrap_or_else(|_| data.to_string());

    let mut servers = Vec::new();
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(profile) = parse_uri(line) {
            servers.push(profile);
        }
    }

    Ok(servers)
}

fn try_base64_decode(data: &str) -> Result<String, anyhow::Error> {
    use base64::Engine as _;
    let trimmed = data.trim();
    let clean: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&clean)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&clean))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&clean))?;

    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn parse_uri(uri: &str) -> Option<ServerProfile> {
    if let Some(remainder) = uri.strip_prefix("vless://") {
        parse_vless(remainder)
    } else if let Some(remainder) = uri.strip_prefix("vmess://") {
        parse_vmess(remainder)
    } else if let Some(remainder) = uri.strip_prefix("ss://") {
        parse_ss(remainder)
    } else if let Some(remainder) = uri.strip_prefix("trojan://") {
        parse_trojan(remainder)
    } else if let Some(remainder) = uri.strip_prefix("hysteria2://") {
        parse_hysteria2(remainder)
    } else if let Some(remainder) = uri.strip_prefix("hy2://") {
        parse_hysteria2(remainder)
    } else if let Some(remainder) = uri.strip_prefix("tuic://") {
        parse_tuic(remainder)
    } else if let Some(remainder) = uri.strip_prefix("wireguard://") {
        parse_wireguard(remainder)
    } else if uri.starts_with("socks5://") || uri.starts_with("socks://") {
        parse_socks(uri)
    } else if uri.starts_with("http://") || uri.starts_with("https://") {
        // Only treat as proxy URI if it matches the http(s):// proxy pattern (has @ for auth)
        if uri.contains('@') {
            parse_http(uri)
        } else {
            None
        }
    } else {
        None
    }
}

fn split_uri(data: &str) -> (&str, &str, &str) {
    let (rest, fragment) = data.split_once('#').unwrap_or((data, ""));
    let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
    (main, query, fragment)
}

fn parse_vless(data: &str) -> Option<ServerProfile> {
    let (main, query, fragment) = split_uri(data);
    let name = urlencoding_decode(fragment);

    let (uuid, server_port) = main.split_once('@')?;
    let (server, port_str) = server_port.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let params = parse_query(query);

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: if name.is_empty() {
            params.get("remark").cloned().unwrap_or_default()
        } else {
            name
        },
        protocol: "vless".into(),
        server: server.to_string(),
        port,
        uuid: Some(uuid.to_string()),
        password: None,
        method: None,
        sni: params.get("sni").cloned(),
        flow: params.get("flow").cloned(),
        public_key: None,
        private_key: None,
        transport_type: params.get("type").cloned(),
        transport_path: params.get("path").cloned(),
        transport_host: params.get("host").cloned(),
        reality_pbk: params.get("pbk").cloned(),
        reality_sid: params.get("sid").cloned(),
        ping_ms: None,
        flag: None,
        wg_local_address: None,
        wg_allowed_ips: None,
        wg_dns: None,
        wg_mtu: None,
        wg_keepalive: None,
        username: None,
    })
}

fn parse_vmess(data: &str) -> Option<ServerProfile> {
    use base64::Engine as _;

    let json_str = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data.trim()))
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data.trim()))
            .ok()?,
    )
    .ok()?;

    let v: serde_json::Value = serde_json::from_str(&json_str).ok()?;

    let server = v.get("add")?.as_str()?.to_string();
    let port: u16 = v.get("port")?.as_u64().or_else(|| v.get("port")?.as_str()?.parse().ok())? as u16;

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: v
            .get("ps")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        protocol: "vmess".into(),
        server,
        port,
        uuid: v.get("id").and_then(|x| x.as_str()).map(String::from),
        password: None,
        method: v
            .get("scy")
            .and_then(|x| x.as_str())
            .map(String::from),
        sni: v
            .get("tls")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        flow: None,
        public_key: None,
        private_key: None,
        transport_type: None,
        transport_path: None,
        transport_host: None,
        reality_pbk: None,
        reality_sid: None,
        ping_ms: None,
        flag: None,
        wg_local_address: None,
        wg_allowed_ips: None,
        wg_dns: None,
        wg_mtu: None,
        wg_keepalive: None,
        username: None,
    })
}

fn parse_ss(data: &str) -> Option<ServerProfile> {
    use base64::Engine as _;

    let (main, _query, fragment) = split_uri(data);
    let name = urlencoding_decode(fragment);

    if let Some((userinfo_b64, server_port)) = main.split_once('@') {
        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(userinfo_b64.trim())
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(userinfo_b64.trim()))
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(userinfo_b64.trim()))
            .ok();

        if let Some(bytes) = decoded_bytes {
            if let Ok(decoded) = String::from_utf8(bytes) {
                if let Some((method, password)) = decoded.split_once(':') {
                    if let Some((server, port_str)) = server_port.rsplit_once(':') {
                        if let Ok(port) = port_str.parse::<u16>() {
                            return Some(ServerProfile {
                                id: uuid::Uuid::new_v4().to_string(),
                                name,
                                protocol: "shadowsocks".into(),
                                server: server.to_string(),
                                port,
                                uuid: None,
                                password: Some(password.to_string()),
                                method: Some(method.to_string()),
                                sni: None,
                                flow: None,
                                public_key: None,
                                private_key: None,
                                transport_type: None,
                                transport_path: None,
                                transport_host: None,
                                reality_pbk: None,
                                reality_sid: None,
                                ping_ms: None,
                                flag: None,
                                wg_local_address: None,
                                wg_allowed_ips: None,
                                wg_dns: None,
                                wg_mtu: None,
                                wg_keepalive: None,
                                username: None,
                            });
                        }
                    }
                }
            }
        }
    }

    let decoded_bytes = base64::engine::general_purpose::STANDARD
        .decode(main.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(main.trim()))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(main.trim()))
        .ok()?;

    let decoded = String::from_utf8(decoded_bytes).ok()?;
    let (userinfo, server_port) = decoded.split_once('@')?;
    let (method, password) = userinfo.split_once(':')?;
    let (server, port_str) = server_port.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        protocol: "shadowsocks".into(),
        server: server.to_string(),
        port,
        uuid: None,
        password: Some(password.to_string()),
        method: Some(method.to_string()),
        sni: None,
        flow: None,
        public_key: None,
        private_key: None,
        transport_type: None,
        transport_path: None,
        transport_host: None,
        reality_pbk: None,
        reality_sid: None,
        ping_ms: None,
        flag: None,
        wg_local_address: None,
        wg_allowed_ips: None,
        wg_dns: None,
        wg_mtu: None,
        wg_keepalive: None,
        username: None,
    })
}

fn parse_trojan(data: &str) -> Option<ServerProfile> {
    let (main, query, fragment) = split_uri(data);
    let name = urlencoding_decode(fragment);

    let (password, server_port) = main.split_once('@')?;
    let (server, port_str) = server_port.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let params = parse_query(query);

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: if name.is_empty() {
            params.get("remark").cloned().unwrap_or_default()
        } else {
            name
        },
        protocol: "trojan".into(),
        server: server.to_string(),
        port,
        uuid: None,
        password: Some(urlencoding_decode(password)),
        method: None,
        sni: params.get("sni").cloned(),
        flow: params.get("flow").cloned(),
        public_key: None,
        private_key: None,
        transport_type: params.get("type").cloned(),
        transport_path: params.get("path").cloned(),
        transport_host: params.get("host").cloned(),
        reality_pbk: params.get("pbk").cloned(),
        reality_sid: params.get("sid").cloned(),
        ping_ms: None,
        flag: None,
        wg_local_address: None,
        wg_allowed_ips: None,
        wg_dns: None,
        wg_mtu: None,
        wg_keepalive: None,
        username: None,
    })
}

fn parse_hysteria2(data: &str) -> Option<ServerProfile> {
    let (main, query, fragment) = split_uri(data);
    let name = urlencoding_decode(fragment);

    let (password, server_port) = main.split_once('@')?;
    let (server, port_str) = server_port.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let params = parse_query(query);

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: if name.is_empty() {
            params.get("remark").cloned().unwrap_or_default()
        } else {
            name
        },
        protocol: "hysteria2".into(),
        server: server.to_string(),
        port,
        uuid: None,
        password: Some(urlencoding_decode(password)),
        method: None,
        sni: params.get("sni").cloned(),
        flow: None,
        public_key: None,
        private_key: None,
        transport_type: None,
        transport_path: None,
        transport_host: None,
        reality_pbk: None,
        reality_sid: None,
        ping_ms: None,
        flag: None,
        wg_local_address: None,
        wg_allowed_ips: None,
        wg_dns: None,
        wg_mtu: None,
        wg_keepalive: None,
        username: None,
    })
}

fn parse_tuic(data: &str) -> Option<ServerProfile> {
    let (main, query, fragment) = split_uri(data);
    let name = urlencoding_decode(fragment);

    let (userinfo, server_port) = main.split_once('@')?;
    let (uuid, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));

    let (server, port_str) = server_port.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let params = parse_query(query);

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: if name.is_empty() {
            params.get("remark").cloned().unwrap_or_default()
        } else {
            name
        },
        protocol: "tuic".into(),
        server: server.to_string(),
        port,
        uuid: Some(uuid.to_string()),
        password: if password.is_empty() { None } else { Some(urlencoding_decode(password)) },
        method: None,
        sni: params.get("sni").cloned(),
        flow: None,
        public_key: None,
        private_key: None,
        transport_type: None,
        transport_path: None,
        transport_host: None,
        reality_pbk: None,
        reality_sid: None,
        ping_ms: None,
        flag: None,
        wg_local_address: None,
        wg_allowed_ips: None,
        wg_dns: None,
        wg_mtu: None,
        wg_keepalive: None,
        username: None,
    })
}

fn parse_wireguard(data: &str) -> Option<ServerProfile> {
    // wireguard://<key>@<server>:<port>?public_key=<pk>&allowed_ips=0.0.0.0/0&dns=1.1.1.1#Remark
    let (main, query, fragment) = split_uri(data);
    let name = urlencoding_decode(fragment);
    let params = parse_query(query);

    let (key_b64, server_port) = main.split_once('@')?;
    let (server, port_str) = server_port.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;

    let private_key = Some(urlencoding_decode(key_b64));

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: if name.is_empty() {
            params.get("remark").cloned().unwrap_or_default()
        } else {
            name
        },
        protocol: "wireguard".into(),
        server: server.to_string(),
        port,
        uuid: None,
        password: None,
        method: None,
        sni: None,
        flow: None,
        public_key: params.get("public_key").cloned(),
        private_key,
        transport_type: None,
        transport_path: None,
        transport_host: None,
        reality_pbk: None,
        reality_sid: None,
        ping_ms: None,
        flag: None,
        wg_local_address: params.get("local_address").or_else(|| params.get("address")).cloned(),
        wg_allowed_ips: params.get("allowed_ips").cloned(),
        wg_dns: params.get("dns").cloned(),
        wg_mtu: params.get("mtu").and_then(|m| m.parse().ok()),
        wg_keepalive: params.get("keepalive").or_else(|| params.get("persistent_keepalive")).and_then(|k| k.parse().ok()),
        username: None,
    })
}

fn parse_socks(uri: &str) -> Option<ServerProfile> {
    // socks5://user:pass@server:port#Remark
    // socks5://server:port#Remark
    let stripped = uri
        .strip_prefix("socks5://")
        .or_else(|| uri.strip_prefix("socks://"))?;

    let (main, _query, fragment) = split_uri(stripped);
    let name = urlencoding_decode(fragment);

    let (server, port, username, password) = if let Some((userinfo, rest)) = main.split_once('@') {
        let (srv, prt) = rest.rsplit_once(':')?;
        let port: u16 = prt.parse().ok()?;
        let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        (srv.to_string(), port, if user.is_empty() { None } else { Some(user.to_string()) }, if pass.is_empty() { None } else { Some(urlencoding_decode(pass)) })
    } else {
        let (srv_host, srv_port) = main.rsplit_once(':').unwrap_or((main, "1080"));
        (srv_host.to_string(), srv_port.parse().ok()?, None, None)
    };

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: if name.is_empty() { server.clone() } else { name },
        protocol: "socks".into(),
        server,
        port,
        uuid: None,
        password,
        method: if username.is_some() { Some("username_password".into()) } else { None },
        username,
        ..Default::default()
    })
}

fn parse_http(uri: &str) -> Option<ServerProfile> {
    // http://user:pass@server:port#Remark
    // https://user:pass@server:port#Remark
    let stripped = uri.strip_prefix("http://").or_else(|| uri.strip_prefix("https://"))?;
    let is_https = uri.starts_with("https://");

    let (main, _query, fragment) = split_uri(stripped);
    let name = urlencoding_decode(fragment);

    let (userinfo, rest) = main.split_once('@')?;
    let (server, port_str) = rest.rsplit_once(':').unwrap_or((rest, if is_https { "443" } else { "80" }));
    let port: u16 = port_str.parse().ok()?;
    let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: if name.is_empty() { user.to_string() } else { name },
        protocol: "http".into(),
        server: server.to_string(),
        port,
        uuid: None,
        password: if pass.is_empty() { None } else { Some(urlencoding_decode(pass)) },
        method: if user.is_empty() { None } else { Some("username_password".into()) },
        sni: None,
        flow: None,
        public_key: None,
        private_key: None,
        transport_type: None,
        transport_path: None,
        transport_host: None,
        reality_pbk: None,
        reality_sid: None,
        ping_ms: None,
        flag: None,
        wg_local_address: None,
        wg_allowed_ips: None,
        wg_dns: None,
        wg_mtu: None,
        wg_keepalive: None,
        username: None,
    })
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    if query.is_empty() {
        return params;
    }
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            params.insert(
                urlencoding_decode(key).to_string(),
                urlencoding_decode(value).to_string(),
            );
        }
    }
    params
}

fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();

    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    result.push(byte as char);
                } else {
                    result.push('%');
                    result.push_str(&hex);
                }
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn test_parse_vless_uri() {
        let uri = "vless://b831381d-6324-4d53-ad4f-8cda48b30811@127.0.0.2:8443?flow=xtls-rprx-vision&sni=127.0.0.2&remark=Test%20VLESS";
        let result = parse_uri(uri);
        assert!(result.is_some());
        let p = result.unwrap();
        assert_eq!(p.protocol, "vless");
        assert_eq!(p.server, "127.0.0.2");
        assert_eq!(p.port, 8443);
        assert_eq!(p.uuid.unwrap(), "b831381d-6324-4d53-ad4f-8cda48b30811");
        assert_eq!(p.flow.unwrap(), "xtls-rprx-vision");
        assert_eq!(p.sni.unwrap(), "127.0.0.2");
        assert_eq!(p.name, "Test VLESS");
    }

    #[test]
    fn test_parse_vless_with_transport_reality() {
        let uri = "vless://b831381d-6324-4d53-ad4f-8cda48b30811@server.example.com:443?type=ws&path=%2Fws&host=ws.example.com&security=reality&pbk=abcdef123456&sid=1&sni=example.com&flow=xtls-rprx-vision#VLESS+Reality+WS";
        let result = parse_uri(uri);
        assert!(result.is_some(), "VLESS with transport/reality should parse");
        let p = result.unwrap();
        assert_eq!(p.protocol, "vless");
        assert_eq!(p.server, "server.example.com");
        assert_eq!(p.port, 443);
        assert_eq!(p.transport_type.unwrap(), "ws");
        assert_eq!(p.transport_path.unwrap(), "/ws");
        assert_eq!(p.transport_host.unwrap(), "ws.example.com");
        assert_eq!(p.reality_pbk.unwrap(), "abcdef123456");
        assert_eq!(p.reality_sid.unwrap(), "1");
        assert_eq!(p.sni.unwrap(), "example.com");
        assert_eq!(p.flow.unwrap(), "xtls-rprx-vision");
        assert_eq!(p.name, "VLESS Reality WS");
    }

    #[test]
    fn test_base64_decode() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("hello world");
        let decoded = try_base64_decode(&encoded).unwrap();
        assert_eq!(decoded, "hello world");
    }

    #[test]
    fn test_urlencoding_decode() {
        assert_eq!(urlencoding_decode("Test%20Server"), "Test Server");
        assert_eq!(urlencoding_decode("hello+world"), "hello world");
        assert_eq!(urlencoding_decode("normal"), "normal");
    }

    #[test]
    fn test_parse_vmess_uri() {
        let json = r#"{"v":"2","ps":"VMess 127.0.0.3","add":"127.0.0.3","port":8444,"id":"b831381d-6324-4d53-ad4f-8cda48b30811","aid":0,"scy":"auto","net":"tcp","type":"none","tls":""}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        let uri = format!("vmess://{}", encoded);
        let result = parse_uri(&uri);
        assert!(result.is_some(), "VMess URI should parse");
        let p = result.unwrap();
        assert_eq!(p.protocol, "vmess");
        assert_eq!(p.server, "127.0.0.3");
        assert_eq!(p.port, 8444);
        assert_eq!(p.uuid.unwrap(), "b831381d-6324-4d53-ad4f-8cda48b30811");
        assert_eq!(p.name, "VMess 127.0.0.3");
        assert_eq!(p.method.unwrap(), "auto");
        assert!(p.sni.is_none(), "Empty tls field means no SNI");
    }

    #[test]
    fn test_parse_vmess_with_tls() {
        let json = r#"{"v":"2","ps":"VMess TLS","add":"127.0.0.3","port":8444,"id":"b831381d-6324-4d53-ad4f-8cda48b30811","aid":0,"scy":"auto","net":"tcp","type":"none","tls":"example.com"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        let uri = format!("vmess://{}", encoded);
        let p = parse_uri(&uri).unwrap();
        assert_eq!(p.sni.unwrap(), "example.com");
    }

    #[test]
    fn test_parse_shadowsocks_uri() {
        let userinfo = base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:omnitun-test-password");
        let uri = format!("ss://{}@127.0.0.4:8446#Shadowsocks+127.0.0.4", userinfo);
        let result = parse_uri(&uri);
        assert!(result.is_some(), "Shadowsocks URI should parse");
        let p = result.unwrap();
        assert_eq!(p.protocol, "shadowsocks");
        assert_eq!(p.server, "127.0.0.4");
        assert_eq!(p.port, 8446);
        assert_eq!(p.method.unwrap(), "aes-256-gcm");
        assert_eq!(p.password.unwrap(), "omnitun-test-password");
        assert_eq!(p.name, "Shadowsocks 127.0.0.4");
    }

    #[test]
    fn test_parse_shadowsocks_without_remark() {
        let userinfo = base64::engine::general_purpose::STANDARD.encode("chacha20-ietf-poly1305:password");
        let uri = format!("ss://{}@example.com:18443", userinfo);
        let p = parse_uri(&uri).unwrap();
        assert_eq!(p.server, "example.com");
        assert_eq!(p.port, 18443);
        assert_eq!(p.name, "");
    }

    #[test]
    fn test_parse_trojan_uri() {
        let uri = "trojan://omnitun-test-password@127.0.0.3:8445?security=none&type=tcp#Trojan+127.0.0.3";
        let p = parse_uri(uri).unwrap();
        assert_eq!(p.protocol, "trojan");
        assert_eq!(p.server, "127.0.0.3");
        assert_eq!(p.port, 8445);
        assert_eq!(p.password.unwrap(), "omnitun-test-password");
        assert_eq!(p.name, "Trojan 127.0.0.3");
    }

    #[test]
    fn test_parse_hysteria2_uri() {
        let uri = "hysteria2://omnitun-test-password@127.0.0.4:8447?insecure=1&sni=localhost#Hysteria2+127.0.0.4";
        let p = parse_uri(uri).unwrap();
        assert_eq!(p.protocol, "hysteria2");
        assert_eq!(p.server, "127.0.0.4");
        assert_eq!(p.port, 8447);
        assert_eq!(p.password.unwrap(), "omnitun-test-password");
        assert_eq!(p.sni.unwrap(), "localhost");
        assert_eq!(p.name, "Hysteria2 127.0.0.4");
    }

    #[test]
    fn test_parse_tuic_uri() {
        let uri = "tuic://b831381d-6324-4d53-ad4f-8cda48b30811:omnitun-test-password@127.0.0.5:8448?congestion_control=bbr&alpn=h3&sni=localhost&insecure=1#TUIC+127.0.0.5";
        let p = parse_uri(uri).unwrap();
        assert_eq!(p.protocol, "tuic");
        assert_eq!(p.server, "127.0.0.5");
        assert_eq!(p.port, 8448);
        assert_eq!(p.uuid.unwrap(), "b831381d-6324-4d53-ad4f-8cda48b30811");
        assert_eq!(p.password.unwrap(), "omnitun-test-password");
        assert_eq!(p.sni.unwrap(), "localhost");
        assert_eq!(p.name, "TUIC 127.0.0.5");
    }

    #[test]
    fn test_parse_subscription_multiple_protocols() {
        let vless = "vless://b831381d-6324-4d53-ad4f-8cda48b30811@127.0.0.2:8443?remark=VLESS+Test";
        let vmess_json = r#"{"v":"2","ps":"VMess Test","add":"127.0.0.3","port":8444,"id":"b831381d-6324-4d53-ad4f-8cda48b30811","aid":0,"scy":"auto","net":"tcp","type":"none","tls":""}"#;
        let vmess = format!("vmess://{}", base64::engine::general_purpose::STANDARD.encode(vmess_json));
        let ss_userinfo = base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:test");
        let ss = format!("ss://{}@127.0.0.4:8446#SS+Test", ss_userinfo);

        let data = format!("{}\n{}\n{}", vless, vmess, ss);
        let servers = parse_subscription(&data).unwrap();
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].protocol, "vless");
        assert_eq!(servers[1].protocol, "vmess");
        assert_eq!(servers[2].protocol, "shadowsocks");
    }
}
