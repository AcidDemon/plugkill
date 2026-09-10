<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/logo-watchdog-dark.svg">
  <img alt="plugkill" src="assets/brand/logo-watchdog-light.svg" width="560">
</picture>

A hardware kill-switch daemon for Linux and FreeBSD. It watches the physical state of the machine and powers it off when something changes that you did not authorize: a USB stick appears, the Thunderbolt bus grows a device, the power cable is pulled, the Ethernet cable is unplugged, the lid closes, a monitor is attached.

Before it shuts down it can shred files, run your own commands, wipe swap and delete its own binary.

## What it watches

Each bus can be turned off in the config (`watch_usb = false`) or on the command line (`--no-usb`).

| Bus | Where it reads | Whitelist key | Config section |
|-----|----------------|---------------|----------------|
| USB | `/sys/bus/usb/devices` | `vendor_id` + `product_id`, with a count | `[whitelist]` |
| Thunderbolt/USB4 | `/sys/bus/thunderbolt/devices` | `unique_id` (UUID) | `[thunderbolt_whitelist]` |
| SD/MMC/SDIO | `/sys/bus/mmc/devices` | `serial` (hex) | `[sdcard_whitelist]` |
| Power supply | `/sys/class/power_supply` | none, policy based | `[power]` |
| Network | `/sys/class/net` | none, interface filter | `[network]` |
| Lid | D-Bus logind, `/proc/acpi` fallback | none, policy based | `[lid]` |
| PCI | `/sys/bus/pci/devices` | none, ignore list | `[pci]` |
| Display | `/sys/class/drm` | none, ignore list | `[display]` |

Power, network and lid monitoring are off by default. The PCI monitor is worth enabling on machines with Thunderbolt: a TB device that tunnels PCIe shows up as a new PCI device, so PCI catches it even where per-device whitelisting is not available.

## Quick start

```bash
git clone https://github.com/AcidDemon/plugkill.git
cd plugkill
cargo build --release
sudo install -m 755 target/release/plugkill /usr/local/bin/
```

Find out what is currently plugged in. Neither command needs root:

```bash
plugkill --list-devices          # USB, Thunderbolt and SD devices with details
plugkill --generate-whitelist    # the same devices as TOML you can paste into the config
```

Write a config and paste the whitelist into it:

```bash
sudo mkdir -p /etc/plugkill /var/log/plugkill /run/plugkill
plugkill --default-config | sudo tee /etc/plugkill/config.toml > /dev/null
sudo chmod 600 /etc/plugkill/config.toml
```

Review `[destruction]` and `[commands]` before going any further, then try it without the consequences:

```bash
sudo plugkill --dry-run     # logs what it would do, shreds nothing, shuts nothing down
```

Plug or unplug something to see it trigger. Once the whitelist looks right, learning mode runs the real daemon but never fires the kill sequence, which is the safer way to validate a whitelist in production:

```bash
sudo plugkill --learn-mode
```

Then run it for real with `sudo plugkill`, or under systemd (see below).

## Runtime control

The daemon listens on a Unix socket at `/run/plugkill/plugkill.sock`, and the same binary talks to it:

```bash
sudo plugkill --status              # JSON: armed, mode, uptime, device counts
sudo plugkill --disarm 300          # disarm for 5 minutes
sudo plugkill --arm                 # re-arm now, re-capturing baselines
sudo plugkill --learn               # switch to learning mode
sudo plugkill --enforce             # switch back
sudo plugkill --reload              # reload the config without restarting
```

There is no indefinite disarm. A timeout is mandatory and the maximum is 3600 seconds. When the timeout expires, or when you re-arm by hand, plugkill re-captures its baselines from whatever is connected at that moment.

The protocol is line-delimited JSON, so scripting it directly works too:

```bash
echo '{"command":"status"}' | sudo socat - UNIX-CONNECT:/run/plugkill/plugkill.sock
```

## Configuration

`plugkill --default-config` prints a commented starting point. The interesting parts:

```toml
[general]
sleep_ms = 250                                    # polling interval, 50-10000
log_file = "/var/log/plugkill/plugkill.log"
watch_usb = true
watch_thunderbolt = true
watch_sdcard = true
watch_power = false
watch_network = false
watch_lid = false

[whitelist]
devices = [
  { vendor_id = "1d6b", product_id = "0002", count = 3 },
]

[thunderbolt_whitelist]
devices = []                     # { unique_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" }

[sdcard_whitelist]
devices = []                     # { serial = "0x12345678" }

[power]
policy = "monitor"               # trigger-once, ac-required, monitor
grace_secs = 0                   # 0-300
require_locked = false           # only trigger while the session is locked

[network]
policy = "monitor"               # kill, monitor
grace_secs = 0
interfaces = ["eth0"]            # empty means all physical NICs

[lid]
policy = "monitor"               # kill, monitor
grace_secs = 0

[destruction]
files_to_remove = []             # shredded with a 3-pass random overwrite
folders_to_remove = []           # shredded recursively
melt_self = false                # delete the plugkill binary and config after the kill
do_sync = true
do_wipe_swap = false
# swap_device = "/dev/sda2"      # required when do_wipe_swap = true

[commands]
kill_commands = []               # [["/usr/bin/truecrypt", "--dismount"]]
```

plugkill refuses to start on a config that fails any of these:

- owned by root
- not group- or world-writable
- every path absolute, with no `..` in it
- every kill command binary given by absolute path

## CLI

