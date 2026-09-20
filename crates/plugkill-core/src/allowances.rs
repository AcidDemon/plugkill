//! Runtime allowances: devices the daemon accepts for this uptime only.
//!
//! An allowance names one device on one bus, the same identity the config
//! whitelist for that bus already matches on, so the daemon can consult it at
//! the point it consults the config and there is no second acceptance path.
//! The table is memory only and never written to disk, so a restart is a clean
//! slate. Spec A1, A2, A5.
//!
//! Nothing here reads the clock. Matching and sweeping take `now` from the
//! caller, which keeps expiry testable without sleeping.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::edid::EdidId;
use crate::sdcard::SdCardDeviceId;
use crate::thunderbolt::ThunderboltDeviceId;
use crate::usb::UsbDeviceId;

/// The identity of one allowed device, per bus, exactly as that bus matches.
/// Power, network and lid are absent on purpose: they are events with nothing
/// to identify. Spec A2, A3.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeviceRef {
    Usb(UsbDeviceId),
    Thunderbolt(ThunderboltDeviceId),
    SdCard(SdCardDeviceId),
    /// PCI selector, for example `0000:01:00.0`.
    Pci(String),
    /// The monitor, never the connector it is plugged into. Spec A2a.
    Display(EdidId),
}

impl DeviceRef {
    /// Bus prefix, matching the keys in `ipc::BUSES` without `_watching`.
    pub fn bus(&self) -> &'static str {
        match self {
            Self::Usb(_) => "usb",
            Self::Thunderbolt(_) => "thunderbolt",
            Self::SdCard(_) => "sdcard",
            Self::Pci(_) => "pci",
            Self::Display(_) => "display",
        }
    }

    /// The selector form, `<bus>:<identity>`. Round trips through `parse`.
    pub fn selector(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for DeviceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usb(id) => write!(f, "usb:{id}"),
            Self::Thunderbolt(id) => write!(f, "thunderbolt:{id}"),
            Self::SdCard(id) => write!(f, "sdcard:{id}"),
            Self::Pci(sel) => write!(f, "pci:{sel}"),
            Self::Display(id) => write!(f, "display:{}", id.selector()),
        }
    }
}

/// `0000:01:00.0`: four hex digits, two, two, then a single function digit.
fn is_pci_selector(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && [0, 1, 2, 3, 5, 6, 8, 9, 11]
            .iter()
            .all(|&i| b[i].is_ascii_hexdigit())
}

/// A USB id as sysfs reports it: up to four hex digits, lowercase. Selectors
/// typed by a person may be uppercase, so normalise rather than refuse.
fn usb_hex(part: &str, what: &str) -> Result<String, String> {
    if part.is_empty() || part.len() > 4 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("'{part}' is not a USB {what} id"));
    }
    Ok(part.to_ascii_lowercase())
}

impl FromStr for DeviceRef {
    type Err = String;

    /// Parse a selector. The pci and display identities contain colons of their
    /// own, so the bus prefix is split off first and the rest is handed to that
    /// bus. Spec D3.
    fn from_str(s: &str) -> Result<Self, String> {
        let Some((bus, rest)) = s.split_once(':') else {
            return Err(format!("selector '{s}' is not <bus>:<identity>"));
        };
        match bus {
            "usb" => {
                let Some((v, p)) = rest.split_once(':') else {
                    return Err(format!("'{s}' is not usb:<vendor>:<product>"));
                };
                Ok(Self::Usb(UsbDeviceId {
                    vendor_id: usb_hex(v, "vendor")?,
                    product_id: usb_hex(p, "product")?,
                }))
            }
            "thunderbolt" if !rest.is_empty() => Ok(Self::Thunderbolt(ThunderboltDeviceId {
                unique_id: rest.to_string(),
            })),
            "sdcard" if !rest.is_empty() => Ok(Self::SdCard(SdCardDeviceId {
                serial: rest.to_string(),
            })),
            "pci" if is_pci_selector(rest) => Ok(Self::Pci(rest.to_string())),
            "pci" => Err(format!("'{rest}' is not a PCI selector like 0000:01:00.0")),
            "display" => parse_display(rest).map(Self::Display),
            "power" | "network" | "lid" => Err(format!(
                "{bus} violations are events rather than devices, so there is nothing to allow; \
                 use disarm for them"
            )),
            "thunderbolt" | "sdcard" => Err(format!("selector '{s}' has no identity")),
            other => Err(format!("unknown bus '{other}' in selector '{s}'")),
        }
    }
}

