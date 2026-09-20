//! One thread keeps the tray and the dashboard current.

use crate::status::{self, Devices, Status, TrayState, Violations};
use crate::tray::PlugkillTray;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Loop tick and blink half-period. Status is fetched every second tick.
pub const TICK: Duration = Duration::from_millis(500);

/// How long a command notice stays on screen. It says what one command did,
/// not what the daemon is doing, so it goes stale on its own.
pub const NOTICE_TTL: Duration = Duration::from_secs(30);

/// Whether a notice set at `at` has been up long enough to drop.
pub fn notice_expired(at: Instant) -> bool {
    at.elapsed() > NOTICE_TTL
}

/// What the poll loop hands to the dashboard and the settings window.
/// `devices` and `violations` are None on the ticks that skip them, and
/// `violations` is never fetched while the settings window is closed; both
/// windows then keep what they already show.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub state: TrayState,
    pub status: Option<Status>,
    pub devices: Option<Devices>,
    pub violations: Option<Violations>,
}

/// The next blink phase: toggles while a kill is pending, rests off otherwise.
pub fn next_blink(state: &TrayState, blink_on: bool) -> bool {
    matches!(state, TrayState::KillPending { .. }) && !blink_on
}

/// What this tick fetches past the status: whether to walk the buses, and
/// whether to ask for the violation history. The buses are walked every fourth
/// tick while a window that shows devices is open, plus the tick either window
/// opens on, so a reopened window is not showing the lists from the last time
/// it was up. Only the settings window shows violations.
pub fn fetches(
    tick: u64,
    dashboard: bool,
    settings: bool,
    was_dashboard: bool,
    was_settings: bool,
) -> (bool, bool) {
    let opened = (dashboard && !was_dashboard) || (settings && !was_settings);
    let walk = (dashboard || settings) && (tick.is_multiple_of(4) || opened);
    (walk, walk && settings)
}

/// Poll `socket` and drive the tray until the tray shuts down. The two flags
/// are set by the GTK side while each window is on screen: the device walk is
/// only asked for while one of them is up, on the ticks `fetches` picks, and
/// the violation history only while the settings window is.
pub fn run(
    tray: ksni::blocking::Handle<PlugkillTray>,
    socket: PathBuf,
    updates: Option<async_channel::Sender<Update>>,
    notices: async_channel::Sender<Option<String>>,
    dashboard_open: Arc<AtomicBool>,
    settings_open: Arc<AtomicBool>,
) {
    let (mut was_dashboard, mut was_settings) = (false, false);
    for tick in 0u64.. {
        let started = Instant::now();
        // First on the tick, before the device walk below: the daemon walks
        // sysfs for that and can hold the socket for its whole read timeout,
        // and the blink must not wait behind it.
        let dropped_notice = tray.update(|t| {
            t.blink_on = next_blink(&t.state, t.blink_on);
            let stale = t.notice.as_ref().is_some_and(|(_, at)| notice_expired(*at));
            if stale {
                t.notice = None;
            }
            stale
        });
        match dropped_notice {
            None => return,
            // The dashboard holds its own copy, so clear that one too.
            Some(true) => {
                let _ = notices.try_send(None);
            }
            Some(false) => {}
        }
        if tick % 2 == 0 {
            let status = status::fetch(&socket);
            let state = TrayState::from_status(status.as_ref());
            if let Some(tx) = &updates {
                // A closed dashboard channel must not stop the tray.
                let _ = tx.try_send(Update {
                    state: state.clone(),
                    status: status.clone(),
                    devices: None,
                    violations: None,
                });
            }
            let applied = tray.update(|t| {
                t.state = state.clone();
                t.status = status.clone();
            });
            if applied.is_none() {
                return;
            }
            // Last on the tick: the daemon walks sysfs for this, so it runs
            // once the tray and the dashboard already carry this tick's state.
            let dashboard = dashboard_open.load(Ordering::Relaxed);
            let settings = settings_open.load(Ordering::Relaxed);
            let (walk, want_violations) =
                fetches(tick, dashboard, settings, was_dashboard, was_settings);
            if let Some(tx) = &updates
                && walk
            {
                let devices = status::fetch_devices(&socket);
                // Memory, not sysfs, but only the settings window shows it.
                let violations = want_violations
                    .then(|| status::fetch_violations(&socket))
                    .flatten();
                let _ = tx.try_send(Update {
                    state,
                    status,
                    devices,
                    violations,
                });
            }
            (was_dashboard, was_settings) = (dashboard, settings);
        }
        // A slow walk eats its own slack instead of pushing the next tick out.
        thread::sleep(TICK.saturating_sub(started.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blink_toggles_only_while_a_kill_is_pending() {
        let pending = TrayState::KillPending {
            bus: "lid".into(),
            reason: "lid closed".into(),
            secs_left: 3,
        };
        assert!(next_blink(&pending, false));
        assert!(!next_blink(&pending, true));
        assert!(!next_blink(&TrayState::Armed, false));
        assert!(
            !next_blink(&TrayState::Armed, true),
            "a stale blink phase must reset"
        );
    }

    #[test]
    fn test_a_notice_drops_once_it_is_stale() {
        let now = Instant::now();
        assert!(!notice_expired(now), "a fresh notice stays up");
        if let Some(old) = now.checked_sub(NOTICE_TTL + Duration::from_secs(1)) {
            assert!(notice_expired(old));
        }
    }

    #[test]
    fn test_buses_are_walked_on_open_and_every_fourth_tick() {
        // (tick, dashboard, settings, was_dashboard, was_settings)
        assert_eq!(fetches(4, true, false, true, false), (true, false));
        assert_eq!(
            fetches(6, true, false, true, false),
            (false, false),
            "between the walk ticks"
        );
        assert_eq!(
            fetches(6, true, false, false, false),
            (true, false),
            "the tick the dashboard opens on"
        );
        assert_eq!(
            fetches(6, true, true, true, false),
            (true, true),
            "settings opening while the dashboard was already up"
        );
        assert_eq!(
            fetches(4, false, false, true, true),
            (false, false),
            "no window open walks nothing"
        );
        assert_eq!(
            fetches(4, true, true, true, true),
            (true, true),
            "both already open still walks on the walk tick"
        );
    }

    #[test]
    fn test_violations_are_only_asked_for_by_the_settings_window() {
        // The dashboard does not show them, and asking costs the daemon a
        // round trip it has nothing to do with.
        assert!(!fetches(4, true, false, true, false).1);
        assert!(fetches(4, false, true, false, true).1);
    }
}
