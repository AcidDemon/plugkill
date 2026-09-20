//! The devices panel: everything the `devices` command reports, grouped by
//! bus and nested by port, and what already covers each entry. Spec D1 to D4,
//! K1, K2.
//!
//! The entries are the daemon's own lines, the same ones the tray tooltips
//! show. Nothing here enumerates anything itself.

use super::editor::EditorPanel;
use super::{Panel, PanelData, allowances};
use crate::status::{BusDevices, Devices};
use gtk::prelude::*;
use plugkill_core::allowances::DeviceRef;
use plugkill_core::ipc::BUSES;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

/// How far one level of the tree is pushed in, in pixels. The editor's
/// tables use the same step, so the two read as one shape (K2).
const INDENT: i32 = 18;

/// Said where a name would go when nothing matching is attached (K1).
pub(super) const NOT_CONNECTED: &str = "not connected";

/// Said instead when the bus could not be listed in full, so nothing here
/// knows whether the device is attached (K1).
const NOT_LISTED: &str = "not in the daemon's listing";

/// Said where a name would go when the device is attached but names itself
/// nothing.
const UNNAMED: &str = "attached, unnamed";

/// What one entry offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Not covered yet, and this listing names it: offer to whitelist it (D2).
    Add,
    /// Already covered, and by what (D3).
    Covered(&'static str),
    /// This bus has a whitelist, but it is keyed on something this listing
    /// does not carry, so the action cannot be offered. Says which.
    Unmatched(&'static str),
    /// This bus lists no identity any whitelist matches on, so there is
    /// nothing to offer and nothing to explain.
    Nothing,
}

/// One line of a bus listing as the reply sends it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// An id, and a name where the daemon knows one.
    pub text: String,
    /// The sysfs port path for USB and Thunderbolt, the address for PCI,
    /// empty everywhere else and on a daemon that does not send one (K2a).
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The line as the daemon sends it: an id, and a name where it knows one.
    pub text: String,
    /// The `<bus>:<identity>` selector, where this listing carries the
    /// identity its whitelist is matched on.
    pub selector: Option<String>,
    /// The name the daemon put after the id, when it knew one.
    pub name: Option<String>,
    pub action: Action,
}

/// A device and whatever is plugged into it (K2). A flat bus is all roots
/// with no children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub row: Row,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: &'static str,
    pub watched: bool,
    /// Why the group is greyed, or None while the bus is watched (D4).
    pub reason: Option<String>,
    pub nodes: Vec<Node>,
    /// How many entries the daemon dropped past its cap.
    pub more: u64,
    /// Whether this bus nests, so a dropped entry can be said to have left a
    /// hole in the tree rather than merely in the list.
    pub nests: bool,
}

/// What the devices reply knows about one device: the name to show beside its
/// id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attached {
    pub name: String,
}

/// The entries of one bus, with the path where the reply carries one. A
/// daemon that sends no path leaves it empty, and every device is a root.
fn entries(d: &BusDevices) -> Vec<Entry> {
    d.entries
        .iter()
        .map(|e| Entry {
            text: e.text.clone(),
            path: e.path.clone(),
        })
        .collect()
}

/// The buses whose path encodes a topology. The rest stay flat: an interface,
/// a lid or a connector has nothing to nest (K2). Thunderbolt is flat as
/// well: its sysfs name packs the route into `<domain>-<route>` with no
/// separator inside it, so there is no parent to read out of it.
fn nests(bus: &str) -> bool {
    matches!(bus, "usb" | "pci")
}

/// The selector for one entry, when this bus lists the identity it is matched
/// on. Thunderbolt lists vendor:device but is keyed by its unique id, and an
/// SD card lists manufacturer:OEM but is keyed by its serial, so neither can
/// be named from its line. Power, network and lid are events with nothing to
/// identify, and a display lists connectors rather than monitors.
fn selector(bus: &str, entry: &str) -> Option<String> {
    if !matches!(bus, "usb" | "pci") {
        return None;
    }
    let id = entry.split_whitespace().next()?;
    // Parsed rather than pasted: a selector the daemon would refuse is no
    // better than none, and the parse normalises what it accepts.
    format!("{bus}:{id}")
        .parse::<DeviceRef>()
        .ok()
        .map(|d| d.selector())
}

/// Why a bus with a whitelist still offers no action on its own listing.
/// Only the two that have one: power, network, lid and display have no
/// whitelist to explain.
fn no_selector_reason(bus: &str) -> Option<&'static str> {
    match bus {
        "thunderbolt" => Some("whitelisted by unique id, which this listing does not carry"),
        "sdcard" => Some("whitelisted by serial, which this listing does not carry"),
        _ => None,
    }
}

/// Whether the config's lists cover this selector. PCI ignore entries are
/// matched as substrings of the bare id, the way `plugkill_core::pci`
/// does it, so a prefix entry covers everything under it. Every other bus is
/// the whole selector.
fn in_whitelist(bus: &str, selector: &str, whitelisted: &[String]) -> bool {
    if bus != "pci" {
        return whitelisted.iter().any(|w| w == selector);
    }
    // Bare against bare: "01:00" is inside "0000:01:00.0", but "pci:01:00"
    // is not inside "pci:0000:01:00.0".
    let bare = selector.strip_prefix("pci:").unwrap_or(selector);
    whitelisted
        .iter()
        .filter_map(|w| w.strip_prefix("pci:"))
        .any(|token| !token.is_empty() && bare.contains(token))
}

