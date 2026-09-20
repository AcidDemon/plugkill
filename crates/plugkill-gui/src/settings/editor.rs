//! The config editor (B1 to B7, C1 to C3).
//!
//! Every control writes into one in-memory `Config` and nothing else. The
//! output pane emits that value as TOML and as a Nix attrset, having first
//! parsed the TOML back with the daemon's own loader, so a config the daemon
//! would reject is never offered. Nothing here writes a file, ever: the only
//! file it touches is the config it reads to open on.

mod highlight;
mod model;

use super::devices::{self, Attached};
use super::{Panel, PanelData};
use crate::commands::Command;
use gtk::prelude::*;
use model::ListKind;
use plugkill_core::config::{
    Config, DisplayPolicy, LidPolicy, NetworkPolicy, PciPolicy, PowerPolicy,
};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

/// A spin button's ceiling. Wider than the daemon accepts on purpose: the
/// loader clamps silently, and the output pane says what it clamped (B6).
const MAX_SLEEP_MS: f64 = 60_000.0;
const MAX_GRACE_SECS: f64 = 3_600.0;

/// The widgets the output pane fills. Held apart from the panel so every
/// control's handler can reach them without reaching the panel itself.
struct Out {
    origin: gtk::Label,
    notice: gtk::Label,
    problem: gtk::Label,
    clamped: gtk::Label,
    toml: Pane,
    nix: Pane,
    was: Pane,
    now: Pane,
    readonly: gtk::Label,
}

/// A code pane: the coloured text, the line numbers beside it, and the raw
/// string the copy button hands over. The numbers live in a label of their
/// own, so selecting or copying the code never picks them up (K3).
struct Pane {
    code: gtk::Label,
    gutter: gtk::Label,
    raw: Rc<RefCell<String>>,
}

impl Pane {
    fn new() -> Self {
        let code = label(&["mono"]);
        code.set_selectable(true);
        // Not wrapped: the gutter beside it is one number per logical line, so
        // a wrapped line would put every number below it beside the wrong one.
        // A long line scrolls sideways in the well instead.
        code.set_wrap(false);
        code.set_valign(gtk::Align::Start);
        code.set_hexpand(true);
        let gutter = label(&["mono", "gutter"]);
        gutter.set_wrap(false);
        gutter.set_xalign(1.0);
        gutter.set_valign(gtk::Align::Start);
        Self {
            code,
            gutter,
            raw: Rc::new(RefCell::new(String::new())),
        }
    }

    /// Fills the pane from one document: coloured code, a number per line,
    /// and the raw text kept aside for the clipboard.
    fn set(&self, lang: highlight::Lang, text: &str) {
        self.code.set_markup(&highlight::markup(lang, text));
        self.gutter
            .set_label(&highlight::gutter(text.lines().count()));
        *self.raw.borrow_mut() = text.to_string();
    }

    /// Fills the pane from the diff. A changed line keeps its B5 mark while
    /// it is coloured: the star is bold and the colour sits inside it (K3a).
    fn set_diff(&self, lines: &[model::Line]) {
        let mut out = String::new();
        for line in lines {
            out.push_str(&highlight::diff_line_markup(
                highlight::Lang::Toml,
                &line.text,
                line.changed,
            ));
            out.push('\n');
        }
        self.code.set_markup(&out);
        self.gutter.set_label(&highlight::gutter(lines.len()));
        *self.raw.borrow_mut() = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }

    /// The pane in its well: the numbers on the left, the code beside them,
    /// the pair of them in a sideways scroller so a long line moves the
    /// numbers with it instead of renumbering the pane.
    fn well(&self) -> gtk::ScrolledWindow {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&self.gutter);
        row.append(&self.code);
        let well = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&row)
            .build();
        well.add_css_class("well");
        well
    }
}

/// Fills every control from the config, one closure per control.
type Syncers = Rc<RefCell<Vec<Box<dyn Fn(&Config)>>>>;

