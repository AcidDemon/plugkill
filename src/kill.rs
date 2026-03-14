use crate::config::Config;
use crate::error::Error;
use log::{debug, error, info, warn};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Maximum time to wait for a kill command to finish.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Buffer size for reading /dev/urandom (8 KiB).
const SHRED_BUF_SIZE: usize = 8192;

/// Number of overwrite passes for file shredding.
const SHRED_PASSES: u32 = 3;

/// Execute the full kill sequence. Under normal operation this function
/// does not return (the system shuts down). Returns Ok(()) only in dry_run mode.
pub fn execute_kill_sequence(config: &Config, reason: &str) -> Result<(), Error> {
    let dry_run = config.general.dry_run;

    // Mask SIGINT and SIGTERM so the kill sequence cannot be interrupted.
    // An adversary could send signals to abort the destruction process.
    if !dry_run {
        // SAFETY: SIG_IGN is a valid signal disposition. These calls have no
        // preconditions and cannot cause undefined behavior.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }

    info!("KILL SEQUENCE INITIATED: {reason}");

    // Step 1: Log the kill event
    log_kill_event(config, reason);

    // Step 2: Securely remove configured files
    for path in &config.destruction.files_to_remove {
        if let Err(e) = shred_file(path, dry_run) {
            error!("failed to shred file {}: {e}", path.display());
        }
    }

    // Step 3: Securely remove configured folders
    for path in &config.destruction.folders_to_remove {
        if let Err(e) = shred_directory(path, dry_run) {
            error!("failed to shred directory {}: {e}", path.display());
        }
    }

    // Step 4: Execute kill commands
    for (i, cmd) in config.commands.kill_commands.iter().enumerate() {
        if let Err(e) = execute_command(cmd, dry_run) {
            error!("kill command {i} failed: {e}");
        }
    }

    // Step 5: Filesystem sync
    if config.destruction.do_sync {
        info!("syncing filesystems");
        if !dry_run {
            // SAFETY: libc::sync() has no preconditions, no arguments, no return value,
            // and no undefined behavior. It simply flushes filesystem buffers to disk.
            unsafe { libc::sync() };
        }
    }

    // Step 6: Wipe swap
    if config.destruction.do_wipe_swap
        && let Some(ref device) = config.destruction.swap_device
        && let Err(e) = wipe_swap(device, dry_run)
    {
        error!("swap wipe failed: {e}");
    }

    // Step 7: Melt self
    if config.destruction.melt_self {
        melt_self(dry_run);
    }

    // Step 8: Shutdown
    if dry_run {
        info!("[DRY RUN] would shut down the system now");
        return Ok(());
    }

    info!("shutting down system");
    shutdown()
}

/// Write kill event to log file.
fn log_kill_event(config: &Config, reason: &str) {
    let log_path = &config.general.log_file;

    // Ensure log directory exists (create_dir_all is idempotent, no TOCTOU)
    if let Some(parent) = log_path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        error!("cannot create log directory {}: {e}", parent.display());
        return;
    }

    let timestamp = chrono_free_timestamp();
    let entry = format!("\n{timestamp} KILL: {reason}\n");

    match OpenOptions::new().create(true).append(true).open(log_path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(entry.as_bytes()) {
                error!("failed to write log: {e}");
            }
        }
        Err(e) => error!("cannot open log file {}: {e}", log_path.display()),
    }
}

/// Generate a timestamp without external crate dependencies.
fn chrono_free_timestamp() -> String {
    // Read /proc/uptime as a rough timestamp source, or fall back to epoch.
    // For a kill-switch, exact wall-clock time is less important than
    // having *some* timestamp. We avoid chrono to minimize dependencies.
    let mut buf = [0u8; 64];
    if let Ok(mut f) = File::open("/proc/uptime")
        && let Ok(n) = f.read(&mut buf)
    {
        let s = String::from_utf8_lossy(&buf[..n]);
        return format!("[uptime: {}s]", s.trim());
    }
    "[unknown time]".to_string()
}

