//! A stand-in control socket for working on plugkill-gui without a real
//! daemon. It answers `status`, `devices` and `violations` for the state you
//! pick and applies arm, disarm, learn, enforce, reload, pair, allow_last,
//! revoke and revoke_all to that state, so the tray, the dashboard and the
//! settings window behave as they would against the daemon.
//!
//! It writes one small config into the temp directory and reports it as the
//! file it loaded, so the editor panel opens on something real. That is the
//! fake daemon writing its own config, not the window: the window never
//! writes anything.
//!
//! cargo run -p plugkill-gui --example fake_daemon -- "$XDG_RUNTIME_DIR/plugkill-fake.sock" pending
//! cargo run -p plugkill-gui -- "$XDG_RUNTIME_DIR/plugkill-fake.sock"
//!
//! States: armed, learning, dry-run, disarmed, pending, stalled.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a fake grace period runs before it starts over.
const GRACE: Duration = Duration::from_secs(23);

/// Uptime the fake reports on top of its own run time, so the footer shows days.
const UPTIME_BASE: u64 = 2 * 86_400 + 4 * 3_600 + 17 * 60;

/// The config the fake writes and then reports as the one it loaded, so the
/// editor opens on a file with something in every section it can show.
const CONFIG: &str = "\
[general]
sleep_ms = 750
require_auth = true

