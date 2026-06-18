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

