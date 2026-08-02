use plugkill_core::state::DaemonMode;
use std::time::Instant;

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
    /// When network link went down (for grace period tracking).
    pub network_link_down_at: Option<Instant>,
    /// When lid was closed (for grace period tracking).
    pub lid_close_at: Option<Instant>,
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
            network_link_down_at: None,
            lid_close_at: None,
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
