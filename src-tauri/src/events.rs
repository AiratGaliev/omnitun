use crate::proxy::ConnectionStatus;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionStatusPayload {
    pub status: ConnectionStatus,
}

pub fn emit_status(app_handle: &AppHandle, status: ConnectionStatus) {
    let payload = ConnectionStatusPayload { status };
    if let Err(e) = app_handle.emit("connection-status-changed", payload) {
        tracing::warn!(target: "system", "emit connection-status-changed failed: {e}");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficPayload {
    pub up: f64,
    pub down: f64,
}

pub fn emit_traffic(app_handle: &AppHandle, up: f64, down: f64) {
    if let Err(e) = app_handle.emit("traffic-update", TrafficPayload { up, down }) {
        tracing::warn!(target: "system", "emit traffic-update failed: {e}");
    }
}

// ── Batch testing progress ────────────────────────────────────────────────

/// Detailed progress of a batch server test cycle.
/// Emitted as `refresh-progress` Tauri event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshProgress {
    /// ID of the subscription profile being refreshed.
    pub profile_id: String,
    /// Total servers in this batch.
    pub total: usize,
    /// Servers tested so far.
    pub tested: usize,
    /// Hits from positive cache (no real test needed).
    pub cache_hits: usize,
    /// Servers that passed the real test.
    pub successful: usize,
    /// Servers that failed the real test.
    pub failed: usize,
    /// Current phase label for the UI progress bar.
    pub phase: String,
    /// Backend updated_at timestamp (RFC3339), set only on phase=done.
    pub updated_at: String,
    /// Backend last_tested timestamp (RFC3339), set only on phase=done.
    pub last_tested: Option<String>,
}

/// Emit a batch testing progress event to the frontend.
pub fn emit_refresh_progress(app_handle: &AppHandle, progress: RefreshProgress) {
    if let Err(e) = app_handle.emit("refresh-progress", progress) {
        tracing::warn!(target: "system", "emit refresh-progress failed: {e}");
    }
}

// ── Incremental server discovery (streaming during batch test) ────────────

/// A chunk of servers that just passed testing and are ready to be displayed
/// in the Dashboard's "Available Endpoints" dropdown. Emitted as
/// `servers-discovered` Tauri event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServersDiscoveredPayload {
    /// ID of the subscription profile being refreshed.
    pub profile_id: String,
    /// Chunk of fully-validated servers (passed ping + batch test, or
    /// cache hit, or CDN skip).
    pub servers: Vec<crate::proxy::ServerProfile>,
}

/// Emit a chunk of discovered servers to the frontend.
/// The backend chunks at ~10 servers per emit to avoid flooding the IPC channel.
pub fn emit_servers_discovered(
    app_handle: &AppHandle,
    profile_id: &str,
    servers: Vec<crate::proxy::ServerProfile>,
) {
    if servers.is_empty() {
        return;
    }
    let payload = ServersDiscoveredPayload {
        profile_id: profile_id.to_string(),
        servers,
    };
    if let Err(e) = app_handle.emit("servers-discovered", payload) {
        tracing::warn!(target: "system", "emit servers-discovered failed: {e}");
    }
}

// ── Proxy connection progress ───────────────────────────────────────

/// Granular progress of start_proxy. Emitted as `proxy-progress` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyProgress {
    /// Machine-readable phase identifier (for potential UI logic).
    pub phase: String,
    /// Human-readable message (already localized by the backend;
    /// the frontend can display it directly or use the phase key
    /// for i18n lookup).
    pub message: String,
}

pub fn emit_proxy_progress(app_handle: &AppHandle, phase: &str, message: impl Into<String>) {
    let progress = ProxyProgress {
        phase: phase.to_string(),
        message: message.into(),
    };
    if let Err(e) = app_handle.emit("proxy-progress", progress) {
        tracing::warn!(target: "system", "emit proxy-progress failed: {e}");
    }
}

/// Per-server test result — emitted immediately when each server's test
/// completes, so the frontend can update rows incrementally instead of
/// waiting for the whole batch.
///
/// Fields:
/// - `phase`: "init" | "cache" | "preflight" | "batch" | "error"
/// - `static_data`: populated only at phase="init" (protocol, server, port, flag)
/// - Dynamic metrics: ping_ms, exit_* fields, behind_cdn
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTestedEvent {
    pub profile_id: String,
    /// Phase identifier: "init" | "cache" | "preflight" | "batch" | "error"
    pub phase: String,
    pub server_id: String,
    /// Static data — only populated when phase="init"
    pub static_data: Option<ServerStaticData>,
    /// Dynamic metrics — populated in cache/preflight/batch phases
    pub ping_ms: Option<u64>,
    pub exit_server: Option<String>,
    pub exit_flag: Option<String>,
    pub exit_country_name: Option<String>,
    pub exit_city_name: Option<String>,
    pub behind_cdn: Option<bool>,
    pub error: Option<String>,
}

/// Static server fields sent once at init phase.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStaticData {
    pub protocol: String,
    pub server: String,
    pub port: u16,
    pub original_name: String,
    pub flag: Option<String>,
    pub country_name: Option<String>,
}

pub fn emit_server_tested(app: &AppHandle, event: ServerTestedEvent) {
    let _ = app.emit("server-tested", event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::ConnectionStatus;

    #[test]
    fn test_connection_status_serialization_disconnected() {
        let s = ConnectionStatus::Disconnected;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"disconnected\"");
    }

    #[test]
    fn test_connection_status_serialization_connecting() {
        let s = ConnectionStatus::Connecting;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"connecting\"");
    }

    #[test]
    fn test_connection_status_serialization_connected() {
        let s = ConnectionStatus::Connected;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"connected\"");
    }

    #[test]
    fn test_connection_status_serialization_error() {
        let s = ConnectionStatus::Error("test error".into());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("error"));
        assert!(json.contains("test error"));
    }

    #[test]
    fn test_connection_status_payload_deserialization() {
        let raw = r#"{"status":"connected"}"#;
        let p: ConnectionStatusPayload = serde_json::from_str(raw).unwrap();
        assert!(matches!(p.status, ConnectionStatus::Connected));
    }

    #[test]
    fn test_connection_status_payload_deserialization_error() {
        let raw = r#"{"status":{"error":"connection lost"}}"#;
        let p: ConnectionStatusPayload = serde_json::from_str(raw).unwrap();
        assert!(matches!(p.status, ConnectionStatus::Error(_)));
    }

    #[test]
    fn test_traffic_payload_serialization() {
        let p = TrafficPayload {
            up: 123.0,
            down: 456.5,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("123.0"));
        assert!(json.contains("456.5"));
        let deser: TrafficPayload = serde_json::from_str(&json).unwrap();
        assert!((deser.up - 123.0).abs() < f64::EPSILON);
    }
}