/// Whether the whitelist still covers this device, taking the instance it is
/// covered by. A USB entry stands for `count` devices (K2b), which the caller
/// spells as that many copies of the selector, so the second device of one
/// model is only covered when the entry says there are two. A PCI entry is a
/// substring of an address rather than a device, so it covers everything
/// under it and counts nothing.
fn takes_instance(
    bus: &str,
    selector: &str,
    whitelisted: &[String],
    left: &mut HashMap<&str, usize>,
) -> bool {
    if bus == "pci" {
        return in_whitelist(bus, selector, whitelisted);
    }
    match left.get_mut(selector) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

/// The name the daemon put after the id, when it knew one.
fn device_name(entry: &str) -> Option<String> {
    entry.split_once(' ').map(|(_, name)| name.to_string())
}

/// The stem, drawn rather than spelled. Box-drawing characters cannot reach
/// across the gap between two rows, so the segments are painted on a strip
/// that spans the row's whole height and meets its neighbours exactly.
fn stem_area(ancestors: &[bool], last: bool) -> gtk::DrawingArea {
    let levels = ancestors.len();
    // No vexpand: a box child already fills the row's height, and asking for
    // more makes every row carrying a strip soak up the panel's spare space,
    // which is what pulled the rows apart.
    let area = gtk::DrawingArea::builder()
        .content_width(levels as i32 * INDENT)
        .build();
    area.add_css_class("stem");
    let ancestors: Vec<bool> = ancestors.to_vec();
    area.set_draw_func(move |a, cr, w, h| {
        let colour = gtk::prelude::WidgetExt::color(a);
        cr.set_source_rgba(
            colour.red() as f64,
            colour.green() as f64,
            colour.blue() as f64,
            colour.alpha() as f64,
        );
        cr.set_line_width(1.0);
        for (x1, y1, x2, y2) in segments(&ancestors, last, w as f64, h as f64) {
            cr.move_to(x1, y1);
            cr.line_to(x2, y2);
        }
        let _ = cr.stroke();
    });
    area
}

/// The stem's lines for one row, as (x1, y1, x2, y2) over a strip `w` wide and
/// `h` tall. A bar for every ancestor that still has rows below it, then this
/// row's own elbow: down to the middle, across to the text, and on down when
/// this is not the last of its siblings. The halves put a one pixel line on
/// the pixel grid instead of smearing it across two.
pub fn segments(ancestors: &[bool], last: bool, w: f64, h: f64) -> Vec<(f64, f64, f64, f64)> {
    let mut out = Vec::new();
    if ancestors.is_empty() {
        return out;
    }
    let step = f64::from(INDENT);
    let bar = |level: usize| (level as f64 * step + step / 2.0).floor() + 0.5;
    for (i, more_below) in ancestors[1..].iter().enumerate() {
        if *more_below {
            out.push((bar(i), 0.0, bar(i), h));
        }
    }
    let x = bar(ancestors.len() - 1);
    let mid = (h / 2.0).floor() + 0.5;
    out.push((x, 0.0, x, if last { mid } else { h }));
    out.push((x, mid, w, mid));
    out
}

/// Nest rows by their path (K2). The parent of `2-3.4` is `2-3`; a row whose
/// own parent is not in the reply hangs off the nearest listed ancestor, and
/// one with no listed ancestor, no separator, or no path at all is a root.
/// Order is the order the daemon sent, at every level.
pub fn tree(rows: Vec<(String, Row)>) -> Vec<Node> {
    let mut listed: HashMap<&str, usize> = HashMap::new();
    for (i, (path, _)) in rows.iter().enumerate() {
        if !path.is_empty() {
            listed.entry(path.as_str()).or_insert(i);
        }
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (i, (path, _)) in rows.iter().enumerate() {
        match ancestor(path, &listed) {
            Some(parent) => children[parent].push(i),
            None => roots.push(i),
        }
    }
    let rows: Vec<Row> = rows.into_iter().map(|(_, row)| row).collect();
    roots.iter().map(|&i| node(i, &rows, &children)).collect()
}

/// The nearest listed ancestor of a path, one component at a time. Every step
/// is strictly shorter than the last, so this cannot loop. The USB root hub
/// is the one edge the separators do not spell out: `2-3` hangs off `usb2`,
/// and so does anything on bus 2 whose own hub the reply left out.
fn ancestor(path: &str, listed: &HashMap<&str, usize>) -> Option<usize> {
    let mut rest = path;
    while let Some(cut) = rest.rfind(['.', '/']) {
        rest = &rest[..cut];
        if let Some(&parent) = listed.get(rest) {
            return Some(parent);
        }
    }
    let bus = rest.split('-').next()?;
    if bus.is_empty() || bus == rest || !bus.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    listed.get(format!("usb{bus}").as_str()).copied()
}

fn node(i: usize, rows: &[Row], children: &[Vec<usize>]) -> Node {
    Node {
        row: rows[i].clone(),
        children: children[i]
            .iter()
            .map(|&c| node(c, rows, children))
            .collect(),
    }
}

/// Every bus in `BUSES` order, with its entries and what covers them.
/// `allowed` and `whitelisted` are selectors: the runtime table from status,
/// and what the editor's config holds.
pub fn groups(devices: &Devices, allowed: &[String], whitelisted: &[String]) -> Vec<Group> {
    // How many devices each entry still covers: one per copy of the selector.
    let mut left: HashMap<&str, usize> = HashMap::new();
    for w in whitelisted {
        *left.entry(w.as_str()).or_default() += 1;
    }
    BUSES
        .iter()
        .map(|&(key, name)| {
            let bus = key.strip_suffix("_watching").unwrap_or(key);
            let Some(d) = devices.get(bus) else {
                // A bus the response does not carry says nothing about itself.
                return Group {
                    name,
                    watched: false,
                    reason: Some("this daemon did not report the bus".to_string()),
                    nodes: Vec::new(),
                    more: 0,
                    nests: nests(bus),
                };
            };
            if !d.watched {
                return Group {
                    name,
                    watched: false,
                    reason: Some(format!("not watched: general.watch_{bus} is off")),
                    nodes: Vec::new(),
                    more: 0,
                    nests: nests(bus),
                };
            }
            let rows = entries(d)
                .into_iter()
                .map(|entry| {
                    let sel = selector(bus, &entry.text);
                    let action = match &sel {
                        Some(s) => {
                            // The config comes first: a whitelist entry
                            // outlives the uptime an allowance is bounded to.
                            if takes_instance(bus, s, whitelisted, &mut left) {
                                Action::Covered("in the whitelist")
                            } else if allowed.contains(s) {
                                Action::Covered("allowed at runtime")
                            } else {
                                Action::Add
                            }
                        }
                        None => match no_selector_reason(bus) {
                            Some(why) => Action::Unmatched(why),
                            None => Action::Nothing,
                        },
                    };
                    let path = if nests(bus) {
                        entry.path
                    } else {
                        String::new()
                    };
                    (
                        path,
                        Row {
                            name: device_name(&entry.text),
                            text: entry.text,
                            selector: sel,
                            action,
                        },
                    )
                })
                .collect();
            Group {
                name,
                watched: true,
                reason: None,
                nodes: tree(rows),
                more: d.more,
                nests: nests(bus),
            }
        })
        .collect()
}

/// Everything "Add this and everything behind it" would add: this device when
/// nothing covers it yet, and each device behind it, by its own identity
/// (K2b). There is deliberately no entry meaning "anything on this port": the
/// config names what is there now, so a device plugged in later is still a
/// violation.
/// Two devices of one model are one whitelist entry, so the rows are folded
/// by selector and each carries how many devices it stands for. The daemon
/// allows that many instances by the entry's `count`, so the number is what
/// makes the second one legal.
pub fn expansion(node: &Node) -> Vec<(Row, u32)> {
    let mut gathered = Vec::new();
    gather(node, &mut gathered);
    // Every device of a model counts towards the entry, covered or not: the
    // daemon allows instances by the entry's `count`, so an entry that says
    // one while two are plugged in still kills on the second. Only a model
    // with something left uncovered needs an entry at all.
    let mut out: Vec<(Row, u32, bool)> = Vec::new();
    for row in gathered {
        let uncovered = row.action == Action::Add;
        match out
            .iter_mut()
            .find(|(seen, _, _)| seen.selector == row.selector)
        {
            Some((_, count, any)) => {
                *count += 1;
                *any |= uncovered;
            }
            None => out.push((row, 1, uncovered)),
        }
    }
    out.into_iter()
        .filter(|(_, _, any)| *any)
        .map(|(row, count, _)| (row, count))
        .collect()
}

fn gather(node: &Node, out: &mut Vec<Row>) {
    if node.row.selector.is_some() {
        out.push(node.row.clone());
    }
    for child in &node.children {
        gather(child, out);
    }
}

/// What an expansion would do, said before it is applied (K2c).
pub fn summary(rows: &[(Row, u32)]) -> String {
    let what: Vec<String> = rows
        .iter()
        .map(|(row, count)| match count {
            1 => row.text.clone(),
            n => format!("{} x{n}", row.text),
        })
        .collect();
    format!(
        "Adds {} whitelist {}, one per device identity: {}. Anything plugged into this port \
         later is still a violation.",
        rows.len(),
        if rows.len() == 1 { "entry" } else { "entries" },
        what.join(", ")
    )
}

/// Every device the reply lists, by its `<bus>:<identity>` selector (K1, K2).
/// Built off `groups`, so the names are the ones on screen. A bus the reply
/// listed in full also gets a bare `<bus>:` marker, which is what lets
/// `describe` say a device is not attached rather than merely unlisted.
pub fn attached(devices: &Devices) -> BTreeMap<String, Attached> {
    let mut out = BTreeMap::new();
    for (&(key, _), group) in BUSES.iter().zip(groups(devices, &[], &[])) {
        let bus = key.strip_suffix("_watching").unwrap_or(key);
        // Watched, listed whole, and listing the identity a whitelist names
        // (the buses `selector` names, and only those). Only then does an
        // absent selector mean the device is not there. A real selector always
        // carries an identity, so `<bus>:` cannot collide with one.
        if matches!(bus, "usb" | "pci") && group.watched && group.more == 0 {
            out.insert(format!("{bus}:"), Attached::default());
        }
        for node in &group.nodes {
            walk(node, &mut out);
        }
    }
    out
}

/// How many devices of each identity the reply lists. Two receivers of one
/// model are one whitelist entry covering two (K2b), so this is the ceiling a
/// panel may raise that entry's `count` to: never more than are plugged in.
pub fn instances(devices: &Devices) -> HashMap<String, u32> {
    let mut out: HashMap<String, u32> = HashMap::new();
    for group in groups(devices, &[], &[]) {
        for node in &group.nodes {
            count_selectors(node, &mut out);
        }
    }
    out
}

fn count_selectors(node: &Node, out: &mut HashMap<String, u32>) {
    if let Some(selector) = &node.row.selector {
        *out.entry(selector.clone()).or_default() += 1;
    }
    for child in &node.children {
        count_selectors(child, out);
    }
}

fn walk(node: &Node, out: &mut BTreeMap<String, Attached>) {
    if let Some(selector) = &node.row.selector {
        out.insert(
            selector.clone(),
            Attached {
                name: node.row.name.clone().unwrap_or_default(),
            },
        );
    }
    for child in &node.children {
        walk(child, out);
    }
}

fn lookup<'a>(selector: &str, attached: &'a BTreeMap<String, Attached>) -> Option<&'a Attached> {
    if let Some(a) = attached.get(selector) {
        return Some(a);
    }
    // A pci entry can be a prefix of the address, which is how the daemon
    // matches it, so an exact miss is tried again the daemon's way.
    let entry = [selector.to_string()];
    attached
        .iter()
        .find(|(listed, _)| listed.starts_with("pci:") && in_whitelist("pci", listed, &entry))
        .map(|(_, a)| a)
}

