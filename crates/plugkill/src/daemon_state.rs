use plugkill_core::allowances::{Allowance, AllowanceTable, DeviceRef, Grant};
use plugkill_core::state::DaemonMode;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many violations the daemon remembers. Memory only, gone on restart.
/// Spec E1, H2, I3.
const HISTORY_LEN: usize = 50;

/// A bus waiting out its grace period. Unless the condition clears first, the
/// violation fires at `until`: the kill sequence in enforce mode, a logged
/// violation in learn mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grace {
    pub until: Instant,
    pub reason: String,
}

/// An open pairing window. While one is open, the next device to appear on a
/// watched bus is admitted as an allowance instead of firing the kill
/// sequence. `ttl` is the `--for` expiry to hand whatever it admits. Spec B1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingWindow {
    pub until: Instant,
    pub ttl: Option<Duration>,
}

/// A violation with the identity of the device that caused it, when the bus
/// reported one. Detection used to format the identity into the message and
/// throw it away, which left `allow_last` nothing to promote. Spec B4, F1.
#[derive(Debug, Clone)]
pub struct Violation {
    pub description: String,
    /// The bus it came off: a device bus, or "power", "network", "lid" for an
    /// event that names no device. The violations panel groups by this, so
    /// every violation carries one whether or not it has an identity.
    pub bus: &'static str,
    /// `None` for an event with nothing to identify: a lid close, a power
    /// unplug, an enumeration failure. `allow_last` refuses on these. Spec B6.
    pub id: Option<DeviceRef>,
    /// Friendly name from the baseline name maps, when one is known.
    pub name: Option<String>,
    /// The change was something appearing rather than going away. Only an
    /// appearance is admissible through a pairing window: a device being
    /// yanked out stays a violation whatever window is open. Spec B1.
    pub appeared: bool,
}

impl Violation {
    /// A device change, with the identity the bus reported.
    pub fn device(
        description: String,
        id: DeviceRef,
        name: Option<String>,
        appeared: bool,
    ) -> Self {
        Self {
            description,
            bus: id.bus(),
            id: Some(id),
            name,
            appeared,
        }
    }

    /// An event violation: a bus and a reason, with no device behind it.
    pub fn event(bus: &'static str, description: String) -> Self {
        Self {
            description,
            bus,
            id: None,
            name: None,
            appeared: false,
        }
    }
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
    /// Set on re-arm (socket `arm` command or disarm-timeout expiry). The main
    /// loop takes it, re-captures every enabled bus baseline, and clears it.
    pub rebaseline_pending: bool,
    /// Reason for a kill requested over the control socket. Set by
    /// `socket::handle_kill`, drained by the main loop, never run on the
    /// socket thread.
    pub kill_pending: Option<String>,
    /// When power went from AC to Battery (for grace period tracking).
    pub power_unplug_at: Option<Instant>,
    /// Whether the trigger-once policy has already fired and needs re-arm.
    pub power_trigger_once_fired: bool,
    /// When network link went down (for grace period tracking).
    pub network_link_down_at: Option<Instant>,
    /// When lid was closed (for grace period tracking).
    pub lid_close_at: Option<Instant>,
    /// Set only while the power checker holds back a violation for
    /// `grace_secs`. Cleared when AC returns, when the grace runs out and the
    /// violation fires, while `require_locked` is unmet, and on re-baseline.
    pub power_grace: Option<Grace>,
    /// Same contract as `power_grace`, for a network link going down.
    pub network_grace: Option<Grace>,
    /// Same contract as `power_grace`, for the lid closing.
    pub lid_grace: Option<Grace>,
    /// Devices accepted for this uptime only. Memory only, never on disk.
    pub allowances: AllowanceTable,
    /// An open pairing window, or None.
    pub pairing: Option<PairingWindow>,
    /// The config file this daemon loaded, as `status` reports it (C1, H1).
    /// A constructor argument so the daemon cannot forget to wire it.
    pub config_path: PathBuf,
    /// What the detector reported, newest first, capped at `HISTORY_LEN`. The
    /// head is what `allow_last` promotes (B4, B5); the whole deque is what the
    /// `violations` command returns (E1, E2).
    violations: VecDeque<(u64, Violation)>,
}

impl DaemonState {
    pub fn new(mode: DaemonMode, config_path: PathBuf) -> Self {
        Self {
            armed: true,
            mode,
            disarm_until: None,
            started_at: Instant::now(),
            violations_logged: 0,
            last_poll: None,
            reload_pending: false,
            rebaseline_pending: false,
            kill_pending: None,
            power_unplug_at: None,
            power_trigger_once_fired: false,
            network_link_down_at: None,
            lid_close_at: None,
            power_grace: None,
            network_grace: None,
            lid_grace: None,
            allowances: AllowanceTable::new(),
            pairing: None,
            config_path,
            violations: VecDeque::new(),
        }
    }

