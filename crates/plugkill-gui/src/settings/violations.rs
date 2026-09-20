//! The violations panel: what the daemon refused, newest first (E1 to E4).
//!
//! The daemon keeps the last 50 in memory and drops them on restart, so the
//! panel says that out loud. An empty table would otherwise read as "nothing
//! has ever happened here" when it only means the daemon was restarted.

use crate::commands::Command;
use crate::settings::devices::{self, Attached};
use crate::settings::editor::EditorPanel;
use crate::settings::{Panel, PanelData};
use crate::status::{Violations, format_uptime};
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

/// What the table is, said where the table would be (E4).
const KEPT_IN_MEMORY: &str = "The daemon keeps the last 50 violations in memory. \
     Nothing is written to disk, and the history is cleared when it restarts.";

/// Said in place of an empty table, so a fresh daemon does not look like a
/// broken panel.
const EMPTY: &str = "No violations since the daemon started.";

/// One row of the table, ready for the labels. Built by `rows`, which is the
/// whole of what the tests cover.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    /// Absolute local time, and how long ago it was.
    pub when: String,
    pub ago: String,
    pub bus: String,
    /// The device as a person reads it, or empty for an event that names none:
    /// a lid close, a power unplug.
    pub identity: String,
    pub message: String,
    /// The canonical `<bus>:<identity>` selector, when there is a device. Only
    /// a row with one carries the two actions (E3).
    pub selector: Option<String>,
    /// `allow_last` promotes the daemon's most recent violation, so only the
    /// newest row can offer it, and only when it names a device.
    pub can_allow: bool,
}

/// The table as the daemon sent it: newest first, in its order, never sorted
/// here. `attached` is what the devices reply knows, for the rows the daemon
/// named nothing on (K1).
fn rows(violations: &Violations, now_unix: u64, attached: &BTreeMap<String, Attached>) -> Vec<Row> {
    violations
        .iter()
        .enumerate()
        .map(|(i, v)| Row {
            when: when(v.at_unix),
            ago: ago(v.at_unix, now_unix),
            bus: v.bus.clone(),
            identity: identity(v.name.as_deref(), v.selector.as_deref(), attached),
            message: v.message.clone(),
            selector: v.selector.clone(),
            can_allow: i == 0 && v.selector.is_some(),
        })
        .collect()
}

/// The device column: the name with its selector behind it, and for a row the
/// daemon named nothing on, the selector with what the devices reply calls it,
/// or a note that it is not attached (K1). Never a bare id on its own, and
/// empty only for an event that identifies nothing.
fn identity(
    name: Option<&str>,
    selector: Option<&str>,
    attached: &BTreeMap<String, Attached>,
) -> String {
    match (name.filter(|n| !n.is_empty()), selector) {
        (Some(name), Some(selector)) => format!("{name} ({selector})"),
        (Some(name), None) => name.to_string(),
        (None, Some(selector)) => {
            format!("{} ({selector})", devices::describe(selector, attached))
        }
        (None, None) => String::new(),
    }
}

/// How long ago, on the scale that matters. A stamp in the future, which is a
/// clock that moved under us, reads as now rather than as a negative age.
fn ago(at_unix: u64, now_unix: u64) -> String {
    match now_unix.saturating_sub(at_unix) {
        0 => "just now".to_string(),
        secs => format!("{} ago", format_uptime(secs)),
    }
}

