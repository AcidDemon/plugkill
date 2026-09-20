use crate::daemon_state::{DaemonState, PairingWindow};
use log::{debug, error, info, warn};
use plugkill_core::allowances::{Allowance, AllowanceTable, DeviceRef, Grant};
use plugkill_core::authz::{
    ACTION_ALLOW, ACTION_DISARM, ACTION_LEARN, ACTION_RELOAD, Authority, Authorization,
};
use plugkill_core::config::{Config, LidPolicy, NetworkPolicy, PowerPolicy};
use plugkill_core::ipc::{AUTH_REQUIRED_ERROR, Request, Response, format_duration};
use plugkill_core::lid::{self, LidState};
use plugkill_core::network::{self, LinkState};
use plugkill_core::power::{self, PowerState};
use plugkill_core::sdcard::{self, SdCardDeviceInfo};
use plugkill_core::state::{Baselines, DaemonMode};
use plugkill_core::thunderbolt::{self, ThunderboltDeviceInfo};
use plugkill_core::usb::{self, UsbDeviceInfo};
use plugkill_core::{display, pci};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Spawn a background thread that accepts connections on the Unix domain socket
/// and dispatches commands.
pub fn start_socket_listener(
    socket_path: PathBuf,
    socket_group: Option<&str>,
    state: Arc<Mutex<DaemonState>>,
    config: Arc<RwLock<Config>>,
    baselines: Arc<RwLock<Baselines>>,
    authority: Arc<dyn Authority>,
) -> std::io::Result<()> {
    // Clean up stale socket from previous run
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)?;

    // Set socket permissions: 0660 (owner + group read/write)
    set_socket_permissions(&socket_path)?;

    // Optional group ownership, for non-root GUI access
    if let Some(group) = socket_group {
        set_socket_group(&socket_path, group)?;
    }

    info!("control socket listening on {}", socket_path.display());

    std::thread::Builder::new()
        .name("socket-listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let state = Arc::clone(&state);
                        let config = Arc::clone(&config);
                        let baselines = Arc::clone(&baselines);
                        let authority = Arc::clone(&authority);
                        std::thread::Builder::new()
                            .name("socket-handler".into())
                            .spawn(move || {
                                if let Err(e) = handle_connection(
                                    stream,
                                    &state,
                                    &config,
                                    &baselines,
                                    authority.as_ref(),
                                ) {
                                    warn!("socket connection error: {e}");
                                }
                            })
                            .ok();
                    }
                    Err(e) => {
                        error!("socket accept error: {e}");
                    }
                }
            }
        })?;

    Ok(())
}

fn set_socket_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o660);
    std::fs::set_permissions(path, perms)
}

fn set_socket_group(path: &Path, group: &str) -> std::io::Result<()> {
    let gid = nix::unistd::Group::from_name(group)
        .map_err(|e| std::io::Error::other(format!("group lookup failed: {e}")))?
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("group '{group}' not found"),
            )
        })?
        .gid;
    nix::unistd::chown(path, None, Some(gid))
        .map_err(|e| std::io::Error::other(format!("chown failed: {e}")))?;
    info!("socket group set to '{group}' (gid {gid})");
    Ok(())
}

/// What the kernel reports about the connected client, never what the client
/// says about itself. `pid` is `None` where the platform's peer-credential
/// sockopt carries no pid; a gated command from a non-root caller is then
/// refused, because there is no subject to hand the authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerCreds {
    uid: u32,
    pid: Option<u32>,
}

/// Credentials of the connected client, or `None` where the platform does not
/// expose them. Callers must treat `None` as unprivileged.
fn peer_creds(stream: &UnixStream) -> Option<PeerCreds> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
        getsockopt(stream, PeerCredentials).ok().map(|c| PeerCreds {
            uid: c.uid(),
            pid: u32::try_from(c.pid()).ok(),
        })
    }
    #[cfg(any(target_os = "freebsd", target_os = "macos", target_os = "ios"))]
    {
        // LocalPeerCred carries no pid, so there is no subject to hand the
        // authority, and the polkit check is compiled Linux-only besides. So
        // require_auth here refuses every non-root gated command by design.
        use nix::sys::socket::{getsockopt, sockopt::LocalPeerCred};
        getsockopt(stream, LocalPeerCred).ok().map(|c| PeerCreds {
            uid: c.uid(),
            pid: None,
        })
    }
    // ponytail: no other platform ships this daemon. Anything else fails
    // closed via kill_authorized; add its sockopt here if a port needs kill.
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        let _ = stream;
        None
    }
}

/// Whether a socket peer may run the kill command.
///
/// The socket is 0660 with an optional group for non-root GUI and CLI use, so
/// every other command is deliberately reachable by that group. `kill` is the
/// only destructive one, so it is restricted to root. `None` (peer credentials
/// unavailable) is not root: this fails closed on purpose.
/// Who sent a command, for the journal. The uid is what the kernel reports for
/// the peer, so it cannot be spoofed by the client; the name is a convenience
/// and falls back to the number when the user is not in passwd.
fn peer_label(uid: Option<u32>) -> String {
    match uid {
        Some(uid) => match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
            Ok(Some(user)) => format!("uid {uid} ({})", user.name),
            _ => format!("uid {uid}"),
        },
        None => "an unidentified peer".to_string(),
    }
}

fn kill_authorized(peer_uid: Option<u32>) -> bool {
    peer_uid == Some(0)
}

fn handle_connection(
    stream: UnixStream,
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    authority: &dyn Authority,
) -> std::io::Result<()> {
    // Set a read timeout so we don't hang forever on misbehaving clients
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    // Read once here: handle_request no longer has the stream to ask.
    let creds = peer_creds(&stream);

    let reader = BufReader::new(&stream);
    let mut writer = &stream;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                // Timeout or connection closed
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(e);
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(req, state, config, baselines, creds, authority),
            Err(e) => Response::err(format!("invalid request: {e}")),
        };

        let mut resp_json = serde_json::to_string(&response)
            .unwrap_or_else(|_| r#"{"ok":false,"error":"serialization error"}"#.to_string());
        resp_json.push('\n');
        writer.write_all(resp_json.as_bytes())?;
        writer.flush()?;
    }

    Ok(())
}

/// The polkit action id for a command that has to authenticate, or `None` for
/// one that never does. `arm` and `enforce` only make the daemon stricter, and
/// `status`, `devices` and `violations` change nothing, so none of them is
/// gated (H4). `kill`
/// keeps its own root-only rule instead.
fn gated_action(req: &Request) -> Option<&'static str> {
    match req {
        Request::Disarm { .. } => Some(ACTION_DISARM),
        Request::Learn => Some(ACTION_LEARN),
        Request::Reload => Some(ACTION_RELOAD),
        // A zero window only closes an open one, which removes a permission
        // the way revoke does, so it is never gated (E3, B3).
        Request::Pair { window_secs: 0, .. } => None,
        // Both accept hardware, which is the bypass gating disarm was for
        // (E2). Revoking only removes a permission, so it is never gated (E3).
        Request::Pair { .. } | Request::AllowLast { .. } => Some(ACTION_ALLOW),
        Request::Status
        | Request::Devices
        | Request::Violations
        | Request::Arm
        | Request::Enforce
        | Request::Revoke { .. }
        | Request::RevokeAll
        | Request::Kill { .. } => None,
    }
}

/// `None` when the command may run, or the refusal to send back.
///
/// Holds no lock while the authority works: the `require_auth` read drops its
/// guard at the end of that statement, and this runs before dispatch, so no
/// handler has taken the state lock yet. A polkit prompt waits on a person, and
/// the poll loop must not wait behind it.
fn authorize(
    req: &Request,
    config: &Arc<RwLock<Config>>,
    creds: Option<PeerCreds>,
    authority: &dyn Authority,
) -> Option<Response> {
    let action = gated_action(req)?;
    let require_auth = config.read().unwrap().general.require_auth;
    if !require_auth {
        return None;
    }

    let uid = creds.map(|c| c.uid);
    let who = peer_label(uid);
    // Root passes without asking, so `sudo plugkill --disarm` still works on a
    // headless box with no agent to prompt.
    if uid == Some(0) {
        info!("{action} allowed for {who} (root)");
        return None;
    }

    // No pid means no subject to hand the authority, which is a refusal like
    // any other. Anything but an explicit yes is a refusal.
    let Some(PeerCreds {
        uid: caller_uid,
        pid: Some(pid),
    }) = creds
    else {
        warn!("{action} refused for {who}: no peer pid");
        return Some(Response::err(format!(
            "{AUTH_REQUIRED_ERROR}: the caller cannot be identified"
        )));
    };

    match authority.check(action, caller_uid, pid) {
        Authorization::Allowed => {
            info!("{action} authorized for {who} (pid {pid})");
            None
        }
        Authorization::Refused(reason) => {
            warn!("{action} refused for {who} (pid {pid}): {reason}");
            Some(Response::err(format!("{AUTH_REQUIRED_ERROR}: {reason}")))
        }
    }
}

fn handle_request(
    req: Request,
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    creds: Option<PeerCreds>,
    authority: &dyn Authority,
) -> Response {
    if let Some(refusal) = authorize(&req, config, creds, authority) {
        return refusal;
    }

    let peer_uid = creds.map(|c| c.uid);
    match req {
        Request::Status => handle_status(state, config, baselines),
        Request::Devices => handle_devices(config),
        Request::Violations => handle_violations(state),
        Request::Disarm { timeout_secs } => handle_disarm(state, timeout_secs, peer_uid),
        Request::Arm => handle_arm(state, peer_uid),
        Request::Learn => handle_learn(state, peer_uid),
        Request::Enforce => handle_enforce(state, peer_uid),
        Request::Reload => handle_reload(state, peer_uid),
        Request::Pair {
            window_secs,
            for_secs,
        } => handle_pair(state, window_secs, for_secs, peer_uid),
        Request::AllowLast { for_secs } => {
            handle_allow_last(state, for_secs, peer_uid, DISPLAY_IDENTITY)
        }
        Request::Revoke { selector } => handle_revoke(state, &selector, peer_uid),
        Request::RevokeAll => handle_revoke_all(state, peer_uid),
        Request::Kill { reason } => handle_kill(state, &reason, peer_uid),
    }
}

/// Status spelling of a power reading: lowercase and stable, unlike `Display`.
fn power_state_key(state: PowerState) -> &'static str {
    match state {
        PowerState::Ac => "ac",
        PowerState::Battery => "battery",
        PowerState::Unknown => "unknown",
    }
}

/// Status spelling of a lid reading.
fn lid_state_key(state: LidState) -> &'static str {
    match state {
        LidState::Open => "open",
        LidState::Closed => "closed",
        LidState::Unknown => "unknown",
    }
}

