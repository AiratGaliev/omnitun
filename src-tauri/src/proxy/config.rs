use crate::proxy::{ProxyError, ServerProfile};
use crate::settings::AppSettings;
use serde_json::Value;
use std::path::Path;

pub use omnitun_core::proxy::config::{
    apply_anti_dpi, apply_mux, build_outbound, generate_kill_switch_config, redact_sensitive_data,
    BatchConfigOptions,
};

pub(crate) fn find_available_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

pub(crate) fn port_available(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

pub fn generate_config(
    cache_dir: &Path,
    proxy_port: u16,
    routing_mode: &str,
    profiles: &[ServerProfile],
    settings: &AppSettings,
) -> Result<(String, u16), ProxyError> {
    let stats_port = find_available_port().unwrap_or(20000);
    let result = omnitun_core::proxy::config::generate_config(
        cache_dir,
        proxy_port,
        routing_mode,
        profiles,
        settings,
        stats_port,
    )?;
    crate::proxy::CLASH_API_PORT.store(stats_port, std::sync::atomic::Ordering::Relaxed);
    Ok(result)
}

pub fn generate_batch_config(
    servers: &[(String, ServerProfile)],
    ports: &[u16],
    test_dir: &Path,
) -> Result<Value, ProxyError> {
    let options = BatchConfigOptions {
        anti_dpi: crate::proxy::batcher::batch_anti_dpi(),
        tls_insecure: crate::proxy::batcher::batch_tls_insecure(),
        enable_mux: crate::proxy::batcher::batch_enable_mux(),
    };
    omnitun_core::proxy::config::generate_batch_config(servers, ports, test_dir, options)
}

pub fn write_config(json: &str, config_dir: &Path) -> Result<std::path::PathBuf, ProxyError> {
    std::fs::create_dir_all(config_dir)?;
    let path = config_dir.join("config.json");
    std::fs::write(&path, json)?;
    Ok(path)
}