/// The stamp in local time. The daemon sends seconds since the epoch, and the
/// person reading the table is sitting in front of the machine it happened on.
fn when(at_unix: u64) -> String {
    i64::try_from(at_unix)
        .ok()
        .and_then(|t| glib::DateTime::from_unix_local(t).ok())
        .and_then(|t| t.format("%Y-%m-%d %H:%M:%S").ok())
        .map_or_else(|| at_unix.to_string(), |s| s.to_string())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub struct ViolationsPanel {
    root: gtk::Box,
    /// Why the last "Add to whitelist" did not take. Shown here, because the
    /// editor's own notice label is on a page nobody is looking at.
    notice: gtk::Label,
    /// Holds one `.tile` per row. Rebuilt only when the history changes, so a
    /// click is not taken out from under the pointer once a second.
    list: gtk::Box,
    empty: gtk::Label,
    shown: RefCell<Option<Violations>>,
    /// One `ago` label per row on screen. The history stops changing but the
    /// ages do not, so these are written in place on the ticks that rebuild
    /// nothing.
    ages: RefCell<Vec<gtk::Label>>,
    /// What the devices reply last said, for the names beside the ids (K1).
    attached: RefCell<BTreeMap<String, Attached>>,
    /// How many of each identity that reply listed, so the whitelist action
    /// can raise an entry's count without passing what is attached (K2b).
    instances: RefCell<HashMap<String, u32>>,
    /// The whitelist action goes through the editor, the same way the devices
    /// panel's does (D2, E3).
    editor: Rc<EditorPanel>,
    commands: async_channel::Sender<Command>,
}

impl ViolationsPanel {
    pub fn new(editor: Rc<EditorPanel>, commands: async_channel::Sender<Command>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);

        let title = gtk::Label::builder()
            .label("VIOLATIONS")
            .xalign(0.0)
            .build();
        title.add_css_class("section");
        root.append(&title);

        let kept = gtk::Label::builder()
            .label(KEPT_IN_MEMORY)
            .xalign(0.0)
            .wrap(true)
            .build();
        kept.add_css_class("state-detail");
        root.append(&kept);

        let notice = gtk::Label::builder().xalign(0.0).wrap(true).build();
        notice.add_css_class("notice");
        notice.set_visible(false);
        root.append(&notice);

        let empty = gtk::Label::builder().label(EMPTY).xalign(0.0).build();
        empty.add_css_class("down");
        // Hidden until a reply has arrived: a daemon that never answered has
        // not told us there is nothing, only that it is not talking.
        empty.set_visible(false);
        root.append(&empty);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        list.add_css_class("well");
        list.set_visible(false);
        root.append(&list);

        Rc::new(Self {
            root,
            notice,
            list,
            empty,
            shown: RefCell::new(None),
            ages: RefCell::new(Vec::new()),
            attached: RefCell::new(BTreeMap::new()),
            instances: RefCell::new(HashMap::new()),
            editor,
            commands,
        })
    }

    fn rebuild(&self, violations: &Violations) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut ages = Vec::new();
        for row in rows(violations, now_unix(), &self.attached.borrow()) {
            self.list.append(&self.build_row(&row, &mut ages));
        }
        self.ages.replace(ages);
        self.list.set_visible(!violations.is_empty());
        self.empty.set_visible(violations.is_empty());
    }

    fn build_row(&self, row: &Row, ages: &mut Vec<gtk::Label>) -> gtk::Box {
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 3);
        tile.add_css_class("tile");

        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        // Fixed widths, because the age is the one field whose length moves:
        // "40s ago" and "1h 6m ago" would otherwise push the bus out of line
        // and the columns would not read as columns. Wide enough for the
        // longest age this formats, a multi-day one.
        for (i, (text, class, chars, right)) in [
            (row.when.as_str(), "tile-value", 16, false),
            (row.ago.as_str(), "tile-value", 12, true),
            (row.bus.as_str(), "tile-name", 12, false),
        ]
        .into_iter()
        .enumerate()
        {
            let label = gtk::Label::builder()
                .label(text)
                .xalign(if right { 1.0 } else { 0.0 })
                .width_chars(chars)
                .max_width_chars(chars)
                .single_line_mode(true)
                .build();
            label.add_css_class(class);
            label.add_css_class("mono");
            // Kept rather than appended and forgotten: the age keeps moving
            // after the row it is on has stopped.
            if i == 1 {
                ages.push(label.clone());
            }
            head.append(&label);
        }
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        head.append(&spacer);

        // Both actions want a device to act on, so an event carries neither.
        if let Some(selector) = row.selector.clone() {
            let whitelist = gtk::Button::with_label("Add to whitelist");
            whitelist.add_css_class("rowbtn");
            whitelist.set_tooltip_text(Some(
                "Puts the entry in the editor. The window never writes the file.",
            ));
            let editor = self.editor.clone();
            let notice = self.notice.clone();
            // The same ceiling the devices panel uses: a second device of one
            // model raises the entry, and never past what is plugged in (K2b).
            let present = self.instances.borrow().get(&selector).copied().unwrap_or(1);
            whitelist.connect_clicked(move |_| {
                let refused = editor.add_whitelist_entry(&selector, present).err();
                notice.set_label(refused.as_deref().unwrap_or_default());
                notice.set_visible(refused.is_some());
            });
            head.append(&whitelist);
        }
        if row.can_allow {
            // Not named after this row on purpose: allow_last carries no
            // selector, so the daemon promotes whatever its newest violation
            // is when it handles the command, which may no longer be this one.
            let allow = gtk::Button::with_label("Allow the newest violation");
            allow.add_css_class("rowbtn");
            allow.set_tooltip_text(Some(
                "Allows whichever violation the daemon has newest when it answers, for as long \
                 as the daemon runs. Cleared on restart.",
            ));
            let commands = self.commands.clone();
            allow.connect_clicked(move |_| {
                // No expiry: an allowance dies with the daemon anyway (E4).
                let _ = commands.try_send(Command::AllowLast { for_secs: None });
            });
            head.append(&allow);
        } else if row.selector.is_some() {
            // E3 asks every identified row for both actions, and the protocol
            // has no allow-by-selector. Say where the second one went.
            let why = gtk::Label::builder()
                .label("runtime allow is only offered on the newest violation")
                .xalign(0.0)
                .build();
            why.add_css_class("state-detail");
            head.append(&why);
        }
        tile.append(&head);

        let message = gtk::Label::builder()
            .label(&row.message)
            .xalign(0.0)
            .wrap(true)
            .build();
        tile.append(&message);

        if !row.identity.is_empty() {
            let identity = gtk::Label::builder()
                .label(&row.identity)
                .xalign(0.0)
                .wrap(true)
                .build();
            identity.add_css_class("tile-value");
            tile.append(&identity);
        }
        tile
    }
}

