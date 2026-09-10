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
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // std's io::Error carries no path, so keep it in the message. The
            // bus is already named by every caller that prints this.
            let msg = format!("permission denied reading {}", path.display());
            Err(Error::Io(std::io::Error::new(e.kind(), msg)))
        }
        Err(e) => {
            warn!("unexpected error reading {}: {}", path.display(), e);
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn test_permission_denied_is_io_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idVendor");
        fs::write(&path, "1d6b").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        // Root ignores the mode bits (the FreeBSD CI job runs tests as root),
        // so there is nothing to assert there.
        if fs::read_to_string(&path).is_ok() {
            return;
        }

        let err = read_sysfs_attr(&path).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "expected Io, got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("permission denied reading"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }
}
