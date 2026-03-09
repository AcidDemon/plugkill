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
    Command::cargo_bin("usbkill")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("USB kill-switch daemon"));
}

#[test]
fn test_cli_version() {
    Command::cargo_bin("usbkill")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("usbkill"));
}

#[test]
fn test_default_config_output() {
    Command::cargo_bin("usbkill")
        .unwrap()
        .arg("--default-config")
        .assert()
        .success()
        .stdout(predicates::str::contains("[general]"))
        .stdout(predicates::str::contains("sleep_ms"))
        .stdout(predicates::str::contains("[whitelist]"))
        .stdout(predicates::str::contains("[destruction]"))
        .stdout(predicates::str::contains("[commands]"));
}

#[test]
fn test_refuses_without_root() {
    if nix::unistd::geteuid().is_root() {
        return;
    }

    let (_dir, path) = write_config("");

    Command::cargo_bin("usbkill")
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

    Command::cargo_bin("usbkill")
        .unwrap()
        .arg("--config")
        .arg("/nonexistent/path/config.toml")
        .assert()
        .failure()
        .stderr(predicates::str::contains("failed to load config"));
}

#[test]
fn test_malformed_config() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }

    let (_dir, path) = write_config("this is not valid toml [[[");

    Command::cargo_bin("usbkill")
        .unwrap()
        .arg("--config")
        .arg(path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("failed to load config"));
}
