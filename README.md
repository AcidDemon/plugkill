# plugkill

![plugkill banner](assets/plugkill-banner-small.png)

A hardware kill-switch daemon for Linux. Monitors USB, Thunderbolt, and SD card buses and shuts down the system when unauthorized device changes are detected.

## What it does

plugkill continuously polls your hardware buses. The moment a device is added, removed, or tampered with, it fires a configurable kill sequence: shreds files, runs custom commands, wipes swap, and powers off. It can also run in learning mode to audit device changes without acting on them, and exposes a Unix socket for runtime control.

## Features

- **Multi-bus monitoring** — USB, Thunderbolt/USB4, and SD/MMC/SDIO, each independently toggleable
- **Selective bus control** — disable individual buses via config (`watch_usb = false`) or CLI (`--no-usb`)
- **Learning mode** — log violations without triggering the kill sequence; switch at runtime via socket
- **Runtime control socket** — disarm/arm, switch modes, reload config, query status over a Unix domain socket
- **Config hot-reload** — change whitelists or settings without restarting the daemon
- **Secure destruction** — multi-pass file shredding, swap wiping, binary self-destruct
- **Hardened systemd integration** — ships with a NixOS module; includes a reference unit file for other distros

### Monitored buses

| Bus | Sysfs path | Whitelist key | Config section |
|-----|-----------|---------------|----------------|
| USB | `/sys/bus/usb/devices` | `vendor_id` + `product_id` (with count) | `[whitelist]` |
| Thunderbolt/USB4 | `/sys/bus/thunderbolt/devices` | `unique_id` (UUID) | `[thunderbolt_whitelist]` |
| SD/MMC/SDIO | `/sys/bus/mmc/devices` | `serial` (hex) | `[sdcard_whitelist]` |

## Getting started

### 1. Build

```bash
git clone https://github.com/AcidDemon/plugkill.git
cd plugkill
cargo build --release
sudo install -m 755 target/release/plugkill /usr/local/bin/
```

