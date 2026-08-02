use crate::error::Error;
use std::collections::HashSet;
use std::fmt;

/// Snapshot of connected PCI devices, keyed by selector. On Linux the selector
/// is the sysfs address (`0000:01:00.0`); on FreeBSD it is the pciconf selector
/// (`pci0:1:0:0`). A Thunderbolt device that tunnels PCIe shows up here as an
/// added device, which is how TB add/remove is caught without a `unique_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciSnapshot {
    devices: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PciChange {
    Added(String),
    Removed(String),
}

impl fmt::Display for PciChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PciChange::Added(s) => write!(f, "unauthorized PCI device added: {s}"),
            PciChange::Removed(s) => write!(f, "PCI device removed: {s}"),
        }
    }
}

impl PciSnapshot {
    pub fn from_set(devices: HashSet<String>) -> Self {
        Self { devices }
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// First add/remove versus a baseline. Ignored selectors are already absent
    /// from both sides (filtered at enumeration), so they never show up here.
    pub fn detect_changes(&self, baseline: &PciSnapshot) -> Option<PciChange> {
        for d in &self.devices {
            if !baseline.devices.contains(d) {
                return Some(PciChange::Added(d.clone()));
            }
        }
        for d in &baseline.devices {
            if !self.devices.contains(d) {
                return Some(PciChange::Removed(d.clone()));
            }
        }
        None
    }
}

/// A selector is ignored if it contains any non-empty ignore token as a
/// substring, so `pci0:1:0` can mask a whole slot's flapping functions.
fn is_ignored(selector: &str, ignore: &[String]) -> bool {
    ignore
        .iter()
        .any(|ig| !ig.is_empty() && selector.contains(ig.as_str()))
}

/// Enumerate connected PCI devices, excluding any matching `ignore`.
pub fn enumerate_pci(ignore: &[String]) -> Result<PciSnapshot, Error> {
    #[cfg(target_os = "linux")]
    {
        linux::enumerate(ignore)
    }
    #[cfg(target_os = "freebsd")]
    {
        freebsd::enumerate(ignore)
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = ignore;
        Ok(PciSnapshot::from_set(HashSet::new()))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Error, HashSet, PciSnapshot, is_ignored};
    use std::path::Path;

    pub fn enumerate(ignore: &[String]) -> Result<PciSnapshot, Error> {
        let root = Path::new("/sys/bus/pci/devices");
        let entries = std::fs::read_dir(root)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", root.display())))?;
        let mut devices = HashSet::new();
        for entry in entries.flatten() {
            let sel = entry.file_name().to_string_lossy().to_string();
            if !is_ignored(&sel, ignore) {
                devices.insert(sel);
            }
        }
        Ok(PciSnapshot::from_set(devices))
    }
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::{Error, PciSnapshot, parse_pciconf};
    use std::process::Command;

    pub fn enumerate(ignore: &[String]) -> Result<PciSnapshot, Error> {
        let out = Command::new("pciconf")
            .arg("-l")
            .output()
            .map_err(|e| Error::Config(format!("cannot run pciconf: {e}")))?;
        if !out.status.success() {
            return Err(Error::Config(format!("pciconf exited: {}", out.status)));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(PciSnapshot::from_set(parse_pciconf(&text, ignore)))
    }
}

/// Parse `pciconf -l` output into the set of selectors, excluding ignored ones.
/// Each line looks like `nvme0@pci0:1:0:0:\tclass=0x010802 ...`; the stable id is
/// the `pciN:b:s:f` selector after the `@`.
#[cfg(any(target_os = "freebsd", test))]
pub fn parse_pciconf(output: &str, ignore: &[String]) -> HashSet<String> {
    output
        .lines()
        .filter_map(parse_pciconf_selector)
        .filter(|s| !is_ignored(s, ignore))
        .collect()
}

#[cfg(any(target_os = "freebsd", test))]
fn parse_pciconf_selector(line: &str) -> Option<String> {
    let head = line.split_whitespace().next()?; // "nvme0@pci0:1:0:0:"
    let sel = head.split_once('@')?.1; // "pci0:1:0:0:"
    let sel = sel.trim_end_matches(':');
    (!sel.is_empty()).then(|| sel.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(items: &[&str]) -> PciSnapshot {
        PciSnapshot::from_set(items.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn test_detect_added_removed() {
        let base = snap(&["pci0:1:0:0", "pci0:2:0:0"]);
        assert_eq!(
            snap(&["pci0:1:0:0", "pci0:2:0:0", "pci0:3:0:0"]).detect_changes(&base),
            Some(PciChange::Added("pci0:3:0:0".to_string()))
        );
        assert_eq!(
            snap(&["pci0:1:0:0"]).detect_changes(&base),
            Some(PciChange::Removed("pci0:2:0:0".to_string()))
        );
        assert_eq!(base.detect_changes(&base), None);
    }

    #[test]
    fn test_parse_pciconf() {
        let out = "\
hostb0@pci0:0:0:0:\tclass=0x060000 card=0x00000000 chip=0x14631022 rev=0x00
nvme0@pci0:1:0:0:\tclass=0x010802 card=0x00000000 chip=0x53472646 rev=0x01
none1@pci0:2:0:0:\tclass=0x088000 card=0x00000000 chip=0x54321234 rev=0x00
";
        let all = parse_pciconf(out, &[]);
        assert_eq!(all.len(), 3);
        assert!(all.contains("pci0:1:0:0"));
        assert!(all.contains("pci0:2:0:0"));

        // ignore a whole slot by substring
        let filtered = parse_pciconf(out, &["pci0:2:0".to_string()]);
        assert_eq!(filtered.len(), 2);
        assert!(!filtered.contains("pci0:2:0:0"));
    }

    #[test]
    fn test_is_ignored() {
        let ig = vec!["0000:01:00".to_string()];
        assert!(is_ignored("0000:01:00.0", &ig));
        assert!(is_ignored("0000:01:00.1", &ig));
        assert!(!is_ignored("0000:02:00.0", &ig));
        // empty token never matches
        assert!(!is_ignored("0000:01:00.0", &[String::new()]));
    }
}