    /// Same, for tests that do not care which config file was loaded.
    #[cfg(test)]
    pub fn new_for_test(mode: DaemonMode) -> Self {
        Self::new(mode, PathBuf::new())
    }

    /// Record a violation at the head of the history, dropping the oldest past
    /// `HISTORY_LEN` (E1, I3).
    ///
    /// The timestamp is a unix second taken here, not an `Instant`: the state
    /// has no wall clock anywhere else, and a client rendering a table wants
    /// the time the violation happened rather than an age it has to subtract
    /// from a "now" that is already a few milliseconds stale.
    pub fn record_violation(&mut self, violation: Violation) {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // A standing condition (lid shut, unwhitelisted device left plugged
        // in) re-fires every poll. Refresh the head instead of pushing, so 50
        // slots hold 50 distinct events rather than 12 seconds of one.
        if let Some((head_at, head)) = self.violations.front_mut()
            && head.bus == violation.bus
            && head.id == violation.id
            && head.description == violation.description
        {
            *head_at = at;
            return;
        }
        self.violations.push_front((at, violation));
        self.violations.truncate(HISTORY_LEN);
    }

    /// The most recent violation, which is what `allow_last` promotes.
    pub fn last_violation(&self) -> Option<&Violation> {
        self.violations.front().map(|(_, v)| v)
    }

    /// The history, newest first, each with the unix second it was recorded.
    pub fn violations(&self) -> impl Iterator<Item = &(u64, Violation)> {
        self.violations.iter()
    }

    /// Returns true if the disarm timeout has expired and the daemon should re-arm.
    pub fn is_disarm_expired(&self) -> bool {
        match self.disarm_until {
            Some(deadline) => Instant::now() >= deadline,
            None => false,
        }
    }

    /// Admit one device through an open pairing window: record it as an
    /// allowance and close the window, so a window admits one device rather
    /// than everything plugged in during the next minute (B2). False when no
    /// window is open, when it has run out, or when the device is one this
    /// platform cannot identify, and the caller treats the change as the
    /// violation it would otherwise be. `has_identity` is `DISPLAY_IDENTITY` at
    /// the poll loop; a parameter so the non-Linux refusal is testable (G12).
    pub fn admit_paired(
        &mut self,
        id: DeviceRef,
        name: Option<String>,
        now: Instant,
        has_identity: bool,
    ) -> bool {
        let Some(window) = self.pairing.filter(|w| w.until > now) else {
            return false;
        };
        if let Some(limit) = crate::socket::display_limit(&id, has_identity) {
            log::warn!("not admitting {}: {limit}", id.selector());
            return false;
        }
        let selector = id.selector();
        self.allowances
            .add(Allowance::new(id, Grant::Paired, now, window.ttl).with_name(name.clone()));
        self.pairing = None;
        log::info!(
            "paired {selector}{}, pairing window closed",
            name.as_deref().map_or(String::new(), |n| format!(" ({n})"))
        );
        true
    }

    /// The running grace period that expires first, with the bus it belongs
    /// to: "power", "network" or "lid". `counts` decides per bus whether its
    /// grace still matters, so a caller can drop a bus that a reload switched
    /// off or to the monitor policy.
    pub fn soonest_grace(&self, counts: impl Fn(&str) -> bool) -> Option<(&'static str, &Grace)> {
        [
            ("power", self.power_grace.as_ref()),
            ("network", self.network_grace.as_ref()),
            ("lid", self.lid_grace.as_ref()),
        ]
        .into_iter()
        .filter(|&(bus, _)| counts(bus))
        .filter_map(|(bus, grace)| grace.map(|g| (bus, g)))
        .min_by_key(|(_, g)| g.until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn grace_at(now: Instant, secs: u64, reason: &str) -> Grace {
        Grace {
            until: now + Duration::from_secs(secs),
            reason: reason.to_string(),
        }
    }

    fn stick(product: &str) -> DeviceRef {
        DeviceRef::Usb(plugkill_core::usb::UsbDeviceId {
            vendor_id: "0781".to_string(),
            product_id: product.to_string(),
        })
    }

    fn window_open(st: &mut DaemonState, now: Instant, secs: u64) {
        st.pairing = Some(PairingWindow {
            until: now + Duration::from_secs(secs),
            ttl: None,
        });
    }

    /// G3. A window admits one device and then closes, so it does not take in
    /// everything plugged in over the next minute.
    #[test]
    fn test_a_pairing_window_admits_one_device_then_closes() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        window_open(&mut st, now, 60);

        assert!(st.admit_paired(stick("5583"), Some("DataTraveler".to_string()), now, true));
        assert!(
            !st.admit_paired(stick("aaaa"), None, now, true),
            "the window closed on the first admission"
        );

        assert_eq!(st.allowances.len(), 1);
        assert!(st.pairing.is_none());
        let granted = st.allowances.iter().next().expect("one allowance");
        assert_eq!(granted.id, stick("5583"));
        assert_eq!(granted.name.as_deref(), Some("DataTraveler"));
    }

    /// G12. A platform that cannot identify a monitor cannot admit one
    /// either, so the change stays the violation it was and the window stays
    /// open for a device this platform can identify.
    #[test]
    fn test_a_window_does_not_admit_a_display_without_connector_data() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        window_open(&mut st, now, 60);
        let display: DeviceRef = "display:SAM:772d:811021873".parse().unwrap();

        assert!(!st.admit_paired(display, None, now, false));

        assert!(st.allowances.is_empty());
        assert!(st.pairing.is_some(), "the window is not spent on a refusal");
    }