/// `SAM:772d:811021873`, or `SAM:772d` when the monitor reports serial 0.
/// The name is not part of identity, so a parsed id carries none. Spec A2a.
fn parse_display(rest: &str) -> Result<EdidId, String> {
    let mut parts = rest.split(':');
    let mfg = parts.next().unwrap_or_default();
    if mfg.len() != 3 || !mfg.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(format!("'{mfg}' is not a three letter manufacturer code"));
    }
    let product = parts
        .next()
        .and_then(|p| u16::from_str_radix(p, 16).ok())
        .ok_or_else(|| format!("'{rest}' has no hex product code"))?;
    let serial = match parts.next() {
        Some(s) => Some(
            s.parse::<u32>()
                .map_err(|_| format!("'{s}' is not a monitor serial"))?,
        ),
        None => None,
    };
    if parts.next().is_some() {
        return Err(format!("'{rest}' has more fields than a display identity"));
    }
    Ok(EdidId {
        mfg: mfg.to_string(),
        product,
        serial,
        name: String::new(),
    })
}

/// How an allowance came to exist. Spec A1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Admitted through a pairing window.
    Paired,
    /// Promoted from the last recorded violation.
    Promoted,
}

impl Grant {
    /// The wire form used in status responses. Spec D2.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Paired => "paired",
            Self::Promoted => "promoted",
        }
    }
}

impl fmt::Display for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One allowed device. No expiry by default: a timer running out on a kill
/// switch would power the machine off with the device still plugged in, and the
/// in memory table already bounds an allowance to one uptime. Spec C1, C2.
#[derive(Debug, Clone)]
pub struct Allowance {
    pub id: DeviceRef,
    pub expires_at: Option<Instant>,
    pub granted_by: Grant,
    pub granted_at: Instant,
    /// Friendly name when the daemon knew one when it granted this.
    pub name: Option<String>,
    /// Set once the approaching-expiry warning has been logged. Spec C2.
    pub warned: bool,
}

impl Allowance {
    pub fn new(id: DeviceRef, granted_by: Grant, now: Instant, ttl: Option<Duration>) -> Self {
        Self {
            id,
            expires_at: ttl.map(|d| now + d),
            granted_by,
            granted_at: now,
            name: None,
            warned: false,
        }
    }

    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
    }

    pub fn is_active(&self, now: Instant) -> bool {
        self.expires_at.is_none_or(|e| e > now)
    }

    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.granted_at)
    }

    pub fn expires_in(&self, now: Instant) -> Option<Duration> {
        self.expires_at.map(|e| e.saturating_duration_since(now))
    }
}

/// The daemon's allowance table.
// ponytail: a Vec, scanned linearly. A table holds a handful of entries; if it
// ever holds thousands, key it by DeviceRef in a HashMap.
#[derive(Debug, Default)]
pub struct AllowanceTable {
    entries: Vec<Allowance>,
}