```
plugkill [OPTIONS]

Daemon:
  -c, --config <PATH>       Config file [default: /etc/plugkill/config.toml]
      --dry-run             Log actions instead of executing them
      --learn-mode          Start in learning mode
      --no-usb              Disable USB monitoring
      --no-thunderbolt      Disable Thunderbolt monitoring
      --no-sdcard           Disable SD card monitoring
      --no-power            Disable power supply monitoring
      --no-network          Disable network link monitoring
      --no-lid              Disable lid close monitoring
      --no-pci              Disable PCI bus monitoring
      --no-display          Disable external display monitoring
      --socket <PATH>       Control socket [default: /run/plugkill/plugkill.sock]

Client, against a running daemon:
      --status              Print daemon status as JSON
      --disarm <SECONDS>    Disarm for N seconds (1-3600)
      --arm                 Re-arm and re-capture baselines
      --learn               Switch to learning mode
      --enforce             Switch to enforce mode
      --reload              Reload the configuration

No root required:
      --default-config      Print the default configuration
      --list-devices        List connected devices with details
      --generate-whitelist  Generate whitelist TOML from connected devices

  -h, --help
  -V, --version
```

## Installing

### NixOS

```nix
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
              general.watch_power = true;
              general.watch_lid = true;
              whitelist.devices = [
                { vendor_id = "1d6b"; product_id = "0002"; count = 3; }
              ];
              power = {
                policy = "trigger-once";
                grace_secs = 30;
                require_locked = true;
              };
              lid = {
                policy = "kill";
                grace_secs = 5;
              };
              destruction.files_to_remove = [ "/home/user/secrets.tar.gpg" ];
            };
          };
        }
      ];
    };
  };
}
```

The module runs plugkill as a hardened systemd service: restricted capabilities, filesystem protections, no network beyond `AF_UNIX`, and a `RuntimeDirectory` for the control socket.

### systemd on other distributions

Build and install as in the quick start, then create `/etc/systemd/system/plugkill.service`:

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

### FreeBSD

Same source tree. USB enumeration links against the libusb in the base system, and `pkgconf` lets the build find it, so the Rust toolchain is the only thing you need to install.

```sh
pkg install rust pkgconf

git clone https://github.com/AcidDemon/plugkill.git
cd plugkill
cargo build --release
install -m 755 target/release/plugkill /usr/local/bin/
install -m 755 target/release/plugkill-relay /usr/local/bin/   # optional

mkdir -p /usr/local/etc/plugkill /var/log/plugkill /var/run/plugkill
plugkill --default-config > /usr/local/etc/plugkill/config.toml
chmod 600 /usr/local/etc/plugkill/config.toml

install -m 755 freebsd/rc.d/plugkill /usr/local/etc/rc.d/plugkill
sysrc plugkill_enable=YES
service plugkill start
```

Test with `plugkill_flags="--dry-run"` in `/etc/rc.conf` first, or run `plugkill --dry-run` by hand. The relay has its own script at `freebsd/rc.d/plugkill-relay` with the `plugkill_relay_enable` knob.

Config lives in `/usr/local/etc/plugkill` and the control socket in `/var/run/plugkill`.

Lid monitoring needs the machine awake long enough for plugkill to see the close, so hand the event to plugkill instead of ACPI:

```sh
sysrc -f /etc/sysctl.conf hw.acpi.lid_switch_state=NONE
sysctl hw.acpi.lid_switch_state=NONE
```

Lid state comes from devd's event socket at `/var/run/devd.pipe`, and devd runs by default.

## How it works

At startup plugkill takes a baseline snapshot of every device on every active bus. Every 250 ms by default it polls again and compares against that baseline plus your whitelists. An unauthorized change fires the kill sequence: mask signals, shred the configured files, run the configured commands, sync, wipe swap, delete itself, power off. In learning mode the violation is logged and counted instead.

If enumeration itself fails, plugkill treats that as tampering rather than as an error to ignore. Buses with no hardware behind them (no Thunderbolt controller, no MMC bus) are skipped without complaint.

Power monitoring tracks AC and battery transitions, with a grace period and optional session lock detection through logind. Network monitoring watches operstate on physical NICs for link-down. Lid monitoring takes a sleep inhibitor so it has a window to act before the machine suspends.

## Platform support

| Platform | Architecture | Status |
|----------|--------------|--------|
| Linux | x86_64 | Supported |
| Linux | aarch64 | Supported |
| FreeBSD | x86_64 | Supported |

One codebase, hardware backend picked at compile time. Linux reads sysfs, asks logind about session lock and lid state, and shuts down through `reboot(2)`. FreeBSD reads USB through libusb, AC through `hw.acpi.acline`, link state through the `SIOCGIFMEDIA` ioctl, PCI through `pciconf -l`, lid and display through devd events, and shuts down through `reboot(RB_POWEROFF)`.

Two things are missing on FreeBSD. Thunderbolt monitoring is unavailable, because there is no per-device `unique_id` to enumerate; the bus reports nothing, warns once, and `watch_thunderbolt` does nothing. And `require_locked` depends on logind, so lock state always reads as unknown.

Two display caveats there as well: the connector `ignore` list has no effect, since the devd event does not name the connector and any connector change trips, and HDMI hotplug has historically been less reliable than DisplayPort under drm-kmod.

SD card serials on FreeBSD are best-effort, read from `dev.mmcsd.N.%pnpinfo`. Check that your card's serial actually appears in `plugkill --list-devices` before you rely on `[sdcard_whitelist]`.

## Origin

A from-scratch Rust rewrite of [usbkill](https://github.com/hephaest0s/usbkill) by [Hephaestos](https://github.com/hephaest0s), with Thunderbolt and SD card monitoring, runtime control, learning mode and config hot-reload added.

## License

GPL-3.0. See [LICENSE](LICENSE).
