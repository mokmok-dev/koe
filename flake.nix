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
        let
          windowsTarget = "x86_64-pc-windows-gnu";
          windowsCross = pkgs.pkgsCross.mingwW64;
          windowsCommonArgs = {
            pname = "koe-windows-gnu";
            version = "0.0.0";
            src = config.rust-project.src;
            strictDeps = true;
            CARGO_BUILD_TARGET = windowsTarget;
            CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${windowsCross.stdenv.cc}/bin/${windowsCross.stdenv.cc.targetPrefix}cc";
            buildInputs = [ windowsCross.windows.pthreads ];
            nativeBuildInputs = [ windowsCross.stdenv.cc ];
            cargoExtraArgs = "--workspace";
          };
          windowsCargoArtifacts = config.rust-project.crane-lib.buildDepsOnly windowsCommonArgs;
          windowsPackage = config.rust-project.crane-lib.buildPackage (
            windowsCommonArgs
            // {
              cargoArtifacts = windowsCargoArtifacts;
              doCheck = false;
            }
          );
          cargoVendorDir = config.rust-project.crane-lib.vendorCargoDeps {
            src = ./.;
            overrideVendorCargoPackage =
              package: drv:
              if package.name == "libspa-sys" && package.version == "0.10.0" then
                drv.overrideAttrs {
                  patches = [ ./nix/patches/libspa-sys-bindgen-out-dir.patch ];
                }
              else
                drv;
          };
        in
        {
          checks = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            windows-gnu = windowsPackage;
          };

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
            defaults.perCrate.crane.args = {
              BINDGEN_EXTRA_CLANG_ARGS = pkgs.lib.optionalString pkgs.stdenv.isLinux "-isystem ${pkgs.llvmPackages_18.clang-unwrapped.lib}/lib/clang/18/include";
              LIBCLANG_PATH = "${pkgs.llvmPackages_18.libclang.lib}/lib";
              inherit cargoVendorDir;
              buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [
                pkgs.alsa-lib
                pkgs.libpulseaudio
                pkgs.pipewire
              ];
              nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [
                pkgs.llvmPackages_18.libclang
                pkgs.pkg-config
              ];
            };
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
