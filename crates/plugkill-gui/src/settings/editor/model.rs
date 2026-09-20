//! The editor's pure part: what it opens on, what it would emit, and the
//! difference between the two. No GTK, no writing, and the only file it
//! touches is the config it reads (C2).

use plugkill_core::allowances::DeviceRef;
use plugkill_core::config::{
    Config, SdCardWhitelistEntry, ThunderboltWhitelistEntry, WhitelistEntry, emit, parse,
};
use std::collections::HashMap;

/// The editor's sections, top to bottom (B1). Each one carries the switches
/// and fields of its config section; the per-bus watch flags live with their
/// bus rather than all together in General, which is where the config keeps
/// them.
pub const SECTIONS: [&str; 9] = [
    "General",
    "USB",
    "Thunderbolt",
    "SD card",
    "Power",
    "Network",
    "Lid",
    "PCI",
    "Display",
];

/// The config the window opened on, and the line that says where it came
/// from. A config that cannot be read opens on the defaults and says so,
/// rather than showing an empty config as the truth (C2).
pub struct Loaded {
    pub config: Config,
    pub origin: String,
    /// False when the file could not be read or did not parse, and `config`
    /// is therefore the defaults rather than what root is running. Nothing
    /// may then be stated as the daemon's.
    pub read: bool,
}

/// Read the file the daemon reported in `status` (C1). Never writes, and
/// never fails: anything that goes wrong becomes the origin line.
pub fn load(path: &str) -> Loaded {
    let defaults = |origin: String| Loaded {
        config: Config::default(),
        origin,
        read: false,
    };
    if path.is_empty() {
        return defaults(
            "No config file: the daemon has not said which one it loaded, so this starts \
             from the defaults."
                .to_string(),
        );
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            return defaults(format!(
                "Cannot read {path}: {e}. Starting from the defaults."
            ));
        }
    };
    match parse(&text) {
        Ok(config) => Loaded {
            config,
            origin: format!("Loaded {path}, the file the daemon is running."),
            read: true,
        },
        Err(e) => defaults(format!(
            "{path} did not parse: {e}. Starting from the defaults."
        )),
    }
}

/// One of the lists the editor shows as a table: the three whitelists, and
/// the three ignore lists that work the same way on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Usb,
    Thunderbolt,
    SdCard,
    Pci,
    Network,
    Display,
}

/// The rows of one table: the entry as it is stored, and the label to show.
/// They differ only where a USB entry allows more than one of the device.
pub fn list_entries(config: &Config, kind: ListKind) -> Vec<(String, String)> {
    let plain = |v: &Vec<String>| v.iter().map(|s| (s.clone(), s.clone())).collect();
    match kind {
        ListKind::Usb => config
            .whitelist
            .devices
            .iter()
            .map(|e| {
                let entry = format!("{}:{}", e.vendor_id, e.product_id);
                let label = if e.count > 1 {
                    format!("{entry}  x{}", e.count)
                } else {
                    entry.clone()
                };
                (entry, label)
            })
            .collect(),
        ListKind::Thunderbolt => config
            .thunderbolt_whitelist
            .devices
            .iter()
            .map(|e| (e.unique_id.clone(), e.unique_id.clone()))
            .collect(),
        ListKind::SdCard => config
            .sdcard_whitelist
            .devices
            .iter()
            .map(|e| (e.serial.clone(), e.serial.clone()))
            .collect(),
        ListKind::Pci => plain(&config.pci.ignore),
        ListKind::Network => plain(&config.network.interfaces),
        ListKind::Display => plain(&config.display.ignore),
    }
}

/// The string the config stores an entry under, or why it cannot store it.
fn normalise(kind: ListKind, text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Type an entry first.".to_string());
    }
    // The USB whitelist matches on two fields, so this one shape has to hold.
    if kind != ListKind::Usb {
        return Ok(text.to_string());
    }
    match format!("usb:{text}").parse::<DeviceRef>() {
        Ok(DeviceRef::Usb(id)) => Ok(format!("{}:{}", id.vendor_id, id.product_id)),
        _ => Err(format!(
            "'{text}' is not a vendor:product pair like 1d6b:0002."
        )),
    }
}