/// What a control needs to make an edit: the value it edits, the value the
/// window loaded, the output widgets, and the other controls to re-read it.
#[derive(Clone)]
struct Ctx {
    config: Rc<RefCell<Config>>,
    loaded: Rc<RefCell<Config>>,
    out: Rc<Out>,
    /// Whether the daemon's own config was actually read. False means the
    /// values are the defaults, and the output panes say so rather than
    /// offering an empty `[commands]` as the config to paste.
    read: Rc<Cell<bool>>,
    /// One direction only: a control changes the config, the config fills
    /// the controls.
    syncers: Syncers,
    /// Set while the controls are being filled, so a widget's own handler
    /// does not send that value back as an edit.
    syncing: Rc<Cell<bool>>,
}

impl Ctx {
    fn add_syncer(&self, f: impl Fn(&Config) + 'static) {
        self.syncers.borrow_mut().push(Box::new(f));
    }

    /// Change the config, then put the change back on screen.
    fn edit(&self, f: impl FnOnce(&mut Config)) {
        if self.syncing.get() {
            return;
        }
        f(&mut self.config.borrow_mut());
        self.sync();
    }

    fn sync(&self) {
        self.syncing.set(true);
        let config = self.config.borrow().clone();
        for syncer in self.syncers.borrow().iter() {
            syncer(&config);
        }
        self.syncing.set(false);
        self.render();
    }

    fn render(&self) {
        let out = model::build(&self.loaded.borrow(), &self.config.borrow());
        set_notice(&self.out.problem, out.error.as_deref());
        let clamped = (!out.clamped.is_empty()).then(|| {
            format!(
                "The daemon clamps, and the output already shows it clamped: {}.",
                out.clamped.join("; ")
            )
        });
        set_notice(&self.out.clamped, clamped.as_deref());
        let prefix = if self.read.get() {
            ""
        } else {
            model::UNREAD_PREFIX
        };
        let pane = |text: &str| match text.is_empty() {
            true => String::new(),
            false => format!("{prefix}{text}"),
        };
        self.out.toml.set(highlight::Lang::Toml, &pane(&out.toml));
        self.out.nix.set(highlight::Lang::Nix, &pane(&out.nix));
        // The diff panes carry it too: they are selectable, and "as it is now"
        // over the defaults would otherwise read as the running config.
        let head: Vec<model::Line> = prefix
            .lines()
            .map(|text| model::Line {
                text: text.to_string(),
                changed: false,
            })
            .collect();
        let with_head = |lines: &[model::Line]| -> Vec<model::Line> {
            head.iter().cloned().chain(lines.iter().cloned()).collect()
        };
        self.out.was.set_diff(&with_head(&out.loaded_lines));
        self.out.now.set_diff(&with_head(&out.emitted_lines));
    }

    /// One line about what the last action refused to do, or nothing.
    fn say(&self, text: Option<&str>) {
        set_notice(&self.out.notice, text);
    }
}

fn set_notice(label: &gtk::Label, text: Option<&str>) {
    label.set_label(text.unwrap_or_default());
    label.set_visible(text.is_some());
}

fn label(classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::builder().xalign(0.0).wrap(true).build();
    for class in classes {
        l.add_css_class(class);
    }
    l
}

/// A section heading and the well its controls sit in.
fn section(root: &gtk::Box, title: &str) -> gtk::Box {
    let heading = label(&["section"]);
    heading.set_label(&title.to_uppercase());
    root.append(&heading);
    let well = gtk::Box::new(gtk::Orientation::Vertical, 8);
    well.add_css_class("well");
    root.append(&well);
    well
}

fn row(well: &gtk::Box, text: &str, control: &impl IsA<gtk::Widget>) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let name = label(&[]);
    name.set_label(text);
    name.set_hexpand(true);
    row.append(&name);
    row.append(control);
    well.append(&row);
}

fn switch_row(
    well: &gtk::Box,
    ctx: &Ctx,
    text: &str,
    get: fn(&Config) -> bool,
    set: fn(&mut Config, bool),
) {
    let sw = gtk::Switch::new();
    sw.set_valign(gtk::Align::Center);
    let c = ctx.clone();
    sw.connect_active_notify(move |s| {
        let v = s.is_active();
        c.edit(move |config| set(config, v));
    });
    let w = sw.clone();
    ctx.add_syncer(move |config| w.set_active(get(config)));
    row(well, text, &sw);
}

