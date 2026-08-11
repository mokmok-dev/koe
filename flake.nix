{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    rust-flake = {
      url = "github:juspay/rust-flake";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks.flakeModule
        inputs.rust-flake.flakeModules.default
        inputs.rust-flake.flakeModules.nixpkgs
      ];

      perSystem =
        {
          self',
          pkgs,
          config,
          lib,
          ...
        }:
        {
          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [
              config.pre-commit.devShell
              self'.devShells.rust
            ];

            packages = lib.optionals pkgs.stdenv.isDarwin [
              pkgs.swift
            ];
          };

          checks = lib.mapAttrs' (
            name: crate:
            let
              crane-lib = config.rust-project.crane-lib;
              args = crate.crane.args // {
                src = config.rust-project.src;
                pname = name;
                cargoExtraArgs = "-p ${name}";
                strictDeps = true;
              };
              cargoArtifacts = crane-lib.buildDepsOnly args;
            in
            lib.nameValuePair "${name}-test" (crane-lib.cargoTest (args // { inherit cargoArtifacts; }))
          ) config.rust-project.crates;

          pre-commit.settings = {
            hooks = {
              actionlint.enable = true;
              deadnix.enable = true;
              statix = {
                enable = true;
                settings.ignore = [
                  ".direnv/**"
                ];
              };
            };
            package = pkgs.prek;
          };

          rust-project = {
            toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
              rustfmt.enable = true;
              rustfmt.package = config.rust-project.toolchain;
              swift-format.enable = true;
              taplo.enable = true;
              yamlfmt.enable = true;
            };
          };
        };

      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
    };
}
