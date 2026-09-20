//! External display (HDMI/DisplayPort/eDP) connect/disconnect monitoring.
//!
//! Produces a `DisplaySnapshot` of every watched connector and, when
//! connected, the monitor identity read from its EDID. Any change between
//! snapshots while armed is the violation, which covers both a laptop yanked
//! from a dock/projector (disconnect) and a rogue capture device attached
//! (connect or identity swap on the same port).
//!
//! Linux enumerates connectors under `/sys/class/drm` and reads each
//! connected one's EDID. FreeBSD has no such sysfs, so a devd listener counts
//! DRM CONNECTOR hotplug events; there the ignore list has no effect because
//! the event does not name the connector.

/// A snapshot of every watched connector and what is attached to it.
pub fn display_snapshot(ignore: &[String]) -> DisplaySnapshot {
    #[cfg(target_os = "linux")]
    {
        linux::snapshot(ignore)
    }
    #[cfg(target_os = "freebsd")]
    {
        let _ = ignore;
        freebsd::snapshot()
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = ignore;
        DisplaySnapshot::default()
    }
}

/// The names of the connectors display monitoring covers, for clients that
/// only want the list. Two cards can expose the same connector name, and the
/// name is all this returns, so a shared name appears once.
pub fn watched_connectors(ignore: &[String]) -> Vec<String> {
    connector_names(&display_snapshot(ignore))
}

/// Split out from `watched_connectors` so the de-duplication can be tested
/// without a sysfs to read.
fn connector_names(snapshot: &DisplaySnapshot) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(snapshot.connectors.len());
    for c in &snapshot.connectors {
        if names.last() != Some(&c.connector) {
            names.push(c.connector.clone());
        }
    }
    names
}

/// A connector is ignored if its name contains any non-empty ignore token,
/// e.g. `eDP` masks the internal panel.
#[cfg(any(target_os = "linux", test))]
fn is_ignored(connector: &str, ignore: &[String]) -> bool {
    ignore
        .iter()
        .any(|ig| !ig.is_empty() && connector.contains(ig.as_str()))
}

/// Connector directories are named `card<N>-<CONNECTOR>`, for example
/// `card0-DP-6`. Everything else under /sys/class/drm (`card0`, `renderD128`,
/// `version`) is not a connector.
#[cfg(any(target_os = "linux", test))]
fn connector_name(dir: &str) -> Option<&str> {
    let (card, conn) = dir.split_once('-')?;
    if !card.starts_with("card") || conn.is_empty() {
        return None;
    }
    Some(conn)
}

/// Apply the ignore list and sort. Separated from the directory walk so it can
/// be tested without a real sysfs.
#[cfg(any(target_os = "linux", test))]
fn build_snapshot(
    found: Vec<(String, bool, Option<EdidId>)>,
    ignore: &[String],
) -> DisplaySnapshot {
    DisplaySnapshot::from_connectors(
        found
            .into_iter()
            .filter(|(name, _, _)| !is_ignored(name, ignore))
            .map(|(connector, connected, edid)| DisplayConnector {
                connector,
                connected,
                edid,
            })
            .collect(),
    )
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{DisplaySnapshot, build_snapshot, connector_name};
    use crate::edid;
    use log::warn;
    use std::io::Read;
    use std::path::Path;
    use std::sync::Once;

    const DRM_CLASS: &str = "/sys/class/drm";

    pub fn snapshot(ignore: &[String]) -> DisplaySnapshot {
        static WARNED: Once = Once::new();
        let mut found = Vec::new();
        match std::fs::read_dir(Path::new(DRM_CLASS)) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let dir = entry.file_name().to_string_lossy().to_string();
                    let Some(connector) = connector_name(&dir) else {
                        continue;
                    };
                    let status = std::fs::read_to_string(entry.path().join("status"))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                    let connected = status == "connected";
                    let edid = if connected {
                        read_edid(&entry.path().join("edid"))
                    } else {
                        None
                    };
                    found.push((connector.to_string(), connected, edid));
                }
            }
            Err(e) => {
                WARNED.call_once(|| warn!("display monitoring: cannot read {DRM_CLASS}: {e}"))
            }
        }
        build_snapshot(found, ignore)
    }

    /// Read at most the base block. sysfs reports these files as zero length in
    /// metadata, so the size is taken from the read rather than from `stat`, and
    /// the buffer is fixed so a misbehaving driver cannot make us allocate.
    fn read_edid(path: &Path) -> Option<edid::EdidId> {
        let mut f = std::fs::File::open(path).ok()?;
        let mut buf = [0u8; edid::EDID_BASE_LEN];
        let mut filled = 0;
        while filled < buf.len() {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => return None,
            }
        }
        edid::parse(&buf[..filled])
    }
}

