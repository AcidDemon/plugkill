use crate::error::Error;
use log::warn;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;

/// A unique identifier for a USB device (vendor:product).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UsbDeviceId {
    pub vendor_id: String,
    pub product_id: String,
}

impl fmt::Display for UsbDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.vendor_id, self.product_id)
    }
}

/// Snapshot of all currently connected USB devices with counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    devices: HashMap<UsbDeviceId, u32>,
}

/// What kind of change was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceChange {
    /// A new device was connected that is not in the allowed set.
    Added(UsbDeviceId),
    /// A device that was present at startup was removed.
    Removed(UsbDeviceId),
    /// Device count increased beyond allowed limit.
    CountIncreased {
        device: UsbDeviceId,
        expected: u32,
        actual: u32,
    },
    /// Device count decreased from startup baseline.
    CountDecreased {
        device: UsbDeviceId,
        expected: u32,
        actual: u32,
    },
}

impl fmt::Display for DeviceChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceChange::Added(id) => write!(f, "unauthorized device added: {id}"),
            DeviceChange::Removed(id) => write!(f, "device removed: {id}"),
            DeviceChange::CountIncreased {
                device,
                expected,
                actual,
            } => write!(
                f,
                "device count increased for {device}: expected <={expected}, got {actual}"
            ),
            DeviceChange::CountDecreased {
                device,
                expected,
                actual,
            } => write!(
                f,
                "device count decreased for {device}: expected {expected}, got {actual}"
            ),
        }
    }
}

impl DeviceSnapshot {
    /// Create an empty snapshot.
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
        }
    }

    /// Create a snapshot from a HashMap (for testing and whitelist construction).
    pub fn from_map(devices: HashMap<UsbDeviceId, u32>) -> Self {
        Self { devices }
    }

    /// Get the device map for iteration.
    pub fn devices(&self) -> &HashMap<UsbDeviceId, u32> {
        &self.devices
    }

    /// Get the count for a specific device, defaulting to 0.
    pub fn count_of(&self, id: &UsbDeviceId) -> u32 {
        self.devices.get(id).copied().unwrap_or(0)
    }

    /// Total number of unique device IDs.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Check current snapshot against a baseline + whitelist.
    /// Returns None if no unauthorized changes, Some(change) if violation detected.
    ///
    /// Detection rules (matching the original Python logic):
    /// 1. Any device in current not in baseline AND not in whitelist => Added violation
    /// 2. Any device in current with count > (baseline_count + whitelist_count) => CountIncreased
    /// 3. Any device in baseline not in current => Removed violation
    /// 4. Any device in baseline with count decreased in current => CountDecreased
    pub fn detect_changes(
        &self,
        baseline: &DeviceSnapshot,
        whitelist: &DeviceSnapshot,
    ) -> Option<DeviceChange> {
        // Check 1 & 2: Look for added or over-count devices
        for (device, &current_count) in &self.devices {
            let baseline_count = baseline.count_of(device);
            let whitelist_count = whitelist.count_of(device);
            let allowed_count = baseline_count.saturating_add(whitelist_count);

            if baseline_count == 0 && whitelist_count == 0 {
                // Device not in baseline or whitelist at all => unauthorized addition
                return Some(DeviceChange::Added(device.clone()));
            }

            if current_count > allowed_count {
                return Some(DeviceChange::CountIncreased {
                    device: device.clone(),
                    expected: allowed_count,
                    actual: current_count,
                });
            }
        }

        // Check 3 & 4: Look for removed or decreased-count baseline devices
        for (device, &baseline_count) in baseline.devices() {
            let current_count = self.count_of(device);

            if current_count == 0 {
                // Device was present at startup but is now completely gone
                return Some(DeviceChange::Removed(device.clone()));
            }

            if current_count < baseline_count {
                return Some(DeviceChange::CountDecreased {
                    device: device.clone(),
                    expected: baseline_count,
                    actual: current_count,
                });
            }
        }

        None
    }
}

