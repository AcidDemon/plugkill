//! External display (HDMI/DisplayPort/eDP) connect/disconnect monitoring.
//!
//! Modeled as a single "topology generation" token that changes whenever a
//! watched connector's state changes. Any change while armed is the violation,
//! which covers both a laptop yanked from a dock/projector (disconnect) and a
//! rogue capture device attached (connect).
//!
//! Linux hashes the per-connector status under `/sys/class/drm`. FreeBSD has no
//! such sysfs, so a devd listener counts DRM CONNECTOR hotplug events; there the
//! ignore list has no effect because the event does not name the connector.

/// A token that changes whenever the watched display topology changes.
pub fn display_generation(ignore: &[String]) -> u64 {
    #[cfg(target_os = "linux")]
    {
        linux::generation(ignore)
    }
    #[cfg(target_os = "freebsd")]
    {
        let _ = ignore;
        freebsd::generation()
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = ignore;
        0
    }
}

/// A connector is ignored if its name contains any non-empty ignore token,
/// e.g. `eDP` masks the internal panel.
#[cfg(any(target_os = "linux", test))]
fn is_ignored(connector: &str, ignore: &[String]) -> bool {
    ignore
        .iter()
        .any(|ig| !ig.is_empty() && connector.contains(ig.as_str()))
}

/// Hash a sorted `(connector, status)` list into a generation token. Order must
/// be stable, so callers sort first.
#[cfg(any(target_os = "linux", test))]
pub fn hash_connectors(items: &[(String, String)]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    items.hash(&mut h);
    h.finish()
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{hash_connectors, is_ignored};
    use std::path::Path;

    pub fn generation(ignore: &[String]) -> u64 {
        let mut items: Vec<(String, String)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(Path::new("/sys/class/drm")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Connector dirs look like "card0-DP-1"; skip "card0",
                // "renderD128", "version".
                let connector = match name.split_once('-') {
                    Some((card, conn)) if card.starts_with("card") && !conn.is_empty() => conn,
                    _ => continue,
                };
                if is_ignored(connector, ignore) {
                    continue;
                }
                let status = std::fs::read_to_string(entry.path().join("status"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                items.push((connector.to_string(), status));
            }
        }
        items.sort();
        hash_connectors(&items)
    }
}

/// A devd line is a DRM connector hotplug if it names the DRM system and the
/// CONNECTOR subsystem.
#[cfg(any(target_os = "freebsd", test))]
pub fn is_drm_connector_event(line: &str) -> bool {
    line.contains("system=DRM") && line.contains("subsystem=CONNECTOR")
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::is_drm_connector_event;
    use log::warn;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Once, OnceLock};
    use std::time::Duration;

    static GEN: AtomicU64 = AtomicU64::new(0);
    const DEVD_PIPE: &str = "/var/run/devd.pipe";

    pub fn generation() -> u64 {
        ensure_listener();
        GEN.load(Ordering::Relaxed)
    }

    fn ensure_listener() {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            let _ = std::thread::Builder::new()
                .name("plugkill-drm-devd".into())
                .spawn(listen_loop);
        });
    }

    fn listen_loop() {
        static WARNED: Once = Once::new();
        loop {
            match UnixStream::connect(DEVD_PIPE) {
                Ok(stream) => {
                    for line in BufReader::new(stream).lines() {
                        let Ok(line) = line else { break };
                        if is_drm_connector_event(&line) {
                            GEN.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(e) => WARNED.call_once(|| warn!("cannot connect to devd at {DEVD_PIPE}: {e}")),
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_changes_with_status() {
        let a = vec![("DP-1".to_string(), "connected".to_string())];
        let b = vec![("DP-1".to_string(), "disconnected".to_string())];
        assert_eq!(hash_connectors(&a), hash_connectors(&a));
        assert_ne!(hash_connectors(&a), hash_connectors(&b));
    }

    #[test]
    fn test_hash_stable_across_calls() {
        let items = vec![
            ("DP-1".to_string(), "connected".to_string()),
            ("eDP-1".to_string(), "connected".to_string()),
        ];
        assert_eq!(hash_connectors(&items), hash_connectors(&items));
    }

    #[test]
    fn test_is_ignored() {
        let ig = vec!["eDP".to_string()];
        assert!(is_ignored("eDP-1", &ig));
        assert!(!is_ignored("DP-1", &ig));
        assert!(!is_ignored("HDMI-A-1", &ig));
    }

    #[test]
    fn test_is_drm_connector_event() {
        assert!(is_drm_connector_event(
            "!system=DRM subsystem=CONNECTOR type=HOTPLUG"
        ));
        assert!(!is_drm_connector_event(
            "!system=ACPI subsystem=Lid notify=0x00"
        ));
        assert!(!is_drm_connector_event("!system=DRM subsystem=DEVICE"));
    }
}
