use crate::sysfs::read_sysfs_attr;
use log::warn;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;

/// Link state of a network interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Up,
    Down,
    Unknown,
}

impl fmt::Display for LinkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkState::Up => write!(f, "up"),
            LinkState::Down => write!(f, "down"),
            LinkState::Unknown => write!(f, "unknown"),
        }
    }
}

/// Snapshot of all monitored network interfaces and their link states.
#[derive(Debug, Clone)]
pub struct NetworkSnapshot {
    interfaces: HashMap<String, LinkState>,
}

/// A detected link state change on a network interface.
#[derive(Debug, Clone)]
pub struct NetworkChange {
    pub interface: String,
    pub from: LinkState,
    pub to: LinkState,
}

impl fmt::Display for NetworkChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "interface {} link changed: {} → {}",
            self.interface, self.from, self.to
        )
    }
}

impl NetworkSnapshot {
    /// The monitored interfaces and their link states.
    pub fn interfaces(&self) -> &HashMap<String, LinkState> {
        &self.interfaces
    }

    /// Detect the first Up → Down transition compared to a baseline.
    pub fn detect_link_down(&self, baseline: &NetworkSnapshot) -> Option<NetworkChange> {
        for (iface, &baseline_state) in &baseline.interfaces {
            if baseline_state != LinkState::Up {
                continue;
            }
            match self.interfaces.get(iface) {
                Some(&LinkState::Down) => {
                    return Some(NetworkChange {
                        interface: iface.clone(),
                        from: LinkState::Up,
                        to: LinkState::Down,
                    });
                }
                None => {
                    // Interface disappeared entirely: treat as link down
                    return Some(NetworkChange {
                        interface: iface.clone(),
                        from: LinkState::Up,
                        to: LinkState::Down,
                    });
                }
                _ => {}
            }
        }
        None
    }
}

/// Enumerate physical network interfaces and their link state.
pub fn enumerate_interfaces(filter: &[String]) -> NetworkSnapshot {
    #[cfg(target_os = "linux")]
    {
        enumerate_interfaces_from(Path::new("/sys/class/net"), filter)
    }
    #[cfg(target_os = "freebsd")]
    {
        freebsd::enumerate(filter)
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = filter;
        NetworkSnapshot {
            interfaces: HashMap::new(),
        }
    }
}

/// Decode a `SIOCGIFMEDIA` status word. `IFM_AVALID` unset means the driver
/// can't report link state.
#[cfg(any(target_os = "freebsd", test))]
pub fn ifmedia_link_state(status: i32) -> LinkState {
    const IFM_AVALID: i32 = 0x0000_0001;
    const IFM_ACTIVE: i32 = 0x0000_0002;
    if status & IFM_AVALID == 0 {
        LinkState::Unknown
    } else if status & IFM_ACTIVE != 0 {
        LinkState::Up
    } else {
        LinkState::Down
    }
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::{LinkState, NetworkSnapshot, ifmedia_link_state};
    use log::warn;
    use std::collections::{BTreeSet, HashMap};

    const IFNAMSIZ: usize = 16;

    // Only ifm_status is read; the rest are ABI layout for the ioctl.
    #[allow(dead_code)]
    #[repr(C)]
    struct IfMediaReq {
        ifm_name: [libc::c_char; IFNAMSIZ],
        ifm_current: libc::c_int,
        ifm_mask: libc::c_int,
        ifm_status: libc::c_int,
        ifm_active: libc::c_int,
        ifm_count: libc::c_int,
        ifm_ulist: *mut libc::c_int,
    }

    // SIOCGIFMEDIA = _IOWR('i', 56, struct ifmediareq); nix encodes the BSD
    // request number from the struct size.
    nix::ioctl_readwrite!(siocgifmedia, b'i', 56, IfMediaReq);

    /// Query one interface. `None` means the ioctl failed, i.e. not a
    /// media-bearing NIC, the FreeBSD analogue of Linux's missing
    /// `/sys/class/net/<if>/device` symlink.
    fn query_link(sock: libc::c_int, name: &str) -> Option<LinkState> {
        let bytes = name.as_bytes();
        if bytes.len() >= IFNAMSIZ {
            return None;
        }
        let mut req: IfMediaReq = unsafe { std::mem::zeroed() };
        for (dst, &b) in req.ifm_name.iter_mut().zip(bytes) {
            *dst = b as libc::c_char;
        }
        // count=0, ulist=null asks only for the status word.
        match unsafe { siocgifmedia(sock, &mut req) } {
            Ok(_) => Some(ifmedia_link_state(req.ifm_status)),
            Err(_) => None,
        }
    }

    pub fn enumerate(filter: &[String]) -> NetworkSnapshot {
        let mut interfaces = HashMap::new();

        // getifaddrs lists each interface once per address family; dedup names.
        let names: BTreeSet<String> = match nix::ifaddrs::getifaddrs() {
            Ok(iter) => iter.map(|ia| ia.interface_name).collect(),
            Err(e) => {
                warn!("getifaddrs failed: {e}");
                return NetworkSnapshot { interfaces };
            }
        };

        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if sock < 0 {
            warn!("cannot open socket for SIOCGIFMEDIA");
            return NetworkSnapshot { interfaces };
        }

        for name in names {
            if !filter.is_empty() && !filter.contains(&name) {
                continue;
            }
            if let Some(state) = query_link(sock, &name) {
                interfaces.insert(name, state);
            }
        }

        unsafe { libc::close(sock) };
        NetworkSnapshot { interfaces }
    }
}

