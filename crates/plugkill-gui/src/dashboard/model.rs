//! What the dashboard shows, computed from the state without touching GTK.

use crate::commands::DISARM_PRESETS;
use crate::icons;
use crate::status::format_uptime;
use crate::status::{Devices, Status, TrayState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Armed, learning or dry run: pick a disarm duration.
    Presets,
    /// Disarmed: a ring that empties towards re-arming.
    Countdown { secs_left: u64, total_secs: u64 },
    /// Enforce mode with a grace period running.
    KillPending {
        secs_left: u64,
        reason: String,
        hint: String,
    },
    /// No daemon to control.
    Down { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub icon: &'static str,
    pub name: &'static str,
    pub value: String,
    pub watched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub icon: &'static str,
    pub title: String,
    pub detail: String,
    /// Header pill text and the CSS class that colours it.
    pub pill: (&'static str, &'static str),
    /// The mode switch position, or None to hide it.
    pub mode_learn: Option<bool>,
    pub body: Body,
    pub watching: String,
    pub tiles: Vec<Tile>,
    pub footer: String,
}

pub fn build(state: &TrayState, status: Option<&Status>) -> Model {
    let (title, detail) = state.title_and_detail(status);
    let pill = match state {
        TrayState::Down { .. } => ("offline", "pill-down"),
        TrayState::Disarmed { .. } => ("disarmed", "pill-disarmed"),
        TrayState::KillPending { .. } => ("kill pending", "pill-pending"),
        TrayState::DryRun => ("dry run", "pill-learning"),
        TrayState::Learning => ("learn", "pill-learning"),
        TrayState::Armed => ("enforce", "pill-armed"),
    };
    let body = match state {
        TrayState::Down { stalled: true } => Body::Down {
            message:
                "The daemon answers, but its poll loop has stopped, so nothing is being checked."
                    .into(),
        },
        TrayState::Down { stalled: false } => Body::Down {
            message: "Nothing is being watched. The tray keeps retrying every second.".into(),
        },
        TrayState::Disarmed { secs_left } => Body::Countdown {
            secs_left: *secs_left,
            total_secs: countdown_total(*secs_left),
        },
        TrayState::KillPending {
            bus,
            reason,
            secs_left,
        } => Body::KillPending {
            secs_left: *secs_left,
            reason: reason.clone(),
            hint: cancel_hint(bus).into(),
        },
        _ => Body::Presets,
    };

    let mut model = Model {
        icon: icons::icon_name(state, false),
        title,
        detail,
        pill,
        mode_learn: None,
        body,
        watching: String::new(),
        tiles: Vec::new(),
        footer: String::new(),
    };
    if let (false, Some(s)) = (matches!(state, TrayState::Down { .. }), status) {
        let buses = s.buses();
        let watched = buses.iter().filter(|b| b.2).count();
        model.mode_learn = Some(s.mode == "learn");
        model.watching = format!("Watching {watched} of {}", buses.len());
        model.tiles = buses
            .iter()
            .map(|&(key, name, on)| Tile {
                icon: bus_icon(key),
                // "Power supply" is ellipsized beside "on battery" in a half-width tile.
                name: if key == "power_watching" {
                    "Power"
                } else {
                    name
                },
                value: tile_value(s, key, on),
                watched: on,
            })
            .collect();
        let violations = match s.violations_logged {
            1 => "1 violation".to_string(),
            n => format!("{n} violations"),
        };
        model.footer = format!("Up {} · {violations}", format_uptime(s.uptime_secs));
    }
    model
}

/// `m:ss` for the countdown ring.
pub fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The daemon does not report how long the disarm was for, so the ring
/// measures against the smallest preset that still covers what is left.
fn countdown_total(secs_left: u64) -> u64 {
    DISARM_PRESETS
        .iter()
        .map(|p| p.0)
        .find(|&total| total >= secs_left)
        .unwrap_or(secs_left)
        .max(1)
}

/// How a person stops a pending kill without touching the tray.
fn cancel_hint(bus: &str) -> &'static str {
    match bus {
        "power" => "Plug the charger back in to cancel.",
        "network" => "Reconnect the cable to cancel.",
        "lid" => "Open the lid to cancel.",
        _ => "Undo the change to cancel.",
    }
}

/// The tile icon for an `ipc::BUSES` key.
fn bus_icon(key: &str) -> &'static str {
    match key {
        "usb_watching" => "plugkill-bus-usb-symbolic",
        "thunderbolt_watching" => "plugkill-bus-thunderbolt-symbolic",
        "sdcard_watching" => "plugkill-bus-sdcard-symbolic",
        "power_watching" => "plugkill-bus-power-symbolic",
        "network_watching" => "plugkill-bus-network-symbolic",
        "lid_watching" => "plugkill-bus-lid-symbolic",
        "pci_watching" => "plugkill-bus-pci-symbolic",
        "display_watching" => "plugkill-bus-display-symbolic",
        _ => "image-missing",
    }
}

