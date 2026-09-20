# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [semantic versioning](https://semver.org/spec/v2.0.0.html).
While the major version is 0 the control protocol and the config schema may
change between minor versions, and any such change is listed here.

## [0.1.0] - 2026-09-20

First release.

### Watching

- Eight buses: USB, Thunderbolt/USB4, SD/MMC/SDIO, power supply, network,
  lid, PCI and display. Power, network and lid are off by default.
- Whitelists per device for USB (`vendor_id`, `product_id` and a count),
  Thunderbolt (`unique_id`) and SD card (`serial`). Ignore lists for PCI
  addresses and display connectors. Policies with a grace period for power,
  network and lid, which are events rather than devices.
- Displays are identified by the monitor's EDID rather than by the connector,
  so a monitor moved between ports is the same monitor and a different monitor
  on the same port is a change.
- A baseline is captured at startup and again on `--arm`, on `--reload` for a
  bus that was just switched on, and when a disarm expires.

### Killing

- Mask signals, shred the configured files, run the configured commands, sync,
  wipe swap, delete the binary, power off.
- Learning mode logs and counts a violation instead of acting on it. It covers
  locally detected violations only: a signature-verified `kill` from a relay
  peer is logged and refused, and the relay answers a refusal by powering its
  own machine off.
- `--dry-run` logs the whole sequence without running any of it.

### Control

- A line-delimited JSON protocol on a Unix socket, spoken by the same binary:
  `--status`, `--disarm`, `--arm`, `--learn`, `--enforce`, `--reload`.
- Disarm always takes a timeout, at most an hour. There is no indefinite
  disarm.
- The socket is root-only unless `--socket-group` names a group. `kill` is
  refused unless the peer's uid is 0.
- `[general] require_auth` puts disarm, learn, reload, `--pair` and
  `--allow-last` behind polkit, which asks for the caller's own password and
  caches nothing. Arm, enforce and revoking an allowance only make the daemon
  stricter, so they stay open. The subject asked about is the caller's logind
  session, not its pid.
- Runtime allowances let one device through without disarming every bus:
  `--pair` for the next device to appear, `--allow-last` for the one that
  caused the last violation, both revocable and both optionally expiring. An
  allowed device is kept out of the baseline, so revoking it means something.
  Allowances live in memory and are cleared by a restart.
- The last 50 violations are kept in memory for `--violations` and for the
  GUI. They are never written to disk.
- `--allowances --toml` and `--allowances --nix` print config text to paste.
  Nothing writes a config file.

### Interfaces

- `plugkill-gui`, an optional tray icon that never runs as root and sends only
  commands a socket group member may send. The icon carries the state in its
  shape; left click opens a dashboard with a tile per bus; right click opens a
  menu with the disarm presets, the mode switch and a per-bus reading.
- A settings window for building a config by clicking: every section, the
  devices the daemon can see nested by the port they hang off, the violation
  history, the allowance table and a diagnostics page. It writes no files. The
  TOML and Nix it shows are parsed back through the daemon's own loader before
  they are displayed, and you copy them out yourself.
- `plugkill-relay`, which fans out ed25519-signed kill messages to configured
  peers over UDP, so one machine going down can take its mesh with it.

### Platforms

- Linux on x86_64 and aarch64, FreeBSD on x86_64, from one codebase with the
  backend chosen at compile time.
- Not available on FreeBSD: Thunderbolt monitoring, which has no per-device
  `unique_id` to enumerate; `require_locked`, which needs logind; connector
  identity for displays, because the devd event does not name the connector;
  and polkit, so `require_auth` there refuses every non-root caller rather
  than prompting.

### Packaging

- A Nix flake with the daemon and the GUI as separate outputs, and NixOS
  modules for both the daemon and the relay.
- rc.d scripts for FreeBSD, a systemd unit in the README, and a polkit action
  file for `require_auth`.

[0.1.0]: https://github.com/AcidDemon/plugkill/releases/tag/v0.1.0