/// A devd line is a DRM connector hotplug if it names the DRM system and the
/// CONNECTOR subsystem.
#[cfg(any(target_os = "freebsd", test))]
fn is_drm_connector_event(line: &str) -> bool {
    line.contains("system=DRM") && line.contains("subsystem=CONNECTOR")
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::is_drm_connector_event;
    use log::warn;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Once, OnceLock};
    use std::time::Duration;

    static GEN: AtomicU64 = AtomicU64::new(0);
    const DEVD_PIPE: &str = "/var/run/devd.pipe";

    /// FreeBSD reports only that the display topology changed, not which
    /// connector moved or which monitor is attached. The counter is carried
    /// in opaque_generation and the connector list is empty, keeping the type
    /// uniform across platforms. Display identity is unavailable here.
    pub fn snapshot() -> super::DisplaySnapshot {
        ensure_listener();
        super::DisplaySnapshot {
            connectors: vec![],
            opaque_generation: Some(GEN.load(Ordering::Relaxed)),
        }
    }

    fn ensure_listener() {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            let _ = std::thread::Builder::new()
                .name("plugkill-drm-devd".into())
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
                        if is_drm_connector_event(&line) {
                            GEN.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(e) => WARNED.call_once(|| {
                    warn!("display monitoring: cannot connect to devd at {DEVD_PIPE}: {e}")
                }),
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

use crate::edid::EdidId;

/// One connector and what is attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayConnector {
    /// Connector name as the kernel reports it, for example "DP-6".
    pub connector: String,
    pub connected: bool,
    /// Monitor identity, when the EDID could be read and parsed.
    pub edid: Option<EdidId>,
}

/// Every watched connector, sorted by name so comparison is order independent.
/// On a platform that reports only that the topology changed, with no per
/// connector data, `opaque_generation` carries that platform's event counter
/// and the connector list is empty. Linux leaves it None.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplaySnapshot {
    pub connectors: Vec<DisplayConnector>,
    pub opaque_generation: Option<u64>,
}

/// What moved between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayChange {
    /// A connector the baseline did not have.
    Appeared(String),
    /// A connector the baseline had and this snapshot does not.
    Disappeared(String),
    Connected(String),
    Disconnected(String),
    /// Same connector, still connected, different monitor.
    Replaced {
        connector: String,
        now: Option<EdidId>,
    },
    /// Topology changed, but the platform reports only that change happened,
    /// not which connector moved or which monitor is attached.
    TopologyChanged,
}

impl std::fmt::Display for DisplayChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Appeared(c) => write!(f, "{c} appeared"),
            Self::Disappeared(c) => write!(f, "{c} disappeared"),
            Self::Connected(c) => write!(f, "{c} connected"),
            Self::Disconnected(c) => write!(f, "{c} disconnected"),
            Self::Replaced { connector, now } => match now {
                Some(id) if !id.name.is_empty() => {
                    write!(f, "{connector} now {} ({})", id.selector(), id.name)
                }
                Some(id) => write!(f, "{connector} now {}", id.selector()),
                None => write!(f, "{connector} now an unidentified monitor"),
            },
            Self::TopologyChanged => write!(f, "display topology changed"),
        }
    }
}

impl DisplaySnapshot {
    /// Build a snapshot, sorting so that enumeration order cannot show up as a
    /// change.
    pub fn from_connectors(mut connectors: Vec<DisplayConnector>) -> Self {
        connectors.sort_by(|a, b| a.connector.cmp(&b.connector));
        Self {
            connectors,
            opaque_generation: None,
        }
    }

    fn get(&self, connector: &str) -> Option<&DisplayConnector> {
        self.connectors.iter().find(|c| c.connector == connector)
    }

