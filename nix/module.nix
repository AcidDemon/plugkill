flake:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.usbkill;
  tomlFormat = pkgs.formats.toml { };
  configFile = tomlFormat.generate "usbkill-config.toml" cfg.settings;
  defaultPackage = flake.packages.${pkgs.stdenv.hostPlatform.system}.default;

  # Dynamically collect paths that need write access from destruction config
  destructionWritePaths =
    (cfg.settings.destruction.files_to_remove or [ ])
    ++ (cfg.settings.destruction.folders_to_remove or [ ])
    ++ lib.optional (cfg.settings.destruction ? swap_device && cfg.settings.destruction.swap_device != null)
      cfg.settings.destruction.swap_device;
in
{
  options.services.usbkill = {
    enable = lib.mkEnableOption "usbkill, a USB kill-switch daemon that shuts down the system on USB device changes";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage;
      description = "The usbkill package to use.";
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = {
        general = {
          sleep_ms = 250;
          log_file = "/var/log/usbkill/usbkill.log";
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
        commands = {
          kill_commands = [ ];
        };
      };
      description = ''
        Configuration for usbkill, serialized to TOML.
        See the project documentation for available options.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Create log directory with restrictive permissions
    systemd.tmpfiles.rules = [
      "d /var/log/usbkill 0750 root root -"
    ];

    systemd.services.usbkill = {
      description = "usbkill USB kill-switch daemon";
      after = [ "multi-user.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} --config ${configFile}";
        Restart = "on-failure";
        RestartSec = 5;

        # Must run as root for shutdown capability, USB sysfs access, and file shredding
        User = "root";
        Group = "root";

        # Capabilities needed for full functionality
        AmbientCapabilities = [
          "CAP_SYS_BOOT"          # reboot(2) syscall for shutdown
          "CAP_SYS_ADMIN"         # swapoff/swapon
          "CAP_DAC_READ_SEARCH"   # read sysfs
          "CAP_DAC_OVERRIDE"      # write log files, shred files
          "CAP_KILL"              # kill processes during shutdown
        ];
        CapabilityBoundingSet = [
          "CAP_SYS_BOOT"
          "CAP_SYS_ADMIN"
          "CAP_DAC_READ_SEARCH"
          "CAP_DAC_OVERRIDE"
          "CAP_KILL"
        ];

        # Filesystem hardening — ProtectSystem=strict makes / read-only,
        # then we selectively open paths the tool needs to write to.
        ProtectSystem = "strict";
        ProtectHome = false;  # tool may need to shred files anywhere
        PrivateTmp = true;
        ReadOnlyPaths = [ "/sys/bus/usb/devices" ];
        ReadWritePaths = [
          "/var/log/usbkill"
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

        # Network hardening — usbkill needs no network access
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
