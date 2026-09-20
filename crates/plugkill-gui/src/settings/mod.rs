//! The settings window: a sidebar of panels over the daemon's own data.
//!
//! An ordinary resizable window, not the dashboard's layer-shell popover,
//! because it is a place to sit and work (A1). It shares the dashboard's
//! stylesheet, so the card, the wells and the keys look the same in both.
//!
//! Nothing here writes a file. The editor panel shows the TOML and the Nix it
//! would emit, with a copy button, and that is the whole of it.

mod editor;

mod allowances;
mod devices;
mod diagnostics;
mod violations;

use crate::commands::Command;
use crate::dashboard;
use crate::poll::Update;
use crate::status::{Devices, Status, TrayState, Violations};
use gtk::glib;
use gtk::prelude::*;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The size it opens at, and the smallest that still keeps the sidebar and one
/// column of content readable (A4).
const WIDTH: i32 = 960;
const HEIGHT: i32 = 640;
const MIN_WIDTH: i32 = 700;
const MIN_HEIGHT: i32 = 440;

/// What the poll loop last heard, handed to every panel on every update. A
/// field is None when this tick did not fetch it, and a panel then keeps what
/// it already shows rather than blanking.
pub struct PanelData<'a> {
    pub state: &'a TrayState,
    pub status: Option<&'a Status>,
    pub devices: Option<&'a Devices>,
    pub violations: Option<&'a Violations>,
}

/// One panel of the window. The shell owns the sidebar title, the order and
/// the scrolling; a panel owns its own widget and what it puts in it.
pub trait Panel {
    /// The sidebar row and the stack page name. One list drives both, so a
    /// panel cannot drift out of the order it is titled in.
    fn title(&self) -> &'static str;

    /// The panel's root. Asked for once, when the window is built.
    fn widget(&self) -> gtk::Widget;

    /// Called on every poll update, roughly once a second. Runs on the GTK
    /// thread, so it must not touch the socket.
    fn update(&self, data: &PanelData);

    /// Why the last command the window sent did not run, or None once one
    /// did. Most panels send nothing and ignore it.
    fn set_notice(&self, _text: Option<&str>) {}
}

/// The panels, in sidebar order.
fn build_panels(commands: &async_channel::Sender<Command>, socket: &Path) -> Vec<Rc<dyn Panel>> {
    // Built first and kept concrete: the devices and violations panels call
    // its `add_whitelist_entry` (D2, E3), and the diagnostics panel asks it
    // for `require_auth` rather than reading the config file again.
    let editor = editor::EditorPanel::new(commands.clone());
    vec![
        editor.clone(),
        devices::DevicesPanel::new(editor.clone()),
        violations::ViolationsPanel::new(editor.clone(), commands.clone()),
        allowances::AllowancesPanel::new(commands.clone()),
        diagnostics::DiagnosticsPanel::new(socket.to_path_buf(), editor),
    ]
}

pub struct Settings {
    window: gtk::Window,
    panels: Vec<Rc<dyn Panel>>,
}

impl Settings {
    /// `open` follows the window: the poller only asks for the devices and the
    /// violation history while it is on screen. `socket` is for the
    /// diagnostics panel to look at, not to talk to.
    pub fn new(
        app: &gtk::Application,
        commands: async_channel::Sender<Command>,
        socket: PathBuf,
        open: Arc<AtomicBool>,
    ) -> Rc<Self> {
        let window = gtk::Window::builder()
            .application(app)
            .title("plugkill settings")
            .default_width(WIDTH)
            .default_height(HEIGHT)
            .build();
        window.add_css_class("plugkill-settings");
        window.set_size_request(MIN_WIDTH, MIN_HEIGHT);
        window.connect_visible_notify(move |w| open.store(w.is_visible(), Ordering::Relaxed));
        // Closing this window is not quitting the tray, and a destroyed window
        // could not be shown again, so it hides instead.
        window.connect_close_request(|w| {
            w.set_visible(false);
            glib::Propagation::Stop
        });

        // Both classes: the card, wells and keys come from the dashboard's
        // stylesheet, and .settings takes back the popover's rounded edge.
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("dashboard");
        root.add_css_class("settings");
        window.set_child(Some(&root));

        let sidebar = gtk::ListBox::new();
        sidebar.add_css_class("sidebar");
        sidebar.set_selection_mode(gtk::SelectionMode::Single);

        let stack = gtk::Stack::new();
        stack.add_css_class("content");
        stack.set_hexpand(true);
        stack.set_vexpand(true);

        let panels = build_panels(&commands, &socket);
        let titles: Vec<&'static str> = panels.iter().map(|p| p.title()).collect();
        for (title, panel) in titles.iter().zip(&panels) {
            let row = gtk::Label::builder().label(*title).xalign(0.0).build();
            sidebar.append(&row);
            // The shell scrolls, so a panel is free to be as long as it needs.
            let scroller = gtk::ScrolledWindow::builder()
                .child(&panel.widget())
                .build();
            stack.add_named(&scroller, Some(title));
        }
        {
            let stack = stack.clone();
            sidebar.connect_row_selected(move |_, row| {
                if let Some(row) = row
                    && let Ok(index) = usize::try_from(row.index())
                    && let Some(name) = titles.get(index)
                {
                    stack.set_visible_child_name(name);
                }
            });
        }
        sidebar.select_row(sidebar.row_at_index(0).as_ref());
        root.append(&sidebar);
        root.append(&stack);

        Rc::new(Self { window, panels })
    }

    /// From the tray menu or the dashboard footer. A second open is the same
    /// window brought forward, never another one.
    pub fn open(&self) {
        dashboard::follow_theme(&self.window);
        self.window.present();
    }

    /// A refusal from the command thread, shown on whichever panel sent the
    /// command. Outside the visibility gate `update` has: it has to be on
    /// screen the moment the window is presented.
    pub fn set_notice(&self, text: Option<&str>) {
        for panel in &self.panels {
            panel.set_notice(text);
        }
    }

    pub fn update(&self, update: &Update) {
        // A hidden window shows nobody anything, and some panels read files
        // to fill themselves. Reopening forces a fetch on that tick, so a
        // presented window is at most one tick stale.
        if !self.window.is_visible() {
            return;
        }
        let data = PanelData {
            state: &update.state,
            status: update.status.as_ref(),
            devices: update.devices.as_ref(),
            violations: update.violations.as_ref(),
        };
        for panel in &self.panels {
            panel.update(&data);
        }
    }
}