/// What a bus reads: its count or live state when watched, "off" when it is
/// not. The menu shows the same wording.
pub fn tile_value(s: &Status, key: &str, watched: bool) -> String {
    if !watched {
        return "off".into();
    }
    match key {
        "usb_watching" => s.usb_devices.to_string(),
        "thunderbolt_watching" => s.thunderbolt_devices.to_string(),
        "sdcard_watching" => s.sdcard_devices.to_string(),
        "pci_watching" => s.pci_devices.to_string(),
        "power_watching" => match s.power_state.as_deref() {
            Some("ac") => "on AC".into(),
            Some("battery") => "on battery".into(),
            _ => "unknown".into(),
        },
        "lid_watching" => s.lid_state.clone().unwrap_or_else(|| "unknown".into()),
        "network_watching" => match s.network_links_down {
            Some(0) => "link up".into(),
            Some(n) => format!("{n} down"),
            None => "unknown".into(),
        },
        _ => "watching".into(),
    }
}

/// What a tile's hover says: the bus, what it reads, and the entries the
/// daemon sees on it. `key` is an `ipc::BUSES` key, `name` its full name.
pub fn bus_tooltip(key: &str, name: &str, devices: &Devices) -> String {
    let bus = key.strip_suffix("_watching").unwrap_or(key);
    // A bus the response does not carry says nothing about whether it is
    // watched, and the lit tile beside this line may say it is.
    let Some(d) = devices.get(bus) else {
        return format!("{name}, device list unavailable");
    };
    if !d.watched {
        return format!("{name} is not watched");
    }
    // An enumeration the daemon could not read looks exactly like an empty bus
    // on the wire, so this must not claim nothing is connected.
    if d.entries.is_empty() {
        return format!("{name}, no devices listed");
    }
    // Power and lid report one reading, so the reading is the whole first line.
    let mut lines = match (bus, d.entries.as_slice()) {
        ("power" | "lid", [reading]) => vec![format!("{name}, {}", reading.text)],
        _ => {
            // Live entries on the bus, listed plus dropped. The tile counts
            // distinct IDs from the baseline, so the two can differ.
            let n = d.entries.len() + d.more as usize;
            let mut lines = vec![format!("{name}, {n} {}", entry_noun(bus, n))];
            lines.extend(d.entries.iter().map(|e| e.text.clone()));
            lines
        }
    };
    if d.more > 0 {
        lines.push(format!("and {} more", d.more));
    }
    lines.join("\n")
}

