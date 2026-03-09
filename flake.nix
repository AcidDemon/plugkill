{
  description = "usbkill - USB kill-switch daemon that shuts down the system on USB device changes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
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
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          packages.default = pkgs.rustPlatform.buildRustPackage {
            pname = "usbkill";
            version = "0.1.0";

            src = pkgs.lib.cleanSource ./.;

            cargoHash = pkgs.lib.fakeHash;

            meta = {
              description = "USB kill-switch daemon -- shuts down the system when USB devices change";
              license = pkgs.lib.licenses.gpl3Plus;
              platforms = [
                "x86_64-linux"
                "aarch64-linux"
              ];
              mainProgram = "usbkill";
            };
          };

          devShells.default = pkgs.mkShell {
            buildInputs = with pkgs; [
              cargo
              rustc
              rust-analyzer
              clippy
              rustfmt
            ];
          };
        }
      )
    // {
      nixosModules.default = import ./nix/module.nix self;
    };
}
