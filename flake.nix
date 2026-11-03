{
  description = "cs-grep";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
      treefmt-nix,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      imports = [
        treefmt-nix.flakeModule
      ];

      perSystem =
        {
          config,
          self',
          inputs',
          pkgs,
          system,
          ...
        }:
        let
          src = ./.;
          manifest = (pkgs.lib.importTOML "${src}/Cargo.toml").workspace.package;
          csGrep =
            with pkgs;
            rustPlatform.buildRustPackage {
              inherit src;
              name = "cs-grep";
              version = manifest.version;
              cargoLock = {
                lockFile = ./Cargo.lock;
              };
              doCheck = false;
            };

          csGrepWeb =
            with pkgs;
            rustPlatform.buildRustPackage {
              inherit src;
              name = "cs-grep-web";
              version = manifest.version;
              cargoLock = {
                lockFile = ./Cargo.lock;
              };
              doCheck = false;
              cargoBuildFlags = [ "-p" "cs-grep-web" ];
            };

        in
        {
          # auto formatting
          treefmt.config = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
            };
          };

          packages = {
            inherit csGrep csGrepWeb;
            default = csGrep;
          };

          devShells.default = pkgs.mkShell {
            buildInputs =
              csGrep.buildInputs
              ++ (with pkgs; [
                cargo
              ]);
          };
        };
    };
}