/// The name to show beside an id: what the reply calls the device, or why
/// there is none (K1). Never empty, so no row shows a blank where a name
/// would go.
pub fn describe(selector: &str, attached: &BTreeMap<String, Attached>) -> String {
    match lookup(selector, attached) {
        Some(a) if !a.name.is_empty() => a.name.clone(),
        Some(_) => UNNAMED.to_string(),
        // A bus whose listing carries no identity to match on cannot say
        // whether this one is attached, so it does not claim it is not.
        None => {
            let bus = selector.split(':').next().unwrap_or_default();
            match no_selector_reason(bus) {
                Some(why) => format!("no name here: {why}"),
                // Only a bus the reply listed in full can say a device is
                // gone; anything else is a listing this cannot see behind.
                None if attached.contains_key(&format!("{bus}:")) => NOT_CONNECTED.to_string(),
                None => NOT_LISTED.to_string(),
            }
        }
    }
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).build();
    for class in classes {
        l.add_css_class(class);
    }
    l
}

pub struct DevicesPanel {
    root: gtk::Box,
    /// Why the last "Add to whitelist" did not take. It belongs here and not
    /// on the editor page, which is not the page this was clicked on.
    notice: gtk::Label,
    /// Holds the groups, so redrawing does not touch the header above them.
    list: gtk::Box,
    /// The whitelist action and the covered check both go through the editor,
    /// which owns the in-memory config (D2, D3).
    editor: Rc<EditorPanel>,
    /// What is on screen. The poll ticks far more often than a bus changes,
    /// so the groups are only rebuilt when they differ.
    shown: RefCell<Option<Vec<Group>>>,
}

