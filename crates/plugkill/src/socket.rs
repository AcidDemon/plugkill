use crate::daemon_state::DaemonState;
use log::{error, info, warn};
use plugkill_core::config::Config;
use plugkill_core::ipc::{Request, Response};
use plugkill_core::state::{Baselines, DaemonMode};
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
                        std::thread::Builder::new()
                            .name("socket-handler".into())
                            .spawn(move || {
                                if let Err(e) =
                                    handle_connection(stream, &state, &config, &baselines)
                                {
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

/// Effective uid of the connected client, or `None` where the platform does
/// not expose peer credentials. Callers must treat `None` as unprivileged.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
        getsockopt(stream, PeerCredentials).ok().map(|c| c.uid())
    }
    #[cfg(any(target_os = "freebsd", target_os = "macos", target_os = "ios"))]
    {
        use nix::sys::socket::{getsockopt, sockopt::LocalPeerCred};
        getsockopt(stream, LocalPeerCred).ok().map(|c| c.uid())
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
fn kill_authorized(peer_uid: Option<u32>) -> bool {
    peer_uid == Some(0)
}

fn handle_connection(
    stream: UnixStream,
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> std::io::Result<()> {
    // Set a read timeout so we don't hang forever on misbehaving clients
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    // Read once here: handle_request no longer has the stream to ask.
    let peer_uid = peer_uid(&stream);

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
            Ok(req) => handle_request(req, state, config, baselines, peer_uid),
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

fn handle_request(
    req: Request,
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    peer_uid: Option<u32>,
) -> Response {
    match req {
        Request::Status => handle_status(state, config, baselines),
        Request::Disarm { timeout_secs } => handle_disarm(state, timeout_secs),
        Request::Arm => handle_arm(state),
        Request::Learn => handle_learn(state),
        Request::Enforce => handle_enforce(state),
        Request::Reload => handle_reload(state),
        Request::Kill { reason } => handle_kill(state, &reason, peer_uid),
    }
}

fn handle_status(
    state: &Arc<Mutex<DaemonState>>,
    config: &Arc<RwLock<Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> Response {
    let st = state.lock().unwrap();
    let cfg = config.read().unwrap();
    let bl = baselines.read().unwrap();

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

    let last_poll_ms_ago = st.last_poll.map(|t| t.elapsed().as_millis() as u64);

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
    }))
}

fn handle_disarm(state: &Arc<Mutex<DaemonState>>, timeout_secs: u64) -> Response {
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
    info!("daemon disarmed for {timeout_secs}s via socket command");

    Response::ok(serde_json::json!({
        "message": format!("disarmed for {timeout_secs} seconds"),
        "disarm_until_secs": timeout_secs,
    }))
}

fn handle_arm(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut st = state.lock().unwrap();
    st.armed = true;
    st.disarm_until = None;
    st.rebaseline_pending = true;
    info!("daemon armed via socket command (baselines will be re-captured)");

    Response::ok(serde_json::json!({
        "message": "armed (baselines will be re-captured on next poll)",
    }))
}

fn handle_learn(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut st = state.lock().unwrap();
    st.mode = DaemonMode::Learn;
    info!("switched to learning mode via socket command");

    Response::ok(serde_json::json!({
        "message": "switched to learning mode",
    }))
}

fn handle_enforce(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut st = state.lock().unwrap();
    st.mode = DaemonMode::Enforce;
    info!("switched to enforce mode via socket command");

    Response::ok(serde_json::json!({
        "message": "switched to enforce mode",
    }))
}

fn handle_reload(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut st = state.lock().unwrap();
    st.reload_pending = true;
    info!("configuration reload scheduled via socket command");

    Response::ok(serde_json::json!({
        "message": "reload scheduled",
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
        warn!("LEARN MODE: RELAY VIOLATION: remote kill from peer: {reason}");
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
    use plugkill_core::state::DeviceNames;

    /// Helper: bind a listener on a socket inside a fresh temp dir.
    /// Returns (tempdir guard, socket path, the shared state the listener mutates).
    fn start_test_listener() -> (tempfile::TempDir, PathBuf, Arc<Mutex<DaemonState>>) {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("plugkill.sock");
        let state = Arc::new(Mutex::new(DaemonState::new(DaemonMode::Enforce)));
        let config = Arc::new(RwLock::new(Config::default()));
        let baselines = Arc::new(RwLock::new(Baselines {
            usb: None,
            thunderbolt: None,
            sdcard: None,
            power: None,
            network: None,
            lid: None,
            pci: None,
            display: None,
            names: DeviceNames::default(),
        }));

        start_socket_listener(
            socket_path.clone(),
            None,
            Arc::clone(&state),
            config,
            baselines,
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
        Arc::new(Mutex::new(DaemonState::new(DaemonMode::Enforce)))
    }

    /// `peer_uid` must actually read the peer's uid on this platform. Without
    /// this, a `getsockopt` that always failed would look identical to a
    /// working gate in every other test here (`None` denies, and denial is what
    /// they assert) while in production it would deny root too and reduce every
    /// remote kill to a bare poweroff with nothing shredded.
    #[test]
    fn test_peer_uid_reads_the_connecting_uid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cred.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        assert_eq!(
            peer_uid(&server_side),
            Some(nix::unistd::geteuid().as_raw()),
            "peer credentials must be readable, or the kill gate denies everyone"
        );
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
    /// `Request::Kill` and the handler. The test process is not root, so this
    /// connection is itself the unauthorized case, which is what the socket
    /// being group-writable exposes in production.
    #[test]
    fn test_socket_kill_command_refused_for_non_root() {
        assert_ne!(
            nix::unistd::geteuid().as_raw(),
            0,
            "this test must not run as root: it asserts the unauthorized path"
        );
        let (_dir, socket_path, state) = start_test_listener();

        let resp = plugkill_core::ipc::send_request(
            &socket_path,
            &serde_json::json!({"command": "kill", "reason": "peer alpha lost AC power"}),
        )
        .expect("transport should succeed; the daemon should refuse at the application level");

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
        let st = Arc::new(Mutex::new(DaemonState::new(DaemonMode::Learn)));

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
        let st = Arc::new(Mutex::new(DaemonState::new(DaemonMode::Learn)));

        let resp = handle_kill(&st, "test", Some(1000));

        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("root"));
        assert_eq!(
            st.lock().unwrap().violations_logged,
            0,
            "an unauthorized request must not be counted as a learn-mode violation"
        );
    }
}