/// Enumerate physical network interfaces from a custom sysfs root (for testing).
pub fn enumerate_interfaces_from(sysfs_root: &Path, filter: &[String]) -> NetworkSnapshot {
    let mut interfaces = HashMap::new();

    let entries = match fs::read_dir(sysfs_root) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                "cannot read network sysfs directory {}: {e}",
                sysfs_root.display()
            );
            return NetworkSnapshot { interfaces };
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("error reading network sysfs entry: {e}");
                continue;
            }
        };

        let iface_name = entry.file_name().to_string_lossy().to_string();
        let iface_path = entry.path();

        // Filter to physical NICs: check if <iface>/device symlink exists
        if !iface_path.join("device").exists() {
            continue;
        }

        // If config specifies interfaces, only monitor those
        if !filter.is_empty() && !filter.contains(&iface_name) {
            continue;
        }

        let state = match read_sysfs_attr(&iface_path.join("operstate")) {
            Ok(Some(s)) => match s.as_str() {
                "up" => LinkState::Up,
                "down" => LinkState::Down,
                _ => LinkState::Unknown,
            },
            _ => LinkState::Unknown,
        };

        interfaces.insert(iface_name, state);
    }

    NetworkSnapshot { interfaces }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    fn create_interface(dir: &Path, name: &str, operstate: &str, physical: bool) {
        let iface = dir.join(name);
        fs::create_dir_all(&iface).unwrap();

        let mut f = fs::File::create(iface.join("operstate")).unwrap();
        writeln!(f, "{operstate}").unwrap();

        if physical {
            // Create a "device" symlink to simulate a physical NIC
            symlink(".", iface.join("device")).unwrap();
        }
    }

    #[test]
    fn test_enumerate_physical_only() {
        let dir = tempfile::tempdir().unwrap();
        create_interface(dir.path(), "eth0", "up", true);
        create_interface(dir.path(), "lo", "up", false); // virtual, no device symlink
        create_interface(dir.path(), "docker0", "down", false);

        let snapshot = enumerate_interfaces_from(dir.path(), &[]);
        assert_eq!(snapshot.interfaces.len(), 1);
        assert_eq!(snapshot.interfaces["eth0"], LinkState::Up);
    }

    #[test]
    fn test_enumerate_with_filter() {
        let dir = tempfile::tempdir().unwrap();
        create_interface(dir.path(), "eth0", "up", true);
        create_interface(dir.path(), "eth1", "up", true);

        let snapshot = enumerate_interfaces_from(dir.path(), &["eth0".to_string()]);
        assert_eq!(snapshot.interfaces.len(), 1);
        assert!(snapshot.interfaces.contains_key("eth0"));
    }

    #[test]
    fn test_detect_link_down() {
        let baseline = NetworkSnapshot {
            interfaces: HashMap::from([
                ("eth0".to_string(), LinkState::Up),
                ("eth1".to_string(), LinkState::Up),
            ]),
        };

        let current = NetworkSnapshot {
            interfaces: HashMap::from([
                ("eth0".to_string(), LinkState::Down),
                ("eth1".to_string(), LinkState::Up),
            ]),
        };

        let change = current.detect_link_down(&baseline);
        assert!(change.is_some());
        let change = change.unwrap();
        assert_eq!(change.interface, "eth0");
        assert_eq!(change.from, LinkState::Up);
        assert_eq!(change.to, LinkState::Down);
    }

    #[test]
    fn test_detect_interface_disappeared() {
        let baseline = NetworkSnapshot {
            interfaces: HashMap::from([("eth0".to_string(), LinkState::Up)]),
        };

        let current = NetworkSnapshot {
            interfaces: HashMap::new(),
        };

        let change = current.detect_link_down(&baseline);
        assert!(change.is_some());
    }

    #[test]
    fn test_no_change() {
        let baseline = NetworkSnapshot {
            interfaces: HashMap::from([("eth0".to_string(), LinkState::Up)]),
        };

        let current = NetworkSnapshot {
            interfaces: HashMap::from([("eth0".to_string(), LinkState::Up)]),
        };

        assert!(current.detect_link_down(&baseline).is_none());
    }

    #[test]
    fn test_baseline_down_stays_down() {
        let baseline = NetworkSnapshot {
            interfaces: HashMap::from([("eth0".to_string(), LinkState::Down)]),
        };

        let current = NetworkSnapshot {
            interfaces: HashMap::from([("eth0".to_string(), LinkState::Down)]),
        };

        // Down → Down is not a violation (only Up → Down triggers)
        assert!(current.detect_link_down(&baseline).is_none());
    }

    #[test]
    fn test_nonexistent_sysfs() {
        let snapshot = enumerate_interfaces_from(Path::new("/nonexistent/net"), &[]);
        assert!(snapshot.interfaces.is_empty());
    }

    #[test]
    fn test_ifmedia_link_state() {
        const AVALID: i32 = 0x1;
        const ACTIVE: i32 = 0x2;
        assert_eq!(ifmedia_link_state(AVALID | ACTIVE), LinkState::Up);
        assert_eq!(ifmedia_link_state(AVALID), LinkState::Down);
        assert_eq!(ifmedia_link_state(0), LinkState::Unknown);
        // ACTIVE without AVALID is not trustworthy → Unknown.
        assert_eq!(ifmedia_link_state(ACTIVE), LinkState::Unknown);
    }
}
