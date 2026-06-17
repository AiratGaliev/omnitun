use crate::vpn::VpnStatus;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize)]
pub struct VpnStatusPayload {
    pub status: VpnStatus,
}

pub fn emit_status(app_handle: &AppHandle, status: VpnStatus) {
    let payload = VpnStatusPayload { status };
    let _ = app_handle.emit("vpn-status-changed", payload);
}

pub fn emit_error(app_handle: &AppHandle, message: &str) {
    let _ = app_handle.emit("vpn-error", serde_json::json!({ "message": message }));
}

pub fn emit_subscription_updated(app_handle: &AppHandle) {
    let _ = app_handle.emit("subscription-updated", serde_json::json!({}));
}
