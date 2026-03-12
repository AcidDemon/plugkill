use crate::error::Error;
use log::warn;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub whitelist: WhitelistConfig,
    #[serde(default)]
    pub destruction: DestructionConfig,
    #[serde(default)]
    pub commands: CommandsConfig,
    #[serde(default)]
    pub thunderbolt_whitelist: ThunderboltWhitelistConfig,
    #[serde(default)]
    pub sdcard_whitelist: SdCardWhitelistConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
    #[serde(default = "default_sleep_ms")]
    pub sleep_ms: u64,
    #[serde(default = "default_log_file")]
    pub log_file: PathBuf,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_true")]
    pub watch_usb: bool,
    #[serde(default = "default_true")]
    pub watch_thunderbolt: bool,
    #[serde(default = "default_true")]
    pub watch_sdcard: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            sleep_ms: default_sleep_ms(),
            log_file: default_log_file(),
            dry_run: false,
            watch_usb: true,
            watch_thunderbolt: true,
            watch_sdcard: true,
        }
    }
}

fn default_sleep_ms() -> u64 {
    250
}

fn default_log_file() -> PathBuf {
    PathBuf::from("/var/log/plugkill/plugkill.log")
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WhitelistConfig {
    #[serde(default)]
    pub devices: Vec<WhitelistEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhitelistEntry {
    pub vendor_id: String,
    pub product_id: String,
    #[serde(default = "default_count")]
    pub count: u32,
}

fn default_count() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestructionConfig {
    #[serde(default)]
    pub files_to_remove: Vec<PathBuf>,
    #[serde(default)]
    pub folders_to_remove: Vec<PathBuf>,
    #[serde(default)]
    pub melt_self: bool,
    #[serde(default = "default_true")]
    pub do_sync: bool,
    #[serde(default)]
    pub do_wipe_swap: bool,
    pub swap_device: Option<String>,
}

impl Default for DestructionConfig {
    fn default() -> Self {
        Self {
            files_to_remove: Vec::new(),
            folders_to_remove: Vec::new(),
            melt_self: false,
            do_sync: true,
            do_wipe_swap: false,
            swap_device: None,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CommandsConfig {
    #[serde(default)]
    pub kill_commands: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ThunderboltWhitelistConfig {
    #[serde(default)]
    pub devices: Vec<ThunderboltWhitelistEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThunderboltWhitelistEntry {
    pub unique_id: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SdCardWhitelistConfig {
    #[serde(default)]
    pub devices: Vec<SdCardWhitelistEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdCardWhitelistEntry {
    pub serial: String,
}

// --- Validation ---

/// Validate that a path is absolute and contains no `..` segments.
fn validate_path(path: &Path, context: &str) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::Config(format!(
            "{context}: path must be absolute: {}",
            path.display()
        )));
    }
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(Error::Config(format!(
                "{context}: path must not contain '..' segments: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Validate that a hex ID is 1-4 hex characters.
fn validate_hex_id(id: &str, field: &str) -> Result<(), Error> {
    if id.is_empty() || id.len() > 4 {
        return Err(Error::Config(format!(
            "{field}: must be 1-4 hex characters, got '{id}'"
        )));
    }
    if !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Config(format!(
            "{field}: must contain only hex characters, got '{id}'"
        )));
    }
    Ok(())
}

/// Validate the entire config, clamping and warning where appropriate.
fn validate(config: &mut Config) -> Result<(), Error> {
    // Clamp sleep_ms
    const MIN_SLEEP: u64 = 50;
    const MAX_SLEEP: u64 = 10_000;
    if config.general.sleep_ms < MIN_SLEEP {
        warn!(
            "sleep_ms {} is below minimum {MIN_SLEEP}, clamping",
            config.general.sleep_ms
        );
        config.general.sleep_ms = MIN_SLEEP;
    }
    if config.general.sleep_ms > MAX_SLEEP {
        warn!(
            "sleep_ms {} is above maximum {MAX_SLEEP}, clamping",
            config.general.sleep_ms
        );
        config.general.sleep_ms = MAX_SLEEP;
    }

    // Validate log file path
    validate_path(&config.general.log_file, "general.log_file")?;

    // Validate whitelist entries
    for (i, entry) in config.whitelist.devices.iter().enumerate() {
        validate_hex_id(
            &entry.vendor_id,
            &format!("whitelist.devices[{i}].vendor_id"),
        )?;
        validate_hex_id(
            &entry.product_id,
            &format!("whitelist.devices[{i}].product_id"),
        )?;
        if entry.count == 0 {
            return Err(Error::Config(format!(
                "whitelist.devices[{i}].count: must be at least 1"
            )));
        }
    }

    // Validate destruction paths
    for (i, path) in config.destruction.files_to_remove.iter().enumerate() {
        validate_path(path, &format!("destruction.files_to_remove[{i}]"))?;
    }
    for (i, path) in config.destruction.folders_to_remove.iter().enumerate() {
        validate_path(path, &format!("destruction.folders_to_remove[{i}]"))?;
    }

    // Validate swap device path if set
    if let Some(ref dev) = config.destruction.swap_device {
        let dev_path = Path::new(dev);
        validate_path(dev_path, "destruction.swap_device")?;
    }
    if config.destruction.do_wipe_swap && config.destruction.swap_device.is_none() {
        return Err(Error::Config(
            "do_wipe_swap is true but swap_device is not set".to_string(),
        ));
    }

    // Validate thunderbolt whitelist entries
    for (i, entry) in config.thunderbolt_whitelist.devices.iter().enumerate() {
        if entry.unique_id.is_empty() {
            return Err(Error::Config(format!(
                "thunderbolt_whitelist.devices[{i}].unique_id: must not be empty"
            )));
        }
    }

    // Validate sdcard whitelist entries
    for (i, entry) in config.sdcard_whitelist.devices.iter().enumerate() {
        if entry.serial.is_empty() {
            return Err(Error::Config(format!(
                "sdcard_whitelist.devices[{i}].serial: must not be empty"
            )));
        }
    }

    // Validate kill commands — binaries must be absolute paths to prevent
    // arbitrary command execution via PATH resolution (e.g. "sh -c ...")
    for (i, cmd) in config.commands.kill_commands.iter().enumerate() {
        if cmd.is_empty() {
            return Err(Error::Config(format!(
                "commands.kill_commands[{i}]: command array must not be empty"
            )));
        }
        let binary = &cmd[0];
        if binary.is_empty() {
            return Err(Error::Config(format!(
                "commands.kill_commands[{i}]: binary name must not be empty"
            )));
        }
        let binary_path = Path::new(binary);
        validate_path(binary_path, &format!("commands.kill_commands[{i}]"))?;
    }

    Ok(())
}

/// Verify the config file is owned by root and not writable by group/others.
fn check_file_permissions(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(|e| {
        Error::Config(format!("cannot stat config file {}: {e}", path.display()))
    })?;
    if meta.uid() != 0 {
        return Err(Error::Config(format!(
            "config file {} must be owned by root (uid 0), owned by uid {}",
            path.display(),
            meta.uid()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(Error::Config(format!(
            "config file {} must not be writable by group/others (mode {:o})",
            path.display(),
            meta.mode() & 0o777
        )));
    }
    Ok(())
}

/// Load and validate configuration from a TOML file.
/// Verifies the file is owned by root and not world/group-writable.
pub fn load(path: &Path) -> Result<Config, Error> {
    // Security: verify file ownership and permissions before trusting content
    check_file_permissions(path)?;
    load_from_path(path)
}

/// Reload configuration (re-checks permissions). Alias for `load` for clarity.
pub fn reload(path: &Path) -> Result<Config, Error> {
    load(path)
}

/// Load config without permission checks (for testing only).
#[cfg(test)]
fn load_for_test(path: &Path) -> Result<Config, Error> {
    load_from_path(path)
}

fn load_from_path(path: &Path) -> Result<Config, Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(format!("failed to read config file {}: {e}", path.display()))
    })?;

    let mut config: Config = toml::from_str(&contents).map_err(|e| {
        Error::Config(format!(
            "failed to parse config file {}: {e}",
            path.display()
        ))
    })?;

    validate(&mut config)?;

    Ok(config)
}

/// Whitelists loaded from a config file (USB, Thunderbolt, and SD card).
pub struct LoadedWhitelists {
    pub usb: WhitelistConfig,
    pub thunderbolt: ThunderboltWhitelistConfig,
    pub sdcard: SdCardWhitelistConfig,
}

/// Load only the whitelist sections from a config file, without permission checks.
/// Returns default (empty) whitelists if the file does not exist.
pub fn load_whitelist_only(path: &Path) -> Result<LoadedWhitelists, Error> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedWhitelists {
                usb: WhitelistConfig::default(),
                thunderbolt: ThunderboltWhitelistConfig::default(),
                sdcard: SdCardWhitelistConfig::default(),
            });
        }
        Err(e) => {
            return Err(Error::Config(format!(
                "failed to read config file {}: {e}",
                path.display()
            )));
        }
    };

    #[derive(Deserialize)]
    struct Partial {
        #[serde(default)]
        whitelist: WhitelistConfig,
        #[serde(default)]
        thunderbolt_whitelist: ThunderboltWhitelistConfig,
        #[serde(default)]
        sdcard_whitelist: SdCardWhitelistConfig,
    }

    let partial: Partial = toml::from_str(&contents).map_err(|e| {
        Error::Config(format!(
            "failed to parse config file {}: {e}",
            path.display()
        ))
    })?;

    Ok(LoadedWhitelists {
        usb: partial.whitelist,
        thunderbolt: partial.thunderbolt_whitelist,
        sdcard: partial.sdcard_whitelist,
    })
}

/// Returns a commented default configuration file.
pub fn default_config_toml() -> &'static str {
    r#"# plugkill configuration

[general]
# Polling interval in milliseconds (50-10000)
sleep_ms = 250
# Log file path
log_file = "/var/log/plugkill/plugkill.log"
# Which buses to monitor (all enabled by default)
watch_usb = true
watch_thunderbolt = true
watch_sdcard = true

[whitelist]
# Whitelisted USB devices. Each entry has vendor_id, product_id, and optional count.
# devices = [
#   { vendor_id = "1234", product_id = "5678", count = 1 },
# ]
devices = []

[destruction]
# Files to securely delete on kill
files_to_remove = []
# Folders to securely delete on kill
folders_to_remove = []
# Remove plugkill binary and config after kill
melt_self = false
# Sync filesystems before shutdown
do_sync = true
# Wipe swap partition
do_wipe_swap = false
# swap_device = "/dev/sda2"

[thunderbolt_whitelist]
# Whitelisted Thunderbolt/USB4 devices by unique_id (UUID).
# Triggers kill on any new physical connection before DMA/authorization.
# devices = [
#   { unique_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" },
# ]
devices = []

[sdcard_whitelist]
# Whitelisted SD/MMC cards by serial number.
# Monitors SD, MMC, and SDIO devices. Triggers kill on insertion or removal.
# devices = [
#   { serial = "0x12345678" },
# ]
devices = []

[commands]
# Commands to execute during kill sequence (each is an argv array)
# kill_commands = [
#   ["/usr/bin/some-command", "--flag"],
# ]
kill_commands = []
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_default_config_parses() {
        let f = write_config(default_config_toml());
        let config = load_for_test(f.path()).unwrap();
        assert_eq!(config.general.sleep_ms, 250);
        assert!(!config.general.dry_run);
        assert!(config.whitelist.devices.is_empty());
        assert!(config.destruction.files_to_remove.is_empty());
        assert!(config.destruction.do_sync);
        assert!(!config.destruction.do_wipe_swap);
    }

    #[test]
    fn test_minimal_config() {
        let f = write_config("");
        let config = load_for_test(f.path()).unwrap();
        assert_eq!(config.general.sleep_ms, 250);
    }

    #[test]
    fn test_whitelist_valid() {
        let f = write_config(
            r#"
[whitelist]
devices = [
    { vendor_id = "1d6b", product_id = "0002", count = 2 },
    { vendor_id = "abcd", product_id = "ef01" },
]
"#,
        );
        let config = load_for_test(f.path()).unwrap();
        assert_eq!(config.whitelist.devices.len(), 2);
        assert_eq!(config.whitelist.devices[0].count, 2);
        assert_eq!(config.whitelist.devices[1].count, 1); // default
    }

    #[test]
    fn test_whitelist_invalid_hex() {
        let f = write_config(
            r#"
[whitelist]
devices = [{ vendor_id = "ZZZZ", product_id = "0002" }]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("hex characters"));
    }

    #[test]
    fn test_whitelist_too_long() {
        let f = write_config(
            r#"
[whitelist]
devices = [{ vendor_id = "12345", product_id = "0002" }]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("1-4 hex"));
    }

    #[test]
    fn test_sleep_ms_clamped_low() {
        let f = write_config(
            r#"
[general]
sleep_ms = 10
"#,
        );
        let config = load_for_test(f.path()).unwrap();
        assert_eq!(config.general.sleep_ms, 50);
    }

    #[test]
    fn test_sleep_ms_clamped_high() {
        let f = write_config(
            r#"
[general]
sleep_ms = 99999
"#,
        );
        let config = load_for_test(f.path()).unwrap();
        assert_eq!(config.general.sleep_ms, 10_000);
    }

    #[test]
    fn test_relative_path_rejected() {
        let f = write_config(
            r#"
[destruction]
files_to_remove = ["relative/path.txt"]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn test_path_traversal_rejected() {
        let f = write_config(
            r#"
[destruction]
files_to_remove = ["/etc/../etc/shadow"]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn test_empty_kill_command_rejected() {
        let f = write_config(
            r#"
[commands]
kill_commands = [[]]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn test_command_path_traversal_rejected() {
        let f = write_config(
            r#"
[commands]
kill_commands = [["../../../bin/evil"]]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("..") || err.to_string().contains("absolute"));
    }

    #[test]
    fn test_relative_command_rejected() {
        let f = write_config(
            r#"
[commands]
kill_commands = [["sh", "-c", "echo pwned"]]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn test_swap_without_device_rejected() {
        let f = write_config(
            r#"
[destruction]
do_wipe_swap = true
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("swap_device is not set"));
    }

    #[test]
    fn test_unknown_field_rejected() {
        let f = write_config(
            r#"
[general]
unknown_field = true
"#,
        );
        let err = load_for_test(f.path());
        assert!(err.is_err());
    }

    #[test]
    fn test_full_config() {
        let f = write_config(
            r#"
[general]
sleep_ms = 500
log_file = "/var/log/usbkill/custom.log"

[whitelist]
devices = [
    { vendor_id = "1d6b", product_id = "0002", count = 3 },
]

[destruction]
files_to_remove = ["/tmp/secret.txt"]
folders_to_remove = ["/tmp/secrets"]
melt_self = true
do_sync = false
do_wipe_swap = true
swap_device = "/dev/sda2"

[commands]
kill_commands = [
    ["/usr/bin/shred", "-vfz", "/dev/sda1"],
    ["/usr/bin/truecrypt", "--dismount"],
]
"#,
        );
        let config = load_for_test(f.path()).unwrap();
        assert_eq!(config.general.sleep_ms, 500);
        assert!(config.destruction.melt_self);
        assert!(!config.destruction.do_sync);
        assert_eq!(config.commands.kill_commands.len(), 2);
    }

    #[test]
    fn test_thunderbolt_whitelist_valid() {
        let f = write_config(
            r#"
[thunderbolt_whitelist]
devices = [
    { unique_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" },
]
"#,
        );
        let config = load_for_test(f.path()).unwrap();
        assert_eq!(config.thunderbolt_whitelist.devices.len(), 1);
        assert_eq!(
            config.thunderbolt_whitelist.devices[0].unique_id,
            "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
        );
    }

    #[test]
    fn test_thunderbolt_whitelist_empty_id_rejected() {
        let f = write_config(
            r#"
[thunderbolt_whitelist]
devices = [{ unique_id = "" }]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn test_thunderbolt_whitelist_default_empty() {
        let f = write_config("");
        let config = load_for_test(f.path()).unwrap();
        assert!(config.thunderbolt_whitelist.devices.is_empty());
    }

    #[test]
    fn test_sdcard_whitelist_valid() {
        let f = write_config(
            r#"
[sdcard_whitelist]
devices = [
    { serial = "0x12345678" },
]
"#,
        );
        let config = load_for_test(f.path()).unwrap();
        assert_eq!(config.sdcard_whitelist.devices.len(), 1);
        assert_eq!(config.sdcard_whitelist.devices[0].serial, "0x12345678");
    }

    #[test]
    fn test_sdcard_whitelist_empty_serial_rejected() {
        let f = write_config(
            r#"
[sdcard_whitelist]
devices = [{ serial = "" }]
"#,
        );
        let err = load_for_test(f.path()).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn test_sdcard_whitelist_default_empty() {
        let f = write_config("");
        let config = load_for_test(f.path()).unwrap();
        assert!(config.sdcard_whitelist.devices.is_empty());
    }

    #[test]
    fn test_watch_flags_default_true() {
        let f = write_config("");
        let config = load_for_test(f.path()).unwrap();
        assert!(config.general.watch_usb);
        assert!(config.general.watch_thunderbolt);
        assert!(config.general.watch_sdcard);
    }

    #[test]
    fn test_watch_flags_disabled() {
        let f = write_config(
            r#"
[general]
watch_usb = false
watch_thunderbolt = false
watch_sdcard = false
"#,
        );
        let config = load_for_test(f.path()).unwrap();
        assert!(!config.general.watch_usb);
        assert!(!config.general.watch_thunderbolt);
        assert!(!config.general.watch_sdcard);
    }

    #[test]
    fn test_watch_flags_partial() {
        let f = write_config(
            r#"
[general]
watch_usb = true
watch_thunderbolt = false
"#,
        );
        let config = load_for_test(f.path()).unwrap();
        assert!(config.general.watch_usb);
        assert!(!config.general.watch_thunderbolt);
        assert!(config.general.watch_sdcard); // default true
    }

    #[test]
    fn test_default_config_toml_has_watch_flags() {
        let f = write_config(default_config_toml());
        let config = load_for_test(f.path()).unwrap();
        assert!(config.general.watch_usb);
        assert!(config.general.watch_thunderbolt);
        assert!(config.general.watch_sdcard);
    }
}
