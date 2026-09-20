mod commands;
mod dashboard;
mod icons;
mod menu;
mod poll;
mod settings;
mod status;
mod tray;

use dashboard::Dashboard;
use gtk::prelude::*;
use gtk::{gio, glib};
use ksni::blocking::TrayMethods;
use log::{error, info, warn};
use settings::Settings;
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tray::{PlugkillTray, TrayEvent};

const APP_ID: &str = "io.github.aciddemon.plugkill";

/// Write the icon theme and return the path to hand the panel, or an empty
/// string when there is nowhere safe to write it.
fn install_icons() -> String {
    let Some(root) = icons::default_root() else {
        warn!("XDG_RUNTIME_DIR is not set, the panel will not find the tray icons");
        return String::new();
    };
    match icons::install(&root) {
        Ok(()) => root.display().to_string(),
        Err(e) => {
            warn!("cannot write tray icons to {}: {e}", root.display());
            String::new()
        }
    }
}

/// Stamp a notice with the moment it went up, so the poller can drop it once
/// it is stale rather than leaving it next to a state that contradicts it.
fn stamped(notice: Option<String>) -> Option<(String, std::time::Instant)> {
    notice.map(|text| (text, std::time::Instant::now()))
}

/// Build the dashboard, tray, poller and command sender. False when a part
/// the program cannot run without failed to start.
fn start(app: &gtk::Application, socket_path: PathBuf) -> bool {
    // The tray advertises icon *names* resolved out of this theme, so with no
    // theme to point at the panel indicator is simply blank, kill countdown
    // included. Better to fail loudly than to run invisibly.
    let icon_theme_path = install_icons();
    if icon_theme_path.is_empty() {
        error!("fatal: cannot install tray icons, the panel would show no indicator");
        return false;
    }
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(&icon_theme_path);
        let css = gtk::CssProvider::new();
        css.load_from_data(include_str!("dashboard/style.css"));
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let (commands_tx, commands_rx) = async_channel::unbounded();
    let (events_tx, events_rx) = async_channel::unbounded();
    let (updates_tx, updates_rx) = async_channel::unbounded();
    let (notices_tx, notices_rx) = async_channel::unbounded();
    let dashboard_open = Arc::new(AtomicBool::new(false));
    let settings_open = Arc::new(AtomicBool::new(false));
    // The settings window first: the dashboard footer opens it.
    let settings = Settings::new(
        app,
        commands_tx.clone(),
        socket_path.clone(),
        settings_open.clone(),
    );
    let dashboard = Dashboard::new(
        app,
        commands_tx.clone(),
        dashboard_open.clone(),
        settings.clone(),
    );

    let handle = match PlugkillTray::new(icon_theme_path, events_tx).spawn() {
        Ok(handle) => handle,
        Err(e) => {
            error!("fatal: failed to start tray: {e}");
            return false;
        }
    };

    // Socket I/O blocks, so commands go out on one worker thread, one at a
    // time, in the order they were given. A disarm the daemon puts behind a
    // polkit prompt sits here until the person answers it.
    let command_socket = socket_path.clone();
    let command_tray = handle.clone();
    let poll_notices = notices_tx.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("command-sender".into())
        .spawn(move || {
            while let Ok(command) = commands_rx.recv_blocking() {
                // A gated command can sit on a password prompt for two minutes
                // and everything clicked after it waits in this queue. Say so
                // before the send blocks; the result overwrites it.
                if command.is_gated() {
                    let waiting = Some(format!("{}: waiting for the daemon", command.name()));
                    let _ = command_tray.update(|t| t.notice = stamped(waiting.clone()));
                    let _ = notices_tx.try_send(waiting);
                }
                // The last command's result, so a refusal clears itself the
                // moment anything works.
                let notice = match commands::send(&command_socket, command) {
                    Ok(()) => None,
                    Err(e) => {
                        warn!("daemon command {command:?} failed: {e}");
                        Some(commands::notice(command, &e))
                    }
                };
                let _ = command_tray.update(|t| t.notice = stamped(notice.clone()));
                let _ = notices_tx.try_send(notice);
            }
        })
    {
        error!("fatal: failed to start command sender: {e}");
        return false;
    }

    if let Err(e) = std::thread::Builder::new()
        .name("status-poller".into())
        .spawn(move || {
            poll::run(
                handle,
                socket_path,
                Some(updates_tx),
                poll_notices,
                dashboard_open,
                settings_open,
            )
        })
    {
        error!("fatal: failed to start poller: {e}");
        return false;
    }

    {
        let dashboard = dashboard.clone();
        let settings = settings.clone();
        let app = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = events_rx.recv().await {
                match event {
                    TrayEvent::Activate { x, y } => dashboard.toggle(x, y),
                    TrayEvent::OpenDashboard => dashboard.open(),
                    TrayEvent::OpenSettings => settings.open(),
                    TrayEvent::Command(command) => {
                        let _ = commands_tx.try_send(command);
                    }
                    TrayEvent::Quit => app.quit(),
                }
            }
        });
    }
    {
        let dashboard = dashboard.clone();
        let settings = settings.clone();
        glib::spawn_future_local(async move {
            // One receiver, fanned out: async_channel is MPMC, so a second
            // recv would take notices away from the dashboard.
            while let Ok(notice) = notices_rx.recv().await {
                dashboard.set_notice(notice.as_deref());
                settings.set_notice(notice.as_deref());
            }
        });
    }
    glib::spawn_future_local(async move {
        while let Ok(update) = updates_rx.recv().await {
            dashboard.update(&update);
            settings.update(&update);
        }
    });
    true
}

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    let socket_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(plugkill_core::ipc::DEFAULT_SOCKET_PATH));
    info!("starting plugkill-gui (socket: {})", socket_path.display());

    // NON_UNIQUE: every run is its own instance talking to its own socket.
    // A unique app would hand a second run to the first one, whose buttons
    // drive whatever socket that first run was given.
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let failed = Rc::new(Cell::new(false));
    {
        let failed = failed.clone();
        app.connect_activate(move |app| {
            if start(app, socket_path.clone()) {
                // ponytail: the application lives as long as the tray; Quit tray
                // ends it through app.quit(), so the hold is never released.
                std::mem::forget(app.hold());
            } else {
                failed.set(true);
                app.quit();
            }
        });
    }
    // The socket path is ours; GTK must not try to parse it.
    let code = app.run_with_args::<&str>(&[]);
    if failed.get() {
        glib::ExitCode::FAILURE
    } else {
        code
    }
}
