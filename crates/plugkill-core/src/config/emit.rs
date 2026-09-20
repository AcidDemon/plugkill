//! Turn a `Config` back into text: TOML in the shape the daemon's loader
//! accepts, and the Nix attrset a NixOS user pastes into
//! `services.plugkill.settings`. Both are generated from the same value, so
//! they cannot disagree.
//!
//! Every field is emitted, defaults included. A config that leaves out what it
//! does not care about is nicer to write by hand, but the window's output is
//! read to answer "what is set", and an omitted key answers nothing.
//!
//! `[commands]` and `[destruction]` are emitted exactly as they were loaded.
//! The window cannot edit them, so they pass through untouched.

use super::Config;
use crate::error::Error;
use toml::Value;

/// Emit the config as TOML that `config::parse` accepts.
pub fn to_toml(config: &Config) -> Result<String, Error> {
    toml::to_string(config)
        .map_err(|e| Error::Config(format!("failed to emit config as TOML: {e}")))
}

/// Emit the config as TOML, having run it through the loader first, and hand
/// back the config the loader produced. The loader clamps `sleep_ms` and the
/// grace periods silently, so this is what to show a user: text that matches
/// the values the daemon would actually run.
pub fn to_toml_checked(config: &Config) -> Result<(String, Config), Error> {
    let back = super::parse(&to_toml(config)?)?;
    Ok((to_toml(&back)?, back))
}

/// Emit the config as a Nix attrset for `services.plugkill.settings`. Keys come
/// out alphabetical, the order `toml::Value` keeps them in.
///
/// `general.require_auth` is left out of the attrset: `nix/module.nix` asserts
/// that key is absent from `services.plugkill.settings` because
/// `services.plugkill.requireAuth` writes it. It goes out as a comment naming
/// that option instead, so the output can be pasted as it stands.
pub fn to_nix(config: &Config) -> Result<String, Error> {
    let mut value = Value::try_from(config)
        .map_err(|e| Error::Config(format!("failed to emit config as Nix: {e}")))?;
    if let Some(general) = value.get_mut("general").and_then(Value::as_table_mut) {
        general.remove("require_auth");
    }
    let mut out = format!(
        "# services.plugkill.requireAuth = {};\n",
        config.general.require_auth
    );
    write_value(&value, 0, &mut out);
    out.push('\n');
    Ok(out)
}

fn write_value(value: &Value, indent: usize, out: &mut String) {
    match value {
        Value::String(s) => out.push_str(&nix_string(s)),
        Value::Integer(i) => out.push_str(&i.to_string()),
        Value::Float(f) => out.push_str(&f.to_string()),
        Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        // No config field is a datetime. Emit one as a string rather than panic
        // if that ever changes.
        Value::Datetime(d) => out.push_str(&nix_string(&d.to_string())),
        Value::Array(items) => write_array(items, indent, out),
        Value::Table(table) => {
            if table.is_empty() {
                out.push_str("{ }");
                return;
            }
            out.push_str("{\n");
            for (key, item) in table {
                pad(indent + 2, out);
                out.push_str(&nix_key(key));
                out.push_str(" = ");
                write_value(item, indent + 2, out);
                out.push_str(";\n");
            }
            pad(indent, out);
            out.push('}');
        }
    }
}

fn write_array(items: &[Value], indent: usize, out: &mut String) {
    if items.is_empty() {
        out.push_str("[ ]");
        return;
    }
    let flat = !items
        .iter()
        .any(|i| matches!(i, Value::Array(_) | Value::Table(_)));
    if flat {
        out.push('[');
        for item in items {
            out.push(' ');
            write_value(item, indent, out);
        }
        out.push_str(" ]");
        return;
    }
    out.push_str("[\n");
    for item in items {
        pad(indent + 2, out);
        write_value(item, indent + 2, out);
        out.push('\n');
    }
    pad(indent, out);
    out.push(']');
}

