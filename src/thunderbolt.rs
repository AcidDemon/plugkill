use crate::error::Error;
use crate::usb::read_sysfs_attr;
use log::warn;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;

/// A unique identifier for a Thunderbolt device (by unique_id UUID).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThunderboltDeviceId {
    pub unique_id: String,
}

impl fmt::Display for ThunderboltDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.unique_id)
    }
}

/// Extended Thunderbolt device information for display purposes.
#[derive(Debug, Clone)]
pub struct ThunderboltDeviceInfo {
    pub unique_id: String,
    pub vendor_id: String,
    pub device_id: String,
    pub vendor_name: Option<String>,
    pub device_name: Option<String>,
    pub authorized: Option<String>,
    pub generation: Option<String>,
}

/// Snapshot of all currently connected Thunderbolt devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThunderboltSnapshot {
    devices: HashMap<ThunderboltDeviceId, u32>,
}

/// What kind of Thunderbolt change was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThunderboltChange {
    Added(ThunderboltDeviceId),
    Removed(ThunderboltDeviceId),
}

impl fmt::Display for ThunderboltChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThunderboltChange::Added(id) => write!(f, "unauthorized thunderbolt device added: {id}"),
            ThunderboltChange::Removed(id) => write!(f, "thunderbolt device removed: {id}"),
        }
    }
}

impl ThunderboltSnapshot {
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
        }
    }

    pub fn from_map(devices: HashMap<ThunderboltDeviceId, u32>) -> Self {
        Self { devices }
    }

    pub fn devices(&self) -> &HashMap<ThunderboltDeviceId, u32> {
        &self.devices
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Check current snapshot against a baseline + whitelist.
    /// Returns None if no unauthorized changes, Some(change) if violation detected.
    ///
    /// Simpler than USB: no count logic, just presence/absence by unique_id.
    pub fn detect_changes(
        &self,
        baseline: &ThunderboltSnapshot,
        whitelist: &ThunderboltSnapshot,
    ) -> Option<ThunderboltChange> {
        // Check for added devices not in baseline or whitelist
        for device in self.devices.keys() {
            if !baseline.devices.contains_key(device) && !whitelist.devices.contains_key(device) {
                return Some(ThunderboltChange::Added(device.clone()));
            }
        }

        // Check for removed baseline devices
        for device in baseline.devices.keys() {
            if !self.devices.contains_key(device) {
                return Some(ThunderboltChange::Removed(device.clone()));
            }
        }

        None
    }
}

/// Returns true if this sysfs entry name represents a real Thunderbolt device.
/// Real devices match pattern like "0-0", "0-1", "1-3", etc.
/// Skip domains (domain0), interfaces (0-0:1.1), and ports (usb4_port*).
fn is_real_device(name: &str) -> bool {
    if name.starts_with("domain") || name.contains(':') || name.starts_with("usb4_port") {
        return false;
    }
    // Must match digit(s)-digit(s) pattern
    let parts: Vec<&str> = name.splitn(2, '-').collect();
    parts.len() == 2
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].chars().all(|c| c.is_ascii_digit())
}

/// Enumerate all currently connected Thunderbolt devices by reading sysfs.
pub fn enumerate_thunderbolt_devices() -> Result<ThunderboltSnapshot, Error> {
    enumerate_thunderbolt_devices_from(Path::new("/sys/bus/thunderbolt/devices"))
}

/// Enumerate Thunderbolt devices from a custom sysfs root (for testing).
pub fn enumerate_thunderbolt_devices_from(sysfs_root: &Path) -> Result<ThunderboltSnapshot, Error> {
    let entries = fs::read_dir(sysfs_root).map_err(|e| {
        Error::Thunderbolt(format!(
            "cannot read Thunderbolt sysfs directory {}: {}",
            sysfs_root.display(),
            e
        ))
    })?;

    let mut devices: HashMap<ThunderboltDeviceId, u32> = HashMap::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("error reading sysfs directory entry: {e}");
                continue;
            }
        };

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !is_real_device(&name_str) {
            continue;
        }

        let dev_path = entry.path();

        // Must have unique_id attribute to be a real device
        let unique_id = match read_sysfs_attr(&dev_path.join("unique_id"))? {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };

        let id = ThunderboltDeviceId { unique_id };
        devices.insert(id, 1);
    }

    Ok(ThunderboltSnapshot { devices })
}

/// Enumerate all connected Thunderbolt devices with extended info for display.
pub fn enumerate_thunderbolt_devices_detailed() -> Result<Vec<ThunderboltDeviceInfo>, Error> {
    enumerate_thunderbolt_devices_detailed_from(Path::new("/sys/bus/thunderbolt/devices"))
}

