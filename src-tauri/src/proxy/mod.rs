pub mod batcher;
pub mod cache;
pub mod config;
pub mod geoip;
pub mod installer;
pub mod port_pool;
pub mod process;

use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::settings::AppSettings;
use installer::Installer;
pub use omnitun_core::{ErrorCode, ErrorDetail, ProxyError, ServerProfile};
pub use process::ProcessManager;
pub use process::{cleanup_stale_pid, remove_pid_file, set_pid_file_path, write_pid_file};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

/// Current Clash API port (0 = not running). Set by config.rs on each start.
pub(crate) static CLASH_API_PORT: AtomicU16 = AtomicU16::new(0);
/// Actual proxy SOCKS5 port. Written by start_proxy, cleared by stop_proxy/cancel_connect.
/// Used by MCP omnitun://status to report the correct proxy port.
pub(crate) static PROXY_PORT: AtomicU16 = AtomicU16::new(0);
pub(crate) static PROXY_PID: AtomicU32 = AtomicU32::new(0);

/// Last connected profile — stored for auto-reconnect on watchdog exit.
pub(crate) static LAST_PROFILE: std::sync::Mutex<Option<LastConnection>> =
    std::sync::Mutex::new(None);

/// Cancellation flag for the in-flight `start_proxy` call.
/// Set by `cancel_connect` / `stop_proxy`, checked in `wait_for_proxy_ready`.
pub(crate) static CONNECTION_CANCELLING: AtomicBool = AtomicBool::new(false);

/// ID of the currently active (connected) server, for exit IP enrichment.
pub(crate) static ACTIVE_SERVER_ID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Data needed to re-establish a connection after unexpected exit.
#[derive(Clone)]
pub struct LastConnection {
    pub all_servers: Vec<ServerProfile>,
    pub routing_mode: String,
    pub proxy_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

pub struct ProxyManager {
    process_mgr: ProcessManager,
    bin_dir: PathBuf,
    config_dir: PathBuf,
    installed: Arc<AtomicBool>,
}

impl ProxyManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let bin_dir = data_dir.join("binaries");
        let config_dir = data_dir.join("config");
        let binary_path = bin_dir.join(Installer::binary_name());

        Self {
            process_mgr: ProcessManager::new(binary_path),
            bin_dir,
            config_dir,
            installed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn ensure_installed(&self, app_handle: &AppHandle) -> Result<PathBuf, ProxyError> {
        if self.installed.load(Ordering::SeqCst) || Installer::is_installed(&self.bin_dir) {
            self.installed.store(true, Ordering::SeqCst);
            return Ok(self.bin_dir.join(Installer::binary_name()));
        }

        let logger = app_handle.state::<Mutex<Logger>>();
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Installer,
            "Looking for sing-box updates...".into(),
        );

        let version = Installer::latest_version(app_handle).await?;
        let binary_path = Installer::install(app_handle, &self.bin_dir, &version).await?;
        self.installed.store(true, Ordering::SeqCst);
        Ok(binary_path)
    }

