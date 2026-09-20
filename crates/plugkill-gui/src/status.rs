//! The daemon's status response as the GUI reads it, and the single state the
//! tray and the dashboard both show.

use plugkill_core::ipc;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// A poll loop this far behind is treated as stuck: longer than the largest
/// allowed `sleep_ms` (10 s) plus a slow detection pass.
pub const STALL_MS: u64 = 30_000;

/// A grace period the daemon is waiting out.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pending {
    pub bus: String,
    pub reason: String,
    pub secs_left: u64,
}

/// The `data` object of a `status` response.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Status {
    pub armed: bool,
    pub mode: String,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub disarm_remaining_secs: Option<u64>,
    #[serde(default)]
    pub usb_devices: u64,
    #[serde(default)]
    pub thunderbolt_devices: u64,
    #[serde(default)]
    pub sdcard_devices: u64,
    #[serde(default)]
    pub pci_devices: u64,
    #[serde(default)]
    pub violations_logged: u64,
    #[serde(default)]
    pub last_poll_ms_ago: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub pending_violation: Option<Pending>,
    #[serde(default)]
    pub power_state: Option<String>,
    #[serde(default)]
    pub lid_state: Option<String>,
    #[serde(default)]
    pub network_links_down: Option<u64>,
    /// The file the daemon loaded, so the settings window reads the same one
    /// rather than guessing (C1). Empty on a daemon that predates it.
    #[serde(default)]
    pub config_path: String,
    /// The daemon's version, for the diagnostics panel (G1).
    #[serde(default)]
    pub version: String,
    /// Every other key, including the `*_watching` flags that `buses` reads.
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

impl Status {
    /// Every bus in `ipc::BUSES` order: its status key, its display name, and
    /// whether the daemon watches it.
    pub fn buses(&self) -> Vec<(&'static str, &'static str, bool)> {
        ipc::BUSES
            .iter()
            .map(|&(key, name)| {
                let watched = self.rest.get(key).and_then(|v| v.as_bool()) == Some(true);
                (key, name, watched)
            })
            .collect()
    }
}

/// One entry of a bus listing: the line a client shows, and the topology path
/// the device sits at (K2a). The path is empty on a bus that has no topology,
/// and on a daemon old enough to still send bare lines.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "EntryWire")]
pub struct DeviceEntry {
    pub text: String,
    pub path: String,
}

/// Both shapes the daemon has sent: the bare line, and the object K2a added.
#[derive(Deserialize)]
#[serde(untagged)]
enum EntryWire {
    Text(String),
    Obj {
        #[serde(default)]
        text: String,
        #[serde(default)]
        path: String,
    },
}

impl From<EntryWire> for DeviceEntry {
    fn from(wire: EntryWire) -> Self {
        match wire {
            EntryWire::Text(text) => Self {
                text,
                path: String::new(),
            },
            EntryWire::Obj { text, path } => Self { text, path },
        }
    }
}

/// An entry from its text alone, for the fixtures in this crate's tests: the
/// panels that nest by path build their own entries with a real one.
#[cfg(test)]
impl From<&str> for DeviceEntry {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            path: String::new(),
        }
    }
}

/// What one bus reports in a `devices` response: the same flag the status
/// response gives, and the entries the daemon sees on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BusDevices {
    #[serde(default)]
    pub watched: bool,
    #[serde(default)]
    pub entries: Vec<DeviceEntry>,
    /// How many entries were dropped past the daemon's cap.
    #[serde(default)]
    pub more: u64,
}

/// One row of a `violations` response, newest first as the daemon sends them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Violation {
    /// When it happened, seconds since the epoch.
    #[serde(default)]
    pub at_unix: u64,
    /// The bus it came off: a device bus, or `power`, `network`, `lid`.
    #[serde(default)]
    pub bus: String,
    /// None for an event with nothing to identify: a lid close, a power
    /// unplug. Only a row with one can be whitelisted or allowed.
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub message: String,
}

/// The `violations` array of a `violations` response, newest first.
pub type Violations = Vec<Violation>;

/// The `buses` object of a `devices` response, keyed by short bus name
/// (`usb`, `lid`, ...). A missing key reads as not watched.
pub type Devices = HashMap<String, BusDevices>;

/// What the tray icon, the menu and the dashboard show. Exactly one applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayState {
    /// No answer on the socket, or (`stalled`) the poll loop has stopped.
    Down {
        stalled: bool,
    },
    Disarmed {
        secs_left: u64,
    },
    /// Enforce mode, not a dry run, and a grace period is running: the machine
    /// goes down when it ends.
    KillPending {
        bus: String,
        reason: String,
        secs_left: u64,
    },
    DryRun,
    Learning,
    Armed,
}

