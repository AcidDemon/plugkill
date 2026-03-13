use crate::power::PowerState;
use crate::sdcard::SdCardSnapshot;
use crate::thunderbolt::ThunderboltSnapshot;
use crate::usb::DeviceSnapshot;
use std::collections::HashMap;
use std::time::Instant;

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
    /// Cached device names from detailed enumeration at baseline capture time.
    pub names: DeviceNames,
}

/// Runtime state of the daemon, shared between the poll loop and socket handler.
pub struct DaemonState {
    pub armed: bool,
    pub mode: DaemonMode,
    pub disarm_until: Option<Instant>,
    pub started_at: Instant,
    pub violations_logged: u64,
    pub last_poll: Option<Instant>,
    pub reload_pending: bool,
    /// When power went from AC to Battery (for grace period tracking).
    pub power_unplug_at: Option<Instant>,
    /// Whether the trigger-once policy has already fired and needs re-arm.
    pub power_trigger_once_fired: bool,
}

impl DaemonState {
    pub fn new(mode: DaemonMode) -> Self {
        Self {
            armed: true,
            mode,
            disarm_until: None,
            started_at: Instant::now(),
            violations_logged: 0,
            last_poll: None,
            reload_pending: false,
            power_unplug_at: None,
            power_trigger_once_fired: false,
        }
    }

    /// Returns true if the disarm timeout has expired and the daemon should re-arm.
    pub fn is_disarm_expired(&self) -> bool {
        match self.disarm_until {
            Some(deadline) => Instant::now() >= deadline,
            None => false,
        }
    }
}