/// Validate that a string is a valid hex ID (1-4 hex chars).
fn is_valid_hex_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 4 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Read a sysfs attribute file, returning trimmed contents.
/// Returns None if the file doesn't exist (normal for interfaces/hubs).
fn read_sysfs_attr(path: &Path) -> Result<Option<String>, Error> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(Error::Usb(format!(
                "permission denied reading {}: {}",
                path.display(),
                e
            )))
        }
        Err(e) => {
            warn!("unexpected error reading {}: {}", path.display(), e);
            Ok(None)
        }
    }
}

/// Enumerate all currently connected USB devices by reading sysfs.
pub fn enumerate_devices() -> Result<DeviceSnapshot, Error> {
    enumerate_devices_from(Path::new("/sys/bus/usb/devices"))
}

/// Enumerate USB devices from a custom sysfs root (for testing).
pub fn enumerate_devices_from(sysfs_root: &Path) -> Result<DeviceSnapshot, Error> {
    let entries = fs::read_dir(sysfs_root).map_err(|e| {
        Error::Usb(format!(
            "cannot read USB sysfs directory {}: {}",
            sysfs_root.display(),
            e
        ))
    })?;

    let mut devices: HashMap<UsbDeviceId, u32> = HashMap::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("error reading sysfs directory entry: {e}");
                continue;
            }
        };

        let dev_path = entry.path();

        // Read idVendor and idProduct — skip if either is missing
        let vendor_id = match read_sysfs_attr(&dev_path.join("idVendor"))? {
            Some(v) => v,
            None => continue,
        };
        let product_id = match read_sysfs_attr(&dev_path.join("idProduct"))? {
            Some(p) => p,
            None => continue,
        };

        // Validate hex format
        if !is_valid_hex_id(&vendor_id) {
            warn!(
                "invalid vendor ID '{}' in {}",
                vendor_id,
                dev_path.display()
            );
            continue;
        }
        if !is_valid_hex_id(&product_id) {
            warn!(
                "invalid product ID '{}' in {}",
                product_id,
                dev_path.display()
            );
            continue;
        }

        let id = UsbDeviceId {
            vendor_id,
            product_id,
        };
        *devices.entry(id).or_insert(0) += 1;
    }

    Ok(DeviceSnapshot { devices })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(vendor: &str, product: &str) -> UsbDeviceId {
        UsbDeviceId {
            vendor_id: vendor.to_string(),
            product_id: product.to_string(),
        }
    }

    fn snapshot(entries: &[(&str, &str, u32)]) -> DeviceSnapshot {
        let mut map = HashMap::new();
        for &(v, p, count) in entries {
            map.insert(id(v, p), count);
        }
        DeviceSnapshot::from_map(map)
    }

    #[test]
    fn test_no_change() {
        let baseline = snapshot(&[("1234", "5678", 1), ("abcd", "ef01", 2)]);
        let current = snapshot(&[("1234", "5678", 1), ("abcd", "ef01", 2)]);
        let whitelist = DeviceSnapshot::new();

        assert_eq!(current.detect_changes(&baseline, &whitelist), None);
    }

    #[test]
    fn test_added_device() {
        let baseline = snapshot(&[("1234", "5678", 1)]);
        let current = snapshot(&[("1234", "5678", 1), ("dead", "beef", 1)]);
        let whitelist = DeviceSnapshot::new();

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(change, Some(DeviceChange::Added(ref d)) if *d == id("dead", "beef")));
    }

    #[test]
    fn test_added_device_whitelisted() {
        let baseline = snapshot(&[("1234", "5678", 1)]);
        let current = snapshot(&[("1234", "5678", 1), ("dead", "beef", 1)]);
        let whitelist = snapshot(&[("dead", "beef", 1)]);

        assert_eq!(current.detect_changes(&baseline, &whitelist), None);
    }

    #[test]
    fn test_removed_device() {
        let baseline = snapshot(&[("1234", "5678", 1), ("abcd", "ef01", 1)]);
        let current = snapshot(&[("1234", "5678", 1)]);
        let whitelist = DeviceSnapshot::new();

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(change, Some(DeviceChange::Removed(ref d)) if *d == id("abcd", "ef01")));
    }

    #[test]
    fn test_count_increased() {
        let baseline = snapshot(&[("1234", "5678", 1)]);
        let current = snapshot(&[("1234", "5678", 3)]);
        let whitelist = DeviceSnapshot::new();

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(
            change,
            Some(DeviceChange::CountIncreased {
                expected: 1,
                actual: 3,
                ..
            })
        ));
    }

    #[test]
    fn test_count_increased_within_whitelist() {
        let baseline = snapshot(&[("1234", "5678", 1)]);
        let current = snapshot(&[("1234", "5678", 3)]);
        let whitelist = snapshot(&[("1234", "5678", 2)]);

        // baseline(1) + whitelist(2) = 3, current = 3 => OK
        assert_eq!(current.detect_changes(&baseline, &whitelist), None);
    }

    #[test]
    fn test_count_decreased() {
        let baseline = snapshot(&[("1234", "5678", 3)]);
        let current = snapshot(&[("1234", "5678", 1)]);
        let whitelist = DeviceSnapshot::new();

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(
            change,
            Some(DeviceChange::CountDecreased {
                expected: 3,
                actual: 1,
                ..
            })
        ));
    }

    #[test]
    fn test_empty_baseline_and_current() {
        let baseline = DeviceSnapshot::new();
        let current = DeviceSnapshot::new();
        let whitelist = DeviceSnapshot::new();

        assert_eq!(current.detect_changes(&baseline, &whitelist), None);
    }

    #[test]
    fn test_whitelisted_device_over_limit() {
        let baseline = DeviceSnapshot::new();
        let current = snapshot(&[("dead", "beef", 3)]);
        let whitelist = snapshot(&[("dead", "beef", 2)]);

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(
            change,
            Some(DeviceChange::CountIncreased {
                expected: 2,
                actual: 3,
                ..
            })
        ));
    }

    #[test]
    fn test_valid_hex_id() {
        assert!(is_valid_hex_id("1234"));
        assert!(is_valid_hex_id("abcd"));
        assert!(is_valid_hex_id("ABCD"));
        assert!(is_valid_hex_id("0"));
        assert!(!is_valid_hex_id(""));
        assert!(!is_valid_hex_id("12345"));
        assert!(!is_valid_hex_id("ghij"));
        assert!(!is_valid_hex_id("12 34"));
    }

    #[test]
    fn test_enumerate_from_mock_sysfs() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a mock USB device directory
        let dev1 = dir.path().join("1-1");
        fs::create_dir(&dev1).unwrap();
        let mut f = fs::File::create(dev1.join("idVendor")).unwrap();
        write!(f, "1d6b\n").unwrap();
        let mut f = fs::File::create(dev1.join("idProduct")).unwrap();
        write!(f, "0002\n").unwrap();

        // Create another device with same ID (tests counting)
        let dev2 = dir.path().join("2-1");
        fs::create_dir(&dev2).unwrap();
        let mut f = fs::File::create(dev2.join("idVendor")).unwrap();
        write!(f, "1d6b\n").unwrap();
        let mut f = fs::File::create(dev2.join("idProduct")).unwrap();
        write!(f, "0002\n").unwrap();

        // Create an interface entry (no idVendor/idProduct — should be skipped)
        let iface = dir.path().join("1-1:1.0");
        fs::create_dir(&iface).unwrap();

        let snapshot = enumerate_devices_from(dir.path()).unwrap();
        assert_eq!(snapshot.count_of(&id("1d6b", "0002")), 2);
        assert_eq!(snapshot.len(), 1);
    }

    #[test]
    fn test_enumerate_nonexistent_dir() {
        let result = enumerate_devices_from(Path::new("/nonexistent/sysfs/path"));
        assert!(result.is_err());
    }
}
