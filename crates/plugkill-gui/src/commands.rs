//! Commands the tray and the dashboard send to the daemon.

use plugkill_core::ipc;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

/// Disarm durations offered everywhere: seconds, long label, short label.
pub const DISARM_PRESETS: [(u64, &str, &str); 5] = [
    (60, "1 minute", "1m"),
    (300, "5 minutes", "5m"),
    (900, "15 minutes", "15m"),
    (1800, "30 minutes", "30m"),
    (3600, "1 hour", "1h"),
];

/// The longest disarm the daemon accepts; it refuses anything above this.
pub const MAX_DISARM_SECS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Arm,
    Disarm(u64),
    Learn,
    Enforce,
    Reload,
    /// Open a pairing window, or close an open one with 0 seconds.
    Pair {
        window_secs: u64,
        for_secs: Option<u64>,
    },
    /// Allow the device behind the most recent violation.
    AllowLast {
        for_secs: Option<u64>,
    },
    /// Drop one allowance. The selector is interned rather than owned, so the
    /// command stays `Copy` for the tray and the menu; see `intern`.
    Revoke(&'static str),
    RevokeAll,
}

/// A selector the revoke command can carry. `Command` is `Copy`, so a string
/// in it has to be `'static`. The allowance table holds a handful of short
/// selectors and the same one is often clicked twice, so they are interned
/// once here rather than leaked once per click.
pub fn intern(selector: &str) -> &'static str {
    static POOL: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(found) = pool.get(selector) {
        return found;
    }
    let leaked: &'static str = selector.to_string().leak();
    pool.insert(leaked);
    leaked
}

impl Command {
    /// The request line the control socket expects.
    pub fn request(self) -> serde_json::Value {
        match self {
            Command::Arm => serde_json::json!({"command": "arm"}),
            Command::Disarm(secs) => {
                serde_json::json!({"command": "disarm", "timeout_secs": secs})
            }
            Command::Learn => serde_json::json!({"command": "learn"}),
            Command::Enforce => serde_json::json!({"command": "enforce"}),
            Command::Reload => serde_json::json!({"command": "reload"}),
            // for_secs is null when no expiry was asked for, which the daemon
            // reads as the default: an allowance that lasts this uptime.
            Command::Pair {
                window_secs,
                for_secs,
            } => {
                serde_json::json!({
                    "command": "pair", "window_secs": window_secs, "for_secs": for_secs
                })
            }
            Command::AllowLast { for_secs } => {
                serde_json::json!({"command": "allow_last", "for_secs": for_secs})
            }
            Command::Revoke(selector) => {
                serde_json::json!({"command": "revoke", "selector": selector})
            }
            Command::RevokeAll => serde_json::json!({"command": "revoke_all"}),
        }
    }

    /// Whether the daemon may put this one behind a password prompt. Status
    /// carries no `require_auth`, so this only says the command can wait on a
    /// person, not that it will.
    pub fn is_gated(self) -> bool {
        // Closing a pairing window removes a permission the way revoke does,
        // and the daemon does not gate it (plugkill/src/socket.rs).
        if let Command::Pair { window_secs: 0, .. } = self {
            return false;
        }
        ipc::GATED_COMMANDS.contains(&self.request()["command"].as_str().unwrap_or(""))
    }

    /// The command as a person reads it, for a message about it.
    pub fn name(self) -> &'static str {
        match self {
            Command::Arm => "Arm",
            Command::Disarm(_) => "Disarm",
            Command::Learn => "Learn mode",
            Command::Enforce => "Enforce mode",
            Command::Reload => "Reload",
            Command::Pair { window_secs: 0, .. } => "Close pairing",
            Command::Pair { .. } => "Pair",
            Command::AllowLast { .. } => "Allow last",
            Command::Revoke(_) => "Revoke",
            Command::RevokeAll => "Revoke all",
        }
    }
}

/// The one line the tray menu and the dashboard show when a command did not
/// run. A daemon that wants a password is named as that and not as a failure;
/// the state on screen stays whatever the daemon reports, which after a
/// refused disarm is still armed.
///
/// The match is loose on purpose: the daemon words the refusal, the clients
/// only have to recognise it.
pub fn notice(command: Command, error: &str) -> String {
    if error.to_lowercase().contains(ipc::AUTH_REQUIRED_ERROR) {
        format!("{} refused: authentication required", command.name())
    } else {
        format!("{} failed: {error}", command.name())
    }
}