/// The running grace period that fires first, as `status` reports it, or null.
///
/// A disarmed daemon reports none, because its poll loop is not counting. A
/// bus a reload switched off, or to the monitor policy, reports none either,
/// so a grace its checker will never finish cannot show up as a countdown.
fn pending_violation_json(st: &DaemonState, cfg: &Config, now: Instant) -> serde_json::Value {
    if !st.armed {
        return serde_json::Value::Null;
    }
    let counts = |bus: &str| match bus {
        "power" => cfg.general.watch_power && cfg.power.policy != PowerPolicy::Monitor,
        "network" => cfg.general.watch_network && cfg.network.policy != NetworkPolicy::Monitor,
        "lid" => cfg.general.watch_lid && cfg.lid.policy != LidPolicy::Monitor,
        _ => false,
    };
    match st.soonest_grace(counts) {
        Some((bus, grace)) => {
            let left = grace.until.saturating_duration_since(now);
            // Round up, so the last second before the deadline reads 1, not 0.
            let secs_left = left.as_millis().div_ceil(1000) as u64;
            serde_json::json!({"bus": bus, "reason": grace.reason, "secs_left": secs_left})
        }
        None => serde_json::Value::Null,
    }
}

/// How many links the daemon counts as down: the interfaces that were Up in
/// the baseline and now read Down or are gone, which is exactly what
/// `detect_link_down` fires on. Absolute counting would report a built-in port
/// that has never had a cable in it, which is no violation and never will be.
/// With no baseline captured yet, nothing is being compared, so nothing is down.
fn links_down(
    current: &network::NetworkSnapshot,
    baseline: Option<&network::NetworkSnapshot>,
) -> usize {
    baseline.map_or(0, |bl| {
        bl.interfaces()
            .iter()
            .filter(|&(iface, &state)| {
                state == LinkState::Up
                    && matches!(
                        current.interfaces().get(iface),
                        Some(LinkState::Down) | None
                    )
            })
            .count()
    })
}

