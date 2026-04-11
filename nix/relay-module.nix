flake:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.plugkill-relay;
  tomlFormat = pkgs.formats.toml { };

  relayConfig = {
    general = {
      listen_port = cfg.listenPort;
      plugkill_socket = cfg.plugkillSocket;
      poll_interval_ms = cfg.pollIntervalMs;
      private_key_file = cfg.privateKeyFile;
    };
    retry = {
      ack_timeout_ms = cfg.ackTimeoutMs;
      max_retries = cfg.maxRetries;
    };
    peers = map (p: {
      inherit (p) name pubkey addresses;
    }) cfg.peers;
  } // cfg.extraSettings;

  configFile = tomlFormat.generate "plugkill-relay-config.toml" relayConfig;
  defaultPackage = flake.packages.${pkgs.stdenv.hostPlatform.system}.default;

  inherit (lib) mkEnableOption mkOption mkIf types;
in
{
  options.services.plugkill-relay = {
    enable = mkEnableOption "plugkill-relay, a kill signal relay mesh for plugkill";

    package = mkOption {
      type = types.package;
      default = defaultPackage;
      description = "The plugkill package to use (provides plugkill-relay binary).";
    };

    dryRun = mkOption {
      type = types.bool;
      default = false;
      description = "Log actions without triggering kills.";
    };

    listenPort = mkOption {
      type = types.port;
      default = 7654;
      description = "UDP port to listen for kill signals.";
    };

    plugkillSocket = mkOption {
      type = types.str;
      default = "/run/plugkill/plugkill.sock";
      description = "Path to plugkill's Unix control socket.";
    };

    pollIntervalMs = mkOption {
      type = types.int;
      default = 250;
      description = "How often to poll plugkill status (ms).";
    };

    privateKeyFile = mkOption {
      type = types.path;
      description = "Path to this node's ed25519 private key (base64, managed via SOPS).";
    };

    ackTimeoutMs = mkOption {
      type = types.int;
      default = 50;
      description = "Time to wait for peer ACK before retrying (ms).";
    };

    maxRetries = mkOption {
      type = types.int;
      default = 3;
      description = "Maximum retry attempts per peer.";
    };

    peers = mkOption {
      type = types.listOf (types.submodule {
        options = {
          name = mkOption {
            type = types.str;
            description = "Human-readable peer name.";
          };
          pubkey = mkOption {
            type = types.str;
            description = "Base64-encoded ed25519 public key.";
          };
          addresses = mkOption {
            type = types.listOf types.str;
            default = [ ];
            description = "List of host:port addresses (IPs or hostnames).";
          };
        };
      });
      default = [ ];
      description = "Peer nodes to relay kill signals to.";
    };

    extraSettings = mkOption {
      type = types.attrs;
      default = { };
      description = "Additional TOML config merged into the generated config.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = config.services.plugkill.enable or false;
        message = "plugkill-relay requires services.plugkill.enable = true";
      }
    ];

    systemd.tmpfiles.rules = [
      "d /etc/plugkill-relay 0700 root root -"
    ];

    systemd.services.plugkill-relay = {
      description = "plugkill-relay kill signal mesh";
      after = [
        "plugkill.service"
        "network-online.target"
      ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.concatStringsSep " " ([
          "${cfg.package}/bin/plugkill-relay"
          "--config ${configFile}"
        ] ++ lib.optional cfg.dryRun "--dry-run");
        Restart = "on-failure";
        RestartSec = 5;

        User = "root";
        Group = "root";

        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ReadOnlyPaths = [ "/etc/plugkill-relay" ];
        ReadWritePaths = [ "/run/plugkill" ];

        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;

        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        SystemCallArchitectures = "native";
        UMask = "0077";

        Environment = "RUST_LOG=info";
      };
    };
  };
}
