//! The dashboard: our own GTK window, opened by a left click on the tray icon.

pub mod model;
mod placement;

use crate::commands::{Command, DISARM_PRESETS};
use crate::poll::Update;
use crate::settings::Settings;
use gtk::prelude::*;
use gtk::{gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use model::Body;
use placement::Rect;
use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const WIDTH: i32 = 352;

type TileWidgets = (gtk::Box, gtk::Box, gtk::Image, gtk::Label, gtk::Label);

pub struct Dashboard {
    window: gtk::Window,
    layer_shell: bool,
    icon: gtk::Image,
    title: gtk::Label,
    detail: gtk::Label,
    pill: gtk::Label,
    pill_class: RefCell<&'static str>,
    /// Why the last command did not run. Hidden until one does not, and it
    /// never moves the state above it.
    notice: gtk::Label,
    mode_row: gtk::Box,
    enforce: gtk::ToggleButton,
    learn: gtk::ToggleButton,
    /// Set while `update` moves the mode toggles, so the move is not sent
    /// back to the daemon as a command.
    syncing: Rc<Cell<bool>>,
    body: gtk::Stack,
    ring: gtk::DrawingArea,
    ring_fraction: Rc<Cell<f64>>,
    /// What the ring measures against, latched when a disarm starts. The
    /// daemon only reports what is left, so recomputing it every second would
    /// refill the ring each time the remainder crosses a preset.
    ring_total: Cell<u64>,
    ring_label: gtk::Label,
    pending_title: gtk::Label,
    pending_reason: gtk::Label,
    down_message: gtk::Label,
    watching: gtk::Label,
    /// Holds the tiles. Hidden with them, so its well does not show up empty.
    grid: gtk::Grid,
    tiles: Vec<TileWidgets>,
    footer: gtk::Label,
    /// Takes the keyboard focus on every open, so a stray Enter or Space
    /// cannot switch the mode or disarm.
    close: gtk::Button,
    /// Set by the first key press, which is when a focus ring is wanted.
    typed: Rc<Cell<bool>>,
}

fn label(classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::builder().xalign(0.0).build();
    for class in classes {
        l.add_css_class(class);
    }
    l
}

fn command_button(
    text: &str,
    command: Command,
    commands: &async_channel::Sender<Command>,
) -> gtk::Button {
    let button = gtk::Button::with_label(text);
    let commands = commands.clone();
    button.connect_clicked(move |_| {
        let _ = commands.try_send(command);
    });
    button
}

impl Dashboard {
    /// `open` follows the window: the poller only asks the daemon to walk the
    /// buses while the dashboard is on screen.
    pub fn new(
        app: &gtk::Application,
        commands: async_channel::Sender<Command>,
        open: Arc<AtomicBool>,
        settings: Rc<Settings>,
    ) -> Rc<Self> {
        let window = gtk::Window::builder()
            .application(app)
            .title("plugkill")
            .resizable(false)
            .default_width(WIDTH)
            .build();
        window.add_css_class("plugkill-dashboard");
        // Every show and hide goes through the property, so this is the one place.
        window.connect_visible_notify(move |w| open.store(w.is_visible(), Ordering::Relaxed));
        let layer_shell = gtk4_layer_shell::is_supported();
        if layer_shell {
            window.init_layer_shell();
            window.set_layer(Layer::Overlay);
            window.set_namespace(Some("plugkill-dashboard"));
            window.set_keyboard_mode(KeyboardMode::OnDemand);
        }

        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("dashboard");
        window.set_child(Some(&root));

        // Header: icon, title and detail, pill.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let icon = gtk::Image::new();
        icon.set_pixel_size(30);
        let titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
        titles.set_hexpand(true);
        let title = label(&["state-title"]);
        let detail = label(&["state-detail"]);
        // A wrapping label asks for its whole text on one line; without a cap
        // the non-resizable window grows to fit it.
        detail.set_wrap(true);
        detail.set_max_width_chars(1);
        titles.append(&title);
        titles.append(&detail);
        let pill = label(&["pill"]);
        pill.set_valign(gtk::Align::Center);
        header.append(&icon);
        header.append(&titles);
        header.append(&pill);
        root.append(&header);

        // A command the daemon refused, under the header where the state is.
        let notice = label(&["notice"]);
        notice.set_wrap(true);
        notice.set_max_width_chars(1);
        notice.set_visible(false);
        root.append(&notice);

        // Mode switch.
        let mode_row = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        mode_row.add_css_class("segm");
        mode_row.set_homogeneous(true);
        let enforce = gtk::ToggleButton::with_label("Enforce");
        let learn = gtk::ToggleButton::with_label("Learn");
        enforce.set_hexpand(true);
        learn.set_hexpand(true);
        enforce.add_css_class("seg");
        learn.add_css_class("seg");
        learn.set_group(Some(&enforce));
        mode_row.append(&enforce);
        mode_row.append(&learn);
        root.append(&mode_row);
        let syncing = Rc::new(Cell::new(false));
        for (button, command) in [(&enforce, Command::Enforce), (&learn, Command::Learn)] {
            let syncing = syncing.clone();
            let commands = commands.clone();
            button.connect_toggled(move |b| {
                if b.is_active() && !syncing.get() {
                    let _ = commands.try_send(command);
                }
            });
        }

        // Body: one page per state.
        let body = gtk::Stack::new();
        // Size the body by the visible page only. A homogeneous stack also measures
        // hidden pages, and when the window shrinks GTK measures its width for the
        // new height, so a squeezed kill-pending reason would widen the window.
        body.set_vhomogeneous(false);
        body.set_hhomogeneous(false);

        let presets = gtk::Box::new(gtk::Orientation::Vertical, 10);
        let presets_title = label(&["section"]);
        presets_title.set_label("DISARM FOR");
        presets.append(&presets_title);
        let preset_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        preset_row.add_css_class("presets");
        preset_row.set_homogeneous(true);
        for (secs, _, short) in DISARM_PRESETS {
            preset_row.append(&command_button(short, Command::Disarm(secs), &commands));
        }
        presets.append(&preset_row);
        body.add_named(&presets, Some("presets"));

        let countdown = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        countdown.add_css_class("countdown");
        let ring_fraction = Rc::new(Cell::new(1.0));
        let ring = gtk::DrawingArea::builder()
            .content_width(74)
            .content_height(74)
            .build();
        {
            let fraction = ring_fraction.clone();
            ring.set_draw_func(move |_, cr, w, h| draw_ring(cr, w, h, fraction.get()));
        }
        let ring_overlay = gtk::Overlay::new();
        ring_overlay.set_child(Some(&ring));
        let ring_label = label(&["ring-label"]);
        ring_label.set_halign(gtk::Align::Center);
        ring_overlay.add_overlay(&ring_label);
        let countdown_text = gtk::Box::new(gtk::Orientation::Vertical, 4);
        countdown_text.set_valign(gtk::Align::Center);
        let unprotected = label(&["box-title"]);
        unprotected.set_label("Unprotected");
        let rearms = label(&["state-detail"]);
        rearms.set_label("re-arms automatically");
        let arm = command_button("Arm now", Command::Arm, &commands);
        arm.add_css_class("bigbtn");
        arm.set_halign(gtk::Align::Start);
        countdown_text.append(&unprotected);
        countdown_text.append(&rearms);
        countdown_text.append(&arm);
        countdown.append(&ring_overlay);
        countdown.append(&countdown_text);
        body.add_named(&countdown, Some("countdown"));

        let pending = gtk::Box::new(gtk::Orientation::Vertical, 6);
        pending.add_css_class("pending");
        let pending_title = label(&["box-title"]);
        let pending_reason = label(&[]);
        pending_reason.set_wrap(true);
        pending_reason.set_max_width_chars(1);
        let disarm = command_button("Disarm 5 min", Command::Disarm(300), &commands);
        disarm.add_css_class("bigbtn");
        disarm.add_css_class("danger");
        disarm.set_halign(gtk::Align::Start);
        pending.append(&pending_title);
        pending.append(&pending_reason);
        pending.append(&disarm);
        body.add_named(&pending, Some("pending"));

        let down_message = label(&["down"]);
        down_message.set_wrap(true);
        down_message.set_max_width_chars(1);
        body.add_named(&down_message, Some("down"));
        root.append(&body);

        // Buses.
        let watching = label(&["section"]);
        root.append(&watching);
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(6)
            .column_homogeneous(true)
            .build();
        grid.add_css_class("well");
        let mut tiles = Vec::new();
        for i in 0..plugkill_core::ipc::BUSES.len() {
            let tile = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            tile.add_css_class("tile");
            // The lamp says watched or not; the size request is the whole widget.
            let lamp = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            lamp.add_css_class("lamp");
            lamp.set_size_request(7, 7);
            lamp.set_halign(gtk::Align::Center);
            lamp.set_valign(gtk::Align::Center);
            let icon = gtk::Image::new();
            // 16 px plus the lamp overflows the 154 px column, so "Power" ellipsizes
            // next to "on battery".
            icon.set_pixel_size(14);
            icon.add_css_class("tile-icon");
            let name = label(&["tile-name"]);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let value = label(&["tile-value"]);
            tile.append(&lamp);
            tile.append(&icon);
            tile.append(&name);
            tile.append(&value);
            grid.attach(&tile, (i % 2) as i32, (i / 2) as i32, 1, 1);
            tiles.push((tile, lamp, icon, name, value));
        }
        root.append(&grid);

        // Footer.
        let foot = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        foot.add_css_class("foot");
        let footer = label(&["state-detail"]);
        footer.set_hexpand(true);
        // One line that has to give way: a long uptime plus violation count
        // would otherwise widen a window the placement math assumes is WIDTH.
        footer.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let close = gtk::Button::with_label("Close");
        {
            let window = window.clone();
            close.connect_clicked(move |_| window.set_visible(false));
        }
        // The second way in, beside the tray menu (A2).
        let settings_button = gtk::Button::with_label("Settings");
        settings_button.connect_clicked(move |_| settings.open());
        foot.append(&footer);
        foot.append(&settings_button);
        foot.append(&command_button("Reload", Command::Reload, &commands));
        foot.append(&close);
        root.append(&foot);

        // Esc closes. The first key press is also what earns a focus ring: GTK
        // turns focus-visible back on by itself once the compositor hands the
        // layer surface its keyboard, which drew a ring on Close while the
        // pointer was somewhere else entirely.
        let typed = Rc::new(Cell::new(false));
        let keys = gtk::EventControllerKey::new();
        {
            let window = window.clone();
            let typed = typed.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                typed.set(true);
                if key == gdk::Key::Escape {
                    window.set_visible(false);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        window.add_controller(keys);
        {
            let typed = typed.clone();
            window.connect_focus_visible_notify(move |w| {
                if w.gets_focus_visible() && !typed.get() {
                    w.set_focus_visible(false);
                }
            });
        }

        Rc::new(Self {
            window,
            layer_shell,
            icon,
            title,
            detail,
            pill,
            pill_class: RefCell::new("pill-down"),
            notice,
            mode_row,
            enforce,
            learn,
            syncing,
            body,
            ring,
            ring_fraction,
            ring_total: Cell::new(0),
            ring_label,
            pending_title,
            pending_reason,
            down_message,
            watching,
            grid,
            tiles,
            footer,
            close,
            typed,
        })
    }

    pub fn update(&self, update: &Update) {
        let m = model::build(&update.state, update.status.as_ref());

        self.icon.set_icon_name(Some(m.icon));
        self.title.set_label(&m.title);
        self.detail.set_label(&m.detail);
        self.pill.set_label(m.pill.0);
        let old = self.pill_class.replace(m.pill.1);
        self.pill.remove_css_class(old);
        self.pill.add_css_class(m.pill.1);

        match m.mode_learn {
            Some(learn) => {
                self.mode_row.set_visible(true);
                self.syncing.set(true);
                if learn {
                    self.learn.set_active(true);
                } else {
                    self.enforce.set_active(true);
                }
                self.syncing.set(false);
            }
            None => self.mode_row.set_visible(false),
        }

        if !matches!(m.body, Body::Countdown { .. }) {
            self.ring_total.set(0);
        }
        match &m.body {
            Body::Presets => self.body.set_visible_child_name("presets"),
            Body::Countdown {
                secs_left,
                total_secs,
            } => {
                // Only a new or extended disarm raises what is left, so the
                // total is captured once and the ring drains from there.
                if *secs_left > self.ring_total.get() {
                    self.ring_total.set(*total_secs);
                }
                self.ring_fraction
                    .set(*secs_left as f64 / self.ring_total.get().max(1) as f64);
                self.ring_label.set_label(&model::clock(*secs_left));
                self.ring.queue_draw();
                self.body.set_visible_child_name("countdown");
            }
            Body::KillPending {
                secs_left,
                reason,
                hint,
            } => {
                self.pending_title
                    .set_label(&format!("Kill in {secs_left} s"));
                self.pending_reason.set_label(&format!("{reason}. {hint}"));
                self.body.set_visible_child_name("pending");
            }
            Body::Down { message } => {
                self.down_message.set_label(message);
                self.body.set_visible_child_name("down");
            }
        }

        self.watching.set_label(&m.watching.to_uppercase());
        self.watching.set_visible(!m.tiles.is_empty());
        self.grid.set_visible(!m.tiles.is_empty());
        for (i, (tile, lamp, icon, name, value)) in self.tiles.iter().enumerate() {
            match m.tiles.get(i) {
                Some(t) => {
                    tile.set_visible(true);
                    icon.set_icon_name(Some(t.icon));
                    name.set_label(t.name);
                    value.set_label(&t.value);
                    if t.watched {
                        tile.remove_css_class("off");
                        lamp.add_css_class("on");
                    } else {
                        tile.add_css_class("off");
                        lamp.remove_css_class("on");
                    }
                }
                None => tile.set_visible(false),
            }
        }
        // None on the ticks that skip the walk: the last list stays on the tiles.
        if let Some(devices) = &update.devices {
            for ((tile, ..), (key, name)) in self.tiles.iter().zip(plugkill_core::ipc::BUSES) {
                // Only on a change: setting the text re-queries the tooltip,
                // and the hover is the one case where the pointer is on the
                // widget being re-queried.
                let text = model::bus_tooltip(key, name, devices);
                if tile.tooltip_text().as_deref() != Some(text.as_str()) {
                    tile.set_tooltip_text(Some(&text));
                }
            }
        }
        self.footer.set_label(&m.footer);
    }

    /// Why the last command did not run, or None once one did. The header,
    /// the body and the tiles are the daemon's word and are not touched here:
    /// a refused disarm leaves the dashboard showing armed, because it is.
    pub fn set_notice(&self, text: Option<&str>) {
        self.notice.set_label(text.unwrap_or_default());
        self.notice.set_visible(text.is_some());
    }

    /// Left click on the tray icon: close if open, otherwise open by the click.
    pub fn toggle(&self, x: i32, y: i32) {
        if self.window.is_visible() {
            self.window.set_visible(false);
        } else {
            self.show_at(x, y);
        }
    }

    /// "Open dashboard" from the menu. The tray's left click uses `toggle`.
    pub fn open(&self) {
        if !self.window.is_visible() {
            self.show_at(0, 0);
        }
        self.window.present();
    }

    fn show_at(&self, x: i32, y: i32) {
        if self.layer_shell {
            self.place(x, y);
        }
        follow_theme(&self.window);
        // Set before showing: GTK only picks the first focusable widget (the
        // Enforce toggle) when the window has no focus widget, and a click on
        // another button while open moves the focus there.
        GtkWindowExt::set_focus(&self.window, Some(&self.close));
        // Focus set by us is not the user asking to see a focus ring: GTK would
        // draw one on Close as soon as the pointer enters the window. A key press
        // turns it back on.
        self.typed.set(false);
        self.window.set_focus_visible(false);
        self.window.present();
    }

    fn place(&self, x: i32, y: i32) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let list = display.monitors();
        let monitors: Vec<gdk::Monitor> = (0..list.n_items())
            .filter_map(|i| list.item(i).and_downcast::<gdk::Monitor>())
            .collect();
        let rects: Vec<Rect> = monitors
            .iter()
            .map(|m| {
                let g = m.geometry();
                Rect {
                    x: g.x(),
                    y: g.y(),
                    width: g.width(),
                    height: g.height(),
                }
            })
            .collect();
        let Some(p) = placement::place(x, y, &rects, WIDTH) else {
            return;
        };
        self.window.set_monitor(monitors.get(p.monitor));
        self.window.set_anchor(Edge::Top, p.top);
        self.window.set_anchor(Edge::Bottom, !p.top);
        self.window.set_anchor(Edge::Right, true);
        self.window.set_anchor(Edge::Left, false);
        let edge = if p.top { Edge::Top } else { Edge::Bottom };
        self.window.set_margin(edge, placement::INSET);
        self.window.set_margin(Edge::Right, p.right_margin);
    }
}

/// The card carries its own colours, so it has to pick the palette itself.
/// The window paints nothing and keeps the theme's text colour, which says
/// whether the desktop is light or dark. GTK's `prefers-color-scheme` was
/// light here on a dark desktop, so it is not used. The settings window picks
/// its palette the same way.
pub fn follow_theme(window: &gtk::Window) {
    let fg = WidgetExt::color(window);
    let light_text = 0.2126 * fg.red() + 0.7152 * fg.green() + 0.0722 * fg.blue() > 0.5;
    if light_text {
        window.remove_css_class("light");
    } else {
        window.add_css_class("light");
    }
}

/// The disarm countdown: a faint full track, and the remaining time over it,
/// both in the disarmed orange.
fn draw_ring(cr: &gtk::cairo::Context, width: i32, height: i32, fraction: f64) {
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let radius = cx.min(cy) - 4.0;
    cr.set_line_width(6.0);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_source_rgba(1.0, 0.541, 0.122, 0.25);
    cr.arc(cx, cy, radius, 0.0, 2.0 * PI);
    let _ = cr.stroke();
    cr.set_source_rgb(1.0, 0.541, 0.122);
    let end = -PI / 2.0 + 2.0 * PI * fraction.clamp(0.0, 1.0);
    cr.arc(cx, cy, radius, -PI / 2.0, end);
    let _ = cr.stroke();
}
