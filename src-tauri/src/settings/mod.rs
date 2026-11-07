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
    /// Background server TESTING interval (minutes), separate from
    /// `auto_update_interval` (subscription REFRESH interval).
    /// Testing re-runs batch-test on existing servers (no URL fetch).
    /// 0 = disabled (only refresh tests servers).
    #[serde(default = "default_auto_test_interval")]
    pub auto_test_interval: u32,
    pub anti_dpi: bool,
    pub enable_mux: bool,
    pub minimize_to_tray: bool,
    pub block_cdn_nodes: bool,
    /// IPinfo.io API token for downloading ipinfo_lite.mmdb.
    /// When empty, ipinfo.io database is not downloaded/used.
    #[serde(default)]
    pub ipinfo_token: String,
    /// Disable TLS certificate verification for proxy outbounds.
    /// Required for many proxy servers with self-signed or mismatched SNI certs.
    /// Default: true (backward compatible — insecure by default).
    #[serde(default = "default_tls_insecure")]
    pub tls_insecure: bool,
    /// Latency test URL host+path (e.g. "cp.cloudflare.com/generate_204").
    /// Picked from AVAILABLE_TEST_URLS. User-configurable via settings.
    #[serde(default = "default_test_url")]
    pub test_url: String,
    /// Use HTTPS for the latency test URL. If false, uses HTTP.
    #[serde(default = "default_test_use_tls")]
    pub test_use_tls: bool,
}

fn default_tls_insecure() -> bool {
    true
}
fn default_test_url() -> String {
    "cp.cloudflare.com/generate_204".to_string()
}
fn default_test_use_tls() -> bool {
    true
}
fn default_auto_test_interval() -> u32 {
    3
}

/// Available test URLs for latency measurement.
/// User picks one via the `test_url` setting.
#[allow(dead_code)]
pub const AVAILABLE_TEST_URLS: &[&str] = &[
    "cp.cloudflare.com/generate_204",
    "www.gstatic.com/generate_204",
    "www.msftconnecttest.com/connecttest.txt",
    "detectportal.firefox.com/success.txt",
];

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
            routing_mode: "system-proxy".to_string(),
            proxy_port: 5055,
            bypass_ru: true,
            start_with_windows: true,
            auto_connect_fastest: true,
            kill_switch: true,
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
            auto_test_interval: 3,
            anti_dpi: true,
            enable_mux: false,
            minimize_to_tray: false,
            block_cdn_nodes: false,
            ipinfo_token: String::new(),
            tls_insecure: true,
            test_url: "cp.cloudflare.com/generate_204".to_string(),
            test_use_tls: true,
        }
    }
}

impl AppSettings {
    /// Normalise old/migrated values to current schema.
    /// Called on load and before save so the frontend never has to.
    pub fn normalize(&mut self) {
        // Old `"proxy"` was renamed to `"system-proxy"`.
        if self.routing_mode == "proxy" {
            self.routing_mode = "system-proxy".to_string();
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
        let mut settings = match fs::read_to_string(&self.file_path) {
            Ok(json) => serde_json::from_str(&json).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppSettings::default()),
            Err(e) => Err(e.to_string()),
        }?;
        settings.normalize();
        Ok(settings)
    }

