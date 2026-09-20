mod daemon_state;
mod kill;
mod socket;

use clap::Parser;
use daemon_state::{DaemonState, Grace, Violation};
use log::{error, info, warn};
use plugkill_core::allowances::DeviceRef;
use plugkill_core::config::{
    self, DisplayPolicy, LidPolicy, NetworkPolicy, PciPolicy, PowerPolicy,
};
use plugkill_core::lid::{self, LidState};
use plugkill_core::power::{self, PowerState};
use plugkill_core::sdcard::{self, SdCardChange, SdCardDeviceId, SdCardSnapshot};
use plugkill_core::state::{Baselines, DaemonMode, DeviceNames};
use plugkill_core::thunderbolt::{
    self, ThunderboltChange, ThunderboltDeviceId, ThunderboltSnapshot,
};
use plugkill_core::usb::{self, DeviceChange, DeviceSnapshot, UsbDeviceId};
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

    /// Admit the next new device as an allowance (default 60, max 3600)
    #[arg(long, value_name = "SECONDS", num_args = 0..=1, default_missing_value = "60")]
    pair: Option<u64>,

    /// Allow the device that caused the last violation
    #[arg(long)]
    allow_last: bool,

    /// Expiry for --pair or --allow-last, such as 30s, 10m or 2h (default: none)
    #[arg(long, value_name = "DURATION")]
    r#for: Option<String>,

    /// Revoke one allowance, named by selector such as usb:1d6b:0002
    #[arg(long, value_name = "SELECTOR")]
    revoke: Option<String>,

    /// Revoke every allowance
    #[arg(long)]
    revoke_all: bool,

    /// List active allowances
    #[arg(long)]
    allowances: bool,

    /// With --allowances, emit TOML config that makes them permanent
    #[arg(long)]
    toml: bool,

    /// With --allowances, emit NixOS module settings that make them permanent
    #[arg(long)]
    nix: bool,

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

    // --for belongs to --pair and --allow-last, so it is parsed once before
    // either of them sends anything.
    let for_secs = match cli
        .r#for
        .as_deref()
        .map(ipc::parse_duration_secs)
        .transpose()
    {
        Ok(secs) => secs,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    if let Some(window_secs) = cli.pair {
        let req = serde_json::json!({
            "command": "pair", "window_secs": window_secs, "for_secs": for_secs
        });
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.allow_last {
        let req = serde_json::json!({"command": "allow_last", "for_secs": for_secs});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(selector) = cli.revoke.as_deref() {
        let req = serde_json::json!({"command": "revoke", "selector": selector});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.revoke_all {
        let req = serde_json::json!({"command": "revoke_all"});
        if let Err(e) = ipc::send_command(&cli.socket, &req, raw_json) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }
    // Listing reads the status response, so it needs no command of its own.
    if cli.allowances {
        let output = if raw_json {
            ipc::AllowanceOutput::Json
        } else if cli.toml {
            ipc::AllowanceOutput::Toml
        } else if cli.nix {
            ipc::AllowanceOutput::Nix
        } else {
            ipc::AllowanceOutput::Table
        };
        if let Err(e) = ipc::print_allowances(&cli.socket, output) {
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

    // Zipped against ipc::BUSES rather than naming the buses again here, so the
    // startup line, the status output and the tray cannot drift apart. The
    // flags are in the same order as that table.
    let watch_flags: [bool; ipc::BUSES.len()] = [
        cfg.general.watch_usb,
        cfg.general.watch_thunderbolt,
        cfg.general.watch_sdcard,
        cfg.general.watch_power,
        cfg.general.watch_network,
        cfg.general.watch_lid,
        cfg.general.watch_pci,
        cfg.general.watch_display,
    ];
    let active_buses: Vec<&str> = ipc::BUSES
        .iter()
        .zip(watch_flags)
        .filter_map(|((_, label), on)| on.then_some(*label))
        .collect();
    info!("monitoring buses: {}", active_buses.join(", "));

    // On the record, so the journal can answer afterwards whether this machine
    // was asking for a password.
    info!(
        "disarm, learn and reload {}",
        if cfg.general.require_auth {
            "require authentication"
        } else {
            "are ungated (require_auth = false)"
        }
    );

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
    // The path the daemon actually loaded, so `status` reports it rather than
    // a client guessing the default (C1, H1). Resolved: a relative --config
    // means nothing to a client with its own cwd. Falls back to the argument
    // when the file is not there yet.
    let config_path = std::fs::canonicalize(&cli.config).unwrap_or_else(|_| cli.config.clone());
    let daemon_state = Arc::new(Mutex::new(DaemonState::new(initial_mode, config_path)));

    // Capture baselines
    let mut device_names = DeviceNames::default();

    let usb_baseline = if cfg.general.watch_usb {
        // Startup is the one caller that exits: refusing to come up is louder
        // than coming up with USB unmonitored, and there is no armed state to
        // preserve yet. capture_usb_baseline already logged the cause.
        let Some((snapshot, names)) = capture_usb_baseline() else {
            std::process::exit(1);
        };
        device_names.usb = names;
        Some(snapshot)
    } else {
        None
    };

    // Nothing is allowed yet at startup: the table is in memory only, and this
    // line logs what the config permits.
    let no_allowances = HashSet::new();
    let usb_whitelist = build_usb_whitelist(&cfg, &no_allowances);
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

    let tb_whitelist = build_thunderbolt_whitelist(&cfg, &no_allowances);
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

    let sd_whitelist = build_sdcard_whitelist(&cfg, &no_allowances);
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
        let snapshot = display::display_snapshot(&cfg.display.ignore);
        info!(
            "display baseline captured: {} connector(s) (policy: {})",
            snapshot.connectors.len(),
            cfg.display.policy
        );
        Some(snapshot)
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
        Arc::new(plugkill_core::authz::PolkitAuthority),
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
        let now = Instant::now();
        let (needs_rebaseline, is_armed, remote_kill) = {
            let mut st = daemon_state.lock().unwrap();
            if !st.armed && st.is_disarm_expired() {
                info!("disarm timeout expired, re-arming");
                st.armed = true;
                st.disarm_until = None;
                st.rebaseline_pending = true;
            }
            // One warning per allowance as its deadline comes up, so the
            // journal says why the device is about to turn into a violation
            // before it does. Spec C2.
            for (selector, left) in st.allowances.due_to_warn(now, EXPIRY_WARN_AHEAD) {
                warn!(
                    "allowance {selector} expires in {}s, the device will become a violation",
                    left.as_secs()
                );
            }
            // An expired allowance is dropped here rather than silently: the
            // device it covered is still plugged in and becomes a violation on
            // this very poll. Spec C2.
            for gone in st.allowances.sweep(now) {
                info!(
                    "allowance expired: {}{}, no longer accepted",
                    gone.id,
                    shown(&gone.name)
                );
            }
            if st.pairing.is_some_and(|w| w.until <= now) {
                st.pairing = None;
                info!("pairing window closed with nothing admitted");
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
            capture_baselines(&cfg, &mut bl, &daemon_state, false, now);
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
                        capture_baselines(&cfg, &mut bl, &daemon_state, true, now);
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
            PollAction::Kill(description) => (Some(Violation::event("relay", description)), true),
            PollAction::Check => (
                detect_violations(&config_arc, &baselines, &daemon_state, now)
                    .or_else(|| {
                        check_power_violation(&config_arc, &baselines, &daemon_state)
                            .map(|d| Violation::event("power", d))
                    })
                    .or_else(|| {
                        check_network_violation(&config_arc, &baselines, &daemon_state)
                            .map(|d| Violation::event("network", d))
                    })
                    .or_else(|| {
                        check_lid_violation(&config_arc, &baselines, &daemon_state)
                            .map(|d| Violation::event("lid", d))
                    })
                    .or_else(|| check_pci_violation(&config_arc, &baselines, &daemon_state, now))
                    .or_else(|| {
                        check_display_violation(&config_arc, &baselines, &daemon_state, now)
                    }),
                false,
            ),
        };

        // A device appearing while a pairing window is open is admitted rather
        // than killed for, and the window closes behind it. Spec B1, B2.
        let violation = violation.and_then(|v| admit_or_keep(&daemon_state, v, now));

        // Process violation outside of read locks
        if let Some(violation) = violation {
            let description = violation.description.clone();
            daemon_state.lock().unwrap().record_violation(violation);
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

/// How far ahead of an allowance expiry the daemon warns. Spec C2.
const EXPIRY_WARN_AHEAD: Duration = Duration::from_secs(60);

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

/// Hand a violation to an open pairing window. Returns the violation unless
/// the window admitted it, in which case it became an allowance and the daemon
/// logs what it took in. Only an appearance with a device identity is
/// admissible: an event, an enumeration failure and a device being removed all
/// stay violations. Spec B1, B2, E6.
fn admit_or_keep(
    state: &Arc<Mutex<DaemonState>>,
    violation: Violation,
    now: Instant,
) -> Option<Violation> {
    if !violation.appeared {
        return Some(violation);
    }
    let id = violation.id.clone()?;
    let mut st = state.lock().unwrap();
    // admit_paired logs what it took in, so there is nothing to add here.
    if st.admit_paired(
        id,
        violation.name.clone(),
        now,
        crate::socket::DISPLAY_IDENTITY,
    ) {
        None
    } else {
        Some(violation)
    }
}

/// Check all active buses for violations. Returns the first violation found,
/// with the identity of the device behind it so `allow_last` and the pairing
/// window have something to act on. Spec F1.
fn detect_violations(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    daemon_state: &Arc<Mutex<DaemonState>>,
    now: Instant,
) -> Option<Violation> {
    let cfg = config_arc.read().unwrap();
    let bl = baselines.read().unwrap();
    let allowed = allowed_ids(daemon_state, now);

    // USB check
    if cfg.general.watch_usb
        && let Some(ref baseline) = bl.usb
    {
        match usb::enumerate_devices() {
            Ok(current) => {
                if let Some(change) =
                    current.detect_changes(baseline, &build_usb_whitelist(&cfg, &allowed))
                {
                    let appeared = matches!(
                        change,
                        DeviceChange::Added(_) | DeviceChange::CountIncreased { .. }
                    );
                    let id = change.device_id().clone();
                    let name = bl
                        .names
                        .usb
                        .get(&(id.vendor_id.clone(), id.product_id.clone()))
                        .cloned();
                    return Some(Violation::device(
                        format!("USB VIOLATION: {change}{}", shown(&name)),
                        DeviceRef::Usb(id),
                        name,
                        appeared,
                    ));
                }
            }
            Err(e) => {
                return Some(Violation::event(
                    "usb",
                    format!("USB VIOLATION: enumeration failure (possible tampering): {e}"),
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
                    current.detect_changes(tb_base, &build_thunderbolt_whitelist(&cfg, &allowed))
                {
                    let appeared = matches!(change, ThunderboltChange::Added(_));
                    let id = change.device_id().clone();
                    let name = bl.names.thunderbolt.get(&id.unique_id).cloned();
                    return Some(Violation::device(
                        format!("THUNDERBOLT VIOLATION: {change}{}", shown(&name)),
                        DeviceRef::Thunderbolt(id),
                        name,
                        appeared,
                    ));
                }
            }
            Err(e) => {
                return Some(Violation::event(
                    "thunderbolt",
                    format!("THUNDERBOLT VIOLATION: enumeration failure (possible tampering): {e}"),
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
                if let Some(change) =
                    current.detect_changes(sd_base, &build_sdcard_whitelist(&cfg, &allowed))
                {
                    let appeared = matches!(change, SdCardChange::Added(_));
                    let id = change.device_id().clone();
                    let name = bl.names.sdcard.get(&id.serial).cloned();
                    return Some(Violation::device(
                        format!("SD CARD VIOLATION: {change}{}", shown(&name)),
                        DeviceRef::SdCard(id),
                        name,
                        appeared,
                    ));
                }
            }
            Err(e) => {
                return Some(Violation::event(
                    "sdcard",
                    format!("SD CARD VIOLATION: enumeration failure (possible tampering): {e}"),
                ));
            }
        }
    }

    // Power check is handled separately in check_power_violation() because
    // it needs mutable access to DaemonState for grace period tracking.
    None
}

/// A friendly name as it appears in a violation line, or nothing.
fn shown(name: &Option<String>) -> String {
    name.as_ref().map(|n| format!(" [{n}]")).unwrap_or_default()
}

/// The identities of every allowance that has not expired at `now`, lifted out
/// of the state lock so the bus checkers can consult them without holding it.
/// Expired entries are already invisible here, before any sweep runs.
fn allowed_ids(daemon_state: &Arc<Mutex<DaemonState>>, now: Instant) -> HashSet<DeviceRef> {
    daemon_state
        .lock()
        .unwrap()
        .allowances
        .active(now)
        .map(|a| a.id.clone())
        .collect()
}

/// The config ignore list plus every allowed PCI selector. PCI filters at
/// enumeration rather than against a whitelist snapshot, so that is where its
/// allowances go: the same point the config exception is already applied.
/// Spec A4.
fn pci_ignore(cfg: &config::Config, allowed: &HashSet<DeviceRef>) -> Vec<String> {
    cfg.pci
        .ignore
        .iter()
        .cloned()
        .chain(allowed.iter().filter_map(|id| match id {
            DeviceRef::Pci(selector) => Some(selector.clone()),
            _ => None,
        }))
        .collect()
}

/// Blank every connector holding an allowed monitor. Applied to both sides of
/// a comparison it makes that monitor invisible: it matches on whatever port
/// it is plugged into, and a different monitor on the same port still reads as
/// a change. Identity, never connector name. Spec A2a, A4.
fn hide_allowed_displays(
    snapshot: &display::DisplaySnapshot,
    allowed: &HashSet<DeviceRef>,
) -> display::DisplaySnapshot {
    let mut out = snapshot.clone();
    if allowed.is_empty() {
        return out;
    }
    for c in &mut out.connectors {
        if c.edid
            .as_ref()
            .is_some_and(|e| allowed.contains(&DeviceRef::Display(e.clone())))
        {
            c.connected = false;
            c.edid = None;
        }
    }
    out
}

/// Drop every device matching an active allowance out of a freshly captured
/// baseline. This is what separates an allowance from a disarm: a device
/// captured into the baseline would stay accepted whether or not its allowance
/// still existed, and revoking would do nothing. Skipping it means the device
/// is accepted because of the allowance and for no other reason, so revoking
/// makes it read as newly appeared on the next poll. PCI is absent here
/// because it is filtered at enumeration by `pci_ignore`. Spec C3, F3.
///
/// `usb_captured` says whether this call re-enumerated the USB bus. USB is the
/// one bus that counts instances, so a standing baseline taken before the
/// allowance existed already accounts for the device: stripping it there as
/// well would leave the live count one above what the baseline and the
/// whitelist allow between them, which is a kill with no hardware change.
fn skip_allowed(bl: &mut Baselines, allowed: &HashSet<DeviceRef>, usb_captured: bool) {
    if allowed.is_empty() {
        return;
    }
    if usb_captured && let Some(usb) = bl.usb.take() {
        bl.usb = Some(DeviceSnapshot::from_map(
            usb.devices()
                .iter()
                // An allowance covers one instance, the same as the count of 1
                // build_usb_whitelist credits it with. Dropping the whole entry
                // would leave the other instances uncovered.
                .filter_map(|(id, count)| {
                    let keep = if allowed.contains(&DeviceRef::Usb(id.clone())) {
                        count.saturating_sub(1)
                    } else {
                        *count
                    };
                    (keep > 0).then(|| (id.clone(), keep))
                })
                .collect(),
        ));
    }
    if let Some(tb) = bl.thunderbolt.take() {
        bl.thunderbolt = Some(ThunderboltSnapshot::from_set(
            tb.devices()
                .iter()
                .filter(|id| !allowed.contains(&DeviceRef::Thunderbolt((*id).clone())))
                .cloned()
                .collect(),
        ));
    }
    if let Some(sd) = bl.sdcard.take() {
        bl.sdcard = Some(SdCardSnapshot::from_set(
            sd.devices()
                .iter()
                .filter(|id| !allowed.contains(&DeviceRef::SdCard((*id).clone())))
                .cloned()
                .collect(),
        ));
    }
    if let Some(dp) = bl.display.take() {
        bl.display = Some(hide_allowed_displays(&dp, allowed));
    }
}

/// Check for PCI add/remove violations, carrying the selector of the device
/// behind one so `pair` and `allow_last` have something to act on (F1). A
/// removal keeps no identity: there is nothing plugged in to allow. Enumeration
/// failure is logged rather than treated as a kill, since the FreeBSD backend
/// shells out to pciconf.
fn check_pci_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    daemon_state: &Arc<Mutex<DaemonState>>,
    now: Instant,
) -> Option<Violation> {
    let cfg = config_arc.read().unwrap();
    if !cfg.general.watch_pci {
        return None;
    }

    let bl = baselines.read().unwrap();
    let baseline = bl.pci.as_ref()?;

    let ignore = pci_ignore(&cfg, &allowed_ids(daemon_state, now));
    let current = match pci::enumerate_pci(&ignore) {
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

    let description = format!("PCI VIOLATION: {change}");
    Some(match change {
        pci::PciChange::Added(selector) => {
            Violation::device(description, DeviceRef::Pci(selector), None, true)
        }
        pci::PciChange::Removed(_) => Violation::event("pci", description),
    })
}

/// Check for display changes by comparing the current snapshot against the
/// baseline. Reports the first difference: a connector appearing or
/// disappearing, connecting or disconnecting, or a different monitor on the
/// same connector. Carries the monitor's EDID identity when the change names
/// one, so `pair` and `allow_last` can allow that monitor and not the port it
/// happens to sit in (F1, A2a).
fn check_display_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    daemon_state: &Arc<Mutex<DaemonState>>,
    now: Instant,
) -> Option<Violation> {
    let cfg = config_arc.read().unwrap();
    if !cfg.general.watch_display {
        return None;
    }

    let baseline = baselines.read().unwrap().display.clone()?;
    let allowed = allowed_ids(daemon_state, now);
    // Both sides are blanked, so an allowed monitor cancels out wherever it is
    // plugged in. The blanked snapshot is never stored, so a revoke takes
    // effect on the next poll with nothing to undo.
    let current = hide_allowed_displays(&display::display_snapshot(&cfg.display.ignore), &allowed);
    let change = current.detect_changes(&hide_allowed_displays(&baseline, &allowed))?;

    if cfg.display.policy == DisplayPolicy::Monitor {
        info!("display change: {change} (monitor mode, no action)");
        baselines.write().unwrap().display = Some(current);
        return None;
    }

    let description = format!("DISPLAY VIOLATION: {change}");
    Some(match display_identity(&change, &current) {
        Some(id) => {
            let name = Some(id.name.clone()).filter(|n| !n.is_empty());
            Violation::device(description, DeviceRef::Display(id), name, true)
        }
        None => Violation::event("display", description),
    })
}

/// The monitor a display change names, when it names one. A replacement
/// carries the new panel's identity; a connector appearing or connecting is
/// looked up in the snapshot that reported it. A disconnect, a connector
/// vanishing and a platform that reports only a counter have no monitor to
/// allow. Spec A2a, B6.
fn display_identity(
    change: &display::DisplayChange,
    current: &display::DisplaySnapshot,
) -> Option<plugkill_core::edid::EdidId> {
    let connector = match change {
        display::DisplayChange::Replaced { now, .. } => return now.clone(),
        display::DisplayChange::Appeared(c) | display::DisplayChange::Connected(c) => c,
        _ => return None,
    };
    current
        .connectors
        .iter()
        .find(|k| &k.connector == connector)
        .and_then(|k| k.edid.clone())
}

/// Check for power supply violations, managing grace period and trigger-once state.
/// Returns a violation description if one should be triggered, or None.
fn check_power_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    daemon_state: &Arc<Mutex<DaemonState>>,
) -> Option<String> {
    check_power_violation_with(
        power::read_power_state,
        power::is_session_locked,
        config_arc,
        baselines,
        daemon_state,
    )
}

/// The decision behind `check_power_violation`, with the two hardware reads
/// handed in so the grace set and clear paths can be tested. Both stay lazy:
/// neither is read on a poll that returns before it needs them.
fn check_power_violation_with(
    read_power: impl FnOnce() -> PowerState,
    session_locked: impl FnOnce() -> Option<bool>,
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

    let current = read_power();
    let mut st = daemon_state.lock().unwrap();

    if cfg.power.policy == PowerPolicy::Monitor {
        st.power_grace = None;
        if current != baseline_power {
            info!("power state changed: {baseline_power} → {current} (monitor mode, no action)");
            // The baseline moves with the reading, or the same transition is
            // logged again on every poll for as long as the cable stays out.
            drop(st);
            let mut bl = baselines.write().unwrap();
            bl.power = Some(current);
        }
        return None;
    }

    let on_battery = current == PowerState::Battery;

    if !on_battery {
        if st.power_unplug_at.is_some() {
            info!("AC power restored during grace period");
            st.power_unplug_at = None;
        }
        st.power_grace = None;
        return None;
    }

    if cfg.power.policy == PowerPolicy::TriggerOnce && st.power_trigger_once_fired {
        return None;
    }

    // Already on battery when the baseline was taken, so nothing was
    // unplugged to notice.
    if baseline_power == PowerState::Battery {
        return None;
    }

    if cfg.power.require_locked {
        match session_locked() {
            Some(true) => {}
            Some(false) => {
                // Someone is sitting at the machine, so a cable coming out is
                // not a theft. The time is still recorded: if the session
                // locks later, the countdown runs from when it came out.
                if st.power_unplug_at.is_none() {
                    st.power_unplug_at = Some(Instant::now());
                }
                // No countdown runs while the lock requirement is unmet.
                st.power_grace = None;
                return None;
            }
            None => {
                // A lock state nobody can read must not become a way to
                // switch the check off.
                warn!("cannot determine session lock state, proceeding with power check");
            }
        }
    }

    if cfg.power.grace_secs > 0 {
        let now = Instant::now();
        let grace = Duration::from_secs(cfg.power.grace_secs);
        match st.power_unplug_at {
            None => {
                info!(
                    "AC power removed, grace period started ({}s)",
                    cfg.power.grace_secs
                );
                st.power_unplug_at = Some(now);
                st.power_grace = Some(Grace {
                    until: now + grace,
                    reason: "AC power removed".to_string(),
                });
                return None;
            }
            Some(unplug_time) => {
                if now.duration_since(unplug_time) < grace {
                    // Still within grace period. Set rather than keep: a
                    // session that just locked starts its countdown here.
                    st.power_grace = Some(Grace {
                        until: unplug_time + grace,
                        reason: "AC power removed".to_string(),
                    });
                    return None;
                }
                // Past the grace period, so this falls through to the
                // violation below.
            }
        }
    } else if st.power_unplug_at.is_none() {
        // Nothing is waited out, so the time is only for the log.
        st.power_unplug_at = Some(Instant::now());
    }
    st.power_grace = None;

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
    check_network_violation_with(
        network::enumerate_interfaces,
        config_arc,
        baselines,
        daemon_state,
    )
}

/// The decision behind `check_network_violation`, with the enumeration handed
/// in so the grace set and clear paths can be tested. It stays lazy: an
/// unwatched or unbaselined bus never enumerates.
fn check_network_violation_with(
    read_interfaces: impl FnOnce(&[String]) -> network::NetworkSnapshot,
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

    let current = read_interfaces(&cfg.network.interfaces);

    let change = match current.detect_link_down(baseline_network) {
        Some(c) => c,
        None => {
            let mut st = daemon_state.lock().unwrap();
            if st.network_link_down_at.is_some() {
                info!("network link restored during grace period");
                st.network_link_down_at = None;
            }
            st.network_grace = None;
            return None;
        }
    };

    drop(bl);

    if cfg.network.policy == NetworkPolicy::Monitor {
        info!("network link change: {change} (monitor mode, no action)");
        let mut st = daemon_state.lock().unwrap();
        st.network_grace = None;
        drop(st);
        // The baseline moves with the reading, or the same change is logged
        // again on every poll for as long as the link stays down.
        let mut bl = baselines.write().unwrap();
        bl.network = Some(current);
        return None;
    }

    let mut st = daemon_state.lock().unwrap();

    if cfg.network.grace_secs > 0 {
        let now = Instant::now();
        let grace = Duration::from_secs(cfg.network.grace_secs);
        let reason = format!("link down on {}", change.interface);
        match st.network_link_down_at {
            None => {
                info!(
                    "network link down on {}, grace period started ({}s)",
                    change.interface, cfg.network.grace_secs
                );
                st.network_link_down_at = Some(now);
                st.network_grace = Some(Grace {
                    until: now + grace,
                    reason,
                });
                return None;
            }
            Some(down_time) => {
                if now.duration_since(down_time) < grace {
                    st.network_grace = Some(Grace {
                        until: down_time + grace,
                        reason,
                    });
                    return None;
                }
            }
        }
    } else if st.network_link_down_at.is_none() {
        st.network_link_down_at = Some(Instant::now());
    }
    st.network_grace = None;

    Some(format!("NETWORK VIOLATION: {change}"))
}

/// Check for lid close violations, managing grace period.
/// Returns a violation description if one should be triggered, or None.
fn check_lid_violation(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
    daemon_state: &Arc<Mutex<DaemonState>>,
) -> Option<String> {
    check_lid_violation_with(lid::read_lid_state, config_arc, baselines, daemon_state)
}

/// The decision behind `check_lid_violation`, with the logind read handed in
/// so the grace set and clear paths can be tested. It stays lazy: an unwatched
/// or unbaselined bus never calls logind.
fn check_lid_violation_with(
    read_lid: impl FnOnce() -> LidState,
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

    let current = read_lid();

    if current != LidState::Closed {
        let mut st = daemon_state.lock().unwrap();
        if st.lid_close_at.is_some() {
            info!("lid reopened during grace period");
            st.lid_close_at = None;
        }
        st.lid_grace = None;
        return None;
    }

    // Already closed when the baseline was taken, so nothing closed.
    if baseline_lid == LidState::Closed {
        return None;
    }

    if cfg.lid.policy == LidPolicy::Monitor {
        info!("lid closed (monitor mode, no action)");
        let mut st = daemon_state.lock().unwrap();
        st.lid_grace = None;
        drop(st);
        let mut bl = baselines.write().unwrap();
        bl.lid = Some(current);
        return None;
    }

    let mut st = daemon_state.lock().unwrap();

    if cfg.lid.grace_secs > 0 {
        let now = Instant::now();
        let grace = Duration::from_secs(cfg.lid.grace_secs);
        match st.lid_close_at {
            None => {
                info!("lid closed, grace period started ({}s)", cfg.lid.grace_secs);
                st.lid_close_at = Some(now);
                st.lid_grace = Some(Grace {
                    until: now + grace,
                    reason: "lid closed".to_string(),
                });
                return None;
            }
            Some(close_time) => {
                if now.duration_since(close_time) < grace {
                    st.lid_grace = Some(Grace {
                        until: close_time + grace,
                        reason: "lid closed".to_string(),
                    });
                    return None;
                }
            }
        }
    } else if st.lid_close_at.is_none() {
        st.lid_close_at = Some(Instant::now());
    }
    st.lid_grace = None;

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

/// USB product names keyed by (vendor id, product id), the shape
/// `DeviceNames::usb` holds.
type UsbNames = HashMap<(String, String), String>;

/// Capture USB baseline, returning None on enumeration failure.
/// Also returns a name lookup map from detailed enumeration.
///
/// The caller decides what a failure means, because the two callers need
/// opposite things. Startup exits: a daemon that cannot read the bus it is
/// configured to watch has nothing to offer. A reload or re-arm must not,
/// because a reload can newly enable USB (including re-enabling a bus whose
/// baseline the clearing logic zeroed), and exiting there turns a transient
/// enumeration failure into a restart loop under Restart=on-failure, with the
/// config still asking for the bus.
fn capture_usb_baseline() -> Option<(DeviceSnapshot, UsbNames)> {
    let snapshot = match usb::enumerate_devices() {
        Ok(s) => s,
        Err(e) => {
            error!("failed to enumerate USB devices: {e}");
            return None;
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

    Some((snapshot, names))
}

/// Whether the bus root directory exists, which is what separates absent
/// hardware from a bus that is present but unreadable. The enumeration errors
/// cannot answer this: `Error::Thunderbolt` and `Error::SdCard` format the
/// `io::Error` into a string, so the `ErrorKind` is gone by the time a caller
/// sees it.
///
/// Takes the bus directory rather than its `devices` child on purpose: a bus
/// that exists with an unreadable `devices` child is present-but-unreadable,
/// not absent.
///
/// Linux only. FreeBSD enumerates through devinfo rather than a filesystem
/// path, so there is nothing to stat, and non-Linux answers true. That routes
/// every failure to the warn branch, which is the safe direction: it
/// over-reports rather than hiding a bus that is present and unreadable.
fn bus_root_exists(sysfs_bus_root: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(sysfs_bus_root).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sysfs_bus_root;
        true
    }
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
            // A present bus that will not enumerate is never silent: the
            // baseline stays None, the checker short-circuits on None forever,
            // and --status still reports thunderbolt_watching from the config
            // flag, so without this line the bus is unmonitored behind a
            // positive watching indicator. Absent hardware is not that case
            // and does not get a warning, or the line that matters drowns in
            // one printed on every machine without a controller.
            if bus_root_exists("/sys/bus/thunderbolt") {
                warn!("Thunderbolt baseline failed, monitoring disabled: {e}");
            } else {
                info!("no Thunderbolt hardware found, Thunderbolt monitoring inactive");
            }
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
            if bus_root_exists("/sys/bus/mmc") {
                warn!("SD card baseline failed, monitoring disabled: {e}");
            } else {
                info!("no MMC bus found, SD card monitoring inactive");
            }
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
    now: Instant,
) {
    // The poll's own clock reading, so the sweep, this capture and the
    // detection that follows all agree on which allowances are still active.
    let allowed = allowed_ids(daemon_state, now);

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

    let usb_captured = cfg.general.watch_usb && !(only_missing && bl.usb.is_some());
    if usb_captured {
        match capture_usb_baseline() {
            Some((snapshot, names)) => {
                bl.usb = Some(snapshot);
                bl.names.usb = names;
            }
            // Not an exit: this runs on reload and re-arm, where exiting would
            // restart-loop against a config that still asks for the bus.
            None => {
                warn!("USB baseline failed, monitoring disabled until the next re-baseline");
                bl.usb = None;
            }
        }
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
        st.power_grace = None;
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
        st.network_grace = None;
    }
    if cfg.general.watch_lid && !(only_missing && bl.lid.is_some()) {
        let state = lid::read_lid_state();
        info!("lid baseline: {state}");
        bl.lid = Some(state);
        let mut st = daemon_state.lock().unwrap();
        st.lid_close_at = None;
        st.lid_grace = None;
    }
    if cfg.general.watch_pci && !(only_missing && bl.pci.is_some()) {
        match pci::enumerate_pci(&pci_ignore(cfg, &allowed)) {
            Ok(snapshot) => {
                info!("PCI baseline: {} device(s)", snapshot.len());
                bl.pci = Some(snapshot);
            }
            Err(e) => warn!("PCI baseline failed: {e}"),
        }
    }
    if cfg.general.watch_display && !(only_missing && bl.display.is_some()) {
        let snapshot = display::display_snapshot(&cfg.display.ignore);
        info!(
            "display baseline captured: {} connector(s)",
            snapshot.connectors.len()
        );
        bl.display = Some(snapshot);
    }

    skip_allowed(bl, &allowed, usb_captured);
}

/// Build a DeviceSnapshot from the USB whitelist config entries plus any
/// active USB allowance. An allowance counts for one instance, the same as a
/// whitelist entry with count 1, so it is consulted at exactly the point the
/// config exception already is. Spec A4, F2.
fn build_usb_whitelist(cfg: &config::Config, allowed: &HashSet<DeviceRef>) -> DeviceSnapshot {
    let mut map = HashMap::new();
    for entry in &cfg.whitelist.devices {
        let id = UsbDeviceId {
            vendor_id: entry.vendor_id.clone(),
            product_id: entry.product_id.clone(),
        };
        *map.entry(id).or_insert(0) += entry.count;
    }
    for id in allowed {
        if let DeviceRef::Usb(usb) = id {
            *map.entry(usb.clone()).or_insert(0) += 1;
        }
    }
    DeviceSnapshot::from_map(map)
}

/// The thunderbolt whitelist config entries plus any active Thunderbolt
/// allowance. Spec A4, F2.
fn build_thunderbolt_whitelist(
    cfg: &config::Config,
    allowed: &HashSet<DeviceRef>,
) -> ThunderboltSnapshot {
    ThunderboltSnapshot::from_set(
        cfg.thunderbolt_whitelist
            .devices
            .iter()
            .map(|entry| ThunderboltDeviceId {
                unique_id: entry.unique_id.clone(),
            })
            .chain(allowed.iter().filter_map(|id| match id {
                DeviceRef::Thunderbolt(tb) => Some(tb.clone()),
                _ => None,
            }))
            .collect(),
    )
}

/// The SD card whitelist config entries plus any active SD card allowance.
/// Spec A4, F2.
fn build_sdcard_whitelist(cfg: &config::Config, allowed: &HashSet<DeviceRef>) -> SdCardSnapshot {
    SdCardSnapshot::from_set(
        cfg.sdcard_whitelist
            .devices
            .iter()
            .map(|entry| SdCardDeviceId {
                serial: entry.serial.clone(),
            })
            .chain(allowed.iter().filter_map(|id| match id {
                DeviceRef::SdCard(sd) => Some(sd.clone()),
                _ => None,
            }))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugkill_core::allowances::{Allowance, Grant};

    /// Only the display bus is on. Display is the one bus a unit test can
    /// drive: `display_snapshot` reports whatever sysfs offers and cannot
    /// fail, while the USB, Thunderbolt and SD card captures need a real
    /// sysfs and so produce a different result per host.
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

    // --- runtime allowances ------------------------------------------------

    fn usb_id(vendor: &str, product: &str) -> UsbDeviceId {
        UsbDeviceId {
            vendor_id: vendor.to_string(),
            product_id: product.to_string(),
        }
    }

    fn allowed<const N: usize>(ids: [DeviceRef; N]) -> HashSet<DeviceRef> {
        ids.into_iter().collect()
    }

    fn monitor(mfg: &str, product: u16, serial: u32) -> plugkill_core::edid::EdidId {
        plugkill_core::edid::EdidId {
            mfg: mfg.to_string(),
            product,
            serial: Some(serial),
            name: String::new(),
        }
    }

    fn attached(
        name: &str,
        edid: Option<plugkill_core::edid::EdidId>,
    ) -> display::DisplayConnector {
        display::DisplayConnector {
            connector: name.to_string(),
            connected: edid.is_some(),
            edid,
        }
    }

    fn snapshot_of(counts: &[(&UsbDeviceId, u32)]) -> DeviceSnapshot {
        DeviceSnapshot::from_map(counts.iter().map(|(id, n)| ((*id).clone(), *n)).collect())
    }

    fn tb_id(unique: &str) -> ThunderboltDeviceId {
        ThunderboltDeviceId {
            unique_id: unique.to_string(),
        }
    }

    fn sd_id(serial: &str) -> SdCardDeviceId {
        SdCardDeviceId {
            serial: serial.to_string(),
        }
    }

    fn granted(state: &Arc<Mutex<DaemonState>>, id: DeviceRef) {
        state.lock().unwrap().allowances.add(Allowance::new(
            id,
            Grant::Promoted,
            Instant::now(),
            None,
        ));
    }

    /// A state with an open pairing window, which is what `admit_or_keep` is
    /// deciding against.
    fn window_state(now: Instant, left: Duration) -> Arc<Mutex<DaemonState>> {
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.pairing = Some(daemon_state::PairingWindow {
            until: now + left,
            ttl: None,
        });
        Arc::new(Mutex::new(st))
    }

    /// Only the USB bus is watched, so a capture with a baseline already in
    /// place enumerates nothing and the test needs no hardware.
    fn usb_only_config() -> config::Config {
        let mut cfg = config::Config::default();
        cfg.general.watch_thunderbolt = false;
        cfg.general.watch_sdcard = false;
        cfg
    }

    /// The same, for the Thunderbolt bus.
    fn thunderbolt_only_config() -> config::Config {
        let mut cfg = config::Config::default();
        cfg.general.watch_usb = false;
        cfg.general.watch_sdcard = false;
        cfg
    }

    /// G1, the property that separates an allowance from a disarm. A captured
    /// baseline must not contain an allowed device: if it did, the device
    /// would stay accepted after the allowance was revoked and revoking would
    /// do nothing.
    #[test]
    fn test_an_allowed_device_is_absent_from_a_captured_baseline() {
        let hub = usb_id("1d6b", "0002");
        let stick = usb_id("0781", "5583");
        let mut bl = empty_baselines();
        bl.usb = Some(snapshot_of(&[(&hub, 1), (&stick, 1)]));

        skip_allowed(&mut bl, &allowed([DeviceRef::Usb(stick.clone())]), true);

        let captured = bl.usb.expect("the usb baseline is still there");
        assert_eq!(
            captured.count_of(&stick),
            0,
            "an allowed device that lands in the baseline makes revoking a no-op"
        );
        assert_eq!(captured.count_of(&hub), 1, "everything else is untouched");
    }

    /// An allowance covers one instance, which is what the whitelist credits
    /// it with, so a baseline holding two of the same id keeps the other one.
    /// Dropping the whole entry left the live count one above what the
    /// baseline and the whitelist allow between them: a kill with no hardware
    /// change at all. Spec C3, A4.
    #[test]
    fn test_an_allowance_takes_one_instance_out_of_the_baseline_not_the_entry() {
        let cfg = config::Config::default();
        let hub = usb_id("1d6b", "0002");
        let grant = allowed([DeviceRef::Usb(hub.clone())]);
        let mut bl = empty_baselines();
        bl.usb = Some(snapshot_of(&[(&hub, 2)]));

        skip_allowed(&mut bl, &grant, true);

        let captured = bl.usb.expect("the usb baseline is still there");
        assert_eq!(
            captured.count_of(&hub),
            1,
            "one instance goes, not the entry"
        );
        let live = snapshot_of(&[(&hub, 2)]);
        assert_eq!(
            live.detect_changes(&captured, &build_usb_whitelist(&cfg, &grant)),
            None,
            "baseline 1 plus the allowance covers both instances"
        );
        assert!(
            matches!(
                live.detect_changes(&captured, &build_usb_whitelist(&cfg, &HashSet::new())),
                Some(DeviceChange::CountIncreased { .. })
            ),
            "revoked, the second instance is a violation again"
        );
    }

    /// A baseline this capture did not re-enumerate was taken before the
    /// allowance existed, so it already accounts for the device. Stripping it
    /// there as well is a kill on a reload with nothing unplugged.
    #[test]
    fn test_a_reload_leaves_a_baseline_it_did_not_recapture_alone() {
        let cfg = usb_only_config();
        let dongle = usb_id("046d", "c52b");
        let grant = allowed([DeviceRef::Usb(dongle.clone())]);
        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        granted(&state, DeviceRef::Usb(dongle.clone()));
        let mut bl = empty_baselines();
        bl.usb = Some(snapshot_of(&[(&dongle, 1)]));

        capture_baselines(&cfg, &mut bl, &state, true, Instant::now());

        let captured = bl.usb.expect("the usb baseline is still there");
        assert_eq!(
            captured.count_of(&dongle),
            1,
            "a baseline taken before the allowance already accounts for it"
        );
        assert_eq!(
            snapshot_of(&[(&dongle, 2)])
                .detect_changes(&captured, &build_usb_whitelist(&cfg, &grant)),
            None,
            "the allowance covers the second one, and reloading does not kill"
        );
    }

    /// G1 and C5 through the function the daemon actually calls. Asserting on
    /// `skip_allowed` alone cannot catch the call going missing from
    /// `capture_baselines`, which would turn every allowance back into a
    /// permanent baseline promotion.
    #[test]
    fn test_capture_baselines_strips_allowed_devices_and_keeps_the_table() {
        let dock = tb_id("dock-0001");
        let cable = tb_id("cable-0002");
        let cfg = thunderbolt_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        granted(&state, DeviceRef::Thunderbolt(dock.clone()));
        let mut bl = empty_baselines();
        bl.thunderbolt = Some(ThunderboltSnapshot::from_set(
            [dock.clone(), cable.clone()].into_iter().collect(),
        ));

        capture_baselines(&cfg, &mut bl, &state, true, Instant::now());

        let captured = bl.thunderbolt.expect("the baseline is still there");
        assert!(
            !captured.devices().contains(&dock),
            "an allowed device in the baseline makes revoking a no-op"
        );
        assert!(captured.devices().contains(&cable), "the rest is untouched");
        assert_eq!(
            state.lock().unwrap().allowances.len(),
            1,
            "a capture re-baselines, it does not drop allowances"
        );
    }

    /// A4 for Thunderbolt: the allowance reaches both the baseline skip and
    /// the whitelist, and revoking turns the dock back into a violation.
    #[test]
    fn test_a_thunderbolt_allowance_covers_the_dock_until_it_is_revoked() {
        let cfg = config::Config::default();
        let dock = tb_id("dock-0001");
        let grant = allowed([DeviceRef::Thunderbolt(dock.clone())]);
        let mut bl = empty_baselines();
        bl.thunderbolt = Some(ThunderboltSnapshot::from_set([dock.clone()].into()));

        skip_allowed(&mut bl, &grant, true);
        let baseline = bl.thunderbolt.expect("the baseline is still there");
        let current = ThunderboltSnapshot::from_set([dock.clone()].into());

        assert!(baseline.devices().is_empty(), "C3: not in the baseline");
        assert_eq!(
            current.detect_changes(&baseline, &build_thunderbolt_whitelist(&cfg, &grant)),
            None,
            "while the allowance stands the dock passes"
        );
        assert_eq!(
            current.detect_changes(
                &baseline,
                &build_thunderbolt_whitelist(&cfg, &HashSet::new())
            ),
            Some(ThunderboltChange::Added(dock)),
            "revoked, it is a violation on the next poll"
        );
    }

    /// The same for SD card, the third bus that matches against a whitelist
    /// snapshot. Spec A4.
    #[test]
    fn test_an_sdcard_allowance_covers_the_card_until_it_is_revoked() {
        let cfg = config::Config::default();
        let card = sd_id("0x0000ba5e");
        let grant = allowed([DeviceRef::SdCard(card.clone())]);
        let mut bl = empty_baselines();
        bl.sdcard = Some(SdCardSnapshot::from_set([card.clone()].into()));

        skip_allowed(&mut bl, &grant, true);
        let baseline = bl.sdcard.expect("the baseline is still there");
        let current = SdCardSnapshot::from_set([card.clone()].into());

        assert!(baseline.devices().is_empty(), "C3: not in the baseline");
        assert_eq!(
            current.detect_changes(&baseline, &build_sdcard_whitelist(&cfg, &grant)),
            None,
            "while the allowance stands the card passes"
        );
        assert_eq!(
            current.detect_changes(&baseline, &build_sdcard_whitelist(&cfg, &HashSet::new())),
            Some(SdCardChange::Added(card)),
            "revoked, it is a violation on the next poll"
        );
    }

    /// G3 at the layer that decides it. Only an appearance carrying a device
    /// identity may be swallowed by a window: without that guard a device
    /// yanked out during one becomes an allowance and the kill switch does
    /// not fire on the yank.
    #[test]
    fn test_a_window_admits_only_an_identified_appearance() {
        let now = Instant::now();
        let stick = DeviceRef::Usb(usb_id("0781", "5583"));
        let seen = |appeared| {
            Violation::device("USB VIOLATION".to_string(), stick.clone(), None, appeared)
        };

        let st = window_state(now, Duration::from_secs(60));
        assert!(
            admit_or_keep(&st, seen(true), now).is_none(),
            "an appearance is admitted rather than killed for"
        );
        assert_eq!(st.lock().unwrap().allowances.len(), 1);

        let st = window_state(now, Duration::from_secs(60));
        assert!(
            admit_or_keep(&st, seen(false), now).is_some(),
            "a device pulled out stays a violation whatever window is open"
        );
        assert!(st.lock().unwrap().allowances.is_empty());

        let st = window_state(now, Duration::from_secs(60));
        let event = Violation::event("power", "POWER VIOLATION: AC removed".to_string());
        assert!(
            admit_or_keep(&st, event, now).is_some(),
            "an event names no device, so there is nothing to admit"
        );
        assert!(st.lock().unwrap().allowances.is_empty());

        let st = window_state(now, Duration::ZERO);
        assert!(
            admit_or_keep(&st, seen(true), now).is_some(),
            "a window that has run out admits nothing"
        );
    }

    /// G11 on the path a grant runs: the change has to carry the monitor it
    /// names, or `pair` and `allow_last` are inert for the display bus.
    #[test]
    fn test_a_display_change_carries_the_monitor_it_names() {
        let mine = monitor("SAM", 0x772d, 811_021_873);
        let current =
            display::DisplaySnapshot::from_connectors(vec![attached("DP-1", Some(mine.clone()))]);
        let replaced = display::DisplayChange::Replaced {
            connector: "DP-1".to_string(),
            now: Some(mine.clone()),
        };

        assert_eq!(display_identity(&replaced, &current), Some(mine.clone()));
        assert_eq!(
            display_identity(&display::DisplayChange::Connected("DP-1".into()), &current),
            Some(mine),
            "a connector connecting is looked up in the snapshot that saw it"
        );
        for blind in [
            display::DisplayChange::Disconnected("DP-1".into()),
            display::DisplayChange::Connected("DP-9".into()),
            display::DisplayChange::TopologyChanged,
        ] {
            assert_eq!(
                display_identity(&blind, &current),
                None,
                "{blind} names no monitor to allow"
            );
        }
    }

    /// The other half of G11: that identity becomes a display allowance, on
    /// the monitor and not the port. Linux only, because `admit_or_keep`
    /// passes the build-time `DISPLAY_IDENTITY` and a build without connector
    /// data refuses a display by design (A2b).
    #[cfg(target_os = "linux")]
    #[test]
    fn test_a_pairing_window_allows_the_monitor_a_swap_brought_in() {
        let now = Instant::now();
        let mine = monitor("SAM", 0x772d, 811_021_873);
        let current =
            display::DisplaySnapshot::from_connectors(vec![attached("DP-1", Some(mine.clone()))]);
        let change = display::DisplayChange::Replaced {
            connector: "DP-1".to_string(),
            now: Some(mine),
        };
        let id = display_identity(&change, &current).expect("a swap names the new monitor");

        let st = window_state(now, Duration::from_secs(60));
        let violation = Violation::device(
            format!("DISPLAY VIOLATION: {change}"),
            DeviceRef::Display(id),
            None,
            true,
        );
        assert!(admit_or_keep(&st, violation, now).is_none());

        let st = st.lock().unwrap();
        assert_eq!(
            st.allowances
                .iter()
                .next()
                .expect("one allowance")
                .id
                .selector(),
            "display:SAM:772d:811021873"
        );
        assert!(st.pairing.is_none(), "the window closed behind it");
    }

    /// G2. The allowance is the only reason the device is accepted, so
    /// dropping it makes the device read as newly appeared.
    #[test]
    fn test_revoking_an_allowance_makes_the_device_a_violation() {
        let cfg = config::Config::default();
        let stick = usb_id("0781", "5583");
        let grant = allowed([DeviceRef::Usb(stick.clone())]);

        let mut bl = empty_baselines();
        bl.usb = Some(snapshot_of(&[(&stick, 1)]));
        skip_allowed(&mut bl, &grant, true);
        let baseline = bl.usb.expect("the usb baseline is still there");
        let current = snapshot_of(&[(&stick, 1)]);

        assert_eq!(
            current.detect_changes(&baseline, &build_usb_whitelist(&cfg, &grant)),
            None,
            "while the allowance stands the device passes"
        );
        assert_eq!(
            current.detect_changes(&baseline, &build_usb_whitelist(&cfg, &HashSet::new())),
            Some(DeviceChange::Added(stick)),
            "revoked, it is a violation on the next poll"
        );
    }

    /// G8. An expiry drops the allowance, and the device is still plugged in.
    #[test]
    fn test_an_expired_allowance_is_dropped_and_the_device_becomes_a_violation() {
        let cfg = config::Config::default();
        let stick = usb_id("0781", "5583");
        let now = Instant::now();
        let mut st = DaemonState::new_for_test(DaemonMode::Enforce);
        st.pairing = Some(daemon_state::PairingWindow {
            until: now + Duration::from_secs(60),
            ttl: Some(Duration::from_secs(30)),
        });
        assert!(st.admit_paired(DeviceRef::Usb(stick.clone()), None, now, true));

        let live: HashSet<_> = st.allowances.active(now).map(|a| a.id.clone()).collect();
        let current = snapshot_of(&[(&stick, 1)]);
        let baseline = snapshot_of(&[]);
        assert_eq!(
            current.detect_changes(&baseline, &build_usb_whitelist(&cfg, &live)),
            None
        );

        let later = now + Duration::from_secs(31);
        assert_eq!(st.allowances.sweep(later).len(), 1, "the expiry drops it");
        let gone: HashSet<_> = st.allowances.active(later).map(|a| a.id.clone()).collect();
        assert_eq!(
            current.detect_changes(&baseline, &build_usb_whitelist(&cfg, &gone)),
            Some(DeviceChange::Added(stick))
        );
    }

    /// G11. The monitor is the identity, never the port: the swap an attacker
    /// would make is plugging a different panel into the allowed socket.
    #[test]
    fn test_a_display_allowance_follows_the_monitor_not_the_port() {
        let mine = monitor("SAM", 0x772d, 811_021_873);
        let theirs = monitor("DEL", 0x4321, 42);
        let grant = allowed([DeviceRef::Display(mine.clone())]);

        // What C3 leaves behind once the monitor is allowed: a baseline with
        // nothing on either port.
        let baseline = hide_allowed_displays(
            &display::DisplaySnapshot::from_connectors(vec![
                attached("DP-1", Some(mine.clone())),
                attached("DP-2", None),
            ]),
            &grant,
        );

        let moved = hide_allowed_displays(
            &display::DisplaySnapshot::from_connectors(vec![
                attached("DP-1", None),
                attached("DP-2", Some(mine)),
            ]),
            &grant,
        );
        assert_eq!(
            moved.detect_changes(&baseline),
            None,
            "the same monitor on another port stays allowed"
        );

        let swapped = hide_allowed_displays(
            &display::DisplaySnapshot::from_connectors(vec![
                attached("DP-1", Some(theirs)),
                attached("DP-2", None),
            ]),
            &grant,
        );
        assert_eq!(
            swapped.detect_changes(&baseline),
            Some(display::DisplayChange::Connected("DP-1".to_string())),
            "a different monitor on the allowed port is a violation"
        );
    }

    /// A PCI allowance is applied where the config exception already is, at
    /// enumeration. Spec A4.
    #[test]
    fn test_a_pci_allowance_joins_the_enumeration_ignore_list() {
        let mut cfg = config::Config::default();
        cfg.pci.ignore = vec!["0000:00:1f".to_string()];

        let ignore = pci_ignore(&cfg, &allowed([DeviceRef::Pci("0000:01:00.0".to_string())]));

        assert!(ignore.contains(&"0000:00:1f".to_string()));
        assert!(ignore.contains(&"0000:01:00.0".to_string()));
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

    /// A grace record that is already running, so a clear is visible.
    fn running_grace(state: &Arc<Mutex<DaemonState>>, reason: &str) {
        let mut st = state.lock().unwrap();
        st.power_grace = Some(Grace {
            until: Instant::now() + Duration::from_secs(60),
            reason: reason.to_string(),
        });
        st.network_grace = st.power_grace.clone();
        st.lid_grace = st.power_grace.clone();
    }

    /// What the three checkers take, in the order they take it.
    type Fixture = (
        Arc<RwLock<config::Config>>,
        Arc<RwLock<Baselines>>,
        Arc<Mutex<DaemonState>>,
    );

    /// Config, baselines and state for one bus's grace machine.
    fn grace_fixture(cfg: config::Config, baseline: impl FnOnce(&mut Baselines)) -> Fixture {
        let mut bl = empty_baselines();
        baseline(&mut bl);
        (
            Arc::new(RwLock::new(cfg)),
            Arc::new(RwLock::new(bl)),
            Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce))),
        )
    }

    fn power_config(policy: PowerPolicy, grace_secs: u64) -> config::Config {
        let mut cfg = config::Config::default();
        cfg.general.watch_power = true;
        cfg.power.policy = policy;
        cfg.power.grace_secs = grace_secs;
        cfg
    }

    /// A snapshot read from a mock `/sys/class/net`: one directory per
    /// interface, with the `device` marker that makes it a physical NIC.
    fn net(ifaces: &[(&str, &str)]) -> network::NetworkSnapshot {
        let dir = tempfile::tempdir().unwrap();
        for (name, operstate) in ifaces {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.join("device")).unwrap();
            std::fs::write(path.join("operstate"), format!("{operstate}\n")).unwrap();
        }
        network::enumerate_interfaces_from(dir.path(), &[])
    }

    /// The countdown starts on battery, and restoring AC has to take it back
    /// down. A grace left standing reports as a kill that never comes: the
    /// tray blinks "Kill in 0 s" until the daemon is reloaded.
    #[test]
    fn test_power_grace_starts_on_battery_and_clears_when_ac_returns() {
        let (config, baselines, state) =
            grace_fixture(power_config(PowerPolicy::AcRequired, 60), |bl| {
                bl.power = Some(PowerState::Ac)
            });

        let fired = check_power_violation_with(
            || PowerState::Battery,
            || None,
            &config,
            &baselines,
            &state,
        );
        assert_eq!(fired, None, "the grace period must hold the kill back");
        assert!(
            state.lock().unwrap().power_grace.is_some(),
            "unplugging must start a countdown the status can report"
        );

        let fired =
            check_power_violation_with(|| PowerState::Ac, || None, &config, &baselines, &state);
        assert_eq!(fired, None);
        let st = state.lock().unwrap();
        assert!(st.power_grace.is_none(), "AC restored must clear the grace");
        assert!(st.power_unplug_at.is_none());
    }

    /// Monitor never kills, so it must never leave a countdown behind either.
    #[test]
    fn test_power_monitor_policy_clears_a_grace() {
        let (config, baselines, state) =
            grace_fixture(power_config(PowerPolicy::Monitor, 60), |bl| {
                bl.power = Some(PowerState::Ac)
            });
        running_grace(&state, "AC power removed");

        let fired = check_power_violation_with(
            || PowerState::Battery,
            || None,
            &config,
            &baselines,
            &state,
        );

        assert_eq!(fired, None);
        assert!(state.lock().unwrap().power_grace.is_none());
        assert_eq!(
            baselines.read().unwrap().power,
            Some(PowerState::Battery),
            "monitor follows the change instead of logging it every poll"
        );
    }

    /// No countdown may run while the lock requirement is unmet: nothing is
    /// going to fire at the end of it.
    #[test]
    fn test_power_require_locked_unmet_clears_a_grace() {
        let mut cfg = power_config(PowerPolicy::AcRequired, 60);
        cfg.power.require_locked = true;
        let (config, baselines, state) = grace_fixture(cfg, |bl| bl.power = Some(PowerState::Ac));
        running_grace(&state, "AC power removed");

        let fired = check_power_violation_with(
            || PowerState::Battery,
            || Some(false),
            &config,
            &baselines,
            &state,
        );

        assert_eq!(fired, None, "an unlocked session is a present user");
        let st = state.lock().unwrap();
        assert!(st.power_grace.is_none(), "no countdown without the lock");
        assert!(
            st.power_unplug_at.is_some(),
            "the unplug time is kept, in case the session locks later"
        );
    }

    /// When the grace runs out the violation fires and the record goes away,
    /// so nothing reports a countdown past the kill.
    #[test]
    fn test_power_grace_expiry_fires_the_violation() {
        let (config, baselines, state) =
            grace_fixture(power_config(PowerPolicy::AcRequired, 60), |bl| {
                bl.power = Some(PowerState::Ac)
            });
        running_grace(&state, "AC power removed");
        state.lock().unwrap().power_unplug_at = Some(Instant::now() - Duration::from_secs(61));

        let fired = check_power_violation_with(
            || PowerState::Battery,
            || None,
            &config,
            &baselines,
            &state,
        );

        assert!(
            fired.is_some_and(|d| d.starts_with("POWER VIOLATION")),
            "an expired grace must fire"
        );
        assert!(state.lock().unwrap().power_grace.is_none());
    }

    /// Same shape for the network bus: a link that comes back has to take the
    /// countdown with it, and monitor must not leave one running.
    #[test]
    fn test_network_grace_clears_on_link_restored_and_on_monitor() {
        let mut cfg = config::Config::default();
        cfg.general.watch_network = true;
        cfg.network.policy = NetworkPolicy::Kill;
        cfg.network.grace_secs = 60;
        let (config, baselines, state) =
            grace_fixture(cfg, |bl| bl.network = Some(net(&[("eth0", "up")])));

        let fired =
            check_network_violation_with(|_| net(&[("eth0", "down")]), &config, &baselines, &state);
        assert_eq!(fired, None, "the grace period holds the kill back");
        assert!(state.lock().unwrap().network_grace.is_some());

        let fired =
            check_network_violation_with(|_| net(&[("eth0", "up")]), &config, &baselines, &state);
        assert_eq!(fired, None);
        let st = state.lock().unwrap();
        assert!(st.network_grace.is_none(), "a restored link clears it");
        assert!(st.network_link_down_at.is_none());
        drop(st);

        config.write().unwrap().network.policy = NetworkPolicy::Monitor;
        running_grace(&state, "link down on eth0");
        let fired =
            check_network_violation_with(|_| net(&[("eth0", "down")]), &config, &baselines, &state);
        assert_eq!(fired, None);
        assert!(
            state.lock().unwrap().network_grace.is_none(),
            "monitor must clear the countdown it will never act on"
        );
    }

    /// The network grace ends in a violation once it runs out.
    #[test]
    fn test_network_grace_expiry_fires_the_violation() {
        let mut cfg = config::Config::default();
        cfg.general.watch_network = true;
        cfg.network.policy = NetworkPolicy::Kill;
        cfg.network.grace_secs = 60;
        let (config, baselines, state) =
            grace_fixture(cfg, |bl| bl.network = Some(net(&[("eth0", "up")])));
        running_grace(&state, "link down on eth0");
        state.lock().unwrap().network_link_down_at = Some(Instant::now() - Duration::from_secs(61));

        let fired =
            check_network_violation_with(|_| net(&[("eth0", "down")]), &config, &baselines, &state);

        assert!(fired.is_some_and(|d| d.starts_with("NETWORK VIOLATION")));
        assert!(state.lock().unwrap().network_grace.is_none());
    }

    /// And the lid: reopening cancels, monitor never leaves a countdown, and
    /// an expired grace fires.
    #[test]
    fn test_lid_grace_clears_on_reopen_and_on_monitor_and_fires_when_it_runs_out() {
        let mut cfg = config::Config::default();
        cfg.general.watch_lid = true;
        cfg.lid.policy = LidPolicy::Kill;
        cfg.lid.grace_secs = 60;
        let (config, baselines, state) = grace_fixture(cfg, |bl| bl.lid = Some(LidState::Open));

        let fired = check_lid_violation_with(|| LidState::Closed, &config, &baselines, &state);
        assert_eq!(fired, None, "the grace period holds the kill back");
        assert!(state.lock().unwrap().lid_grace.is_some());

        let fired = check_lid_violation_with(|| LidState::Open, &config, &baselines, &state);
        assert_eq!(fired, None);
        let st = state.lock().unwrap();
        assert!(st.lid_grace.is_none(), "reopening clears the countdown");
        assert!(st.lid_close_at.is_none());
        drop(st);

        config.write().unwrap().lid.policy = LidPolicy::Monitor;
        running_grace(&state, "lid closed");
        let fired = check_lid_violation_with(|| LidState::Closed, &config, &baselines, &state);
        assert_eq!(fired, None);
        assert!(
            state.lock().unwrap().lid_grace.is_none(),
            "monitor must clear the countdown it will never act on"
        );

        config.write().unwrap().lid.policy = LidPolicy::Kill;
        baselines.write().unwrap().lid = Some(LidState::Open);
        running_grace(&state, "lid closed");
        state.lock().unwrap().lid_close_at = Some(Instant::now() - Duration::from_secs(61));
        let fired = check_lid_violation_with(|| LidState::Closed, &config, &baselines, &state);
        assert!(fired.is_some_and(|d| d.starts_with("LID VIOLATION")));
        assert!(state.lock().unwrap().lid_grace.is_none());
    }

    #[test]
    fn test_reload_baselines_newly_enabled_bus() {
        let cfg = display_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        let mut bl = empty_baselines();

        capture_baselines(&cfg, &mut bl, &state, true, Instant::now());

        assert!(
            bl.display.is_some(),
            "a bus enabled by reload must get a baseline, or its checker stays short-circuited"
        );
        assert!(bl.usb.is_none(), "a disabled bus must stay unbaselined");
    }

    #[test]
    fn test_reload_keeps_existing_baseline() {
        let cfg = display_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        let mut bl = empty_baselines();
        bl.display = Some(sentinel_display_baseline());

        capture_baselines(&cfg, &mut bl, &state, true, Instant::now());

        assert_eq!(
            bl.display,
            Some(sentinel_display_baseline()),
            "reload must not re-baseline a bus that was already armed"
        );
    }

    #[test]
    fn test_reload_clears_a_disabled_bus_then_recaptures_it() {
        let mut cfg = display_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        let mut bl = empty_baselines();
        bl.display = Some(sentinel_display_baseline());

        cfg.general.watch_display = false;
        capture_baselines(&cfg, &mut bl, &state, true, Instant::now());
        assert_eq!(
            bl.display, None,
            "a bus switched off by reload must lose its baseline"
        );

        cfg.general.watch_display = true;
        capture_baselines(&cfg, &mut bl, &state, true, Instant::now());
        assert!(
            bl.display.is_some(),
            "switching a bus back on must capture a baseline"
        );
        assert_ne!(
            bl.display,
            Some(sentinel_display_baseline()),
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

    /// The absent-hardware and present-but-unreadable branches are chosen by
    /// `bus_root_exists`, so it has to actually stat the path. Degraded to a
    /// constant it fails silently in one direction or the other: always true
    /// puts every machine without a Thunderbolt controller back to warning
    /// about hardware it never had, always false sends a present bus that will
    /// not enumerate back to being silently unmonitored. Asserting which log
    /// line fires would need a capturing logger, which is not worth a
    /// dependency for two lines.
    ///
    /// Linux only: the non-Linux arm answers true by design.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_bus_root_exists_distinguishes_absent_from_present() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("no-such-bus");

        assert!(
            bus_root_exists(dir.path().to_str().unwrap()),
            "an existing bus root must read as present, or absent hardware swallows a real failure"
        );
        assert!(
            !bus_root_exists(absent.to_str().unwrap()),
            "a missing bus root must read as absent, or every machine without the hardware warns"
        );
    }

    /// No real sysfs enumeration produces this connector name, so it is safe
    /// to use as a sentinel that a real capture must never reproduce.
    fn sentinel_display_baseline() -> display::DisplaySnapshot {
        display::DisplaySnapshot::from_connectors(vec![display::DisplayConnector {
            connector: "sentinel-not-a-real-connector".to_string(),
            connected: true,
            edid: None,
        }])
    }

    #[test]
    fn test_rearm_replaces_existing_baseline() {
        let cfg = display_only_config();
        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        let mut bl = empty_baselines();
        bl.display = Some(sentinel_display_baseline());

        capture_baselines(&cfg, &mut bl, &state, false, Instant::now());

        assert_ne!(bl.display, Some(sentinel_display_baseline()));
    }

    /// `detect_changes` plus the `DisplayChange` formatter for a monitor
    /// swapped on the same connector. The old topology hash saw the same
    /// (connector, status) pair and reported nothing, which is the hole this
    /// closes. This test does not call `check_display_violation`; it only
    /// pins the snapshot diff and message formatting. See the enforce and
    /// monitor mode tests below for coverage of the function itself.
    #[test]
    fn test_snapshot_swap_renders_a_violation_message() {
        use plugkill_core::display::{DisplayConnector, DisplaySnapshot};
        use plugkill_core::edid::EdidId;

        let mk = |serial: u32| {
            DisplaySnapshot::from_connectors(vec![DisplayConnector {
                connector: "DP-6".to_string(),
                connected: true,
                edid: Some(EdidId {
                    mfg: "SAM".to_string(),
                    product: 0x772d,
                    serial: Some(serial),
                    name: "Odyssey G93SD".to_string(),
                }),
            }])
        };

        let change = mk(2).detect_changes(&mk(1)).expect("swap must be a change");
        let msg = format!("DISPLAY VIOLATION: {change}");
        assert!(
            msg.contains("DP-6"),
            "message must name the connector: {msg}"
        );
        assert!(
            msg.contains("SAM:772d:2"),
            "message must name the monitor: {msg}"
        );
    }

    /// `check_display_violation` end to end in enforce (Kill) mode. The
    /// sentinel baseline cannot equal any real capture from this host's
    /// sysfs, so a difference and therefore a violation is guaranteed
    /// whether or not this machine has a display attached. Set the policy
    /// explicitly since `display_only_config` leaves the config default,
    /// which is Monitor, not Kill.
    #[test]
    fn test_check_display_violation_enforce_mode_returns_violation() {
        let mut cfg = display_only_config();
        cfg.display.policy = DisplayPolicy::Kill;
        let config_arc = Arc::new(RwLock::new(cfg));
        let mut bl = empty_baselines();
        bl.display = Some(sentinel_display_baseline());
        let baselines = Arc::new(RwLock::new(bl));

        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        let result = check_display_violation(&config_arc, &baselines, &state, Instant::now());

        let v = result.expect("sentinel baseline must never match a real capture");
        assert_eq!(
            v.bus, "display",
            "a display change must be filed under display"
        );
        assert!(
            v.description.starts_with("DISPLAY VIOLATION:"),
            "message must be tagged as a violation: {}",
            v.description
        );
    }

    /// `check_display_violation` end to end in monitor mode, the only branch
    /// that takes `baselines.write()` after `baselines.read()`. If the read
    /// guard from `.display.clone()?` were still held at that point, this
    /// test would not fail, it would HANG forever on the write lock. A hang
    /// on this test means the self-deadlock the `.clone()` fixes is back,
    /// which would silently stop the poll loop of a kill-switch daemon.
    #[test]
    fn test_check_display_violation_monitor_mode_rearms_without_deadlock() {
        let mut cfg = display_only_config();
        cfg.display.policy = DisplayPolicy::Monitor;
        let config_arc = Arc::new(RwLock::new(cfg));
        let mut bl = empty_baselines();
        bl.display = Some(sentinel_display_baseline());
        let baselines = Arc::new(RwLock::new(bl));

        let state = Arc::new(Mutex::new(DaemonState::new_for_test(DaemonMode::Enforce)));
        let result = check_display_violation(&config_arc, &baselines, &state, Instant::now());

        assert!(
            result.is_none(),
            "monitor mode must never report a violation"
        );
        assert_ne!(
            baselines.read().unwrap().display,
            Some(sentinel_display_baseline()),
            "monitor mode must rearm the baseline to the fresh capture"
        );
    }
}
