use base64::prelude::*;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default = "default_plugkill_socket")]
    pub plugkill_socket: PathBuf,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    pub private_key_file: Option<PathBuf>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            listen_port: default_listen_port(),
            plugkill_socket: default_plugkill_socket(),
            poll_interval_ms: default_poll_interval_ms(),
            private_key_file: None,
        }
    }
}

fn default_listen_port() -> u16 {
    7654
}
fn default_plugkill_socket() -> PathBuf {
    PathBuf::from("/run/plugkill/plugkill.sock")
}
fn default_poll_interval_ms() -> u64 {
    250
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    #[serde(default = "default_ack_timeout_ms")]
    pub ack_timeout_ms: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            ack_timeout_ms: default_ack_timeout_ms(),
            max_retries: default_max_retries(),
        }
    }
}

fn default_ack_timeout_ms() -> u64 {
    50
}
fn default_max_retries() -> u32 {
    3
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    pub name: String,
    pub pubkey: String,
    #[serde(default)]
    pub addresses: Vec<String>,
}

impl PeerConfig {
    pub fn decode_pubkey(&self) -> Result<[u8; 32], String> {
        let bytes = BASE64_STANDARD
            .decode(&self.pubkey)
            .map_err(|e| format!("peer '{}': invalid base64 pubkey: {e}", self.name))?;
        bytes.try_into().map_err(|v: Vec<u8>| {
            format!(
                "peer '{}': pubkey must be 32 bytes, got {}",
                self.name,
                v.len()
            )
        })
    }
}

pub fn load(path: &Path) -> Result<Config, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    let config: Config =
        toml::from_str(&contents).map_err(|e| format!("failed to parse config: {e}"))?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &Config) -> Result<(), String> {
    if config.general.private_key_file.is_none() {
        return Err("general.private_key_file is required".into());
    }

    let key_path = config.general.private_key_file.as_ref().unwrap();
    if !key_path.is_absolute() {
        return Err(format!(
            "private_key_file must be an absolute path, got: {}",
            key_path.display()
        ));
    }

    if config.general.poll_interval_ms < 50 {
        return Err("poll_interval_ms must be >= 50".into());
    }
    if config.general.poll_interval_ms > 10000 {
        return Err("poll_interval_ms must be <= 10000".into());
    }

    if config.retry.ack_timeout_ms < 10 {
        return Err("ack_timeout_ms must be >= 10".into());
    }
    if config.retry.max_retries > 10 {
        return Err("max_retries must be <= 10".into());
    }

    let mut names = std::collections::HashSet::new();
    for peer in &config.peers {
        if peer.name.is_empty() {
            return Err("peer name must not be empty".into());
        }
        if !names.insert(&peer.name) {
            return Err(format!("duplicate peer name: '{}'", peer.name));
        }
        peer.decode_pubkey()?;

        for addr in &peer.addresses {
            if !addr.contains(':') {
                return Err(format!(
                    "peer '{}': address '{}' must include a port (host:port)",
                    peer.name, addr
                ));
            }
        }
    }

    Ok(())
}

pub fn load_private_key(path: &Path) -> Result<[u8; 32], String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read private key {}: {e}", path.display()))?;
    let trimmed = contents.trim();
    let bytes = BASE64_STANDARD
        .decode(trimmed)
        .map_err(|e| format!("private key is not valid base64: {e}"))?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("private key must be 32 bytes, got {}", v.len()))
}

pub fn default_config_toml() -> &'static str {
    r#"# plugkill-relay configuration

[general]
# UDP port to listen on
listen_port = 7654

# Path to plugkill's Unix socket
plugkill_socket = "/run/plugkill/plugkill.sock"

# How often to poll plugkill status (ms, 50-10000)
poll_interval_ms = 250

# Path to this node's ed25519 private key (base64, 32 bytes)
private_key_file = "/etc/plugkill-relay/key.priv"

[retry]
# Time to wait for ACK before retrying (ms)
ack_timeout_ms = 50

# Maximum retry attempts per peer
max_retries = 3

# [[peers]]
# name = "ted"
# pubkey = "base64-encoded-ed25519-pubkey"
# addresses = ["ted.tail12345.ts.net:7654", "192.168.1.10:7654"]
#
# [[peers]]
# name = "android"
# pubkey = "base64-encoded-ed25519-pubkey"
# # send-only peer — no addresses needed
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn valid_pubkey_b64() -> String {
        let (_, pubkey) = crypto::generate_keypair();
        BASE64_STANDARD.encode(pubkey)
    }

    #[test]
    fn test_minimal_valid_config() {
        let pk = valid_pubkey_b64();
        let f = write_config(&format!(
            r#"
[general]
private_key_file = "/etc/plugkill-relay/key.priv"

[[peers]]
name = "test"
pubkey = "{pk}"
addresses = ["192.168.1.1:7654"]
"#
        ));
        let config = load(f.path()).unwrap();
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.general.listen_port, 7654);
    }

    #[test]
    fn test_missing_private_key_file_rejects() {
        let f = write_config("[general]\n");
        let err = load(f.path()).unwrap_err();
        assert!(err.contains("private_key_file is required"));
    }

    #[test]
    fn test_relative_key_path_rejects() {
        let f = write_config(
            r#"
[general]
private_key_file = "relative/path"
"#,
        );
        let err = load(f.path()).unwrap_err();
        assert!(err.contains("absolute path"));
    }

    #[test]
    fn test_duplicate_peer_name_rejects() {
        let pk = valid_pubkey_b64();
        let f = write_config(&format!(
            r#"
[general]
private_key_file = "/etc/key"

[[peers]]
name = "dup"
pubkey = "{pk}"

[[peers]]
name = "dup"
pubkey = "{pk}"
"#
        ));
        let err = load(f.path()).unwrap_err();
        assert!(err.contains("duplicate peer name"));
    }

    #[test]
    fn test_invalid_pubkey_rejects() {
        let f = write_config(
            r#"
[general]
private_key_file = "/etc/key"

[[peers]]
name = "bad"
pubkey = "not-base64!!!"
"#,
        );
        let err = load(f.path()).unwrap_err();
        assert!(err.contains("invalid base64"));
    }

    #[test]
    fn test_address_without_port_rejects() {
        let pk = valid_pubkey_b64();
        let f = write_config(&format!(
            r#"
[general]
private_key_file = "/etc/key"

[[peers]]
name = "noport"
pubkey = "{pk}"
addresses = ["192.168.1.1"]
"#
        ));
        let err = load(f.path()).unwrap_err();
        assert!(err.contains("must include a port"));
    }

    #[test]
    fn test_defaults_applied() {
        let pk = valid_pubkey_b64();
        let f = write_config(&format!(
            r#"
[general]
private_key_file = "/etc/key"

[[peers]]
name = "p"
pubkey = "{pk}"
"#
        ));
        let config = load(f.path()).unwrap();
        assert_eq!(config.retry.ack_timeout_ms, 50);
        assert_eq!(config.retry.max_retries, 3);
        assert_eq!(config.general.poll_interval_ms, 250);
    }

    #[test]
    fn test_hostname_addresses_accepted() {
        let pk = valid_pubkey_b64();
        let f = write_config(&format!(
            r#"
[general]
private_key_file = "/etc/key"

[[peers]]
name = "ts"
pubkey = "{pk}"
addresses = ["ted.tail12345.ts.net:7654", "192.168.1.10:7654"]
"#
        ));
        let config = load(f.path()).unwrap();
        assert_eq!(config.peers[0].addresses.len(), 2);
    }
}