[whitelist]
devices = [{ vendor_id = \"1050\", product_id = \"0407\", count = 1 }]

[pci]
ignore = [\"0000:00:02\"]

[commands]
kill_commands = [[\"/run/current-system/sw/bin/systemctl\", \"poweroff\"]]

[destruction]
files_to_remove = [\"/home/you/.ssh/id_ed25519\"]
";

/// One runtime allowance, as `status` reports it.
struct Allowance {
    selector: String,
    bus: String,
    name: Option<String>,
    granted_by: &'static str,
    at: Instant,
    expires: Option<Instant>,
}

struct Fake {
    config_path: String,
    allowances: Vec<Allowance>,
    pairing_until: Option<Instant>,
    armed: bool,
    learn: bool,
    dry_run: bool,
    stalled: bool,
    disarm_until: Option<Instant>,
    pending_until: Option<Instant>,
    started: Instant,
}

impl Fake {
    fn new(state: &str, config_path: String) -> Result<Self, String> {
        let now = Instant::now();
        let mut fake = Fake {
            config_path,
            allowances: vec![Allowance {
                selector: "usb:1d6b:0002".into(),
                bus: "usb".into(),
                name: Some("Linux Foundation 2.0 root hub".into()),
                granted_by: "paired",
                at: now,
                expires: Some(now + Duration::from_secs(252)),
            }],
            pairing_until: None,
            armed: true,
            learn: false,
            dry_run: false,
            stalled: false,
            disarm_until: None,
            pending_until: None,
            started: now,
        };
        match state {
            "armed" => {}
            "learning" => fake.learn = true,
            "dry-run" => fake.dry_run = true,
            "disarmed" => {
                fake.armed = false;
                fake.disarm_until = Some(now + Duration::from_secs(252));
            }
            "pending" => fake.pending_until = Some(now + GRACE),
            "stalled" => fake.stalled = true,
            other => {
                return Err(format!(
                    "unknown state {other}: pick armed, learning, dry-run, disarmed, pending or stalled"
                ));
            }
        }
        Ok(fake)
    }

    fn status(&mut self) -> Value {
        let now = Instant::now();
        if let Some(until) = self.disarm_until
            && until <= now
        {
            self.armed = true;
            self.disarm_until = None;
        }
        // A grace that runs out starts over, so the countdown can be watched.
        if let Some(until) = self.pending_until
            && until <= now
        {
            self.pending_until = Some(now + GRACE);
        }
        let secs_left = |t: Option<Instant>| {
            t.map(|t| t.saturating_duration_since(now).as_millis().div_ceil(1000) as u64)
        };
        let pending = if self.armed {
            secs_left(self.pending_until)
                .map(|s| json!({"bus": "power", "reason": "AC power removed", "secs_left": s}))
        } else {
            None
        };
        let allowances: Vec<Value> = self
            .allowances
            .iter()
            .map(|a| {
                json!({
                    "selector": a.selector,
                    "bus": a.bus,
                    "name": a.name,
                    "granted_by": a.granted_by,
                    "age_secs": a.at.elapsed().as_secs(),
                    "expires_in_secs": secs_left(a.expires),
                })
            })
            .collect();
        json!({
            "armed": self.armed,
            "config_path": self.config_path,
            "version": env!("CARGO_PKG_VERSION"),
            "allowances": allowances,
            "pairing_window_secs_left": secs_left(self.pairing_until),
            "mode": if self.learn { "learn" } else { "enforce" },
            "uptime_secs": UPTIME_BASE + self.started.elapsed().as_secs(),
            "disarm_remaining_secs": secs_left(self.disarm_until),
            "usb_devices": 7,
            "thunderbolt_devices": 1,
            "sdcard_devices": 0,
            "pci_devices": 23,
            "usb_watching": true,
            "thunderbolt_watching": true,
            "sdcard_watching": true,
            "power_watching": true,
            "network_watching": false,
            "lid_watching": true,
            "pci_watching": true,
            "display_watching": false,
            "violations_logged": if self.learn { 3 } else { 0 },
            "last_poll_ms_ago": if self.stalled { 45_000 } else { 180 },
            "dry_run": self.dry_run,
            "pending_violation": pending,
            "power_state": if pending.is_some() { "battery" } else { "ac" },
            "lid_state": "open",
            "network_links_down": null,
        })
    }

    /// What the buses see, matching `status`: the same watched flags and the
    /// same counts, with PCI over the daemon's cap of 12 so "and N more" shows.
    fn devices(&self) -> Value {
        let on_battery = self.armed && self.pending_until.is_some();
        // A real sysfs topology, so the window's nesting and the hub
        // expansion can be seen against the fake as well as the daemon.
        let usb = [
            ("1d6b:0002", "usb2"),
            ("04f2:b6dd Chicony Integrated Camera", "2-1"),
            ("8087:0026 Intel Bluetooth", "2-2"),
            ("05e3:0610 Genesys Logic USB2.0 Hub", "2-3"),
            ("1050:0407 Yubico YubiKey OTP+FIDO+CCID", "2-3.1"),
            ("046d:c52b Logitech USB Receiver", "2-3.4"),
            // The same receiver twice, on two ports of the hub. One id, two
            // devices, which is what a whitelist entry has to cover.
            ("046d:c52b Logitech USB Receiver", "2-3.5"),
            ("0781:5583 SanDisk Ultra Fit", "2-3.6"),
            ("0bda:8153 Realtek USB 10/100/1000 LAN", "2-4"),
        ];
        let pci: Vec<Value> = (0..23)
            .map(|i| {
                let addr = format!("0000:00:{:02x}.{}", i / 3, i % 3);
                dev(&addr, &format!("pci0000:00/{addr}"))
            })
            .take(12)
            .collect();
        fn bus(watched: bool, entries: Vec<Value>, more: u64) -> Value {
            json!({"watched": watched, "entries": entries, "more": more})
        }
        /// An entry with a topology path, the shape K2a added.
        fn dev(text: &str, path: &str) -> Value {
            json!({"text": text, "path": path})
        }
        /// An entry on a bus with nothing to nest.
        fn flat(text: &str) -> Value {
            json!({"text": text})
        }
        fn one(entry: &str) -> Value {
            bus(true, vec![flat(entry)], 0)
        }
        json!({
            "usb": bus(true, usb.iter().map(|(t, p)| dev(t, p)).collect(), 0),
            "thunderbolt": bus(true, vec![dev("8086:1234 Dell WD19TB Dock", "0-1")], 0),
            "sdcard": bus(true, vec![], 0),
            "power": one(if on_battery { "on battery" } else { "on AC" }),
            "network": bus(false, vec![], 0),
            "lid": one("open"),
            "pci": bus(true, pci, 11),
            "display": bus(false, vec![], 0),
        })
    }

    /// A seeded history, newest first, in the shape the panel parses. The
    /// stamps move with the clock so "ago" reads sensibly.
    fn violations(&self) -> Vec<Value> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        vec![
            json!({"at_unix": now - 40, "bus": "usb", "selector": "usb:0781:5583",
                   "name": "SanDisk Ultra Fit", "message": "new usb device 0781:5583"}),
            json!({"at_unix": now - 900, "bus": "lid", "selector": null, "name": null,
                   "message": "lid closed"}),
            json!({"at_unix": now - 4_000, "bus": "pci", "selector": "pci:0000:03:00.0",
                   "name": null, "message": "new pci device 0000:03:00.0"}),
        ]
    }

    fn apply(&mut self, request: &Value) -> Value {
        match request.get("command").and_then(Value::as_str) {
            Some("status") => return json!({"ok": true, "data": self.status()}),
            Some("devices") => return json!({"ok": true, "data": {"buses": self.devices()}}),
            Some("arm") => {
                self.armed = true;
                self.disarm_until = None;
            }
            Some("disarm") => {
                let secs = request
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if !(1..=3600).contains(&secs) {
                    return json!({"ok": false, "error": "timeout_secs must be between 1 and 3600"});
                }
                self.armed = false;
                self.pending_until = None;
                self.disarm_until = Some(Instant::now() + Duration::from_secs(secs));
            }
            Some("violations") => {
                return json!({"ok": true, "data": {"violations": self.violations()}});
            }
            Some("learn") => self.learn = true,
            Some("enforce") => self.learn = false,
            Some("reload") => {}
            Some("pair") => {
                let secs = request
                    .get("window_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.pairing_until = (secs > 0).then(|| Instant::now() + Duration::from_secs(secs));
            }
            Some("allow_last") => {
                // The newest violation that names a device, the way the daemon
                // resolves it at request time.
                let Some(v) = self
                    .violations()
                    .into_iter()
                    .find(|v| !v["selector"].is_null())
                else {
                    return json!({"ok": false, "error": "no violation to allow"});
                };
                let selector = v["selector"].as_str().unwrap_or_default().to_string();
                self.allowances.retain(|a| a.selector != selector);
                self.allowances.push(Allowance {
                    bus: v["bus"].as_str().unwrap_or_default().to_string(),
                    name: v["name"].as_str().map(str::to_string),
                    selector,
                    granted_by: "promoted",
                    at: Instant::now(),
                    expires: request
                        .get("for_secs")
                        .and_then(Value::as_u64)
                        .map(|s| Instant::now() + Duration::from_secs(s)),
                });
            }
            Some("revoke") => {
                let selector = request
                    .get("selector")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let before = self.allowances.len();
                self.allowances.retain(|a| a.selector != selector);
                if self.allowances.len() == before {
                    return json!({"ok": false, "error": format!("no allowance for {selector}")});
                }
            }
            Some("revoke_all") => self.allowances.clear(),
            other => return json!({"ok": false, "error": format!("unsupported command {other:?}")}),
        }
        json!({"ok": true, "data": {"message": "ok"}})
    }
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: fake_daemon <socket path> [state]")?;
    let parent = Path::new(&path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = std::fs::canonicalize(parent)
        .map_err(|e| format!("cannot resolve {}: {e}", parent.display()))?;
    if dir.starts_with("/run/plugkill") || dir.starts_with("/var/run/plugkill") {
        return Err("refusing to bind in the real daemon's socket directory".into());
    }
    let state = args.next().unwrap_or_else(|| "armed".into());
    // The fake daemon's own config, so the editor panel has a file to open.
    let config_path = std::env::temp_dir().join("plugkill-fake.toml");
    std::fs::write(&config_path, CONFIG)
        .map_err(|e| format!("cannot write {}: {e}", config_path.display()))?;
    let fake = Arc::new(Mutex::new(Fake::new(
        &state,
        config_path.display().to_string(),
    )?));

    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| format!("cannot bind {path}: {e}"))?;
    eprintln!("fake plugkill daemon ({state}) on {path}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let fake = fake.clone();
        std::thread::spawn(move || {
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                return;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(request) => fake.lock().unwrap().apply(&request),
                Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
            };
            let mut writer = &stream;
            let _ = writeln!(writer, "{reply}");
        });
    }
    Ok(())
}
