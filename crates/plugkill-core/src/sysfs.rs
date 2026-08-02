use crate::error::Error;
use log::warn;
use std::fs;
use std::path::Path;

/// Read a sysfs attribute file, returning trimmed contents.
/// Returns None if the file doesn't exist (normal for interfaces/hubs).
pub fn read_sysfs_attr(path: &Path) -> Result<Option<String>, Error> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(Error::Usb(format!(
            "permission denied reading {}: {}",
            path.display(),
            e
        ))),
        Err(e) => {
            warn!("unexpected error reading {}: {}", path.display(), e);
            Ok(None)
        }
    }
}
