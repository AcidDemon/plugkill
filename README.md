# usbkill

An anti-forensic USB kill-switch daemon that monitors your USB ports and immediately shuts down the system when unauthorized device changes are detected.

`usbkill` continuously polls connected USB devices and triggers a configurable kill sequence — securely shredding files, executing custom commands, wiping swap, and powering off — the moment a device is added, removed, or tampered with.

## Origin

This is a from-scratch Rust rewrite of the original [usbkill](https://github.com/hephaest0s/usbkill) by [Hephaestos](https://github.com/hephaest0s), which was written in Python.

### Why rewrite in Rust?

- **Single static binary** — no runtime dependencies, trivial to deploy and audit
- **Memory safety** — eliminates entire classes of vulnerabilities (buffer overflows, use-after-free) without a garbage collector
- **Smaller attack surface** — no Python interpreter, no pip packages, fewer moving parts on a security-critical daemon
- **Performance** — compiled native code with minimal overhead for real-time USB monitoring
- **Systemd integration** — ships with a hardened NixOS module and can easily be adapted for other init systems

## How it works

1. On startup, `usbkill` captures a **baseline snapshot** of all connected USB devices via `/sys/bus/usb/devices`
2. Every 250ms (configurable), it polls the current device state and compares it against the baseline and an optional whitelist
3. If **any** unauthorized change is detected — a new device plugged in, a baseline device removed, or device counts changed — the **kill sequence** fires:
   - Signals are masked (SIGINT/SIGTERM ignored so the sequence can't be interrupted)
   - Configured files and directories are securely shredded (3-pass random overwrite)
   - Custom kill commands are executed (e.g. dismount encrypted volumes)
   - Filesystems are synced
   - Swap is wiped (optional)
   - The binary and config self-destruct (optional)
   - The system powers off
4. If USB enumeration itself fails, this is treated as tampering and also triggers the kill sequence

## Use cases

- Prevent forensic data extraction via USB devices
- Dead man's switch: attach a USB key to your wrist — if the machine is taken, the key pulls out and the system shuts down
- Protect against rubber ducky / BadUSB attacks
- Prevent unauthorized USB storage access

## Installation

### NixOS (flake)

Add the flake to your inputs and enable the module:

```nix
# flake.nix
{
  inputs.usbkill.url = "github:AcidDemon/usbkill-rs";

  outputs = { self, nixpkgs, usbkill, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        usbkill.nixosModules.default
        {
          services.usbkill = {
            enable = true;
            settings = {
              general.sleep_ms = 250;
              whitelist.devices = [
                { vendor_id = "1d6b"; product_id = "0002"; count = 3; }
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

The NixOS module runs `usbkill` as a hardened systemd service with restrictive capabilities, filesystem protections, and network isolation out of the box.

### Cargo (any Linux distribution)

```bash
# Build from source
git clone https://github.com/AcidDemon/usbkill-rs.git
cd usbkill-rs
cargo build --release

# Install the binary
sudo install -m 755 target/release/usbkill /usr/local/bin/

# Create config directory and default config
sudo mkdir -p /etc/usbkill /var/log/usbkill
sudo ./target/release/usbkill --default-config | sudo tee /etc/usbkill/config.toml
sudo chmod 600 /etc/usbkill/config.toml
```

### Systemd service (non-NixOS)

Create `/etc/systemd/system/usbkill.service`:

```ini
[Unit]
Description=usbkill USB kill-switch daemon
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/local/bin/usbkill --config /etc/usbkill/config.toml
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info

# Hardening
ProtectSystem=strict
ReadWritePaths=/var/log/usbkill
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

Then enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now usbkill.service
```

### Arch Linux (manual)

```bash
# Install Rust if not present
sudo pacman -S rust

# Build and install
git clone https://github.com/AcidDemon/usbkill-rs.git
cd usbkill-rs
cargo build --release
sudo install -m 755 target/release/usbkill /usr/local/bin/
```

### Fedora / RHEL

```bash
sudo dnf install cargo
git clone https://github.com/AcidDemon/usbkill-rs.git
cd usbkill-rs
cargo build --release
sudo install -m 755 target/release/usbkill /usr/local/bin/
```

### Debian / Ubuntu

```bash
sudo apt install cargo
git clone https://github.com/AcidDemon/usbkill-rs.git
cd usbkill-rs
cargo build --release
sudo install -m 755 target/release/usbkill /usr/local/bin/
```

## Usage

```
usbkill [OPTIONS]

Options:
  -c, --config <PATH>   Path to config file [default: /etc/usbkill/config.toml]
      --dry-run          Log actions without executing them
      --default-config   Print default configuration and exit
  -h, --help             Print help
  -V, --version          Print version
```

Run directly (must be root):

```bash
sudo usbkill                          # Use default config
sudo usbkill --config ./my-config.toml
sudo usbkill --dry-run                # Test without executing destructive actions
```

## Configuration

The configuration file uses TOML format. Generate the default with `usbkill --default-config`.

```toml
[general]
sleep_ms = 250                                  # Polling interval in ms (50–10000)
log_file = "/var/log/usbkill/usbkill.log"      # Kill event log
dry_run = false

[whitelist]
devices = [
  { vendor_id = "1d6b", product_id = "0002", count = 3 },
  # { vendor_id = "abcd", product_id = "1234", count = 1 },
]

[destruction]
files_to_remove = []             # Files to securely shred
folders_to_remove = []           # Directories to recursively shred
melt_self = false                # Delete usbkill binary and config after kill
do_sync = true                   # Sync filesystems before shutdown
do_wipe_swap = false             # Overwrite swap partition
# swap_device = "/dev/sda2"     # Required if do_wipe_swap = true

[commands]
kill_commands = [
  # ["/usr/bin/truecrypt", "--dismount"],
  # ["/usr/bin/shred", "-vfz", "-n", "3", "/path/to/secret"],
]
```

### Security requirements

- The config file **must** be owned by root
- The config file **must not** be world-writable or group-writable
- All paths must be absolute (no relative paths, no `..` traversal)
- Kill commands must use absolute paths to the executable

## Platform support

| Platform | Architecture | Status |
|----------|-------------|--------|
| Linux    | x86_64      | Supported |
| Linux    | aarch64     | Supported |

`usbkill` is Linux-only — it reads from `/sys/bus/usb/devices` (sysfs) and uses the `reboot(2)` syscall for shutdown.

## License

GPL-3.0 — see [LICENSE](LICENSE) for details.

## Acknowledgements

Based on the original [usbkill](https://github.com/hephaest0s/usbkill) by [Hephaestos](https://github.com/hephaest0s).
