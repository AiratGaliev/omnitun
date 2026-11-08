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
