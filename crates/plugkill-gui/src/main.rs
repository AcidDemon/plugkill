mod tray;

use log::{error, info};
use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    let socket_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(plugkill_core::ipc::DEFAULT_SOCKET_PATH));

    info!("starting plugkill-gui (socket: {})", socket_path.display());

    if let Err(e) = tray::run(socket_path) {
        error!("fatal: {e}");
        std::process::exit(1);
    }
}
