use log::{debug, error, info, warn};
use plugkill_core::config::Config;
use plugkill_core::error::Error;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path};
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
///
/// `config_path` is the `--config` path this config was loaded from; `melt_self`
/// removes that directory rather than a hardcoded one.
pub fn execute_kill_sequence(
    config: &Config,
    config_path: &Path,
    reason: &str,
) -> Result<(), Error> {
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

    log_kill_event(config, reason);

    for path in &config.destruction.files_to_remove {
        if let Err(e) = shred_file(path, dry_run) {
            error!("failed to shred file {}: {e}", path.display());
        }
    }

    for path in &config.destruction.folders_to_remove {
        if let Err(e) = shred_directory(path, dry_run) {
            error!("failed to shred directory {}: {e}", path.display());
        }
    }

    for (i, cmd) in config.commands.kill_commands.iter().enumerate() {
        if let Err(e) = execute_command(cmd, dry_run) {
            error!("kill command {i} failed: {e}");
        }
    }

    if config.destruction.do_sync {
        info!("syncing filesystems");
        if !dry_run {
            // SAFETY: sync() takes no args and has no failure mode.
            unsafe { libc::sync() };
        }
    }

    if config.destruction.do_wipe_swap
        && let Some(ref device) = config.destruction.swap_device
        && let Err(e) = wipe_swap(device, dry_run)
    {
        error!("swap wipe failed: {e}");
    }

    if config.destruction.melt_self {
        melt_self(dry_run, config_path);
    }

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
    // /proc/uptime as a rough timestamp; avoids a chrono dependency.
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
/// - Not effective against COW/journaling filesystems; use full disk encryption
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

    // A symlink is removed, never followed: the target is not what the config
    // named for destruction.
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
                "file {} has {} hardlinks, data may remain accessible via other links",
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

    let mut urandom = File::open("/dev/urandom")
        .map_err(|e| Error::Kill(format!("cannot open /dev/urandom: {e}")))?;

    // One fd for every pass: reopening between passes would let the path be
    // swapped for another file under us.
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

    drop(file);

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
                if let Err(e) = shred_directory(&entry_path, dry_run) {
                    error!("failed to shred subdirectory {}: {e}", entry_path.display());
                }
            } else if let Err(e) = shred_file(&entry_path, dry_run) {
                error!("failed to shred file {}: {e}", entry_path.display());
            }
        }

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
            // Timed out; kill the child process
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

    let status = Command::new("swapoff")
        .arg(device)
        .status()
        .map_err(|e| Error::Kill(format!("failed to run swapoff: {e}")))?;

    if !status.success() {
        warn!("swapoff failed for {device}, continuing anyway");
    }

    let status = Command::new("dd")
        .args(["if=/dev/urandom", &format!("of={device}"), "bs=1M"])
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) => {
            // dd "fails" with an exit code when it hits end of device, which is expected
            debug!("dd overwrite of {device} finished with status: {s}");
        }
        Err(e) => warn!("dd failed for {device}: {e}"),
    }

    // swapon (best effort)
    let _ = Command::new("swapon").arg(device).status();

    Ok(())
}

/// The directory `melt_self` removes: the one holding the config that was
/// loaded, so FreeBSD's `/usr/local/etc/plugkill` is not missed and a custom
/// `--config` is not left behind.
///
/// `None` unless that directory is an absolute, traversal-free path whose
/// final component is literally named `plugkill`. Both documented layouts
/// (`/etc/plugkill`, `/usr/local/etc/plugkill`) match; anything else refuses,
/// since this feeds a `remove_dir_all` run right before poweroff and a wrong
/// guess (e.g. `--config /usr/local/plugkill.toml` shredding `/usr/local`)
/// cannot be undone or even diagnosed afterward.
fn config_dir_to_remove(config_path: &Path) -> Option<&Path> {
    let parent = config_path.parent()?;
    if !parent.is_absolute() || parent.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    (parent.file_name() == Some(OsStr::new("plugkill"))).then_some(parent)
}