fn text_row(
    well: &gtk::Box,
    ctx: &Ctx,
    text: &str,
    placeholder: &str,
    get: fn(&Config) -> String,
    set: fn(&mut Config, &str),
) {
    let entry = gtk::Entry::builder()
        .placeholder_text(placeholder)
        .hexpand(true)
        .build();
    let c = ctx.clone();
    entry.connect_changed(move |e| {
        let v = e.text().to_string();
        c.edit(move |config| set(config, &v));
    });
    let w = entry.clone();
    ctx.add_syncer(move |config| {
        // Only on a change: setting the text moves the cursor to the end, and
        // this runs on every keystroke.
        let v = get(config);
        if w.text() != v {
            w.set_text(&v);
        }
    });
    row(well, text, &entry);
}

fn number_row(
    well: &gtk::Box,
    ctx: &Ctx,
    text: &str,
    max: f64,
    get: fn(&Config) -> u64,
    set: fn(&mut Config, u64),
) {
    let spin = gtk::SpinButton::with_range(0.0, max, 1.0);
    spin.set_valign(gtk::Align::Center);
    let c = ctx.clone();
    spin.connect_value_changed(move |s| {
        let v = s.value().max(0.0) as u64;
        c.edit(move |config| set(config, v));
    });
    let w = spin.clone();
    ctx.add_syncer(move |config| {
        let v = get(config) as f64;
        if w.value() != v {
            w.set_value(v);
        }
    });
    row(well, text, &spin);
}

fn policy_row<P: Copy + PartialEq + 'static>(
    well: &gtk::Box,
    ctx: &Ctx,
    text: &str,
    options: &'static [(&'static str, P)],
    get: fn(&Config) -> P,
    set: fn(&mut Config, P),
) {
    let names: Vec<&str> = options.iter().map(|(name, _)| *name).collect();
    let drop = gtk::DropDown::from_strings(&names);
    drop.set_valign(gtk::Align::Center);
    let c = ctx.clone();
    drop.connect_selected_notify(move |d| {
        if let Some(&(_, policy)) = options.get(d.selected() as usize) {
            c.edit(move |config| set(config, policy));
        }
    });
    let w = drop.clone();
    ctx.add_syncer(move |config| {
        let current = get(config);
        if let Some(i) = options.iter().position(|(_, p)| *p == current) {
            w.set_selected(i as u32);
        }
    });
    row(well, text, &drop);
}

/// One whitelist or ignore list: the rows it holds, with a remove on each,
/// and an entry with an add (B2).
fn table(well: &gtk::Box, ctx: &Ctx, attached: &Attachments, kind: ListKind, placeholder: &str) {
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 4);
    well.append(&rows);
    let empty = label(&["state-detail"]);
    empty.set_label("Nothing listed.");
    rows.append(&empty);

    let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::Entry::builder()
        .placeholder_text(placeholder)
        .hexpand(true)
        .build();
    let add = gtk::Button::with_label("Add");
    add.add_css_class("rowbtn");
    add_row.append(&entry);
    add_row.append(&add);
    well.append(&add_row);

    let submit = {
        let ctx = ctx.clone();
        let entry = entry.clone();
        move || {
            let text = entry.text().to_string();
            let mut refused = None;
            ctx.edit(|config| match model::list_add(config, kind, &text) {
                Ok(_) => {}
                Err(e) => refused = Some(e),
            });
            ctx.say(refused.as_deref());
            if refused.is_none() {
                entry.set_text("");
            }
        }
    };
    {
        let submit = submit.clone();
        add.connect_clicked(move |_| submit());
    }
    entry.connect_activate(move |_| submit());

    let ctx_rows = ctx.clone();
    let attached = attached.clone();
    ctx.add_syncer(move |config| {
        while let Some(child) = rows.first_child() {
            rows.remove(&child);
        }
        let entries = model::list_entries(config, kind);
        if entries.is_empty() {
            rows.append(&empty);
            return;
        }
        for (value, text) in entries {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.add_css_class("tile");
            let shown = label(&[]);
            // The name beside the id, so what was clicked on the devices
            // panel is recognisable here (K1). No indent: these rows are in
            // config order, which says nothing about what hangs off what.
            let known = attached
                .borrow()
                .as_ref()
                .zip(model::entry_selector(kind, &value))
                .map(|(attached, selector)| devices::describe(&selector, attached));
            shown.set_label(&match known {
                Some(note) => format!("{text}  {note}"),
                None => text,
            });
            shown.set_hexpand(true);
            shown.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let remove = gtk::Button::with_label("Remove");
            remove.add_css_class("rowbtn");
            let ctx = ctx_rows.clone();
            remove.connect_clicked(move |_| {
                let value = value.clone();
                ctx.edit(move |config| model::list_remove(config, kind, &value));
            });
            line.append(&shown);
            line.append(&remove);
            rows.append(&line);
        }
    });
}

