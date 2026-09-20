//! The allowances panel: what the daemon is letting through right now, and
//! the commands that change it. Spec F1 to F3.
//!
//! The table is the daemon's own answer, read off the status response. Nothing
//! here writes it: a button sends a command and the next status says what
//! happened, so a refused pair leaves the table exactly as it was.

use super::devices::{self, Attached};
use super::{Panel, PanelData};
use crate::commands::{self, Command, DISARM_PRESETS};
use crate::status::Status;
use gtk::prelude::*;
use plugkill_core::ipc::format_duration;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// One line of the table (F1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub selector: String,
    pub bus: String,
    /// The name the daemon knew when it granted this, what the devices reply
    /// calls the device now, or why there is neither (K1). Never empty.
    pub name: String,
    /// How it was granted: paired, or promoted from a violation.
    pub granted: String,
    pub age: String,
    pub expiry: String,
}

/// The column titles, in the order `Row` lists its fields.
const COLUMNS: [&str; 6] = ["SELECTOR", "BUS", "NAME", "GRANTED", "AGE", "EXPIRES"];

/// The `allowances` array of a status response, empty when there is none.
fn list(status: Option<&Status>) -> &[serde_json::Value] {
    status
        .and_then(|s| s.rest.get("allowances"))
        .and_then(|v| v.as_array())
        .map_or(&[], |v| v.as_slice())
}

fn text(entry: &serde_json::Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The table as the daemon reports it (F1). `attached` is what the devices
/// reply knows, for the rows the daemon recorded no name on (K1).
pub fn rows(status: Option<&Status>, attached: &BTreeMap<String, Attached>) -> Vec<Row> {
    list(status)
        .iter()
        .map(|a| Row {
            name: match text(a, "name") {
                n if n.is_empty() => devices::describe(&text(a, "selector"), attached),
                n => n,
            },
            selector: text(a, "selector"),
            bus: text(a, "bus"),
            granted: text(a, "granted_by"),
            age: format_duration(a.get("age_secs").and_then(|v| v.as_u64()).unwrap_or(0)),
            // The daemon rounds up, so 0 is an allowance that has run out and
            // is waiting for the next sweep, not one with a second to live.
            expiry: match a.get("expires_in_secs").and_then(|v| v.as_u64()) {
                None => "never".to_string(),
                Some(0) => "expired".to_string(),
                Some(secs) => format!("in {}", format_duration(secs)),
            },
        })
        .collect()
}

/// Whether two tables are the same rows, ignoring age and expiry. Those two
/// move every second, so comparing them would rebuild the grid every tick and
/// take a half-made click with it.
fn same_rows(a: &[Row], b: &[Row]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            (&x.selector, &x.bus, &x.name, &x.granted) == (&y.selector, &y.bus, &y.name, &y.granted)
        })
}

/// The selectors alone, for the devices panel to mark what is already covered
/// (D3).
pub fn selectors(status: Option<&Status>) -> Vec<String> {
    list(status)
        .iter()
        .filter_map(|a| Some(a.get("selector")?.as_str()?.to_string()))
        .collect()
}

/// What the pairing controls say about themselves (F3).
fn pairing_line(status: Option<&Status>) -> String {
    match status
        .and_then(|s| s.rest.get("pairing_window_secs_left"))
        .and_then(|v| v.as_u64())
    {
        Some(secs) => format!(
            "A pairing window is open: the next new device is allowed, {} left",
            format_duration(secs)
        ),
        None => "No pairing window is open.".to_string(),
    }
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).build();
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
        // Socket I/O belongs to the command thread; this only queues.
        let _ = commands.try_send(command);
    });
    button
}

pub struct AllowancesPanel {
    root: gtk::Box,
    commands: async_channel::Sender<Command>,
    /// A command the daemon did not run, worded by `commands::notice`.
    notice: gtk::Label,
    table: gtk::Grid,
    empty: gtk::Label,
    pairing: gtk::Label,
    /// The last table drawn. The poll ticks once a second and the table
    /// changes far less often, so it is only rebuilt when it differs.
    shown: RefCell<Option<Vec<Row>>>,
    /// What the devices reply last said, for the names beside the ids (K1).
    attached: RefCell<BTreeMap<String, Attached>>,
    /// The age and expiry label of each drawn row, in row order. They are
    /// written in place on the ticks that change nothing else.
    ages: RefCell<Vec<(gtk::Label, gtk::Label)>>,
}

impl AllowancesPanel {
    pub fn new(commands: async_channel::Sender<Command>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);

        let notice = label("", &["notice"]);
        notice.set_wrap(true);
        notice.set_visible(false);
        root.append(&notice);

