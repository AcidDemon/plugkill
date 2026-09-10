mod daemon_state;
mod kill;
mod socket;

use clap::Parser;
use daemon_state::DaemonState;
use log::{error, info, warn};
use plugkill_core::config::{
    self, DisplayPolicy, LidPolicy, NetworkPolicy, PciPolicy, PowerPolicy,
};
use plugkill_core::lid::{self, LidState};
use plugkill_core::power::{self, PowerState};
use plugkill_core::sdcard::{self, SdCardDeviceId, SdCardSnapshot};
use plugkill_core::state::{Baselines, DaemonMode, DeviceNames};
use plugkill_core::thunderbolt::{self, ThunderboltDeviceId, ThunderboltSnapshot};
use plugkill_core::usb::{self, DeviceSnapshot, UsbDeviceId};
use plugkill_core::{display, ipc, network, pci};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const BANNER: &str = concat!(
    "Plugkill ",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/AcidDemon/plugkill )"
);

/// Hardware kill-switch daemon that shuts down the system when device changes are detected.
#[derive(Parser, Debug)]
#[command(name = "plugkill", version, about, before_help = BANNER)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/plugkill/config.toml")]
    config: PathBuf,

    /// Dry-run mode: log actions without executing them
    #[arg(long)]
    dry_run: bool,

    /// Print the default configuration and exit
    #[arg(long)]
    default_config: bool,

    /// List connected USB devices with details and exit
    #[arg(long)]
    list_devices: bool,

    /// Output a ready-to-paste TOML whitelist from connected devices and exit
    #[arg(long)]
    generate_whitelist: bool,

    /// Disable USB monitoring
    #[arg(long)]
    no_usb: bool,

    /// Disable Thunderbolt monitoring
    #[arg(long)]
    no_thunderbolt: bool,

    /// Disable SD card monitoring
    #[arg(long)]
    no_sdcard: bool,

    /// Disable power supply monitoring
    #[arg(long)]
    no_power: bool,

    /// Disable network link monitoring
    #[arg(long)]
    no_network: bool,

    /// Disable lid close monitoring
    #[arg(long)]
    no_lid: bool,

    /// Disable PCI bus monitoring
    #[arg(long)]
    no_pci: bool,

    /// Disable external display monitoring
    #[arg(long)]
    no_display: bool,

    /// Start in learning mode (log violations, don't kill)
    #[arg(long)]
    learn_mode: bool,

    // --- Client commands (connect to running daemon) ---
    /// Disarm the daemon for N seconds
    #[arg(long, value_name = "SECONDS")]
    disarm: Option<u64>,

    /// Re-arm the daemon and re-capture baselines
    #[arg(long)]
    arm: bool,

    /// Query daemon status
    #[arg(long)]
    status: bool,

    /// Switch daemon to learning mode
    #[arg(long)]
    learn: bool,

    /// Switch daemon to enforce mode
    #[arg(long)]
    enforce: bool,

    /// Reload daemon configuration
    #[arg(long)]
    reload: bool,

    /// Output client responses as JSON instead of human-readable text
    #[arg(long)]
    json: bool,

    /// Path to the control socket
    #[arg(long, default_value = ipc::DEFAULT_SOCKET_PATH)]
    socket: PathBuf,

    /// Group name for socket ownership (allows non-root GUI access)
    #[arg(long)]
    socket_group: Option<String>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    let cli = Cli::parse();

    if cli.default_config {
        print!("{}", config::default_config_toml());
        return;
    }

    // Handle --generate-whitelist (no root needed)
    if cli.generate_whitelist {
        match usb::enumerate_devices_detailed() {
            Ok(devices) => print!("{}", usb::generate_whitelist_toml(&devices)),
            Err(e) => {
                eprintln!("Error: failed to enumerate USB devices: {e}");
                std::process::exit(1);
            }
        }
        if let Ok(tb_devices) = thunderbolt::enumerate_thunderbolt_devices_detailed()
            && !tb_devices.is_empty()
        {
            print!(
                "{}",
                thunderbolt::generate_thunderbolt_whitelist_toml(&tb_devices)
            );
        }
        if let Ok(sd_devices) = sdcard::enumerate_sdcard_devices_detailed()
            && !sd_devices.is_empty()
        {
            print!("{}", sdcard::generate_sdcard_whitelist_toml(&sd_devices));
        }
        return;
    }

    // Handle --list-devices (no root needed)
    if cli.list_devices {
        let loaded_wl = if cli.config.exists() {
            config::load_whitelist_only(&cli.config).ok()
        } else {
            None
        };

        match usb::enumerate_devices_detailed() {
            Ok(devices) => {
                let whitelist_ids = loaded_wl.as_ref().map(|wl| {
                    wl.usb
                        .devices
                        .iter()
                        .map(|e| (e.vendor_id.clone(), e.product_id.clone()))
                        .collect::<HashSet<(String, String)>>()
                });
                usb::print_device_list(&devices, whitelist_ids.as_ref());
            }
            Err(e) => {
                eprintln!("Error: failed to enumerate USB devices: {e}");
                std::process::exit(1);
            }
        }

        if let Ok(tb_devices) = thunderbolt::enumerate_thunderbolt_devices_detailed()
            && !tb_devices.is_empty()
        {
            let tb_whitelist_ids = loaded_wl.as_ref().map(|wl| {
                wl.thunderbolt
                    .devices
                    .iter()
                    .map(|e| e.unique_id.clone())
                    .collect::<HashSet<String>>()
            });
            thunderbolt::print_thunderbolt_device_list(&tb_devices, tb_whitelist_ids.as_ref());
        }

        if let Ok(sd_devices) = sdcard::enumerate_sdcard_devices_detailed()
            && !sd_devices.is_empty()
        {
            let sd_whitelist_ids = loaded_wl.as_ref().map(|wl| {
                wl.sdcard
                    .devices
                    .iter()
                    .map(|e| e.serial.clone())
                    .collect::<HashSet<String>>()
            });
            sdcard::print_sdcard_device_list(&sd_devices, sd_whitelist_ids.as_ref());
        }
        return;
    }

    // Handle client commands (connect to running daemon via socket)
    let raw_json = cli.json;
    if let Some(timeout) = cli.disarm {
        let req = serde_json::json!({"command": "disarm", "timeout_secs": timeout});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.arm {
        let req = serde_json::json!({"command": "arm"});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.status {
        let req = serde_json::json!({"command": "status"});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.learn {
        let req = serde_json::json!({"command": "learn"});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.enforce {
        let req = serde_json::json!({"command": "enforce"});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.reload {
        let req = serde_json::json!({"command": "reload"});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }

    if !nix::unistd::geteuid().is_root() {
        error!("plugkill must run as root (need device access and shutdown capability)");
        std::process::exit(1);
    }

    let mut cfg = match config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            if cli.dry_run && !cli.config.exists() {
                warn!(
                    "config {} not found, using defaults (--dry-run mode)",
                    cli.config.display()
                );
                config::Config::default()
            } else {
                error!("failed to load config: {e}");
                std::process::exit(1);
            }
        }
    };

    // CLI overrides
    if cli.dry_run {
        cfg.general.dry_run = true;
    }
    if cli.no_usb {
        cfg.general.watch_usb = false;
    }
    if cli.no_thunderbolt {
        cfg.general.watch_thunderbolt = false;
    }
    if cli.no_sdcard {
        cfg.general.watch_sdcard = false;
    }
    if cli.no_power {
        cfg.general.watch_power = false;
    }
    if cli.no_network {
        cfg.general.watch_network = false;
    }
    if cli.no_lid {
        cfg.general.watch_lid = false;
    }
    if cli.no_pci {
        cfg.general.watch_pci = false;
    }
    if cli.no_display {
        cfg.general.watch_display = false;
    }

    if cfg.general.dry_run {
        warn!("[DRY RUN] no destructive actions will be taken");
    }

    let active_buses: Vec<&str> = [
        cfg.general.watch_usb.then_some("USB"),
        cfg.general.watch_thunderbolt.then_some("Thunderbolt"),
        cfg.general.watch_sdcard.then_some("SD card"),
        cfg.general.watch_power.then_some("power supply"),
        cfg.general.watch_network.then_some("network"),
        cfg.general.watch_lid.then_some("lid"),
        cfg.general.watch_pci.then_some("PCI"),
        cfg.general.watch_display.then_some("display"),
    ]
    .into_iter()
    .flatten()
    .collect();
    info!("monitoring buses: {}", active_buses.join(", "));

    // Set up signal handling for clean exit
    let running = Arc::new(AtomicBool::new(true));

    for &sig in &[signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let r = running.clone();
        if let Err(e) = signal_hook::flag::register_conditional_default(sig, r) {
            error!("failed to register signal handler for {sig}: {e}");
            std::process::exit(1);
        }
    }

    let initial_mode = if cli.learn_mode {
        info!("starting in LEARN mode: violations will be logged but not acted upon");
        DaemonMode::Learn
    } else {
        DaemonMode::Enforce
    };
    let daemon_state = Arc::new(Mutex::new(DaemonState::new(initial_mode)));

    // Capture baselines
    let mut device_names = DeviceNames::default();

    let usb_baseline = if cfg.general.watch_usb {
        let (snapshot, names) = capture_usb_baseline();
        device_names.usb = names;
        Some(snapshot)
    } else {
        None
    };

    let usb_whitelist = build_usb_whitelist(&cfg);
    if !usb_whitelist.devices().is_empty() {
        info!("USB whitelist:");
        let mut entries: Vec<_> = usb_whitelist.devices().iter().collect();
        entries.sort_by_key(|&(id, _)| (id.vendor_id.clone(), id.product_id.clone()));
        for (id, count) in entries {
            info!("  {id} (max count: {count})");
        }
    }

    let (tb_baseline, tb_names) = if cfg.general.watch_thunderbolt {
        capture_thunderbolt_baseline(&cfg)
    } else {
        (None, HashMap::new())
    };
    device_names.thunderbolt = tb_names;

    let tb_whitelist = build_thunderbolt_whitelist(&cfg);
    if !tb_whitelist.devices().is_empty() {
        info!("Thunderbolt whitelist:");
        let mut ids: Vec<_> = tb_whitelist.devices().iter().collect();
        ids.sort_by_key(|id| id.unique_id.clone());
        for id in ids {
            info!("  {id}");
        }
    }

    let (sd_baseline, sd_names) = if cfg.general.watch_sdcard {
        capture_sdcard_baseline(&cfg)
    } else {
        (None, HashMap::new())
    };
    device_names.sdcard = sd_names;

    let sd_whitelist = build_sdcard_whitelist(&cfg);
    if !sd_whitelist.devices().is_empty() {
        info!("SD card whitelist:");
        let mut ids: Vec<_> = sd_whitelist.devices().iter().collect();
        ids.sort_by_key(|id| id.serial.clone());
        for id in ids {
            info!("  {id}");
        }
    }

    let power_baseline = if cfg.general.watch_power {
        let state = power::read_power_state();
        info!("power baseline: {state} (policy: {})", cfg.power.policy);
        if cfg.power.require_locked {
            info!("  power violations require session to be locked");
        }
        if cfg.power.grace_secs > 0 {
            info!("  grace period: {}s", cfg.power.grace_secs);
        }
        Some(state)
    } else {
        None
    };

    let network_baseline = if cfg.general.watch_network {
        let snapshot = network::enumerate_interfaces(&cfg.network.interfaces);
        let iface_count = snapshot.interfaces().len();
        info!(
            "network baseline: {iface_count} interface(s) (policy: {})",
            cfg.network.policy
        );
        let mut ifaces: Vec<_> = snapshot.interfaces().iter().collect();
        ifaces.sort_by_key(|&(name, _)| name.clone());
        for (name, state) in ifaces {
            info!("  {name}: {state}");
        }
        if cfg.network.grace_secs > 0 {
            info!("  grace period: {}s", cfg.network.grace_secs);
        }
        Some(snapshot)
    } else {
        None
    };

    let lid_baseline = if cfg.general.watch_lid {
        let state = lid::read_lid_state();
        info!("lid baseline: {state} (policy: {})", cfg.lid.policy);
        if cfg.lid.grace_secs > 0 {
            info!("  grace period: {}s", cfg.lid.grace_secs);
        }
        Some(state)
    } else {
        None
    };

    // Acquire sleep inhibitor if lid monitoring is enabled
    let _sleep_inhibitor = if cfg.general.watch_lid {
        match lid::acquire_sleep_inhibitor() {
            Some(fd) => {
                info!("acquired logind sleep inhibitor for lid monitoring");
                Some(fd)
            }
            None => {
                #[cfg(target_os = "linux")]
                warn!(
                    "failed to acquire sleep inhibitor; lid close may not be detected before suspend"
                );
                #[cfg(target_os = "freebsd")]
                info!(
                    "no sleep inhibitor on FreeBSD; set hw.acpi.lid_switch_state=NONE so lid close is seen before suspend"
                );
                None
            }
        }
    } else {
        None
    };

    let pci_baseline = if cfg.general.watch_pci {
        match pci::enumerate_pci(&cfg.pci.ignore) {
            Ok(snapshot) => {
                info!(
                    "PCI baseline: {} device(s) (policy: {})",
                    snapshot.len(),
                    cfg.pci.policy
                );
                Some(snapshot)
            }
            Err(e) => {
                warn!("PCI baseline failed, disabling PCI monitoring: {e}");
                None
            }
        }
    } else {
        None
    };

    let display_baseline = if cfg.general.watch_display {
        let generation = display::display_generation(&cfg.display.ignore);
        info!("display baseline captured (policy: {})", cfg.display.policy);
        Some(generation)
    } else {
        None
    };

    let baselines = Arc::new(RwLock::new(Baselines {
        usb: usb_baseline,
        thunderbolt: tb_baseline,
        sdcard: sd_baseline,
        power: power_baseline,
        network: network_baseline,
        lid: lid_baseline,
        pci: pci_baseline,
        display: display_baseline,
        names: device_names,
    }));
    let config_arc = Arc::new(RwLock::new(cfg));

    let socket_path = cli.socket.clone();
    if let Err(e) = socket::start_socket_listener(
        socket_path.clone(),
        cli.socket_group.as_deref(),
        Arc::clone(&daemon_state),
        Arc::clone(&config_arc),
        Arc::clone(&baselines),
    ) {
        warn!("failed to start control socket: {e} (continuing without socket)");
    }

    info!(
        "patrolling every {}ms (dry_run={}, mode={})",
        config_arc.read().unwrap().general.sleep_ms,
        config_arc.read().unwrap().general.dry_run,
        initial_mode,
    );

    // Main polling loop
    while running.load(Ordering::Relaxed) {
        // One lock acquisition for everything this iteration reads off the
        // shared state: the disarm-expiry re-arm, the re-baseline flag, the
        // armed flag, and any kill a relay peer queued. handle_arm sets armed,
        // disarm_until and rebaseline_pending together, so reading them in
        // separate acquisitions can see an arm as armed with its re-baseline
        // still pending and run one detection pass against the pre-disarm
        // baseline. The guard drops here: capture_baselines below re-locks
        // daemon_state for the power, network and lid baselines.
        let (needs_rebaseline, is_armed, remote_kill) = {
            let mut st = daemon_state.lock().unwrap();
            if !st.armed && st.is_disarm_expired() {
                info!("disarm timeout expired, re-arming");
                st.armed = true;
                st.disarm_until = None;
                st.rebaseline_pending = true;
            }
            (
                std::mem::take(&mut st.rebaseline_pending),
                st.armed,
                st.kill_pending.take(),
            )
        };

        if needs_rebaseline {
            let cfg = config_arc.read().unwrap();
            let mut bl = baselines.write().unwrap();
            info!("re-capturing baselines after re-arm");
            capture_baselines(&cfg, &mut bl, &daemon_state, false);
        }

        // Handle config reload
        {
            let mut st = daemon_state.lock().unwrap();
            if st.reload_pending {
                st.reload_pending = false;
                drop(st); // Release lock before doing I/O

                match config::reload(&cli.config) {
                    Ok(mut new_cfg) => {
                        // Preserve CLI overrides
                        if cli.dry_run {
                            new_cfg.general.dry_run = true;
                        }
                        if cli.no_usb {
                            new_cfg.general.watch_usb = false;
                        }
                        if cli.no_thunderbolt {
                            new_cfg.general.watch_thunderbolt = false;
                        }
                        if cli.no_sdcard {
                            new_cfg.general.watch_sdcard = false;
                        }
                        if cli.no_power {
                            new_cfg.general.watch_power = false;
                        }
                        if cli.no_network {
                            new_cfg.general.watch_network = false;
                        }
                        if cli.no_lid {
                            new_cfg.general.watch_lid = false;
                        }
                        if cli.no_pci {
                            new_cfg.general.watch_pci = false;
                        }
                        if cli.no_display {
                            new_cfg.general.watch_display = false;
                        }

                        *config_arc.write().unwrap() = new_cfg;
                        info!("configuration reloaded successfully");

                        // A bus this reload switched on has no baseline yet and
                        // its checker short-circuits on None. Buses that already
                        // have one keep it: re-baselining an armed bus would
                        // accept whatever was plugged in since.
                        let cfg = config_arc.read().unwrap();
                        let mut bl = baselines.write().unwrap();
                        capture_baselines(&cfg, &mut bl, &daemon_state, true);
                    }
                    Err(e) => {
                        error!("config reload failed: {e}");
                    }
                }
            }
        }

        // Re-read every iteration so a reloaded general.sleep_ms takes effect
        // without a restart. Both sleep sites below are after this point.
        let sleep_duration = Duration::from_millis(config_arc.read().unwrap().general.sleep_ms);

        // The checkers hold read locks internally; the violation is processed
        // below with none of them held.
        let (violation, from_remote_kill) = match poll_action(remote_kill, is_armed) {
            PollAction::Skip => {
                thread::sleep(sleep_duration);
                continue;
            }
            PollAction::Kill(description) => (Some(description), true),
            PollAction::Check => (
                detect_violations(&config_arc, &baselines)
                    .or_else(|| check_power_violation(&config_arc, &baselines, &daemon_state))
                    .or_else(|| check_network_violation(&config_arc, &baselines, &daemon_state))
                    .or_else(|| check_lid_violation(&config_arc, &baselines, &daemon_state))
                    .or_else(|| check_pci_violation(&config_arc, &baselines))
                    .or_else(|| check_display_violation(&config_arc, &baselines)),
                false,
            ),
        };

        // Process violation outside of read locks
        if let Some(description) = violation {
            if handle_violation(&daemon_state, &description, &config_arc.read().unwrap()) {
                if let Err(e) = kill::execute_kill_sequence(
                    &config_arc.read().unwrap(),
                    &cli.config,
                    &description,
                ) {
                    error!("kill sequence error: {e}");
                    if !config_arc.read().unwrap().general.dry_run {
                        std::process::exit(1);
                    }
                }
                if config_arc.read().unwrap().general.dry_run {
                    warn!("[DRY RUN] continuing patrol");
                }
            } else if from_remote_kill {
                // handle_kill checked the mode at the socket and answered
                // ok:true, so the relay will not fall back to a poweroff. A
                // learn command landing between that answer and this drain
                // therefore drops a kill nothing else covers. Accepted design
                // limit, but it does not get to be silent.
                warn!(
                    "drained remote kill dropped: daemon entered learn mode after the socket accepted it"
                );
            }
        }

        daemon_state.lock().unwrap().last_poll = Some(std::time::Instant::now());
        thread::sleep(sleep_duration);
    }

    info!("received exit signal, shutting down gracefully");
    socket::cleanup_socket(&cli.socket);
}

/// What a poll iteration does with the state it drained under the lock.
#[derive(Debug, PartialEq, Eq)]
enum PollAction {
    /// Disarmed with nothing queued: sleep out the interval, run no checks.
    Skip,
    /// A kill a relay peer queued. The string is the violation description.
    Kill(String),
    /// Armed with nothing queued: run the bus checkers.
    Check,
}

/// Turn a drained `kill_pending` and the armed flag into what this poll does.
///
/// Pure, so the decision that turns a queued kill into a kill is testable
/// without a running daemon. The side effects stay at the call site: the
/// sleep, the bus checkers, and `kill::execute_kill_sequence`.
fn poll_action(remote_kill: Option<String>, is_armed: bool) -> PollAction {
    match remote_kill {
        // A remote kill overrides the disarm window: a trusted peer's KILL is
        // not the local operator's dock swap, and leaving it queued until
        // re-arm would fire it at an arbitrary later time. It also
        // short-circuits the bus checks, which have nothing to add.
        Some(reason) => {
            PollAction::Kill(format!("RELAY VIOLATION: remote kill from peer: {reason}"))
        }
        None if is_armed => PollAction::Check,
        None => PollAction::Skip,
    }
}

/// Check all active buses for violations. Returns the first violation description found, or None.
fn detect_violations(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> Option<String> {
    let cfg = config_arc.read().unwrap();
    let bl = baselines.read().unwrap();

    // USB check
    if cfg.general.watch_usb
        && let Some(ref baseline) = bl.usb
    {
        match usb::enumerate_devices() {
            Ok(current) => {
                if let Some(change) = current.detect_changes(baseline, &build_usb_whitelist(&cfg)) {
                    let id = change.device_id();
                    let name = bl
                        .names
                        .usb
                        .get(&(id.vendor_id.clone(), id.product_id.clone()))
                        .map(|n| format!(" [{n}]"))
                        .unwrap_or_default();
                    return Some(format!("USB VIOLATION: {change}{name}"));
                }
            }
            Err(e) => {
                return Some(format!(
                    "USB VIOLATION: enumeration failure (possible tampering): {e}"
                ));
            }
        }
    }

    // Thunderbolt check
    if cfg.general.watch_thunderbolt
        && let Some(ref tb_base) = bl.thunderbolt
    {
        match thunderbolt::enumerate_thunderbolt_devices() {
            Ok(current) => {
                if let Some(change) =
                    current.detect_changes(tb_base, &build_thunderbolt_whitelist(&cfg))
                {
                    let id = change.device_id();
                    let name = bl
                        .names
                        .thunderbolt
                        .get(&id.unique_id)
                        .map(|n| format!(" [{n}]"))
                        .unwrap_or_default();
                    return Some(format!("THUNDERBOLT VIOLATION: {change}{name}"));
                }
            }
            Err(e) => {
                return Some(format!(
                    "THUNDERBOLT VIOLATION: enumeration failure (possible tampering): {e}"
                ));
            }
        }
    }

    // SD card check
    if cfg.general.watch_sdcard
        && let Some(ref sd_base) = bl.sdcard
    {
        match sdcard::enumerate_sdcard_devices() {
            Ok(current) => {
                if let Some(change) = current.detect_changes(sd_base, &build_sdcard_whitelist(&cfg))
                {
                    let id = change.device_id();
                    let name = bl
                        .names
                        .sdcard
                        .get(&id.serial)
                        .map(|n| format!(" [{n}]"))
                        .unwrap_or_default();
                    return Some(format!("SD CARD VIOLATION: {change}{name}"));
                }
            }
            Err(e) => {
                return Some(format!(
                    "SD CARD VIOLATION: enumeration failure (possible tampering): {e}"
                ));
            }
        }
    }

    // Power check is handled separately in check_power_violation() because
    // it needs mutable access to DaemonState for grace period tracking.
    None
}

/// Check for PCI add/remove violations. Enumeration failure is logged rather
/// than treated as a kill, since the FreeBSD backend shells out to pciconf.
fn check_pci_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> Option<String> {
    let cfg = config_arc.read().unwrap();
    if !cfg.general.watch_pci {
        return None;
    }

    let bl = baselines.read().unwrap();
    let baseline = bl.pci.as_ref()?;

    let current = match pci::enumerate_pci(&cfg.pci.ignore) {
        Ok(c) => c,
        Err(e) => {
            warn!("PCI enumeration failed: {e}");
            return None;
        }
    };

    let change = current.detect_changes(baseline)?;
    drop(bl);

    if cfg.pci.policy == PciPolicy::Monitor {
        info!("PCI change: {change} (monitor mode, no action)");
        baselines.write().unwrap().pci = Some(current);
        return None;
    }

    Some(format!("PCI VIOLATION: {change}"))
}

/// Check for external display connect/disconnect via the topology generation.
fn check_display_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> Option<String> {
    let cfg = config_arc.read().unwrap();
    if !cfg.general.watch_display {
        return None;
    }

    let baseline = baselines.read().unwrap().display?;
    let current = display::display_generation(&cfg.display.ignore);
    if current == baseline {
        return None;
    }

    if cfg.display.policy == DisplayPolicy::Monitor {
        info!("display topology changed (monitor mode, no action)");
        baselines.write().unwrap().display = Some(current);
        return None;
    }

    Some("DISPLAY VIOLATION: connector topology changed".to_string())
}

/// Check for power supply violations, managing grace period and trigger-once state.
/// Returns a violation description if one should be triggered, or None.
fn check_power_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    daemon_state: &Arc<Mutex<DaemonState>>,
) -> Option<String> {
    let cfg = config_arc.read().unwrap();
    if !cfg.general.watch_power {
        return None;
    }

    let bl = baselines.read().unwrap();
    let baseline_power = bl.power?;
    drop(bl);

    let current = power::read_power_state();
    let mut st = daemon_state.lock().unwrap();

    // Monitor policy: log transitions but never violate
    if cfg.power.policy == PowerPolicy::Monitor {
        if current != baseline_power {
            info!("power state changed: {baseline_power} → {current} (monitor mode, no action)");
            // Update baseline to avoid repeated logging
            drop(st);
            let mut bl = baselines.write().unwrap();
            bl.power = Some(current);
        }
        return None;
    }

    // Check if we transitioned to battery
    let on_battery = current == PowerState::Battery;

    // If not on battery, clear any pending grace period and return
    if !on_battery {
        if st.power_unplug_at.is_some() {
            info!("AC power restored during grace period");
            st.power_unplug_at = None;
        }
        return None;
    }

    // Trigger-once: skip if already fired
    if cfg.power.policy == PowerPolicy::TriggerOnce && st.power_trigger_once_fired {
        return None;
    }

    // If baseline was already battery, no transition occurred
    if baseline_power == PowerState::Battery {
        return None;
    }

    // require_locked: only trigger if session is locked
    if cfg.power.require_locked {
        match power::is_session_locked() {
            Some(true) => {} // locked, proceed with violation check
            Some(false) => {
                // Not locked, user is present, don't trigger
                // But track the unplug time in case session locks later
                if st.power_unplug_at.is_none() {
                    st.power_unplug_at = Some(Instant::now());
                }
                return None;
            }
            None => {
                // Can't determine lock state, proceed without this check
                warn!("cannot determine session lock state, proceeding with power check");
            }
        }
    }

    // Grace period handling
    if cfg.power.grace_secs > 0 {
        let now = Instant::now();
        match st.power_unplug_at {
            None => {
                // First detection of battery, start grace period
                info!(
                    "AC power removed, grace period started ({}s)",
                    cfg.power.grace_secs
                );
                st.power_unplug_at = Some(now);
                return None;
            }
            Some(unplug_time) => {
                let elapsed = now.duration_since(unplug_time);
                if elapsed < Duration::from_secs(cfg.power.grace_secs) {
                    // Still within grace period
                    return None;
                }
                // Grace period expired, fall through to violation
            }
        }
    } else if st.power_unplug_at.is_none() {
        // No grace period and first detection, record unplug time for logging
        st.power_unplug_at = Some(Instant::now());
    }

    // Mark trigger-once as fired
    if cfg.power.policy == PowerPolicy::TriggerOnce {
        st.power_trigger_once_fired = true;
    }

    Some(format!(
        "POWER VIOLATION: AC power removed (policy: {})",
        cfg.power.policy
    ))
}

/// Check for network link-down violations, managing grace period.
/// Returns a violation description if one should be triggered, or None.
fn check_network_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    daemon_state: &Arc<Mutex<DaemonState>>,
) -> Option<String> {
    let cfg = config_arc.read().unwrap();
    if !cfg.general.watch_network {
        return None;
    }

    let bl = baselines.read().unwrap();
    let baseline_network = bl.network.as_ref()?;

    let current = network::enumerate_interfaces(&cfg.network.interfaces);

    // Check for link-down transition
    let change = match current.detect_link_down(baseline_network) {
        Some(c) => c,
        None => {
            // Link restored, clear grace period if active
            let mut st = daemon_state.lock().unwrap();
            if st.network_link_down_at.is_some() {
                info!("network link restored during grace period");
                st.network_link_down_at = None;
            }
            return None;
        }
    };

    drop(bl);

    // Monitor policy: log but never violate
    if cfg.network.policy == NetworkPolicy::Monitor {
        info!("network link change: {change} (monitor mode, no action)");
        // Update baseline to avoid repeated logging
        let mut bl = baselines.write().unwrap();
        bl.network = Some(current);
        return None;
    }

    let mut st = daemon_state.lock().unwrap();

    // Grace period handling
    if cfg.network.grace_secs > 0 {
        let now = Instant::now();
        match st.network_link_down_at {
            None => {
                info!(
                    "network link down on {}, grace period started ({}s)",
                    change.interface, cfg.network.grace_secs
                );
                st.network_link_down_at = Some(now);
                return None;
            }
            Some(down_time) => {
                let elapsed = now.duration_since(down_time);
                if elapsed < Duration::from_secs(cfg.network.grace_secs) {
                    return None;
                }
            }
        }
    } else if st.network_link_down_at.is_none() {
        st.network_link_down_at = Some(Instant::now());
    }

    Some(format!("NETWORK VIOLATION: {change}"))
}

/// Check for lid close violations, managing grace period.
/// Returns a violation description if one should be triggered, or None.
fn check_lid_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    daemon_state: &Arc<Mutex<DaemonState>>,
) -> Option<String> {
    let cfg = config_arc.read().unwrap();
    if !cfg.general.watch_lid {
        return None;
    }

    let bl = baselines.read().unwrap();
    let baseline_lid = bl.lid?;
    drop(bl);

    let current = lid::read_lid_state();

    // Only care about transitions to Closed
    if current != LidState::Closed {
        let mut st = daemon_state.lock().unwrap();
        if st.lid_close_at.is_some() {
            info!("lid reopened during grace period");
            st.lid_close_at = None;
        }
        return None;
    }

    // If baseline was already closed, no transition
    if baseline_lid == LidState::Closed {
        return None;
    }

    // Monitor policy: log but never violate
    if cfg.lid.policy == LidPolicy::Monitor {
        info!("lid closed (monitor mode, no action)");
        let mut bl = baselines.write().unwrap();
        bl.lid = Some(current);
        return None;
    }

    let mut st = daemon_state.lock().unwrap();

    // Grace period handling
    if cfg.lid.grace_secs > 0 {
        let now = Instant::now();
        match st.lid_close_at {
            None => {
                info!("lid closed, grace period started ({}s)", cfg.lid.grace_secs);
                st.lid_close_at = Some(now);
                return None;
            }
            Some(close_time) => {
                let elapsed = now.duration_since(close_time);
                if elapsed < Duration::from_secs(cfg.lid.grace_secs) {
                    return None;
                }
            }
        }
    } else if st.lid_close_at.is_none() {
        st.lid_close_at = Some(Instant::now());
    }

    Some("LID VIOLATION: laptop lid closed".to_string())
}