impl AllowanceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one, replacing any existing allowance for the same device and
    /// returning what it replaced.
    pub fn add(&mut self, allowance: Allowance) -> Option<Allowance> {
        let old = self.remove(&allowance.id);
        self.entries.push(allowance);
        old
    }

    /// Drop the allowance for one device. Spec C6.
    pub fn remove(&mut self, id: &DeviceRef) -> Option<Allowance> {
        let at = self.entries.iter().position(|a| &a.id == id)?;
        Some(self.entries.remove(at))
    }

    /// Drop every allowance, returning how many went. Spec C6.
    pub fn clear(&mut self) -> usize {
        std::mem::take(&mut self.entries).len()
    }

    /// Drop expired entries and hand them back so the caller can log them.
    pub fn sweep(&mut self, now: Instant) -> Vec<Allowance> {
        let (keep, dropped) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|a| a.is_active(now));
        self.entries = keep;
        dropped
    }

    /// Every entry, expired ones included.
    pub fn iter(&self) -> impl Iterator<Item = &Allowance> {
        self.entries.iter()
    }

    /// Entries that have not expired at `now`.
    pub fn active(&self, now: Instant) -> impl Iterator<Item = &Allowance> {
        self.entries.iter().filter(move |a| a.is_active(now))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries whose expiry is within `ahead` and that have not been warned
    /// about yet, flagged as warned so each one warns once. Spec C2.
    pub fn due_to_warn(&mut self, now: Instant, ahead: Duration) -> Vec<(String, Duration)> {
        let mut due = Vec::new();
        for a in &mut self.entries {
            if let Some(left) = a.expires_in(now)
                && !a.warned
                && left <= ahead
            {
                a.warned = true;
                due.push((a.id.selector(), left));
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> DeviceRef {
        s.parse().expect("selector should parse")
    }

    /// What the daemon builds out of the table before it matches. Matching
    /// lives in the daemon, so the table is only asserted on what it hands out.
    fn active(table: &AllowanceTable, now: Instant) -> Vec<DeviceRef> {
        table.active(now).map(|a| a.id.clone()).collect()
    }

    fn edid(mfg: &str, product: u16, serial: Option<u32>) -> EdidId {
        EdidId {
            mfg: mfg.to_string(),
            product,
            serial,
            name: String::new(),
        }
    }

    #[test]
    fn test_selectors_round_trip_on_every_bus() {
        for s in [
            "usb:1d6b:0002",
            "thunderbolt:0040-0c08-c4c1-a7f3-0000-0000-0000-0000",
            "sdcard:0x0000ba5e",
            "pci:0000:01:00.0",
            "display:SAM:772d:811021873",
            "display:TMA:2064",
        ] {
            assert_eq!(r(s).selector(), s, "round trip of {s}");
        }
    }

    #[test]
    fn test_pci_selector_keeps_its_own_colons() {
        assert_eq!(r("pci:0000:01:00.0"), DeviceRef::Pci("0000:01:00.0".into()));
        assert_eq!(r("pci:0000:01:00.0").bus(), "pci");
    }

    #[test]
    fn test_display_without_serial_parses_and_prints_short() {
        let d = r("display:TMA:2064");
        assert_eq!(d, DeviceRef::Display(edid("TMA", 0x2064, None)));
        assert_eq!(d.selector(), "display:TMA:2064");
    }

    #[test]
    fn test_usb_ids_are_normalised_to_sysfs_case() {
        assert_eq!(r("usb:1D6B:00A2").selector(), "usb:1d6b:00a2");
    }

    /// Spec A3: these are events, not devices.
    #[test]
    fn test_event_buses_are_refused_and_point_at_disarm() {
        for bus in ["power", "network", "lid"] {
            let err = format!("{bus}:anything").parse::<DeviceRef>().unwrap_err();
            assert!(err.contains("events"), "{bus}: {err}");
            assert!(err.contains("disarm"), "{bus}: {err}");
        }
    }

    #[test]
    fn test_malformed_selectors_are_refused() {
        for s in [
            "",
            "usb",
            "usb:1d6b",
            "usb:zzzz:0002",
            "usb:1d6b:00002",
            "thunderbolt:",
            "sdcard:",
            "pci:01:00.0",
            "pci:0000:01:00.x",
            "display:SAMSUNG:772d",
            "display:SAM",
            "display:SAM:772d:811021873:extra",
            "display:SAM:772d:notaserial",
            "keyboard:a",
        ] {
            assert!(s.parse::<DeviceRef>().is_err(), "'{s}' should be refused");
        }
    }

    #[test]
    fn test_sweep_drops_expired_and_reports_them() {
        let t0 = Instant::now();
        let mut table = AllowanceTable::new();
        table.add(Allowance::new(
            r("usb:1d6b:0002"),
            Grant::Paired,
            t0,
            Some(Duration::from_secs(60)),
        ));
        table.add(Allowance::new(r("sdcard:abc"), Grant::Promoted, t0, None));

        let later = t0 + Duration::from_secs(61);
        assert!(active(&table, t0 + Duration::from_secs(59)).contains(&r("usb:1d6b:0002")));
        assert!(
            !active(&table, later).contains(&r("usb:1d6b:0002")),
            "expired before sweep too"
        );

        let dropped = table.sweep(later);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id.selector(), "usb:1d6b:0002");
        assert_eq!(table.len(), 1);
        assert!(
            active(&table, later).contains(&r("sdcard:abc")),
            "no expiry means no drop"
        );
    }

    /// C2: the deadline is warned about once, not on every poll.
    #[test]
    fn test_an_approaching_expiry_warns_once() {
        let t0 = Instant::now();
        let mut table = AllowanceTable::new();
        table.add(Allowance::new(
            r("usb:1d6b:0002"),
            Grant::Paired,
            t0,
            Some(Duration::from_secs(90)),
        ));
        table.add(Allowance::new(r("sdcard:abc"), Grant::Promoted, t0, None));

        let ahead = Duration::from_secs(60);
        assert!(
            table.due_to_warn(t0, ahead).is_empty(),
            "90s out is not approaching"
        );

        let close = t0 + Duration::from_secs(31);
        let due = table.due_to_warn(close, ahead);
        assert_eq!(due.len(), 1, "only the timed one warns: {due:?}");
        assert_eq!(due[0].0, "usb:1d6b:0002");
        assert_eq!(due[0].1.as_secs(), 59);

        assert!(
            table.due_to_warn(close, ahead).is_empty(),
            "a warned allowance must not warn again on every poll"
        );
    }

    #[test]
    fn test_add_replaces_and_remove_takes_one() {
        let t0 = Instant::now();
        let mut table = AllowanceTable::new();
        table.add(Allowance::new(r("usb:1d6b:0002"), Grant::Paired, t0, None));
        let old = table.add(Allowance::new(
            r("usb:1d6b:0002"),
            Grant::Promoted,
            t0,
            None,
        ));
        assert_eq!(old.map(|a| a.granted_by), Some(Grant::Paired));
        assert_eq!(table.len(), 1);

        assert!(table.remove(&r("usb:dead:beef")).is_none());
        assert!(table.remove(&r("usb:1d6b:0002")).is_some());
        assert!(table.is_empty());
        assert_eq!(table.clear(), 0);
    }

    /// Spec G11: identity, not port.
    #[test]
    fn test_display_matches_the_monitor_not_the_connector() {
        let t0 = Instant::now();
        let mut table = AllowanceTable::new();
        table.add(Allowance::new(
            r("display:SAM:772d:811021873"),
            Grant::Paired,
            t0,
            None,
        ));

        // The same panel, read from another port: same EDID, different name
        // padding, still allowed.
        let mut moved = edid("SAM", 0x772d, Some(811_021_873));
        moved.name = "Odyssey G93SD".into();
        let live = active(&table, t0);
        assert!(live.contains(&DeviceRef::Display(moved)));

        // A different monitor, the same model with a different serial, and a
        // serial-less panel against a serialled allowance.
        for other in [
            edid("DEL", 0x4321, Some(7)),
            edid("SAM", 0x772d, Some(1)),
            edid("SAM", 0x772d, None),
        ] {
            assert!(!live.contains(&DeviceRef::Display(other)));
        }
    }
}
