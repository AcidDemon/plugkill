use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Default control-socket path. Linux uses `/run` (systemd RuntimeDirectory);
/// FreeBSD and others have no `/run`, so `/var/run`.
#[cfg(target_os = "linux")]
pub const DEFAULT_SOCKET_PATH: &str = "/run/plugkill/plugkill.sock";
#[cfg(not(target_os = "linux"))]
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/plugkill/plugkill.sock";

/// JSON request from a client.
#[derive(Debug, Deserialize)]
#[serde(tag = "command")]
pub enum Request {
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "disarm")]
    Disarm { timeout_secs: u64 },
    #[serde(rename = "arm")]
    Arm,
    #[serde(rename = "learn")]
    Learn,
    #[serde(rename = "enforce")]
    Enforce,
    #[serde(rename = "reload")]
    Reload,
    /// Run the configured kill sequence. Sent by plugkill-relay when a
    /// signature-verified KILL arrives from a trusted peer.
    #[serde(rename = "kill")]
    Kill { reason: String },
}

/// JSON response to a client.
#[derive(Debug, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

/// Connect to the daemon, send one JSON request, and return the response line.
fn request_line(socket_path: &Path, request: &serde_json::Value) -> Result<String, String> {
    let stream = UnixStream::connect(socket_path).map_err(|e| {
        format!(
            "cannot connect to daemon socket {}: {e} (is the daemon running?)",
            socket_path.display()
        )
    })?;

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("failed to set socket timeout: {e}"))?;

    let mut writer = &stream;
    let reader = BufReader::new(&stream);

    let mut req_json = serde_json::to_string(request).map_err(|e| format!("JSON error: {e}"))?;
    req_json.push('\n');
    writer
        .write_all(req_json.as_bytes())
        .map_err(|e| format!("failed to send command: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("failed to flush: {e}"))?;

    match reader.lines().next() {
        Some(Ok(line)) => Ok(line),
        Some(Err(e)) => Err(format!("failed to read response: {e}")),
        None => Err("no response from daemon".to_string()),
    }
}

/// Send a request to the daemon and return the parsed JSON response.
///
/// The whole envelope comes back, `ok: false` included: callers such as the
/// relay's trigger and the tray inspect `ok` themselves.
pub fn send_request(
    socket_path: &Path,
    request: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let line = request_line(socket_path, request)?;
    serde_json::from_str(&line).map_err(|e| format!("invalid JSON response: {e}"))
}

/// Send a request to the daemon and print the response.
///
/// When `raw_json` is true, the response is pretty-printed as JSON.
/// Otherwise, a human-readable summary is printed.
///
/// A daemon that answers `ok: false` rejected the command, so its error is
/// returned rather than printed: the caller's exit code has to show it.
pub fn send_command(
    socket_path: &Path,
    request: &serde_json::Value,
    raw_json: bool,
) -> Result<(), String> {
    let line = request_line(socket_path, request)?;

    if let Ok(resp) = serde_json::from_str::<serde_json::Value>(&line)
        && resp.get("ok").and_then(|v| v.as_bool()) != Some(true)
    {
        return Err(resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error")
            .to_string());
    }

    if raw_json {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            println!("{}", serde_json::to_string_pretty(&value).unwrap_or(line));
        } else {
            println!("{line}");
        }
    } else {
        print_human_response(&line);
    }
    Ok(())
}