/// Handle a violation according to the current daemon mode.
/// Returns true if the kill sequence should proceed (enforce mode),
/// false if the violation was only logged (learn mode).
fn handle_violation(
    state: &Arc<Mutex<DaemonState>>,
    description: &str,
    _config: &config::Config,
) -> bool {
    let mut st = state.lock().unwrap();
    match st.mode {
        DaemonMode::Enforce => {
            error!("{description}");
            true
        }
        DaemonMode::Learn => {
            st.violations_logged += 1;
            warn!("LEARN mode: {description}");
            false
        }
    }
}

/// Capture USB baseline, exiting on failure.
/// Also returns a name lookup map from detailed enumeration.
fn capture_usb_baseline() -> (DeviceSnapshot, HashMap<(String, String), String>) {
    let snapshot = match usb::enumerate_devices() {
        Ok(s) => s,
        Err(e) => {
            error!("failed to enumerate USB devices: {e}");
            std::process::exit(1);
        }
    };

    // Build name lookup from detailed enumeration (best-effort)
    let names = usb::enumerate_devices_detailed()
        .map(|devices| {
            devices
                .into_iter()
                .filter_map(|d| d.product.map(|name| ((d.vendor_id, d.product_id), name)))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    info!(
        "USB baseline captured: {} unique device ID(s)",
        snapshot.len()
    );
    let mut entries: Vec<_> = snapshot.devices().iter().collect();
    entries.sort_by_key(|&(id, _)| (id.vendor_id.clone(), id.product_id.clone()));
    for (id, count) in entries {
        let name = names
            .get(&(id.vendor_id.clone(), id.product_id.clone()))
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        info!("  {id}{name} (count: {count})");
    }

    (snapshot, names)
}

/// Capture Thunderbolt baseline, returning None if hardware not present.
/// Also returns a name lookup map from detailed enumeration.
fn capture_thunderbolt_baseline(
    cfg: &config::Config,
) -> (Option<ThunderboltSnapshot>, HashMap<String, String>) {
    match thunderbolt::enumerate_thunderbolt_devices() {
        Ok(snapshot) => {
            let names = thunderbolt::enumerate_thunderbolt_devices_detailed()
                .map(|devices| {
                    devices
                        .into_iter()
                        .filter_map(|d| d.device_name.map(|name| (d.unique_id, name)))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();

            info!("Thunderbolt baseline: {} device(s)", snapshot.len());
            let mut ids: Vec<_> = snapshot.devices().iter().collect();
            ids.sort_by_key(|id| id.unique_id.clone());
            for id in ids {
                let name = names
                    .get(&id.unique_id)
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                info!("  {id}{name}");
            }
            (Some(snapshot), names)
        }
        Err(e) => {
            // Never silent: the baseline stays None, the checker short-circuits
            // on None forever, and --status still reports thunderbolt_watching
            // from the config flag, so without this line the bus is
            // unmonitored behind a positive watching indicator.
            warn!("Thunderbolt baseline failed, monitoring disabled: {e}");
            if !cfg.thunderbolt_whitelist.devices.is_empty() {
                warn!("thunderbolt_whitelist configured but no thunderbolt hardware found");
            }
            (None, HashMap::new())
        }
    }
}

/// Capture SD card baseline, returning None if MMC bus not present.
/// Also returns a name lookup map from detailed enumeration.
fn capture_sdcard_baseline(
    cfg: &config::Config,
) -> (Option<SdCardSnapshot>, HashMap<String, String>) {
    match sdcard::enumerate_sdcard_devices() {
        Ok(snapshot) => {
            let names = sdcard::enumerate_sdcard_devices_detailed()
                .map(|devices| {
                    devices
                        .into_iter()
                        .filter_map(|d| d.name.map(|name| (d.serial, name)))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();

            info!("SD card baseline: {} device(s)", snapshot.len());
            let mut ids: Vec<_> = snapshot.devices().iter().collect();
            ids.sort_by_key(|id| id.serial.clone());
            for id in ids {
                let name = names
                    .get(&id.serial)
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                info!("  {id}{name}");
            }
            (Some(snapshot), names)
        }
        Err(e) => {
            // Same reasoning as the Thunderbolt arm above.
            warn!("SD card baseline failed, monitoring disabled: {e}");
            if !cfg.sdcard_whitelist.devices.is_empty() {
                warn!("sdcard_whitelist configured but no MMC bus found");
            }
            (None, HashMap::new())
        }
    }
}

/// Capture baselines for the enabled buses, replacing whatever is there.
///
/// `only_missing` skips a bus that already has a baseline, which is what a
/// config reload needs: a bus switched on at runtime has no baseline and the
/// per-bus checkers short-circuit on `None`, while a bus that was already
/// armed must keep the baseline it was armed against. With `only_missing`
/// false every enabled bus is re-captured, which is what a re-arm needs.
fn capture_baselines(
    cfg: &config::Config,
    bl: &mut Baselines,
    daemon_state: &Arc<Mutex<DaemonState>>,
    only_missing: bool,
) {
    // A bus switched off keeps its baseline unless it is cleared here, and
    // re-enabling it later would then compare live devices against the
    // pre-off snapshot: a kill caused by toggling a documented switch.
    // Clearing lets the only_missing pass below capture a fresh one.
    let g = &cfg.general;
    if !g.watch_usb {
        bl.usb = None;
    }
    if !g.watch_thunderbolt {
        bl.thunderbolt = None;
    }
    if !g.watch_sdcard {
        bl.sdcard = None;
    }
    if !g.watch_power {
        bl.power = None;
    }
    if !g.watch_network {
        bl.network = None;
    }
    if !g.watch_lid {
        bl.lid = None;
    }
    if !g.watch_pci {
        bl.pci = None;
    }
    if !g.watch_display {
        bl.display = None;
    }

    if cfg.general.watch_usb && !(only_missing && bl.usb.is_some()) {
        let (snapshot, names) = capture_usb_baseline();
        bl.usb = Some(snapshot);
        bl.names.usb = names;
    }
    if cfg.general.watch_thunderbolt && !(only_missing && bl.thunderbolt.is_some()) {
        let (snapshot, names) = capture_thunderbolt_baseline(cfg);
        bl.thunderbolt = snapshot;
        bl.names.thunderbolt = names;
    }
    if cfg.general.watch_sdcard && !(only_missing && bl.sdcard.is_some()) {
        let (snapshot, names) = capture_sdcard_baseline(cfg);
        bl.sdcard = snapshot;
        bl.names.sdcard = names;
    }
    if cfg.general.watch_power && !(only_missing && bl.power.is_some()) {
        let state = power::read_power_state();
        info!("power baseline: {state}");
        bl.power = Some(state);
        // Reset power trigger state on baseline capture
        let mut st = daemon_state.lock().unwrap();
        st.power_unplug_at = None;
        st.power_trigger_once_fired = false;
    }
    if cfg.general.watch_network && !(only_missing && bl.network.is_some()) {
        let snapshot = network::enumerate_interfaces(&cfg.network.interfaces);
        info!(
            "network baseline: {} interface(s)",
            snapshot.interfaces().len()
        );
        bl.network = Some(snapshot);
        let mut st = daemon_state.lock().unwrap();
        st.network_link_down_at = None;
    }
    if cfg.general.watch_lid && !(only_missing && bl.lid.is_some()) {
        let state = lid::read_lid_state();
        info!("lid baseline: {state}");
        bl.lid = Some(state);
        let mut st = daemon_state.lock().unwrap();
        st.lid_close_at = None;
    }
    if cfg.general.watch_pci && !(only_missing && bl.pci.is_some()) {
        match pci::enumerate_pci(&cfg.pci.ignore) {
            Ok(snapshot) => {
                info!("PCI baseline: {} device(s)", snapshot.len());
                bl.pci = Some(snapshot);
            }
            Err(e) => warn!("PCI baseline failed: {e}"),
        }
    }
    if cfg.general.watch_display && !(only_missing && bl.display.is_some()) {
        bl.display = Some(display::display_generation(&cfg.display.ignore));
        info!("display baseline captured");
    }
}

/// Build a DeviceSnapshot from the USB whitelist config entries.
fn build_usb_whitelist(cfg: &config::Config) -> DeviceSnapshot {
    let mut map = HashMap::new();
    for entry in &cfg.whitelist.devices {
        let id = UsbDeviceId {
            vendor_id: entry.vendor_id.clone(),
            product_id: entry.product_id.clone(),
        };
        *map.entry(id).or_insert(0) += entry.count;
    }
    DeviceSnapshot::from_map(map)
}

/// Build a ThunderboltSnapshot from the thunderbolt whitelist config entries.
fn build_thunderbolt_whitelist(cfg: &config::Config) -> ThunderboltSnapshot {
    ThunderboltSnapshot::from_set(
        cfg.thunderbolt_whitelist
            .devices
            .iter()
            .map(|entry| ThunderboltDeviceId {
                unique_id: entry.unique_id.clone(),
            })
            .collect(),
    )
}

/// Build an SdCardSnapshot from the SD card whitelist config entries.
fn build_sdcard_whitelist(cfg: &config::Config) -> SdCardSnapshot {
    SdCardSnapshot::from_set(
        cfg.sdcard_whitelist
            .devices
            .iter()
            .map(|entry| SdCardDeviceId {
                serial: entry.serial.clone(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the display bus is on. Display is the one bus a unit test can
    /// drive: `display_generation` hashes whatever sysfs offers and cannot
    /// fail, while `capture_usb_baseline` calls `process::exit(1)` when
    /// enumeration fails, which would take the test harness with it.
    /// `Config::default()` already leaves power/network/lid/pci/display off
    /// and usb/thunderbolt/sdcard on, so only four flags need setting.
    fn display_only_config() -> config::Config {
        let mut cfg = config::Config::default();
        cfg.general.watch_usb = false;
        cfg.general.watch_thunderbolt = false;
        cfg.general.watch_sdcard = false;
        cfg.general.watch_display = true;
        cfg
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

    #[test]
    fn test_reload_baselines_newly_enabled_bus() {
        let cfg = display_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new(DaemonMode::Enforce)));
        let mut bl = empty_baselines();

        capture_baselines(&cfg, &mut bl, &state, true);

        assert!(
            bl.display.is_some(),
            "a bus enabled by reload must get a baseline, or its checker stays short-circuited"
        );
        assert!(bl.usb.is_none(), "a disabled bus must stay unbaselined");
    }

    #[test]
    fn test_reload_keeps_existing_baseline() {
        let cfg = display_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new(DaemonMode::Enforce)));
        let mut bl = empty_baselines();
        bl.display = Some(0xdead_beef);

        capture_baselines(&cfg, &mut bl, &state, true);

        assert_eq!(
            bl.display,
            Some(0xdead_beef),
            "reload must not re-baseline a bus that was already armed"
        );
    }

    #[test]
    fn test_reload_clears_a_disabled_bus_then_recaptures_it() {
        let mut cfg = display_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new(DaemonMode::Enforce)));
        let mut bl = empty_baselines();
        bl.display = Some(0xdead_beef);

        cfg.general.watch_display = false;
        capture_baselines(&cfg, &mut bl, &state, true);
        assert_eq!(
            bl.display, None,
            "a bus switched off by reload must lose its baseline"
        );

        cfg.general.watch_display = true;
        capture_baselines(&cfg, &mut bl, &state, true);
        assert!(
            bl.display.is_some(),
            "switching a bus back on must capture a baseline"
        );
        assert_ne!(
            bl.display,
            Some(0xdead_beef),
            "the recaptured baseline must be fresh, not the pre-off one"
        );
    }

    /// A drained kill has to reach the kill sequence, and it has to carry the
    /// peer's reason into the description that `execute_kill_sequence` logs.
    /// This is the regression test for the fail-open the branch fixed: the
    /// relay already got `ok:true`, so a kill dropped here is a kill nothing
    /// falls back for.
    #[test]
    fn test_drained_kill_becomes_a_violation_carrying_the_reason() {
        let action = poll_action(Some("peer alpha lost AC power".to_string()), true);

        let PollAction::Kill(description) = action else {
            panic!("a drained kill must produce a kill, got {action:?}");
        };
        assert!(
            description.contains("peer alpha lost AC power"),
            "the peer's reason must survive into the description: {description}"
        );
        assert!(
            description.starts_with("RELAY VIOLATION:"),
            "the description must carry the bus prefix alert rules key on: {description}"
        );
    }

    /// A trusted peer's KILL is not the local operator's dock swap, so it fires
    /// through an active disarm window rather than waiting for re-arm and
    /// firing at an arbitrary later time.
    #[test]
    fn test_remote_kill_overrides_the_disarm_window() {
        let action = poll_action(Some("peer beta seized".to_string()), false);

        assert!(
            matches!(action, PollAction::Kill(_)),
            "a remote kill must override the disarm window, got {action:?}"
        );
    }

    /// The other half of that override: without a remote kill, a disarm window
    /// still suppresses everything. If this passed for the wrong reason the
    /// test above would too.
    #[test]
    fn test_disarmed_poll_without_a_remote_kill_skips() {
        assert_eq!(poll_action(None, false), PollAction::Skip);
    }

    #[test]
    fn test_armed_poll_without_a_remote_kill_runs_the_checks() {
        assert_eq!(poll_action(None, true), PollAction::Check);
    }

    #[test]
    fn test_rearm_replaces_existing_baseline() {
        let cfg = display_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new(DaemonMode::Enforce)));
        let mut bl = empty_baselines();
        // No real generation token is u64::MAX: on Linux it is a DefaultHasher
        // digest of a connector list, on FreeBSD a small event counter.
        bl.display = Some(u64::MAX);

        capture_baselines(&cfg, &mut bl, &state, false);

        assert_ne!(bl.display, Some(u64::MAX));
    }
}
