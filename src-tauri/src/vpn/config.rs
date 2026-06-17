use crate::vpn::{ServerProfile, VpnError};
use serde_json::{json, Value};
use std::path::Path;

pub fn generate_config(
    base_template: &str,
    server_profile: &ServerProfile,
) -> Result<String, VpnError> {
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

    let json_string =
        serde_json::to_string_pretty(&config).map_err(|e| VpnError::Config(e.to_string()))?;

    Ok(json_string)
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
        outbound["flow"] = json!(flow);
    }
    if let Some(sni) = &server_profile.sni {
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

    /// Шаблон клиентского конфига — HTTP inbound (без прав администратора),
    /// outbounds direct + block, rule_set пустой, final будет заменён на proxy.
    fn base_template() -> &'static str {
        r#"{
            "log": { "level": "debug", "timestamp": true },
            "inbounds": [{
                "type": "http",
                "tag": "proxy-in",
                "listen": "127.0.0.1",
                "listen_port": 10809,
                "sniff": true
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

    #[test]
    fn test_vless_plain() {
        let result = generate_config(base_template(), &vless_profile());
        assert!(
            result.is_ok(),
            "Config generation failed: {:?}",
            result.err()
        );

        let config: Value = serde_json::from_str(&result.unwrap()).unwrap();
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
        let result = generate_config(base_template(), &vless_tls_profile());
        assert!(
            result.is_ok(),
            "Config generation failed: {:?}",
            result.err()
        );

        let config: Value = serde_json::from_str(&result.unwrap()).unwrap();
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
    fn test_unsupported_protocol() {
        let profile = ServerProfile {
            protocol: "unknown".into(),
            ..vless_profile()
        };

        let result = generate_config(base_template(), &profile);
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
}
