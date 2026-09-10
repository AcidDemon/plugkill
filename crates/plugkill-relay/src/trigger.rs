use log::{error, info, warn};
use plugkill_core::ipc;
use std::path::Path;
use std::process::Command;

/// What the daemon's answer to a `kill` command means.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The daemon accepted the command and runs its configured kill sequence.
    Accepted,
    /// The daemon is unreachable or refused. The string is the log reason.
    Poweroff(String),
}

/// Ask the local daemon to run its kill sequence over the control socket.
///
/// Falls back to a direct poweroff on transport failure or on an `ok:false`
/// envelope: if plugkill is not running or will not act, there is nothing to
/// shred with and powering off is still the right outcome.
pub fn trigger_local_kill(socket_path: &Path, reason: &str) {
    info!("triggering local kill sequence");

    let req = serde_json::json!({"command": "kill", "reason": reason});
    match classify(ipc::send_request(socket_path, &req)) {
        Outcome::Accepted => info!("local plugkill accepted the kill command"),
        Outcome::Poweroff(why) => {
            warn!("{why}, falling back to poweroff");
            force_poweroff();
        }
    }
}

/// Map a `kill` response to an outcome. Pure, so the fallback decision is
/// testable without powering off the machine.
fn classify(resp: Result<serde_json::Value, String>) -> Outcome {
    let value = match resp {
        Ok(v) => v,
        Err(e) => return Outcome::Poweroff(format!("failed to send kill command: {e}")),
    };

    if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return Outcome::Accepted;
    }

    let err = value
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown error");
    Outcome::Poweroff(format!("daemon refused the kill command: {err}"))
}

fn force_poweroff() {
    #[cfg(target_os = "linux")]
    let (cmd, args): (&str, &[&str]) = ("poweroff", &["-f"]);
    #[cfg(not(target_os = "linux"))]
    let (cmd, args): (&str, &[&str]) = ("shutdown", &["-p", "now"]);
    error!("executing direct power off: {cmd} {args:?}");
    let _ = Command::new(cmd).args(args).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ok_true_is_accepted() {
        let resp = Ok(serde_json::json!({
            "ok": true,
            "data": {"message": "kill sequence scheduled"}
        }));
        assert_eq!(classify(resp), Outcome::Accepted);
    }

    #[test]
    fn test_ok_false_falls_back_to_poweroff() {
        let resp = Ok(serde_json::json!({"ok": false, "error": "kill already pending"}));
        let Outcome::Poweroff(why) = classify(resp) else {
            panic!("expected Outcome::Poweroff");
        };
        assert!(why.contains("kill already pending"));
    }

    #[test]
    fn test_transport_error_falls_back_to_poweroff() {
        let resp = Err("cannot connect to daemon socket /run/plugkill/plugkill.sock".to_string());
        let Outcome::Poweroff(why) = classify(resp) else {
            panic!("expected Outcome::Poweroff");
        };
        assert!(why.contains("cannot connect"));
    }

    #[test]
    fn test_missing_ok_field_falls_back_to_poweroff() {
        // A response we cannot read as success is not success.
        let resp = Ok(serde_json::json!({"data": {}}));
        assert!(matches!(classify(resp), Outcome::Poweroff(_)));
    }
}
