#![allow(deprecated)] // cargo_bin deprecation — acceptable for tests

use assert_cmd::Command;
use std::fs;
use std::io::Write;
use tempfile::TempDir;

/// Helper: create a temp config file with given TOML content and return (dir, path).
fn write_config(content: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    (dir, path)
}

#[test]
fn test_cli_help() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("Hardware kill-switch daemon"));
}

#[test]
fn test_cli_version() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("plugkill"));
}

#[test]
fn test_default_config_output() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--default-config")
        .assert()
        .success()
        .stdout(predicates::str::contains("[general]"))
        .stdout(predicates::str::contains("sleep_ms"))
        .stdout(predicates::str::contains("[whitelist]"))
        .stdout(predicates::str::contains("[thunderbolt_whitelist]"))
        .stdout(predicates::str::contains("[sdcard_whitelist]"))
        .stdout(predicates::str::contains("[destruction]"))
        .stdout(predicates::str::contains("[commands]"))
        .stdout(predicates::str::contains("watch_usb"))
        .stdout(predicates::str::contains("watch_thunderbolt"))
        .stdout(predicates::str::contains("watch_sdcard"));
}

#[test]
fn test_refuses_without_root() {
    if nix::unistd::geteuid().is_root() {
        return;
    }

    let (_dir, path) = write_config("");

    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--config")
        .arg(path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("must run as root"));
}

#[test]
fn test_invalid_config_path() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }

    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--config")
        .arg("/nonexistent/path/config.toml")
        .assert()
        .failure()
        .stderr(predicates::str::contains("failed to load config"));
}

#[test]
fn test_list_devices_no_root() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--list-devices")
        .assert()
        .success()
        .stdout(predicates::str::contains("USB devices"));
}

#[test]
fn test_generate_whitelist_no_root() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--generate-whitelist")
        .assert()
        .success()
        .stdout(predicates::str::contains("[whitelist]"));
}

#[test]
fn test_malformed_config() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }

    let (_dir, path) = write_config("this is not valid toml [[[");

    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--config")
        .arg(path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("failed to load config"));
}

// --- New tests for selective monitoring flags ---

#[test]
fn test_help_mentions_bus_flags() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("--no-usb"))
        .stdout(predicates::str::contains("--no-thunderbolt"))
        .stdout(predicates::str::contains("--no-sdcard"));
}

#[test]
fn test_help_mentions_client_flags() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("--disarm"))
        .stdout(predicates::str::contains("--arm"))
        .stdout(predicates::str::contains("--status"))
        .stdout(predicates::str::contains("--learn"))
        .stdout(predicates::str::contains("--enforce"))
        .stdout(predicates::str::contains("--reload"));
}

#[test]
fn test_help_mentions_learn_mode() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("--learn-mode"));
}

#[test]
fn test_client_status_fails_no_daemon() {
    // When no daemon is running, --status should fail gracefully
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--status")
        .arg("--socket")
        .arg("/tmp/plugkill-test-nonexistent.sock")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot connect"));
}

#[test]
fn test_client_disarm_fails_no_daemon() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--disarm")
        .arg("60")
        .arg("--socket")
        .arg("/tmp/plugkill-test-nonexistent.sock")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot connect"));
}

#[test]
fn test_client_arm_fails_no_daemon() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--arm")
        .arg("--socket")
        .arg("/tmp/plugkill-test-nonexistent.sock")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot connect"));
}

#[test]
fn test_client_reload_fails_no_daemon() {
    Command::cargo_bin("plugkill")
        .unwrap()
        .arg("--reload")
        .arg("--socket")
        .arg("/tmp/plugkill-test-nonexistent.sock")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot connect"));
}