    /// G4. A window that runs out admits nothing.
    #[test]
    fn test_a_pairing_window_that_times_out_leaves_no_allowance() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        window_open(&mut st, now, 60);

        assert!(!st.admit_paired(stick("5583"), None, now + Duration::from_secs(61), true));

        assert!(
            st.allowances.is_empty(),
            "a window that runs out admits nothing"
        );
    }

    /// I3. The history is bounded and the oldest entry is the one that goes.
    #[test]
    fn test_violation_history_is_bounded_and_drops_the_oldest() {
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        for i in 0..HISTORY_LEN + 10 {
            st.record_violation(Violation::event("lid", format!("lid closed {i}")));
        }

        let kept: Vec<&str> = st
            .violations()
            .map(|(_, v)| v.description.as_str())
            .collect();
        assert_eq!(kept.len(), HISTORY_LEN);
        assert_eq!(kept[0], "lid closed 59", "newest first");
        assert_eq!(kept[HISTORY_LEN - 1], "lid closed 10", "oldest dropped");
        assert_eq!(
            st.last_violation().map(|v| v.description.as_str()),
            Some("lid closed 59"),
            "allow_last reads the head of the history"
        );
    }

    /// A standing condition re-fires every poll. It must not push the other 49
    /// entries out of the history.
    #[test]
    fn test_a_repeated_violation_refreshes_the_head_instead_of_flooding() {
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.record_violation(Violation::event("power", "AC power removed".to_string()));
        for _ in 0..60 {
            st.record_violation(Violation::event("lid", "lid closed".to_string()));
        }

        assert_eq!(st.violations().count(), 2, "one row per distinct event");
        assert_eq!(
            st.last_violation().map(|v| v.description.as_str()),
            Some("lid closed"),
            "the newest stays at the head"
        );
    }

    /// A violation with nothing to identify is still recorded, with a bus and
    /// no device (E1).
    #[test]
    fn test_an_event_violation_is_recorded_without_an_identity() {
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.record_violation(Violation::event("power", "AC power removed".to_string()));

        let (at, v) = st.violations().next().expect("one violation");
        assert!(v.id.is_none());
        assert_eq!(v.bus, "power");
        assert!(*at > 1_700_000_000, "a unix second was stamped: {at}");
    }

    #[test]
    fn test_soonest_grace_is_none_without_a_running_grace() {
        let st = DaemonState::new_for_test(DaemonMode::Enforce);
        assert!(st.soonest_grace(|_| true).is_none());
    }

    #[test]
    fn test_soonest_grace_picks_the_earliest_deadline() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.power_grace = Some(grace_at(now, 40, "AC power removed"));
        st.lid_grace = Some(grace_at(now, 12, "lid closed"));

        let (bus, grace) = st.soonest_grace(|_| true).expect("two graces are running");

        assert_eq!(bus, "lid");
        assert_eq!(grace.reason, "lid closed");
    }

    #[test]
    fn test_soonest_grace_skips_buses_the_filter_rejects() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.power_grace = Some(grace_at(now, 40, "AC power removed"));
        st.lid_grace = Some(grace_at(now, 12, "lid closed"));

        let (bus, _) = st
            .soonest_grace(|bus| bus != "lid")
            .expect("power is still running");

        assert_eq!(bus, "power");
    }
}
