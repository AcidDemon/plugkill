use base64::prelude::*;
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn test_generate_keys_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_plugkill-relay"))
        .arg("--generate-keys")
        .output()
        .expect("failed to run plugkill-relay");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Private key"));
    assert!(stdout.contains("Public key"));

    // Extract base64 lines (non-label, non-empty)
    let key_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| !l.contains("Private") && !l.contains("Public") && !l.is_empty())
        .collect();
    assert_eq!(key_lines.len(), 2);

    // Both should be valid 32-byte base64
    for line in &key_lines {
        let decoded = BASE64_STANDARD.decode(line.trim()).unwrap();
        assert_eq!(decoded.len(), 32);
    }
}

#[test]
fn test_default_config_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_plugkill-relay"))
        .arg("--default-config")
        .output()
        .expect("failed to run plugkill-relay");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("listen_port"));
    assert!(stdout.contains("private_key_file"));
    assert!(stdout.contains("[[peers]]"));
}

#[test]
fn test_show_pubkey() {
    // Generate keys, extract private key, write to file, verify --show-pubkey matches
    let gen_output = std::process::Command::new(env!("CARGO_BIN_EXE_plugkill-relay"))
        .arg("--generate-keys")
        .output()
        .expect("failed to run plugkill-relay");

    let stdout = String::from_utf8(gen_output.stdout).unwrap();
    let key_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| !l.contains("Private") && !l.contains("Public") && !l.is_empty())
        .collect();

    let priv_b64 = key_lines[0].trim();
    let pub_b64 = key_lines[1].trim();

    let mut keyfile = NamedTempFile::new().unwrap();
    keyfile.write_all(priv_b64.as_bytes()).unwrap();
    keyfile.flush().unwrap();

    let show_output = std::process::Command::new(env!("CARGO_BIN_EXE_plugkill-relay"))
        .arg("--show-pubkey")
        .arg(keyfile.path())
        .output()
        .expect("failed to run plugkill-relay");

    assert!(show_output.status.success());
    let shown_pub = String::from_utf8(show_output.stdout).unwrap();
    assert_eq!(shown_pub.trim(), pub_b64);
}
