use crate::config::Config;
use crate::state::{Baselines, DaemonMode, DaemonState};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

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
    fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

/// Spawn a background thread that accepts connections on the Unix domain socket
/// and dispatches commands.
pub fn start_socket_listener(
    socket_path: PathBuf,
    state: Arc<Mutex<DaemonState>>,
    config: Arc<RwLock<Config>>,
    baselines: Arc<RwLock<Baselines>>,
) -> std::io::Result<()> {
    // Clean up stale socket from previous run
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    // Ensure parent directory exists
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)?;

    // Set socket permissions: 0660 (owner + group read/write)
    set_socket_permissions(&socket_path)?;

    info!("control socket listening on {}", socket_path.display());

    std::thread::Builder::new()
        .name("socket-listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let state = Arc::clone(&state);
                        let config = Arc::clone(&config);
                        let baselines = Arc::clone(&baselines);
                        std::thread::Builder::new()
                            .name("socket-handler".into())
                            .spawn(move || {
                                if let Err(e) =
                                    handle_connection(stream, &state, &config, &baselines)
                                {
                                    warn!("socket connection error: {e}");
                                }
                            })
                            .ok();
                    }
                    Err(e) => {
                        error!("socket accept error: {e}");
                    }
                }
            }
        })?;

    Ok(())
}

fn set_socket_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o660);
    std::fs::set_permissions(path, perms)
}

fn handle_connection(
    stream: UnixStream,
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> std::io::Result<()> {
    // Set a read timeout so we don't hang forever on misbehaving clients
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let reader = BufReader::new(&stream);
    let mut writer = &stream;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                // Timeout or connection closed
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(e);
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(req, state, config, baselines),
            Err(e) => Response::err(format!("invalid request: {e}")),
        };

        let mut resp_json = serde_json::to_string(&response)
            .unwrap_or_else(|_| r#"{"ok":false,"error":"serialization error"}"#.to_string());
        resp_json.push('\n');
        writer.write_all(resp_json.as_bytes())?;
        writer.flush()?;
    }

    Ok(())
}

fn handle_request(
    req: Request,
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> Response {
    match req {
        Request::Status => handle_status(state, config, baselines),
        Request::Disarm { timeout_secs } => handle_disarm(state, timeout_secs),
        Request::Arm => handle_arm(state),
        Request::Learn => handle_learn(state),
        Request::Enforce => handle_enforce(state),
        Request::Reload => handle_reload(state),
    }
}

fn handle_status(
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> Response {
    let st = state.lock().unwrap();
    let cfg = config.read().unwrap();
    let bl = baselines.read().unwrap();

    let uptime_secs = st.started_at.elapsed().as_secs();
    let disarm_remaining_secs = st.disarm_until.map(|deadline| {
        let now = Instant::now();
        if deadline > now {
            (deadline - now).as_secs()
        } else {
            0
        }
    });

    let usb_devices = bl.usb.as_ref().map(|s| s.len()).unwrap_or(0);
    let thunderbolt_devices = bl.thunderbolt.as_ref().map(|s| s.len()).unwrap_or(0);
    let sdcard_devices = bl.sdcard.as_ref().map(|s| s.len()).unwrap_or(0);

    let last_poll_ms_ago = st.last_poll.map(|t| t.elapsed().as_millis() as u64);

    Response::ok(serde_json::json!({
        "armed": st.armed,
        "mode": st.mode.to_string(),
        "uptime_secs": uptime_secs,
        "disarm_remaining_secs": disarm_remaining_secs,
        "usb_devices": usb_devices,
        "thunderbolt_devices": thunderbolt_devices,
        "sdcard_devices": sdcard_devices,
        "usb_watching": cfg.general.watch_usb,
        "thunderbolt_watching": cfg.general.watch_thunderbolt,
        "sdcard_watching": cfg.general.watch_sdcard,
        "power_watching": cfg.general.watch_power,
        "network_watching": cfg.general.watch_network,
        "lid_watching": cfg.general.watch_lid,
        "violations_logged": st.violations_logged,
        "last_poll_ms_ago": last_poll_ms_ago,
    }))
}

fn handle_disarm(state: &Arc<Mutex<DaemonState>>, timeout_secs: u64) -> Response {
    if timeout_secs == 0 {
        return Response::err("timeout_secs must be > 0");
    }

    const MAX_DISARM_SECS: u64 = 3600; // 1 hour max
    if timeout_secs > MAX_DISARM_SECS {
        return Response::err(format!(
            "timeout_secs must be <= {MAX_DISARM_SECS} (1 hour)"
        ));
    }

    let mut st = state.lock().unwrap();
    st.armed = false;
    st.disarm_until = Some(Instant::now() + Duration::from_secs(timeout_secs));
    info!("daemon disarmed for {timeout_secs}s via socket command");

    Response::ok(serde_json::json!({
        "message": format!("disarmed for {timeout_secs} seconds"),
        "disarm_until_secs": timeout_secs,
    }))
}

fn handle_arm(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut st = state.lock().unwrap();
    st.armed = true;
    st.disarm_until = None;
    // Note: baselines are re-captured in the main loop when it detects re-arm
    info!("daemon armed via socket command (baselines will be re-captured)");

    Response::ok(serde_json::json!({
        "message": "armed (baselines will be re-captured on next poll)",
    }))
}

fn handle_learn(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut st = state.lock().unwrap();
    st.mode = DaemonMode::Learn;
    info!("switched to learning mode via socket command");

    Response::ok(serde_json::json!({
        "message": "switched to learning mode",
    }))
}

fn handle_enforce(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut st = state.lock().unwrap();
    st.mode = DaemonMode::Enforce;
    info!("switched to enforce mode via socket command");

    Response::ok(serde_json::json!({
        "message": "switched to enforce mode",
    }))
}

fn handle_reload(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut st = state.lock().unwrap();
    st.reload_pending = true;
    info!("configuration reload scheduled via socket command");

    Response::ok(serde_json::json!({
        "message": "reload scheduled",
    }))
}

// --- Client functions ---

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

/// Remove the socket file (for clean shutdown).
pub fn cleanup_socket(socket_path: &Path) {
    if socket_path.exists()
        && let Err(e) = std::fs::remove_file(socket_path)
    {
        warn!("failed to remove socket {}: {e}", socket_path.display());
    }
}
