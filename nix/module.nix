flake:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.plugkill;
  tomlFormat = pkgs.formats.toml { };
  configFile = tomlFormat.generate "plugkill-config.toml" cfg.settings;
  defaultPackage = flake.packages.${pkgs.stdenv.hostPlatform.system}.default;

  # Dynamically collect paths that need write access from destruction config
  destruction = cfg.settings.destruction or {};
  destructionWritePaths =
    (destruction.files_to_remove or [ ])
    ++ (destruction.folders_to_remove or [ ])
    ++ lib.optional (destruction ? swap_device && destruction.swap_device != null)
      destruction.swap_device;
in
{
  options.services.plugkill = {
    enable = lib.mkEnableOption "plugkill, a hardware kill-switch daemon that shuts down the system on device changes (USB, Thunderbolt, SD card, power, network, lid)";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage;
      description = "The plugkill package to use.";
    };

    learnMode = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Start in learning mode (log violations without triggering kill sequence).";
    };

    dryRun = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Log actions without executing them.";
    };

    socketGroup = lib.mkOption {
      type = lib.types.str;
      default = "plugkill";
      description = "Group that owns the control socket (members can use the GUI and CLI).";
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = {
        general = {
          sleep_ms = 250;
          log_file = "/var/log/plugkill/plugkill.log";
          watch_usb = true;
          watch_thunderbolt = true;
          watch_sdcard = true;
          watch_power = false;
          watch_network = false;
          watch_lid = false;
        };
        whitelist = {
          devices = [ ];
        };
        destruction = {
          files_to_remove = [ ];
          folders_to_remove = [ ];
          melt_self = false;
          do_sync = true;
          do_wipe_swap = false;
        };
        thunderbolt_whitelist = {
          devices = [ ];
        };
        sdcard_whitelist = {
          devices = [ ];
        };
        power = {
          policy = "monitor";
          grace_secs = 0;
          require_locked = false;
        };
        network = {
          policy = "monitor";
          grace_secs = 0;
          interfaces = [ ];
        };
        lid = {
          policy = "monitor";
          grace_secs = 0;
        };
        commands = {
          kill_commands = [ ];
        };
      };
      description = ''
        Configuration for plugkill, serialized to TOML.
        See the project documentation for available options.
      '';
    };
  };

  # For lid monitoring, set services.logind.lidSwitchIgnoreInhibited = false
  # so that plugkill's delay inhibitor is respected by logind.

  config = lib.mkIf cfg.enable {
    # Create the plugkill group so GUI/CLI users can access the control socket
    users.groups.${cfg.socketGroup} = {};

    # Create directories with correct ownership
    systemd.tmpfiles.rules = [
      "d /var/log/plugkill 0750 root root -"
      "d /run/plugkill 0750 root ${cfg.socketGroup} -"
    ];

    systemd.services.plugkill = {
      description = "plugkill hardware kill-switch daemon";
      after = [ "multi-user.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.concatStringsSep " " ([
          "${lib.getExe cfg.package}"
          "--config ${configFile}"
          "--socket-group ${cfg.socketGroup}"
        ]
          ++ lib.optional cfg.learnMode "--learn-mode"
          ++ lib.optional cfg.dryRun "--dry-run");
        Restart = "on-failure";
        RestartSec = 5;

        # Must run as root for shutdown capability, sysfs access, and file shredding
        User = "root";
        Group = "root";

        # Capabilities needed for full functionality
        AmbientCapabilities = [
          "CAP_SYS_BOOT"          # reboot(2) syscall for shutdown
          "CAP_SYS_ADMIN"         # swapoff/swapon
          "CAP_DAC_READ_SEARCH"   # read sysfs
          "CAP_DAC_OVERRIDE"      # write log files, shred files
          "CAP_KILL"              # kill processes during shutdown
          "CAP_CHOWN"             # chown control socket to socketGroup
        ];
        CapabilityBoundingSet = [
          "CAP_SYS_BOOT"
          "CAP_SYS_ADMIN"
          "CAP_DAC_READ_SEARCH"
          "CAP_DAC_OVERRIDE"
          "CAP_KILL"
          "CAP_CHOWN"
        ];

        # Filesystem hardening — ProtectSystem=strict makes / read-only,
        # then we selectively open paths the tool needs to write to.
        ProtectSystem = "strict";
        ProtectHome = false;  # tool may need to shred files anywhere
        PrivateTmp = true;
        # Prefix with '-' so systemd ignores paths that don't exist on this machine
        # Prefix with '-' so systemd ignores paths that don't exist on this machine
        ReadOnlyPaths = [ "-/sys/bus/usb/devices" "-/sys/bus/thunderbolt/devices" "-/sys/bus/mmc/devices" "-/sys/class/power_supply" "-/sys/class/net" "-/proc/acpi" ];
        RuntimeDirectory = "plugkill";
        RuntimeDirectoryMode = "0750";
        ReadWritePaths = [
          "/var/log/plugkill"
          "/run/plugkill"
        ] ++ destructionWritePaths;

        # Process hardening
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictSUIDSGID = true;
        MemoryDenyWriteExecute = true;
        LockPersonality = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;

        # Network hardening — plugkill needs no network access
        RestrictAddressFamilies = [ "AF_UNIX" ];
        IPAddressDeny = "any";

        # Syscall filtering
        SystemCallArchitectures = "native";

        # File creation mask
        UMask = "0077";

        # Lock log level
        Environment = "RUST_LOG=info";
      };
    };
  };
}