/// Add one entry that stands for `n` devices of the same model, for a hub
/// expansion (K2b). An entry the config already holds is not a failure here,
/// it is raised to cover `n` instances: the daemon allows that many by the
/// summed `count`, so without the raise the second device still kills.
/// `list_add` keeps refusing a duplicate, because a person typing one twice
/// means something else.
pub fn list_add_instances(
    config: &mut Config,
    kind: ListKind,
    text: &str,
    n: u32,
) -> Result<String, String> {
    let entry = normalise(kind, text)?;
    if !list_entries(config, kind).iter().any(|(e, _)| *e == entry) {
        list_add(config, kind, &entry)?;
    }
    if kind == ListKind::Usb
        && let Some(listed) = config
            .whitelist
            .devices
            .iter_mut()
            .find(|w| format!("{}:{}", w.vendor_id, w.product_id) == entry)
    {
        listed.count = listed.count.max(n);
    }
    Ok(entry)
}

/// Cover one more device of a model, for a row a panel offered because the
/// whitelist does not reach that device yet (K2b). A USB entry the config
/// already holds is raised by one instance rather than refused: the daemon
/// allows devices of a model by the entry's summed `count`, so the second
/// receiver only stops being a violation once the entry says two. `present`
/// is how many the daemon lists, and the raise never passes it, so a click
/// cannot authorise a device nobody has plugged in. Every other list matches
/// one device per entry, where a duplicate is the mistake `list_add` calls it.
pub fn list_cover_one(
    config: &mut Config,
    kind: ListKind,
    text: &str,
    present: u32,
) -> Result<String, String> {
    let entry = normalise(kind, text)?;
    let listed = list_entries(config, kind).iter().any(|(e, _)| *e == entry);
    if !listed || kind != ListKind::Usb {
        return list_add(config, kind, &entry);
    }
    if let Some(e) = config
        .whitelist
        .devices
        .iter_mut()
        .find(|w| format!("{}:{}", w.vendor_id, w.product_id) == entry)
        && e.count < present
    {
        e.count += 1;
    }
    Ok(entry)
}

/// Add one entry, typed or handed over by another panel. Only the shapes the
/// config cannot store are refused here; everything else is left to the
/// daemon's own loader, which B6 runs before any output is shown.
pub fn list_add(config: &mut Config, kind: ListKind, text: &str) -> Result<String, String> {
    let entry = normalise(kind, text)?;
    if list_entries(config, kind).iter().any(|(e, _)| *e == entry) {
        return Err(format!("{entry} is already listed."));
    }
    match kind {
        ListKind::Usb => {
            let (vendor_id, product_id) = entry.split_once(':').unwrap_or_default();
            config.whitelist.devices.push(WhitelistEntry {
                vendor_id: vendor_id.to_string(),
                product_id: product_id.to_string(),
                count: 1,
            });
        }
        ListKind::Thunderbolt => {
            config
                .thunderbolt_whitelist
                .devices
                .push(ThunderboltWhitelistEntry {
                    unique_id: entry.clone(),
                })
        }
        ListKind::SdCard => config.sdcard_whitelist.devices.push(SdCardWhitelistEntry {
            serial: entry.clone(),
        }),
        ListKind::Pci => config.pci.ignore.push(entry.clone()),
        ListKind::Network => config.network.interfaces.push(entry.clone()),
        ListKind::Display => config.display.ignore.push(entry.clone()),
    }
    Ok(entry)
}

/// Drop one entry, by the string `list_entries` stored it under.
pub fn list_remove(config: &mut Config, kind: ListKind, entry: &str) {
    match kind {
        ListKind::Usb => config
            .whitelist
            .devices
            .retain(|e| format!("{}:{}", e.vendor_id, e.product_id) != entry),
        ListKind::Thunderbolt => config
            .thunderbolt_whitelist
            .devices
            .retain(|e| e.unique_id != entry),
        ListKind::SdCard => config
            .sdcard_whitelist
            .devices
            .retain(|e| e.serial != entry),
        ListKind::Pci => config.pci.ignore.retain(|e| e != entry),
        ListKind::Network => config.network.interfaces.retain(|e| e != entry),
        ListKind::Display => config.display.ignore.retain(|e| e != entry),
    }
}