/// Overwrite a file with random data, fsync, then unlink.
///
/// Security notes:
/// - Refuses to follow symlinks (uses symlink_metadata + O_NOFOLLOW equivalent)
/// - Warns on hardlinked files (nlink > 1) but proceeds
/// - Opens file once and reuses fd across passes to prevent TOCTOU
/// - Not effective against COW/journaling filesystems — use full disk encryption
fn shred_file(path: &Path, dry_run: bool) -> Result<(), Error> {
    // Defensive: only shred absolute paths without path traversal
    if !path.is_absolute() {
        return Err(Error::Kill(format!(
            "refusing to shred non-absolute path: {}",
            path.display()
        )));
    }
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(Error::Kill(format!(
                "refusing to shred path with '..': {}",
                path.display()
            )));
        }
    }

    if dry_run {
        info!("[DRY RUN] would shred file: {}", path.display());
        return Ok(());
    }

    debug!("shredding file: {}", path.display());

    // Use symlink_metadata to detect symlinks WITHOUT following them
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| Error::Kill(format!("cannot stat file {}: {e}", path.display())))?;

    if metadata.file_type().is_symlink() {
        warn!(
            "refusing to shred symlink {} (would follow to target)",
            path.display()
        );
        // Remove the symlink itself but don't shred the target
        fs::remove_file(path)
            .map_err(|e| Error::Kill(format!("cannot remove symlink {}: {e}", path.display())))?;
        return Ok(());
    }

    // Warn on hardlinked files
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            warn!(
                "file {} has {} hardlinks — data may remain accessible via other links",
                path.display(),
                metadata.nlink()
            );
        }
    }

    let file_size = metadata.len() as usize;

    if file_size == 0 {
        fs::remove_file(path)
            .map_err(|e| Error::Kill(format!("cannot remove file {}: {e}", path.display())))?;
        return Ok(());
    }

    // Open /dev/urandom for random data
    let mut urandom = File::open("/dev/urandom")
        .map_err(|e| Error::Kill(format!("cannot open /dev/urandom: {e}")))?;

    // Open the file ONCE and reuse the fd across passes to prevent TOCTOU
    let mut file = OpenOptions::new().write(true).open(path).map_err(|e| {
        Error::Kill(format!(
            "cannot open file for shredding {}: {e}",
            path.display()
        ))
    })?;

    let mut buf = vec![0u8; SHRED_BUF_SIZE];

    for pass in 0..SHRED_PASSES {
        debug!(
            "shred pass {}/{SHRED_PASSES} for {}",
            pass + 1,
            path.display()
        );

        // Seek back to start for each pass
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|e| Error::Kill(format!("seek failed for {}: {e}", path.display())))?;

        let mut remaining = file_size;
        while remaining > 0 {
            let chunk_size = remaining.min(SHRED_BUF_SIZE);
            urandom
                .read_exact(&mut buf[..chunk_size])
                .map_err(|e| Error::Kill(format!("error reading /dev/urandom: {e}")))?;
            file.write_all(&buf[..chunk_size])
                .map_err(|e| Error::Kill(format!("error writing to {}: {e}", path.display())))?;
            remaining -= chunk_size;
        }

        file.sync_all()
            .map_err(|e| Error::Kill(format!("fsync failed for {}: {e}", path.display())))?;
    }

    // Drop the file handle before unlinking
    drop(file);

    // Unlink the file
    fs::remove_file(path)
        .map_err(|e| Error::Kill(format!("cannot remove file {}: {e}", path.display())))?;

    debug!("shredded and removed: {}", path.display());
    Ok(())
}

/// Recursively shred all files in a directory, then remove the directory tree.
fn shred_directory(path: &Path, dry_run: bool) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::Kill(format!(
            "refusing to shred non-absolute path: {}",
            path.display()
        )));
    }

    if dry_run {
        info!("[DRY RUN] would shred directory: {}", path.display());
        return Ok(());
    }

    debug!("shredding directory: {}", path.display());

    // Walk the directory and shred all files first
    if path.is_dir() {
        let entries = fs::read_dir(path)
            .map_err(|e| Error::Kill(format!("cannot read directory {}: {e}", path.display())))?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("error reading directory entry in {}: {e}", path.display());
                    continue;
                }
            };

            let entry_path = entry.path();
            if entry_path.is_dir() {
                // Recurse into subdirectories
                if let Err(e) = shred_directory(&entry_path, dry_run) {
                    error!("failed to shred subdirectory {}: {e}", entry_path.display());
                }
            } else if let Err(e) = shred_file(&entry_path, dry_run) {
                error!("failed to shred file {}: {e}", entry_path.display());
            }
        }

        // Remove the now-empty directory
        fs::remove_dir(path)
            .map_err(|e| Error::Kill(format!("cannot remove directory {}: {e}", path.display())))?;
    }

    Ok(())
}

/// Execute a command safely without shell interpolation.
fn execute_command(argv: &[String], dry_run: bool) -> Result<(), Error> {
    if argv.is_empty() {
        return Err(Error::Kill("empty command array".to_string()));
    }

    if dry_run {
        info!("[DRY RUN] would execute: {:?}", argv);
        return Ok(());
    }

    info!("executing command: {:?}", argv);

    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .map_err(|e| Error::Kill(format!("failed to spawn {:?}: {e}", argv[0])))?;

    match child.wait_timeout(COMMAND_TIMEOUT) {
        Ok(Some(status)) => {
            if !status.success() {
                warn!("command {:?} exited with status: {status}", argv[0]);
            }
            Ok(())
        }
        Ok(None) => {
            // Timeout — kill the child process
            warn!(
                "command {:?} timed out after {COMMAND_TIMEOUT:?}, killing",
                argv[0]
            );
            let _ = child.kill();
            let _ = child.wait();
            Ok(())
        }
        Err(e) => {
            warn!("error waiting for command {:?}: {e}", argv[0]);
            Ok(())
        }
    }
}

