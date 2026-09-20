{
  description = "Hardware kill-switch daemon for Linux and FreeBSD that powers off on hardware changes (USB, Thunderbolt, SD, PCI, power, network, lid, display)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachSystem
    [
      "x86_64-linux"
      "aarch64-linux"
    ]
    (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "plugkill";
          version = (builtins.fromTOML (builtins.readFile ./crates/plugkill/Cargo.toml)).package.version;

          src = pkgs.lib.cleanSource ./.;

          cargoLock.lockFile = ./Cargo.lock;

          # Integration tests require /sys/bus/usb/devices which is unavailable in the Nix sandbox
          checkFlags = [
            "--skip=test_list_devices_no_root"
            "--skip=test_generate_whitelist_no_root"
          ];

          # The polkit actions for require_auth. Inert unless the daemon is
          # started with it, and polkit only reads them once the package is on
          # the system path.
          postInstall = ''
            install -Dm444 assets/net.acidnetworks.plugkill.policy \
              -t $out/share/polkit-1/actions
          '';

          meta = {
            description = "Hardware kill-switch daemon that shuts down the system when device changes are detected";
            license = pkgs.lib.licenses.gpl3Plus;
            platforms = [
              "x86_64-linux"
              "aarch64-linux"
            ];
            mainProgram = "plugkill";
          };
        };

        packages.gui = pkgs.rustPlatform.buildRustPackage {
          pname = "plugkill-gui";
          version = (builtins.fromTOML (builtins.readFile ./crates/plugkill-gui/Cargo.toml)).package.version;

          src = pkgs.lib.cleanSource ./.;

          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "--package" "plugkill-gui" ];
          cargoTestFlags = [ "--package" "plugkill-gui" ];

          nativeBuildInputs = with pkgs; [ pkg-config wrapGAppsHook4 ];
          buildInputs = with pkgs; [ gtk4 gtk4-layer-shell ];

          meta = {
            description = "Tray icon and dashboard for the plugkill daemon";
            license = pkgs.lib.licenses.gpl3Plus;
            platforms = [ "x86_64-linux" "aarch64-linux" ];
            mainProgram = "plugkill-gui";
          };
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            gtk4
            gtk4-layer-shell
          ];
        };
      }
    )
    // {
      nixosModules.default = import ./nix/module.nix self;
      nixosModules.relay = import ./nix/relay-module.nix self;
    };
}
