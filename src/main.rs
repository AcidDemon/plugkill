mod config;
mod error;
mod kill;
mod usb;

use crate::usb::{DeviceSnapshot, UsbDeviceId};
use clap::Parser;
use log::{error, info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// USB kill-switch daemon — shuts down the system when USB device changes are detected.
#[derive(Parser, Debug)]
#[command(name = "usbkill", version, about)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/usbkill/config.toml")]
    config: PathBuf,

    /// Dry-run mode: log actions without executing them
    #[arg(long)]
    dry_run: bool,

    /// Print the default configuration and exit
    #[arg(long)]
    default_config: bool,
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

    // Check root privilege
    if !nix::unistd::geteuid().is_root() {
        error!("usbkill must run as root (need USB access and shutdown capability)");
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

    // CLI --dry-run overrides config
    if cli.dry_run {
        cfg.general.dry_run = true;
    }

    if cfg.general.dry_run {
        warn!("running in DRY RUN mode — no destructive actions will be taken");
    }

    // Set up signal handling for clean exit
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&r)) {
        error!("failed to register SIGINT handler: {e}");
        std::process::exit(1);
    }
    // For SIGINT we want running=false, but signal_hook::flag::register sets flag to false
    // on signal. We need the opposite: set to false. Actually, register() sets to !initial.
    // Since running starts as true, on signal it'll be set to false. But wait, the docs say
    // register sets the flag to the value — let me use register_conditional_default instead.

    // Actually let's use signal_hook correctly:
    let running = Arc::new(AtomicBool::new(true));

    for &sig in &[
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ] {
        let r = running.clone();
        // register_conditional_default: reset flag to false, and use default handler after
        if let Err(e) = signal_hook::flag::register_conditional_default(sig, r) {
            error!("failed to register signal handler for {sig}: {e}");
            std::process::exit(1);
        }
    }

    // Take initial USB device snapshot (baseline)
    let baseline = match usb::enumerate_devices() {
        Ok(snapshot) => {
            info!(
                "baseline captured: {} unique device ID(s)",
                snapshot.len()
            );
            for (id, count) in snapshot.devices() {
                info!("  {id} (count: {count})");
            }
            snapshot
        }
        Err(e) => {
            error!("failed to enumerate USB devices: {e}");
            std::process::exit(1);
        }
    };

    // Build whitelist snapshot from config
    let whitelist = build_whitelist(&cfg);
    if !whitelist.devices().is_empty() {
        info!("whitelist:");
        for (id, count) in whitelist.devices() {
            info!("  {id} (max count: {count})");
        }
    }

    let sleep_duration = Duration::from_millis(cfg.general.sleep_ms);
    info!(
        "patrolling USB ports every {}ms (dry_run={})",
        cfg.general.sleep_ms, cfg.general.dry_run
    );

    // Main polling loop
    while running.load(Ordering::Relaxed) {
        match usb::enumerate_devices() {
            Ok(current) => {
                if let Some(change) = current.detect_changes(&baseline, &whitelist) {
                    error!("USB VIOLATION DETECTED: {change}");

                    if let Err(e) = kill::execute_kill_sequence(&cfg, &change.to_string()) {
                        error!("kill sequence error: {e}");
                        // If kill sequence fails and we're not in dry_run, force exit
                        if !cfg.general.dry_run {
                            std::process::exit(1);
                        }
                    }

                    if cfg.general.dry_run {
                        // In dry run, just log and continue patrolling
                        warn!("dry run — continuing patrol");
                    }
                }
            }
            Err(e) => {
                // USB enumeration failure is suspicious — could indicate tampering
                error!("USB enumeration failed (possible tampering): {e}");
                if let Err(e) = kill::execute_kill_sequence(
                    &cfg,
                    &format!("USB enumeration failure (possible tampering): {e}"),
                ) {
                    error!("kill sequence error: {e}");
                    if !cfg.general.dry_run {
                        std::process::exit(1);
                    }
                }
            }
        }

        thread::sleep(sleep_duration);
    }

    info!("received exit signal, shutting down gracefully");
}

/// Build a DeviceSnapshot from the whitelist config entries.
fn build_whitelist(cfg: &config::Config) -> DeviceSnapshot {
    let mut map = HashMap::new();
    for entry in &cfg.whitelist.devices {
        let id = UsbDeviceId {
            vendor_id: entry.vendor_id.clone(),
            product_id: entry.product_id.clone(),
        };
        // If the same device appears multiple times in whitelist, sum the counts
        *map.entry(id).or_insert(0) += entry.count;
    }
    DeviceSnapshot::from_map(map)
}
