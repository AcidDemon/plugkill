use crate::allowances::DeviceRef;
use crate::sdcard::{self, SdCardDeviceInfo};
use crate::thunderbolt::{self, ThunderboltDeviceInfo};
use crate::usb::{self, UsbDeviceInfo};
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

/// The commands the daemon can put behind polkit when `require_auth` is set.
/// Here with the protocol so the daemon and the clients cannot drift apart.
pub const GATED_COMMANDS: [&str; 5] = ["disarm", "learn", "reload", "pair", "allow_last"];

/// What the daemon answers when a gated command was not authorized. Clients
/// match on it to say "authentication required" rather than "failed".
pub const AUTH_REQUIRED_ERROR: &str = "authentication required";

/// How long a client waits for an answer. A gated command does not return
/// until the person has answered the polkit prompt, or it has timed out.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const AUTH_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Read off the request itself, not passed in: no caller can pick the wrong
/// one, and a command added to `GATED_COMMANDS` gets the long wait for free.
fn read_timeout(request: &serde_json::Value) -> Duration {
    // Closing a pairing window is the one "pair" the daemon never gates: it
    // takes a permission away. Waiting two minutes on it would hold the
    // client's command thread for nothing.
    match request.get("command").and_then(|v| v.as_str()) {
        Some("pair") if request.get("window_secs").and_then(|v| v.as_u64()) == Some(0) => {
            READ_TIMEOUT
        }
        Some(command) if GATED_COMMANDS.contains(&command) => AUTH_READ_TIMEOUT,
        _ => READ_TIMEOUT,
    }
}

