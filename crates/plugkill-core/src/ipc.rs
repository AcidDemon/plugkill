use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Default socket path (inside systemd RuntimeDirectory).
pub const DEFAULT_SOCKET_PATH: &str = "/run/plugkill/plugkill.sock";

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

/// Send a request to the daemon and return the parsed JSON response.
pub fn send_request(
    socket_path: &Path,
    request: &serde_json::Value,
) -> Result<serde_json::Value, String> {
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

    let mut lines = reader.lines();
    match lines.next() {
        Some(Ok(line)) => {
            serde_json::from_str(&line).map_err(|e| format!("invalid JSON response: {e}"))
        }
        Some(Err(e)) => Err(format!("failed to read response: {e}")),
        None => Err("no response from daemon".to_string()),
    }
}

/// Send a request to the daemon and print the response.
///
/// When `raw_json` is true, the response is pretty-printed as JSON.
/// Otherwise, a human-readable summary is printed.
pub fn send_command(
    socket_path: &Path,
    request: &serde_json::Value,
    raw_json: bool,
) -> Result<(), String> {
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

    // Read response
    let mut lines = reader.lines();
    match lines.next() {
        Some(Ok(line)) => {
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
        Some(Err(e)) => Err(format!("failed to read response: {e}")),
        None => Err("no response from daemon".to_string()),
    }
}

fn format_duration(secs: u64) -> String {
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

fn print_human_response(line: &str) {
    let Ok(resp) = serde_json::from_str::<serde_json::Value>(line) else {
        println!("{line}");
        return;
    };

    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);

    if !ok {
        let msg = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        eprintln!("Error: {msg}");
        return;
    }

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
        println!("Devices:    {usb} USB, {tb} Thunderbolt, {sd} SD card");

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
            watching.push("Power");
        }
        if data.get("network_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("Network");
        }
        if data.get("lid_watching").and_then(|v| v.as_bool()) == Some(true) {
            watching.push("Lid");
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
