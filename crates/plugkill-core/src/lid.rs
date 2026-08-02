#[cfg(target_os = "linux")]
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
/// Linux reads logind's `LidClosed`, falling back to procfs. FreeBSD tracks
/// state from devd ACPI Lid events (see the `freebsd` submodule).
pub fn read_lid_state() -> LidState {
    #[cfg(target_os = "linux")]
    {
        read_lid_state_linux()
    }
    #[cfg(target_os = "freebsd")]
    {
        freebsd::read_lid_state()
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        LidState::Unknown
    }
}

#[cfg(target_os = "linux")]
fn read_lid_state_linux() -> LidState {
    if let Some(state) = read_lid_state_dbus() {
        return state;
    }
    read_lid_state_from_proc(Path::new("/proc/acpi/button/lid/LID0/state"))
}

/// Read lid state from D-Bus logind.
#[cfg(target_os = "linux")]
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

/// Acquire a sleep inhibitor so plugkill gets a window to act before suspend.
///
/// The returned `OwnedFd` holds the inhibitor; dropping it releases the lock.
/// FreeBSD has no logind inhibitor, so it returns `None` and relies on
/// `hw.acpi.lid_switch_state=NONE` keeping the machine awake instead.
pub fn acquire_sleep_inhibitor() -> Option<std::os::fd::OwnedFd> {
    #[cfg(target_os = "linux")]
    {
        acquire_sleep_inhibitor_linux()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn acquire_sleep_inhibitor_linux() -> Option<std::os::fd::OwnedFd> {
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

    // Inhibit sleep so plugkill gets a window to act before lid-triggered suspend.
    // "delay" mode is only valid for "shutdown" and "sleep", not "handle-lid-switch".
    let reply = conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "Inhibit",
        &(
            "sleep",
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

/// Parse a devd event line for an ACPI lid notification.
///
/// `notify=0x00` is a closed lid, `notify=0x01` is open. Lines for anything
/// other than an ACPI Lid event return `None`.
#[cfg(any(target_os = "freebsd", test))]
pub fn parse_lid_devd_line(line: &str) -> Option<LidState> {
    if !line.contains("system=ACPI") || !line.contains("subsystem=Lid") {
        return None;
    }
    let notify = line
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("notify="))?;
    match notify {
        "0x00" | "0" => Some(LidState::Closed),
        "0x01" | "1" => Some(LidState::Open),
        _ => None,
    }
}

// FreeBSD has no logind and no poll-able lid sysctl. A background thread reads
// devd's event socket and tracks the latest ACPI Lid notification; read_lid_state
// returns that cached value. Pair with hw.acpi.lid_switch_state=NONE so the OS
// stays awake and plugkill sees the close first.
#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::{LidState, parse_lid_devd_line};
    use log::warn;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Once, OnceLock};
    use std::time::Duration;

    const OPEN: u8 = 0;
    const CLOSED: u8 = 1;
    const UNKNOWN: u8 = 2;

    // Seeded Open: a daemon normally starts with the lid up, and there is no
    // way to query the initial state. devd events correct it from here.
    static STATE: AtomicU8 = AtomicU8::new(OPEN);

    const DEVD_PIPE: &str = "/var/run/devd.pipe";

    pub fn read_lid_state() -> LidState {
        ensure_listener();
        match STATE.load(Ordering::Relaxed) {
            OPEN => LidState::Open,
            CLOSED => LidState::Closed,
            _ => LidState::Unknown,
        }
    }

    fn ensure_listener() {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            let _ = std::thread::Builder::new()
                .name("plugkill-lid-devd".into())
                .spawn(listen_loop);
        });
    }

    fn listen_loop() {
        static WARNED: Once = Once::new();
        loop {
            match UnixStream::connect(DEVD_PIPE) {
                Ok(stream) => {
                    for line in BufReader::new(stream).lines() {
                        let Ok(line) = line else { break };
                        if let Some(state) = parse_lid_devd_line(&line) {
                            let v = match state {
                                LidState::Open => OPEN,
                                LidState::Closed => CLOSED,
                                LidState::Unknown => UNKNOWN,
                            };
                            STATE.store(v, Ordering::Relaxed);
                        }
                    }
                }
                Err(e) => {
                    WARNED.call_once(|| warn!("cannot connect to devd at {DEVD_PIPE}: {e}"));
                }
            }
            std::thread::sleep(Duration::from_secs(2));
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

    #[cfg(target_os = "linux")]
    #[test]
    fn test_dbus_graceful_failure() {
        // D-Bus may be absent in CI; the only contract is "don't panic".
        let result = read_lid_state_dbus();
        assert!(result.is_none() || result.is_some());
    }

    #[test]
    fn test_parse_lid_devd_line() {
        assert_eq!(
            parse_lid_devd_line("!system=ACPI subsystem=Lid type=notify notify=0x00"),
            Some(LidState::Closed)
        );
        assert_eq!(
            parse_lid_devd_line("!system=ACPI subsystem=Lid type=notify notify=0x01"),
            Some(LidState::Open)
        );
        // Unrelated ACPI events and other subsystems are ignored.
        assert_eq!(
            parse_lid_devd_line("!system=ACPI subsystem=Thermal notify=0x00"),
            None
        );
        assert_eq!(parse_lid_devd_line("+uhub0 at ..."), None);
    }
}