impl TrayState {
    /// Precedence follows the design: down, disarmed, stalled, kill pending,
    /// dry run, learning, armed. Disarmed comes before stalled because the
    /// daemon stops updating `last_poll` while disarmed.
    pub fn from_status(status: Option<&Status>) -> TrayState {
        let Some(s) = status else {
            return TrayState::Down { stalled: false };
        };
        if !s.armed {
            return TrayState::Disarmed {
                secs_left: s.disarm_remaining_secs.unwrap_or(0),
            };
        }
        if s.last_poll_ms_ago.is_some_and(|ms| ms > STALL_MS) {
            return TrayState::Down { stalled: true };
        }
        if s.mode == "enforce"
            && !s.dry_run
            && let Some(p) = &s.pending_violation
        {
            return TrayState::KillPending {
                bus: p.bus.clone(),
                reason: p.reason.clone(),
                secs_left: p.secs_left,
            };
        }
        if s.dry_run {
            return TrayState::DryRun;
        }
        if s.mode == "learn" {
            return TrayState::Learning;
        }
        TrayState::Armed
    }

    /// A short title and one line of detail, shared by the menu header, the
    /// tooltip and the dashboard header.
    pub fn title_and_detail(&self, status: Option<&Status>) -> (String, String) {
        match self {
            TrayState::Down { stalled: false } => (
                "Daemon not running".into(),
                "no answer on the control socket".into(),
            ),
            TrayState::Down { stalled: true } => (
                "Daemon not polling".into(),
                "the socket answers but the poll loop has stopped".into(),
            ),
            TrayState::Disarmed { secs_left } => (
                "Disarmed".into(),
                format!("re-arms in {}", ipc::format_duration(*secs_left)),
            ),
            TrayState::KillPending {
                reason, secs_left, ..
            } => (format!("Kill in {secs_left} s"), reason.clone()),
            TrayState::DryRun => (
                "Dry run".into(),
                "violations are logged, nothing is killed".into(),
            ),
            TrayState::Learning => {
                let detail = match status.map_or(0, |s| s.violations_logged) {
                    0 => "not killing anything".to_string(),
                    1 => "1 violation logged".to_string(),
                    n => format!("{n} violations logged"),
                };
                ("Learning".into(), detail)
            }
            TrayState::Armed => ("Armed".into(), "enforce mode".into()),
        }
    }

    /// States a person should look at: nothing is protecting the machine, or
    /// it is about to go down.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            TrayState::Down { .. } | TrayState::Disarmed { .. } | TrayState::KillPending { .. }
        )
    }
}

/// How long the daemon has been up, for the places that only need the scale:
/// seconds while it is younger than a minute, then minutes, hours and days.
/// `format_duration` keeps the seconds where they matter, in the countdowns.
pub fn format_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, m) => format!("{d}d {h}h {m}m"),
    }
}

/// The status inside a response envelope, or None unless `ok` is true and the
/// data parses.
fn parse_response(resp: &serde_json::Value) -> Option<Status> {
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    serde_json::from_value(resp.get("data")?.clone()).ok()
}

/// Ask the daemon for its status. Blocking: never call this on the GTK thread.
pub fn fetch(socket: &Path) -> Option<Status> {
    let resp = ipc::send_request(socket, &serde_json::json!({"command": "status"})).ok()?;
    parse_response(&resp)
}

/// The buses of a `devices` response. A bus whose value does not parse is
/// dropped rather than failing the whole response.
fn parse_devices(resp: &serde_json::Value) -> Option<Devices> {
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let buses = resp.get("data")?.get("buses")?.as_object()?;
    Some(
        buses
            .iter()
            .filter_map(|(key, v)| Some((key.clone(), serde_json::from_value(v.clone()).ok()?)))
            .collect(),
    )
}

/// Ask the daemon what each bus currently sees. The daemon walks sysfs for
/// this, so it is only worth asking while the dashboard is open. Blocking:
/// never call this on the GTK thread.
pub fn fetch_devices(socket: &Path) -> Option<Devices> {
    let resp = ipc::send_request(socket, &serde_json::json!({"command": "devices"})).ok()?;
    parse_devices(&resp)
}

/// The rows of a `violations` response. A row that does not parse is dropped
/// rather than failing the whole response.
fn parse_violations(resp: &serde_json::Value) -> Option<Violations> {
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let rows = resp.get("data")?.get("violations")?.as_array()?;
    Some(
        rows.iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect(),
    )
}