impl Panel for ViolationsPanel {
    fn title(&self) -> &'static str {
        "Violations"
    }

    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn update(&self, data: &PanelData) {
        // A device plugged or unplugged changes what the rows can be named,
        // so the table is redrawn for that too (K1).
        let mut renamed = false;
        if let Some(devices) = data.devices {
            self.instances.replace(devices::instances(devices));
            let fresh = devices::attached(devices);
            if *self.attached.borrow() != fresh {
                self.attached.replace(fresh);
                renamed = true;
            }
        }
        // None on a tick that did not fetch, and unchanged on most that did:
        // rebuilding either way would drop a half-made click.
        let Some(violations) = data.violations else {
            return;
        };
        if !renamed && self.shown.borrow().as_ref() == Some(violations) {
            // Nothing new to draw, but "40s ago" is older than it was.
            let now = now_unix();
            for (label, v) in self.ages.borrow().iter().zip(violations) {
                label.set_label(&ago(v.at_unix, now));
            }
            return;
        }
        self.rebuild(violations);
        self.shown.replace(Some(violations.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Violation;

    fn device(at_unix: u64) -> Violation {
        Violation {
            at_unix,
            bus: "usb".into(),
            selector: Some("usb:1050:0407".into()),
            name: Some("YubiKey".into()),
            message: "new usb device".into(),
        }
    }

    /// A reply that listed both bus trees in full and found nothing on them.
    /// The bare bus markers are what let a row say a device is not connected
    /// rather than merely unlisted (K1).
    fn nothing() -> BTreeMap<String, Attached> {
        BTreeMap::from([
            ("usb:".to_string(), Attached::default()),
            ("pci:".to_string(), Attached::default()),
        ])
    }

    fn event(at_unix: u64) -> Violation {
        Violation {
            at_unix,
            bus: "lid".into(),
            selector: None,
            name: None,
            message: "lid closed".into(),
        }
    }

    #[test]
    fn test_an_empty_history_makes_no_rows() {
        assert!(rows(&Vec::new(), 1_700_000_000, &nothing()).is_empty());
    }

    #[test]
    fn test_an_event_carries_no_identity_and_no_actions() {
        let rows = rows(&vec![event(1_700_000_000)], 1_700_000_060, &nothing());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bus, "lid");
        assert_eq!(rows[0].message, "lid closed");
        assert!(rows[0].identity.is_empty(), "nothing to identify");
        assert!(rows[0].selector.is_none(), "so nothing to whitelist");
        assert!(!rows[0].can_allow, "and nothing to allow");
        assert_eq!(rows[0].ago, "1m ago");
        assert!(!rows[0].when.is_empty(), "the stamp still renders");
    }

    #[test]
    fn test_a_device_row_names_it_and_keeps_the_selector_for_the_editor() {
        let rows = rows(&vec![device(1_700_000_000)], 1_700_000_000, &nothing());
        assert_eq!(rows[0].identity, "YubiKey (usb:1050:0407)");
        assert_eq!(rows[0].selector.as_deref(), Some("usb:1050:0407"));
    }

    #[test]
    fn test_a_device_the_daemon_named_nothing_on_is_named_from_the_reply() {
        let mut v = device(1);
        v.name = None;
        // Nothing matching is attached: the row says so rather than showing a
        // bare id with a blank beside it (K1).
        assert_eq!(
            rows(&vec![v.clone()], 1, &nothing())[0].identity,
            format!("{} (usb:1050:0407)", devices::NOT_CONNECTED)
        );
        let attached = BTreeMap::from([(
            "usb:1050:0407".to_string(),
            Attached {
                name: "YubiKey".to_string(),
            },
        )]);
        assert_eq!(
            rows(&vec![v], 1, &attached)[0].identity,
            "YubiKey (usb:1050:0407)"
        );
    }

    #[test]
    fn test_only_the_newest_row_can_be_allowed() {
        // allow_last promotes the daemon's last violation and nothing else, so
        // the second device row must not offer a button that would allow the
        // first one.
        let rows = rows(&vec![device(3), device(2), event(1)], 4, &nothing());
        assert!(rows[0].can_allow);
        assert!(!rows[1].can_allow, "an older device is not the last one");
        assert!(
            rows[1].selector.is_some(),
            "it keeps the selector, so it still offers the whitelist action"
        );
        assert!(!rows[2].can_allow);
    }

    #[test]
    fn test_the_newest_row_being_an_event_leaves_nothing_to_allow() {
        // The daemon refuses allow_last here, so no row offers it: the device
        // below is no longer the last violation.
        let rows = rows(&vec![event(2), device(1)], 3, &nothing());
        assert!(rows.iter().all(|r| !r.can_allow));
    }

    #[test]
    fn test_the_daemon_order_is_kept() {
        let rows = rows(&vec![device(30), event(20), device(10)], 40, &nothing());
        assert_eq!(rows[0].ago, "10s ago");
        assert_eq!(rows[1].ago, "20s ago");
        assert_eq!(rows[2].ago, "30s ago");
    }

    #[test]
    fn test_how_long_ago_drops_to_the_scale_that_matters() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(100, 142), "42s ago");
        assert_eq!(ago(100, 100 + 600), "10m ago");
        assert_eq!(ago(100, 100 + 7_200), "2h 0m ago");
        assert_eq!(ago(100, 100 + 2 * 86_400), "2d 0h 0m ago");
    }

    #[test]
    fn test_a_stamp_from_the_future_reads_as_now() {
        // A clock that moved between the daemon stamping and us reading.
        assert_eq!(ago(1_700_000_100, 1_700_000_000), "just now");
    }
}
