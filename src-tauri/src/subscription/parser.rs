use crate::vpn::{ServerProfile, VpnError};

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

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
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
