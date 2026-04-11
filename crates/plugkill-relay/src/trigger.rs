use log::{error, info, warn};
use plugkill_core::ipc;
use std::path::Path;
use std::process::Command;

/// Trigger local kill via Unix socket. Falls back to direct poweroff.
pub fn trigger_local_kill(socket_path: &Path) {
    info!("triggering local kill sequence");

    let enforce_req = serde_json::json!({"command": "enforce"});
    if let Err(e) = ipc::send_request(socket_path, &enforce_req) {
        warn!("failed to send enforce command: {e} — falling back to poweroff");
        force_poweroff();
        return;
    }

    let arm_req = serde_json::json!({"command": "arm"});
    if let Err(e) = ipc::send_request(socket_path, &arm_req) {
        warn!("failed to send arm command: {e} — falling back to poweroff");
        force_poweroff();
        return;
    }

    info!("local plugkill armed and in enforce mode");
}

fn force_poweroff() {
    error!("executing direct poweroff -f");
    let _ = Command::new("poweroff").arg("-f").spawn();
}
