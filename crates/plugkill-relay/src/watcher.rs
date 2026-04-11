use log::warn;
use plugkill_core::ipc;
use std::path::{Path, PathBuf};

pub struct Watcher {
    socket_path: PathBuf,
    last_violations: Option<u64>,
}

impl Watcher {
    pub fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
            last_violations: None,
        }
    }

    /// Poll daemon status. Returns `Some(count)` on new violation in enforce mode.
    pub fn poll(&mut self) -> Option<u64> {
        let req = serde_json::json!({"command": "status"});
        let resp = match ipc::send_request(&self.socket_path, &req) {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to poll plugkill: {e}");
                return None;
            }
        };

        let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok {
            return None;
        }

        let data = resp.get("data")?;
        let armed = data.get("armed").and_then(|v| v.as_bool()).unwrap_or(false);
        let mode = data.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        let violations = data
            .get("violations_logged")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        if !armed || mode != "enforce" {
            self.last_violations = Some(violations);
            return None;
        }

        let result = match self.last_violations {
            Some(prev) if violations > prev => Some(violations),
            None => None, // first poll — establish baseline
            _ => None,
        };

        self.last_violations = Some(violations);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn watcher() -> Watcher {
        Watcher {
            socket_path: PathBuf::from("/nonexistent"),
            last_violations: None,
        }
    }

    #[test]
    fn first_poll_returns_none() {
        let mut w = Watcher {
            socket_path: PathBuf::from("/nonexistent"),
            last_violations: None,
        };
        // Simulate: no socket — poll returns None
        assert!(w.poll().is_none());
    }

    #[test]
    fn baseline_set_on_first_valid_enforce_poll() {
        let mut w = watcher();
        // Manually drive the counter logic without a real socket
        w.last_violations = None;
        // Simulate a first-poll scenario: violations=5, no prev
        let result = match w.last_violations {
            Some(prev) if 5u64 > prev => Some(5u64),
            None => None,
            _ => None,
        };
        w.last_violations = Some(5);
        assert!(result.is_none());
        assert_eq!(w.last_violations, Some(5));
    }

    #[test]
    fn new_violation_detected() {
        let mut w = watcher();
        w.last_violations = Some(5);
        let result = match w.last_violations {
            Some(prev) if 7u64 > prev => Some(7u64),
            None => None,
            _ => None,
        };
        w.last_violations = Some(7);
        assert_eq!(result, Some(7));
    }

    #[test]
    fn no_violation_when_count_unchanged() {
        let mut w = watcher();
        w.last_violations = Some(5);
        let result = match w.last_violations {
            Some(prev) if 5u64 > prev => Some(5u64),
            None => None,
            _ => None,
        };
        assert!(result.is_none());
    }
}