/// JSON request from a client.
#[derive(Debug, Deserialize)]
#[serde(tag = "command")]
pub enum Request {
    #[serde(rename = "status")]
    Status,
    /// What each bus currently sees, one short line per device.
    #[serde(rename = "devices")]
    Devices,
    /// The violations the daemon remembers, newest first. Spec E2, H3.
    #[serde(rename = "violations")]
    Violations,
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
    /// Open a pairing window: the next new device on a watched bus is recorded
    /// as an allowance instead of firing the kill sequence. `window_secs` 0
    /// closes an open window.
    #[serde(rename = "pair")]
    Pair {
        window_secs: u64,
        /// Expiry for what the window admits, from `--for`. None never expires.
        #[serde(default)]
        for_secs: Option<u64>,
    },
    /// Allow the device behind the most recent recorded violation.
    #[serde(rename = "allow_last")]
    AllowLast {
        #[serde(default)]
        for_secs: Option<u64>,
    },
    /// Drop one allowance, named by its selector.
    #[serde(rename = "revoke")]
    Revoke { selector: String },
    /// Drop every allowance.
    #[serde(rename = "revoke_all")]
    RevokeAll,
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
        .set_read_timeout(Some(read_timeout(request)))
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
/// The watched buses, in one place: the key the daemon reports each bus under
/// in a status response, and the name a person sees. The CLI status output, the
/// daemon's startup line and the tray menu all read this list, so a bus cannot
/// end up named three ways, and a bus added here cannot be silently missed by
/// one of them. The order is the order everything prints in.
pub const BUSES: [(&str, &str); 8] = [
    ("usb_watching", "USB"),
    ("thunderbolt_watching", "Thunderbolt"),
    ("sdcard_watching", "SD card"),
    ("power_watching", "Power supply"),
    ("network_watching", "Network"),
    ("lid_watching", "Lid"),
    ("pci_watching", "PCI"),
    ("display_watching", "Display"),
];

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

/// A status response's `data` as the lines `--status` prints. Kept apart from
/// the printing so the wording is testable.
fn format_status(data: &serde_json::Value) -> String {
    let u64_of = |key: &str| data.get(key).and_then(|v| v.as_u64());
    let mut out = Vec::new();

    let armed = data.get("armed").and_then(|v| v.as_bool()).unwrap_or(false);
    let mode = data
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let status_str = if armed { "armed" } else { "disarmed" };
    out.push(format!("Status:     {status_str} ({mode} mode)"));

    if let Some(secs) = u64_of("uptime_secs") {
        out.push(format!("Uptime:     {}", format_duration(secs)));
    }
    if let Some(secs) = u64_of("disarm_remaining_secs")
        && secs > 0
    {
        out.push(format!("Re-arms in: {}", format_duration(secs)));
    }
    if data.get("dry_run").and_then(|v| v.as_bool()) == Some(true) {
        out.push("Dry run:    yes, violations are logged but nothing is killed".to_string());
    }
    if let Some(pending) = data.get("pending_violation").filter(|v| v.is_object()) {
        let bus = pending
            .get("bus")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let reason = pending.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        let secs = pending
            .get("secs_left")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        out.push(format!(
            "Pending:    {bus} violation in {}: {reason}",
            format_duration(secs)
        ));
    }

    let usb = u64_of("usb_devices").unwrap_or(0);
    let tb = u64_of("thunderbolt_devices").unwrap_or(0);
    let sd = u64_of("sdcard_devices").unwrap_or(0);
    let pci = u64_of("pci_devices").unwrap_or(0);
    out.push(format!(
        "Devices:    {usb} USB IDs, {tb} Thunderbolt, {sd} SD card, {pci} PCI"
    ));

    let watching: Vec<&str> = BUSES
        .iter()
        .filter(|(key, _)| data.get(*key).and_then(|v| v.as_bool()) == Some(true))
        .map(|(_, label)| *label)
        .collect();
    if !watching.is_empty() {
        out.push(format!("Watching:   {}", watching.join(", ")));
    }
    if let Some(secs) = u64_of("pairing_window_secs_left") {
        out.push(format!(
            "Pairing:    window closes in {}",
            format_duration(secs)
        ));
    }
    let allowed = allowance_list(data);
    if !allowed.is_empty() {
        let selectors: Vec<&str> = allowed.iter().map(|a| text(a, "selector")).collect();
        out.push(format!("Allowances: {}", selectors.join(", ")));
    }

    let mut live = Vec::new();
    match data.get("power_state").and_then(|v| v.as_str()) {
        Some("ac") => live.push("on AC".to_string()),
        Some("battery") => live.push("on battery".to_string()),
        Some(other) => live.push(format!("power {other}")),
        None => {}
    }
    if let Some(lid) = data.get("lid_state").and_then(|v| v.as_str()) {
        live.push(format!("lid {lid}"));
    }
    match u64_of("network_links_down") {
        Some(0) => live.push("all links up".to_string()),
        Some(n) => live.push(format!("{n} link(s) down")),
        None => {}
    }
    if !live.is_empty() {
        out.push(format!("Live:       {}", live.join(", ")));
    }

    out.push(format!(
        "Violations: {}",
        u64_of("violations_logged").unwrap_or(0)
    ));
    if let Some(ms) = u64_of("last_poll_ms_ago") {
        out.push(format!("Last poll:  {ms}ms ago"));
    }
    out.join("\n")
}

/// A string field of one allowance entry, or "" when it is absent or null.
fn text<'a>(entry: &'a serde_json::Value, key: &str) -> &'a str {
    entry.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// The `allowances` array of a status response's data, empty when absent.
fn allowance_list(data: &serde_json::Value) -> &[serde_json::Value] {
    data.get("allowances")
        .and_then(|v| v.as_array())
        .map_or(&[], |v| v.as_slice())
}

/// What `--allowances` prints. Spec D5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowanceOutput {
    /// A human table.
    Table,
    /// The raw array from the status response.
    Json,
    /// Config that makes the current allowances permanent.
    Toml,
    /// The same, as NixOS module settings.
    Nix,
}

/// `--for` takes `30s`, `10m`, `2h`, or a bare number of seconds.
pub fn parse_duration_secs(input: &str) -> Result<u64, String> {
    let s = input.trim();
    let bad = || format!("'{input}' is not a duration like 30s, 10m or 2h");
    let (digits, unit) = match s.as_bytes().last() {
        Some(b's') => (&s[..s.len() - 1], 1),
        Some(b'm') => (&s[..s.len() - 1], 60),
        Some(b'h') => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    let n: u64 = digits.trim().parse().map_err(|_| bad())?;
    n.checked_mul(unit).ok_or_else(bad)
}

/// Ask the daemon for its status and print the allowances out of it. Listing
/// needs no command of its own (D2).
pub fn print_allowances(socket_path: &Path, output: AllowanceOutput) -> Result<(), String> {
    let resp = send_request(socket_path, &serde_json::json!({"command": "status"}))?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error")
            .to_string());
    }
    let data = resp.get("data").cloned().unwrap_or(serde_json::Value::Null);
    print!("{}", render_allowances(allowance_list(&data), output));
    Ok(())
}