/// Enumerate Thunderbolt devices with extended info from a custom sysfs root (for testing).
pub fn enumerate_thunderbolt_devices_detailed_from(
    sysfs_root: &Path,
) -> Result<Vec<ThunderboltDeviceInfo>, Error> {
    let entries = fs::read_dir(sysfs_root).map_err(|e| {
        Error::Thunderbolt(format!(
            "cannot read Thunderbolt sysfs directory {}: {}",
            sysfs_root.display(),
            e
        ))
    })?;

    let mut devices = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("error reading sysfs directory entry: {e}");
                continue;
            }
        };

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !is_real_device(&name_str) {
            continue;
        }

        let dev_path = entry.path();

        let unique_id = match read_sysfs_attr(&dev_path.join("unique_id"))? {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };

        let read_optional =
            |attr: &str| -> Result<Option<String>, Error> {
                Ok(read_sysfs_attr(&dev_path.join(attr))?.filter(|s| !s.is_empty()))
            };

        let vendor_id = read_optional("vendor")?.unwrap_or_default();
        let device_id = read_optional("device")?.unwrap_or_default();

        devices.push(ThunderboltDeviceInfo {
            unique_id,
            vendor_id,
            device_id,
            vendor_name: read_optional("vendor_name")?,
            device_name: read_optional("device_name")?,
            authorized: read_optional("authorized")?,
            generation: read_optional("generation")?,
        });
    }

    devices.sort_by(|a, b| a.unique_id.cmp(&b.unique_id));

    Ok(devices)
}

/// Print a formatted list of Thunderbolt devices to stdout.
pub fn print_thunderbolt_device_list(
    devices: &[ThunderboltDeviceInfo],
    whitelist: Option<&HashMap<String, ()>>,
) {
    println!();
    println!(
        "Connected Thunderbolt devices ({} found):",
        devices.len()
    );

    for dev in devices {
        let device_name = dev.device_name.as_deref().unwrap_or("Unknown device");
        let auth_status = match dev.authorized.as_deref() {
            Some("0") => " [not authorized]",
            Some("1") => " [authorized]",
            Some("2") => " [authorized (secure)]",
            _ => "",
        };

        println!();
        println!(
            "  ID {}:{} {} (unique_id: {}){}",
            dev.vendor_id, dev.device_id, device_name, dev.unique_id, auth_status
        );

        if let Some(ref vendor) = dev.vendor_name {
            println!("    Vendor:     {vendor}");
        }
        if let Some(ref gen) = dev.generation {
            let gen_label = match gen.as_str() {
                "1" => "Thunderbolt 1",
                "2" => "Thunderbolt 2",
                "3" => "Thunderbolt 3",
                "4" => "USB4/Thunderbolt 4",
                other => other,
            };
            println!("    Generation: {gen_label}");
        }
    }

    // Summary
    println!();
    println!("Thunderbolt device summary (for whitelist configuration):");
    for dev in devices {
        let name = dev.device_name.as_deref().unwrap_or("Unknown device");
        let annotation = match whitelist {
            Some(wl) => {
                if wl.contains_key(&dev.unique_id) {
                    " [whitelisted]"
                } else {
                    " [NOT whitelisted]"
                }
            }
            None => "",
        };
        println!("  unique_id: {}  ({name}){annotation}", dev.unique_id);
    }
}