/// Split a `<bus>:<identity>` selector, as the devices and violations panels
/// hand it over, into the table it belongs in and the entry to put there.
pub fn selector_entry(selector: &str) -> Result<(ListKind, String), String> {
    Ok(match selector.parse::<DeviceRef>()? {
        DeviceRef::Usb(id) => (ListKind::Usb, format!("{}:{}", id.vendor_id, id.product_id)),
        DeviceRef::Thunderbolt(id) => (ListKind::Thunderbolt, id.unique_id),
        DeviceRef::SdCard(id) => (ListKind::SdCard, id.serial),
        DeviceRef::Pci(sel) => (ListKind::Pci, sel),
        // The config has no display whitelist: it ignores connectors by name,
        // which is not what a monitor id says.
        DeviceRef::Display(_) => {
            return Err(
                "A monitor can only be allowed at runtime. The config ignores connectors \
                 by name, such as eDP-1."
                    .to_string(),
            );
        }
    })
}

/// The `<bus>:<identity>` selector one row of a table stands for, the inverse
/// of `selector_entry`, so a row can be looked up in the devices reply (K1).
/// None for the two lists that name no device: a network interface and a
/// display connector are not identities any listing carries.
///
/// Not parsed back through `DeviceRef`: a pci entry may be a prefix of an
/// address, which is how the daemon matches it and not a selector on its own.
pub fn entry_selector(kind: ListKind, entry: &str) -> Option<String> {
    let bus = match kind {
        ListKind::Usb => "usb",
        ListKind::Thunderbolt => "thunderbolt",
        ListKind::SdCard => "sdcard",
        ListKind::Pci => "pci",
        ListKind::Network | ListKind::Display => return None,
    };
    Some(format!("{bus}:{entry}"))
}

/// One line of the side by side view, marked when the other side has no such
/// line (B5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub changed: bool,
}

/// A plain line by line comparison: a line is marked unless the other side
/// has one like it still unspoken for. Both sides come out of the same
/// emitter, so only an edit can make a line differ.
fn diff(left: &str, right: &str) -> (Vec<Line>, Vec<Line>) {
    fn counts(text: &str) -> HashMap<&str, usize> {
        let mut map = HashMap::new();
        for line in text.lines() {
            *map.entry(line).or_insert(0) += 1;
        }
        map
    }
    fn mark(text: &str, other: &mut HashMap<&str, usize>) -> Vec<Line> {
        text.lines()
            .map(|line| {
                let matched = match other.get_mut(line) {
                    Some(n) if *n > 0 => {
                        *n -= 1;
                        true
                    }
                    _ => false,
                };
                Line {
                    text: line.to_string(),
                    changed: !matched,
                }
            })
            .collect()
    }
    (
        mark(left, &mut counts(right)),
        mark(right, &mut counts(left)),
    )
}

/// What the output pane shows. On `error` nothing else is filled: a config the
/// daemon would reject is never offered as output (B6).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub toml: String,
    pub nix: String,
    pub loaded_lines: Vec<Line>,
    pub emitted_lines: Vec<Line>,
    /// Values the daemon would silently clamp, as "field: was becomes is".
    pub clamped: Vec<String>,
    pub error: Option<String>,
}

/// The whole output pane from one in-memory config. The TOML is parsed back
/// with the daemon's loader first, and both panes are emitted from what came
/// back, so what is shown is what the daemon would actually run.
pub fn build(loaded: &Config, edited: &Config) -> Output {
    let failed = |e: plugkill_core::error::Error| Output {
        error: Some(e.to_string()),
        ..Output::default()
    };
    let (toml, checked) = match emit::to_toml_checked(edited) {
        Ok(pair) => pair,
        Err(e) => return failed(e),
    };
    let nix = match emit::to_nix(&checked) {
        Ok(nix) => nix,
        Err(e) => return failed(e),
    };
    let was = emit::to_toml(loaded).unwrap_or_default();
    let (loaded_lines, emitted_lines) = diff(&was, &toml);
    Output {
        toml,
        nix,
        loaded_lines,
        emitted_lines,
        clamped: clamped(edited, &checked),
        error: None,
    }
}