fn handle_status(
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> Response {
    // Live readings come first, with no lock held: reading the lid can wait on
    // logind, and the poll loop must not stall behind a status request.
    let (watch_power, watch_lid, watch_network, iface_filter) = {
        let cfg = config.read().unwrap();
        (
            cfg.general.watch_power,
            cfg.general.watch_lid,
            cfg.general.watch_network,
            cfg.network.interfaces.clone(),
        )
    };
    let power_state = watch_power.then(|| power_state_key(power::read_power_state()));
    let lid_state = watch_lid.then(|| lid_state_key(lid::read_lid_state()));
    let network_now = watch_network.then(|| network::enumerate_interfaces(&iface_filter));

    // Global lock order is config, then baselines, then state. Every other
    // site takes them this way: the poll loop holds config and baselines while
    // `capture_baselines` locks state inside its power, network and lid
    // branches. Acquiring state first here deadlocks that capture against a
    // relay `status` poll, which leaves the poll thread holding the baselines
    // write guard while systemd still sees a healthy process. Keep this order.
    let cfg = config.read().unwrap();
    let bl = baselines.read().unwrap();
    let st = state.lock().unwrap();

    let uptime_secs = st.started_at.elapsed().as_secs();
    let disarm_remaining_secs = st.disarm_until.map(|deadline| {
        let now = Instant::now();
        if deadline > now {
            (deadline - now).as_secs()
        } else {
            0
        }
    });

    let usb_devices = bl.usb.as_ref().map(|s| s.len()).unwrap_or(0);
    let thunderbolt_devices = bl.thunderbolt.as_ref().map(|s| s.len()).unwrap_or(0);
    let sdcard_devices = bl.sdcard.as_ref().map(|s| s.len()).unwrap_or(0);
    let pci_devices = bl.pci.as_ref().map(|s| s.len()).unwrap_or(0);

    let network_links_down = network_now
        .as_ref()
        .map(|now| links_down(now, bl.network.as_ref()));

    let last_poll_ms_ago = st.last_poll.map(|t| t.elapsed().as_millis() as u64);
    let now = Instant::now();
    let pending_violation = pending_violation_json(&st, &cfg, now);
    let allowances = allowances_json(&st.allowances, now);
    let pairing_window_secs_left = st
        .pairing
        .as_ref()
        .map(|w| w.until.saturating_duration_since(now).as_secs());

    Response::ok(serde_json::json!({
        "armed": st.armed,
        "mode": st.mode.to_string(),
        "uptime_secs": uptime_secs,
        "disarm_remaining_secs": disarm_remaining_secs,
        "usb_devices": usb_devices,
        "thunderbolt_devices": thunderbolt_devices,
        "sdcard_devices": sdcard_devices,
        "pci_devices": pci_devices,
        "usb_watching": cfg.general.watch_usb,
        "thunderbolt_watching": cfg.general.watch_thunderbolt,
        "sdcard_watching": cfg.general.watch_sdcard,
        "power_watching": cfg.general.watch_power,
        "network_watching": cfg.general.watch_network,
        "lid_watching": cfg.general.watch_lid,
        "pci_watching": cfg.general.watch_pci,
        "display_watching": cfg.general.watch_display,
        "violations_logged": st.violations_logged,
        "last_poll_ms_ago": last_poll_ms_ago,
        "dry_run": cfg.general.dry_run,
        "pending_violation": pending_violation,
        "power_state": power_state,
        "lid_state": lid_state,
        "network_links_down": network_links_down,
        "allowances": allowances,
        "pairing_window_secs_left": pairing_window_secs_left,
        // The file this daemon actually loaded, so a client reads the same one
        // rather than guessing a default (C1, H1). Empty when nothing set it.
        "config_path": st.config_path.display().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// The remembered violations, newest first (E2, H3). A read: it takes the
/// state lock, copies, and changes nothing.
fn handle_violations(state: &Arc<Mutex<DaemonState>>) -> Response {
    let st = state.lock().unwrap();
    let violations: Vec<serde_json::Value> = st
        .violations()
        .map(|(at, v)| {
            serde_json::json!({
                "at_unix": at,
                "bus": v.bus,
                // Null for an event with nothing to identify: a lid close, a
                // power unplug. A row with one can be whitelisted or allowed.
                "selector": v.id.as_ref().map(|id| id.selector()),
                // Device-supplied, so scrubbed and capped like a device
                // table line: a client draws it as one row.
                "name": v.name.as_deref().map(one_line),
                "message": one_message(&v.description),
            })
        })
        .collect();
    Response::ok(serde_json::json!({ "violations": violations }))
}

/// How many entries a bus reports before the rest are only counted.
const MAX_ENTRIES: usize = 12;

/// How long one entry may be. A device names itself, so the name is as long
/// and as strange as its descriptor says.
const MAX_LINE: usize = 80;

/// How long a violation message may be. Not `MAX_LINE`: that sizes a device
/// table row, and an enumeration failure spends its first 65 characters on the
/// prefix before it gets to the cause. The 50-entry history caps the reply.
const MAX_MESSAGE: usize = 512;

/// Same control-character scrub as `one_line`, with room for the cause.
fn one_message(description: &str) -> String {
    description
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_MESSAGE)
        .collect()
}

/// One entry is one short line. Device-supplied names reach us straight from
/// sysfs, and a client renders these lines one per row, so a name carrying a
/// newline would show up as an extra device that does not exist.
fn one_line(entry: &str) -> String {
    entry
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LINE)
        .collect()
}

/// One entry of a bus: the line a client shows, and the topology path the
/// device sits at. The path is absent for a bus that has no topology, and for
/// a device whose path could not be read.
type Entry = (String, Option<String>);

/// An entry with no topology, for the buses that are a single reading.
fn flat(text: impl Into<String>) -> Entry {
    (text.into(), None)
}

/// Where a PCI device sits, as the bridges above it: `/sys/bus/pci/devices`
/// is a flat directory of links, and the link target is the tree. A device
/// whose link cannot be read reports no path and renders as a root.
fn pci_path(address: &str) -> Option<String> {
    let link = std::fs::read_link(format!("/sys/bus/pci/devices/{address}")).ok()?;
    pci_topology(&link)
}

/// The part of a sysfs device link that is the topology: everything under
/// `devices/`, so `../../../devices/pci0000:00/0000:00:1c.0/0000:01:00.0`
/// becomes `pci0000:00/0000:00:1c.0/0000:01:00.0` and the bridge above it is
/// a prefix a client can nest on.
fn pci_topology(link: &Path) -> Option<String> {
    let mut parts = link.components().map(|c| c.as_os_str().to_str());
    parts.find(|c| *c == Some("devices"))?;
    let rest: Option<Vec<&str>> = parts.collect();
    let rest = rest?;
    if rest.is_empty() {
        return None;
    }
    Some(rest.join("/"))
}

/// One bus of a `devices` response: sorted, capped, and the dropped count.
/// Sorting keeps repeated calls stable, so a client's list does not reshuffle.
/// It is sorted by text, not by path: a client that nests rebuilds the order
/// it wants from the paths anyway.
fn bus_json(watched: bool, mut entries: Vec<Entry>) -> serde_json::Value {
    for (text, path) in &mut entries {
        *text = one_line(text);
        *path = path.as_deref().map(one_line).filter(|p| !p.is_empty());
    }
    entries.sort();
    let more = entries.len().saturating_sub(MAX_ENTRIES);
    entries.truncate(MAX_ENTRIES);
    let entries: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|(text, path)| match path {
            Some(p) => serde_json::json!({"text": text, "path": p}),
            None => serde_json::json!({"text": text}),
        })
        .collect();
    serde_json::json!({"watched": watched, "entries": entries, "more": more})
}

/// `"<id> <name>"`, or the bare id when nothing names the device.
fn entry_line(id: String, name: Option<&str>) -> String {
    match name.filter(|n| !n.is_empty()) {
        Some(n) => format!("{id} {n}"),
        None => id,
    }
}

/// Entries for one enumerated bus. An unwatched bus reports nothing, and so
/// does one that cannot be read: a single failing bus must not fail the whole
/// response.
fn bus_entries<T, E: std::fmt::Display>(
    watched: bool,
    bus: &str,
    enumerate: impl FnOnce() -> Result<Vec<T>, E>,
    line: impl Fn(&T) -> Entry,
) -> Vec<Entry> {
    if !watched {
        return Vec::new();
    }
    match enumerate() {
        Ok(items) => items.iter().map(line).collect(),
        Err(e) => {
            // Debug, not warn: a machine without a Thunderbolt controller or an
            // MMC host fails here on every walk, and the dashboard asks every
            // two seconds. A bus that is present but broken is already warned
            // about once at baseline capture and logged as a violation.
            debug!("devices: cannot enumerate {bus}: {e}");
            Vec::new()
        }
    }
}

/// Serial numbers are left out on purpose: they identify the exact unit and
/// this line crosses a socket. The vendor:product pair does not.
fn usb_line(dev: &UsbDeviceInfo) -> String {
    entry_line(
        format!("{}:{}", dev.vendor_id, dev.product_id),
        dev.product.as_deref().or(dev.manufacturer.as_deref()),
    )
}

/// Same shape as `usb_line`. The `unique_id` UUID is a per-unit id, so it is
/// left out for the same reason a USB serial is.
fn thunderbolt_line(dev: &ThunderboltDeviceInfo) -> String {
    entry_line(
        format!("{}:{}", dev.vendor_id, dev.device_id),
        dev.device_name.as_deref().or(dev.vendor_name.as_deref()),
    )
}

/// Same shape again. A card is keyed by serial and its CID embeds that serial,
/// so the id here is the manufacturer/OEM pair instead.
fn sdcard_line(dev: &SdCardDeviceInfo) -> String {
    entry_line(
        format!(
            "{}:{}",
            dev.manfid.as_deref().unwrap_or("?"),
            dev.oemid.as_deref().unwrap_or("?")
        ),
        dev.name.as_deref().or(dev.card_type.as_deref()),
    )
}

/// The one entry the power bus reports.
fn power_line(state: PowerState) -> &'static str {
    match state {
        PowerState::Ac => "on AC",
        PowerState::Battery => "on battery",
        PowerState::Unknown => "unknown",
    }
}

/// What each bus currently sees, so a client can show the devices behind a bus
/// instead of only its on/off flag. `watched` is the same flag `status` reports
/// as `<key>_watching`.
///
/// Like `handle_status`, every enumeration and live reading happens with no
/// lock held: reading the lid can wait on logind, and the poll loop must not
/// stall behind a devices request. Nothing here needs the daemon state, so the
/// state lock is never taken at all.
fn handle_devices(config: &Arc<RwLock<Config>>) -> Response {
    let (general, iface_filter, display_ignore) = {
        let cfg = config.read().unwrap();
        (
            cfg.general.clone(),
            cfg.network.interfaces.clone(),
            cfg.display.ignore.clone(),
        )
    };

    let usb_entries = bus_entries(
        general.watch_usb,
        "USB",
        usb::enumerate_devices_detailed,
        |d| (usb_line(d), d.port.clone()),
    );
    let thunderbolt_entries = bus_entries(
        general.watch_thunderbolt,
        "Thunderbolt",
        thunderbolt::enumerate_thunderbolt_devices_detailed,
        |d| (thunderbolt_line(d), d.port.clone()),
    );
    let sdcard_entries = bus_entries(
        general.watch_sdcard,
        "SD card",
        sdcard::enumerate_sdcard_devices_detailed,
        |d| flat(sdcard_line(d)),
    );
    let pci_entries = bus_entries(
        general.watch_pci,
        "PCI",
        // Unfiltered by `pci.ignore`: that list says which addresses do not
        // kill, not which exist. Dropping them here left a client unable to
        // tell an ignored card from an absent one.
        || pci::enumerate_pci(&[]).map(|s| s.devices().iter().cloned().collect::<Vec<_>>()),
        // A PCI address is an identity, not a topology: the bridge a device
        // sits behind is only in the sysfs link, so the path comes from there.
        |a: &String| (a.clone(), pci_path(a)),
    );

    let network_entries: Vec<Entry> = if general.watch_network {
        network::enumerate_interfaces(&iface_filter)
            .interfaces()
            .iter()
            .map(|(iface, state)| flat(format!("{iface}: {state}")))
            .collect()
    } else {
        Vec::new()
    };
    let power_entries: Vec<Entry> = if general.watch_power {
        vec![flat(power_line(power::read_power_state()))]
    } else {
        Vec::new()
    };
    let lid_entries: Vec<Entry> = if general.watch_lid {
        vec![flat(lid_state_key(lid::read_lid_state()))]
    } else {
        Vec::new()
    };
    let display_entries: Vec<Entry> = if general.watch_display {
        display::watched_connectors(&display_ignore)
            .into_iter()
            .map(flat)
            .collect()
    } else {
        Vec::new()
    };

    Response::ok(serde_json::json!({"buses": {
        "usb": bus_json(general.watch_usb, usb_entries),
        "thunderbolt": bus_json(general.watch_thunderbolt, thunderbolt_entries),
        "sdcard": bus_json(general.watch_sdcard, sdcard_entries),
        "power": bus_json(general.watch_power, power_entries),
        "network": bus_json(general.watch_network, network_entries),
        "lid": bus_json(general.watch_lid, lid_entries),
        "pci": bus_json(general.watch_pci, pci_entries),
        "display": bus_json(general.watch_display, display_entries),
    }}))
}

fn handle_disarm(
    state: &Arc<Mutex<DaemonState>>,
    timeout_secs: u64,
    peer_uid: Option<u32>,
) -> Response {
    if timeout_secs == 0 {
        return Response::err("timeout_secs must be > 0");
    }

    const MAX_DISARM_SECS: u64 = 3600; // 1 hour max
    if timeout_secs > MAX_DISARM_SECS {
        return Response::err(format!(
            "timeout_secs must be <= {MAX_DISARM_SECS} (1 hour)"
        ));
    }

    let mut st = state.lock().unwrap();
    st.armed = false;
    st.disarm_until = Some(Instant::now() + Duration::from_secs(timeout_secs));
    info!(
        "daemon disarmed for {} by {}",
        format_duration(timeout_secs),
        peer_label(peer_uid)
    );

    Response::ok(serde_json::json!({
        "message": format!("disarmed for {}", format_duration(timeout_secs)),
        "disarm_until_secs": timeout_secs,
    }))
}

fn handle_arm(state: &Arc<Mutex<DaemonState>>, peer_uid: Option<u32>) -> Response {
    let mut st = state.lock().unwrap();
    // Only a disarmed -> armed transition re-baselines. `arm` is ungated, so
    // an arm on an already-armed daemon must not re-capture: that would bless
    // hardware or cancel a running grace with no authentication. `armed` is
    // cleared only by the gated disarm, so being disarmed here means someone
    // already authenticated.
    let was_disarmed = !st.armed;
    st.armed = true;
    st.disarm_until = None;
    if !was_disarmed {
        info!(
            "daemon already armed, nothing to do for {}",
            peer_label(peer_uid)
        );
        return Response::ok(serde_json::json!({
            "message": "already armed",
        }));
    }
    st.rebaseline_pending = true;
    info!(
        "daemon armed by {} (baselines will be re-captured)",
        peer_label(peer_uid)
    );

    Response::ok(serde_json::json!({
        "message": "armed (baselines will be re-captured on next poll)",
    }))
}

fn handle_learn(state: &Arc<Mutex<DaemonState>>, peer_uid: Option<u32>) -> Response {
    let mut st = state.lock().unwrap();
    st.mode = DaemonMode::Learn;
    info!("switched to learning mode by {}", peer_label(peer_uid));

    Response::ok(serde_json::json!({
        "message": "switched to learning mode",
    }))
}

fn handle_enforce(state: &Arc<Mutex<DaemonState>>, peer_uid: Option<u32>) -> Response {
    let mut st = state.lock().unwrap();
    st.mode = DaemonMode::Enforce;
    info!("switched to enforce mode by {}", peer_label(peer_uid));

    Response::ok(serde_json::json!({
        "message": "switched to enforce mode",
    }))
}

fn handle_reload(state: &Arc<Mutex<DaemonState>>, peer_uid: Option<u32>) -> Response {
    let mut st = state.lock().unwrap();
    st.reload_pending = true;
    info!("configuration reload scheduled by {}", peer_label(peer_uid));

    Response::ok(serde_json::json!({
        "message": "reload scheduled",
    }))
}

/// The pairing window cap, the same hour disarm has (B1).
const MAX_PAIR_SECS: u64 = 3600;

/// Whether this platform reports which monitor is attached. FreeBSD reports a
/// display event counter with no connector data, so a display allowance could
/// never match there (A2b).
pub(crate) const DISPLAY_IDENTITY: bool = cfg!(target_os = "linux");

/// The refusal for a display allowance on a platform that cannot identify a
/// monitor, or None for anything that can be allowed (G12). Also the guard the
/// poll loop uses before it admits a paired device.
pub(crate) fn display_limit(id: &DeviceRef, has_identity: bool) -> Option<String> {
    (matches!(id, DeviceRef::Display(_)) && !has_identity).then(|| {
        "this platform reports a display event counter with no connector data, \
         so a monitor cannot be identified and a display cannot be allowed"
            .to_string()
    })
}

/// `--for`, as an expiry. `Some(0)` would grant something already expired.
fn ttl(for_secs: Option<u64>) -> Result<Option<Duration>, Response> {
    match for_secs {
        Some(0) => Err(Response::err("for_secs must be > 0")),
        Some(secs) => Ok(Some(Duration::from_secs(secs))),
        None => Ok(None),
    }
}

/// `" (Kingston DataTraveler)"`, or nothing when no name is known.
fn named(name: &Option<String>) -> String {
    name.as_deref().map_or(String::new(), |n| format!(" ({n})"))
}

/// The active allowances as `status` reports them (D2).
fn allowances_json(table: &AllowanceTable, now: Instant) -> serde_json::Value {
    table
        .active(now)
        .map(|a| {
            serde_json::json!({
                "selector": a.id.selector(),
                "bus": a.id.bus(),
                "name": a.name,
                "granted_by": a.granted_by.as_str(),
                "age_secs": a.age(now).as_secs(),
                // Rounded up, like a pending grace: the last second before
                // an allowance lapses reads 1 rather than 0.
                "expires_in_secs": a.expires_in(now).map(|d| d.as_millis().div_ceil(1000) as u64),
            })
        })
        .collect()
}

/// Open a pairing window. A second `pair` replaces the deadline rather than
/// stacking, and 0 closes an open window (B3). What the window admits is the
/// poll loop's business: it closes the window on the first admission (B2).
fn handle_pair(
    state: &Arc<Mutex<DaemonState>>,
    window_secs: u64,
    for_secs: Option<u64>,
    peer_uid: Option<u32>,
) -> Response {
    if window_secs > MAX_PAIR_SECS {
        return Response::err(format!("window_secs must be <= {MAX_PAIR_SECS} (1 hour)"));
    }
    let ttl = match ttl(for_secs) {
        Ok(ttl) => ttl,
        Err(refusal) => return refusal,
    };

    let mut st = state.lock().unwrap();
    if window_secs == 0 {
        let was_open = st.pairing.take().is_some();
        info!("pairing window closed by {}", peer_label(peer_uid));
        return Response::ok(serde_json::json!({
            "message": if was_open { "pairing window closed" } else { "no pairing window was open" },
            "window_secs": 0,
        }));
    }

    st.pairing = Some(PairingWindow {
        until: Instant::now() + Duration::from_secs(window_secs),
        ttl,
    });
    info!(
        "pairing window open for {} by {}",
        format_duration(window_secs),
        peer_label(peer_uid)
    );
    Response::ok(serde_json::json!({
        "message": format!("pairing window open for {}", format_duration(window_secs)),
        "window_secs": window_secs,
    }))
}

/// Promote the device behind the most recent recorded violation (B4). Refused
/// with the reason when there is nothing recorded, or when what was recorded
/// carries no device identity (B6). `has_identity` is `DISPLAY_IDENTITY` at the
/// dispatch site; it is a parameter so the refusal a non-Linux build takes is
/// reachable from a test on any host (G12).
fn handle_allow_last(
    state: &Arc<Mutex<DaemonState>>,
    for_secs: Option<u64>,
    peer_uid: Option<u32>,
    has_identity: bool,
) -> Response {
    let ttl = match ttl(for_secs) {
        Ok(ttl) => ttl,
        Err(refusal) => return refusal,
    };

    let mut st = state.lock().unwrap();
    let Some(last) = st.last_violation().cloned() else {
        return Response::err("no violation has been recorded, so there is nothing to allow");
    };
    let Some(id) = last.id else {
        return Response::err(format!(
            "the last violation names no device, so there is nothing to allow: {}",
            last.description
        ));
    };
    if let Some(refusal) = display_limit(&id, has_identity) {
        return Response::err(refusal);
    }

    let selector = id.selector();
    let allowance =
        Allowance::new(id, Grant::Promoted, Instant::now(), ttl).with_name(last.name.clone());
    st.allowances.add(allowance);
    info!(
        "allowed {selector}{} promoted from the last violation, asked for by {}",
        named(&last.name),
        peer_label(peer_uid)
    );
    Response::ok(serde_json::json!({
        "message": format!("allowed {selector}"),
        "selector": selector,
    }))
}

/// Drop one allowance (C6). The selector parse carries the refusals for a bus
/// that has no devices to allow (A3) and for anything malformed.
fn handle_revoke(
    state: &Arc<Mutex<DaemonState>>,
    selector: &str,
    peer_uid: Option<u32>,
) -> Response {
    let id: DeviceRef = match selector.parse() {
        Ok(id) => id,
        Err(e) => return Response::err(e),
    };

    let mut st = state.lock().unwrap();
    match st.allowances.remove(&id) {
        Some(gone) => {
            info!(
                "revoked {}{} by {}",
                gone.id.selector(),
                named(&gone.name),
                peer_label(peer_uid)
            );
            Response::ok(serde_json::json!({
                "message": format!("revoked {}", gone.id.selector()),
                "selector": gone.id.selector(),
            }))
        }
        None => Response::err(format!("no allowance for {}", id.selector())),
    }
}

fn handle_revoke_all(state: &Arc<Mutex<DaemonState>>, peer_uid: Option<u32>) -> Response {
    let mut st = state.lock().unwrap();
    for a in st.allowances.iter() {
        info!("revoked {}{}", a.id.selector(), named(&a.name));
    }
    let count = st.allowances.clear();
    info!("revoked {count} allowance(s) by {}", peer_label(peer_uid));
    Response::ok(serde_json::json!({
        "message": format!("revoked {count} allowance(s)"),
        "revoked": count,
    }))
}

/// Record a kill request. The main loop runs the sequence: it owns the config
/// and the single `kill::execute_kill_sequence` call site, so a socket kill
/// honors dry_run, `[destruction]` and `commands.kill_commands` exactly as a
/// locally detected violation does.
///
/// Refuses in two cases, both of which make the relay fall back to a direct
/// poweroff rather than leaving the request silently unhonored:
/// non-root peers, and learn mode.
fn handle_kill(state: &Arc<Mutex<DaemonState>>, reason: &str, peer_uid: Option<u32>) -> Response {
    // Authorization before mode: an unauthorized caller learns nothing about
    // the daemon's state.
    if !kill_authorized(peer_uid) {
        warn!("refused kill command from non-root peer uid {peer_uid:?}: {reason}");
        return Response::err("kill requires root");
    }

    let mut st = state.lock().unwrap();

    // Learn mode does not suppress a peer's kill. Unlike a local violation it
    // carries no uncalibrated-baseline false-positive risk, and swallowing it
    // would let one `learn` command neutralize the fleet kill switch. Record it
    // on the way out: the node is about to go down on the relay's fallback.
    if st.mode == DaemonMode::Learn {
        st.violations_logged += 1;
        warn!("LEARN mode: RELAY VIOLATION: remote kill from peer: {reason}");
        return Response::err("daemon in learn mode, refusing remote kill");
    }

    st.kill_pending = Some(reason.to_string());
    error!("kill sequence requested via socket command: {reason}");

    Response::ok(serde_json::json!({
        "message": "kill sequence scheduled",
    }))
}

/// Remove the socket file (for clean shutdown).
pub fn cleanup_socket(socket_path: &Path) {
    if socket_path.exists()
        && let Err(e) = std::fs::remove_file(socket_path)
    {
        warn!("failed to remove socket {}: {e}", socket_path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_state::{Grace, Violation};
    use plugkill_core::config::{LidPolicy, NetworkPolicy, PowerPolicy};
    use plugkill_core::state::DeviceNames;

    /// An authority that answers the same way every time and records every
    /// call, so a test can assert it was never consulted. No polkit, no agent,
    /// no prompt: nothing here can stop on someone's desktop waiting for a
    /// password.
    struct FakeAuthority {
        allow: bool,
        calls: Mutex<Vec<(String, u32, u32)>>,
    }

    impl FakeAuthority {
        fn allowing() -> Self {
            Self {
                allow: true,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn refusing() -> Self {
            Self {
                allow: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, u32, u32)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Authority for FakeAuthority {
        fn check(&self, action: &str, caller_uid: u32, caller_pid: u32) -> Authorization {
            self.calls
                .lock()
                .unwrap()
                .push((action.to_string(), caller_uid, caller_pid));
            if self.allow {
                Authorization::Allowed
            } else {
                Authorization::Refused("no polkit agent".to_string())
            }
        }
    }

    fn empty_baselines() -> Baselines {
        Baselines {
            usb: None,
            thunderbolt: None,
            sdcard: None,
            power: None,
            network: None,
            lid: None,
            pci: None,
            display: None,
            names: DeviceNames::default(),
        }
    }

    /// Helper: bind a listener on a socket inside a fresh temp dir.
    /// Returns (tempdir guard, socket path, the shared state the listener mutates).
    fn start_test_listener() -> (tempfile::TempDir, PathBuf, Arc<Mutex<DaemonState>>) {
        start_test_listener_with(Config::default(), Arc::new(FakeAuthority::refusing()))
    }

    fn start_test_listener_with(
        config: Config,
        authority: Arc<dyn Authority>,
    ) -> (tempfile::TempDir, PathBuf, Arc<Mutex<DaemonState>>) {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("plugkill.sock");
        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        let config = Arc::new(RwLock::new(config));
        let baselines = Arc::new(RwLock::new(empty_baselines()));

        start_socket_listener(
            socket_path.clone(),
            None,
            Arc::clone(&state),
            config,
            baselines,
            authority,
        )
        .unwrap();

        (dir, socket_path, state)
    }

    fn send(socket_path: &Path, request: serde_json::Value) -> serde_json::Value {
        let resp = plugkill_core::ipc::send_request(socket_path, &request)
            .unwrap_or_else(|e| panic!("request {request} failed: {e}"));
        assert!(
            resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            "request {request} rejected: {resp}"
        );
        resp
    }

    /// C1, H1. The settings window reads the config off the path the daemon
    /// loaded, so status has to report that path and not a default guess, plus
    /// the version it is running.
    #[test]
    fn test_status_reports_the_loaded_config_path_and_version() {
        let (_dir, socket_path, state) = start_test_listener();
        state.lock().unwrap().config_path = PathBuf::from("/etc/plugkill/other.toml");

        let resp = send(&socket_path, serde_json::json!({"command": "status"}));
        let data = resp.get("data").expect("status response has no data");

        assert_eq!(
            data.get("config_path").and_then(|v| v.as_str()),
            Some("/etc/plugkill/other.toml")
        );
        assert_eq!(
            data.get("version").and_then(|v| v.as_str()),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    /// E2, H3, H4. The history comes back newest first, with the fields the
    /// panel needs. Ungatedness is proved by
    /// `test_ungated_commands_never_consult_the_authority`.
    #[test]
    fn test_violations_returns_the_history_newest_first() {
        let (_dir, socket_path, state) = start_test_listener();
        {
            let mut st = state.lock().unwrap();
            st.record_violation(Violation::event("lid", "LID VIOLATION: closed".to_string()));
            st.record_violation(Violation::device(
                "USB VIOLATION: added".to_string(),
                usb_ref(),
                Some("DataTraveler".to_string()),
                true,
            ));
        }

        let resp = send(&socket_path, serde_json::json!({"command": "violations"}));
        let rows = resp
            .pointer("/data/violations")
            .and_then(|v| v.as_array())
            .expect("the reply carries a violations array")
            .clone();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("message").unwrap(), "USB VIOLATION: added");
        assert_eq!(rows[0].get("bus").unwrap(), "usb");
        assert_eq!(rows[0].get("selector").unwrap(), &json_str(&usb_ref()));
        assert_eq!(rows[0].get("name").unwrap(), "DataTraveler");
        assert!(
            rows[0].get("at_unix").and_then(|v| v.as_u64()).unwrap() > 1_700_000_000,
            "each row carries the second it happened: {}",
            rows[0]
        );

        assert_eq!(rows[1].get("bus").unwrap(), "lid");
        assert!(
            rows[1].get("selector").unwrap().is_null(),
            "an event names no device: {}",
            rows[1]
        );
    }

    /// An enumeration failure spends its first 65 characters on the prefix, so
    /// a device-table-sized cap would cut off the cause.
    #[test]
    fn test_a_long_violation_message_keeps_its_cause() {
        let (_dir, socket_path, state) = start_test_listener();
        let long = format!(
            "USB VIOLATION: enumeration failure (possible tampering): {}",
            "failed to read /sys/bus/usb/devices: permission denied on a very long path"
        );
        state
            .lock()
            .unwrap()
            .record_violation(Violation::event("usb", long.clone()));

        let resp = send(&socket_path, serde_json::json!({"command": "violations"}));

        assert_eq!(
            resp.pointer("/data/violations/0/message")
                .and_then(|v| v.as_str()),
            Some(long.as_str()),
            "the cause must survive: {resp}"
        );

        // The two things the cap and the scrub are for, neither of which a
        // clean 131-character literal exercises.
        state.lock().unwrap().record_violation(Violation::event(
            "usb",
            format!("USB VIOLATION: two\nlines, and {}", "x".repeat(MAX_MESSAGE)),
        ));
        let resp = send(&socket_path, serde_json::json!({"command": "violations"}));
        let message = resp
            .pointer("/data/violations/0/message")
            .and_then(|v| v.as_str())
            .expect("the newest row carries its message");
        assert!(
            !message.contains('\n'),
            "one message, one paragraph: {message:?}"
        );
        assert_eq!(message.chars().count(), MAX_MESSAGE, "{message:?}");
    }

    /// A device names itself, so a violation name is whatever its descriptor
    /// says. The panel draws it in a wrapping label, where an embedded newline
    /// reads as a second device that was never there.
    #[test]
    fn test_a_violation_name_is_one_short_line() {
        let (_dir, socket_path, state) = start_test_listener();
        let nasty = format!("Mouse\n1050:0407 Yubico YubiKey{}", "!".repeat(100));
        state.lock().unwrap().record_violation(Violation::device(
            "USB VIOLATION: added".to_string(),
            usb_ref(),
            Some(nasty),
            true,
        ));

        let resp = send(&socket_path, serde_json::json!({"command": "violations"}));
        let name = resp
            .pointer("/data/violations/0/name")
            .and_then(|v| v.as_str())
            .expect("the row carries the name");

        assert!(!name.contains('\n'), "one name, one row: {name:?}");
        assert!(name.chars().count() <= MAX_LINE, "{name:?}");
    }

    fn json_str(id: &DeviceRef) -> serde_json::Value {
        serde_json::Value::String(id.selector())
    }

    /// The tray and the CLI both read the watched buses out of a status
    /// response by the keys in `ipc::BUSES`. A key named there but not emitted
    /// here reads as "this bus is off" rather than as an error, so the bus goes
    /// quietly missing from both. That is how the tray came to show six of the
    /// eight buses.
    #[test]
    fn test_status_emits_every_bus_key() {
        let (_dir, socket_path, _state) = start_test_listener();
        let resp = send(&socket_path, serde_json::json!({"command": "status"}));
        let data = resp.get("data").expect("status response has no data");
        for (key, label) in plugkill_core::ipc::BUSES {
            assert!(
                data.get(key).is_some_and(|v| v.is_boolean()),
                "status response is missing a boolean {key} for {label}: {data}"
            );
        }
    }

    /// The eight bus keys a `devices` response carries. The order here is the
    /// order everything prints in; the JSON object itself is keyed by name and
    /// both sides look a bus up by key, so the order it serializes in is not
    /// part of the contract.
    const BUS_KEYS: [&str; 8] = [
        "usb",
        "thunderbolt",
        "sdcard",
        "power",
        "network",
        "lid",
        "pci",
        "display",
    ];

    /// Every watch flag off, so the enumerators never touch the host.
    fn nothing_watched() -> Config {
        let mut cfg = Config::default();
        cfg.general.watch_usb = false;
        cfg.general.watch_thunderbolt = false;
        cfg.general.watch_sdcard = false;
        cfg
    }

    /// `devices` and `status` must name the same eight buses, or a client
    /// keying one listing off the other silently drops a bus.
    #[test]
    fn test_device_bus_keys_are_the_status_bus_keys() {
        let from_status: Vec<&str> = plugkill_core::ipc::BUSES
            .iter()
            .map(|(key, _)| {
                key.strip_suffix("_watching")
                    .expect("a status bus key ends in _watching")
            })
            .collect();
        assert_eq!(from_status, BUS_KEYS);
    }

    /// All eight keys are always present, and an unwatched bus says so with an
    /// empty list rather than going missing.
    #[test]
    fn test_devices_reports_every_bus_and_unwatched_ones_are_empty() {
        let (_dir, socket_path, _state) =
            start_test_listener_with(nothing_watched(), Arc::new(FakeAuthority::refusing()));
        let resp = send(&socket_path, serde_json::json!({"command": "devices"}));
        let buses = &resp["data"]["buses"];

        for key in BUS_KEYS {
            let bus = buses
                .get(key)
                .unwrap_or_else(|| panic!("devices response is missing {key}: {buses}"));
            assert_eq!(bus["watched"], false, "{key}: {bus}");
            assert_eq!(bus["entries"], serde_json::json!([]), "{key}: {bus}");
            assert_eq!(bus["more"], 0, "{key}: {bus}");
        }
    }

    /// Power and lid are single readings, so each reports exactly one entry.
    /// Asserted on the helpers, not over the socket: watching those two buses
    /// would read this machine's hardware, lid state through a blocking logind
    /// call. The response shape is covered by the test above.
    #[test]
    fn test_devices_power_and_lid_report_one_entry() {
        let power = bus_json(true, vec![flat(power_line(PowerState::Ac))]);
        assert_eq!(
            power["entries"],
            serde_json::json!([{"text": "on AC"}]),
            "{power}"
        );
        assert_eq!(power["watched"], true, "{power}");
        assert_eq!(power["more"], 0, "{power}");

        let lid = bus_json(true, vec![flat(lid_state_key(LidState::Closed))]);
        assert_eq!(
            lid["entries"],
            serde_json::json!([{"text": "closed"}]),
            "{lid}"
        );
        assert_eq!(lid["watched"], true, "{lid}");
        assert_eq!(lid["more"], 0, "{lid}");

        // The entry wording is its own: the status field says "ac", the entry
        // says "on AC". The lid says the same word in both.
        assert_eq!(power_line(PowerState::Battery), "on battery");
        assert_eq!(power_line(PowerState::Unknown), "unknown");
    }

    #[test]
    fn test_bus_json_sorts_caps_and_counts_the_rest() {
        let bus = bus_json(true, vec![flat("b"), flat("a")]);
        assert_eq!(
            bus["entries"],
            serde_json::json!([{"text": "a"}, {"text": "b"}])
        );
        assert_eq!(bus["more"], 0);

        let many: Vec<Entry> = (0..15).rev().map(|i| flat(format!("dev{i:02}"))).collect();
        let bus = bus_json(true, many);
        let entries = bus["entries"].as_array().unwrap();
        assert_eq!(entries.len(), MAX_ENTRIES, "{bus}");
        assert_eq!(entries[0]["text"], "dev00", "entries must be sorted: {bus}");
        assert_eq!(bus["more"], 3, "{bus}");
    }

    /// An entry crosses the socket, so it must never carry a serial number or
    /// any other per-unit id: those identify the exact piece of hardware.
    #[test]
    fn test_entries_carry_no_serial_number() {
        let usb = usb_line(&UsbDeviceInfo {
            vendor_id: "1d6b".to_string(),
            product_id: "0002".to_string(),
            manufacturer: Some("Linux Foundation".to_string()),
            product: Some("2.0 root hub".to_string()),
            serial: Some("SN-USB-0001".to_string()),
            speed: None,
            busnum: None,
            devnum: None,
            port: Some("usb1".to_string()),
        });
        assert_eq!(usb, "1d6b:0002 2.0 root hub");

        let tb = thunderbolt_line(&ThunderboltDeviceInfo {
            unique_id: "0123-4567-89ab-cdef".to_string(),
            vendor_id: "0x8086".to_string(),
            device_id: "0x1234".to_string(),
            vendor_name: Some("Intel".to_string()),
            device_name: Some("Thunderbolt Dock".to_string()),
            authorized: Some("1".to_string()),
            generation: Some("4".to_string()),
            port: Some("0-1".to_string()),
        });
        assert_eq!(tb, "0x8086:0x1234 Thunderbolt Dock");

        let sd = sdcard_line(&SdCardDeviceInfo {
            serial: "0xdeadbeef".to_string(),
            name: Some("SD32G".to_string()),
            card_type: Some("SD".to_string()),
            cid: Some("035344533332470xdeadbeef".to_string()),
            manfid: Some("0x000003".to_string()),
            oemid: Some("0x5344".to_string()),
            date: Some("08/2021".to_string()),
        });
        assert_eq!(sd, "0x000003:0x5344 SD32G");

        for line in [&usb, &tb, &sd] {
            for id in ["SN-USB-0001", "0123-4567-89ab-cdef", "0xdeadbeef"] {
                assert!(!line.contains(id), "{line} leaks the per-unit id {id}");
            }
        }
    }

    /// A device names itself, so a descriptor can carry a newline and pass for
    /// two devices in a client that prints one entry per row.
    #[test]
    fn test_entries_are_one_short_line() {
        let dev = |product: &str| UsbDeviceInfo {
            vendor_id: "1d6b".to_string(),
            product_id: "0002".to_string(),
            manufacturer: None,
            product: Some(product.to_string()),
            serial: None,
            speed: None,
            busnum: None,
            devnum: None,
            port: Some("2-3".to_string()),
        };
        let bus = bus_json(
            true,
            vec![
                flat(usb_line(&dev("Hub\n1050:0407 Yubico"))),
                flat(usb_line(&dev(&"A".repeat(200)))),
            ],
        );
        let entries = bus["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "{bus}");
        for entry in entries {
            let line = entry["text"].as_str().unwrap();
            assert!(!line.contains('\n'), "{line} is two lines");
            assert!(line.chars().count() <= MAX_LINE, "{line} is not short");
        }
    }

    /// Every entry is an object with a `text`, and a `path` only where the bus
    /// has topology. This is the shape a client nests by, so it is asserted
    /// whole rather than field by field.
    #[test]
    fn test_entry_objects_carry_text_and_an_optional_path() {
        let bus = bus_json(
            true,
            vec![
                ("1050:0407 YubiKey".to_string(), Some("2-3.4".to_string())),
                ("closed".to_string(), None),
            ],
        );
        assert_eq!(
            bus["entries"],
            serde_json::json!([
                {"text": "1050:0407 YubiKey", "path": "2-3.4"},
                {"text": "closed"},
            ]),
            "{bus}"
        );
    }

    /// A USB entry's path is the sysfs directory name, which is what encodes
    /// the tree, and an interface directory is not a device at all.
    #[test]
    fn test_usb_paths_are_sysfs_names_and_interfaces_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // A root hub, a device on it, a device behind that one, and an
        // interface of the last, which must not show up.
        for (name, vendor, product) in [
            ("usb2", "1d6b", "0003"),
            ("2-3", "05e3", "0610"),
            ("2-3.4", "1050", "0407"),
            ("2-3.4:1.0", "1050", "0407"),
        ] {
            let dev = dir.path().join(name);
            std::fs::create_dir(&dev).unwrap();
            for (attr, val) in [("idVendor", vendor), ("idProduct", product)] {
                let mut f = std::fs::File::create(dev.join(attr)).unwrap();
                write!(f, "{val}").unwrap();
            }
        }

        let devices = usb::enumerate_devices_detailed_from(dir.path()).unwrap();
        let bus = bus_json(
            true,
            devices
                .iter()
                .map(|d| (usb_line(d), d.port.clone()))
                .collect(),
        );
        let paths: Vec<&str> = bus["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["path"].as_str())
            .collect();
        assert_eq!(
            paths.len(),
            3,
            "an interface directory is not a device: {bus}"
        );
        for want in ["usb2", "2-3", "2-3.4"] {
            assert!(paths.contains(&want), "missing {want}: {bus}");
        }
    }

    /// A PCI path is the bridges above the device, so a client nests on it.
    /// The address alone cannot do that: 0000:01:00.0 shares no prefix with
    /// the bridge 0000:00:1c.0 it hangs off.
    #[test]
    fn test_a_pci_path_is_the_bridge_chain_and_not_the_address() {
        let device = Path::new("../../../devices/pci0000:00/0000:00:1c.0/0000:01:00.0");
        let bridge = Path::new("../../../devices/pci0000:00/0000:00:1c.0");
        let device = pci_topology(device).expect("a device link is a path");
        let bridge = pci_topology(bridge).expect("a bridge link is a path");
        assert_eq!(device, "pci0000:00/0000:00:1c.0/0000:01:00.0");
        assert_eq!(bridge, "pci0000:00/0000:00:1c.0");
        assert!(device.starts_with(&format!("{bridge}/")), "{device}");
        // Nothing under devices/, and nothing that is a device link at all.
        assert_eq!(pci_topology(Path::new("../../../devices")), None);
        assert_eq!(pci_topology(Path::new("/proc/self")), None);
    }

    /// A device the enumerator could not place reports no path at all, rather
    /// than an empty string a client would have to special-case.
    #[test]
    fn test_an_entry_without_a_path_omits_the_key() {
        let bus = bus_json(
            true,
            vec![flat("wlan0: up"), ("x".to_string(), Some(String::new()))],
        );
        for entry in bus["entries"].as_array().unwrap() {
            assert!(entry.get("path").is_none(), "{entry} carries an empty path");
        }
    }

    /// A device with no product name still gets a line: the ids alone.
    #[test]
    fn test_usb_entry_falls_back_to_manufacturer_then_bare_ids() {
        let dev = |product, manufacturer| UsbDeviceInfo {
            vendor_id: "046d".to_string(),
            product_id: "c52b".to_string(),
            manufacturer,
            product,
            serial: None,
            speed: None,
            busnum: None,
            devnum: None,
            port: None,
        };
        assert_eq!(
            usb_line(&dev(None, Some("Logitech".to_string()))),
            "046d:c52b Logitech"
        );
        assert_eq!(usb_line(&dev(None, None)), "046d:c52b");
        assert_eq!(usb_line(&dev(Some(String::new()), None)), "046d:c52b");
    }

    /// A manual `plugkill --arm` must schedule a baseline re-capture, otherwise
    /// it re-arms against the baseline captured before the disarm window and
    /// any device attached during that window becomes accepted state.
    #[test]
    fn test_socket_arm_sets_rebaseline_pending() {
        let (_dir, socket_path, state) = start_test_listener();

        send(
            &socket_path,
            serde_json::json!({"command": "disarm", "timeout_secs": 60}),
        );
        {
            let st = state.lock().unwrap();
            assert!(!st.armed, "disarm must clear armed");
            assert!(
                !st.rebaseline_pending,
                "disarm must not schedule a re-baseline"
            );
        }

        send(&socket_path, serde_json::json!({"command": "arm"}));

        let st = state.lock().unwrap();
        assert!(st.armed, "arm must set armed");
        assert!(
            st.disarm_until.is_none(),
            "arm must clear the disarm window"
        );
        assert!(
            st.rebaseline_pending,
            "arm must schedule a baseline re-capture"
        );
    }

    fn state() -> Arc<Mutex<DaemonState>> {
        Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)))
    }

    /// A journal line has to say who, and stay readable when the kernel gives
    /// us nothing or the uid has no passwd entry.
    #[test]
    fn test_peer_label_names_the_caller() {
        assert_eq!(peer_label(Some(0)), "uid 0 (root)");
        assert_eq!(peer_label(Some(4_294_967_294)), "uid 4294967294");
        assert_eq!(peer_label(None), "an unidentified peer");
    }

    /// `peer_creds` must actually read the peer on this platform. Without this,
    /// a `getsockopt` that always failed would look identical to a working gate
    /// in every other test here (`None` denies, and denial is what they assert)
    /// while in production it would deny root too and reduce every remote kill
    /// to a bare poweroff with nothing shredded. The pid matters for the same
    /// reason: no pid is no subject, and every gated command refuses.
    #[test]
    fn test_peer_creds_read_the_connecting_uid_and_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cred.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        let creds =
            peer_creds(&server_side).expect("peer credentials must be readable on this platform");
        assert_eq!(
            creds.uid,
            nix::unistd::geteuid().as_raw(),
            "peer credentials must be readable, or the kill gate denies everyone"
        );
        // The client is this process, so its pid is ours.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(
            creds.pid,
            Some(std::process::id()),
            "the pid is the subject handed to the authority"
        );
    }

    /// uid 1000 with a pid: the ordinary caller the gate is about.
    const USER: Option<PeerCreds> = Some(PeerCreds {
        uid: 1000,
        pid: Some(4242),
    });
    const ROOT: Option<PeerCreds> = Some(PeerCreds {
        uid: 0,
        pid: Some(1),
    });

    fn gated() -> Vec<Request> {
        vec![
            Request::Disarm { timeout_secs: 60 },
            Request::Learn,
            Request::Reload,
            Request::Pair {
                window_secs: 60,
                for_secs: None,
            },
        ]
    }

    fn ungated() -> Vec<Request> {
        vec![
            Request::Status,
            Request::Devices,
            Request::Violations,
            Request::Arm,
            Request::Enforce,
            Request::RevokeAll,
        ]
    }

    /// The handles a request handler needs, with nothing watched so no test
    /// here reads this machine's hardware.
    #[allow(clippy::type_complexity)]
    fn parts(
        require_auth: bool,
    ) -> (
        Arc<Mutex<DaemonState>>,
        Arc<RwLock<Config>>,
        Arc<RwLock<Baselines>>,
    ) {
        let mut cfg = nothing_watched();
        cfg.general.require_auth = require_auth;
        (
            state(),
            Arc::new(RwLock::new(cfg)),
            Arc::new(RwLock::new(empty_baselines())),
        )
    }

    /// A refused authority stops disarm, learn and reload, and the state is
    /// exactly as it was: the point of the gate is that the command never ran.
    #[test]
    fn test_gated_commands_refuse_when_the_authority_refuses() {
        for req in gated() {
            let action = gated_action(&req).expect("this command is gated");
            let (st, cfg, bl) = parts(true);
            let fake = FakeAuthority::refusing();

            let resp = handle_request(req, &st, &cfg, &bl, USER, &fake);

            assert!(!resp.ok, "{action} must be refused");
            let error = resp.error.expect("a refusal carries an error");
            assert!(
                error.contains(AUTH_REQUIRED_ERROR),
                "{action}: a client must be able to recognise this: {error}"
            );
            assert_eq!(fake.calls(), vec![(action.to_string(), 1000, 4242)]);

            let s = st.lock().unwrap();
            assert!(s.armed, "{action} ran anyway: disarmed");
            assert_eq!(s.mode, DaemonMode::Enforce, "{action} ran anyway: mode");
            assert!(!s.reload_pending, "{action} ran anyway: reload scheduled");
        }
    }

    #[test]
    fn test_gated_commands_run_when_the_authority_allows() {
        for req in gated() {
            let action = gated_action(&req).expect("this command is gated");
            let (st, cfg, bl) = parts(true);
            let fake = FakeAuthority::allowing();

            let resp = handle_request(req, &st, &cfg, &bl, USER, &fake);

            assert!(resp.ok, "{action} must run once allowed: {:?}", resp.error);
            assert_eq!(
                fake.calls(),
                vec![(action.to_string(), 1000, 4242)],
                "{action} must be checked under its own action id, for the caller's own uid and pid"
            );
        }
    }

    /// `arm` and `enforce` only make the daemon stricter, and `status`,
    /// `devices` and `violations` change nothing. None of them may stop on a password prompt,
    /// so the authority must not be touched at all.
    #[test]
    fn test_ungated_commands_never_consult_the_authority() {
        for req in ungated() {
            let label = format!("{req:?}");
            let (st, cfg, bl) = parts(true);
            let fake = FakeAuthority::refusing();

            let resp = handle_request(req, &st, &cfg, &bl, USER, &fake);

            assert!(resp.ok, "{label} must run: {:?}", resp.error);
            assert!(
                fake.calls().is_empty(),
                "{label} consulted the authority: {:?}",
                fake.calls()
            );
        }
    }

    /// `arm` is ungated, so an arm on a daemon that is already armed must not
    /// re-capture baselines: that would bless a hardware change, or cancel a
    /// running grace, with nothing authenticated.
    #[test]
    fn test_arm_while_armed_does_not_rebaseline() {
        let (st, cfg, bl) = parts(true);
        let fake = FakeAuthority::refusing();
        assert!(st.lock().unwrap().armed, "the daemon starts armed");

        let resp = handle_request(Request::Arm, &st, &cfg, &bl, USER, &fake);

        assert!(resp.ok, "arm stays ungated: {:?}", resp.error);
        assert!(
            fake.calls().is_empty(),
            "arm must not consult the authority"
        );

        let s = st.lock().unwrap();
        assert!(s.armed, "still armed");
        assert!(
            !s.rebaseline_pending,
            "an arm that changes nothing must not re-capture baselines"
        );
    }

    /// Off by default means off: no bus call, no prompt, no refusal.
    #[test]
    fn test_require_auth_off_consults_nothing() {
        for req in gated() {
            let label = format!("{req:?}");
            let (st, cfg, bl) = parts(false);
            let fake = FakeAuthority::refusing();

            let resp = handle_request(req, &st, &cfg, &bl, USER, &fake);

            assert!(resp.ok, "{label} must run: {:?}", resp.error);
            assert!(
                fake.calls().is_empty(),
                "{label} consulted the authority with require_auth off"
            );
        }
    }

    /// `sudo plugkill --disarm` has to keep working on a headless box, where
    /// there is no agent and the authority refuses everything.
    #[test]
    fn test_root_passes_an_authority_that_always_refuses() {
        for req in gated() {
            let label = format!("{req:?}");
            let (st, cfg, bl) = parts(true);
            let fake = FakeAuthority::refusing();

            let resp = handle_request(req, &st, &cfg, &bl, ROOT, &fake);

            assert!(resp.ok, "root must pass: {label}: {:?}", resp.error);
            assert!(fake.calls().is_empty(), "root must not be asked: {label}");
        }
    }

    /// No pid is no subject to ask about, so it is a refusal and not an
    /// unchecked pass. This is the FreeBSD path, where `LocalPeerCred` carries
    /// no pid and there is no polkit either.
    #[test]
    fn test_a_caller_without_a_pid_is_refused() {
        let (st, cfg, bl) = parts(true);
        let fake = FakeAuthority::allowing();
        let no_pid = Some(PeerCreds {
            uid: 1000,
            pid: None,
        });

        let resp = handle_request(
            Request::Disarm { timeout_secs: 60 },
            &st,
            &cfg,
            &bl,
            no_pid,
            &fake,
        );

        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains(AUTH_REQUIRED_ERROR));
        assert!(
            fake.calls().is_empty(),
            "there is no subject to hand the authority"
        );
        assert!(st.lock().unwrap().armed, "a refused disarm leaves it armed");
    }

    /// The refusal has to survive the wire: a client reads it as an ordinary
    /// error response whose message it can recognise.
    ///
    /// The outcome depends on the uid of whoever runs the suite, the same way
    /// the kill dispatch test does: the Linux CI job runs as a normal user and
    /// gets the refusal, the FreeBSD job runs as root and gets the root pass.
    #[test]
    fn test_a_refusal_reaches_the_client_through_the_socket() {
        let mut cfg = nothing_watched();
        cfg.general.require_auth = true;
        let fake = Arc::new(FakeAuthority::refusing());
        let (_dir, socket_path, state) =
            start_test_listener_with(cfg, Arc::clone(&fake) as Arc<dyn Authority>);

        let resp = plugkill_core::ipc::send_request(
            &socket_path,
            &serde_json::json!({"command": "disarm", "timeout_secs": 60}),
        )
        .expect("transport should succeed; the refusal is at the application level");

        if nix::unistd::geteuid().is_root() {
            assert_eq!(
                resp.get("ok").and_then(|v| v.as_bool()),
                Some(true),
                "{resp}"
            );
            assert!(fake.calls().is_empty(), "root must not be asked: {resp}");
        } else {
            assert_eq!(
                resp.get("ok").and_then(|v| v.as_bool()),
                Some(false),
                "a non-root disarm must be refused: {resp}"
            );
            assert!(
                resp["error"]
                    .as_str()
                    .unwrap()
                    .contains(AUTH_REQUIRED_ERROR),
                "the client must be able to recognise this: {resp}"
            );
            assert!(
                state.lock().unwrap().armed,
                "a refused disarm must leave the daemon armed: {resp}"
            );
        }
    }

    /// Only root may run the kill command. Peer credentials we cannot read are
    /// not root either: unknown must fail closed, or an unsupported platform
    /// silently becomes an open destructive endpoint.
    #[test]
    fn test_kill_authorized_only_for_root() {
        assert!(kill_authorized(Some(0)), "root may kill");
        assert!(!kill_authorized(Some(1000)), "a normal user may not kill");
        assert!(
            !kill_authorized(None),
            "unavailable peer credentials must fail closed"
        );
    }

    /// The wire path the relay actually uses: a `kill` line over a real socket
    /// has to reach `handle_kill`. Covers the dispatch arm between
    /// `Request::Kill` and the handler in both environments this suite runs
    /// in, because the outcome is the uid gate's and so depends on the uid:
    /// the Linux CI job runs as a normal user and gets the refusal the
    /// group-writable socket exposes in production, while the FreeBSD CI job
    /// runs as root and gets the authorized path. Asserting only one of them
    /// would hard-fail in the other job.
    #[test]
    fn test_socket_kill_command_dispatches_to_handle_kill() {
        let (_dir, socket_path, state) = start_test_listener();
        let reason = "peer alpha lost AC power";

        let resp = plugkill_core::ipc::send_request(
            &socket_path,
            &serde_json::json!({"command": "kill", "reason": reason}),
        )
        .expect("transport should succeed; any refusal is at the application level");

        if nix::unistd::geteuid().is_root() {
            assert_eq!(
                resp.get("ok").and_then(|v| v.as_bool()),
                Some(true),
                "a root kill must be authorized: {resp}"
            );
            assert_eq!(
                state.lock().unwrap().kill_pending.as_deref(),
                Some(reason),
                "an authorized kill must queue its reason for the main loop"
            );
        } else {
            assert_eq!(
                resp.get("ok").and_then(|v| v.as_bool()),
                Some(false),
                "a non-root kill must be refused: {resp}"
            );
            assert!(
                resp["error"].as_str().unwrap().contains("root"),
                "the refusal must say it requires root: {resp}"
            );
            assert!(
                state.lock().unwrap().kill_pending.is_none(),
                "a refused kill must not queue anything for the main loop"
            );
        }
    }

    #[test]
    fn test_handle_kill_sets_pending() {
        let st = state();
        let resp = handle_kill(&st, "peer alpha lost AC power", Some(0));
        assert!(resp.ok);
        assert_eq!(
            st.lock().unwrap().kill_pending.as_deref(),
            Some("peer alpha lost AC power")
        );
    }

    #[test]
    fn test_handle_kill_does_not_run_on_socket_thread() {
        // The socket handler only records the request. The main loop drains it
        // through the same path a local violation takes, so nothing here may
        // flip armed/mode or count a violation.
        let st = state();
        handle_kill(&st, "test", Some(0));
        let s = st.lock().unwrap();
        assert!(s.armed);
        assert_eq!(s.mode, DaemonMode::Enforce);
        assert_eq!(s.violations_logged, 0);
    }

    /// Learn mode must not silently swallow a peer's kill. Refusing with
    /// `ok:false` is what makes the relay's `force_poweroff` fallback fire, so
    /// a remote kill always has an effect.
    #[test]
    fn test_handle_kill_refused_in_learn_mode() {
        let st = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Learn)));

        let resp = handle_kill(&st, "peer alpha lost AC power", Some(0));

        assert!(!resp.ok, "learn mode must refuse, not silently accept");
        assert!(resp.error.unwrap().contains("learn mode"));
        let s = st.lock().unwrap();
        assert!(
            s.kill_pending.is_none(),
            "learn mode must not queue a kill for the main loop"
        );
        assert_eq!(
            s.violations_logged, 1,
            "the refused kill must still be recorded locally"
        );
    }

    /// Authorization is checked before mode, so a non-root kill is refused as
    /// unauthorized rather than leaking whether the node is in learn mode.
    #[test]
    fn test_handle_kill_checks_authorization_before_mode() {
        let st = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Learn)));

        let resp = handle_kill(&st, "test", Some(1000));

        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("root"));
        assert_eq!(
            st.lock().unwrap().violations_logged,
            0,
            "an unauthorized request must not be counted as a learn-mode violation"
        );
    }

    #[test]
    fn test_disarm_message_uses_format_duration() {
        let st = state();
        let resp = handle_disarm(&st, 90, Some(1000));
        assert!(resp.ok);
        let data = resp.data.expect("disarm response carries data");
        assert_eq!(data["message"], "disarmed for 1m 30s");
        assert_eq!(data["disarm_until_secs"], 90);
    }

    /// Power, network and lid watched under policies that can fire.
    fn kill_policies_watched() -> Config {
        let mut cfg = Config::default();
        cfg.general.watch_power = true;
        cfg.general.watch_network = true;
        cfg.general.watch_lid = true;
        cfg.power.policy = PowerPolicy::AcRequired;
        cfg.network.policy = NetworkPolicy::Kill;
        cfg.lid.policy = LidPolicy::Kill;
        cfg
    }

    fn grace_at(now: Instant, secs: u64, reason: &str) -> Grace {
        Grace {
            until: now + Duration::from_secs(secs),
            reason: reason.to_string(),
        }
    }

    #[test]
    fn test_pending_violation_is_null_without_a_grace() {
        let st = DaemonState::new_for_test(DaemonMode::Enforce);
        let value = pending_violation_json(&st, &kill_policies_watched(), Instant::now());
        assert!(value.is_null(), "{value}");
    }

    #[test]
    fn test_pending_violation_reports_the_soonest_grace() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.network_grace = Some(grace_at(now, 40, "link down on eth0"));
        st.lid_grace = Some(grace_at(now, 12, "lid closed"));

        let value = pending_violation_json(&st, &kill_policies_watched(), now);

        assert_eq!(
            value,
            serde_json::json!({"bus": "lid", "reason": "lid closed", "secs_left": 12})
        );
    }

    #[test]
    fn test_pending_violation_is_null_while_disarmed() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.power_grace = Some(grace_at(now, 30, "AC power removed"));
        st.armed = false;

        let value = pending_violation_json(&st, &kill_policies_watched(), now);

        assert!(
            value.is_null(),
            "a disarmed daemon is not counting: {value}"
        );
    }

    #[test]
    fn test_pending_violation_ignores_an_unwatched_or_monitor_bus() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.lid_grace = Some(grace_at(now, 5, "lid closed"));
        st.network_grace = Some(grace_at(now, 8, "link down on eth0"));
        st.power_grace = Some(grace_at(now, 30, "AC power removed"));
        let mut cfg = kill_policies_watched();
        cfg.general.watch_lid = false;
        cfg.network.policy = NetworkPolicy::Monitor;

        let value = pending_violation_json(&st, &cfg, now);

        assert_eq!(value["bus"], "power", "{value}");
    }

    #[test]
    fn test_pending_violation_rounds_seconds_up() {
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.power_grace = Some(Grace {
            until: now + Duration::from_millis(1500),
            reason: "AC power removed".to_string(),
        });

        let value = pending_violation_json(&st, &kill_policies_watched(), now);

        assert_eq!(value["secs_left"], 2, "{value}");
    }

    /// A snapshot read from a mock `/sys/class/net`: one directory per
    /// interface, with the `device` marker that makes it a physical NIC.
    fn net(ifaces: &[(&str, &str)]) -> plugkill_core::network::NetworkSnapshot {
        let dir = tempfile::tempdir().unwrap();
        for (name, operstate) in ifaces {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.join("device")).unwrap();
            std::fs::write(path.join("operstate"), format!("{operstate}\n")).unwrap();
        }
        network::enumerate_interfaces_from(dir.path(), &[])
    }

    /// The reading must say what the daemon would act on. A built-in port that
    /// has never had a cable in it is Down forever and is no violation, so it
    /// must not show up as a standing "1 down" in the tray or in --status.
    #[test]
    fn test_links_down_counts_only_links_that_were_up_at_baseline() {
        let baseline = net(&[("eth0", "up"), ("eth1", "down")]);

        let same = net(&[("eth0", "up"), ("eth1", "down")]);
        assert_eq!(links_down(&same, Some(&baseline)), 0);

        let dropped = net(&[("eth0", "down"), ("eth1", "down")]);
        assert_eq!(links_down(&dropped, Some(&baseline)), 1);

        let gone = net(&[("eth1", "down")]);
        assert_eq!(
            links_down(&gone, Some(&baseline)),
            1,
            "a vanished NIC is down"
        );

        assert_eq!(
            links_down(&dropped, None),
            0,
            "with no baseline there is nothing to compare against"
        );
    }

    #[test]
    fn test_state_keys_are_lowercase_and_stable() {
        assert_eq!(power_state_key(PowerState::Ac), "ac");
        assert_eq!(power_state_key(PowerState::Battery), "battery");
        assert_eq!(power_state_key(PowerState::Unknown), "unknown");
        assert_eq!(lid_state_key(LidState::Open), "open");
        assert_eq!(lid_state_key(LidState::Closed), "closed");
        assert_eq!(lid_state_key(LidState::Unknown), "unknown");
    }

    #[test]
    fn test_status_reports_dry_run_and_null_readings_for_unwatched_buses() {
        let (_dir, socket_path, _state) = start_test_listener();
        let resp = send(&socket_path, serde_json::json!({"command": "status"}));
        let data = resp.get("data").expect("status response has no data");

        assert_eq!(data["dry_run"], false, "{data}");
        for key in [
            "pending_violation",
            "power_state",
            "lid_state",
            "network_links_down",
        ] {
            assert!(
                data.get(key).is_some_and(|v| v.is_null()),
                "{key} must be present and null with the default config: {data}"
            );
        }
    }

    // --- runtime allowances -------------------------------------------------

    fn usb_ref() -> DeviceRef {
        "usb:1d6b:0002".parse().unwrap()
    }

    /// What the detector records for a device that appeared.
    fn seen(state: &Arc<Mutex<DaemonState>>, id: DeviceRef, name: &str) {
        state.lock().unwrap().record_violation(Violation::device(
            format!("new device {}", id.selector()),
            id,
            Some(name.to_string()),
            true,
        ));
    }

    /// The `allowances` array out of a status response.
    fn allowances_of(
        st: &Arc<Mutex<DaemonState>>,
        cfg: &Arc<RwLock<Config>>,
        bl: &Arc<RwLock<Baselines>>,
    ) -> Vec<serde_json::Value> {
        let resp = handle_request(
            Request::Status,
            st,
            cfg,
            bl,
            USER,
            &FakeAuthority::refusing(),
        );
        resp.data.expect("status has data")["allowances"]
            .as_array()
            .expect("status reports an allowances array")
            .clone()
    }

    fn run(
        req: Request,
        st: &Arc<Mutex<DaemonState>>,
        cfg: &Arc<RwLock<Config>>,
        bl: &Arc<RwLock<Baselines>>,
    ) -> Response {
        handle_request(req, st, cfg, bl, USER, &FakeAuthority::refusing())
    }

    /// B1 and D2: a window opens, status counts it down, and the cap is the
    /// hour disarm has.
    #[test]
    fn test_pair_opens_a_window_status_reports_and_an_hour_is_the_cap() {
        let (st, cfg, bl) = parts(false);

        let resp = run(
            Request::Pair {
                window_secs: 60,
                for_secs: None,
            },
            &st,
            &cfg,
            &bl,
        );
        assert!(resp.ok, "{:?}", resp.error);

        let status = run(Request::Status, &st, &cfg, &bl).data.unwrap();
        let left = status["pairing_window_secs_left"]
            .as_u64()
            .expect("an open window counts down");
        assert!(left <= 60 && left > 55, "{status}");

        let too_long = run(
            Request::Pair {
                window_secs: MAX_PAIR_SECS + 1,
                for_secs: None,
            },
            &st,
            &cfg,
            &bl,
        );
        assert!(!too_long.ok);
        assert!(too_long.error.unwrap().contains("3600"));
    }

    /// B3: a second pair replaces the deadline rather than stacking, and
    /// `--pair 0` closes the window.
    #[test]
    fn test_a_second_pair_replaces_the_deadline_and_zero_closes_it() {
        let (st, cfg, bl) = parts(false);
        run(
            Request::Pair {
                window_secs: 3600,
                for_secs: None,
            },
            &st,
            &cfg,
            &bl,
        );

        run(
            Request::Pair {
                window_secs: 30,
                for_secs: None,
            },
            &st,
            &cfg,
            &bl,
        );
        let status = run(Request::Status, &st, &cfg, &bl).data.unwrap();
        assert!(
            status["pairing_window_secs_left"].as_u64().unwrap() <= 30,
            "the second window replaced the first: {status}"
        );

        let closed = run(
            Request::Pair {
                window_secs: 0,
                for_secs: None,
            },
            &st,
            &cfg,
            &bl,
        );
        assert!(closed.ok, "{:?}", closed.error);
        assert!(st.lock().unwrap().pairing.is_none(), "the window is closed");
        let status = run(Request::Status, &st, &cfg, &bl).data.unwrap();
        assert!(status["pairing_window_secs_left"].is_null(), "{status}");
    }

    /// G5: the promoted allowance names the device the detector reported, and
    /// keeps the friendly name with it (D2, E6).
    #[test]
    fn test_allow_last_promotes_the_device_the_detector_reported() {
        let (st, cfg, bl) = parts(false);
        // An older one first: with a history behind it, allow_last still
        // promotes the newest, not whatever is left in the deque.
        seen(&st, "usb:0781:5583".parse().unwrap(), "SanDisk");
        seen(&st, usb_ref(), "Kingston DataTraveler");

        let resp = run(Request::AllowLast { for_secs: None }, &st, &cfg, &bl);

        assert!(resp.ok, "{:?}", resp.error);
        let list = allowances_of(&st, &cfg, &bl);
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0]["selector"], "usb:1d6b:0002");
        assert_eq!(list[0]["bus"], "usb");
        assert_eq!(list[0]["granted_by"], "promoted");
        assert_eq!(list[0]["name"], "Kingston DataTraveler");
        assert!(list[0]["expires_in_secs"].is_null(), "no expiry by default");
    }

    /// C2: `--for` is the tighter contract, and status says how long is left.
    #[test]
    fn test_for_sets_an_expiry_and_zero_is_refused() {
        let (st, cfg, bl) = parts(false);
        seen(&st, usb_ref(), "Kingston DataTraveler");

        let resp = run(Request::AllowLast { for_secs: Some(90) }, &st, &cfg, &bl);
        assert!(resp.ok, "{:?}", resp.error);

        let list = allowances_of(&st, &cfg, &bl);
        assert_eq!(list[0]["expires_in_secs"].as_u64().unwrap(), 90);

        let zero = run(Request::AllowLast { for_secs: Some(0) }, &st, &cfg, &bl);
        assert!(!zero.ok, "an allowance that expires at once is refused");
    }

    /// G6: nothing recorded, and a recorded event with no device behind it.
    #[test]
    fn test_allow_last_refuses_with_nothing_to_promote() {
        let (st, cfg, bl) = parts(false);

        let empty = run(Request::AllowLast { for_secs: None }, &st, &cfg, &bl);
        assert!(!empty.ok);
        assert!(
            empty
                .error
                .unwrap()
                .contains("no violation has been recorded"),
            "the refusal names the reason"
        );

        st.lock()
            .unwrap()
            .record_violation(Violation::event("lid", "lid closed".to_string()));
        let no_identity = run(Request::AllowLast { for_secs: None }, &st, &cfg, &bl);
        assert!(!no_identity.ok);
        let error = no_identity.error.unwrap();
        assert!(error.contains("names no device"), "{error}");
        assert!(error.contains("lid closed"), "the reason is named: {error}");
        assert!(allowances_of(&st, &cfg, &bl).is_empty());
    }

    /// C6: one goes, then all of them, and revoking what is not there says so.
    #[test]
    fn test_revoke_and_revoke_all() {
        let (st, cfg, bl) = parts(false);
        seen(&st, usb_ref(), "Kingston DataTraveler");
        run(Request::AllowLast { for_secs: None }, &st, &cfg, &bl);

        let missing = run(
            Request::Revoke {
                selector: "usb:dead:beef".to_string(),
            },
            &st,
            &cfg,
            &bl,
        );
        assert!(!missing.ok);
        assert!(missing.error.unwrap().contains("no allowance for"));

        let gone = run(
            Request::Revoke {
                selector: "usb:1d6b:0002".to_string(),
            },
            &st,
            &cfg,
            &bl,
        );
        assert!(gone.ok, "{:?}", gone.error);
        assert!(allowances_of(&st, &cfg, &bl).is_empty());

        seen(&st, usb_ref(), "Kingston DataTraveler");
        run(Request::AllowLast { for_secs: None }, &st, &cfg, &bl);
        let all = run(Request::RevokeAll, &st, &cfg, &bl);
        assert_eq!(all.data.unwrap()["revoked"], 1);
        assert!(allowances_of(&st, &cfg, &bl).is_empty());
    }

    /// G7 at the handler: neither request drops an allowance. Both only set a
    /// pending flag, so the re-capture they schedule is asserted in main.rs,
    /// where `capture_baselines` is callable.
    #[test]
    fn test_allowances_survive_reload_and_arm() {
        let (st, cfg, bl) = parts(false);
        seen(&st, usb_ref(), "Kingston DataTraveler");
        run(Request::AllowLast { for_secs: None }, &st, &cfg, &bl);

        run(Request::Reload, &st, &cfg, &bl);
        st.lock().unwrap().armed = false;
        run(Request::Arm, &st, &cfg, &bl);

        let list = allowances_of(&st, &cfg, &bl);
        assert_eq!(
            list.len(),
            1,
            "reload and arm left the table alone: {list:?}"
        );
        assert_eq!(list[0]["selector"], "usb:1d6b:0002");
    }

    /// A3 and G10: the event buses have nothing to identify, so a command
    /// naming one is refused with an error that says so.
    #[test]
    fn test_a_command_naming_an_event_bus_is_refused() {
        let (st, cfg, bl) = parts(false);
        for selector in ["power:ac", "network:eth0", "lid:closed"] {
            let resp = run(
                Request::Revoke {
                    selector: selector.to_string(),
                },
                &st,
                &cfg,
                &bl,
            );
            assert!(!resp.ok, "{selector} must be refused");
            let error = resp.error.unwrap();
            assert!(
                error.contains("events rather than devices") && error.contains("disarm"),
                "{selector}: {error}"
            );
        }
    }

    /// G12: where the platform reports a display event counter and no
    /// connector data, a monitor cannot be identified, so it cannot be
    /// allowed. Asserted on the guard both the promotion and the pairing
    /// admission run, since the answer is a build-time property of the host.
    #[test]
    fn test_a_display_cannot_be_allowed_without_connector_data() {
        let display: DeviceRef = "display:SAM:772d:811021873".parse().unwrap();

        let refusal = display_limit(&display, false).expect("refused without connector data");
        assert!(refusal.contains("connector data"), "{refusal}");
        assert!(
            display_limit(&display, true).is_none(),
            "Linux identifies it"
        );
        assert!(
            display_limit(&usb_ref(), false).is_none(),
            "only the display bus needs connector data"
        );
    }

    /// G5 and G12 through the command rather than the helper: a recorded
    /// display violation promotes to a selector on the monitor's identity,
    /// and the same command on a build with no connector data is refused.
    #[test]
    fn test_allow_last_promotes_a_display_and_refuses_it_without_connector_data() {
        let (st, cfg, bl) = parts(false);
        let display: DeviceRef = "display:SAM:772d:811021873".parse().unwrap();
        seen(&st, display, "Odyssey G93SD");

        let refused = handle_allow_last(&st, None, Some(1000), false);
        assert!(!refused.ok, "no connector data, no display allowance");
        assert!(refused.error.unwrap().contains("connector data"));
        assert!(allowances_of(&st, &cfg, &bl).is_empty());

        let promoted = handle_allow_last(&st, None, Some(1000), true);
        assert!(promoted.ok, "{:?}", promoted.error);
        let list = allowances_of(&st, &cfg, &bl);
        assert_eq!(list[0]["selector"], "display:SAM:772d:811021873");
        assert_eq!(list[0]["bus"], "display");
        assert_eq!(list[0]["name"], "Odyssey G93SD");
    }

    /// G9: every allowance command is reachable by the socket group, the way
    /// disarm and arm are. Only kill is root's.
    #[test]
    fn test_every_allowance_command_works_for_a_non_root_peer() {
        let (st, cfg, bl) = parts(false);
        seen(&st, usb_ref(), "Kingston DataTraveler");

        for req in [
            Request::Pair {
                window_secs: 60,
                for_secs: None,
            },
            Request::AllowLast { for_secs: None },
            Request::Revoke {
                selector: "usb:1d6b:0002".to_string(),
            },
            Request::RevokeAll,
        ] {
            let label = format!("{req:?}");
            let resp = run(req, &st, &cfg, &bl);
            assert!(resp.ok, "{label} must work unprivileged: {:?}", resp.error);
        }

        let killed = run(
            Request::Kill {
                reason: "test".to_string(),
            },
            &st,
            &cfg,
            &bl,
        );
        assert!(!killed.ok, "kill is still root only");
        assert!(killed.error.unwrap().contains("root"));
    }

    /// G9a and E3: with require_auth on, granting asks and revoking does not.
    /// The gated half is covered for pair by the shared `gated()` list; this
    /// is allow_last, whose happy path needs a violation to promote.
    #[test]
    fn test_granting_is_gated_and_revoking_is_not() {
        let (st, cfg, bl) = parts(true);
        seen(&st, usb_ref(), "Kingston DataTraveler");
        let refusing = FakeAuthority::refusing();

        let resp = handle_request(
            Request::AllowLast { for_secs: None },
            &st,
            &cfg,
            &bl,
            USER,
            &refusing,
        );
        assert!(!resp.ok, "a refused authority stops allow_last");
        assert!(resp.error.unwrap().contains(AUTH_REQUIRED_ERROR));
        assert_eq!(
            refusing.calls(),
            vec![(ACTION_ALLOW.to_string(), 1000, 4242)]
        );

        // The grant that the revokes below remove, through an authority that
        // says yes. Nothing else here consults one.
        let allowing = FakeAuthority::allowing();
        assert!(
            handle_request(
                Request::AllowLast { for_secs: None },
                &st,
                &cfg,
                &bl,
                USER,
                &allowing
            )
            .ok
        );

        let never = FakeAuthority::refusing();
        for req in [
            // Closing a window only removes a permission, the same as a
            // revoke, so it must not need a password either (E3, B3).
            Request::Pair {
                window_secs: 0,
                for_secs: None,
            },
            Request::Revoke {
                selector: "usb:1d6b:0002".to_string(),
            },
            Request::RevokeAll,
        ] {
            let label = format!("{req:?}");
            let resp = handle_request(req, &st, &cfg, &bl, USER, &never);
            assert!(resp.ok, "{label} is never gated: {:?}", resp.error);
        }
        assert!(
            never.calls().is_empty(),
            "revoking only removes a permission, so it must not ask: {:?}",
            never.calls()
        );
    }
}
