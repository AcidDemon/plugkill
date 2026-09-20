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
  # requireAuth is an option rather than a settings key, so it is written into
  # the generated config here.
  configFile = tomlFormat.generate "plugkill-config.toml" (
    lib.recursiveUpdate cfg.settings { general.require_auth = cfg.requireAuth; }
  );
  defaultPackage = flake.packages.${pkgs.stdenv.hostPlatform.system}.default;

  # Collect paths that need write access from the destruction config
  destruction = cfg.settings.destruction or {};
  destructionWritePaths =
    (destruction.files_to_remove or [ ])
    ++ (destruction.folders_to_remove or [ ])
    ++ lib.optional (destruction ? swap_device && destruction.swap_device != null)
      destruction.swap_device;
in
{
  options.services.plugkill = {
    enable = lib.mkEnableOption "plugkill, a hardware kill-switch daemon that shuts down the system on hardware changes (USB, Thunderbolt, SD, PCI, power, network, lid, display)";

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

    requireAuth = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Ask the caller for their own password before disarm, learn, reload and
        allowing a device, so that someone at an unlocked machine cannot simply
        turn plugkill off. Needs `security.polkit.enable` and a polkit agent in
        the session: with no agent to ask, every non-root caller is refused.
        root is always allowed, so `sudo plugkill --disarm` keeps working.
      '';
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
    assertions = [
      {
        # requireAuth is written into the generated config on top of settings,
        # so the same key in settings would be dropped without a word. Say so
        # at build time instead of handing out a config that reads false.
        assertion = !((cfg.settings.general or { }) ? require_auth);
        message = "services.plugkill.settings.general.require_auth is overwritten by services.plugkill.requireAuth. Set services.plugkill.requireAuth instead.";
      }
      {
        assertion = !cfg.requireAuth || config.security.polkit.enable;
        message = "services.plugkill.requireAuth needs security.polkit.enable; without it every non-root disarm, learn, reload and allowance is refused.";
      }
    ];

    # Create the plugkill group so GUI/CLI users can access the control socket
    users.groups.${cfg.socketGroup} = {};

    # polkit reads actions out of the system path, which security.polkit.enable
    # links; the package carries share/polkit-1/actions. This also puts the CLI
    # on the path, which is where a person answers the prompt from.
    environment.systemPackages = lib.optional cfg.requireAuth cfg.package;

    # Create directories with correct ownership. RuntimeDirectory= below
    # re-applies owner and mode to /run/plugkill on every start, so keep this
    # rule in sync with RuntimeDirectoryMode rather than fighting it.
    systemd.tmpfiles.rules = [
      "d /var/log/plugkill 0750 root root -"
      "d /run/plugkill 0755 root root -"
    ];

    systemd.services.plugkill = {
      description = "plugkill hardware kill-switch daemon";
      after = [ "local-fs.target" "sysinit.target" ];
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

        # Capabilities the daemon needs
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

        # Filesystem hardening: ProtectSystem=strict makes / read-only,
        # then we selectively open the paths the tool needs to write to.
        ProtectSystem = "strict";
        ProtectHome = false;  # tool may need to shred files anywhere
        PrivateTmp = true;
        # Prefix with '-' so systemd ignores paths that don't exist on this machine
        ReadOnlyPaths = [ "-/sys/bus/usb/devices" "-/sys/bus/thunderbolt/devices" "-/sys/bus/mmc/devices" "-/sys/class/power_supply" "-/sys/class/net" "-/proc/acpi" ];
        RuntimeDirectory = "plugkill";
        # 0755, not 0750: the unit runs as root:root, so a 0750 directory
        # gives socketGroup members no traverse bit and they cannot reach the
        # socket inside it. systemd chowns RuntimeDirectory to the unit's own
        # user and group on every start, so the tmpfiles rule above cannot
        # grant that traversal instead. The directory holds only the socket,
        # whose 0660 mode and group ownership are the real access control, so
        # traversal permission leaks nothing.
        RuntimeDirectoryMode = "0755";
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

        # Network hardening: plugkill needs no network access
        RestrictAddressFamilies = [ "AF_UNIX" ];
        IPAddressDeny = "any";

        # Syscall filtering
        SystemCallArchitectures = "native";

        UMask = "0077";

        # Lock log level
        Environment = "RUST_LOG=info";
      };
    };
  };
}