/// The allowance array in the shape the caller asked for.
fn render_allowances(list: &[serde_json::Value], output: AllowanceOutput) -> String {
    match output {
        AllowanceOutput::Json => {
            serde_json::to_string_pretty(list).unwrap_or_else(|_| "[]".to_string()) + "\n"
        }
        AllowanceOutput::Table => allowance_table(list),
        AllowanceOutput::Toml => allowance_toml(&by_bus(list)),
        AllowanceOutput::Nix => allowance_nix(&by_bus(list)),
    }
}

fn allowance_table(list: &[serde_json::Value]) -> String {
    if list.is_empty() {
        return "No allowances.\n".to_string();
    }
    let width = list
        .iter()
        .map(|a| text(a, "selector").len())
        .chain([8])
        .max()
        .unwrap_or(8);
    let mut out = format!(
        "{:<width$}  {:<8}  {:<9}  {:<9}  NAME\n",
        "SELECTOR", "GRANTED", "AGE", "EXPIRES"
    );
    for a in list {
        let expires = match a.get("expires_in_secs").and_then(|v| v.as_u64()) {
            Some(secs) => format_duration(secs),
            None => "never".to_string(),
        };
        let age = format_duration(a.get("age_secs").and_then(|v| v.as_u64()).unwrap_or(0));
        out.push_str(&format!(
            "{:<width$}  {:<8}  {:<9}  {:<9}  {}\n",
            text(a, "selector"),
            text(a, "granted_by"),
            age,
            expires,
            text(a, "name"),
        ));
    }
    out
}

/// The allowances split per bus, in the shapes the config emitters take. An
/// entry whose selector does not parse is skipped: it came from a daemon that
/// knows a bus this client does not.
#[derive(Default)]
struct ByBus {
    usb: Vec<UsbDeviceInfo>,
    thunderbolt: Vec<ThunderboltDeviceInfo>,
    sdcard: Vec<SdCardDeviceInfo>,
    pci: Vec<String>,
    display: Vec<String>,
}

fn by_bus(list: &[serde_json::Value]) -> ByBus {
    let mut out = ByBus::default();
    for entry in list {
        let name = text(entry, "name");
        let name = (!name.is_empty()).then(|| name.to_string());
        match text(entry, "selector").parse::<DeviceRef>() {
            Ok(DeviceRef::Usb(id)) => out.usb.push(UsbDeviceInfo {
                vendor_id: id.vendor_id,
                product_id: id.product_id,
                product: name,
                ..Default::default()
            }),
            Ok(DeviceRef::Thunderbolt(id)) => out.thunderbolt.push(ThunderboltDeviceInfo {
                unique_id: id.unique_id,
                device_name: name,
                ..Default::default()
            }),
            Ok(DeviceRef::SdCard(id)) => out.sdcard.push(SdCardDeviceInfo {
                serial: id.serial,
                name,
                ..Default::default()
            }),
            Ok(DeviceRef::Pci(selector)) => out.pci.push(selector),
            Ok(DeviceRef::Display(id)) => out.display.push(id.selector()),
            Err(_) => continue,
        }
    }
    out
}

/// Config that makes the allowances permanent, through the same emitters
/// `--generate-whitelist` uses, so the same device produces the same line
/// (D5). A bus with nothing allowed emits nothing: an empty section pasted
/// into a config would erase the whitelist that is already there.
fn allowance_toml(bus: &ByBus) -> String {
    let mut out = String::new();
    if !bus.usb.is_empty() {
        out.push_str(&usb::generate_whitelist_toml(&bus.usb));
    }
    if !bus.thunderbolt.is_empty() {
        out.push_str(&thunderbolt::generate_thunderbolt_whitelist_toml(
            &bus.thunderbolt,
        ));
    }
    if !bus.sdcard.is_empty() {
        out.push_str(&sdcard::generate_sdcard_whitelist_toml(&bus.sdcard));
    }
    if !bus.pci.is_empty() {
        out.push_str("\n[pci]\nignore = [\n");
        for selector in &bus.pci {
            out.push_str(&format!("    \"{selector}\",\n"));
        }
        out.push_str("]\n");
    }
    out.push_str(&display_note(bus, "# "));
    out
}