/// A code pane with its copy button. The button copies the raw string the
/// pane was filled with, so what lands on the clipboard is the config alone,
/// without the line numbers and without the markup.
fn output_pane(root: &gtk::Box, title: &str, pane: &Pane) {
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let heading = label(&["section"]);
    heading.set_label(&title.to_uppercase());
    heading.set_hexpand(true);
    let copy = gtk::Button::with_label("Copy");
    copy.add_css_class("rowbtn");
    let raw = pane.raw.clone();
    let widget = pane.code.clone();
    copy.connect_clicked(move |_| widget.clipboard().set_text(&raw.borrow()));
    head.append(&heading);
    head.append(&copy);
    root.append(&head);
    root.append(&pane.well());
}

/// What the devices reply last said about each selector, or None until one
/// has arrived. Empty is an answer, None is not: before the first reply a row
/// says nothing about whether its device is attached.
type Attachments = Rc<RefCell<Option<BTreeMap<String, Attached>>>>;

pub struct EditorPanel {
    root: gtk::Box,
    ctx: Ctx,
    attached: Attachments,
    /// The config path the editor last opened on, so a status every second
    /// does not re-read the file over a person's edits.
    path: RefCell<Option<String>>,
}

impl EditorPanel {
    pub fn new(commands: async_channel::Sender<Command>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);

        let out = Rc::new(Out {
            origin: label(&["state-detail"]),
            notice: label(&["notice"]),
            problem: label(&["notice"]),
            clamped: label(&["notice"]),
            toml: Pane::new(),
            nix: Pane::new(),
            was: Pane::new(),
            now: Pane::new(),
            readonly: label(&["mono"]),
        });
        root.append(&out.origin);
        root.append(&out.notice);
        set_notice(&out.notice, None);

        let ctx = Ctx {
            config: Rc::new(RefCell::new(Config::default())),
            loaded: Rc::new(RefCell::new(Config::default())),
            out: out.clone(),
            read: Rc::new(Cell::new(false)),
            syncers: Rc::new(RefCell::new(Vec::new())),
            syncing: Rc::new(Cell::new(false)),
        };
        let attached: Attachments = Rc::new(RefCell::new(None));

        build_sections(&root, &ctx, &attached);
        build_readonly(&root, &out);
        build_output(&root, &out, &commands);

