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
          nuget =
            name: version: hash:
            pkgs.fetchurl {
              name = "${name}.${version}.nupkg";
              url = "https://api.nuget.org/v3-flatcontainer/${name}/${version}/${name}.${version}.nupkg";
              inherit hash;
            };
          foundryCore =
            nuget "microsoft.ai.foundry.local.core" "1.2.3"
              "sha256-BKJ4UlITwyo2O/dpv2Kl8wcDo+6gbQLM/JTkURN7t8g=";
          foundryCoreWinml =
            nuget "microsoft.ai.foundry.local.core.winml" "1.2.3"
              "sha256-DTMON5vzKNX9I5YeduhBtSFooC3n22M/GGEi82u/jpI=";
          foundryOrtGpuLinux =
            nuget "microsoft.ml.onnxruntime.gpu.linux" "1.26.0"
              "sha256-z3lD9YuTYP4USMS69ArBLOQXWXjs0/5vIaTACXb2Olk=";
          foundryOrt =
            nuget "microsoft.ml.onnxruntime.foundry" "1.26.0"
              "sha256-YiBacnVB8boUm6vEUY3S1u/YTI+fSiIfH4/mxg54EiQ=";
          foundryGenai =
            nuget "microsoft.ml.onnxruntimegenai.foundry" "0.14.1"
              "sha256-Bz7EGuqkZrguwjyJDU58fC8VuOWJtWJsXzVi8UgF/oc=";
          foundryWindowsMl =
            nuget "microsoft.windows.ai.machinelearning" "2.1.1"
              "sha256-IDgsV3yggzvSd7BIZZ5kTXJJx4zG3NaWH0ZIvO+XGIw=";
          mkFoundryNative =
            {
              rid,
              extension,
              archives,
              windowsMlArchive ? null,
            }:
            pkgs.runCommand "foundry-local-native-${rid}" { nativeBuildInputs = [ pkgs.unzip ]; } ''
              mkdir -p "$out"
              for archive in ${pkgs.lib.escapeShellArgs (map toString archives)}; do
                unzip -j -o "$archive" 'runtimes/${rid}/native/*.${extension}' -d "$out"
              done
              ${pkgs.lib.optionalString (windowsMlArchive != null) ''
                unzip -j -o "${windowsMlArchive}" \
                  'runtimes/${rid}/native/Microsoft.Windows.AI.MachineLearning.dll' -d "$out"
              ''}
              ${pkgs.lib.optionalString (extension == "dll") ''
                rm -f "$out/DirectML.dll"
              ''}
            '';
          foundryNative =
            if pkgs.stdenv.hostPlatform.system == "x86_64-linux" then
              mkFoundryNative {
                rid = "linux-x64";
                extension = "so";
                archives = [
                  foundryCore
                  foundryOrtGpuLinux
                  foundryGenai
                ];
              }
            else if pkgs.stdenv.hostPlatform.system == "aarch64-linux" then
              mkFoundryNative {
                rid = "linux-arm64";
                extension = "so";
                archives = [
                  foundryCore
                  foundryOrt
                  foundryGenai
                ];
              }
            else
              mkFoundryNative {
                rid = "osx-arm64";
                extension = "dylib";
                archives = [
                  foundryCore
                  foundryOrt
                  foundryGenai
                ];
              };
          windowsFoundryNative = mkFoundryNative {
            rid = "win-x64";
            extension = "dll";
            archives = [
              foundryCoreWinml
              foundryOrt
              foundryGenai
            ];
            windowsMlArchive = foundryWindowsMl;
          };
          windowsTarget = "x86_64-pc-windows-gnu";
          windowsCross = pkgs.pkgsCross.mingwW64;
          windowsCommonArgs = {
            pname = "koe-windows-gnu";
            version = "0.0.0";
            src = config.rust-project.src;
            strictDeps = true;
            inherit cargoVendorDir;
            CARGO_BUILD_TARGET = windowsTarget;
            FOUNDRY_NATIVE_OFFLINE = "1";
            FOUNDRY_NATIVE_OVERRIDE_DIR = windowsFoundryNative;
            CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${windowsCross.stdenv.cc}/bin/${windowsCross.stdenv.cc.targetPrefix}cc";
            TARGET_CC = "${windowsCross.stdenv.cc}/bin/${windowsCross.stdenv.cc.targetPrefix}cc";
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
              else if package.name == "pipewire-sys" && package.version == "0.10.0" then
                drv.overrideAttrs {
                  patches = [ ./nix/patches/pipewire-sys-bindgen-out-dir.patch ];
                }
              else
                drv;
          };
        in
        {
          checks = {
            foundry-native = foundryNative;
          }
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            windows-gnu = windowsPackage;
            windows-foundry-native = windowsFoundryNative;
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
              FOUNDRY_NATIVE_OFFLINE = "1";
              FOUNDRY_NATIVE_OVERRIDE_DIR = foundryNative;
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