/// The same as NixOS module settings, since the config on NixOS is a read-only
/// store path and the TOML cannot be edited in place.
fn allowance_nix(bus: &ByBus) -> String {
    let mut out = String::new();
    let usb: Vec<String> = bus
        .usb
        .iter()
        .map(|d| {
            nix_item(
                &format!(
                    "vendor_id = \"{}\"; product_id = \"{}\"; count = 1;",
                    d.vendor_id, d.product_id
                ),
                d.product.as_deref(),
            )
        })
        .collect();
    let thunderbolt: Vec<String> = bus
        .thunderbolt
        .iter()
        .map(|d| {
            nix_item(
                &format!("unique_id = \"{}\";", d.unique_id),
                d.device_name.as_deref(),
            )
        })
        .collect();
    let sdcard: Vec<String> = bus
        .sdcard
        .iter()
        .map(|d| nix_item(&format!("serial = \"{}\";", d.serial), d.name.as_deref()))
        .collect();
    let pci: Vec<String> = bus.pci.iter().map(|s| format!("\"{s}\"")).collect();

    for (path, items) in [
        ("whitelist.devices", &usb),
        ("thunderbolt_whitelist.devices", &thunderbolt),
        ("sdcard_whitelist.devices", &sdcard),
        ("pci.ignore", &pci),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("services.plugkill.settings.{path} = [\n"));
        for item in items {
            out.push_str(&format!("  {item}\n"));
        }
        out.push_str("];\n");
    }
    out.push_str(&display_note(bus, "# "));
    out
}

fn nix_item(fields: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{{ {fields} }}  # {name}"),
        None => format!("{{ {fields} }}"),
    }
}