    pub fn save(&self, settings: &AppSettings) -> Result<(), String> {
        let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
        let tmp = self.file_path.with_extension("json.tmp");
        fs::write(&tmp, &json).map_err(|e| format!("Failed to write temp settings: {e}"))?;
        fs::rename(&tmp, &self.file_path).map_err(|e| format!("Failed to rename settings: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "omnitun-settings-test-{}-{}",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn test_new_creates_correct_path() {
        let mgr = SettingsManager::new(PathBuf::from("/tmp/omnitun"));
        assert!(mgr.file_path.ends_with("settings.json"));
    }

    #[test]
    fn test_load_returns_default_when_file_missing() {
        let dir = test_dir("missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mgr = SettingsManager::new(dir.clone());
        let result = mgr.load();
        assert!(result.is_ok(), "expected Ok for missing file, got Err");
        let settings = result.unwrap();
        assert_eq!(settings.routing_mode, "system-proxy");
        assert_eq!(settings.proxy_port, 5055);
        assert!(settings.bypass_ru);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_returns_error_on_invalid_json() {
        let dir = test_dir("invalid-json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), "not valid json {").unwrap();

        let mgr = SettingsManager::new(dir.clone());
        let result = mgr.load();
        assert!(result.is_err(), "expected Err for invalid JSON");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = test_dir("save-load");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let original = AppSettings {
            routing_mode: "local-proxy".into(),
            proxy_port: 9090,
            allow_lan: false,
            allowed_countries: vec!["US".into(), "DE".into()],
            blocked_protocols: vec!["trojan".into()],
            language: "ru".into(),
            anti_dpi: false,
            enable_mux: true,
            minimize_to_tray: true,
            block_cdn_nodes: true,
            ipinfo_token: "test_token_123".into(),
            ..AppSettings::default()
        };

        {
            let mgr = SettingsManager::new(dir.clone());
            mgr.save(&original).unwrap();
        }
        {
            let mgr = SettingsManager::new(dir.clone());
            let loaded = mgr.load().unwrap();
            assert_eq!(loaded.routing_mode, "local-proxy");
            assert_eq!(loaded.proxy_port, 9090);
            assert!(!loaded.allow_lan);
            assert_eq!(loaded.allowed_countries, vec!["US", "DE"]);
            assert_eq!(loaded.blocked_protocols, vec!["trojan"]);
            assert_eq!(loaded.language, "ru");
            assert!(!loaded.anti_dpi);
            assert!(loaded.enable_mux);
            assert!(loaded.minimize_to_tray);
            assert!(loaded.block_cdn_nodes);
            assert_eq!(loaded.ipinfo_token, "test_token_123");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_default_values() {
        let s = AppSettings::default();
        assert!(!s.accept_terms);
        assert_eq!(s.routing_mode, "system-proxy");
        assert_eq!(s.proxy_port, 5055);
        assert!(s.bypass_ru);
        assert!(s.start_with_windows);
        assert!(s.auto_connect_fastest);
        assert!(s.kill_switch);
        assert!(s.allow_lan);
        assert!(!s.custom_dns);
        assert_eq!(s.dns_address, "1.1.1.1");
        assert!(s.language.is_empty() || s.language == "en");
        assert_eq!(s.auto_update_interval, 5);
        assert!(s.anti_dpi);
        assert!(!s.enable_mux);
        assert!(!s.minimize_to_tray);
        assert!(!s.block_cdn_nodes);
    }

    #[test]
    fn test_custom_country_serde() {
        let cc = CustomCountry {
            code: "CH".into(),
            name: "Switzerland".into(),
            flag: "🇨🇭".into(),
        };
        let json = serde_json::to_string(&cc).unwrap();
        assert!(json.contains("CH"));
        let deser: CustomCountry = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.code, "CH");
        assert_eq!(deser.name, "Switzerland");
    }

    #[test]
    fn test_serialization_uses_camel_case() {
        let s = AppSettings::default();
        let json = serde_json::to_string(&s).unwrap();
        // Must use camelCase, not snake_case
        assert!(
            json.contains("acceptTerms"),
            "expected camelCase 'acceptTerms', got: {json}"
        );
        assert!(
            json.contains("routingMode"),
            "expected camelCase 'routingMode'"
        );
        assert!(json.contains("bypassRu"), "expected camelCase 'bypassRu'");
        assert!(
            json.contains("blockCdnNodes"),
            "expected camelCase 'blockCdnNodes'"
        );
        assert!(
            json.contains("ipinfoToken"),
            "expected camelCase 'ipinfoToken'"
        );
    }
}
