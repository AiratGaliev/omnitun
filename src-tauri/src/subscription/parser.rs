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
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&clean))?;

    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn parse_uri(uri: &str) -> Option<ServerProfile> {
    if let Some(remainder) = uri.strip_prefix("vless://") {
        parse_vless(remainder)
    } else if let Some(remainder) = uri.strip_prefix("vmess://") {
        parse_vmess(remainder)
    } else if let Some(remainder) = uri.strip_prefix("ss://") {
        parse_ss(remainder)
    } else {
        None
    }
}

fn parse_vless(data: &str) -> Option<ServerProfile> {
    let (uuid, rest) = data.split_once('@')?;
    let (server_port, params_part) = rest.split_once('?').unwrap_or((rest, ""));
    let (server, port_str) = server_port.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    let params = parse_query(params_part);

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name: params
            .get("remark")
            .or(params.get("#"))
            .cloned()
            .unwrap_or_default(),
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
    })
}

fn parse_vmess(data: &str) -> Option<ServerProfile> {
    use base64::Engine as _;

    let json_str = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .ok()?,
    )
    .ok()?;

    let v: serde_json::Value = serde_json::from_str(&json_str).ok()?;

    let server = v.get("add")?.as_str()?.to_string();
    let port: u16 = v.get("port")?.as_u64()? as u16;

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
    })
}

fn parse_ss(data: &str) -> Option<ServerProfile> {
    use base64::Engine as _;

    let (userinfo_b64, host_part) = data.split_once('@')?;

    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(userinfo_b64.trim())
            .ok()?,
    )
    .ok()?;

    let (method, password) = decoded.split_once(':')?;

    let (host, rest) = host_part.rsplit_once(':')?;
    let (port_str, name) = if let Some((p, n)) = rest.split_once('#') {
        (p, Some(n.to_string()))
    } else {
        (rest, None)
    };
    let port: u16 = port_str.parse().ok()?;

    let server = host.to_string();
    let name = name
        .map(|n| urlencoding_decode(&n))
        .unwrap_or_default();

    Some(ServerProfile {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        protocol: "ss".into(),
        server,
        port,
        uuid: None,
        password: Some(password.to_string()),
        method: Some(method.to_string()),
        sni: None,
        flow: None,
        public_key: None,
        private_key: None,
    })
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
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
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
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
        assert_eq!(p.protocol, "ss");
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
        assert_eq!(servers[2].protocol, "ss");
    }
}