/// A display allowance names a monitor, and the config only knows connector
/// names, so there is nothing to paste. Say so rather than drop it silently.
fn display_note(bus: &ByBus, comment: &str) -> String {
    if bus.display.is_empty() {
        return String::new();
    }
    format!(
        "\n{comment}no config form for the allowed monitor(s) {}: the display bus\n\
         {comment}matches monitors at runtime, and [display] ignore takes connector names.\n",
        bus.display.join(", ")
    )
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
    if data.get("armed").and_then(|v| v.as_bool()).is_some() {
        println!("{}", format_status(data));
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
    fn test_only_gated_commands_wait_for_a_password() {
        for command in GATED_COMMANDS {
            let req = serde_json::json!({"command": command, "timeout_secs": 60});
            assert_eq!(read_timeout(&req), AUTH_READ_TIMEOUT, "{command}");
        }
        for command in ["status", "devices", "arm", "enforce", "kill"] {
            let req = serde_json::json!({"command": command});
            assert_eq!(read_timeout(&req), READ_TIMEOUT, "{command}");
        }
        // Nothing recognisable waits the long time either.
        assert_eq!(read_timeout(&serde_json::json!({})), READ_TIMEOUT);
    }

    #[test]
    fn test_format_duration_units() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3725), "1h 2m 5s");
    }

    #[test]
    fn test_format_status_keeps_the_existing_lines() {
        let data = serde_json::json!({
            "armed": true, "mode": "enforce", "uptime_secs": 65,
            "usb_devices": 3, "thunderbolt_devices": 0, "sdcard_devices": 0, "pci_devices": 12,
            "usb_watching": true, "violations_logged": 0, "last_poll_ms_ago": 120
        });

        assert_eq!(
            format_status(&data),
            "Status:     armed (enforce mode)\n\
             Uptime:     1m 5s\n\
             Devices:    3 USB IDs, 0 Thunderbolt, 0 SD card, 12 PCI\n\
             Watching:   USB\n\
             Violations: 0\n\
             Last poll:  120ms ago"
        );
    }

    #[test]
    fn test_format_status_shows_dry_run_pending_and_live_readings() {
        let data = serde_json::json!({
            "armed": true, "mode": "enforce", "dry_run": true,
            "pending_violation": {"bus": "power", "reason": "AC power removed", "secs_left": 23},
            "power_state": "battery", "lid_state": "open", "network_links_down": 0,
            "usb_devices": 0, "thunderbolt_devices": 0, "sdcard_devices": 0, "pci_devices": 0,
            "violations_logged": 0
        });

        let out = format_status(&data);

        assert!(
            out.contains("Dry run:    yes, violations are logged but nothing is killed"),
            "{out}"
        );
        assert!(
            out.contains("Pending:    power violation in 23s: AC power removed"),
            "{out}"
        );
        assert!(
            out.contains("Live:       on battery, lid open, all links up"),
            "{out}"
        );
    }

    /// The four shapes the spec writes out (D1). `for_secs` is optional, so
    /// the `pair` line from the spec parses as printed.
    #[test]
    fn test_allowance_requests_deserialize() {
        let pair: Request = serde_json::from_str(r#"{"command":"pair","window_secs":60}"#).unwrap();
        assert!(matches!(
            pair,
            Request::Pair {
                window_secs: 60,
                for_secs: None
            }
        ));
        let last: Request =
            serde_json::from_str(r#"{"command":"allow_last","for_secs":null}"#).unwrap();
        assert!(matches!(last, Request::AllowLast { for_secs: None }));
        let revoke: Request =
            serde_json::from_str(r#"{"command":"revoke","selector":"usb:1d6b:0002"}"#).unwrap();
        let Request::Revoke { selector } = revoke else {
            panic!("expected Request::Revoke");
        };
        assert_eq!(selector, "usb:1d6b:0002");
        assert!(matches!(
            serde_json::from_str(r#"{"command":"revoke_all"}"#).unwrap(),
            Request::RevokeAll
        ));
    }

    /// Granting waits on a person at a password prompt, revoking does not.
    #[test]
    fn test_granting_waits_for_a_password_and_revoking_does_not() {
        for command in ["pair", "allow_last"] {
            let req = serde_json::json!({"command": command});
            assert_eq!(read_timeout(&req), AUTH_READ_TIMEOUT, "{command}");
        }
        for command in ["revoke", "revoke_all"] {
            let req = serde_json::json!({"command": command});
            assert_eq!(read_timeout(&req), READ_TIMEOUT, "{command}");
        }
        // Closing a window takes a permission away, so the daemon answers it
        // without a prompt and the client must not wait two minutes for one.
        assert_eq!(
            read_timeout(&serde_json::json!({"command": "pair", "window_secs": 0})),
            READ_TIMEOUT
        );
        assert_eq!(
            read_timeout(&serde_json::json!({"command": "pair", "window_secs": 300})),
            AUTH_READ_TIMEOUT
        );
    }

    #[test]
    fn test_parse_duration_secs_takes_suffixes_and_refuses_junk() {
        assert_eq!(parse_duration_secs("90"), Ok(90));
        assert_eq!(parse_duration_secs("30s"), Ok(30));
        assert_eq!(parse_duration_secs("10m"), Ok(600));
        assert_eq!(parse_duration_secs("2h"), Ok(7200));
        assert!(parse_duration_secs("soon").is_err());
        assert!(parse_duration_secs("-5").is_err());
        assert!(parse_duration_secs("").is_err());
    }

    fn allowance(selector: &str, name: &str, expires_in_secs: Option<u64>) -> serde_json::Value {
        serde_json::json!({
            "selector": selector,
            "bus": selector.split(':').next().unwrap(),
            "name": name,
            "granted_by": "paired",
            "age_secs": 312,
            "expires_in_secs": expires_in_secs,
        })
    }

    #[test]
    fn test_allowance_table_says_so_when_there_are_none() {
        assert_eq!(
            render_allowances(&[], AllowanceOutput::Table),
            "No allowances.\n"
        );
    }

    #[test]
    fn test_allowance_table_lists_one_per_row() {
        let list = [
            allowance("usb:1d6b:0002", "Kingston DataTraveler", None),
            allowance("pci:0000:01:00.0", "", Some(90)),
        ];

        let out = render_allowances(&list, AllowanceOutput::Table);
        let lines: Vec<&str> = out.lines().collect();

        assert!(lines[0].starts_with("SELECTOR"), "{out}");
        assert!(
            lines[1].contains("usb:1d6b:0002")
                && lines[1].contains("paired")
                && lines[1].contains("5m 12s")
                && lines[1].contains("never")
                && lines[1].contains("Kingston DataTraveler"),
            "{out}"
        );
        assert!(
            lines[2].contains("pci:0000:01:00.0") && lines[2].contains("1m 30s"),
            "{out}"
        );
        assert_eq!(lines.len(), 3, "{out}");
    }

    /// D5: the TOML is the `--generate-whitelist` emitter's own text, so a
    /// device that was allowed and a device that was connected produce the
    /// same line. A bus with nothing allowed emits no section at all.
    #[test]
    fn test_allowance_toml_is_the_whitelist_emitter_text() {
        let list = [allowance("usb:1d6b:0002", "Kingston DataTraveler", None)];

        let out = render_allowances(&list, AllowanceOutput::Toml);

        let same = crate::usb::generate_whitelist_toml(&[UsbDeviceInfo {
            vendor_id: "1d6b".to_string(),
            product_id: "0002".to_string(),
            product: Some("Kingston DataTraveler".to_string()),
            ..Default::default()
        }]);
        assert_eq!(out, same);
        assert!(!out.contains("thunderbolt_whitelist"), "{out}");
        assert!(!out.contains("[pci]"), "{out}");
    }

    #[test]
    fn test_allowance_toml_carries_every_bus_it_can_express() {
        let list = [
            allowance("thunderbolt:uuid-aaa", "Dock", None),
            allowance("sdcard:0xdeadbeef", "SanDisk", None),
            allowance("pci:0000:01:00.0", "", None),
            allowance("display:SAM:772d:811021873", "U28E590", None),
        ];

        let out = render_allowances(&list, AllowanceOutput::Toml);

        assert!(out.contains("unique_id = \"uuid-aaa\""), "{out}");
        assert!(out.contains("serial = \"0xdeadbeef\""), "{out}");
        assert!(
            out.contains("[pci]\nignore = [\n    \"0000:01:00.0\",\n]"),
            "{out}"
        );
        // A monitor has no config form, so it is named rather than dropped.
        assert!(out.contains("# no config form"), "{out}");
        assert!(out.contains("SAM:772d:811021873"), "{out}");
    }

    #[test]
    fn test_allowance_nix_emits_module_settings() {
        let list = [
            allowance("usb:1d6b:0002", "Kingston DataTraveler", None),
            allowance("pci:0000:01:00.0", "", None),
        ];

        let out = render_allowances(&list, AllowanceOutput::Nix);

        assert!(
            out.contains(
                "services.plugkill.settings.whitelist.devices = [\n  { vendor_id = \"1d6b\"; \
                 product_id = \"0002\"; count = 1; }  # Kingston DataTraveler\n];"
            ),
            "{out}"
        );
        assert!(
            out.contains("services.plugkill.settings.pci.ignore = [\n  \"0000:01:00.0\"\n];"),
            "{out}"
        );
        assert!(!out.contains("sdcard_whitelist"), "{out}");
    }

    #[test]
    fn test_allowance_json_is_the_raw_array() {
        let list = [allowance("usb:1d6b:0002", "Kingston DataTraveler", None)];

        let out = render_allowances(&list, AllowanceOutput::Json);

        let back: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(back, list);
    }

    #[test]
    fn test_format_status_shows_allowances_and_an_open_pairing_window() {
        let data = serde_json::json!({
            "armed": true, "mode": "enforce",
            "allowances": [allowance("usb:1d6b:0002", "Kingston DataTraveler", None)],
            "pairing_window_secs_left": 42,
            "violations_logged": 0
        });

        let out = format_status(&data);

        assert!(out.contains("Pairing:    window closes in 42s"), "{out}");
        assert!(out.contains("Allowances: usb:1d6b:0002"), "{out}");
    }
}