        let panel = Rc::new(Self {
            root,
            ctx,
            attached,
            path: RefCell::new(None),
        });
        // Defaults on screen until a status says which file the daemon loaded.
        panel.open_on(model::load(""));
        panel
    }

    /// Put one entry in the whitelist of its bus, for the devices and
    /// violations panels (D2, E3). The name is not passed in and is never
    /// stored: the row reads it out of the devices reply, so a device that is
    /// no longer attached says so rather than showing a name from last time
    /// (K1). The refusal is returned rather than shown: the caller is on
    /// another stack page, and a notice written here would land where nobody
    /// can see it.
    ///
    /// `present` is how many devices of this identity the daemon lists. Two of
    /// one model are one entry standing for both, so a row for the second is
    /// not a duplicate to refuse but a count to raise (K2b).
    pub fn add_whitelist_entry(&self, selector: &str, present: u32) -> Result<(), String> {
        let (kind, entry) = model::selector_entry(selector)?;
        let mut refused = None;
        self.ctx.edit(|config| {
            if let Err(e) = model::list_cover_one(config, kind, &entry, present) {
                refused = Some(e);
            }
        });
        match refused {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The same, for a hub expansion that found `count` devices of one model
    /// behind the port (K2b). An entry already listed is raised to cover them
    /// rather than refused, so a partial expansion never reports a failure.
    pub fn add_whitelist_entries(&self, selector: &str, count: u32) -> Result<(), String> {
        let (kind, entry) = model::selector_entry(selector)?;
        let mut refused = None;
        self.ctx.edit(|config| {
            if let Err(e) = model::list_add_instances(config, kind, &entry, count) {
                refused = Some(e);
            }
        });
        match refused {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// `general.require_auth` out of the config the editor loaded, for the
    /// diagnostics panel (G1). None when the file could not be read: the
    /// defaults behind it are not the daemon's answer.
    pub fn require_auth(&self) -> Option<bool> {
        self.ctx
            .read
            .get()
            .then(|| self.ctx.loaded.borrow().general.require_auth)
    }

    /// The whitelists as `<bus>:<identity>` selectors, so the devices panel
    /// can mark what is already covered (D3). The edited config, not the
    /// loaded one: an entry just added here covers the device at once.
    pub fn whitelist_selectors(&self) -> Vec<String> {
        model::selectors(&self.ctx.config.borrow())
    }

    fn open_on(&self, loaded: model::Loaded) {
        self.ctx.out.origin.set_label(&loaded.origin);
        self.ctx.read.set(loaded.read);
        self.ctx.out.readonly.set_label(&match loaded.read {
            true => model::readonly_text(&loaded.config),
            // The defaults are not what root runs, so they are not stated as
            // if they were.
            false => model::UNREAD_READONLY.to_string(),
        });
        *self.ctx.loaded.borrow_mut() = loaded.config.clone();
        *self.ctx.config.borrow_mut() = loaded.config;
        self.ctx.sync();
    }
}

impl Panel for EditorPanel {
    fn title(&self) -> &'static str {
        "Settings"
    }

    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn update(&self, data: &PanelData) {
        // The names and the nesting the tables show come from the devices
        // reply (K1, K2). Redrawn only when it says something new: a redraw
        // every second would take a half-made click with it.
        if let Some(devices) = data.devices {
            let fresh = devices::attached(devices);
            if self.attached.borrow().as_ref() != Some(&fresh) {
                self.attached.replace(Some(fresh));
                self.ctx.sync();
            }
        }
        let Some(status) = data.status else {
            return;
        };
        // Only when the daemon names a file this editor has not opened yet.
        // Re-reading on every status would throw away what a person typed.
        if self.path.borrow().as_deref() == Some(status.config_path.as_str()) {
            return;
        }
        self.path.replace(Some(status.config_path.clone()));
        // Only onto an editor nobody has touched: the window is live before
        // the first status arrives, and with the daemon down it can be live
        // for a long time.
        let untouched = *self.ctx.config.borrow() == *self.ctx.loaded.borrow();
        if untouched {
            self.open_on(model::load(&status.config_path));
        } else {
            self.ctx.say(Some(&format!(
                "The daemon is running {}. Your edits are kept, so this window is showing \
                 them and not that file.",
                status.config_path
            )));
        }
    }

    fn set_notice(&self, text: Option<&str>) {
        self.ctx.say(text);
    }
}

/// The nine sections, in `model::SECTIONS` order (B1). The placeholder on each
/// field is the daemon's own default.
fn build_sections(root: &gtk::Box, ctx: &Ctx, attached: &Attachments) {
    let d = Config::default();
    let titles = model::SECTIONS;

    let general = section(root, titles[0]);
    number_row(
        &general,
        ctx,
        &format!("Poll interval, ms (default {})", d.general.sleep_ms),
        MAX_SLEEP_MS,
        |c| c.general.sleep_ms,
        |c, v| c.general.sleep_ms = v,
    );
    text_row(
        &general,
        ctx,
        "Log file",
        &d.general.log_file.display().to_string(),
        |c| c.general.log_file.display().to_string(),
        |c, v| c.general.log_file = v.into(),
    );
    switch_row(
        &general,
        ctx,
        "Dry run: log violations, kill nothing",
        |c| c.general.dry_run,
        |c, v| c.general.dry_run = v,
    );
    switch_row(
        &general,
        ctx,
        "Require authentication for disarm, learn and reload",
        |c| c.general.require_auth,
        |c, v| c.general.require_auth = v,
    );

    let usb = section(root, titles[1]);
    switch_row(
        &usb,
        ctx,
        "Watch USB",
        |c| c.general.watch_usb,
        |c, v| c.general.watch_usb = v,
    );
    table(
        &usb,
        ctx,
        attached,
        ListKind::Usb,
        "vendor:product, 1d6b:0002",
    );

    let tb = section(root, titles[2]);
    switch_row(
        &tb,
        ctx,
        "Watch Thunderbolt",
        |c| c.general.watch_thunderbolt,
        |c, v| c.general.watch_thunderbolt = v,
    );
    table(&tb, ctx, attached, ListKind::Thunderbolt, "unique id");

    let sd = section(root, titles[3]);
    switch_row(
        &sd,
        ctx,
        "Watch SD cards",
        |c| c.general.watch_sdcard,
        |c, v| c.general.watch_sdcard = v,
    );
    table(&sd, ctx, attached, ListKind::SdCard, "card serial");

    let power = section(root, titles[4]);
    switch_row(
        &power,
        ctx,
        "Watch power",
        |c| c.general.watch_power,
        |c, v| c.general.watch_power = v,
    );
    policy_row(
        &power,
        ctx,
        "Policy (default monitor)",
        &[
            ("monitor: log only", PowerPolicy::Monitor),
            ("trigger-once: first unplug", PowerPolicy::TriggerOnce),
            ("ac-required: any battery", PowerPolicy::AcRequired),
        ],
        |c| c.power.policy,
        |c, v| c.power.policy = v,
    );
    number_row(
        &power,
        ctx,
        "Grace, seconds (default 0, max 300)",
        MAX_GRACE_SECS,
        |c| c.power.grace_secs,
        |c, v| c.power.grace_secs = v,
    );
    switch_row(
        &power,
        ctx,
        "Only when the session is locked",
        |c| c.power.require_locked,
        |c, v| c.power.require_locked = v,
    );

    let network = section(root, titles[5]);
    switch_row(
        &network,
        ctx,
        "Watch network",
        |c| c.general.watch_network,
        |c, v| c.general.watch_network = v,
    );
    policy_row(
        &network,
        ctx,
        "Policy (default monitor)",
        &[
            ("monitor: log only", NetworkPolicy::Monitor),
            ("kill: link loss is a violation", NetworkPolicy::Kill),
        ],
        |c| c.network.policy,
        |c, v| c.network.policy = v,
    );
    number_row(
        &network,
        ctx,
        "Grace, seconds (default 0, max 300)",
        MAX_GRACE_SECS,
        |c| c.network.grace_secs,
        |c, v| c.network.grace_secs = v,
    );
    let ifaces = label(&["state-detail"]);
    ifaces.set_label("Interfaces to watch. Empty watches every interface.");
    network.append(&ifaces);
    table(&network, ctx, attached, ListKind::Network, "eth0");

    let lid = section(root, titles[6]);
    switch_row(
        &lid,
        ctx,
        "Watch the lid",
        |c| c.general.watch_lid,
        |c, v| c.general.watch_lid = v,
    );
    policy_row(
        &lid,
        ctx,
        "Policy (default monitor)",
        &[
            ("monitor: log only", LidPolicy::Monitor),
            ("kill: a close is a violation", LidPolicy::Kill),
        ],
        |c| c.lid.policy,
        |c, v| c.lid.policy = v,
    );
    number_row(
        &lid,
        ctx,
        "Grace, seconds (default 0, max 300)",
        MAX_GRACE_SECS,
        |c| c.lid.grace_secs,
        |c, v| c.lid.grace_secs = v,
    );

    let pci = section(root, titles[7]);
    switch_row(
        &pci,
        ctx,
        "Watch PCI",
        |c| c.general.watch_pci,
        |c, v| c.general.watch_pci = v,
    );
    policy_row(
        &pci,
        ctx,
        "Policy (default monitor)",
        &[
            ("monitor: log only", PciPolicy::Monitor),
            ("kill: any add or remove", PciPolicy::Kill),
        ],
        |c| c.pci.policy,
        |c, v| c.pci.policy = v,
    );
    let pci_hint = label(&["state-detail"]);
    pci_hint.set_label("Selectors never triggered on, matched as substrings.");
    pci.append(&pci_hint);
    table(&pci, ctx, attached, ListKind::Pci, "0000:01:00.0");

    let display = section(root, titles[8]);
    switch_row(
        &display,
        ctx,
        "Watch displays",
        |c| c.general.watch_display,
        |c, v| c.general.watch_display = v,
    );
    policy_row(
        &display,
        ctx,
        "Policy (default monitor)",
        &[
            ("monitor: log only", DisplayPolicy::Monitor),
            ("kill: any connect or disconnect", DisplayPolicy::Kill),
        ],
        |c| c.display.policy,
        |c, v| c.display.policy = v,
    );
    let display_hint = label(&["state-detail"]);
    display_hint.set_label("Connectors to skip, such as eDP for the internal panel.");
    display.append(&display_hint);
    table(&display, ctx, attached, ListKind::Display, "eDP-1");
}

/// `[commands]` and `[destruction]`, shown and not editable (B7).
fn build_readonly(root: &gtk::Box, out: &Rc<Out>) {
    let well = section(root, "Commands and destruction (read only)");
    let why = label(&["state-detail"]);
    why.set_label(
        "These are edited in the config file, by root. The kill commands run as root, and \
         the destruction list names files that get shredded, so a window anyone in the \
         socket group can open is no place to set them.",
    );
    well.append(&why);
    well.append(&out.readonly);
}

/// The output pane: what would be emitted, beside what was loaded, with the
/// reload the spec asks for next to it (B4, B5, C3).
fn build_output(root: &gtk::Box, out: &Rc<Out>, commands: &async_channel::Sender<Command>) {
    let heading = label(&["section"]);
    heading.set_label("OUTPUT");
    root.append(&heading);
    let explain = label(&["state-detail"]);
    explain.set_label(
        "This window never writes a file. Copy the text below into your config, or into \
         services.plugkill.settings on NixOS.",
    );
    root.append(&explain);
    root.append(&out.problem);
    root.append(&out.clamped);
    set_notice(&out.problem, None);
    set_notice(&out.clamped, None);

    output_pane(root, "TOML", &out.toml);
    output_pane(root, "Nix", &out.nix);

    let diff_heading = label(&["section"]);
    diff_heading.set_label("WHAT YOUR EDITS CHANGED");
    root.append(&diff_heading);
    let diff_hint = label(&["state-detail"]);
    // Each pane is named, because "the other side" means nothing until both
    // sides have a name.
    diff_hint.set_label(
        "The config as it is now, next to the config your edits would produce. \
         A * marks a line that appears in one but not the other.",
    );
    root.append(&diff_hint);
    let panes = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    panes.set_homogeneous(true);
    for (title, pane) in [("AS IT IS NOW", &out.was), ("WITH YOUR EDITS", &out.now)] {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let heading = label(&["section"]);
        heading.set_label(title);
        column.append(&heading);
        column.append(&pane.well());
        panes.append(&column);
    }
    root.append(&panes);

    let foot = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let reload = gtk::Button::with_label("Reload the daemon's own config file");
    reload.add_css_class("bigbtn");
    reload.set_halign(gtk::Align::Start);
    let commands = commands.clone();
    reload.connect_clicked(move |_| {
        let _ = commands.try_send(Command::Reload);
    });
    let caption = label(&["state-detail"]);
    caption.set_label(
        "Reloads the file on disk, the one named at the top. It does not apply anything on \
         this screen: the window never writes a config.",
    );
    foot.append(&reload);
    foot.append(&caption);
    root.append(&foot);
}
