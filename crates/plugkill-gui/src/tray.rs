use ksni::blocking::TrayMethods;
use ksni::menu::{StandardItem, SubMenu};
use ksni::{Category, Icon, MenuItem, Status, ToolTip};
use log::{info, warn};
use plugkill_core::ipc;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// How often we poll the daemon for status updates.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Disarm duration options shown in the submenu.
const DISARM_DURATIONS: &[(u64, &str)] = &[
    (60, "1 minute"),
    (300, "5 minutes"),
    (600, "10 minutes"),
    (1800, "30 minutes"),
    (3600, "1 hour"),
];

/// Icon size for generated pixmap icons.
const ICON_SIZE: i32 = 22;

/// Commands sent from menu callbacks to the action thread.
enum Action {
    Arm,
    Disarm(u64),
    LearnMode,
    EnforceMode,
    Reload,
    Quit,
}

/// Daemon status as understood by the tray.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonStatus {
    /// Connected: armed in enforce mode.
    Armed,
    /// Connected: disarmed with optional remaining seconds.
    Disarmed(Option<u64>),
    /// Connected: learning mode.
    Learning,
    /// Cannot reach daemon.
    Disconnected,
}

/// The tray state.
struct PlugkillTray {
    status: DaemonStatus,
    violations: u64,
    uptime_secs: Option<u64>,
    watching: Vec<String>,
    action_tx: mpsc::Sender<Action>,
}

/// Generate a solid circle icon in ARGB32 format.
/// The circle is drawn on a transparent background.
fn make_circle_icon(r: u8, g: u8, b: u8) -> Icon {
    let size = ICON_SIZE;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let center = size as f32 / 2.0;
    let radius = center - 1.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center + 0.5;
            let dy = y as f32 - center + 0.5;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist <= radius {
                // ARGB32 network byte order
                data.extend_from_slice(&[255, r, g, b]);
            } else if dist <= radius + 1.0 {
                // Anti-aliased edge
                let alpha = ((radius + 1.0 - dist) * 255.0) as u8;
                data.extend_from_slice(&[alpha, r, g, b]);
            } else {
                // Transparent
                data.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }

    Icon {
        width: size,
        height: size,
        data,
    }
}