fn pad(indent: usize, out: &mut String) {
    out.extend(std::iter::repeat_n(' ', indent));
}

/// Bare where Nix allows a bare identifier, quoted otherwise. Every key the
/// config defines is bare; the quoting is there so a key that is not cannot
/// produce broken Nix.
fn nix_key(key: &str) -> String {
    let mut chars = key.chars();
    let bare = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '\'')
        }
        _ => false,
    };
    if bare {
        key.to_string()
    } else {
        nix_string(key)
    }
}

/// Quote a Nix string. `$` is escaped everywhere, which is valid Nix and saves
/// looking ahead for the `${` that starts an interpolation.
fn nix_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        CommandsConfig, DestructionConfig, DisplayConfig, DisplayPolicy, GeneralConfig, LidConfig,
        LidPolicy, NetworkConfig, NetworkPolicy, PciConfig, PciPolicy, PowerConfig, PowerPolicy,
        SdCardWhitelistConfig, SdCardWhitelistEntry, ThunderboltWhitelistConfig,
        ThunderboltWhitelistEntry, WhitelistConfig, WhitelistEntry, parse,
    };
    use std::path::PathBuf;

    /// Emit, parse with the daemon's own loader, emit again. The text compare
    /// gives a readable diff; the value compare is the one that matters, since
    /// a field the emitter drops cancels out on both sides of the text.
    /// Returns the parsed config for tests that want to look at it.
    fn assert_round_trips(config: &Config) -> Config {
        let text = to_toml(config).expect("emit TOML");
        let back = parse(&text)
            .unwrap_or_else(|e| panic!("the loader rejected our own TOML: {e}\n{text}"));
        assert_eq!(to_toml(&back).expect("re-emit TOML"), text);
        assert_eq!(
            &back, config,
            "the loader gave back a different config\n{text}"
        );
        back
    }

    fn full_config() -> Config {
        Config {
            general: GeneralConfig {
                sleep_ms: 500,
                log_file: PathBuf::from("/var/log/plugkill/loud.log"),
                dry_run: true,
                watch_usb: true,
                watch_thunderbolt: true,
                watch_sdcard: true,
                watch_power: true,
                watch_network: true,
                watch_lid: true,
                watch_pci: true,
                watch_display: true,
                require_auth: true,
            },
            whitelist: WhitelistConfig {
                devices: vec![
                    WhitelistEntry {
                        vendor_id: "1d6b".to_string(),
                        product_id: "0002".to_string(),
                        count: 2,
                    },
                    WhitelistEntry {
                        vendor_id: "046d".to_string(),
                        product_id: "c52b".to_string(),
                        count: 1,
                    },
                ],
            },
            destruction: DestructionConfig {
                files_to_remove: vec![PathBuf::from("/home/user/secrets.kdbx")],
                folders_to_remove: vec![PathBuf::from("/home/user/vault")],
                melt_self: true,
                do_sync: false,
                do_wipe_swap: true,
                swap_device: Some("/dev/sda2".to_string()),
            },
            commands: CommandsConfig {
                kill_commands: vec![
                    vec!["/usr/bin/systemctl".to_string(), "poweroff".to_string()],
                    vec!["/usr/bin/logger".to_string(), "he said \"go\"".to_string()],
                ],
            },
            thunderbolt_whitelist: ThunderboltWhitelistConfig {
                devices: vec![ThunderboltWhitelistEntry {
                    unique_id: "00000000-1111-2222-3333-444444444444".to_string(),
                }],
            },
            sdcard_whitelist: SdCardWhitelistConfig {
                devices: vec![SdCardWhitelistEntry {
                    serial: "0x12345678".to_string(),
                }],
            },
            power: PowerConfig {
                policy: PowerPolicy::AcRequired,
                grace_secs: 5,
                require_locked: true,
            },
            network: NetworkConfig {
                policy: NetworkPolicy::Kill,
                grace_secs: 10,
                interfaces: vec!["eth0".to_string(), "wlan0".to_string()],
            },
            lid: LidConfig {
                policy: LidPolicy::Kill,
                grace_secs: 3,
            },
            pci: PciConfig {
                policy: PciPolicy::Kill,
                ignore: vec!["0000:01:00.0".to_string()],
            },
            display: DisplayConfig {
                policy: DisplayPolicy::Kill,
                ignore: vec!["eDP".to_string()],
            },
        }
    }

    /// A path with a quote and a backslash in it, everywhere a path is allowed.
    fn awkward_config() -> Config {
        let mut config = full_config();
        config.general.log_file = PathBuf::from("/var/log/plug\"kill\\odd/${HOME}/plugkill.log");
        config.destruction.files_to_remove = vec![PathBuf::from("/home/user/a \"quoted\\file")];
        config.destruction.folders_to_remove = vec![PathBuf::from("/home/user/back\\slash")];
        config.destruction.swap_device = Some("/dev/disk/by-id/we\"ird".to_string());
        config
    }

    #[test]
    fn default_config_round_trips() {
        assert_round_trips(&Config::default());
    }

    #[test]
    fn full_config_round_trips() {
        assert_round_trips(&full_config());
    }

    #[test]
    fn every_power_policy_round_trips() {
        for policy in [
            PowerPolicy::TriggerOnce,
            PowerPolicy::AcRequired,
            PowerPolicy::Monitor,
        ] {
            let mut config = full_config();
            config.power.policy = policy;
            let back = assert_round_trips(&config);
            assert_eq!(back.power.policy, policy);
        }
    }

    #[test]
    fn monitor_policies_round_trip() {
        let mut config = full_config();
        config.network.policy = NetworkPolicy::Monitor;
        config.lid.policy = LidPolicy::Monitor;
        config.pci.policy = PciPolicy::Monitor;
        config.display.policy = DisplayPolicy::Monitor;
        let back = assert_round_trips(&config);
        assert_eq!(back.network.policy, NetworkPolicy::Monitor);
        assert_eq!(back.lid.policy, LidPolicy::Monitor);
        assert_eq!(back.pci.policy, PciPolicy::Monitor);
        assert_eq!(back.display.policy, DisplayPolicy::Monitor);
    }

    #[test]
    fn quoted_and_escaped_paths_round_trip() {
        let back = assert_round_trips(&awkward_config());
        assert_eq!(
            back.general.log_file,
            PathBuf::from("/var/log/plug\"kill\\odd/${HOME}/plugkill.log")
        );
        assert_eq!(
            back.destruction.swap_device.as_deref(),
            Some("/dev/disk/by-id/we\"ird")
        );
    }

    #[test]
    fn empty_config_emits_the_defaults() {
        let empty = parse("").expect("an empty config is the default config");
        assert_eq!(
            to_toml(&empty).unwrap(),
            to_toml(&Config::default()).unwrap()
        );
        assert_round_trips(&empty);
    }

    #[test]
    fn all_watch_flags_off_round_trips() {
        let mut config = full_config();
        config.general.watch_usb = false;
        config.general.watch_thunderbolt = false;
        config.general.watch_sdcard = false;
        config.general.watch_power = false;
        config.general.watch_network = false;
        config.general.watch_lid = false;
        config.general.watch_pci = false;
        config.general.watch_display = false;
        let back = assert_round_trips(&config);
        assert!(!back.general.watch_usb);
        assert!(!back.general.watch_display);
    }

    #[test]
    fn commands_and_destruction_survive_untouched() {
        let config = full_config();
        let back = assert_round_trips(&config);
        assert_eq!(back.commands.kill_commands, config.commands.kill_commands);
        assert_eq!(
            back.destruction.files_to_remove,
            config.destruction.files_to_remove
        );
        assert_eq!(
            back.destruction.folders_to_remove,
            config.destruction.folders_to_remove
        );
        assert_eq!(back.destruction.melt_self, config.destruction.melt_self);
        assert_eq!(back.destruction.do_sync, config.destruction.do_sync);
        assert_eq!(
            back.destruction.do_wipe_swap,
            config.destruction.do_wipe_swap
        );
        assert_eq!(back.destruction.swap_device, config.destruction.swap_device);
    }

    #[test]
    fn nix_quotes_what_needs_quoting() {
        let nix = to_nix(&awkward_config()).unwrap();
        assert!(nix.contains(r#"\""#), "quote not escaped:\n{nix}");
        assert!(nix.contains(r"\\"), "backslash not escaped:\n{nix}");
        assert!(nix.contains("policy = \"ac-required\";"), "{nix}");
        assert!(nix.contains(r#"interfaces = [ "eth0" "wlan0" ];"#), "{nix}");
        // Every `$` is escaped, so no `${` is left starting an interpolation.
        assert!(
            !nix.replace("\\$", "").contains("${"),
            "interpolation left open:\n{nix}"
        );
    }

    #[test]
    fn nix_empty_collections_are_empty() {
        let nix = to_nix(&Config::default()).unwrap();
        assert!(nix.contains("devices = [ ];"), "{nix}");
        assert!(nix.contains("kill_commands = [ ];"), "{nix}");
    }

    /// `nix/module.nix` asserts `require_auth` is not in
    /// `services.plugkill.settings`, so the attrset must not carry it.
    #[test]
    fn nix_leaves_require_auth_to_the_module_option() {
        let nix = to_nix(&full_config()).unwrap();
        let (comment, attrset) = nix.split_once('\n').expect("the comment line");
        assert_eq!(comment, "# services.plugkill.requireAuth = true;");
        assert!(
            !attrset.contains("require_auth"),
            "the module refuses this key:\n{nix}"
        );
    }

    /// The loader clamps out-of-range values silently, so a window showing the
    /// emitted text must show what the daemon would run, not what was typed.
    #[test]
    fn checked_emit_shows_the_clamped_values() {
        let mut config = full_config();
        config.general.sleep_ms = 10;
        config.power.grace_secs = 600;

        let (text, back) = to_toml_checked(&config).unwrap();

        assert_eq!(back.general.sleep_ms, 50);
        assert_eq!(back.power.grace_secs, 300);
        assert!(text.contains("sleep_ms = 50"), "{text}");
        assert!(text.contains("grace_secs = 300"), "{text}");
    }

    /// Evaluate the emitted Nix and compare every value against the config, not
    /// just that it parses: a wrong-typed or misnamed key parses fine and only
    /// fails on the user's `nixos-rebuild`.
    #[test]
    fn nix_output_evaluates_to_the_config() {
        for config in [Config::default(), full_config(), awkward_config()] {
            let nix = to_nix(&config).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.nix");
            std::fs::write(&path, &nix).unwrap();
            let out = match std::process::Command::new("nix-instantiate")
                .args(["--eval", "--strict", "--json"])
                .arg(&path)
                .output()
            {
                Ok(out) => out,
                // Skip where nix is not installed.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(e) => panic!("running nix-instantiate: {e}"),
            };
            let err = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                // A nix that cannot reach its store or state directory is not a
                // verdict on the emitted text. Only a complaint about the text
                // itself is.
                assert!(
                    !err.contains("syntax error") && !err.contains("undefined variable"),
                    "nix-instantiate rejected:\n{nix}\n{err}"
                );
                continue;
            }
            let got: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            let mut want = serde_json::to_value(&config).unwrap();
            // Not emitted: the module option owns it.
            want["general"]
                .as_object_mut()
                .unwrap()
                .remove("require_auth");
            assert_eq!(got, want, "nix values disagree with the config:\n{nix}");
        }
    }
}