        root.append(&label("ALLOWANCES", &["section"]));
        root.append(&label(
            "Devices the running daemon accepts. Held in memory only, so a restart clears them.",
            &["state-detail"],
        ));

        let table = gtk::Grid::builder()
            .row_spacing(2)
            .column_spacing(12)
            .build();
        table.add_css_class("well");
        root.append(&table);

        let empty = label("Nothing is allowed at runtime.", &["state-detail"]);
        root.append(&empty);

        let revoke_all = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        revoke_all.add_css_class("foot");
        revoke_all.append(&command_button("Revoke all", Command::RevokeAll, &commands));
        root.append(&revoke_all);

        root.append(&label("PAIRING", &["section"]));
        let pairing = label("", &["state-detail"]);
        pairing.set_wrap(true);
        root.append(&pairing);
        root.append(&label(
            "A pairing window allows the next new device instead of killing for it.",
            &["state-detail"],
        ));

        let window_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        window_row.add_css_class("foot");
        for (secs, _, short) in DISARM_PRESETS {
            window_row.append(&command_button(
                short,
                Command::Pair {
                    window_secs: secs,
                    for_secs: None,
                },
                &commands,
            ));
        }
        window_row.append(&command_button(
            "Close window",
            Command::Pair {
                window_secs: 0,
                for_secs: None,
            },
            &commands,
        ));
        root.append(&window_row);

        let last_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        last_row.add_css_class("foot");
        last_row.append(&command_button(
            "Allow last violation",
            Command::AllowLast { for_secs: None },
            &commands,
        ));
        root.append(&last_row);

        Rc::new(Self {
            root,
            commands,
            notice,
            table,
            empty,
            pairing,
            shown: RefCell::new(None),
            attached: RefCell::new(BTreeMap::new()),
            ages: RefCell::new(Vec::new()),
        })
    }

    fn draw(&self, rows: &[Row]) {
        while let Some(child) = self.table.first_child() {
            self.table.remove(&child);
        }
        for (col, title) in COLUMNS.iter().enumerate() {
            self.table
                .attach(&label(title, &["section"]), col as i32, 0, 1, 1);
        }
        let mut ages = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let y = i as i32 + 1;
            for (col, cell) in [
                (0, &row.selector),
                (1, &row.bus),
                (2, &row.name),
                (3, &row.granted),
            ] {
                let classes: &[&str] = if col == 0 {
                    &["mono"]
                } else {
                    &["state-detail"]
                };
                self.table.attach(&label(cell, classes), col, y, 1, 1);
            }
            // Kept rather than attached and forgotten: `update` writes these
            // two in place on the ticks where nothing else moved.
            let age = label(&row.age, &["state-detail"]);
            let expiry = label(&row.expiry, &["state-detail"]);
            self.table.attach(&age, 4, y, 1, 1);
            self.table.attach(&expiry, 5, y, 1, 1);
            ages.push((age, expiry));
            let revoke = command_button(
                "Revoke",
                Command::Revoke(commands::intern(&row.selector)),
                &self.commands,
            );
            revoke.add_css_class("rowbtn");
            self.table.attach(&revoke, COLUMNS.len() as i32, y, 1, 1);
        }
        self.ages.replace(ages);
        self.table.set_visible(!rows.is_empty());
        self.empty.set_visible(rows.is_empty());
    }
}

