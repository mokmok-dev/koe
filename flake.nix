{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      nixpkgs,
      flake-parts,
      git-hooks,
      treefmt-nix,
      rust-overlay,
      crane,
      ...
    }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        treefmt-nix.flakeModule
        git-hooks.flakeModule
      ];

      perSystem =
        {
          pkgs,
          config,
          system,
          ...
        }:
        let
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            targets = [
              "aarch64-apple-darwin"
              "thumbv7em-none-eabi"
              "x86_64-pc-windows-gnu"
            ];
          };
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          linkerPackages = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (
            with pkgs;
            [
              clang
              mold
            ]
          );
          linkerEnv = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            export RUSTFLAGS="-C link-arg=-fuse-ld=mold"
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="clang"
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="clang"
          '';
        in
        {
          _module.args = {
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            };
          };

          checks =
            let
              src = pkgs.lib.cleanSourceWith {
                src = ./.;
                filter =
                  path: _type:
                  let
                    name = baseNameOf path;
                  in
                  !(builtins.elem name [
                    "target"
                    "mutants.out"
                    "mutants.out.old"
                    ".direnv"
                  ])
                  && !(pkgs.lib.hasPrefix "result" name);
              };

              platformFeature = if pkgs.stdenv.hostPlatform.isDarwin then "macos" else "linux";

              cargoCommonArgs = {
                inherit src;
                pname = "koe";
                strictDeps = true;
              }
              // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
                buildInputs = [ pkgs.libpulseaudio ];
                LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libpulseaudio ];
              };
              cargoArtifacts = craneLib.buildDepsOnly cargoCommonArgs;

              coreArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-capture-core --all-targets -- --deny warnings";
                cargoTestExtraArgs = "--package koe-capture-core";
              };
              facadeArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-capture --all-targets --features ${platformFeature} -- --deny warnings";
                cargoTestExtraArgs = "--package koe-capture --features ${platformFeature}";
              };

              crossCheck =
                target: package:
                craneLib.mkCargoDerivation {
                  inherit src;
                  pname = "koe-check-${target}";
                  cargoArtifacts = null;
                  doCheck = false;
                  buildPhaseCargoCommand = "";
                  buildPhase = ''
                    runHook preBuild
                    cargo clippy --package ${package} --all-targets --target ${target} -- --deny warnings
                    runHook postBuild
                  '';
                  installPhase = "mkdir -p $out";
                };
            in
            {
              coreClippy = craneLib.cargoClippy coreArgs;
              facadeClippy = craneLib.cargoClippy facadeArgs;
              coreTest = craneLib.cargoTest coreArgs;
              facadeTest = craneLib.cargoTest facadeArgs;
            }
            // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
              mutation = craneLib.mkCargoDerivation {
                inherit
                  src
                  cargoArtifacts
                  ;
                pname = "koe-mutants";
                nativeBuildInputs = [ pkgs.cargo-mutants ];
                doCheck = false;
                buildPhaseCargoCommand = "";
                buildPhase = ''
                  runHook preBuild
                  cargo mutants \
                    --package koe-capture-core \
                    --jobs 8
                  test ! -s mutants.out/missed.txt
                  test ! -s mutants.out/timeout.txt
                  runHook postBuild
                '';
                installPhase = "mkdir -p $out";
              };
              crossMacos = crossCheck "aarch64-apple-darwin" "koe-capture-macos";
              crossWindows = crossCheck "x86_64-pc-windows-gnu" "koe-capture-windows";
              coreNoStd = craneLib.mkCargoDerivation {
                inherit src;
                pname = "koe-core-no-std";
                cargoArtifacts = null;
                doCheck = false;
                buildPhaseCargoCommand = "";
                buildPhase = ''
                  runHook preBuild
                  cargo check --package koe-capture-core --target thumbv7em-none-eabi
                  runHook postBuild
                '';
                installPhase = "mkdir -p $out";
              };
            };

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ config.pre-commit.devShell ];

            packages =
              with pkgs;
              [
                cargo-mutants
                linkerPackages
                rustToolchain
                sccache
              ]
              ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux libpulseaudio;

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              ${linkerEnv}
            '';
          };

          pre-commit.settings = {
            hooks = {
              deadnix.enable = true;
              statix.enable = true;
            };
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
              rustfmt.enable = true;
              rustfmt.package = rustToolchain;
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
