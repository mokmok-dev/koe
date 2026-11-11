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

          guiPackages = with pkgs; [
            dbus
            fontconfig
            freetype
            libglvnd
            libx11
            libxcb
            libxcursor
            libxi
            libxkbcommon
            libxrandr
            mesa
            vulkan-loader
            wayland
          ];
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
                ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.pkg-config
                ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isDarwin pkgs.swift;
              }
              // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
                buildInputs = [
                  pkgs.libpulseaudio
                  pkgs.libxkbcommon
                ]
                ++ guiPackages;
                LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
                  pkgs.libpulseaudio
                  pkgs.libxkbcommon
                ];
              };
              cargoArtifacts = craneLib.buildDepsOnly cargoCommonArgs;

              # The facades reject `--all-features` and accept exactly one
              # platform feature, so the host feature is named per package.
              hostFeatures = "koe-capture/${platformFeature},koe-transcribe/${platformFeature},koe-cli/${platformFeature},koe-desktop/${platformFeature}";
              # `clippy`, `test`, and `doctest` share this so a narrower set
              # cannot `cfg` platform code out while the gate stays green.
              hostWorkspaceArgs = "--workspace --features ${hostFeatures}";
              hostArgs = cargoCommonArgs // {
                inherit cargoArtifacts;
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
              clippy = craneLib.cargoClippy (
                hostArgs
                // {
                  cargoClippyExtraArgs = "--all-targets ${hostWorkspaceArgs} -- --deny warnings";
                }
              );
              test = craneLib.cargoNextest (
                hostArgs
                // {
                  # crane's nextest default leaves out `--locked`, so a stale
                  # Cargo.lock would pass this gate.
                  cargoExtraArgs = "--locked";
                  # One red gate should name every failure; nextest ignores the
                  # RUST_TEST_THREADS crane derives from NIX_BUILD_CORES.
                  cargoNextestExtraArgs = "${hostWorkspaceArgs} --no-fail-fast --test-threads $NIX_BUILD_CORES";
                }
              );
              # Nextest does not run doctests, and the crate docs carry runnable
              # examples.
              doctest = craneLib.cargoDocTest (
                hostArgs
                // {
                  cargoTestExtraArgs = hostWorkspaceArgs;
                }
              );
            }
            // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
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
                cargo-nextest
                cmake
                linkerPackages
                rustToolchain
                sccache
              ]
              ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux libpulseaudio
              ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux pkg-config
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux guiPackages
              ++ pkgs.lib.optional pkgs.stdenv.hostPlatform.isDarwin swift;

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              ${linkerEnv}
              ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
                export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath guiPackages}:$LD_LIBRARY_PATH"
              ''}
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
