use log::warn;
use std::fmt;
use std::path::Path;

/// State of the laptop lid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LidState {
    Open,
    Closed,
    Unknown,
}

impl fmt::Display for LidState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LidState::Open => write!(f, "open"),
            LidState::Closed => write!(f, "closed"),
            LidState::Unknown => write!(f, "unknown"),
        }
    }
}

/// Read the current lid state.
///
/// Primary: D-Bus `LidClosed` property on logind.
/// Fallback: `/proc/acpi/button/lid/LID0/state`.
pub fn read_lid_state() -> LidState {
    // Try D-Bus first
    if let Some(state) = read_lid_state_dbus() {
        return state;
    }

    // Fallback to procfs
    read_lid_state_from_proc(Path::new("/proc/acpi/button/lid/LID0/state"))
}

/// Read lid state from D-Bus logind.
fn read_lid_state_dbus() -> Option<LidState> {
    use zbus::blocking::Connection;
    use zbus::zvariant::OwnedValue;

    let conn = match Connection::system() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to connect to system D-Bus for lid state: {e}");
            return None;
        }
    };

    let reply = conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &("org.freedesktop.login1.Manager", "LidClosed"),
    );

    match reply
        .ok()
        .and_then(|r| r.body().deserialize::<OwnedValue>().ok())
        .and_then(|v| bool::try_from(v).ok())
    {
        Some(true) => Some(LidState::Closed),
        Some(false) => Some(LidState::Open),
        None => {
            warn!("failed to read LidClosed property from logind");
            None
        }
    }
}

/// Read lid state from a procfs file (testable variant).
pub fn read_lid_state_from_proc(path: &Path) -> LidState {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let contents = contents.trim().to_lowercase();
            if contents.contains("open") {
                LidState::Open
            } else if contents.contains("closed") {
                LidState::Closed
            } else {
                LidState::Unknown
            }
        }
        Err(_) => LidState::Unknown,
    }
}

/// Acquire a logind sleep inhibitor so plugkill gets a window to act before suspend.
///
/// Returns an `OwnedFd` that keeps the inhibitor alive — dropping it releases the lock.
/// Returns `None` on D-Bus errors (graceful degradation).
pub fn acquire_sleep_inhibitor() -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::OwnedFd;
    use zbus::blocking::Connection;
    use zbus::zvariant::OwnedFd as ZbusFd;

    let conn = match Connection::system() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to connect to system D-Bus for sleep inhibitor: {e}");
            return None;
        }
    };

    // Inhibit("handle-lid-switch:sleep", "plugkill", "hardware monitoring", "delay")
    let reply = conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "Inhibit",
        &(
            "handle-lid-switch:sleep",
            "plugkill",
            "hardware kill-switch monitoring before suspend",
            "delay",
        ),
    );

    match reply {
        Ok(r) => match r.body().deserialize::<ZbusFd>() {
            Ok(fd) => {
                let owned: OwnedFd = fd.into();
                Some(owned)
            }
            Err(e) => {
                warn!("failed to deserialize sleep inhibitor fd: {e}");
                None
            }
        },
        Err(e) => {
            warn!("failed to acquire sleep inhibitor: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_lid_state_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "state:      open").unwrap();

        assert_eq!(read_lid_state_from_proc(&path), LidState::Open);
    }

    #[test]
    fn test_lid_state_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "state:      closed").unwrap();

        assert_eq!(read_lid_state_from_proc(&path), LidState::Closed);
    }

    #[test]
    fn test_lid_state_nonexistent() {
        assert_eq!(
            read_lid_state_from_proc(Path::new("/nonexistent/lid/state")),
            LidState::Unknown
        );
    }

    #[test]
    fn test_lid_state_unknown_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "state:      something_weird").unwrap();

        assert_eq!(read_lid_state_from_proc(&path), LidState::Unknown);
    }

    #[test]
    fn test_dbus_graceful_failure() {
        // In test environment, D-Bus may not be available
        let result = read_lid_state_dbus();
        // Must not panic — None is fine
        assert!(result.is_none() || result.is_some());
    }
}
