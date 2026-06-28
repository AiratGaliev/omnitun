use crate::vpn::VpnStatus;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpnStatusPayload {
    pub status: VpnStatus,
}

pub fn emit_status(app_handle: &AppHandle, status: VpnStatus) {
    let payload = VpnStatusPayload { status };
    let _ = app_handle.emit("vpn-status-changed", payload);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficPayload {
    pub up: f64,
    pub down: f64,
}

pub fn emit_traffic(app_handle: &AppHandle, up: f64, down: f64) {
    let _ = app_handle.emit("traffic-update", TrafficPayload { up, down });
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
}

/// Emit a batch testing progress event to the frontend.
pub fn emit_refresh_progress(app_handle: &AppHandle, progress: RefreshProgress) {
    let _ = app_handle.emit("refresh-progress", progress);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vpn::VpnStatus;

    #[test]
    fn test_vpn_status_serialization_disconnected() {
        let s = VpnStatus::Disconnected;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"disconnected\"");
    }

    #[test]
    fn test_vpn_status_serialization_connecting() {
        let s = VpnStatus::Connecting;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"connecting\"");
    }

    #[test]
    fn test_vpn_status_serialization_connected() {
        let s = VpnStatus::Connected;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"connected\"");
    }

    #[test]
    fn test_vpn_status_serialization_error() {
        let s = VpnStatus::Error("test error".into());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("error"));
        assert!(json.contains("test error"));
    }

    #[test]
    fn test_vpn_status_payload_deserialization() {
        let raw = r#"{"status":"connected"}"#;
        let p: VpnStatusPayload = serde_json::from_str(raw).unwrap();
        assert!(matches!(p.status, VpnStatus::Connected));
    }

    #[test]
    fn test_vpn_status_payload_deserialization_error() {
        let raw = r#"{"status":{"error":"connection lost"}}"#;
        let p: VpnStatusPayload = serde_json::from_str(raw).unwrap();
        assert!(matches!(p.status, VpnStatus::Error(_)));
    }

    #[test]
    fn test_traffic_payload_serialization() {
        let p = TrafficPayload { up: 123.0, down: 456.5 };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("123.0"));
        assert!(json.contains("456.5"));
        let deser: TrafficPayload = serde_json::from_str(&json).unwrap();
        assert!((deser.up - 123.0).abs() < f64::EPSILON);
    }
}