impl DevicesPanel {
    pub fn new(editor: Rc<EditorPanel>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
        root.append(&label("DEVICES", &["section"]));
        root.append(&label(
            "What the daemon sees on each bus, nested by the port it hangs off. Adding an entry \
             here changes the config in the Settings panel, which is still only text until you \
             copy it out.",
            &["state-detail"],
        ));
        let notice = label("", &["notice"]);
        notice.set_wrap(true);
        notice.set_visible(false);
        root.append(&notice);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 14);
        root.append(&list);
        Rc::new(Self {
            root,
            notice,
            list,
            editor,
            shown: RefCell::new(None),
        })
    }

    fn draw(&self, groups: &[Group], counts: &HashMap<String, u32>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for group in groups {
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
            box_.add_css_class("group");
            if !group.watched {
                box_.add_css_class("off");
            }
            box_.append(&label(&group.name.to_uppercase(), &["section"]));
            if let Some(reason) = &group.reason {
                box_.append(&label(reason, &["state-detail"]));
            }
            if group.watched {
                // No spacing: the stem is drawn per row and its segments have
                // to meet at the row boundary, which a gap would break.
                let well = gtk::Box::new(gtk::Orientation::Vertical, 0);
                well.add_css_class("well");
                if group.nodes.is_empty() {
                    // An enumeration the daemon could not read looks like an
                    // empty bus on the wire, so this cannot claim it is empty.
                    well.append(&label("no devices listed", &["state-detail", "tile"]));
                }
                for (i, node) in group.nodes.iter().enumerate() {
                    let last = i + 1 == group.nodes.len();
                    well.append(&self.node_widget(node, &[], last, counts));
                }
                if group.more > 0 {
                    // The daemon caps by text, not by topology, so a dropped
                    // hub leaves its children looking like roots.
                    let note = if group.nests {
                        format!("and {} more, so this tree is incomplete", group.more)
                    } else {
                        format!("and {} more", group.more)
                    };
                    well.append(&label(&note, &["state-detail", "tile"]));
                }
                box_.append(&well);
            }
            self.list.append(&box_);
        }
    }

    /// One device, then whatever hangs off it, drawn like `tree` so a glance
    /// says what hangs off what (K2).
    fn node_widget(
        &self,
        node: &Node,
        ancestors: &[bool],
        last: bool,
        counts: &HashMap<String, u32>,
    ) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        line.add_css_class("treerow");
        line.set_hexpand(true);
        if !ancestors.is_empty() {
            row.append(&stem_area(ancestors, last));
        }
        row.append(&line);
        let text = label(&node.row.text, &["mono"]);
        text.set_hexpand(true);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&text);
        match &node.row.action {
            Action::Add => {
                if let Some(selector) = node.row.selector.clone() {
                    let button = gtk::Button::with_label("Add to whitelist");
                    button.add_css_class("rowbtn");
                    let editor = self.editor.clone();
                    let notice = self.notice.clone();
                    // Two of one model are one entry standing for both, so the
                    // click has to be able to raise an entry that is already
                    // there. What is plugged in is the ceiling (K2b).
                    let present = counts.get(&selector).copied().unwrap_or(1);
                    button.connect_clicked(move |_| {
                        let refused = editor.add_whitelist_entry(&selector, present).err();
                        notice.set_label(refused.as_deref().unwrap_or_default());
                        notice.set_visible(refused.is_some());
                    });
                    line.append(&button);
                }
            }
            Action::Covered(by) | Action::Unmatched(by) => {
                line.append(&label(by, &["state-detail"]));
            }
            Action::Nothing => {}
        }
        box_.append(&row);
        let rows = expansion(node);
        let mut confirm = None;
        if !node.children.is_empty() && !rows.is_empty() {
            let (button, tile) = self.expand_button(rows, ancestors.len() as i32);
            line.append(&button);
            confirm = Some(tile);
        }
        // A child's own stem needs to know whether this row still has siblings
        // below it, so the vertical bar continues past its subtree or stops.
        let mut below = ancestors.to_vec();
        below.push(!last);
        for (i, child) in node.children.iter().enumerate() {
            let child_last = i + 1 == node.children.len();
            box_.append(&self.node_widget(child, &below, child_last, counts));
        }
        // After the children, not between them and their parent: a row's stem
        // is drawn on its own strip and has to meet the next row's, which a
        // tile wedged in the middle would break.
        if let Some(tile) = confirm {
            box_.append(&tile);
        }
        box_
    }

    /// "Add this and everything behind it", and the confirmation it opens:
    /// what would be added, and a way out, before anything is (K2b, K2c).
    fn expand_button(&self, rows: Vec<(Row, u32)>, depth: i32) -> (gtk::Button, gtk::Box) {
        let confirm = gtk::Box::new(gtk::Orientation::Vertical, 4);
        confirm.add_css_class("tile");
        confirm.set_margin_start((depth + 1) * INDENT);
        confirm.set_visible(false);
        let what = label(&summary(&rows), &["state-detail"]);
        what.set_wrap(true);
        confirm.append(&what);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let apply = gtk::Button::with_label(&format!("Add {} now", rows.len()));
        apply.add_css_class("rowbtn");
        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("rowbtn");
        buttons.append(&apply);
        buttons.append(&cancel);
        confirm.append(&buttons);

        {
            let confirm = confirm.clone();
            cancel.connect_clicked(move |_| confirm.set_visible(false));
        }
        {
            let editor = self.editor.clone();
            let notice = self.notice.clone();
            let confirm = confirm.clone();
            apply.connect_clicked(move |_| {
                let mut refused = None;
                for (row, count) in &rows {
                    let Some(selector) = &row.selector else {
                        continue;
                    };
                    if let Err(e) = editor.add_whitelist_entries(selector, *count) {
                        refused.get_or_insert(e);
                    }
                }
                notice.set_label(refused.as_deref().unwrap_or_default());
                notice.set_visible(refused.is_some());
                confirm.set_visible(false);
            });
        }

        let button = gtk::Button::with_label("Add this and everything behind it");
        button.add_css_class("rowbtn");
        button.set_tooltip_text(Some(
            "One entry per device behind this port, each by its own identity. Nothing is added \
             until you confirm.",
        ));
        let shown = confirm.clone();
        button.connect_clicked(move |_| shown.set_visible(true));
        (button, confirm)
    }
}