/// Remove the plugkill binary, the directory holding the loaded config, and
/// the log directory `/var/log/plugkill`.
fn melt_self(dry_run: bool, config_path: &Path) {
    let target = config_dir_to_remove(config_path);

    if dry_run {
        match target {
            Some(dir) => info!(
                "[DRY RUN] would melt self (remove binary, {} and log directory)",
                dir.display()
            ),
            None => info!(
                "[DRY RUN] would melt self (remove binary and log directory; would refuse to remove config directory of {}, not a directory named plugkill)",
                config_path.display()
            ),
        }
        return;
    }

    // Emit the refusal, if any, before any removal happens: it is the one
    // diagnostic that matters here, and it should get the most possible time
    // to reach its destination before the log directory disappears too.
    if target.is_none() {
        warn!(
            "refusing to remove the config directory of {}: not a directory named plugkill",
            config_path.display()
        );
    }

    info!("melting self, removing binary, config directory and log directory");

    if let Ok(exe) = std::env::current_exe()
        && let Err(e) = fs::remove_file(&exe)
    {
        error!("cannot remove own binary {}: {e}", exe.display());
    }

    if let Some(dir) = target
        && let Err(e) = fs::remove_dir_all(dir)
    {
        error!("cannot remove config directory {}: {e}", dir.display());
    }

    if let Err(e) = fs::remove_dir_all("/var/log/plugkill") {
        error!("cannot remove /var/log/plugkill: {e}");
    }
}

/// Power off: reboot(2) syscall first (can't be blocked), command as fallback.
fn shutdown() -> Result<(), Error> {
    #[cfg(target_os = "linux")]
    {
        info!("calling reboot(RB_POWER_OFF)");
        match nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF) {
            Ok(infallible) => match infallible {},
            Err(e) => error!("reboot syscall failed: {e}, falling back to poweroff command"),
        }
        poweroff_command("poweroff", &["-f"])
    }
    #[cfg(target_os = "freebsd")]
    {
        info!("calling reboot(RB_POWEROFF)");
        // SAFETY: reboot() takes one int flag and does not return on success.
        unsafe { libc::reboot(libc::RB_POWEROFF) };
        error!(
            "reboot syscall returned ({}), falling back to shutdown -p",
            std::io::Error::last_os_error()
        );
        poweroff_command("shutdown", &["-p", "now"])
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        poweroff_command("shutdown", &["-h", "now"])
    }
}

/// Run the platform power-off command as the syscall fallback.
fn poweroff_command(cmd: &str, args: &[&str]) -> Result<(), Error> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| Error::Kill(format!("{cmd} command failed: {e}")))?;
    if !status.success() {
        return Err(Error::Kill(format!("{cmd} exited with status: {status}")));
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

    #[test]
    fn test_config_dir_to_remove() {
        assert_eq!(
            config_dir_to_remove(Path::new("/etc/plugkill/config.toml")),
            Some(Path::new("/etc/plugkill"))
        );
        // FreeBSD path from the README.
        assert_eq!(
            config_dir_to_remove(Path::new("/usr/local/etc/plugkill/config.toml")),
            Some(Path::new("/usr/local/etc/plugkill"))
        );
        // Parent is a top-level directory, not named plugkill: refuse, that
        // would shred /etc.
        assert_eq!(config_dir_to_remove(Path::new("/etc/plugkill.toml")), None);
        assert_eq!(config_dir_to_remove(Path::new("/config.toml")), None);
        // Config path at the filesystem root itself: parent() is None.
        assert_eq!(config_dir_to_remove(Path::new("/")), None);
        // Two levels deep but not plugkill's own directory: refuse. A depth
        // check alone would have accepted these and shredded someone else's
        // directory.
        assert_eq!(
            config_dir_to_remove(Path::new("/usr/local/plugkill.toml")),
            None
        );
        assert_eq!(
            config_dir_to_remove(Path::new("/home/alice/plugkill.toml")),
            None
        );
        assert_eq!(
            config_dir_to_remove(Path::new("/var/lib/config.toml")),
            None
        );
        // Relative paths and traversal: refuse.
        assert_eq!(config_dir_to_remove(Path::new("config.toml")), None);
        assert_eq!(
            config_dir_to_remove(Path::new("etc/plugkill/config.toml")),
            None
        );
        assert_eq!(
            config_dir_to_remove(Path::new("/etc/plugkill/../../config.toml")),
            None
        );
    }

    #[test]
    fn test_melt_self_dry_run_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join("plugkill");
        fs::create_dir(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("config.toml");
        fs::write(&cfg_path, "").unwrap();

        melt_self(true, &cfg_path);

        assert!(cfg_path.exists());
        assert!(cfg_dir.exists());
    }
}