/// The values the loader moved into range on its way past. Every clamp the
/// daemon does is on one of these four.
fn clamped(before: &Config, after: &Config) -> Vec<String> {
    [
        (
            "general.sleep_ms",
            before.general.sleep_ms,
            after.general.sleep_ms,
        ),
        (
            "power.grace_secs",
            before.power.grace_secs,
            after.power.grace_secs,
        ),
        (
            "network.grace_secs",
            before.network.grace_secs,
            after.network.grace_secs,
        ),
        (
            "lid.grace_secs",
            before.lid.grace_secs,
            after.lid.grace_secs,
        ),
    ]
    .into_iter()
    .filter(|(_, was, is)| was != is)
    .map(|(field, was, is)| format!("{field}: {was} becomes {is}"))
    .collect()
}

/// Said above both output panes, and in place of the read-only section, when
/// the running config could not be read. The defaults behind them are not the
/// daemon's, and `[commands]` and `[destruction]` emitted empty would erase
/// what root runs.
pub const UNREAD_PREFIX: &str = "# placeholder: the running config could not be read; \
[commands] and\n# [destruction] here are NOT the daemon's.\n\n";

/// Said in place of `readonly_text` when the config could not be read.
pub const UNREAD_READONLY: &str =
    "Kill commands and shredding: unknown, the config file could not be read.";

/// The whitelists and ignore lists as `<bus>:<identity>` selectors, for the
/// devices panel to mark what is already covered (D3). The ids are emitted
/// exactly as the config holds them: the daemon matches them verbatim too
/// (plugkill/src/main.rs build_usb_whitelist), so normalising here would
/// report a dead entry as covering a device.
/// A USB entry stands for `count` devices, so it is repeated that many times:
/// the daemon allows the summed count (plugkill/src/main.rs build_usb_whitelist),
/// so a second device of one model is covered only by an entry that says two.
pub fn selectors(config: &Config) -> Vec<String> {
    let mut out: Vec<String> = config
        .whitelist
        .devices
        .iter()
        .flat_map(|e| {
            std::iter::repeat_n(
                format!("usb:{}:{}", e.vendor_id, e.product_id),
                e.count as usize,
            )
        })
        .collect();
    for (kind, bus) in [
        (ListKind::Thunderbolt, "thunderbolt"),
        (ListKind::SdCard, "sdcard"),
        (ListKind::Pci, "pci"),
    ] {
        out.extend(
            list_entries(config, kind)
                .into_iter()
                .map(|(entry, _)| format!("{bus}:{entry}")),
        );
    }
    out
}

