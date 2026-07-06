use crate::logging::Logger;
use crate::profile::ProfileManager;
use crate::proxy::ProxyManager;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

pub mod profile;
pub mod proxy;

/// Environment-agnostic runtime for OmniTun core operations.
///
/// Implementations provide access to:
/// - Stateful managers (proxy, profile)
/// - Logging
/// - Event emission
/// - Cancellation tokens
///
/// NOTE: no `Clone` bound — `Clone` on trait objects is unsound.
/// Pass via `Arc<dyn Runtime>` instead.
#[async_trait]
pub trait Runtime: Send + Sync + 'static {
    /// Emit a named event with a JSON payload.
    /// In Tauri mode: forwarded as IPC events to the frontend.
    /// In MCP mode: forwarded as notifications to the client.
    async fn emit(&self, event: &str, payload: Value);

    /// Proxy manager — controls the sing-box process lifecycle.
    fn proxy_state(&self) -> Arc<TokioMutex<ProxyManager>>;

    /// Profile manager — manages subscription profiles and servers.
    fn profile_state(&self) -> Arc<TokioMutex<ProfileManager>>;

    /// Logger ring buffer (for backward-compat `get_logs`).
    fn logger(&self) -> Arc<Mutex<Logger>>;

    /// Cancellation token for proxy lifecycle (watchdog, failover).
    fn cancel_token(&self) -> CancellationToken;
}

// ── Tauri runtime ────────────────────────────────────────────────────────

#[cfg(feature = "tauri")]
pub mod tauri_runtime {
    use super::*;
    use crate::logging::Logger;
    use crate::profile::ProfileManager;
    use crate::proxy::ProxyManager;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::path::PathBuf;
    use tauri::{AppHandle, Emitter, Manager};
    use tokio::sync::Mutex as TokioMutex;
    use tokio_util::sync::CancellationToken;

    static TAURI_RT_LOGGER: OnceLock<Arc<Mutex<Logger>>> = OnceLock::new();
    static TAURI_RT_PROXY: OnceLock<Arc<TokioMutex<ProxyManager>>> = OnceLock::new();
    static TAURI_RT_PROFILE: OnceLock<Arc<TokioMutex<ProfileManager>>> = OnceLock::new();
    static TAURI_RT_CANCEL: OnceLock<CancellationToken> = OnceLock::new();
    static TAURI_RT_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

    pub struct TauriRuntime {
        app: AppHandle,
    }

    impl TauriRuntime {
        pub fn new(app: AppHandle) -> Self {
            Self { app }
        }

        /// Initialise statics at setup time.
        pub fn init(app: &AppHandle) {
            let _ = TAURI_RT_DATA_DIR.set(
                app.path().app_data_dir().expect("app data dir"),
            );
            let _ = TAURI_RT_LOGGER.set(
                Arc::new(Mutex::new(Logger::new(1000, None))),
            );
            if let Some(ps) = app.try_state::<crate::ProxyState>() {
                let _ = TAURI_RT_PROXY.set(ps.manager.clone());
            }
            if let Some(ps) = app.try_state::<crate::ProfileState>() {
                let _ = TAURI_RT_PROFILE.set(ps.manager.clone());
            }
            if let Some(ps) = app.try_state::<crate::ProxyCancelState>() {
                if let Ok(g) = ps.0.lock() {
                    let _ = TAURI_RT_CANCEL.set(g.clone());
                }
            }
        }
    }

    impl Clone for TauriRuntime {
        fn clone(&self) -> Self {
            Self { app: self.app.clone() }
        }
    }

    #[async_trait]
    impl Runtime for TauriRuntime {
        async fn emit(&self, event: &str, payload: Value) {
            let _ = self.app.emit(event, payload);
        }

        fn proxy_state(&self) -> Arc<TokioMutex<ProxyManager>> {
            TAURI_RT_PROXY
                .get()
                .cloned()
                .unwrap_or_else(|| {
                    let ps = self.app.state::<crate::ProxyState>();
                    ps.manager.clone()
                })
        }

        fn profile_state(&self) -> Arc<TokioMutex<ProfileManager>> {
            TAURI_RT_PROFILE
                .get()
                .cloned()
                .unwrap_or_else(|| {
                    let ps = self.app.state::<crate::ProfileState>();
                    ps.manager.clone()
                })
        }

        fn logger(&self) -> Arc<Mutex<Logger>> {
            TAURI_RT_LOGGER
                .get()
                .cloned()
                .unwrap_or_else(|| {
                    let l = self.app.state::<Mutex<Logger>>();
                    let guard = l.lock().unwrap_or_else(|e| e.into_inner());
                    let entries = guard.entries();
                    let logger = Logger::new(entries.len().max(1000), None);
                    Arc::new(Mutex::new(logger))
                })
        }

        fn cancel_token(&self) -> CancellationToken {
            TAURI_RT_CANCEL
                .get()
                .cloned()
                .unwrap_or_default()
        }
    }
}

// ── MCP runtime ──────────────────────────────────────────────────────────

#[cfg(feature = "mcp-server")]
pub mod mcp_runtime {
    use super::*;
    use crate::logging::Logger;
    use crate::profile::ProfileManager;
    use crate::proxy::ProxyManager;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::sync::Mutex as TokioMutex;
    use tokio_util::sync::CancellationToken;

    /// Headless runtime for the MCP server.
    pub struct McpRuntime {
        proxy_state: Arc<TokioMutex<ProxyManager>>,
        profile_state: Arc<TokioMutex<ProfileManager>>,
        logger: Arc<Mutex<Logger>>,
        cancel: CancellationToken,
        data_dir: PathBuf,
        settings: Arc<TokioMutex<crate::settings::AppSettings>>,
    }

    impl McpRuntime {
        pub fn new(
            data_dir: PathBuf,
            proxy_state: Arc<TokioMutex<ProxyManager>>,
            profile_state: Arc<TokioMutex<ProfileManager>>,
            logger: Arc<Mutex<Logger>>,
            settings: Arc<TokioMutex<crate::settings::AppSettings>>,
        ) -> Self {
            Self {
                proxy_state,
                profile_state,
                logger,
                cancel: CancellationToken::new(),
                data_dir,
                settings,
            }
        }

        pub fn data_dir(&self) -> &PathBuf {
            &self.data_dir
        }

        pub fn settings(&self) -> &TokioMutex<crate::settings::AppSettings> {
            &self.settings
        }
    }

    impl Clone for McpRuntime {
        fn clone(&self) -> Self {
            Self {
                proxy_state: self.proxy_state.clone(),
                profile_state: self.profile_state.clone(),
                logger: self.logger.clone(),
                cancel: self.cancel.clone(),
                data_dir: self.data_dir.clone(),
                settings: self.settings.clone(),
            }
        }
    }

    #[async_trait]
    impl Runtime for McpRuntime {
        async fn emit(&self, _event: &str, _payload: Value) {
            // MCP mode: events are forwarded via transport in mcp_server module
        }

        fn proxy_state(&self) -> Arc<TokioMutex<ProxyManager>> {
            self.proxy_state.clone()
        }

        fn profile_state(&self) -> Arc<TokioMutex<ProfileManager>> {
            self.profile_state.clone()
        }

        fn logger(&self) -> Arc<Mutex<Logger>> {
            self.logger.clone()
        }

        fn cancel_token(&self) -> CancellationToken {
            self.cancel.clone()
        }
    }
}