impl Panel for AllowancesPanel {
    fn title(&self) -> &'static str {
        "Allowances"
    }

    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn update(&self, data: &PanelData) {
        // No status this tick means no news, not an empty table.
        if data.status.is_none() {
            return;
        }
        if let Some(devices) = data.devices {
            self.attached.replace(devices::attached(devices));
        }
        self.pairing.set_label(&pairing_line(data.status));
        // A name that changed is a row that differs, so `same_rows` already
        // redraws the table when a device is plugged or unplugged.
        let rows = rows(data.status, &self.attached.borrow());
        let same = self
            .shown
            .borrow()
            .as_deref()
            .is_some_and(|shown| same_rows(shown, &rows));
        if same {
            for ((age, expiry), row) in self.ages.borrow().iter().zip(&rows) {
                age.set_label(&row.age);
                expiry.set_label(&row.expiry);
            }
        } else {
            self.draw(&rows);
        }
        self.shown.replace(Some(rows));
    }

    /// Why the last command did not run, or None once one did. The table is
    /// the daemon's word and is not touched here, so a refused pair leaves it
    /// showing what the daemon still holds (F3).
    fn set_notice(&self, text: Option<&str>) {
        self.notice.set_label(text.unwrap_or_default());
        self.notice.set_visible(text.is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A status response carrying `extra`, as the poll loop would parse it.
    fn status(extra: serde_json::Value) -> Status {
        let mut data = serde_json::json!({"armed": true, "mode": "enforce"});
        data.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(data).expect("status parses")
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

    fn allowances() -> Status {
        status(serde_json::json!({"allowances": [
            {"selector": "usb:1050:0407", "bus": "usb", "name": "YubiKey",
             "granted_by": "paired", "age_secs": 65, "expires_in_secs": 252},
            {"selector": "pci:0000:01:00.0", "bus": "pci", "name": null,
             "granted_by": "promoted", "age_secs": 0, "expires_in_secs": null},
            {"selector": "sdcard:0x0000ba5e", "bus": "sdcard", "name": "SD card",
             "granted_by": "paired", "age_secs": 3600, "expires_in_secs": 0},
        ]}))
    }

    #[test]
    fn test_rows_render_every_column_the_daemon_sends() {
        let rows = rows(Some(&allowances()), &nothing());
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            Row {
                selector: "usb:1050:0407".into(),
                bus: "usb".into(),
                name: "YubiKey".into(),
                granted: "paired".into(),
                age: "1m 5s".into(),
                expiry: "in 4m 12s".into(),
            }
        );
        // No name and no expiry are the common case, not a missing field.
        // Nothing is attached here, so the row says that rather than nothing.
        assert_eq!(rows[1].name, devices::NOT_CONNECTED);
        assert_eq!(rows[1].expiry, "never");
        assert_eq!(rows[1].age, "0s");
    }

    #[test]
    fn test_a_row_the_daemon_named_nothing_on_takes_the_name_from_the_reply() {
        let attached = BTreeMap::from([(
            "pci:0000:01:00.0".to_string(),
            Attached {
                name: "Network controller".to_string(),
            },
        )]);
        let rows = rows(Some(&allowances()), &attached);
        assert_eq!(rows[1].name, "Network controller");
        // The daemon's own name still wins where it has one: it is what was
        // granted, whatever is plugged in now.
        assert_eq!(rows[0].name, "YubiKey");
    }

    #[test]
    fn test_an_expiry_that_has_passed_says_so() {
        // The daemon rounds the remainder up, so 0 is an allowance that has
        // run out and not one with a second left.
        let rows = rows(Some(&allowances()), &nothing());
        assert_eq!(rows[2].expiry, "expired");
    }

    #[test]
    fn test_no_status_or_no_allowances_is_an_empty_table() {
        assert!(rows(None, &nothing()).is_empty());
        assert!(rows(Some(&status(serde_json::json!({}))), &nothing()).is_empty());
        assert!(
            rows(
                Some(&status(serde_json::json!({"allowances": []}))),
                &nothing()
            )
            .is_empty()
        );
    }

    #[test]
    fn test_selectors_are_what_the_devices_panel_matches_on() {
        assert_eq!(
            selectors(Some(&allowances())),
            ["usb:1050:0407", "pci:0000:01:00.0", "sdcard:0x0000ba5e"]
        );
        assert!(selectors(None).is_empty());
    }

    #[test]
    fn test_the_pairing_line_says_whether_a_window_is_open() {
        assert_eq!(
            pairing_line(Some(&status(
                serde_json::json!({"pairing_window_secs_left": 252})
            ))),
            "A pairing window is open: the next new device is allowed, 4m 12s left"
        );
        assert_eq!(pairing_line(None), "No pairing window is open.");
        assert_eq!(
            pairing_line(Some(&status(serde_json::json!({})))),
            "No pairing window is open."
        );
    }

    #[test]
    fn test_only_a_real_change_rebuilds_the_table() {
        // The age ticks every second. Rebuilding on that would take a
        // half-made Revoke click with it.
        let mut older = allowances();
        let list = older.rest.get_mut("allowances").unwrap();
        list[0]["age_secs"] = serde_json::json!(66);
        list[0]["expires_in_secs"] = serde_json::json!(251);
        assert!(same_rows(
            &rows(Some(&allowances()), &nothing()),
            &rows(Some(&older), &nothing())
        ));

        let mut moved = allowances();
        moved.rest.get_mut("allowances").unwrap()[0]["selector"] =
            serde_json::json!("usb:1050:0408");
        assert!(!same_rows(
            &rows(Some(&allowances()), &nothing()),
            &rows(Some(&moved), &nothing())
        ));
        assert!(!same_rows(
            &rows(Some(&allowances()), &nothing()),
            &rows(None, &nothing())
        ));
    }
}