    pub async fn start(
        &self,
        app_handle: &AppHandle,
        proxy_port: u16,
        routing_mode: &str,
        profiles: &[ServerProfile],
        settings: &AppSettings,
    ) -> Result<(), ProxyError> {
        let logger = app_handle.state::<Mutex<Logger>>();

        // ── Idempotent start: clean up any existing process (proxy or Kill Switch) ──
        // Frontend must never manage process lifecycle. This is backend's job.
        if self.process_mgr.is_running() {
            log(
                app_handle,
                &logger,
                LogLevel::Info,
                LogSource::Proxy,
                "Cleaning up existing process (Kill Switch or stale proxy) before starting..."
                    .into(),
            );
            let _ = self.stop(app_handle).await;
        }
        // ────────────────────────────────────────────────────────────────────────────

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            format!(
                "Starting in {routing_mode} mode with {} server(s)...",
                profiles.len()
            ),
        );
        self.ensure_installed(app_handle).await?;

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            "Generating sing-box config...".into(),
        );
        let (config_json, _stats_port) = config::generate_config(
            &self.config_dir,
            proxy_port,
            routing_mode,
            profiles,
            settings,
        )?;
        let config_path = config::write_config(&config_json, &self.config_dir)?;
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            format!("Config written to {}", config_path.display()),
        );

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Process,
            "Starting sing-box process...".into(),
        );
        self.process_mgr.start(&config_path, app_handle).await?;

        // In system-proxy mode, sing-box sets OS proxy automatically
        // via mixed inbound `set_system_proxy: true`.
        if routing_mode == "system-proxy" {
            log(
                app_handle,
                &logger,
                LogLevel::Info,
                LogSource::Proxy,
                "System proxy will be managed by sing-box (set_system_proxy)".into(),
            );
        }

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Proxy,
            format!(
                "sing-box proxy process started (PID {})",
                crate::proxy::PROXY_PID.load(std::sync::atomic::Ordering::Acquire),
            ),
        );

        Ok(())
    }

    pub async fn stop(&self, app_handle: &AppHandle) -> Result<(), ProxyError> {
        // Reset Clash API port so fetcher goes direct
        crate::proxy::CLASH_API_PORT.store(0, std::sync::atomic::Ordering::Relaxed);

        let logger = app_handle.state::<Mutex<Logger>>();
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Process,
            "Terminating sing-box process...".into(),
        );
        self.process_mgr.stop().await
    }

    pub fn process_pid(&self) -> Option<u32> {
        self.process_mgr.pid()
    }

    /// Atomically get + clear the tracked PID. Used by stop_proxy under the outer lock.
    pub fn clear_pid(&self) -> Option<u32> {
        self.process_mgr.clear_pid()
    }

    pub fn status(&self) -> ConnectionStatus {
        if let Some(pid) = self.process_mgr.pid() {
            if crate::is_process_alive(pid) {
                return ConnectionStatus::Connected;
            }
            // Process died but PID wasn't cleared yet — clean up
            self.process_mgr.clear_pid();
        }
        ConnectionStatus::Disconnected
    }

    /// Activates Kill Switch: stops any running process and starts a block-only
    /// sing-box config that holds the port open but rejects all traffic.
    /// This prevents OS/apps from bypassing the proxy when it's down.
    pub async fn activate_kill_switch(
        &self,
        app_handle: &AppHandle,
        proxy_port: u16,
        routing_mode: &str,
    ) -> Result<(), ProxyError> {
        // Stop any existing process (dead proxy or previous kill switch)
        let _ = self.stop(app_handle).await;

        // Ensure sing-box binary exists (it should from initial start, but guard against edge cases)
        self.ensure_installed(app_handle).await?;

        let ks_config_json = config::generate_kill_switch_config(proxy_port, routing_mode)?;
        let config_path = config::write_config(&ks_config_json, &self.config_dir)?;

        self.process_mgr.start(&config_path, app_handle).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    // Keep the legacy CDN keyword assertions against the Core implementation.
    fn keyword_cdn_check(host: &str) -> bool {
        omnitun_core::profile::is_cdn_server(&ServerProfile {
            server: host.to_string(),
            ..ServerProfile::default()
        })
    }

    #[test]
    fn test_keyword_cdn_workers_dev() {
        assert!(keyword_cdn_check("my-worker.workers.dev"));
    }

    #[test]
    fn test_keyword_cdn_pages_dev() {
        assert!(keyword_cdn_check("my-site.pages.dev"));
    }

    #[test]
    fn test_keyword_cdn_cfargotunnel() {
        assert!(keyword_cdn_check("tunnel.cfargotunnel.com"));
    }

    #[test]
    fn test_keyword_cdn_worker_subdomain() {
        assert!(keyword_cdn_check("test.worker.example.com"));
    }

    #[test]
    fn test_keyword_cdn_fastly() {
        assert!(keyword_cdn_check("global.ssl.fastly.net"));
    }

    #[test]
    fn test_keyword_cdn_cloudflare() {
        assert!(keyword_cdn_check("cdn.cloudflare.net"));
    }

    #[test]
    fn test_keyword_cdn_cdn_prefix() {
        assert!(keyword_cdn_check("cdn.gamingdll.ir"));
    }

    #[test]
    fn test_keyword_cdn_akamai() {
        assert!(keyword_cdn_check("a1245.g.akamai.net"));
    }

    #[test]
    fn test_keyword_cdn_not_cdn() {
        assert!(!keyword_cdn_check("example.com"));
        assert!(!keyword_cdn_check("my-server.io"));
    }

    // ── Invariant checker (I-CDN-1) regression tests ─────────────────

    /// Helper: creates a ServerProfile with given params.
    fn make_test_server(
        behind_cdn: bool,
        exit_server: Option<&str>,
        exit_flag: Option<&str>,
        exit_country: Option<&str>,
        exit_city: Option<&str>,
    ) -> ServerProfile {
        ServerProfile {
            id: "test-server-001".to_string(),
            protocol: "trojan".to_string(),
            server: "1.2.3.4".to_string(),
            port: 443,
            uuid: None,
            password: Some("test-pass".to_string()),
            method: None,
            sni: Some("example.com".to_string()),
            flow: None,
            public_key: None,
            private_key: None,
            transport_type: None,
            transport_path: None,
            transport_host: None,
            reality_pbk: None,
            reality_sid: None,
            ping_ms: Some(100),
            flag: Some("US".to_string()),
            country_name: Some("United States".to_string()),
            city_name: Some("Test City".to_string()),
            wg_local_address: None,
            wg_allowed_ips: None,
            wg_dns: None,
            wg_mtu: None,
            wg_keepalive: None,
            username: None,
            original_name: "Test Server".to_string(),
            behind_cdn,
            exit_server: exit_server.map(|s| s.to_string()),
            exit_flag: exit_flag.map(|s| s.to_string()),
            exit_country_name: exit_country.map(|s| s.to_string()),
            exit_city_name: exit_city.map(|s| s.to_string()),
        }
    }

    /// Test: does not panic for any combination of params.
    #[test]
    fn test_assert_no_cdn_exit_data_never_panics() {
        let cases = vec![
            (false, None, None, None, None),
            (false, Some("1.1.1.1"), Some("US"), Some("USA"), Some("NYC")),
            (true, None, None, None, None),
            (true, Some("1.1.1.1"), None, None, None),
            (true, None, Some("US"), None, None),
            (true, None, None, Some("USA"), None),
            (true, None, None, None, Some("NYC")),
            (true, Some("1.1.1.1"), Some("US"), Some("USA"), Some("NYC")),
        ];
        for (cdn, ip, flag, country, city) in cases {
            let server = make_test_server(cdn, ip, flag, country, city);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    server.assert_no_cdn_exit_data("panic_test");
                }))
                .is_ok(),
                "assert_no_cdn_exit_data MUST NOT panic"
            );
        }
    }

    /// Test: does not mutate server state (pure observer).
    #[test]
    fn test_assert_no_cdn_exit_data_does_not_mutate() {
        let server = make_test_server(true, Some("1.2.3.4"), Some("US"), Some("USA"), Some("NYC"));
        let before = server.clone();
        server.assert_no_cdn_exit_data("mutation_test");
        assert_eq!(server.behind_cdn, before.behind_cdn);
        assert_eq!(server.exit_server, before.exit_server);
        assert_eq!(server.exit_flag, before.exit_flag);
        assert_eq!(server.exit_country_name, before.exit_country_name);
        assert_eq!(server.exit_city_name, before.exit_city_name);
    }

    /// Test: silent when behind_cdn=false (invariant not applicable).
    #[test]
    fn test_assert_no_cdn_exit_data_silent_when_not_cdn() {
        let server = make_test_server(false, Some("1.1.1.1"), Some("US"), Some("USA"), Some("NYC"));
        // Must not panic and must preserve state
        let before = server.clone();
        server.assert_no_cdn_exit_data("not_cdn");
        assert_eq!(server.exit_server, before.exit_server);
    }

    /// Test: silent when behind_cdn=true and all exit fields are None (happy path).
    #[test]
    fn test_assert_no_cdn_exit_data_silent_when_no_exit_data() {
        let server = make_test_server(true, None, None, None, None);
        let before = server.clone();
        server.assert_no_cdn_exit_data("clean");
        assert_eq!(server.exit_server, before.exit_server);
    }

    /// Test: simulate B4 regression — violation + recovery (no panic).
    #[test]
    fn test_regression_simulate_b4_violation_and_recovery() {
        let mut server = make_test_server(
            true,
            Some("151.115.75.222"),
            Some("FR"),
            Some("France"),
            Some("Paris"),
        );
        // Step 1: violation — must not panic
        server.assert_no_cdn_exit_data("regression_b4_step1");
        // Step 2: clear (as production code does)
        server.exit_server = None;
        server.exit_flag = None;
        server.exit_country_name = None;
        server.exit_city_name = None;
        // Step 3: after clearing, no panic and state is clean
        server.assert_no_cdn_exit_data("regression_b4_step3");
        assert!(server.exit_server.is_none());
        assert!(server.exit_flag.is_none());
        assert!(server.exit_country_name.is_none());
        assert!(server.exit_city_name.is_none());
    }

    // ═══════════════════════════════════════════════════════════════════
    // LOG-VERIFICATION TESTS (with_captured_logs helper)
    // ═══════════════════════════════════════════════════════════════════
    //
    // Используем with_captured_logs (tracing::subscriber::with_default)
    // вместо tracing-test, потому что tracing_test использует
    // set_global_default, который может быть установлен только ОДИН раз
    // на процесс. При параллельных тестах tracing_test даёт ложно-отрицательные
    // результаты (ошибки из-за того, что subscriber уже установлен другим тестом).
    //
    // with_captured_logs создаёт изолированный subscriber через
    // with_default — не затрагивает глобальный subscriber и работает
    // корректно при параллельном запуске тестов.

    /// Helper: run closure under a custom tracing subscriber that captures
    /// all output to a String. Uses with_default() so it doesn't conflict
    /// with other tests running in parallel.
    fn with_captured_logs<F: FnOnce()>(f: F) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let buf_writer = buf.clone();

        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for BufWriter {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.lock().unwrap().flush()
            }
        }

        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufWriter(buf_writer.clone()))
            .with_target(true)
            .with_level(true)
            .finish();

        tracing::subscriber::with_default(subscriber, f);

        let captured = buf.lock().unwrap();
        String::from_utf8_lossy(&captured).to_string()
    }

    /// ТЕСТ: нарушение инварианты логируется с префиксом [BUG][I-CDN-1].
    #[test]
    fn test_assert_no_cdn_exit_data_logs_when_violated() {
        let output = with_captured_logs(|| {
            let server = make_test_server(true, Some("151.115.75.222"), None, None, None);
            server.assert_no_cdn_exit_data("test_violation");
        });
        assert!(output.contains("[BUG][I-CDN-1]"), "output: {output}");
        assert!(output.contains("INVARIANT VIOLATION"), "output: {output}");
        assert!(output.contains("test_violation"), "output: {output}");
        assert!(output.contains("test-server-001"), "output: {output}");
        assert!(output.contains("1.2.3.4:443"), "output: {output}");
        assert!(output.contains("151.115.75.222"), "output: {output}");
        assert!(output.contains("exit_server"), "output: {output}");
    }

    /// ТЕСТ: happy path — нет нарушения, нет [BUG]-лога.
    #[test]
    fn test_assert_no_cdn_exit_data_silent_when_no_exit_data_traced() {
        let output = with_captured_logs(|| {
            let server = make_test_server(true, None, None, None, None);
            server.assert_no_cdn_exit_data("test_clean");
        });
        assert!(!output.contains("[BUG]"), "output: {output}");
        assert!(!output.contains("I-CDN-1"), "output: {output}");
        assert!(!output.contains("INVARIANT VIOLATION"), "output: {output}");
    }

    /// ТЕСТ: не-CDN сервер — инварианта не применима.
    #[test]
    fn test_assert_no_cdn_exit_data_silent_when_not_cdn_traced() {
        let output = with_captured_logs(|| {
            let server = make_test_server(
                false,
                Some("151.115.75.222"),
                Some("FR"),
                Some("France"),
                Some("Paris"),
            );
            server.assert_no_cdn_exit_data("test_not_cdn");
        });
        assert!(!output.contains("[BUG]"), "output: {output}");
        assert!(!output.contains("I-CDN-1"), "output: {output}");
    }

    /// ТЕСТ: все 4 поля заполнены — все логируются.
    #[test]
    fn test_assert_no_cdn_exit_data_logs_all_violating_fields() {
        let output = with_captured_logs(|| {
            let server = make_test_server(
                true,
                Some("1.1.1.1"),
                Some("US"),
                Some("United States"),
                Some("Test City"),
            );
            server.assert_no_cdn_exit_data("test_all_fields");
        });
        assert!(output.contains("exit_server"), "output: {output}");
        assert!(output.contains("exit_flag"), "output: {output}");
        assert!(output.contains("exit_country"), "output: {output}");
        assert!(output.contains("exit_city"), "output: {output}");
        assert!(output.contains("1.1.1.1"), "output: {output}");
        assert!(output.contains("United States"), "output: {output}");
        assert!(output.contains("Test City"), "output: {output}");
    }

    /// ТЕСТ: правильный target "cdn-invariant" (косвенно).
    #[test]
    fn test_assert_no_cdn_exit_data_correct_target_indirect() {
        let output = with_captured_logs(|| {
            let server = make_test_server(true, Some("1.2.3.4"), None, None, None);
            server.assert_no_cdn_exit_data("target_test");
        });
        assert!(output.contains("[BUG][I-CDN-1]"), "output: {output}");
    }

    /// ТЕСТ: context в сообщении.
    #[test]
    fn test_assert_no_cdn_exit_data_includes_context() {
        let output = with_captured_logs(|| {
            let server = make_test_server(true, Some("1.2.3.4"), None, None, None);
            server.assert_no_cdn_exit_data("UNIQUE_CONTEXT_MARKER_42");
        });
        assert!(
            output.contains("UNIQUE_CONTEXT_MARKER_42"),
            "output: {output}"
        );
    }

    /// ТЕСТ: server id в сообщении.
    #[test]
    fn test_assert_no_cdn_exit_data_includes_server_id() {
        let output = with_captured_logs(|| {
            let mut server = make_test_server(true, Some("1.2.3.4"), None, None, None);
            server.id = "CUSTOM_ID_FOR_TEST_12345".to_string();
            server.assert_no_cdn_exit_data("id_test");
        });
        assert!(
            output.contains("CUSTOM_ID_FOR_TEST_12345"),
            "output: {output}"
        );
    }

    /// ТЕСТ: server:port в сообщении.
    #[test]
    fn test_assert_no_cdn_exit_data_includes_server_address() {
        let output = with_captured_logs(|| {
            let mut server = make_test_server(true, Some("1.2.3.4"), None, None, None);
            server.server = "9.8.7.6".to_string();
            server.port = 9999;
            server.assert_no_cdn_exit_data("addr_test");
        });
        assert!(output.contains("9.8.7.6:9999"), "output: {output}");
    }

    /// ТЕСТ: проверка частичных нарушений (только 1 из 4 полей).
    #[test]
    fn test_assert_no_cdn_exit_data_partial_violations() {
        let out1 = with_captured_logs(|| {
            let s = make_test_server(true, None, Some("FR"), None, None);
            s.assert_no_cdn_exit_data("partial_flag");
        });
        assert!(out1.contains("[BUG][I-CDN-1]"), "out1: {out1}");
        assert!(out1.contains("exit_flag"), "out1: {out1}");

        let out2 = with_captured_logs(|| {
            let s = make_test_server(true, None, None, Some("France"), None);
            s.assert_no_cdn_exit_data("partial_country");
        });
        assert!(out2.contains("[BUG][I-CDN-1]"), "out2: {out2}");

        let out3 = with_captured_logs(|| {
            let s = make_test_server(true, None, None, None, Some("Paris"));
            s.assert_no_cdn_exit_data("partial_city");
        });
        assert!(out3.contains("[BUG][I-CDN-1]"), "out3: {out3}");
    }

    /// ТЕСТ: проверка target "cdn-invariant" через содержимое лога.
    #[test]
    fn test_assert_no_cdn_exit_data_target_via_logs() {
        let output = with_captured_logs(|| {
            let server = make_test_server(true, Some("1.2.3.4"), None, None, None);
            server.assert_no_cdn_exit_data("target_test");
        });
        assert!(
            output.contains("cdn-invariant"),
            "target 'cdn-invariant' should appear in log output: {output}"
        );
    }
}