impl ksni::Tray for PlugkillTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "plugkill".to_string()
    }

    fn category(&self) -> Category {
        Category::SystemServices
    }

    fn title(&self) -> String {
        match &self.status {
            DaemonStatus::Armed => "Plugkill: Armed".to_string(),
            DaemonStatus::Disarmed(Some(s)) => {
                format!("Plugkill: Disarmed ({})", format_duration(*s))
            }
            DaemonStatus::Disarmed(None) => "Plugkill: Disarmed".to_string(),
            DaemonStatus::Learning => "Plugkill: Learning".to_string(),
            DaemonStatus::Disconnected => "Plugkill: Disconnected".to_string(),
        }
    }

    fn status(&self) -> Status {
        match self.status {
            DaemonStatus::Disarmed(_) => Status::NeedsAttention,
            _ => Status::Active,
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let icon = match self.status {
            DaemonStatus::Armed => make_circle_icon(0x2e, 0xcc, 0x40), // green
            DaemonStatus::Learning => make_circle_icon(0xf5, 0xc2, 0x11), // yellow
            DaemonStatus::Disarmed(_) => make_circle_icon(0xe0, 0x4f, 0x5f), // red
            DaemonStatus::Disconnected => make_circle_icon(0x88, 0x88, 0x88), // grey
        };
        vec![icon]
    }

    fn tool_tip(&self) -> ToolTip {
        let title = self.title();
        let mut desc = String::new();

        if let Some(secs) = self.uptime_secs {
            desc.push_str(&format!("Uptime: {}\n", format_duration(secs)));
        }
        if !self.watching.is_empty() {
            desc.push_str(&format!("Watching: {}\n", self.watching.join(", ")));
        }
        if self.violations > 0 {
            desc.push_str(&format!("Violations logged: {}", self.violations));
        }

        ToolTip {
            title,
            description: desc.trim().to_string(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();
        let connected = self.status != DaemonStatus::Disconnected;

        // Status header
        let (status_label, status_icon) = match &self.status {
            DaemonStatus::Armed => ("Armed (enforce mode)".to_string(), "security-high"),
            DaemonStatus::Disarmed(Some(s)) => (
                format!("Disarmed (re-arms in {})", format_duration(*s)),
                "security-low",
            ),
            DaemonStatus::Disarmed(None) => ("Disarmed".to_string(), "security-low"),
            DaemonStatus::Learning => ("Armed (learning mode)".to_string(), "security-medium"),
            DaemonStatus::Disconnected => ("Daemon not running".to_string(), "network-offline"),
        };
        items.push(
            StandardItem {
                label: status_label,
                icon_name: status_icon.to_string(),
                enabled: false,
                ..Default::default()
            }
            .into(),
        );

        if self.violations > 0 {
            items.push(
                StandardItem {
                    label: format!("Violations: {}", self.violations),
                    icon_name: "dialog-warning".to_string(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);

        // Arm
        items.push(
            StandardItem {
                label: "Arm".to_string(),
                icon_name: "media-playback-start".to_string(),
                enabled: connected && !matches!(self.status, DaemonStatus::Armed),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.action_tx.send(Action::Arm);
                }),
                ..Default::default()
            }
            .into(),
        );

        // Disarm submenu
        let disarm_items: Vec<MenuItem<Self>> = DISARM_DURATIONS
            .iter()
            .map(|&(secs, label)| {
                StandardItem {
                    label: label.to_string(),
                    icon_name: "appointment-soon".to_string(),
                    activate: Box::new(move |tray: &mut Self| {
                        let _ = tray.action_tx.send(Action::Disarm(secs));
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        items.push(
            SubMenu {
                label: "Disarm".to_string(),
                icon_name: "media-playback-pause".to_string(),
                enabled: connected
                    && matches!(self.status, DaemonStatus::Armed | DaemonStatus::Learning),
                submenu: disarm_items,
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // Mode switching
        items.push(
            StandardItem {
                label: "Switch to Learning Mode".to_string(),
                icon_name: "dialog-information".to_string(),
                enabled: connected && !matches!(self.status, DaemonStatus::Learning),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.action_tx.send(Action::LearnMode);
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(
            StandardItem {
                label: "Switch to Enforce Mode".to_string(),
                icon_name: "dialog-error".to_string(),
                enabled: connected && matches!(self.status, DaemonStatus::Learning),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.action_tx.send(Action::EnforceMode);
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // Reload config
        items.push(
            StandardItem {
                label: "Reload Config".to_string(),
                icon_name: "view-refresh".to_string(),
                enabled: connected,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.action_tx.send(Action::Reload);
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // Quit
        items.push(
            StandardItem {
                label: "Quit".to_string(),
                icon_name: "application-exit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.action_tx.send(Action::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

/// Query daemon status and parse the response into tray state fields.
fn poll_status(socket_path: &Path) -> Option<(DaemonStatus, u64, Option<u64>, Vec<String>)> {
    let req = serde_json::json!({"command": "status"});
    let resp = ipc::send_request(socket_path, &req).ok()?;

    if !resp.get("ok")?.as_bool()? {
        return None;
    }

    let data = resp.get("data")?;

    let armed = data.get("armed")?.as_bool()?;
    let mode = data.get("mode")?.as_str()?;
    let violations = data
        .get("violations_logged")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let uptime = data.get("uptime_secs").and_then(|v| v.as_u64());
    let disarm_remaining = data.get("disarm_remaining_secs").and_then(|v| v.as_u64());

    let status = if !armed {
        DaemonStatus::Disarmed(disarm_remaining.filter(|&s| s > 0))
    } else if mode == "learn" {
        DaemonStatus::Learning
    } else {
        DaemonStatus::Armed
    };

    let mut watching = Vec::new();
    for (key, label) in [
        ("usb_watching", "USB"),
        ("thunderbolt_watching", "Thunderbolt"),
        ("sdcard_watching", "SD card"),
        ("power_watching", "Power"),
        ("network_watching", "Network"),
        ("lid_watching", "Lid"),
    ] {
        if data.get(key).and_then(|v| v.as_bool()) == Some(true) {
            watching.push(label.to_string());
        }
    }

    Some((status, violations, uptime, watching))
}

/// Send a command to the daemon (fire-and-forget for menu actions).
fn send_action(socket_path: &Path, request: serde_json::Value) {
    match ipc::send_request(socket_path, &request) {
        Ok(resp) => {
            if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                let err = resp
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                warn!("daemon command failed: {err}");
            }
        }
        Err(e) => warn!("failed to send command: {e}"),
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Run the system tray. Blocks until quit is requested.
pub fn run(socket_path: PathBuf) -> Result<(), String> {
    let (action_tx, action_rx) = mpsc::channel();

    let tray = PlugkillTray {
        status: DaemonStatus::Disconnected,
        violations: 0,
        uptime_secs: None,
        watching: Vec::new(),
        action_tx,
    };

    let handle = tray
        .spawn()
        .map_err(|e| format!("failed to start tray: {e}"))?;

    info!("tray icon active");

    // Status polling thread
    let poll_handle = handle.clone();
    let poll_socket = socket_path.clone();
    std::thread::Builder::new()
        .name("status-poller".into())
        .spawn(move || {
            loop {
                let new_state = poll_status(&poll_socket);

                poll_handle.update(|tray: &mut PlugkillTray| {
                    if let Some((status, violations, uptime, watching)) = new_state {
                        tray.status = status;
                        tray.violations = violations;
                        tray.uptime_secs = uptime;
                        tray.watching = watching;
                    } else {
                        tray.status = DaemonStatus::Disconnected;
                        tray.violations = 0;
                        tray.uptime_secs = None;
                        tray.watching.clear();
                    }
                });

                std::thread::sleep(POLL_INTERVAL);
            }
        })
        .map_err(|e| format!("failed to start poller: {e}"))?;

    // Action handler thread — processes menu commands
    let action_socket = socket_path;
    std::thread::Builder::new()
        .name("action-handler".into())
        .spawn(move || {
            while let Ok(action) = action_rx.recv() {
                match action {
                    Action::Arm => {
                        info!("menu: arm");
                        send_action(&action_socket, serde_json::json!({"command": "arm"}));
                    }
                    Action::Disarm(secs) => {
                        info!("menu: disarm for {secs}s");
                        send_action(
                            &action_socket,
                            serde_json::json!({"command": "disarm", "timeout_secs": secs}),
                        );
                    }
                    Action::LearnMode => {
                        info!("menu: learn mode");
                        send_action(&action_socket, serde_json::json!({"command": "learn"}));
                    }
                    Action::EnforceMode => {
                        info!("menu: enforce mode");
                        send_action(&action_socket, serde_json::json!({"command": "enforce"}));
                    }
                    Action::Reload => {
                        info!("menu: reload config");
                        send_action(&action_socket, serde_json::json!({"command": "reload"}));
                    }
                    Action::Quit => {
                        info!("menu: quit");
                        handle.shutdown();
                        return;
                    }
                }
            }
        })
        .map_err(|e| format!("failed to start action handler: {e}"))?;

    // Block main thread until tray shuts down
    loop {
        std::thread::park();
    }
}
