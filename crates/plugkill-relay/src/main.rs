mod config;
mod crypto;
mod listener;
mod protocol;
mod resolve;
mod sender;
mod trigger;
mod watcher;

use base64::prelude::*;
use clap::Parser;
use log::{error, info};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const BANNER: &str = concat!(
    "plugkill-relay ",
    env!("CARGO_PKG_VERSION"),
    " — kill signal relay mesh for plugkill"
);

#[derive(Parser, Debug)]
#[command(name = "plugkill-relay", version, about, before_help = BANNER)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/plugkill-relay/config.toml")]
    config: PathBuf,

    /// Generate an ed25519 keypair and print to stdout
    #[arg(long)]
    generate_keys: bool,

    /// Print the public key for a given private key file
    #[arg(long, value_name = "PATH")]
    show_pubkey: Option<PathBuf>,

    /// Send a KILL signal to all configured peers and exit
    #[arg(long, value_name = "REASON")]
    trigger: Option<String>,

    /// Dry-run mode: log actions without triggering kills
    #[arg(long)]
    dry_run: bool,

    /// Print the default configuration and exit
    #[arg(long)]
    default_config: bool,
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

    if cli.generate_keys {
        let (privkey, pubkey) = crypto::generate_keypair();
        println!("Private key (store in SOPS, deploy to private_key_file):");
        println!("{}", BASE64_STANDARD.encode(privkey));
        println!();
        println!("Public key (add to peers config):");
        println!("{}", BASE64_STANDARD.encode(pubkey));
        return;
    }

    if let Some(path) = &cli.show_pubkey {
        match config::load_private_key(path) {
            Ok(privkey) => {
                let signing_key = ed25519_dalek::SigningKey::from_bytes(&privkey);
                let pubkey = signing_key.verifying_key().to_bytes();
                println!("{}", BASE64_STANDARD.encode(pubkey));
            }
            Err(e) => {
                error!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Load config (needed for --trigger and daemon mode)
    let cfg = match config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to load config: {e}");
            std::process::exit(1);
        }
    };

    let key_path = cfg.general.private_key_file.as_ref().unwrap();
    let private_key = match config::load_private_key(key_path) {
        Ok(k) => k,
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    };
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&private_key);
    let our_pubkey = signing_key.verifying_key().to_bytes();

    // Handle --trigger (send-only, no daemon)
    if let Some(reason) = &cli.trigger {
        info!("sending KILL to all peers: reason={}", reason);
        let result = sender::fan_out(&cfg, &private_key, &our_pubkey, reason, None);
        for name in &result.acked {
            info!("  {} — ACKed", name);
        }
        for name in &result.timed_out {
            error!("  {} — timed out", name);
        }
        if result.timed_out.is_empty() {
            info!("all peers acknowledged");
        } else {
            error!(
                "{}/{} peers timed out",
                result.timed_out.len(),
                result.acked.len() + result.timed_out.len()
            );
            std::process::exit(1);
        }
        return;
    }

    // --- Daemon mode ---
    let dry_run = cli.dry_run;
    if dry_run {
        info!("DRY-RUN MODE — will log but not trigger kills");
    }
    info!("starting plugkill-relay daemon");
    info!("  peers: {}", cfg.peers.len());
    info!("  listen port: {}", cfg.general.listen_port);
    info!(
        "  plugkill socket: {}",
        cfg.general.plugkill_socket.display()
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_signal = shutdown.clone();

    signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown_signal.clone())
        .expect("failed to register SIGINT handler");
    signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown_signal)
        .expect("failed to register SIGTERM handler");

    // Start UDP listener in background thread
    let listener_cfg = cfg.clone();
    let listener_privkey = private_key;
    let listener_pubkey = our_pubkey;
    let listener_shutdown = shutdown.clone();
    let listener_handle = thread::spawn(move || {
        listener::run(
            &listener_cfg,
            &listener_privkey,
            &listener_pubkey,
            listener_shutdown,
            dry_run,
        );
    });

    // Main thread: poll plugkill for violations
    let mut watcher = watcher::Watcher::new(&cfg.general.plugkill_socket);
    let poll_interval = Duration::from_millis(cfg.general.poll_interval_ms);

    while !shutdown.load(Ordering::Relaxed) {
        if let Some(violations) = watcher.poll() {
            info!(
                "violation detected (count: {}) — fanning out KILL to peers",
                violations
            );
            let result = sender::fan_out(&cfg, &private_key, &our_pubkey, "local_violation", None);
            for name in &result.acked {
                info!("  {} — ACKed", name);
            }
            for name in &result.timed_out {
                error!("  {} — timed out", name);
            }
        }
        thread::sleep(poll_interval);
    }

    info!("shutting down");
    let _ = listener_handle.join();
}