/// What a bus counts in its tooltip's first line.
fn entry_noun(bus: &str, n: usize) -> &'static str {
    match (bus, n) {
        ("network", 1) => "interface",
        ("network", _) => "interfaces",
        ("display", 1) => "output",
        ("display", _) => "outputs",
        (_, 1) => "device",
        _ => "devices",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::BusDevices;

    fn devices(pairs: &[(&str, bool, &[&str], u64)]) -> Devices {
        pairs
            .iter()
            .map(|&(key, watched, entries, more)| {
                let entries = entries.iter().map(|&e| e.into()).collect();
                (
                    key.to_string(),
                    BusDevices {
                        watched,
                        entries,
                        more,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn test_tooltip_counts_the_devices_it_lists() {
        let d = devices(&[(
            "usb",
            true,
            &["046d:c52b Logitech Receiver", "1d6b:0002"],
            0,
        )]);
        assert_eq!(
            bus_tooltip("usb_watching", "USB", &d),
            "USB, 2 devices\n046d:c52b Logitech Receiver\n1d6b:0002"
        );
        let one = devices(&[("usb", true, &["1d6b:0002"], 0)]);
        assert_eq!(
            bus_tooltip("usb_watching", "USB", &one),
            "USB, 1 device\n1d6b:0002"
        );
    }

    #[test]
    fn test_tooltip_says_how_many_were_dropped() {
        let d = devices(&[("pci", true, &["0000:00:00.0", "0000:00:02.0"], 11)]);
        assert_eq!(
            bus_tooltip("pci_watching", "PCI", &d),
            "PCI, 13 devices\n0000:00:00.0\n0000:00:02.0\nand 11 more",
            "the header counts the whole live bus, listed plus dropped"
        );
    }

    #[test]
    fn test_tooltip_for_an_empty_or_unwatched_bus() {
        let d = devices(&[("sdcard", true, &[], 0), ("network", false, &[], 0)]);
        assert_eq!(
            bus_tooltip("sdcard_watching", "SD card", &d),
            "SD card, no devices listed"
        );
        assert_eq!(
            bus_tooltip("network_watching", "Network", &d),
            "Network is not watched"
        );
        assert_eq!(
            bus_tooltip("display_watching", "Display", &d),
            "Display, device list unavailable",
            "a bus the daemon does not report must not claim it is unwatched"
        );
    }

    #[test]
    fn test_tooltip_reads_a_live_state_as_a_state() {
        let d = devices(&[
            ("power", true, &["on AC"], 0),
            ("lid", true, &["closed"], 0),
            ("network", true, &["eth0: up", "wlan0: down"], 0),
        ]);
        assert_eq!(
            bus_tooltip("power_watching", "Power supply", &d),
            "Power supply, on AC"
        );
        assert_eq!(bus_tooltip("lid_watching", "Lid", &d), "Lid, closed");
        assert_eq!(
            bus_tooltip("network_watching", "Network", &d),
            "Network, 2 interfaces\neth0: up\nwlan0: down"
        );
    }

    fn status(extra: serde_json::Value) -> Status {
        let mut data = serde_json::json!({"armed": true, "mode": "enforce", "uptime_secs": 7625});
        data.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(data).unwrap()
    }

    #[test]
    fn test_armed_model_shows_presets_mode_and_tiles() {
        let s = status(serde_json::json!({
            "usb_watching": true, "usb_devices": 7, "power_watching": true, "power_state": "ac",
            "violations_logged": 1
        }));
        let m = build(&TrayState::Armed, Some(&s));
        assert_eq!(m.icon, "plugkill-armed-symbolic");
        assert_eq!(m.pill, ("enforce", "pill-armed"));
        assert_eq!(m.mode_learn, Some(false));
        assert_eq!(m.body, Body::Presets);
        assert_eq!(m.watching, "Watching 2 of 8");
        assert_eq!(m.tiles.len(), 8);
        assert_eq!(
            m.tiles[0],
            Tile {
                icon: "plugkill-bus-usb-symbolic",
                name: "USB",
                value: "7".into(),
                watched: true
            }
        );
        assert!(m.tiles.contains(&Tile {
            icon: "plugkill-bus-power-symbolic",
            name: "Power",
            value: "on AC".into(),
            watched: true
        }));
        assert!(m.tiles.contains(&Tile {
            icon: "plugkill-bus-display-symbolic",
            name: "Display",
            value: "off".into(),
            watched: false
        }));
        assert_eq!(m.footer, "Up 2h 7m · 1 violation");
    }

    #[test]
    fn test_every_bus_has_an_embedded_icon() {
        for (key, _) in plugkill_core::ipc::BUSES {
            let icon = bus_icon(key);
            assert!(
                icons::ICONS.iter().any(|(n, _)| *n == icon),
                "{key} maps to {icon}, which is not embedded"
            );
        }
    }

    #[test]
    fn test_disarmed_ring_measures_against_the_covering_preset() {
        let s = status(serde_json::json!({"armed": false, "disarm_remaining_secs": 252}));
        let m = build(&TrayState::Disarmed { secs_left: 252 }, Some(&s));
        assert_eq!(
            m.body,
            Body::Countdown {
                secs_left: 252,
                total_secs: 300
            }
        );
        assert_eq!(countdown_total(3600), 3600);
        assert_eq!(countdown_total(4000), 4000);
        assert_eq!(countdown_total(0), 60);
        // It jumps at every preset boundary, so the dashboard latches the
        // total for the life of one disarm instead of following this.
        assert_eq!(countdown_total(61), 300);
        assert_eq!(countdown_total(60), 60);
    }

    #[test]
    fn test_kill_pending_says_how_to_cancel() {
        let state = TrayState::KillPending {
            bus: "power".into(),
            reason: "AC power removed".into(),
            secs_left: 23,
        };
        let m = build(&state, Some(&status(serde_json::json!({}))));
        assert_eq!(
            m.icon, "plugkill-pending-symbolic",
            "the dashboard never blinks"
        );
        assert_eq!(
            m.body,
            Body::KillPending {
                secs_left: 23,
                reason: "AC power removed".into(),
                hint: "Plug the charger back in to cancel.".into()
            }
        );
    }

    #[test]
    fn test_down_model_hides_mode_and_tiles() {
        let m = build(&TrayState::Down { stalled: false }, None);
        assert_eq!(m.mode_learn, None);
        assert!(m.tiles.is_empty());
        assert!(m.watching.is_empty() && m.footer.is_empty());
        assert!(matches!(m.body, Body::Down { .. }));
    }

    #[test]
    fn test_network_and_lid_tile_values() {
        let s = status(serde_json::json!({
            "network_watching": true, "network_links_down": 2,
            "lid_watching": true, "lid_state": "closed"
        }));
        let m = build(&TrayState::Armed, Some(&s));
        assert!(m.tiles.contains(&Tile {
            icon: "plugkill-bus-network-symbolic",
            name: "Network",
            value: "2 down".into(),
            watched: true
        }));
        assert!(m.tiles.contains(&Tile {
            icon: "plugkill-bus-lid-symbolic",
            name: "Lid",
            value: "closed".into(),
            watched: true
        }));
    }

    #[test]
    fn test_clock_formats_minutes_and_seconds() {
        assert_eq!(clock(252), "4:12");
        assert_eq!(clock(5), "0:05");
        assert_eq!(clock(3600), "60:00");
    }
}
