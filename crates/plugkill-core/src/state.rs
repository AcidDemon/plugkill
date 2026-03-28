use crate::lid::LidState;
use crate::network::NetworkSnapshot;
use crate::power::PowerState;
use crate::sdcard::SdCardSnapshot;
use crate::thunderbolt::ThunderboltSnapshot;
use crate::usb::DeviceSnapshot;
use std::collections::HashMap;

/// Operating mode of the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonMode {
    /// Normal mode: violations trigger the kill sequence.
    Enforce,
    /// Learning/audit mode: violations are logged but the system is not shut down.
    Learn,
}

impl std::fmt::Display for DaemonMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonMode::Enforce => write!(f, "enforce"),
            DaemonMode::Learn => write!(f, "learn"),
        }
    }
}

/// Device name lookup maps for human-readable violation messages.
#[derive(Debug, Default)]
pub struct DeviceNames {
    /// USB vendor:product → product name
    pub usb: HashMap<(String, String), String>,
    /// Thunderbolt unique_id → device name
    pub thunderbolt: HashMap<String, String>,
    /// SD card serial → card name
    pub sdcard: HashMap<String, String>,
}

/// Baseline snapshots for all monitored buses.
pub struct Baselines {
    pub usb: Option<DeviceSnapshot>,
    pub thunderbolt: Option<ThunderboltSnapshot>,
    pub sdcard: Option<SdCardSnapshot>,
    pub power: Option<PowerState>,
    pub network: Option<NetworkSnapshot>,
    pub lid: Option<LidState>,
    /// Cached device names from detailed enumeration at baseline capture time.
    pub names: DeviceNames,
}
