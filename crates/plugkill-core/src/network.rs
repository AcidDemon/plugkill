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
    /// Get the interface map for inspection.
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
                    // Interface disappeared entirely — treat as link down
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

/// Enumerate physical network interfaces from the default sysfs path.
pub fn enumerate_interfaces(filter: &[String]) -> NetworkSnapshot {
    enumerate_interfaces_from(Path::new("/sys/class/net"), filter)
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
        create_interface(dir.path(), "lo", "up", false); // virtual — no device symlink
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
}
