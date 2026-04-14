use crate::error::Error;
use crate::sysfs::read_sysfs_attr;
use log::warn;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;

/// A unique identifier for an SD/MMC card (by serial number).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SdCardDeviceId {
    pub serial: String,
}

impl fmt::Display for SdCardDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.serial)
    }
}

/// Extended SD/MMC card information for display purposes.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SdCardDeviceInfo {
    pub serial: String,
    pub name: Option<String>,
    pub card_type: Option<String>,
    pub cid: Option<String>,
    pub manfid: Option<String>,
    pub oemid: Option<String>,
    pub hwrev: Option<String>,
    pub fwrev: Option<String>,
    pub date: Option<String>,
}

/// Snapshot of all currently connected SD/MMC cards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdCardSnapshot {
    devices: HashMap<SdCardDeviceId, u32>,
}

/// What kind of SD card change was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdCardChange {
    Added(SdCardDeviceId),
    Removed(SdCardDeviceId),
}

impl SdCardChange {
    /// Extract the device ID from any change variant.
    pub fn device_id(&self) -> &SdCardDeviceId {
        match self {
            SdCardChange::Added(id) | SdCardChange::Removed(id) => id,
        }
    }
}

impl fmt::Display for SdCardChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SdCardChange::Added(id) => write!(f, "unauthorized SD card added: {id}"),
            SdCardChange::Removed(id) => write!(f, "SD card removed: {id}"),
        }
    }
}

impl SdCardSnapshot {
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
        }
    }

    pub fn from_map(devices: HashMap<SdCardDeviceId, u32>) -> Self {
        Self { devices }
    }

    pub fn devices(&self) -> &HashMap<SdCardDeviceId, u32> {
        &self.devices
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Check current snapshot against a baseline + whitelist.
    /// Returns None if no unauthorized changes, Some(change) if violation detected.
    pub fn detect_changes(
        &self,
        baseline: &SdCardSnapshot,
        whitelist: &SdCardSnapshot,
    ) -> Option<SdCardChange> {
        // Check for added devices not in baseline or whitelist
        for device in self.devices.keys() {
            if !baseline.devices.contains_key(device) && !whitelist.devices.contains_key(device) {
                return Some(SdCardChange::Added(device.clone()));
            }
        }

        // Check for removed baseline devices
        for device in baseline.devices.keys() {
            if !self.devices.contains_key(device) {
                return Some(SdCardChange::Removed(device.clone()));
            }
        }

        None
    }
}

/// Returns true if this sysfs entry name represents an MMC card device.
/// MMC card entries look like "mmc0:0001" (host:address).
fn is_mmc_card(name: &str) -> bool {
    name.starts_with("mmc") && name.contains(':')
}

/// Enumerate all currently connected SD/MMC cards by reading sysfs.
pub fn enumerate_sdcard_devices() -> Result<SdCardSnapshot, Error> {
    enumerate_sdcard_devices_from(Path::new("/sys/bus/mmc/devices"))
}

/// Enumerate SD/MMC cards from a custom sysfs root (for testing).
pub fn enumerate_sdcard_devices_from(sysfs_root: &Path) -> Result<SdCardSnapshot, Error> {
    let entries = fs::read_dir(sysfs_root).map_err(|e| {
        Error::SdCard(format!(
            "cannot read MMC sysfs directory {}: {}",
            sysfs_root.display(),
            e
        ))
    })?;

    let mut devices: HashMap<SdCardDeviceId, u32> = HashMap::new();

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

        if !is_mmc_card(&name_str) {
            continue;
        }

        let dev_path = entry.path();

        // Must have a type attribute to be a real card
        if read_sysfs_attr(&dev_path.join("type"))?.is_none() {
            continue;
        }

        // Must have a serial attribute
        let serial = match read_sysfs_attr(&dev_path.join("serial"))? {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };

        let id = SdCardDeviceId { serial };
        devices.insert(id, 1);
    }

    Ok(SdCardSnapshot { devices })
}

/// Enumerate all connected SD/MMC cards with extended info for display.
pub fn enumerate_sdcard_devices_detailed() -> Result<Vec<SdCardDeviceInfo>, Error> {
    enumerate_sdcard_devices_detailed_from(Path::new("/sys/bus/mmc/devices"))
}