/// Ask the daemon for its violation history. A copy out of memory, so it costs
/// the daemon nothing, but it is only worth asking while the settings window
/// is open. Blocking: never call this on the GTK thread.
pub fn fetch_violations(socket: &Path) -> Option<Violations> {
    let resp = ipc::send_request(socket, &serde_json::json!({"command": "violations"})).ok()?;
    parse_violations(&resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uptime_drops_to_the_scale_that_matters() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(42), "42s");
        assert_eq!(format_uptime(60), "1m");
        assert_eq!(format_uptime(3_599), "59m");
        assert_eq!(format_uptime(3_600), "1h 0m");
        assert_eq!(format_uptime(86_399), "23h 59m");
        assert_eq!(
            format_uptime(2 * 86_400 + 4 * 3_600 + 17 * 60 + 5),
            "2d 4h 17m"
        );
    }

    fn status(extra: serde_json::Value) -> Status {
        let mut data = serde_json::json!({"armed": true, "mode": "enforce"});
        data.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        parse_response(&serde_json::json!({"ok": true, "data": data})).expect("status parses")
    }

    #[test]
    fn test_parse_response_reads_old_and_new_fields() {
        let s = status(serde_json::json!({
            "uptime_secs": 7, "usb_devices": 4, "usb_watching": true, "lid_watching": false,
            "dry_run": true, "power_state": "ac",
            "pending_violation": {"bus": "lid", "reason": "lid closed", "secs_left": 9}
        }));
        assert_eq!(s.uptime_secs, 7);
        assert_eq!(s.usb_devices, 4);
        assert!(s.dry_run);
        assert_eq!(s.power_state.as_deref(), Some("ac"));
        assert_eq!(s.pending_violation.unwrap().secs_left, 9);
    }

    #[test]
    fn test_parse_response_tolerates_an_older_daemon() {
        let s = status(serde_json::json!({}));
        assert!(!s.dry_run);
        assert!(s.pending_violation.is_none());
        assert!(s.power_state.is_none());
    }

    #[test]
    fn test_parse_response_rejects_a_refusal() {
        let resp = serde_json::json!({"ok": false, "error": "nope"});
        assert!(parse_response(&resp).is_none());
    }

    #[test]
    fn test_buses_follow_ipc_order_and_flags() {
        let s = status(serde_json::json!({"usb_watching": true, "pci_watching": true}));
        let buses = s.buses();
        assert_eq!(buses.len(), ipc::BUSES.len());
        assert_eq!(buses[0], ("usb_watching", "USB", true));
        assert_eq!(buses[1], ("thunderbolt_watching", "Thunderbolt", false));
        assert!(buses.contains(&("pci_watching", "PCI", true)));
    }

    #[test]
    fn test_parse_devices_drops_only_the_key_it_cannot_read() {
        let resp = serde_json::json!({"ok": true, "data": {"buses": {
            "usb": {"watched": true, "entries": [
                {"text": "1050:0407 YubiKey", "path": "2-3.4"},
                {"text": "closed"},
            ], "more": 0},
            // An older daemon sends bare lines, and they still parse.
            "sdcard": {"watched": true, "entries": ["0x03:0x5344 SD"], "more": 0},
            "lid": {"watched": true},
            "pci": "not an object",
        }}});
        let d = parse_devices(&resp).expect("devices parse");
        assert_eq!(
            d["usb"].entries,
            vec![
                DeviceEntry {
                    text: "1050:0407 YubiKey".to_string(),
                    path: "2-3.4".to_string(),
                },
                DeviceEntry {
                    text: "closed".to_string(),
                    path: String::new(),
                },
            ]
        );
        assert_eq!(
            d["sdcard"].entries,
            vec![DeviceEntry::from("0x03:0x5344 SD")]
        );
        assert_eq!(
            d["lid"],
            BusDevices {
                watched: true,
                ..BusDevices::default()
            },
            "missing fields default"
        );
        assert!(!d.contains_key("pci"), "an unreadable bus is dropped");
    }

    #[test]
    fn test_parse_devices_rejects_a_refusal_or_an_older_daemon() {
        assert!(parse_devices(&serde_json::json!({"ok": false, "error": "nope"})).is_none());
        assert!(parse_devices(&serde_json::json!({"ok": true, "data": {}})).is_none());
    }

    #[test]
    fn test_parse_violations_keeps_the_daemon_order_and_drops_a_bad_row() {
        let resp = serde_json::json!({"ok": true, "data": {"violations": [
            {"at_unix": 2, "bus": "usb", "selector": "1050:0407", "name": "YubiKey",
             "message": "new usb device"},
            "not a row",
            {"at_unix": 1, "bus": "lid", "selector": null, "name": null, "message": "lid closed"},
        ]}});
        let v = parse_violations(&resp).expect("violations parse");
        assert_eq!(v.len(), 2, "an unreadable row is dropped");
        assert_eq!(v[0].at_unix, 2, "newest first, as the daemon sends them");
        assert_eq!(v[0].selector.as_deref(), Some("1050:0407"));
        assert!(v[1].selector.is_none(), "an event names no device");
        assert_eq!(v[1].message, "lid closed");
    }

    #[test]
    fn test_parse_violations_rejects_a_refusal_or_an_older_daemon() {
        assert!(parse_violations(&serde_json::json!({"ok": false, "error": "nope"})).is_none());
        assert!(parse_violations(&serde_json::json!({"ok": true, "data": {}})).is_none());
    }

    #[test]
    fn test_status_carries_the_loaded_config_path_and_version() {
        let s =
            status(serde_json::json!({"config_path": "/etc/plugkill.toml", "version": "0.1.0"}));
        assert_eq!(s.config_path, "/etc/plugkill.toml");
        assert_eq!(s.version, "0.1.0");
        // An older daemon sends neither, and the window says so rather than
        // guessing a path.
        assert!(status(serde_json::json!({})).config_path.is_empty());
    }

    #[test]
    fn test_from_status_without_an_answer_is_down() {
        assert_eq!(
            TrayState::from_status(None),
            TrayState::Down { stalled: false }
        );
    }

    #[test]
    fn test_from_status_disarmed_wins_over_a_stale_poll() {
        let s = status(serde_json::json!({
            "armed": false, "disarm_remaining_secs": 252, "last_poll_ms_ago": 90000
        }));
        assert_eq!(
            TrayState::from_status(Some(&s)),
            TrayState::Disarmed { secs_left: 252 }
        );
    }

    #[test]
    fn test_from_status_stale_poll_is_down_stalled() {
        let s = status(serde_json::json!({"last_poll_ms_ago": 45000}));
        assert_eq!(
            TrayState::from_status(Some(&s)),
            TrayState::Down { stalled: true }
        );
        // A dead poll thread leaves the grace record standing, so both arrive
        // together: the frozen countdown must not win over "not polling".
        let s = status(serde_json::json!({
            "last_poll_ms_ago": 90000,
            "pending_violation": {"bus": "power", "reason": "AC power removed", "secs_left": 7}
        }));
        assert_eq!(
            TrayState::from_status(Some(&s)),
            TrayState::Down { stalled: true }
        );
    }

    #[test]
    fn test_from_status_enforce_with_a_grace_is_kill_pending() {
        let s = status(serde_json::json!({
            "pending_violation": {"bus": "power", "reason": "AC power removed", "secs_left": 23}
        }));
        assert_eq!(
            TrayState::from_status(Some(&s)),
            TrayState::KillPending {
                bus: "power".into(),
                reason: "AC power removed".into(),
                secs_left: 23
            }
        );
    }

    #[test]
    fn test_from_status_a_grace_in_learn_mode_or_dry_run_is_not_a_kill() {
        let pending = serde_json::json!({"bus": "lid", "reason": "lid closed", "secs_left": 4});
        let learn = status(serde_json::json!({"mode": "learn", "pending_violation": pending}));
        let dry = status(serde_json::json!({"dry_run": true, "pending_violation": pending}));
        assert_eq!(TrayState::from_status(Some(&learn)), TrayState::Learning);
        assert_eq!(TrayState::from_status(Some(&dry)), TrayState::DryRun);
    }

    #[test]
    fn test_from_status_plain_enforce_is_armed() {
        let s = status(serde_json::json!({"last_poll_ms_ago": 180}));
        assert_eq!(TrayState::from_status(Some(&s)), TrayState::Armed);
    }

    #[test]
    fn test_title_and_detail_wording() {
        let learning = status(serde_json::json!({"mode": "learn", "violations_logged": 3}));
        assert_eq!(
            TrayState::Learning.title_and_detail(Some(&learning)),
            ("Learning".to_string(), "3 violations logged".to_string())
        );
        assert_eq!(
            TrayState::Disarmed { secs_left: 252 }.title_and_detail(None),
            ("Disarmed".to_string(), "re-arms in 4m 12s".to_string())
        );
        let pending = TrayState::KillPending {
            bus: "power".into(),
            reason: "AC power removed".into(),
            secs_left: 23,
        };
        assert_eq!(
            pending.title_and_detail(None),
            ("Kill in 23 s".to_string(), "AC power removed".to_string())
        );
    }

    #[test]
    fn test_needs_attention_only_when_unprotected_or_about_to_kill() {
        assert!(TrayState::Down { stalled: false }.needs_attention());
        assert!(TrayState::Down { stalled: true }.needs_attention());
        assert!(TrayState::Disarmed { secs_left: 1 }.needs_attention());
        assert!(
            TrayState::KillPending {
                bus: "power".into(),
                reason: "AC power removed".into(),
                secs_left: 5
            }
            .needs_attention()
        );
        assert!(!TrayState::Armed.needs_attention());
        assert!(!TrayState::Learning.needs_attention());
        assert!(!TrayState::DryRun.needs_attention());
    }
}
