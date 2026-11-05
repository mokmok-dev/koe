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
                # The Darwin stdenv has no swiftc, and the macOS speech shim
                # cannot be built without it.
                nativeBuildInputs = [
                  pkgs.cmake
                ]
                ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isDarwin pkgs.swift;
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
              backendArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-capture-linux --package koe-capture-macos --package koe-capture-windows --all-targets -- --deny warnings";
                cargoTestExtraArgs = "--package koe-capture-linux --package koe-capture-macos --package koe-capture-windows";
              };
              transcribeCoreArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-transcribe-core --all-targets -- --deny warnings";
                cargoTestExtraArgs = "--package koe-transcribe-core";
              };
              sessionArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-session --all-targets -- --deny warnings";
                cargoTestExtraArgs = "--package koe-session";
              };
              storeArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-session-store --all-targets -- --deny warnings";
                cargoTestExtraArgs = "--package koe-session-store";
              };
              recordCoreArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-record-core --all-targets -- --deny warnings";
                cargoTestExtraArgs = "--package koe-record-core";
              };
              recordArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-record --all-targets -- --deny warnings";
                cargoTestExtraArgs = "--package koe-record";
              };
              transcribeFacadeArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-transcribe --all-targets --features ${platformFeature} -- --deny warnings";
                cargoTestExtraArgs = "--package koe-transcribe --features ${platformFeature}";
              };
              transcribeBackendArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--package koe-transcribe-ggml --package koe-transcribe-macos --all-targets -- --deny warnings";
                cargoTestExtraArgs = "--package koe-transcribe-ggml --package koe-transcribe-macos";
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
              backendClippy = craneLib.cargoClippy backendArgs;
              backendTest = craneLib.cargoTest backendArgs;
              transcribeCoreClippy = craneLib.cargoClippy transcribeCoreArgs;
              transcribeCoreTest = craneLib.cargoTest transcribeCoreArgs;
              sessionClippy = craneLib.cargoClippy sessionArgs;
              sessionTest = craneLib.cargoTest sessionArgs;
              storeClippy = craneLib.cargoClippy storeArgs;
              storeTest = craneLib.cargoTest storeArgs;
              recordCoreClippy = craneLib.cargoClippy recordCoreArgs;
              recordCoreTest = craneLib.cargoTest recordCoreArgs;
              recordClippy = craneLib.cargoClippy recordArgs;
              recordTest = craneLib.cargoTest recordArgs;
              transcribeFacadeClippy = craneLib.cargoClippy transcribeFacadeArgs;
              transcribeBackendClippy = craneLib.cargoClippy transcribeBackendArgs;
              transcribeFacadeTest = craneLib.cargoTest transcribeFacadeArgs;
              transcribeBackendTest = craneLib.cargoTest transcribeBackendArgs;
            }
            // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
              mutation = craneLib.mkCargoDerivation (
                cargoCommonArgs
                // {
                  inherit cargoArtifacts;
                  pname = "koe-mutants";
                  nativeBuildInputs = [
                    pkgs.cargo-mutants
                    pkgs.cmake
                  ];
                  doCheck = false;
                  buildPhaseCargoCommand = "";
                  buildPhase = ''
                    runHook preBuild
                    # The ggml backend rebuilds its C++ library per mutant.
                    # The bundled libopus rebuild dwarfs the recording crate's run.
                    cargo mutants --jobs 8 --exclude koe-transcribe-ggml --exclude koe-record
                    test ! -s mutants.out/missed.txt
                    test ! -s mutants.out/timeout.txt
                    runHook postBuild
                  '';
                  installPhase = "mkdir -p $out";
                }
              );
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
                  cargo check --package koe-capture-core --package koe-session --package koe-record-core --target thumbv7em-none-eabi
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
                cmake
                linkerPackages
                rustToolchain
                sccache
              ]
              ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux libpulseaudio
              ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isDarwin swift;

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