/// Enumerate SD/MMC cards with extended info from a custom sysfs root (for testing).
pub fn enumerate_sdcard_devices_detailed_from(
    sysfs_root: &Path,
) -> Result<Vec<SdCardDeviceInfo>, Error> {
    let entries = fs::read_dir(sysfs_root).map_err(|e| {
        Error::SdCard(format!(
            "cannot read MMC sysfs directory {}: {}",
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

        if !is_mmc_card(&name_str) {
            continue;
        }

        let dev_path = entry.path();

        // Must have a type attribute
        let card_type = read_sysfs_attr(&dev_path.join("type"))?;
        if card_type.is_none() {
            continue;
        }

        let serial = match read_sysfs_attr(&dev_path.join("serial"))? {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };

        let read_optional = |attr: &str| -> Result<Option<String>, Error> {
            Ok(read_sysfs_attr(&dev_path.join(attr))?.filter(|s| !s.is_empty()))
        };

        devices.push(SdCardDeviceInfo {
            serial,
            name: read_optional("name")?,
            card_type,
            cid: read_optional("cid")?,
            manfid: read_optional("manfid")?,
            oemid: read_optional("oemid")?,
            hwrev: read_optional("hwrev")?,
            fwrev: read_optional("fwrev")?,
            date: read_optional("date")?,
        });
    }

    devices.sort_by(|a, b| a.serial.cmp(&b.serial));

    Ok(devices)
}

/// Print a formatted list of SD/MMC cards to stdout.
pub fn print_sdcard_device_list(
    devices: &[SdCardDeviceInfo],
    whitelist: Option<&HashMap<String, ()>>,
) {
    println!();
    println!("Connected SD/MMC cards ({} found):", devices.len());

    for dev in devices {
        let card_name = dev.name.as_deref().unwrap_or("Unknown card");
        let card_type = dev.card_type.as_deref().unwrap_or("?");

        println!();
        println!("  {card_name} ({card_type}) serial: {}", dev.serial);

        if let Some(ref cid) = dev.cid {
            println!("    CID:    {cid}");
        }
        if let Some(ref manfid) = dev.manfid {
            println!("    Manfid: {manfid}");
        }
        if let Some(ref oemid) = dev.oemid {
            println!("    OEM ID: {oemid}");
        }
        if let Some(ref date) = dev.date {
            println!("    Date:   {date}");
        }
    }

    // Summary
    println!();
    println!("SD/MMC card summary (for whitelist configuration):");
    for dev in devices {
        let name = dev.name.as_deref().unwrap_or("Unknown card");
        let card_type = dev.card_type.as_deref().unwrap_or("?");
        let annotation = match whitelist {
            Some(wl) => {
                if wl.contains_key(&dev.serial) {
                    " [whitelisted]"
                } else {
                    " [NOT whitelisted]"
                }
            }
            None => "",
        };
        println!(
            "  serial: {}  ({name}, {card_type}){annotation}",
            dev.serial
        );
    }
}

/// Generate a ready-to-paste TOML `[sdcard_whitelist]` block from connected cards.
pub fn generate_sdcard_whitelist_toml(devices: &[SdCardDeviceInfo]) -> String {
    let mut out = String::from("\n[sdcard_whitelist]\ndevices = [\n");
    for dev in devices {
        let name = dev.name.as_deref().unwrap_or("Unknown card");
        let card_type = dev.card_type.as_deref().unwrap_or("?");
        out.push_str(&format!(
            "    {{ serial = \"{}\" }},  # {name} ({card_type})\n",
            dev.serial
        ));
    }
    out.push_str("]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sd_id(serial: &str) -> SdCardDeviceId {
        SdCardDeviceId {
            serial: serial.to_string(),
        }
    }

    fn sd_snapshot(serials: &[&str]) -> SdCardSnapshot {
        let mut map = HashMap::new();
        for &s in serials {
            map.insert(sd_id(s), 1);
        }
        SdCardSnapshot::from_map(map)
    }

    #[test]
    fn test_no_change() {
        let baseline = sd_snapshot(&["0x12345678", "0xabcdef01"]);
        let current = sd_snapshot(&["0x12345678", "0xabcdef01"]);
        let whitelist = SdCardSnapshot::new();

        assert_eq!(current.detect_changes(&baseline, &whitelist), None);
    }

    #[test]
    fn test_added_device() {
        let baseline = sd_snapshot(&["0x12345678"]);
        let current = sd_snapshot(&["0x12345678", "0xdeadbeef"]);
        let whitelist = SdCardSnapshot::new();

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(change, Some(SdCardChange::Added(ref d)) if d.serial == "0xdeadbeef"));
    }

    #[test]
    fn test_removed_device() {
        let baseline = sd_snapshot(&["0x12345678", "0xabcdef01"]);
        let current = sd_snapshot(&["0x12345678"]);
        let whitelist = SdCardSnapshot::new();

        let change = current.detect_changes(&baseline, &whitelist);
        assert!(matches!(change, Some(SdCardChange::Removed(ref d)) if d.serial == "0xabcdef01"));
    }

    #[test]
    fn test_added_device_whitelisted() {
        let baseline = sd_snapshot(&["0x12345678"]);
        let current = sd_snapshot(&["0x12345678", "0xdeadbeef"]);
        let whitelist = sd_snapshot(&["0xdeadbeef"]);

        assert_eq!(current.detect_changes(&baseline, &whitelist), None);
    }

    #[test]
    fn test_enumerate_from_mock_sysfs() {
        let dir = tempfile::tempdir().unwrap();

        // Create a real MMC card entry (mmc0:0001)
        let dev1 = dir.path().join("mmc0:0001");
        fs::create_dir(&dev1).unwrap();
        let mut f = fs::File::create(dev1.join("type")).unwrap();
        writeln!(f, "SD").unwrap();
        let mut f = fs::File::create(dev1.join("serial")).unwrap();
        writeln!(f, "0x12345678").unwrap();

        // Create another card
        let dev2 = dir.path().join("mmc1:0001");
        fs::create_dir(&dev2).unwrap();
        let mut f = fs::File::create(dev2.join("type")).unwrap();
        writeln!(f, "MMC").unwrap();
        let mut f = fs::File::create(dev2.join("serial")).unwrap();
        writeln!(f, "0xabcdef01").unwrap();

        // Create an entry without type attr (should be skipped)
        let notype = dir.path().join("mmc2:0001");
        fs::create_dir(&notype).unwrap();
        let mut f = fs::File::create(notype.join("serial")).unwrap();
        writeln!(f, "0x99999999").unwrap();

        // Create non-mmc entry (should be skipped)
        let other = dir.path().join("something-else");
        fs::create_dir(&other).unwrap();

        let snapshot = enumerate_sdcard_devices_from(dir.path()).unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.devices().contains_key(&sd_id("0x12345678")));
        assert!(snapshot.devices().contains_key(&sd_id("0xabcdef01")));
    }

    #[test]
    fn test_skip_non_card_entries() {
        assert!(is_mmc_card("mmc0:0001"));
        assert!(is_mmc_card("mmc1:0001"));
        assert!(!is_mmc_card("something-else"));
        assert!(!is_mmc_card("mmc0"));
        assert!(!is_mmc_card("usb1-1"));
    }

    #[test]
    fn test_enumerate_detailed_from_mock_sysfs() {
        let dir = tempfile::tempdir().unwrap();

        let dev = dir.path().join("mmc0:0001");
        fs::create_dir(&dev).unwrap();
        for (name, val) in [
            ("type", "SD"),
            ("serial", "0x12345678"),
            ("name", "SD32G"),
            ("cid", "03534453333247800123456700015600"),
            ("manfid", "0x000003"),
            ("oemid", "0x5344"),
            ("hwrev", "0x8"),
            ("fwrev", "0x0"),
            ("date", "01/2024"),
        ] {
            let mut f = fs::File::create(dev.join(name)).unwrap();
            writeln!(f, "{val}").unwrap();
        }

        let devices = enumerate_sdcard_devices_detailed_from(dir.path()).unwrap();
        assert_eq!(devices.len(), 1);
        let d = &devices[0];
        assert_eq!(d.serial, "0x12345678");
        assert_eq!(d.name.as_deref(), Some("SD32G"));
        assert_eq!(d.card_type.as_deref(), Some("SD"));
        assert_eq!(d.manfid.as_deref(), Some("0x000003"));
        assert_eq!(d.date.as_deref(), Some("01/2024"));
    }

    #[test]
    fn test_enumerate_nonexistent_dir() {
        let result = enumerate_sdcard_devices_from(Path::new("/nonexistent/sysfs/path"));
        assert!(result.is_err());
    }
}
