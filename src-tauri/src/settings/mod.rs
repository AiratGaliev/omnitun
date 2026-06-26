use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub accept_terms: bool,
    pub routing_mode: String,
    pub proxy_port: u16,
    pub bypass_ru: bool,
    pub start_with_windows: bool,
    pub auto_connect_fastest: bool,
    pub kill_switch: bool,
    pub allow_lan: bool,
    pub custom_dns: bool,
    pub dns_address: String,
    pub allowed_countries: Vec<String>,
    pub blocked_countries: Vec<String>,
    pub custom_countries: Vec<CustomCountry>,
    pub allowed_protocols: Vec<String>,
    pub blocked_protocols: Vec<String>,
    pub language: String,
    pub auto_update_interval: u32,
    pub anti_dpi: bool,
    pub enable_mux: bool,
    pub minimize_to_tray: bool,
    pub block_cdn_nodes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomCountry {
    pub code: String,
    pub name: String,
    pub flag: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            accept_terms: false,
            routing_mode: "vpn".to_string(),
            proxy_port: 1080,
            bypass_ru: true,
            start_with_windows: true,
            auto_connect_fastest: true,
            kill_switch: false,
            allow_lan: true,
            custom_dns: false,
            dns_address: "1.1.1.1".to_string(),
            allowed_countries: Vec::new(),
            blocked_countries: Vec::new(),
            custom_countries: Vec::new(),
            allowed_protocols: Vec::new(),
            blocked_protocols: Vec::new(),
            language: "en".to_string(),
            auto_update_interval: 5,
            anti_dpi: true,
            enable_mux: false,
            minimize_to_tray: false,
            block_cdn_nodes: false,
        }
    }
}

pub struct SettingsManager {
    file_path: PathBuf,
}

impl SettingsManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            file_path: data_dir.join("settings.json"),
        }
    }

    pub fn load(&self) -> Result<AppSettings, String> {
        match fs::read_to_string(&self.file_path) {
            Ok(json) => serde_json::from_str(&json).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppSettings::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn save(&self, settings: &AppSettings) -> Result<(), String> {
        let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
        fs::write(&self.file_path, json).map_err(|e| e.to_string())
    }
}