impl Panel for DevicesPanel {
    fn title(&self) -> &'static str {
        "Devices"
    }

    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn update(&self, data: &PanelData) {
        // No walk this tick means no news, not an empty machine.
        let Some(devices) = data.devices else {
            return;
        };
        let groups = groups(
            devices,
            &allowances::selectors(data.status),
            &self.editor.whitelist_selectors(),
        );
        if self.shown.borrow().as_deref() == Some(groups.as_slice()) {
            return;
        }
        self.draw(&groups, &instances(devices));
        self.shown.replace(Some(groups));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{BusDevices, DeviceEntry};

    fn devices(buses: &[(&str, bool, &[&str], u64)]) -> Devices {
        buses
            .iter()
            .map(|(bus, watched, entries, more)| {
                (
                    (*bus).to_string(),
                    BusDevices {
                        watched: *watched,
                        entries: entries.iter().map(|&e| e.into()).collect(),
                        more: *more,
                    },
                )
            })
            .collect()
    }

    /// One bus whose entries carry the topology paths the daemon sends, for
    /// the tests that go through `groups` rather than straight to `tree`.
    fn nested(bus: &str, entries: &[(&str, &str)]) -> Devices {
        Devices::from([(
            bus.to_string(),
            BusDevices {
                watched: true,
                entries: entries
                    .iter()
                    .map(|(text, path)| DeviceEntry {
                        text: (*text).to_string(),
                        path: (*path).to_string(),
                    })
                    .collect(),
                more: 0,
            },
        )])
    }

    fn group<'a>(groups: &'a [Group], name: &str) -> &'a Group {
        groups.iter().find(|g| g.name == name).expect("bus listed")
    }

    /// A USB row as `groups` would build it, for the tree tests.
    fn row(path: &str, id: &str, name: &str) -> (String, Row) {
        (
            path.to_string(),
            Row {
                text: format!("{id} {name}"),
                selector: Some(format!("usb:{id}")),
                name: Some(name.to_string()),
                action: Action::Add,
            },
        )
    }

    fn ids(nodes: &[Node]) -> Vec<&str> {
        nodes.iter().map(|n| n.row.text.as_str()).collect()
    }

    #[test]
    fn test_every_bus_is_grouped_in_the_order_the_daemon_names_them() {
        let d = devices(&[("usb", true, &["1050:0407 YubiKey"], 0)]);
        let groups = groups(&d, &[], &[]);
        assert_eq!(groups.len(), BUSES.len(), "every bus is listed");
        assert_eq!(groups[0].name, "USB");
        assert_eq!(groups[0].nodes.len(), 1);
        assert_eq!(groups[0].nodes[0].row.text, "1050:0407 YubiKey");
    }

    #[test]
    fn test_an_uncovered_device_offers_the_whitelist_entry_with_its_name() {
        let d = devices(&[("usb", true, &["1050:0407 YubiKey"], 0)]);
        let row = &groups(&d, &[], &[])[0].nodes[0].row;
        assert_eq!(row.action, Action::Add);
        assert_eq!(row.selector.as_deref(), Some("usb:1050:0407"));
        assert_eq!(row.name.as_deref(), Some("YubiKey"));
        // An unnamed device still carries the action, with no name to fill in.
        let bare = devices(&[("usb", true, &["1d6b:0002"], 0)]);
        let row = &groups(&bare, &[], &[])[0].nodes[0].row;
        assert_eq!(row.action, Action::Add);
        assert_eq!(row.name, None);
    }

    #[test]
    fn test_a_covered_device_says_which_covers_it() {
        let d = devices(&[
            ("usb", true, &["1050:0407 YubiKey", "1d6b:0002 Hub"], 0),
            ("pci", true, &["0000:01:00.0"], 0),
        ]);
        let allowed = vec!["usb:1d6b:0002".to_string()];
        let whitelisted = vec!["usb:1050:0407".to_string(), "pci:0000:01:00.0".to_string()];
        let groups = groups(&d, &allowed, &whitelisted);
        let usb = group(&groups, "USB");
        assert_eq!(usb.nodes[0].row.action, Action::Covered("in the whitelist"));
        assert_eq!(
            usb.nodes[1].row.action,
            Action::Covered("allowed at runtime")
        );
        assert_eq!(
            group(&groups, "PCI").nodes[0].row.action,
            Action::Covered("in the whitelist")
        );
    }

    #[test]
    fn test_the_config_wins_when_both_cover_a_device() {
        // An allowance lasts one uptime; a whitelist entry is what a person
        // would go and look at, so it is the one named.
        let d = devices(&[("usb", true, &["1050:0407 YubiKey"], 0)]);
        let both = vec!["usb:1050:0407".to_string()];
        assert_eq!(
            groups(&d, &both, &both)[0].nodes[0].row.action,
            Action::Covered("in the whitelist")
        );
    }

    #[test]
    fn test_a_listing_without_an_identity_says_why_where_there_is_a_reason() {
        // Thunderbolt lists vendor:device but is whitelisted by unique id, an
        // SD card lists manufacturer:OEM but is whitelisted by serial. The
        // event buses have no whitelist at all, so there is nothing to say.
        let d = devices(&[
            ("thunderbolt", true, &["8086:0b26 Dock"], 0),
            ("sdcard", true, &["0x03:0x5344 SD"], 0),
            ("lid", true, &["open"], 0),
            ("network", true, &["eth0: up"], 0),
        ]);
        let groups = groups(&d, &[], &[]);
        assert_eq!(
            group(&groups, "Thunderbolt").nodes[0].row.action,
            Action::Unmatched("whitelisted by unique id, which this listing does not carry")
        );
        assert_eq!(
            group(&groups, "SD card").nodes[0].row.action,
            Action::Unmatched("whitelisted by serial, which this listing does not carry")
        );
        for bus in ["Lid", "Network"] {
            assert_eq!(group(&groups, bus).nodes[0].row.action, Action::Nothing);
        }
    }

    #[test]
    fn test_a_pci_ignore_prefix_covers_everything_under_it() {
        // The daemon matches pci.ignore as a substring, so a prefix entry
        // quiets the whole bus and the panel must not offer to add it again.
        let d = devices(&[("pci", true, &["0000:01:00.0", "0000:02:00.0"], 0)]);
        let groups = groups(&d, &[], &["pci:0000:01".to_string()]);
        let pci = group(&groups, "PCI");
        assert_eq!(pci.nodes[0].row.action, Action::Covered("in the whitelist"));
        assert_eq!(pci.nodes[1].row.action, Action::Add);
    }

    #[test]
    fn test_an_unwatched_bus_is_listed_greyed_with_its_reason() {
        let d = devices(&[("usb", true, &[], 0), ("pci", false, &[], 0)]);
        let groups = groups(&d, &[], &[]);
        let pci = group(&groups, "PCI");
        assert!(!pci.watched);
        assert_eq!(
            pci.reason.as_deref(),
            Some("not watched: general.watch_pci is off")
        );
        assert!(pci.nodes.is_empty());
        assert!(group(&groups, "USB").reason.is_none());
    }

    #[test]
    fn test_a_bus_the_daemon_left_out_says_so_rather_than_guessing() {
        let groups = groups(&devices(&[]), &[], &[]);
        let usb = group(&groups, "USB");
        assert!(!usb.watched);
        assert_eq!(
            usb.reason.as_deref(),
            Some("this daemon did not report the bus")
        );
    }

    #[test]
    fn test_the_dropped_entries_are_carried_through() {
        let d = devices(&[("pci", true, &["0000:00:00.0"], 11)]);
        assert_eq!(group(&groups(&d, &[], &[]), "PCI").more, 11);
    }

    #[test]
    fn test_a_path_nests_under_the_port_it_names() {
        let nodes = tree(vec![
            row("2-3", "1d6b:0002", "Hub"),
            row("2-3.4", "1050:0407", "YubiKey"),
            row("2-3.4.3", "046d:c52b", "Receiver"),
            row("1-1", "8087:0aaa", "Bluetooth"),
        ]);
        assert_eq!(ids(&nodes), ["1d6b:0002 Hub", "8087:0aaa Bluetooth"]);
        let hub = &nodes[0];
        assert_eq!(ids(&hub.children), ["1050:0407 YubiKey"]);
        assert_eq!(ids(&hub.children[0].children), ["046d:c52b Receiver"]);
        assert!(nodes[1].children.is_empty(), "a leaf has no children");
    }

    #[test]
    fn test_a_child_whose_parent_is_missing_hangs_off_the_nearest_listed_port() {
        // The daemon caps a bus at twelve entries, so the hub in the middle
        // can be missing from a reply its children are in.
        let nodes = tree(vec![
            row("2-3", "1d6b:0002", "Hub"),
            row("2-3.4.3", "046d:c52b", "Receiver"),
        ]);
        assert_eq!(ids(&nodes), ["1d6b:0002 Hub"]);
        assert_eq!(ids(&nodes[0].children), ["046d:c52b Receiver"]);
    }

    #[test]
    fn test_a_path_with_no_parent_and_a_malformed_one_are_roots() {
        let nodes = tree(vec![
            // No ancestor in the reply at all.
            row("4-2.1", "1050:0407", "YubiKey"),
            // No path: an older daemon, or a bus with no topology.
            row("", "8087:0aaa", "Bluetooth"),
            // Nothing that looks like a port path.
            row("...", "046d:c52b", "Receiver"),
            row("2-3", "1d6b:0002", "Hub"),
        ]);
        assert_eq!(nodes.len(), 4, "every one of them is a root");
        assert!(nodes.iter().all(|n| n.children.is_empty()));
    }

    #[test]
    fn test_two_entries_on_one_path_do_not_parent_each_other() {
        let nodes = tree(vec![
            row("2-3", "1d6b:0002", "Hub"),
            row("2-3", "1d6b:0003", "Hub"),
        ]);
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn test_an_expansion_is_the_devices_behind_the_port_and_nothing_else() {
        let mut nodes = tree(vec![
            row("2-3", "1d6b:0002", "Hub"),
            row("2-3.1", "1050:0407", "YubiKey"),
            row("2-3.2", "046d:c52b", "Receiver"),
            // Another port entirely: never part of this expansion.
            row("1-1", "8087:0aaa", "Bluetooth"),
        ]);
        // One device behind the hub is already covered, so the expansion does
        // not offer it again.
        nodes[0].children[1].row.action = Action::Covered("in the whitelist");
        let rows = expansion(&nodes[0]);
        assert_eq!(
            rows.iter()
                .map(|(r, n)| (r.selector.clone().unwrap_or_default(), *n))
                .collect::<Vec<_>>(),
            [
                ("usb:1d6b:0002".to_string(), 1),
                ("usb:1050:0407".to_string(), 1)
            ]
        );
        assert!(summary(&rows).contains("Adds 2 whitelist entries"));
        // A leaf offers nothing to expand.
        assert_eq!(expansion(&nodes[1]).len(), 1);
    }

    #[test]
    fn test_a_name_is_resolved_or_the_row_says_why_it_is_not() {
        let attached = BTreeMap::from([
            (
                "usb:1050:0407".to_string(),
                Attached {
                    name: "YubiKey".to_string(),
                },
            ),
            (
                "usb:1d6b:0002".to_string(),
                // A device that named itself nothing: still attached.
                Attached {
                    name: String::new(),
                },
            ),
            ("pci:0000:01:00.0".to_string(), Attached::default()),
            // Both buses were listed in full.
            ("usb:".to_string(), Attached::default()),
            ("pci:".to_string(), Attached::default()),
        ]);
        assert_eq!(describe("usb:1050:0407", &attached), "YubiKey");
        assert_eq!(describe("usb:1d6b:0002", &attached), UNNAMED);
        assert_eq!(describe("usb:dead:beef", &attached), NOT_CONNECTED);
        // A pci prefix entry matches the way the daemon matches it.
        assert_eq!(describe("pci:0000:01", &attached), UNNAMED);
        assert_eq!(describe("pci:0000:09", &attached), NOT_CONNECTED);
        // A bus the listing cannot match on does not claim it is unplugged.
        assert!(describe("thunderbolt:abc", &attached).contains("unique id"));
        assert_eq!(describe("", &attached), NOT_LISTED);
        // Nor does a bus the reply could not list in full: a device past the
        // cap, or on a bus that failed to enumerate, is not "not connected".
        let capped = BTreeMap::new();
        assert_eq!(describe("usb:1050:0407", &capped), NOT_LISTED);
    }

    /// The stem is what makes the nesting readable, so its geometry is pinned:
    /// a full height bar for an ancestor with rows still below it, an elbow
    /// that stops at the middle on the last child and runs on when it is not.
    #[test]
    fn test_the_stem_draws_the_tree() {
        assert!(
            segments(&[], true, 40.0, 20.0).is_empty(),
            "a root has no stem"
        );

        let last = segments(&[true], true, 40.0, 20.0);
        assert_eq!(last, [(9.5, 0.0, 9.5, 10.5), (9.5, 10.5, 40.0, 10.5)]);

        let more = segments(&[true], false, 40.0, 20.0);
        assert_eq!(
            more,
            [(9.5, 0.0, 9.5, 20.0), (9.5, 10.5, 40.0, 10.5)],
            "a row with siblings below keeps its bar going to the bottom"
        );

        // Two levels down, under a grandparent that still has siblings: its
        // bar runs the whole height beside this row's own elbow.
        let deep = segments(&[true, true], true, 60.0, 20.0);
        assert_eq!(
            deep,
            [
                (9.5, 0.0, 9.5, 20.0),
                (27.5, 0.0, 27.5, 10.5),
                (27.5, 10.5, 60.0, 10.5)
            ]
        );

        // Same depth, but the grandparent was the last of its own: no bar.
        let quiet = segments(&[true, false], true, 60.0, 20.0);
        assert_eq!(quiet, [(27.5, 0.0, 27.5, 10.5), (27.5, 10.5, 60.0, 10.5)]);
    }

    #[test]
    fn test_the_attached_table_carries_the_name_and_says_which_buses_it_speaks_for() {
        let d = nested(
            "usb",
            &[
                ("1d6b:0002 Root", "usb2"),
                ("05e3:0610 Hub", "2-3"),
                ("1050:0407 YubiKey", "2-3.4"),
            ],
        );
        let attached = attached(&d);
        assert_eq!(
            attached.get("usb:1050:0407"),
            Some(&Attached {
                name: "YubiKey".to_string(),
            })
        );
        assert_eq!(attached["usb:05e3:0610"].name, "Hub");
        // The bus was watched and listed whole, so a miss on it is a device
        // that is really not there (K1).
        assert!(attached.contains_key("usb:"));
        assert_eq!(describe("usb:dead:beef", &attached), NOT_CONNECTED);

        // Capped, so it cannot speak for what it did not list.
        let mut capped = nested("usb", &[("1d6b:0002 Root", "usb2")]);
        capped.get_mut("usb").unwrap().more = 3;
        let capped = super::attached(&capped);
        assert!(!capped.contains_key("usb:"));
        assert_eq!(describe("usb:dead:beef", &capped), NOT_LISTED);
    }

    #[test]
    fn test_groups_nests_the_paths_the_daemon_actually_sends() {
        // Straight through `groups`, not `tree`: this is the seam where the
        // wire shape and the panel can drift apart.
        let d = nested(
            "usb",
            &[
                ("1d6b:0002 Root hub", "usb2"),
                ("05e3:0610 Hub", "2-3"),
                ("1050:0407 YubiKey", "2-3.4"),
            ],
        );
        let usb = group(&groups(&d, &[], &[]), "USB").clone();
        assert_eq!(ids(&usb.nodes), ["1d6b:0002 Root hub"]);
        assert_eq!(ids(&usb.nodes[0].children), ["05e3:0610 Hub"]);
        assert_eq!(
            ids(&usb.nodes[0].children[0].children),
            ["1050:0407 YubiKey"]
        );
        // Expanding the root hub reaches everything behind it.
        assert_eq!(expansion(&usb.nodes[0]).len(), 3);
    }

    #[test]
    fn test_a_bus_with_no_topology_stays_flat() {
        let d = nested("network", &[("eth0: up", ""), ("wlan0: up", "")]);
        let net = group(&groups(&d, &[], &[]), "Network").clone();
        assert_eq!(net.nodes.len(), 2);
        assert!(net.nodes.iter().all(|n| n.children.is_empty()));
    }

    #[test]
    fn test_a_sibling_port_is_not_a_parent_because_it_shares_a_prefix() {
        // 2-30.1 hangs off port 30, not off port 3. A prefix scan would get
        // this wrong and whitelist a device on another port.
        let nodes = tree(vec![
            row("2-3", "05e3:0610", "Hub"),
            row("2-30.1", "1050:0407", "YubiKey"),
            row("2-3.4", "046d:c52b", "Receiver"),
        ]);
        assert_eq!(ids(&nodes), ["05e3:0610 Hub", "1050:0407 YubiKey"]);
        assert_eq!(ids(&nodes[0].children), ["046d:c52b Receiver"]);
        assert!(nodes[1].children.is_empty());
    }

    #[test]
    fn test_a_pci_bridge_chain_nests_on_its_path() {
        let d = nested(
            "pci",
            &[
                ("0000:00:1c.0", "pci0000:00/0000:00:1c.0"),
                ("0000:01:00.0", "pci0000:00/0000:00:1c.0/0000:01:00.0"),
            ],
        );
        let pci = group(&groups(&d, &[], &[]), "PCI").clone();
        assert_eq!(ids(&pci.nodes), ["0000:00:1c.0"]);
        assert_eq!(ids(&pci.nodes[0].children), ["0000:01:00.0"]);
    }

    #[test]
    fn test_the_instance_count_is_how_many_of_a_model_are_plugged_in() {
        // The ceiling a row's whitelist action may raise an entry to, so it
        // can cover a second receiver and no more than that (K2b).
        let d = nested(
            "usb",
            &[
                ("1d6b:0002 Root", "usb2"),
                ("05e3:0610 Hub", "2-3"),
                ("046d:c52b Receiver", "2-3.4"),
                ("046d:c52b Receiver", "2-3.5"),
            ],
        );
        let counts = instances(&d);
        assert_eq!(counts.get("usb:046d:c52b"), Some(&2), "both, at any depth");
        assert_eq!(counts.get("usb:05e3:0610"), Some(&1));
        assert_eq!(counts.get("usb:dead:beef"), None);
    }

    #[test]
    fn test_a_whitelist_entry_covers_as_many_devices_as_it_says() {
        // The daemon allows `count` instances of a model, and the caller
        // spells that as one copy of the selector per instance. A second
        // identical dongle an entry for one does not cover still kills, so
        // the panel must offer to add it rather than call it covered.
        let d = devices(&[(
            "usb",
            true,
            &["046d:c52b Receiver", "046d:c52b Receiver"],
            0,
        )]);
        let one = vec!["usb:046d:c52b".to_string()];
        let usb = group(&groups(&d, &[], &one), "USB").clone();
        assert_eq!(usb.nodes[0].row.action, Action::Covered("in the whitelist"));
        assert_eq!(usb.nodes[1].row.action, Action::Add);

        let two = vec![one[0].clone(), one[0].clone()];
        let usb = group(&groups(&d, &[], &two), "USB").clone();
        assert!(
            usb.nodes
                .iter()
                .all(|n| n.row.action == Action::Covered("in the whitelist"))
        );
    }

    #[test]
    fn test_two_of_one_model_are_one_entry_that_stands_for_both() {
        // The daemon does not deduplicate, so a hub with two identical
        // dongles behind it lists the same id twice. One whitelist entry
        // covers them, and only if it says there are two.
        let nodes = tree(vec![
            row("2-3", "05e3:0610", "Hub"),
            row("2-3.1", "046d:c52b", "Receiver"),
            row("2-3.2", "046d:c52b", "Receiver"),
        ]);
        let rows = expansion(&nodes[0]);
        assert_eq!(rows.len(), 2, "one row per identity");
        assert_eq!(rows[1].1, 2, "and it stands for both devices");

        // The entry has to cover every device of the model behind the port,
        // covered ones included: the daemon counts what is plugged in, not
        // what is left to add.
        let mut covered = nodes.clone();
        covered[0].children[0].row.action = Action::Covered("in the whitelist");
        let rows = expansion(&covered[0]);
        assert_eq!(rows.len(), 2, "one row per identity");
        assert_eq!(rows[1].1, 2, "both devices still count towards the entry");
        let summary = summary(&rows);
        assert!(summary.contains("Adds 2 whitelist entries"), "{summary}");
        assert!(summary.contains("046d:c52b Receiver x2"), "{summary}");
    }
}