/// `[commands]` and `[destruction]` as text, for the read-only section (B7).
/// They come from the loaded config, because nothing on screen can change
/// them.
pub fn readonly_text(config: &Config) -> String {
    let mut out = String::new();
    if config.commands.kill_commands.is_empty() {
        out.push_str("Kill commands: none set, so the daemon uses its built-in poweroff.\n");
    } else {
        out.push_str("Kill commands:\n");
        for cmd in &config.commands.kill_commands {
            out.push_str(&format!("  {}\n", cmd.join(" ")));
        }
    }
    let list = |paths: &Vec<std::path::PathBuf>| {
        if paths.is_empty() {
            "none".to_string()
        } else {
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let d = &config.destruction;
    out.push_str(&format!("Files shredded: {}\n", list(&d.files_to_remove)));
    out.push_str(&format!(
        "Folders shredded: {}\n",
        list(&d.folders_to_remove)
    ));
    out.push_str(&format!(
        "Melt self: {}. Sync first: {}. Wipe swap: {}{}",
        yes_no(d.melt_self),
        yes_no(d.do_sync),
        yes_no(d.do_wipe_swap),
        d.swap_device
            .as_ref()
            .map_or(String::new(), |s| format!(" ({s})")),
    ));
    out
}

fn yes_no(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_selectors_are_the_config_ids_in_bus_order() {
        let mut c = Config::default();
        list_add(&mut c, ListKind::Usb, "1d6b:0002").expect("a usb entry");
        list_add(&mut c, ListKind::Pci, "0000:01:00.0").expect("a pci entry");
        assert_eq!(selectors(&c), ["usb:1d6b:0002", "pci:0000:01:00.0"]);

        // Verbatim, uppercase included: the daemon looks a whitelist entry up
        // by the exact string too, so an entry that matches nothing there must
        // not read as covering a device here.
        let mut upper = Config::default();
        upper.whitelist.devices.push(WhitelistEntry {
            vendor_id: "1D6B".into(),
            product_id: "0002".into(),
            count: 1,
        });
        assert_eq!(selectors(&upper), ["usb:1D6B:0002"]);

        // An entry for two devices covers two: the daemon allows them by the
        // summed count, so one copy per instance is what the panel matches on.
        let mut two = Config::default();
        two.whitelist.devices.push(WhitelistEntry {
            vendor_id: "046d".into(),
            product_id: "c52b".into(),
            count: 2,
        });
        assert_eq!(selectors(&two), ["usb:046d:c52b", "usb:046d:c52b"]);
    }

    #[test]
    fn test_an_expansion_counts_the_devices_it_stands_for() {
        let mut c = Config::default();
        // Two of one model behind a hub: one entry, allowed twice. Without
        // the count the daemon kills on the second one.
        assert_eq!(
            list_add_instances(&mut c, ListKind::Usb, "046d:c52b", 2),
            Ok("046d:c52b".to_string())
        );
        assert_eq!(
            c.whitelist.devices,
            vec![WhitelistEntry {
                vendor_id: "046d".into(),
                product_id: "c52b".into(),
                count: 2,
            }]
        );
        // Adding it again is not a failure: it is already covered, and a
        // count already high enough is left where it is.
        assert!(list_add_instances(&mut c, ListKind::Usb, "046d:c52b", 1).is_ok());
        assert_eq!(c.whitelist.devices[0].count, 2);
        assert!(list_add_instances(&mut c, ListKind::Usb, "046d:c52b", 3).is_ok());
        assert_eq!(c.whitelist.devices[0].count, 3);
        // A person typing the same entry twice still hears about it.
        assert!(list_add(&mut c, ListKind::Usb, "046d:c52b").is_err());
        // A shape the config cannot store is refused either way.
        assert!(list_add_instances(&mut c, ListKind::Usb, "nonsense", 1).is_err());
        // A list with no count is added once and never duplicated.
        assert!(list_add_instances(&mut c, ListKind::Pci, "0000:01:00.0", 2).is_ok());
        assert!(list_add_instances(&mut c, ListKind::Pci, "0000:01:00.0", 2).is_ok());
        assert_eq!(c.pci.ignore, ["0000:01:00.0"]);
    }

    #[test]
    fn test_a_second_device_of_one_model_raises_the_entry_it_does_not_refuse() {
        // Two identical receivers on one hub. The panel offers the second row
        // because an entry for one does not cover it; the click has to make
        // it legal, not report that the model is already there.
        let mut c = Config::default();
        assert_eq!(
            list_cover_one(&mut c, ListKind::Usb, "046d:c52b", 2),
            Ok("046d:c52b".to_string())
        );
        assert_eq!(c.whitelist.devices[0].count, 1, "the first covers one");
        assert!(list_cover_one(&mut c, ListKind::Usb, "046d:c52b", 2).is_ok());
        assert_eq!(c.whitelist.devices[0].count, 2, "the second raises it");
        assert_eq!(c.whitelist.devices.len(), 1, "one entry, not two");

        // And stops there: a click may never authorise a device nobody has
        // plugged in, however often it is clicked.
        assert!(list_cover_one(&mut c, ListKind::Usb, "046d:c52b", 2).is_ok());
        assert_eq!(c.whitelist.devices[0].count, 2);

        // A list matching one device per entry keeps refusing a duplicate.
        assert!(list_cover_one(&mut c, ListKind::Pci, "0000:01:00.0", 2).is_ok());
        assert!(list_cover_one(&mut c, ListKind::Pci, "0000:01:00.0", 2).is_err());
        // As does a shape the config cannot store.
        assert!(list_cover_one(&mut c, ListKind::Usb, "a stick", 2).is_err());
    }

    #[test]
    fn test_a_config_that_did_not_load_says_it_was_not_read() {
        // Nothing may be stated as the daemon's when the file never opened.
        let loaded = load("/nonexistent/plugkill.toml");
        assert!(!loaded.read);
        assert!(!load("").read);
    }

    #[test]
    fn test_the_tables_edit_the_config_a_person_clicked() {
        let mut c = Config::default();
        // A typed entry, in the case a person types it.
        assert_eq!(
            list_add(&mut c, ListKind::Usb, " 1D6B:0002 "),
            Ok("1d6b:0002".to_string())
        );
        assert_eq!(
            c.whitelist.devices,
            vec![WhitelistEntry {
                vendor_id: "1d6b".into(),
                product_id: "0002".into(),
                count: 1
            }]
        );
        assert!(
            list_add(&mut c, ListKind::Usb, "1d6b:0002").is_err(),
            "twice"
        );
        assert!(list_add(&mut c, ListKind::Usb, "a stick").is_err());
        assert!(list_add(&mut c, ListKind::Usb, "  ").is_err());

        list_add(&mut c, ListKind::Network, "eth0").expect("interface");
        list_add(&mut c, ListKind::Pci, "0000:01:00.0").expect("pci selector");
        assert_eq!(c.network.interfaces, vec!["eth0".to_string()]);
        assert_eq!(c.pci.ignore, vec!["0000:01:00.0".to_string()]);

        list_remove(&mut c, ListKind::Usb, "1d6b:0002");
        list_remove(&mut c, ListKind::Network, "eth0");
        assert!(c.whitelist.devices.is_empty());
        assert!(c.network.interfaces.is_empty());
    }

    #[test]
    fn test_a_usb_entry_for_more_than_one_device_says_so_but_stores_the_pair() {
        let mut c = Config::default();
        c.whitelist.devices.push(WhitelistEntry {
            vendor_id: "1d6b".into(),
            product_id: "0002".into(),
            count: 3,
        });
        assert_eq!(
            list_entries(&c, ListKind::Usb),
            vec![("1d6b:0002".to_string(), "1d6b:0002  x3".to_string())]
        );
    }

    #[test]
    fn test_a_selector_from_another_panel_lands_in_its_own_table() {
        assert_eq!(
            selector_entry("usb:1D6B:0002"),
            Ok((ListKind::Usb, "1d6b:0002".to_string()))
        );
        assert_eq!(
            selector_entry("thunderbolt:0001-1234"),
            Ok((ListKind::Thunderbolt, "0001-1234".to_string()))
        );
        assert_eq!(
            selector_entry("sdcard:0x1234abcd"),
            Ok((ListKind::SdCard, "0x1234abcd".to_string()))
        );
        assert_eq!(
            selector_entry("pci:0000:01:00.0"),
            Ok((ListKind::Pci, "0000:01:00.0".to_string()))
        );
        // A monitor and an event have no config entry to make.
        assert!(selector_entry("display:SAM:772d:811021873").is_err());
        assert!(selector_entry("lid:closed").is_err());
    }

    #[test]
    fn test_a_table_row_names_the_selector_it_came_from() {
        // The devices reply is keyed by selector, so a row has to reach it by
        // the same string it arrived as (K1).
        for selector in [
            "usb:1d6b:0002",
            "thunderbolt:0001-1234",
            "sdcard:0x1234abcd",
            "pci:0000:01:00.0",
        ] {
            let (kind, entry) = selector_entry(selector).expect("a table takes it");
            assert_eq!(entry_selector(kind, &entry).as_deref(), Some(selector));
        }
        // Neither of these names a device the reply could know.
        assert_eq!(entry_selector(ListKind::Network, "eth0"), None);
        assert_eq!(entry_selector(ListKind::Display, "eDP-1"), None);
    }

    #[test]
    fn test_the_diff_marks_only_the_lines_that_differ() {
        let (left, right) = diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(
            left.iter().map(|l| l.changed).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert_eq!(
            right.iter().map(|l| l.changed).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert_eq!(right[1].text, "B");

        // An added line marks itself and leaves the rest alone, which a
        // positional compare would not manage.
        let (left, right) = diff("a\nc\n", "a\nb\nc\n");
        assert!(left.iter().all(|l| !l.changed));
        assert_eq!(
            right.iter().map(|l| l.changed).collect::<Vec<_>>(),
            [false, true, false]
        );
    }

    #[test]
    fn test_an_edit_shows_up_in_the_output_and_nothing_else_does() {
        let loaded = Config::default();
        let mut edited = loaded.clone();
        edited.general.dry_run = true;
        let out = build(&loaded, &edited);
        assert!(out.error.is_none());
        assert!(out.toml.contains("dry_run = true"));
        assert!(out.nix.contains("dry_run = true;"));
        let marked: Vec<&str> = out
            .emitted_lines
            .iter()
            .filter(|l| l.changed)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(marked, ["dry_run = true"]);
        assert_eq!(
            out.loaded_lines
                .iter()
                .filter(|l| l.changed)
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["dry_run = false"]
        );
    }

    #[test]
    fn test_a_config_the_daemon_would_reject_is_never_offered() {
        let mut edited = Config::default();
        edited.whitelist.devices.push(WhitelistEntry {
            vendor_id: "zzzz".into(),
            product_id: "0002".into(),
            count: 1,
        });
        let out = build(&Config::default(), &edited);
        let error = out.error.expect("the loader rejects a non-hex vendor id");
        assert!(error.contains("vendor_id"), "{error}");
        assert!(out.toml.is_empty(), "nothing is offered to copy");
        assert!(out.nix.is_empty());
    }

    #[test]
    fn test_a_clamped_value_is_shown_the_way_the_daemon_would_apply_it() {
        let mut edited = Config::default();
        edited.general.sleep_ms = 20;
        edited.lid.grace_secs = 900;
        let out = build(&Config::default(), &edited);
        assert_eq!(
            out.clamped,
            [
                "general.sleep_ms: 20 becomes 50".to_string(),
                "lid.grace_secs: 900 becomes 300".to_string()
            ]
        );
        assert!(out.toml.contains("sleep_ms = 50"), "{}", out.toml);
        assert!(out.toml.contains("grace_secs = 300"));
        assert!(!out.toml.contains("sleep_ms = 20"));
    }

    #[test]
    fn test_an_unreadable_config_opens_on_defaults_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nowhere.toml");
        let loaded = load(&missing.display().to_string());
        assert_eq!(loaded.config, Config::default());
        assert!(
            loaded.origin.starts_with("Cannot read"),
            "{}",
            loaded.origin
        );

        // An older daemon reports no path at all.
        let none = load("");
        assert_eq!(none.config, Config::default());
        assert!(none.origin.contains("defaults"));

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[general]\nsleep_ms = \"soon\"\n").unwrap();
        let loaded = load(&bad.display().to_string());
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.origin.contains("did not parse"), "{}", loaded.origin);
    }

    #[test]
    fn test_a_config_that_reads_is_the_one_the_editor_opens_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugkill.toml");
        std::fs::write(
            &path,
            "[general]\nwatch_power = true\n\n[[whitelist.devices]]\n\
             vendor_id = \"1d6b\"\nproduct_id = \"0002\"\n",
        )
        .unwrap();
        let loaded = load(&path.display().to_string());
        assert!(loaded.config.general.watch_power);
        assert_eq!(
            list_entries(&loaded.config, ListKind::Usb),
            vec![("1d6b:0002".to_string(), "1d6b:0002".to_string())]
        );
        assert!(loaded.origin.contains("the daemon is running"));
    }

    #[test]
    fn test_the_read_only_section_names_what_root_edits() {
        let mut c = Config::default();
        let text = readonly_text(&c);
        assert!(text.contains("none set"), "{text}");
        assert!(text.contains("Files shredded: none"));

        c.commands.kill_commands = vec![vec!["/sbin/poweroff".into(), "-f".into()]];
        c.destruction.files_to_remove = vec!["/root/secret".into()];
        let text = readonly_text(&c);
        assert!(text.contains("/sbin/poweroff -f"), "{text}");
        assert!(text.contains("Files shredded: /root/secret"));
    }
}