/// Wipe swap by disabling it, overwriting the device, and re-enabling.
fn wipe_swap(device: &str, dry_run: bool) -> Result<(), Error> {
    if dry_run {
        info!("[DRY RUN] would wipe swap device: {device}");
        return Ok(());
    }

    info!("wiping swap device: {device}");

    // swapoff
    let status = Command::new("swapoff")
        .arg(device)
        .status()
        .map_err(|e| Error::Kill(format!("failed to run swapoff: {e}")))?;

    if !status.success() {
        warn!("swapoff failed for {device}, continuing anyway");
    }

    // Overwrite with urandom using dd
    let status = Command::new("dd")
        .args(["if=/dev/urandom", &format!("of={device}"), "bs=1M"])
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) => {
            // dd will "fail" with exit code when it hits end of device — that's expected
            debug!("dd overwrite of {device} finished with status: {s}");
        }
        Err(e) => warn!("dd failed for {device}: {e}"),
    }

    // swapon (best effort)
    let _ = Command::new("swapon").arg(device).status();

    Ok(())
}

/// Remove the plugkill binary and config directory.
fn melt_self(dry_run: bool) {
    if dry_run {
        info!("[DRY RUN] would melt self (remove binary and config)");
        return;
    }

    info!("melting self — removing binary and config");

    // Remove the running binary
    if let Ok(exe) = std::env::current_exe()
        && let Err(e) = fs::remove_file(&exe)
    {
        error!("cannot remove own binary {}: {e}", exe.display());
    }

    // Remove config directory
    if let Err(e) = fs::remove_dir_all("/etc/plugkill") {
        error!("cannot remove /etc/plugkill: {e}");
    }

    // Remove log directory
    if let Err(e) = fs::remove_dir_all("/var/log/plugkill") {
        error!("cannot remove /var/log/plugkill: {e}");
    }
}

/// Shut down the system using the reboot(2) syscall.
fn shutdown() -> Result<(), Error> {
    // First attempt: direct syscall via nix
    info!("calling reboot(RB_POWER_OFF)");
    match nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF) {
        Ok(infallible) => match infallible {},
        Err(e) => {
            error!("reboot syscall failed: {e}, falling back to poweroff command");
        }
    }

    // Fallback: poweroff command
    let status = Command::new("poweroff")
        .arg("-f")
        .status()
        .map_err(|e| Error::Kill(format!("poweroff command failed: {e}")))?;

    if !status.success() {
        return Err(Error::Kill(format!(
            "poweroff exited with status: {status}"
        )));
    }

    Ok(())
}

/// Trait extension for Child to add wait_timeout.
trait ChildExt {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<std::process::ExitStatus>, std::io::Error>;
}

impl ChildExt for std::process::Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<std::process::ExitStatus>, std::io::Error> {
        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(50);

        loop {
            match self.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None => {
                    if start.elapsed() >= timeout {
                        return Ok(None);
                    }
                    std::thread::sleep(poll_interval);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shred_file_rejects_relative_path() {
        let err = shred_file(Path::new("relative/path.txt"), false);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("non-absolute"));
    }

    #[test]
    fn test_shred_file_rejects_path_traversal() {
        let err = shred_file(Path::new("/tmp/../etc/shadow"), false);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains(".."));
    }

    #[test]
    fn test_shred_file_dry_run() {
        // Should succeed without touching the filesystem
        let result = shred_file(Path::new("/tmp/nonexistent_test_file"), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_shred_file_real() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        fs::write(&path, "sensitive data here!!!").unwrap();

        assert!(path.exists());
        let result = shred_file(&path, false);
        assert!(result.is_ok());
        assert!(!path.exists());
    }

    #[test]
    fn test_shred_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        fs::write(&path, "").unwrap();

        let result = shred_file(&path, false);
        assert!(result.is_ok());
        assert!(!path.exists());
    }

    #[test]
    fn test_shred_directory_dry_run() {
        let result = shred_directory(Path::new("/tmp/nonexistent_dir"), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_shred_directory_real() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("subdir");
        fs::create_dir(&sub).unwrap();
        fs::write(dir.path().join("file1.txt"), "secret1").unwrap();
        fs::write(sub.join("file2.txt"), "secret2").unwrap();

        let target = dir.path().to_path_buf();
        let result = shred_directory(&target, false);
        assert!(result.is_ok());
        assert!(!target.exists());
    }

    #[test]
    fn test_execute_command_dry_run() {
        let argv = vec!["echo".to_string(), "hello".to_string()];
        let result = execute_command(&argv, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_execute_command_empty() {
        let argv: Vec<String> = vec![];
        let result = execute_command(&argv, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_execute_command_real() {
        let argv = vec!["true".to_string()];
        let result = execute_command(&argv, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_shred_directory_rejects_relative() {
        let err = shred_directory(Path::new("relative/dir"), false);
        assert!(err.is_err());
    }
}