/// Send `command` and wait for the answer. Blocking: never call this on the
/// GTK thread.
pub fn send(socket: &Path, command: Command) -> Result<(), String> {
    let resp = ipc::send_request(socket, &command.request())?;
    if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        Ok(())
    } else {
        Err(resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    /// Answer one request on a fresh socket with `response`, returning the
    /// request line through the join handle.
    fn serve_once(
        response: &'static str,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::thread::JoinHandle<String>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let mut writer = &stream;
            writeln!(writer, "{response}").unwrap();
            line
        });
        (dir, path, handle)
    }

    #[test]
    fn test_requests_match_the_socket_protocol() {
        assert_eq!(
            Command::Arm.request(),
            serde_json::json!({"command": "arm"})
        );
        assert_eq!(
            Command::Disarm(300).request(),
            serde_json::json!({"command": "disarm", "timeout_secs": 300})
        );
        assert_eq!(
            Command::Learn.request(),
            serde_json::json!({"command": "learn"})
        );
        assert_eq!(
            Command::Enforce.request(),
            serde_json::json!({"command": "enforce"})
        );
        assert_eq!(
            Command::Reload.request(),
            serde_json::json!({"command": "reload"})
        );
    }

    #[test]
    fn test_send_accepts_ok_and_sends_the_request() {
        let (_dir, path, handle) = serve_once(r#"{"ok":true,"data":{"message":"re-armed"}}"#);
        assert_eq!(send(&path, Command::Arm), Ok(()));
        let line: serde_json::Value = serde_json::from_str(&handle.join().unwrap()).unwrap();
        assert_eq!(line, serde_json::json!({"command": "arm"}));
    }

    #[test]
    fn test_send_returns_the_daemon_error() {
        let (_dir, path, _handle) =
            serve_once(r#"{"ok":false,"error":"timeout_secs must be > 0"}"#);
        assert_eq!(
            send(&path, Command::Disarm(0)),
            Err("timeout_secs must be > 0".to_string())
        );
    }

    #[test]
    fn test_a_refused_disarm_reads_as_authentication_not_failure() {
        let refused = serve_once(r#"{"ok":false,"error":"authentication required"}"#);
        let (_dir, path, _handle) = refused;
        let err = send(&path, Command::Disarm(300)).unwrap_err();

        assert_eq!(
            notice(Command::Disarm(300), &err),
            "Disarm refused: authentication required"
        );
        // However the daemon words it, and whatever it adds.
        assert_eq!(
            notice(
                Command::Learn,
                "Authentication required: no polkit agent answered"
            ),
            "Learn mode refused: authentication required"
        );
    }

    #[test]
    fn test_any_other_refusal_keeps_the_daemons_own_words() {
        assert_eq!(
            notice(Command::Disarm(0), "timeout_secs must be > 0"),
            "Disarm failed: timeout_secs must be > 0"
        );
        assert_eq!(
            notice(Command::Reload, "config is world-writable"),
            "Reload failed: config is world-writable"
        );
    }

    #[test]
    fn test_allowance_requests_match_the_socket_protocol() {
        assert_eq!(
            Command::Pair {
                window_secs: 30,
                for_secs: None
            }
            .request(),
            serde_json::json!({"command": "pair", "window_secs": 30, "for_secs": null})
        );
        assert_eq!(
            Command::AllowLast {
                for_secs: Some(600)
            }
            .request(),
            serde_json::json!({"command": "allow_last", "for_secs": 600})
        );
        assert_eq!(
            Command::Revoke("usb:1050:0407").request(),
            serde_json::json!({"command": "revoke", "selector": "usb:1050:0407"})
        );
        assert_eq!(
            Command::RevokeAll.request(),
            serde_json::json!({"command": "revoke_all"})
        );
    }

    #[test]
    fn test_a_selector_is_interned_once_however_often_it_is_clicked() {
        let first = intern("usb:1050:0407");
        assert_eq!(first, "usb:1050:0407");
        assert!(
            std::ptr::eq(first, intern(&format!("usb:{}", "1050:0407"))),
            "the same selector must not leak twice"
        );
        assert!(!std::ptr::eq(first, intern("pci:0000:01:00.0")));
    }

    #[test]
    fn test_a_refused_grant_reads_as_authentication_not_failure() {
        // F3: pair and allow-last are gated, so the panel has to word a
        // refusal the way the dashboard does.
        let pair = Command::Pair {
            window_secs: 300,
            for_secs: None,
        };
        assert_eq!(
            notice(pair, "authentication required"),
            "Pair refused: authentication required"
        );
        assert_eq!(
            notice(
                Command::AllowLast { for_secs: None },
                "Authentication required: no polkit agent answered"
            ),
            "Allow last refused: authentication required"
        );
        // Revoking is not gated, so its refusals keep the daemon's own words.
        assert_eq!(
            notice(
                Command::Revoke("usb:1050:0407"),
                "no allowance for usb:1050:0407"
            ),
            "Revoke failed: no allowance for usb:1050:0407"
        );
    }

    #[test]
    fn test_the_gated_commands_are_the_ones_that_can_wait_on_a_person() {
        assert!(Command::Disarm(60).is_gated());
        assert!(Command::Learn.is_gated());
        assert!(Command::Reload.is_gated());
        assert!(!Command::Arm.is_gated());
        assert!(!Command::Enforce.is_gated());
        assert!(
            Command::Pair {
                window_secs: 60,
                for_secs: None
            }
            .is_gated()
        );
        assert!(Command::AllowLast { for_secs: None }.is_gated());
        // Closing a window takes a permission away, so the daemon answers it
        // at once rather than asking for a password.
        assert!(
            !Command::Pair {
                window_secs: 0,
                for_secs: None
            }
            .is_gated()
        );
        assert!(!Command::Revoke("usb:1050:0407").is_gated());
        assert!(!Command::RevokeAll.is_gated());
    }

    #[test]
    fn test_presets_are_ascending_and_within_the_daemon_limit() {
        let secs: Vec<u64> = DISARM_PRESETS.iter().map(|p| p.0).collect();
        assert!(secs.windows(2).all(|w| w[0] < w[1]));
        assert!(secs.iter().all(|&s| (1..=MAX_DISARM_SECS).contains(&s)));
    }
}