Or on NixOS, add the flake input and enable the module (see [NixOS section](#nixos-flake) below).

### 2. Discover your devices

Run these as any user — no root required:

```bash
plugkill --list-devices          # show all USB, Thunderbolt, and SD devices with details
plugkill --generate-whitelist    # generate whitelist TOML you can paste into your config
```

### 3. Create a config

```bash
sudo mkdir -p /etc/plugkill /var/log/plugkill /run/plugkill
plugkill --default-config | sudo tee /etc/plugkill/config.toml > /dev/null
sudo chmod 600 /etc/plugkill/config.toml
```

Edit `/etc/plugkill/config.toml` and paste in the whitelist output from step 2. Review the `[destruction]` and `[commands]` sections.

### 4. Test with dry-run

```bash
sudo plugkill --dry-run
```

This logs what would happen on a violation without actually shredding anything or shutting down. Plug or unplug a device to see it trigger.

### 5. Test with learning mode

```bash
sudo plugkill --learn-mode
```

Like dry-run but for the violation logic only: the daemon runs normally, logs every violation it would have acted on, but never fires the kill sequence. Useful for validating your whitelist in production before switching to enforce.

### 6. Run for real

```bash
sudo plugkill
```

Or via systemd (see below).

## Runtime control

While the daemon is running, you can control it from another terminal using the same binary:

```bash
sudo plugkill --status              # JSON status: armed, mode, uptime, device counts, etc.
sudo plugkill --disarm 300           # disarm for 5 minutes (mandatory timeout, max 1 hour)
sudo plugkill --arm                  # re-arm immediately; re-captures baselines
sudo plugkill --learn                # switch to learning mode at runtime
sudo plugkill --enforce              # switch back to enforce mode
sudo plugkill --reload               # hot-reload config without restarting
```

These commands connect to the daemon's Unix socket at `/run/plugkill/plugkill.sock` (override with `--socket`). The protocol is line-delimited JSON, so you can also script it directly:

```bash
echo '{"command":"status"}' | sudo socat - UNIX-CONNECT:/run/plugkill/plugkill.sock
```

### Disarm / arm behavior

- **Disarm requires a timeout** — there is no indefinite disarm. Maximum is 3600 seconds (1 hour).
- **On re-arm** (timeout expiry or manual `--arm`), baselines are re-captured from whatever devices are currently connected.

## Configuration

Generate the default with `plugkill --default-config`. The file is TOML:

```toml
[general]
sleep_ms = 250                                    # polling interval in ms (50-10000)
log_file = "/var/log/plugkill/plugkill.log"       # kill event log
watch_usb = true                                  # monitor USB bus
watch_thunderbolt = true                          # monitor Thunderbolt bus
watch_sdcard = true                               # monitor SD/MMC bus

[whitelist]
devices = [
  { vendor_id = "1d6b", product_id = "0002", count = 3 },
]

[thunderbolt_whitelist]
devices = [
  # { unique_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" },
]

[sdcard_whitelist]
devices = [
  # { serial = "0x12345678" },
]

[destruction]
files_to_remove = []             # files to securely shred (3-pass random overwrite)
folders_to_remove = []           # directories to recursively shred
melt_self = false                # delete plugkill binary and config after kill
do_sync = true                   # sync filesystems before shutdown
do_wipe_swap = false             # overwrite swap partition
# swap_device = "/dev/sda2"     # required if do_wipe_swap = true

[commands]
kill_commands = [
  # ["/usr/bin/truecrypt", "--dismount"],
]
```

### Security requirements

- Config file **must** be owned by root (uid 0)
- Config file **must not** be group-writable or world-writable
- All paths must be absolute — no relative paths, no `..` traversal
- Kill command binaries must use absolute paths

## CLI reference

```
plugkill [OPTIONS]

Daemon options:
  -c, --config <PATH>       Config file path [default: /etc/plugkill/config.toml]
      --dry-run             Log actions without executing them
      --learn-mode          Start in learning mode (log violations, don't kill)
      --no-usb              Disable USB monitoring
      --no-thunderbolt      Disable Thunderbolt monitoring
      --no-sdcard           Disable SD card monitoring
      --socket <PATH>       Control socket path [default: /run/plugkill/plugkill.sock]

Client commands (connect to running daemon):
      --status              Query daemon status (JSON)
      --disarm <SECONDS>    Disarm for N seconds (1-3600)
      --arm                 Re-arm and re-capture baselines
      --learn               Switch to learning mode
      --enforce             Switch to enforce mode
      --reload              Hot-reload configuration

Utility (no root required):
      --default-config      Print default configuration and exit
      --list-devices        List connected devices with details
      --generate-whitelist  Generate whitelist TOML from connected devices

  -h, --help                Print help
  -V, --version             Print version
```

## Installation

### NixOS (flake)

```nix
# flake.nix
{
  inputs.plugkill.url = "github:AcidDemon/plugkill";

  outputs = { self, nixpkgs, plugkill, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        plugkill.nixosModules.default
        {
          services.plugkill = {
            enable = true;
            settings = {
              general.sleep_ms = 250;
              whitelist.devices = [
                { vendor_id = "1d6b"; product_id = "0002"; count = 3; }
              ];
              thunderbolt_whitelist.devices = [
                { unique_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"; }
              ];
              sdcard_whitelist.devices = [
                { serial = "0x12345678"; }
              ];
              destruction = {
                files_to_remove = [ "/home/user/secrets.tar.gpg" ];
                do_sync = true;
              };
            };
          };
        }
      ];
    };
  };
}
```

The NixOS module runs plugkill as a hardened systemd service with restrictive capabilities, filesystem protections, network isolation, and a `RuntimeDirectory` for the control socket.

### Cargo (any Linux distribution)

```bash
git clone https://github.com/AcidDemon/plugkill.git
cd plugkill
cargo build --release
sudo install -m 755 target/release/plugkill /usr/local/bin/
sudo mkdir -p /etc/plugkill /var/log/plugkill /run/plugkill
plugkill --default-config | sudo tee /etc/plugkill/config.toml > /dev/null
sudo chmod 600 /etc/plugkill/config.toml
```

### Systemd service (non-NixOS)

Create `/etc/systemd/system/plugkill.service`:

```ini
[Unit]
Description=plugkill hardware kill-switch daemon
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/local/bin/plugkill --config /etc/plugkill/config.toml
Restart=on-failure
RestartSec=5
RuntimeDirectory=plugkill
RuntimeDirectoryMode=0755
Environment=RUST_LOG=info

# Hardening
ProtectSystem=strict
ReadWritePaths=/var/log/plugkill /run/plugkill
PrivateTmp=true
NoNewPrivileges=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_UNIX
MemoryDenyWriteExecute=true
UMask=0077

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now plugkill.service
```

## Use cases

- **Anti-forensic dead man's switch** — attach a USB key to your wrist; if the machine is seized, the key pulls out and the system shuts down
- **Prevent BadUSB / rubber ducky attacks** — any unauthorized USB insertion triggers immediate shutdown
- **Block Thunderbolt DMA attacks** — detect new physical connections before device authorization
- **Audit hardware changes** — run in learning mode to log every device event without acting on it
- **Production hardening** — detect unauthorized SD card or USB insertion on embedded/kiosk systems

## How it works

1. On startup, plugkill captures a **baseline snapshot** of all connected devices on each active bus
2. Every 250ms (configurable), it polls the current device state and compares against the baseline + whitelists
3. If any unauthorized change is detected:
   - In **enforce mode**: the kill sequence fires (mask signals, shred files, run commands, sync, wipe swap, self-destruct, power off)
   - In **learn mode**: the violation is logged and counted, but no action is taken
4. If device enumeration itself fails, this is treated as tampering
5. Buses that lack hardware (no Thunderbolt controller, no MMC bus) are silently skipped

## Origin

A from-scratch Rust rewrite of the original [usbkill](https://github.com/hephaest0s/usbkill) by [Hephaestos](https://github.com/hephaest0s). Extended with Thunderbolt/SD card monitoring, runtime control, learning mode, and config hot-reload.

## Platform support

| Platform | Architecture | Status |
|----------|-------------|--------|
| Linux | x86_64 | Supported |
| Linux | aarch64 | Supported |

plugkill is Linux-only. It reads from sysfs (`/sys/bus/`) and uses the `reboot(2)` syscall for shutdown.

## License

GPL-3.0 — see [LICENSE](LICENSE) for details.