    /// First difference against `baseline`, or `None` when they agree.
    pub fn detect_changes(&self, baseline: &DisplaySnapshot) -> Option<DisplayChange> {
        if self.opaque_generation != baseline.opaque_generation {
            return Some(DisplayChange::TopologyChanged);
        }
        for c in &self.connectors {
            let Some(was) = baseline.get(&c.connector) else {
                return Some(DisplayChange::Appeared(c.connector.clone()));
            };
            if c.connected != was.connected {
                return Some(if c.connected {
                    DisplayChange::Connected(c.connector.clone())
                } else {
                    DisplayChange::Disconnected(c.connector.clone())
                });
            }
            // Identity only means anything while something is attached. A
            // disconnected connector reports no EDID on either side.
            if c.connected && c.edid != was.edid {
                return Some(DisplayChange::Replaced {
                    connector: c.connector.clone(),
                    now: c.edid.clone(),
                });
            }
        }
        for was in &baseline.connectors {
            if self.get(&was.connector).is_none() {
                return Some(DisplayChange::Disappeared(was.connector.clone()));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watched_connectors_lists_a_shared_name_once() {
        // Two cards exposing DP-1 is what a laptop with a dock looks like.
        let snapshot = DisplaySnapshot::from_connectors(vec![
            conn("DP-1", true, None),
            conn("DP-1", false, None),
            conn("eDP-1", true, None),
        ]);
        assert_eq!(
            connector_names(&snapshot),
            vec!["DP-1".to_string(), "eDP-1".to_string()]
        );
    }

    /// The directory walk is untestable without a real sysfs, so the naming
    /// rule it depends on is factored out and tested directly.
    #[test]
    fn test_connector_name_extraction() {
        assert_eq!(connector_name("card0-DP-6"), Some("DP-6"));
        assert_eq!(connector_name("card0-eDP-1"), Some("eDP-1"));
        assert_eq!(connector_name("card1-HDMI-A-1"), Some("HDMI-A-1"));
        assert_eq!(connector_name("card0"), None);
        assert_eq!(connector_name("renderD128"), None);
        assert_eq!(connector_name("version"), None);
        assert_eq!(connector_name("card0-"), None);
    }

    /// Spec F10.
    #[test]
    fn test_ignored_connectors_are_absent_from_a_snapshot() {
        let built = build_snapshot(
            vec![
                ("eDP-1".to_string(), true, None),
                ("DP-6".to_string(), true, None),
            ],
            &["eDP".to_string()],
        );
        assert_eq!(built.connectors.len(), 1);
        assert_eq!(built.connectors[0].connector, "DP-6");
    }

    #[test]
    fn test_is_ignored() {
        let ig = vec!["eDP".to_string()];
        assert!(is_ignored("eDP-1", &ig));
        assert!(!is_ignored("DP-1", &ig));
        assert!(!is_ignored("HDMI-A-1", &ig));
    }

    #[test]
    fn test_is_drm_connector_event() {
        assert!(is_drm_connector_event(
            "!system=DRM subsystem=CONNECTOR type=HOTPLUG"
        ));
        assert!(!is_drm_connector_event(
            "!system=ACPI subsystem=Lid notify=0x00"
        ));
        assert!(!is_drm_connector_event("!system=DRM subsystem=DEVICE"));
    }

    fn conn(name: &str, connected: bool, edid: Option<EdidId>) -> DisplayConnector {
        DisplayConnector {
            connector: name.to_string(),
            connected,
            edid,
        }
    }

    fn id(mfg: &str, product: u16, serial: Option<u32>) -> EdidId {
        EdidId {
            mfg: mfg.to_string(),
            product,
            serial,
            name: String::new(),
        }
    }

    #[test]
    fn test_identical_snapshots_report_no_change() {
        let a = DisplaySnapshot::from_connectors(vec![
            conn("DP-1", false, None),
            conn("eDP-1", true, Some(id("TMA", 0x2064, None))),
        ]);
        let b = DisplaySnapshot::from_connectors(vec![
            conn("eDP-1", true, Some(id("TMA", 0x2064, None))),
            conn("DP-1", false, None),
        ]);
        assert_eq!(a.detect_changes(&b), None, "order must not matter");
    }

    /// Spec B1.
    #[test]
    fn test_new_connector_is_a_change() {
        let base = DisplaySnapshot::from_connectors(vec![conn("DP-1", false, None)]);
        let now = DisplaySnapshot::from_connectors(vec![
            conn("DP-1", false, None),
            conn("HDMI-A-1", false, None),
        ]);
        assert_eq!(
            now.detect_changes(&base),
            Some(DisplayChange::Appeared("HDMI-A-1".to_string()))
        );
    }

    /// Spec B1.
    #[test]
    fn test_missing_connector_is_a_change() {
        let base = DisplaySnapshot::from_connectors(vec![
            conn("DP-1", false, None),
            conn("HDMI-A-1", false, None),
        ]);
        let now = DisplaySnapshot::from_connectors(vec![conn("DP-1", false, None)]);
        assert_eq!(
            now.detect_changes(&base),
            Some(DisplayChange::Disappeared("HDMI-A-1".to_string()))
        );
    }

    /// Spec B2.
    #[test]
    fn test_plugging_a_monitor_in_is_a_change() {
        let base = DisplaySnapshot::from_connectors(vec![conn("DP-6", false, None)]);
        let now = DisplaySnapshot::from_connectors(vec![conn(
            "DP-6",
            true,
            Some(id("SAM", 0x772d, Some(811_021_873))),
        )]);
        assert_eq!(
            now.detect_changes(&base),
            Some(DisplayChange::Connected("DP-6".to_string()))
        );
    }

    /// Spec B2.
    #[test]
    fn test_unplugging_a_monitor_is_a_change() {
        let base = DisplaySnapshot::from_connectors(vec![conn(
            "DP-6",
            true,
            Some(id("SAM", 0x772d, Some(811_021_873))),
        )]);
        let now = DisplaySnapshot::from_connectors(vec![conn("DP-6", false, None)]);
        assert_eq!(
            now.detect_changes(&base),
            Some(DisplayChange::Disconnected("DP-6".to_string()))
        );
    }

    /// A different monitor on the same connector, still connected, is a change.
    /// Spec B3.
    #[test]
    fn test_a_different_monitor_on_the_same_port_is_a_change() {
        let base = DisplaySnapshot::from_connectors(vec![conn(
            "DP-6",
            true,
            Some(id("SAM", 0x772d, Some(811_021_873))),
        )]);
        let now = DisplaySnapshot::from_connectors(vec![conn(
            "DP-6",
            true,
            Some(id("SAM", 0x772d, Some(999_999_999))),
        )]);
        match now.detect_changes(&base) {
            Some(DisplayChange::Replaced { connector, now }) => {
                assert_eq!(connector, "DP-6");
                assert_eq!(now.unwrap().serial, Some(999_999_999));
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    /// Spec B4: the free text name is descriptive and is not part of identity.
    #[test]
    fn test_name_alone_does_not_make_a_change() {
        let mut a = id("SAM", 0x772d, Some(1));
        a.name = "Odyssey G93SD".to_string();
        let mut b = id("SAM", 0x772d, Some(1));
        b.name = "ODYSSEY".to_string();
        let base = DisplaySnapshot::from_connectors(vec![conn("DP-6", true, Some(a))]);
        let now = DisplaySnapshot::from_connectors(vec![conn("DP-6", true, Some(b))]);
        assert_eq!(now.detect_changes(&base), None);
    }

    /// Spec B5: unreadable EDID on both sides falls back to name and state.
    #[test]
    fn test_unreadable_edid_on_both_sides_is_no_change() {
        let base = DisplaySnapshot::from_connectors(vec![conn("DP-6", true, None)]);
        let now = DisplaySnapshot::from_connectors(vec![conn("DP-6", true, None)]);
        assert_eq!(now.detect_changes(&base), None);
    }

    /// Spec B6.
    #[test]
    fn test_gaining_a_readable_identity_is_a_change() {
        let base = DisplaySnapshot::from_connectors(vec![conn("DP-6", true, None)]);
        let now = DisplaySnapshot::from_connectors(vec![conn(
            "DP-6",
            true,
            Some(id("SAM", 0x772d, Some(1))),
        )]);
        assert!(matches!(
            now.detect_changes(&base),
            Some(DisplayChange::Replaced { .. })
        ));
    }

    /// Spec B7: the message has to name what moved.
    #[test]
    fn test_change_messages_name_the_connector() {
        assert_eq!(
            DisplayChange::Connected("HDMI-A-1".to_string()).to_string(),
            "HDMI-A-1 connected"
        );
        let mut m = id("SAM", 0x772d, Some(811_021_873));
        m.name = "Odyssey G93SD".to_string();
        assert_eq!(
            DisplayChange::Replaced {
                connector: "DP-6".to_string(),
                now: Some(m),
            }
            .to_string(),
            "DP-6 now SAM:772d:811021873 (Odyssey G93SD)"
        );
    }
}
