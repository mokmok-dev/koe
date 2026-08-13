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
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks.flakeModule
      ];

      perSystem =
        {
          config,
          lib,
          system,
          ...
        }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

          src = craneLib.cleanCargoSource ./.;
          commonArgs = {
            inherit src;
            pname = "koe-workspace";
            strictDeps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          workspaceCargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          crates = workspaceCargoToml.workspace.members;

          crateArgs =
            name:
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = name;
              cargoExtraArgs = "-p ${name}";
            };

          koeFfiBindings = import ./nix/koe-ffi-bindings.nix {
            inherit lib pkgs craneLib;
            args = crateArgs "koe-ffi";
          };

          koe = craneLib.buildPackage (
            crateArgs "koe-cli"
            // {
              pname = "koe";
              cargoBuildProfile = "release";
            }
          );
        in
        {
          apps = {
            default = {
              type = "app";
              program = "${koe}/bin/koe";
            };
          }
          // lib.optionalAttrs pkgs.stdenv.isDarwin {
            generate-ffi-bindings = {
              type = "app";
              program = "${koeFfiBindings.packages.koe-ffi-populate}/bin/populate-koe-ffi-bindings";
            };
          };

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [
              config.pre-commit.devShell
            ];

            packages = [
              rustToolchain
            ]
            ++ lib.optionals pkgs.stdenv.isDarwin [
              pkgs.swift
            ];

            shellHook = koeFfiBindings.devShellHook;
          };

          packages = koeFfiBindings.packages // {
            inherit koe;
            default = koe;
          };

          checks =
            builtins.listToAttrs (
              map (name: lib.nameValuePair "${name}-test" (craneLib.cargoTest (crateArgs name))) crates
            )
            // koeFfiBindings.checks;

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

          treefmt = {
            projectRootFile = "flake.nix";
            settings.global.excludes = [
              "koe-native/generated/**"
            ];
            programs = {
              nixfmt.enable = true;
              rustfmt.enable = true;
              rustfmt.package = rustToolchain;
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
