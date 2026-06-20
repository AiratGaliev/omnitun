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

#[derive(Debug, Clone, Serialize)]
pub struct TrafficPayload {
    pub up: f64,
    pub down: f64,
}

pub fn emit_traffic(app_handle: &AppHandle, up: f64, down: f64) {
    let _ = app_handle.emit("traffic-update", TrafficPayload { up, down });
}
