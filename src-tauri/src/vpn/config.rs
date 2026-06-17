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
        return Err(VpnError::Config("Missing outbounds array".into()));
    }

    serde_json::to_string_pretty(&config).map_err(|e| VpnError::Config(e.to_string()))
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
        if !flow.is_empty() {
            outbound["flow"] = json!(flow);
        }
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_vless_config() {
        let template = include_str!("../../config/base.json");
        let profile = ServerProfile {
            id: "test-1".into(),
            name: "Test VLESS".into(),
            protocol: "vless".into(),
            server: "127.0.0.2".into(),
            port: 8443,
            uuid: Some("b831381d-6324-4d53-ad4f-8cda48b30811".into()),
            flow: Some("".into()),
            sni: None,
            password: None,
            method: None,
            public_key: None,
            private_key: None,
        };

        let result = generate_config(template, &profile).unwrap();
        println!("=== GENERATED CONFIG ===");
        println!("{}", result);
    }
}