/// Render a whole-second duration as `1h 2m 5s`, dropping empty leading units.
pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Render a successful response envelope. `send_command` has already turned an
/// `ok: false` envelope into an error, so only the success shapes get here.
fn print_human_response(line: &str) {
    let Ok(resp) = serde_json::from_str::<serde_json::Value>(line) else {
        println!("{line}");
        return;
    };

    let Some(data) = resp.get("data") else {
        println!("OK");
        return;
    };

    // Status response (has "armed" field)
    if let Some(armed) = data.get("armed").and_then(|v| v.as_bool()) {
        let mode = data
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let status_str = if armed { "armed" } else { "disarmed" };
        println!("Status:     {status_str} ({mode} mode)");

        if let Some(secs) = data.get("uptime_secs").and_then(|v| v.as_u64()) {
            println!("Uptime:     {}", format_duration(secs));
        }

        if let Some(secs) = data.get("disarm_remaining_secs").and_then(|v| v.as_u64())
            && secs > 0
        {
            println!("Re-arms in: {}", format_duration(secs));
        }

        let usb = data
            .get("usb_devices")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let tb = data
            .get("thunderbolt_devices")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let sd = data
            .get("sdcard_devices")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let pci = data
            .get("pci_devices")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!("Devices:    {usb} USB IDs, {tb} Thunderbolt, {sd} SD card, {pci} PCI");

        let mut watching = Vec::new();
        if data.get("usb_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("USB");
        }
        if data.get("thunderbolt_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("Thunderbolt");
        }
        if data.get("sdcard_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("SD card");
        }
        if data.get("power_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("power supply");
        }
        if data.get("network_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("network");
        }
        if data.get("lid_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("lid");
        }
        if data.get("pci_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("PCI");
        }
        if data.get("display_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("display");
        }
        if !watching.is_empty() {
            println!("Watching:   {}", watching.join(", "));
        }

        let violations = data
            .get("violations_logged")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!("Violations: {violations}");

        if let Some(ms) = data.get("last_poll_ms_ago").and_then(|v| v.as_u64()) {
            println!("Last poll:  {ms}ms ago");
        }

        return;
    }

    // Action responses (have "message" field)
    if let Some(msg) = data.get("message").and_then(|v| v.as_str()) {
        println!("{msg}");
        return;
    }

    // Fallback: pretty-print as JSON
    if let Ok(pretty) = serde_json::to_string_pretty(data) {
        println!("{pretty}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

    #[test]
    fn test_kill_request_deserializes() {
        // Exactly the bytes plugkill-relay's trigger::trigger_local_kill sends.
        let line = r#"{"command":"kill","reason":"peer alpha lost AC power"}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        let Request::Kill { reason } = req else {
            panic!("expected Request::Kill");
        };
        assert_eq!(reason, "peer alpha lost AC power");
    }

    #[test]
    fn test_kill_request_requires_reason() {
        let err = serde_json::from_str::<Request>(r#"{"command":"kill"}"#).unwrap_err();
        assert!(err.to_string().contains("reason"));
    }

    /// Serve exactly one request on a fresh socket and answer with `response`.
    /// The TempDir is returned so the caller keeps the socket alive.
    fn serve_once(response: &'static str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plugkill.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut req = String::new();
            BufReader::new(&stream).read_line(&mut req).unwrap();
            let mut writer = &stream;
            writeln!(writer, "{response}").unwrap();
            writer.flush().unwrap();
        });
        (dir, path)
    }

    #[test]
    fn test_send_command_errors_on_not_ok_envelope() {
        let (_dir, path) = serve_once(r#"{"ok":false,"error":"timeout_secs must be > 0"}"#);
        let req = serde_json::json!({"command": "disarm", "timeout_secs": 0});

        let err = send_command(&path, &req, false).unwrap_err();

        assert_eq!(err, "timeout_secs must be > 0");
    }

    #[test]
    fn test_send_command_accepts_ok_envelope() {
        let (_dir, path) = serve_once(r#"{"ok":true,"data":{"message":"disarmed for 60s"}}"#);
        let req = serde_json::json!({"command": "disarm", "timeout_secs": 60});

        assert!(send_command(&path, &req, false).is_ok());
    }

    #[test]
    fn test_send_request_returns_whole_envelope() {
        let (_dir, path) = serve_once(r#"{"ok":false,"error":"nope"}"#);
        let req = serde_json::json!({"command": "kill", "reason": "test"});

        // send_request must not judge the envelope: the relay reads resp["ok"].
        let resp = send_request(&path, &req).unwrap();

        assert_eq!(resp["ok"], serde_json::Value::Bool(false));
        assert_eq!(resp["error"], "nope");
    }

    #[test]
    fn test_format_duration_units() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3725), "1h 2m 5s");
    }
}
