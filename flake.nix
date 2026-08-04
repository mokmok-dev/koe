{
  description = "koe - A fully offline recording & transcription app.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    rust-flake.url = "github:juspay/rust-flake";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      imports = [
        inputs.git-hooks.flakeModule
        inputs.treefmt-nix.flakeModule
        inputs.rust-flake.flakeModules.default
        inputs.rust-flake.flakeModules.nixpkgs
      ];

      perSystem =
        {
          self',
          pkgs,
          config,
          ...
        }:
        {
          devShells = {
            default = pkgs.mkShellNoCC {
              inputsFrom = [ self'.devShells.rust ];
            };
          };

          pre-commit = {
            check.enable = false;
            settings.hooks = {
              treefmt.enable = true;
            };
          };

          rust-project = {
            toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              deadnix.enable = true;
              nixfmt.enable = true;
              rustfmt = {
                enable = true;
                package = config.rust-project.toolchain;
              };
              statix.enable = true;
            };
          };
        };
    };
}
