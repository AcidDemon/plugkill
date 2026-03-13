mod config;
mod error;
mod kill;
mod power;
mod sdcard;
mod socket;
mod state;
mod thunderbolt;
mod usb;

use crate::config::PowerPolicy;
use crate::power::PowerState;
use crate::sdcard::{SdCardDeviceId, SdCardSnapshot};
use crate::state::{Baselines, DaemonMode, DaemonState, DeviceNames};
use crate::thunderbolt::{ThunderboltDeviceId, ThunderboltSnapshot};
use crate::usb::{DeviceSnapshot, UsbDeviceId};
use clap::Parser;
use log::{error, info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// Hardware kill-switch daemon — shuts down the system when device changes are detected.
#[derive(Parser, Debug)]
#[command(name = "plugkill", version, about)]
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

    /// Path to the control socket
    #[arg(long, default_value = socket::DEFAULT_SOCKET_PATH)]
    socket: PathBuf,
}

fn main() {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    let cli = Cli::parse();

    // Handle --default-config
    if cli.default_config {
        print!("{}", config::default_config_toml());
        return;
    }

    // Handle --generate-whitelist (no root needed)
    if cli.generate_whitelist {
        match usb::enumerate_devices_detailed() {
            Ok(devices) => print!("{}", usb::generate_whitelist_toml(&devices)),
            Err(e) => {
                error!("failed to enumerate USB devices: {e}");
                std::process::exit(1);
            }
        }
        if let Ok(tb_devices) = thunderbolt::enumerate_thunderbolt_devices_detailed() {
            if !tb_devices.is_empty() {
                print!(
                    "{}",
                    thunderbolt::generate_thunderbolt_whitelist_toml(&tb_devices)
                );
            }
        }
        if let Ok(sd_devices) = sdcard::enumerate_sdcard_devices_detailed() {
            if !sd_devices.is_empty() {
                print!("{}", sdcard::generate_sdcard_whitelist_toml(&sd_devices));
            }
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
                let whitelist_map = loaded_wl.as_ref().map(|wl| {
                    let mut map: HashMap<(String, String), u32> = HashMap::new();
                    for entry in &wl.usb.devices {
                        *map.entry((entry.vendor_id.clone(), entry.product_id.clone()))
                            .or_insert(0) += entry.count;
                    }
                    map
                });
                usb::print_device_list(&devices, whitelist_map.as_ref());
            }
            Err(e) => {
                error!("failed to enumerate USB devices: {e}");
                std::process::exit(1);
            }
        }

        if let Ok(tb_devices) = thunderbolt::enumerate_thunderbolt_devices_detailed() {
            if !tb_devices.is_empty() {
                let tb_whitelist_map = loaded_wl.as_ref().map(|wl| {
                    let mut map: HashMap<String, ()> = HashMap::new();
                    for entry in &wl.thunderbolt.devices {
                        map.insert(entry.unique_id.clone(), ());
                    }
                    map
                });
                thunderbolt::print_thunderbolt_device_list(&tb_devices, tb_whitelist_map.as_ref());
            }
        }

        if let Ok(sd_devices) = sdcard::enumerate_sdcard_devices_detailed() {
            if !sd_devices.is_empty() {
                let sd_whitelist_map = loaded_wl.as_ref().map(|wl| {
                    let mut map: HashMap<String, ()> = HashMap::new();
                    for entry in &wl.sdcard.devices {
                        map.insert(entry.serial.clone(), ());
                    }
                    map
                });
                sdcard::print_sdcard_device_list(&sd_devices, sd_whitelist_map.as_ref());
            }
        }
        return;
    }

    // Handle client commands (connect to running daemon via socket)
    if let Some(timeout) = cli.disarm {
        let req = serde_json::json!({"command": "disarm", "timeout_secs": timeout});
        if let Err(e) = socket::send_command(&cli.socket, &req) {
            error!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.arm {
        let req = serde_json::json!({"command": "arm"});
        if let Err(e) = socket::send_command(&cli.socket, &req) {
            error!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.status {
        let req = serde_json::json!({"command": "status"});
        if let Err(e) = socket::send_command(&cli.socket, &req) {
            error!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.learn {
        let req = serde_json::json!({"command": "learn"});
        if let Err(e) = socket::send_command(&cli.socket, &req) {
            error!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.enforce {
        let req = serde_json::json!({"command": "enforce"});
        if let Err(e) = socket::send_command(&cli.socket, &req) {
            error!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if cli.reload {
        let req = serde_json::json!({"command": "reload"});
        if let Err(e) = socket::send_command(&cli.socket, &req) {
            error!("{e}");
            std::process::exit(1);
        }
        return;
    }

    // --- Daemon mode ---

    // Check root privilege
    if !nix::unistd::geteuid().is_root() {
        error!("plugkill must run as root (need device access and shutdown capability)");
        std::process::exit(1);
    }

    // Load configuration
    let mut cfg = match config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to load config: {e}");
            std::process::exit(1);
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

    if cfg.general.dry_run {
        warn!("running in DRY RUN mode — no destructive actions will be taken");
    }

    // Log active buses
    let active_buses: Vec<&str> = [
        cfg.general.watch_usb.then_some("USB"),
        cfg.general.watch_thunderbolt.then_some("Thunderbolt"),
        cfg.general.watch_sdcard.then_some("SD card"),
        cfg.general.watch_power.then_some("power supply"),
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

    // Initialize shared state
    let initial_mode = if cli.learn_mode {
        info!("starting in LEARNING mode — violations will be logged but not acted upon");
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
        for (id, count) in usb_whitelist.devices() {
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
        for id in tb_whitelist.devices().keys() {
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
        for id in sd_whitelist.devices().keys() {
            info!("  {id}");
        }
    }

    let power_baseline = if cfg.general.watch_power {
        let state = power::read_power_state();
        info!("power baseline: {state} (policy: {:?})", cfg.power.policy);
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

    // Shared structures
    let baselines = Arc::new(RwLock::new(Baselines {
        usb: usb_baseline,
        thunderbolt: tb_baseline,
        sdcard: sd_baseline,
        power: power_baseline,
        names: device_names,
    }));
    let config_arc = Arc::new(RwLock::new(cfg));

    // Start socket listener
    let socket_path = cli.socket.clone();
    if let Err(e) = socket::start_socket_listener(
        socket_path.clone(),
        Arc::clone(&daemon_state),
        Arc::clone(&config_arc),
        Arc::clone(&baselines),
    ) {
        warn!("failed to start control socket: {e} (continuing without socket)");
    }

    // Track whether we need to re-capture baselines after re-arm
    let mut needs_rebaseline = false;

    let sleep_duration = {
        let cfg = config_arc.read().unwrap();
        Duration::from_millis(cfg.general.sleep_ms)
    };
    info!(
        "patrolling every {}ms (dry_run={}, mode={})",
        sleep_duration.as_millis(),
        config_arc.read().unwrap().general.dry_run,
        initial_mode,
    );

    // Main polling loop
    while running.load(Ordering::Relaxed) {
        // Check disarm timeout expiry
        {
            let mut st = daemon_state.lock().unwrap();
            if !st.armed && st.is_disarm_expired() {
                info!("disarm timeout expired, re-arming");
                st.armed = true;
                st.disarm_until = None;
                needs_rebaseline = true;
            }
        }

        // Handle re-baseline after re-arm (from timeout or socket arm command)
        if needs_rebaseline {
            let cfg = config_arc.read().unwrap();
            let mut bl = baselines.write().unwrap();
            info!("re-capturing baselines after re-arm");

            if cfg.general.watch_usb {
                let (snapshot, names) = capture_usb_baseline();
                bl.usb = Some(snapshot);
                bl.names.usb = names;
            }
            if cfg.general.watch_thunderbolt {
                let (snapshot, names) = capture_thunderbolt_baseline(&cfg);
                bl.thunderbolt = snapshot;
                bl.names.thunderbolt = names;
            }
            if cfg.general.watch_sdcard {
                let (snapshot, names) = capture_sdcard_baseline(&cfg);
                bl.sdcard = snapshot;
                bl.names.sdcard = names;
            }
            if cfg.general.watch_power {
                let state = power::read_power_state();
                info!("power re-baseline: {state}");
                bl.power = Some(state);
                // Reset power trigger state on re-baseline
                let mut st = daemon_state.lock().unwrap();
                st.power_unplug_at = None;
                st.power_trigger_once_fired = false;
            }
            needs_rebaseline = false;
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

                        *config_arc.write().unwrap() = new_cfg;
                        info!("configuration reloaded successfully");
                    }
                    Err(e) => {
                        error!("config reload failed: {e}");
                    }
                }
            }
        }

        // Skip checks if disarmed
        let is_armed = daemon_state.lock().unwrap().armed;
        if !is_armed {
            thread::sleep(sleep_duration);
            continue;
        }

        // Detect violations while holding read locks, collect description if any
        let violation = detect_violations(&config_arc, &baselines)
            .or_else(|| check_power_violation(&config_arc, &baselines, &daemon_state));

        // Process violation outside of read locks
        if let Some(description) = violation {
            if handle_violation(&daemon_state, &description, &config_arc.read().unwrap()) {
                if let Err(e) =
                    kill::execute_kill_sequence(&config_arc.read().unwrap(), &description)
                {
                    error!("kill sequence error: {e}");
                    if !config_arc.read().unwrap().general.dry_run {
                        std::process::exit(1);
                    }
                }
                if config_arc.read().unwrap().general.dry_run {
                    warn!("dry run — continuing patrol");
                }
            }
        }

        daemon_state.lock().unwrap().last_poll = Some(std::time::Instant::now());
        thread::sleep(sleep_duration);
    }

    info!("received exit signal, shutting down gracefully");
    socket::cleanup_socket(&cli.socket);
}

/// Check all active buses for violations. Returns the first violation description found, or None.
fn detect_violations(
    config_arc: &Arc<RwLock<config::Config>>,
    baselines: &Arc<RwLock<Baselines>>,
) -> Option<String> {
    let cfg = config_arc.read().unwrap();
    let bl = baselines.read().unwrap();

    // USB check
    if cfg.general.watch_usb {
        if let Some(ref baseline) = bl.usb {
            match usb::enumerate_devices() {
                Ok(current) => {
                    if let Some(change) =
                        current.detect_changes(baseline, &build_usb_whitelist(&cfg))
                    {
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
                    return Some(format!("USB enumeration failure (possible tampering): {e}"));
                }
            }
        }
    }

    // Thunderbolt check
    if cfg.general.watch_thunderbolt {
        if let Some(ref tb_base) = bl.thunderbolt {
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
                        "Thunderbolt enumeration failure (possible tampering): {e}"
                    ));
                }
            }
        }
    }

    // SD card check
    if cfg.general.watch_sdcard {
        if let Some(ref sd_base) = bl.sdcard {
            match sdcard::enumerate_sdcard_devices() {
                Ok(current) => {
                    if let Some(change) =
                        current.detect_changes(sd_base, &build_sdcard_whitelist(&cfg))
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
                        "SD card enumeration failure (possible tampering): {e}"
                    ));
                }
            }
        }
    }

    // Power check is handled separately in check_power_violation() because
    // it needs mutable access to DaemonState for grace period tracking.
    None
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
            Some(true) => {} // locked — proceed with violation check
            Some(false) => {
                // Not locked — user is present, don't trigger
                // But track the unplug time in case session locks later
                if st.power_unplug_at.is_none() {
                    st.power_unplug_at = Some(Instant::now());
                }
                return None;
            }
            None => {
                // Can't determine lock state — proceed without this check
                warn!("cannot determine session lock state, proceeding with power check");
            }
        }
    }

    // Grace period handling
    if cfg.power.grace_secs > 0 {
        let now = Instant::now();
        match st.power_unplug_at {
            None => {
                // First detection of battery — start grace period
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
                // Grace period expired — fall through to violation
            }
        }
    } else if st.power_unplug_at.is_none() {
        // No grace period and first detection — record unplug time for logging
        st.power_unplug_at = Some(Instant::now());
    }

    // Mark trigger-once as fired
    if cfg.power.policy == PowerPolicy::TriggerOnce {
        st.power_trigger_once_fired = true;
    }

    let policy_label = match cfg.power.policy {
        PowerPolicy::TriggerOnce => "trigger-once",
        PowerPolicy::AcRequired => "ac-required",
        PowerPolicy::Monitor => unreachable!(),
    };
    Some(format!(
        "POWER VIOLATION: AC power removed (policy: {policy_label})"
    ))
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
            warn!("LEARN MODE — {description}");
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
    for (id, count) in snapshot.devices() {
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
            for id in snapshot.devices().keys() {
                let name = names
                    .get(&id.unique_id)
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                info!("  {id}{name}");
            }
            (Some(snapshot), names)
        }
        Err(_) => {
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
            for id in snapshot.devices().keys() {
                let name = names
                    .get(&id.serial)
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                info!("  {id}{name}");
            }
            (Some(snapshot), names)
        }
        Err(_) => {
            if !cfg.sdcard_whitelist.devices.is_empty() {
                warn!("sdcard_whitelist configured but no MMC bus found");
            }
            (None, HashMap::new())
        }
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
    let mut map = HashMap::new();
    for entry in &cfg.thunderbolt_whitelist.devices {
        let id = ThunderboltDeviceId {
            unique_id: entry.unique_id.clone(),
        };
        map.insert(id, 1);
    }
    ThunderboltSnapshot::from_map(map)
}

/// Build an SdCardSnapshot from the SD card whitelist config entries.
fn build_sdcard_whitelist(cfg: &config::Config) -> SdCardSnapshot {
    let mut map = HashMap::new();
    for entry in &cfg.sdcard_whitelist.devices {
        let id = SdCardDeviceId {
            serial: entry.serial.clone(),
        };
        map.insert(id, 1);
    }
    SdCardSnapshot::from_map(map)
}
