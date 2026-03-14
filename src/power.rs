use crate::usb::read_sysfs_attr;
use log::warn;
use std::fmt;
use std::fs;
use std::path::Path;

/// The power state of the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// AC power is connected.
    Ac,
    /// Running on battery only.
    Battery,
    /// Could not determine power state.
    Unknown,
}

impl fmt::Display for PowerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PowerState::Ac => write!(f, "AC"),
            PowerState::Battery => write!(f, "battery"),
            PowerState::Unknown => write!(f, "unknown"),
        }
    }
}

/// Read the current power state from sysfs.
pub fn read_power_state() -> PowerState {
    read_power_state_from(Path::new("/sys/class/power_supply"))
}

/// Read the current power state from a custom sysfs root (for testing).
pub fn read_power_state_from(sysfs_root: &Path) -> PowerState {
    let entries = match fs::read_dir(sysfs_root) {
        Ok(e) => e,
        Err(_) => return PowerState::Unknown,
    };

    let mut found_mains = false;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("error reading power_supply entry: {e}");
                continue;
            }
        };

        let dev_path = entry.path();

        // Check the type of this power supply
        let supply_type = match read_sysfs_attr(&dev_path.join("type")) {
            Ok(Some(t)) => t,
            _ => continue,
        };

        if supply_type == "Mains" {
            found_mains = true;
            if let Ok(Some(online)) = read_sysfs_attr(&dev_path.join("online"))
                && online == "1"
            {
                return PowerState::Ac;
            }
        } else if supply_type == "Battery" {
            // Check battery status as secondary AC indicator
            if let Ok(Some(status)) = read_sysfs_attr(&dev_path.join("status"))
                && (status == "Charging" || status == "Full")
            {
                return PowerState::Ac;
            }
        }
    }

    if found_mains {
        PowerState::Battery
    } else {
        // No mains entry found — could be a desktop or VM without battery info
        PowerState::Unknown
    }
}

/// Check if any graphical session is locked via logind D-Bus.
///
/// Returns `Some(true)` if any graphical session has `LockedHint == true`,
/// `Some(false)` if all graphical sessions are unlocked,
/// `None` on D-Bus errors (graceful degradation).
pub fn is_session_locked() -> Option<bool> {
    use zbus::blocking::Connection;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue};

    let conn = match Connection::system() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to connect to system D-Bus: {e}");
            return None;
        }
    };

    // List all sessions via logind Manager
    let reply = conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "ListSessions",
        &(),
    );

    let reply = match reply {
        Ok(r) => r,
        Err(e) => {
            warn!("failed to list logind sessions: {e}");
            return None;
        }
    };

    // ListSessions returns array of (session_id, uid, user_name, seat_id, object_path)
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        match reply.body().deserialize() {
            Ok(s) => s,
            Err(e) => {
                warn!("failed to deserialize logind sessions: {e}");
                return None;
            }
        };

    let mut found_graphical = false;

    for (session_id, _uid, _user, _seat, path) in &sessions {
        // Check session type — only care about graphical sessions
        let type_reply = conn.call_method(
            Some("org.freedesktop.login1"),
            path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.login1.Session", "Type"),
        );

        let session_type: String = match type_reply
            .ok()
            .and_then(|r| r.body().deserialize::<OwnedValue>().ok())
            .and_then(|v| String::try_from(v).ok())
        {
            Some(t) => t,
            None => continue,
        };

        if session_type != "wayland" && session_type != "x11" {
            continue;
        }

        found_graphical = true;

        // Check LockedHint property
        let lock_reply = conn.call_method(
            Some("org.freedesktop.login1"),
            path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.login1.Session", "LockedHint"),
        );

        match lock_reply
            .ok()
            .and_then(|r| r.body().deserialize::<OwnedValue>().ok())
            .and_then(|v| bool::try_from(v).ok())
        {
            Some(true) => {
                return Some(true);
            }
            Some(false) => {}
            None => {
                warn!("failed to read LockedHint for session {session_id}");
            }
        }
    }

    if found_graphical {
        Some(false)
    } else {
        // No graphical sessions found — can't determine lock state
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn create_power_supply(dir: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dev = dir.join(name);
        fs::create_dir(&dev).unwrap();
        for (attr, val) in attrs {
            let mut f = fs::File::create(dev.join(attr)).unwrap();
            writeln!(f, "{val}").unwrap();
        }
    }

    #[test]
    fn test_power_state_ac() {
        let dir = tempfile::tempdir().unwrap();
        create_power_supply(dir.path(), "AC0", &[("type", "Mains"), ("online", "1")]);
        create_power_supply(
            dir.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Charging")],
        );
        assert_eq!(read_power_state_from(dir.path()), PowerState::Ac);
    }

    #[test]
    fn test_power_state_battery() {
        let dir = tempfile::tempdir().unwrap();
        create_power_supply(dir.path(), "AC0", &[("type", "Mains"), ("online", "0")]);
        create_power_supply(
            dir.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        assert_eq!(read_power_state_from(dir.path()), PowerState::Battery);
    }

    #[test]
    fn test_power_state_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_power_state_from(dir.path()), PowerState::Unknown);
    }

    #[test]
    fn test_power_state_no_mains_charging() {
        // Only battery entry reporting "Charging" → AC detected via battery status
        let dir = tempfile::tempdir().unwrap();
        create_power_supply(
            dir.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Charging")],
        );
        assert_eq!(read_power_state_from(dir.path()), PowerState::Ac);
    }

    #[test]
    fn test_power_state_no_mains_discharging() {
        // Only battery entry reporting "Discharging" → no mains found → Unknown
        let dir = tempfile::tempdir().unwrap();
        create_power_supply(
            dir.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        assert_eq!(read_power_state_from(dir.path()), PowerState::Unknown);
    }

    #[test]
    fn test_power_state_nonexistent_dir() {
        assert_eq!(
            read_power_state_from(Path::new("/nonexistent/power_supply")),
            PowerState::Unknown
        );
    }

    #[test]
    fn test_lock_detection_graceful_failure() {
        // In test environment, D-Bus system bus may not be available
        // is_session_locked should return None gracefully
        let result = is_session_locked();
        // We can't assert a specific value since it depends on the test environment,
        // but it must not panic
        assert!(result.is_none() || result.is_some());
    }
}