/// Generate a ready-to-paste TOML `[thunderbolt_whitelist]` block from connected devices.
pub fn generate_thunderbolt_whitelist_toml(devices: &[ThunderboltDeviceInfo]) -> String {
    let mut out = String::from("\n[thunderbolt_whitelist]\ndevices = [\n");
    for dev in devices {
        let name = dev.device_name.as_deref().unwrap_or("Unknown device");
        out.push_str(&format!(
            "    {{ unique_id = \"{}\" }},  # {name}\n",
            dev.unique_id
        ));
    }
    out.push_str("]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tb_id(uid: &str) -> ThunderboltDeviceId {
        ThunderboltDeviceId {
            unique_id: uid.to_string(),
        }
    }

    fn tb_snapshot(ids: &[&str]) -> ThunderboltSnapshot {
        let mut map = HashMap::new();
        for &uid in ids {
            map.insert(tb_id(uid), 1);
        }
        ThunderboltSnapshot::from_map(map)
    }

    #[test]
    fn test_no_change() {
        let baseline = tb_snapshot(&["uuid-aaa", "uuid-bbb"]);
        let current = tb_snapshot(&["uuid-aaa", "uuid-bbb"]);
        let whitelist = ThunderboltSnapshot::new();

        assert_eq!(current.detect_changes(&baseline, &whitelist), None);
    }

    #[test]
    fn test_added_device() {
        let baseline = tb_snapshot(&["uuid-aaa"]);
        let current = tb_snapshot(&["uuid-aaa", "uuid-new"]);
        let whitelist = ThunderboltSnapshot::new();

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(change, Some(ThunderboltChange::Added(ref d)) if d.unique_id == "uuid-new"));
    }

    #[test]
    fn test_removed_device() {
        let baseline = tb_snapshot(&["uuid-aaa", "uuid-bbb"]);
        let current = tb_snapshot(&["uuid-aaa"]);
        let whitelist = ThunderboltSnapshot::new();

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(change, Some(ThunderboltChange::Removed(ref d)) if d.unique_id == "uuid-bbb"));
    }

    #[test]
    fn test_added_device_whitelisted() {
        let baseline = tb_snapshot(&["uuid-aaa"]);
        let current = tb_snapshot(&["uuid-aaa", "uuid-new"]);
        let whitelist = tb_snapshot(&["uuid-new"]);

        assert_eq!(current.detect_changes(&baseline, &whitelist), None);
    }

    #[test]
    fn test_enumerate_from_mock_sysfs() {
        let dir = tempfile::tempdir().unwrap();

        // Create a real Thunderbolt device directory (0-1)
        let dev1 = dir.path().join("0-1");
        fs::create_dir(&dev1).unwrap();
        let mut f = fs::File::create(dev1.join("unique_id")).unwrap();
        writeln!(f, "some-uuid-1234").unwrap();
        let mut f = fs::File::create(dev1.join("vendor")).unwrap();
        writeln!(f, "0x8087").unwrap();
        let mut f = fs::File::create(dev1.join("device")).unwrap();
        writeln!(f, "0x0b27").unwrap();

        // Create host controller (0-0)
        let dev0 = dir.path().join("0-0");
        fs::create_dir(&dev0).unwrap();
        let mut f = fs::File::create(dev0.join("unique_id")).unwrap();
        writeln!(f, "host-uuid-0000").unwrap();

        // Create a domain entry (should be skipped)
        let domain = dir.path().join("domain0");
        fs::create_dir(&domain).unwrap();

        // Create an interface entry (should be skipped)
        let iface = dir.path().join("0-1:1.1");
        fs::create_dir(&iface).unwrap();

        // Create a usb4_port entry (should be skipped)
        let port = dir.path().join("usb4_port1");
        fs::create_dir(&port).unwrap();

        let snapshot = enumerate_thunderbolt_devices_from(dir.path()).unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.devices().contains_key(&tb_id("some-uuid-1234")));
        assert!(snapshot.devices().contains_key(&tb_id("host-uuid-0000")));
    }

    #[test]
    fn test_skip_domains_and_interfaces() {
        assert!(!is_real_device("domain0"));
        assert!(!is_real_device("domain1"));
        assert!(!is_real_device("0-0:1.1"));
        assert!(!is_real_device("0-1:3.1"));
        assert!(!is_real_device("usb4_port1"));
        assert!(!is_real_device("usb4_port2"));
        assert!(is_real_device("0-0"));
        assert!(is_real_device("0-1"));
        assert!(is_real_device("1-3"));
    }

    #[test]
    fn test_enumerate_detailed_from_mock_sysfs() {
        let dir = tempfile::tempdir().unwrap();

        let dev = dir.path().join("0-1");
        fs::create_dir(&dev).unwrap();
        for (name, val) in [
            ("unique_id", "test-uuid-5678"),
            ("vendor", "0x8087"),
            ("device", "0x0b27"),
            ("vendor_name", "Intel Corp."),
            ("device_name", "Thunderbolt Controller"),
            ("authorized", "1"),
            ("generation", "4"),
        ] {
            let mut f = fs::File::create(dev.join(name)).unwrap();
            writeln!(f, "{val}").unwrap();
        }

        let devices = enumerate_thunderbolt_devices_detailed_from(dir.path()).unwrap();
        assert_eq!(devices.len(), 1);
        let d = &devices[0];
        assert_eq!(d.unique_id, "test-uuid-5678");
        assert_eq!(d.vendor_id, "0x8087");
        assert_eq!(d.device_id, "0x0b27");
        assert_eq!(d.vendor_name.as_deref(), Some("Intel Corp."));
        assert_eq!(d.device_name.as_deref(), Some("Thunderbolt Controller"));
        assert_eq!(d.authorized.as_deref(), Some("1"));
        assert_eq!(d.generation.as_deref(), Some("4"));
    }

    #[test]
    fn test_enumerate_nonexistent_dir() {
        let result = enumerate_thunderbolt_devices_from(Path::new("/nonexistent/sysfs/path"));
        assert!(result.is_err());
    }
}
