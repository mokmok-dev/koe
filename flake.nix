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

          inherit (craneLib.crateNameFromCargoToml { src = ./.; }) version;
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

          packages =
            let
              # The facades reject `--all-features` and accept exactly one
              # platform feature, so a release build names the host one.
              releaseFeatures = "koe-capture/${platformFeature},koe-transcribe/${platformFeature},koe-cli/${platformFeature},koe-desktop/${platformFeature}";

              # A release artifact tree is distinct from the debug tree the
              # checks build, so it is its own cache entry.
              releaseArtifacts = craneLib.buildDepsOnly (
                cargoCommonArgs
                // {
                  pname = "koe-deps-release";
                  doCheck = false;
                  CARGO_PROFILE = "release";
                  cargoExtraArgs = "--locked --workspace --features ${releaseFeatures}";
                }
              );

              releasePackage =
                package: binary:
                craneLib.buildPackage (
                  cargoCommonArgs
                  // {
                    pname = binary;
                    inherit version;
                    cargoArtifacts = releaseArtifacts;
                    doCheck = false;
                    CARGO_PROFILE = "release";
                    cargoExtraArgs = "--locked --package ${package} --features ${platformFeature}";
                  }
                );

              cli = releasePackage "koe-cli" "koe";
              desktop = releasePackage "koe-desktop" "koe-desktop";

              # `TCC` cannot grant the microphone or the system audio to a bare
              # binary, so the app ships as a bundle. The app sets its own
              # accessory activation policy at runtime.
              infoPlist = pkgs.writeText "koe-Info.plist" ''
                <?xml version="1.0" encoding="UTF-8"?>
                <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
                <plist version="1.0">
                <dict>
                  <key>CFBundleIdentifier</key>
                  <string>dev.mokmok.koe</string>
                  <key>CFBundleName</key>
                  <string>koe</string>
                  <key>CFBundleDisplayName</key>
                  <string>koe</string>
                  <key>CFBundleExecutable</key>
                  <string>koe-desktop</string>
                  <key>CFBundlePackageType</key>
                  <string>APPL</string>
                  <key>CFBundleShortVersionString</key>
                  <string>${version}</string>
                  <key>CFBundleVersion</key>
                  <string>${version}</string>
                  <key>LSMinimumSystemVersion</key>
                  <string>14.2</string>
                  <key>NSHighResolutionCapable</key>
                  <true/>
                  <key>NSMicrophoneUsageDescription</key>
                  <string>koe records the microphone to transcribe your meeting and dictation on this Mac.</string>
                  <key>NSAudioCaptureUsageDescription</key>
                  <string>koe records the system audio to transcribe the other side of your meeting on this Mac.</string>
                </dict>
                </plist>
              '';

              desktopApp = pkgs.runCommandLocal "koe-app-${version}" { } ''
                mkdir -p "$out/Koe.app/Contents/MacOS"
                install -m755 ${desktop}/bin/koe-desktop "$out/Koe.app/Contents/MacOS/koe-desktop"
                install -m644 ${infoPlist} "$out/Koe.app/Contents/Info.plist"

                # `libc` links `iconv` on every Apple target, so the store path in
                # that load command stops the bundle at launch. No koe symbol uses
                # it. Every other library has to be one the system ships.
                binutils=${pkgs.darwin.binutils-unwrapped}/bin
                binary="$out/Koe.app/Contents/MacOS/koe-desktop"
                chmod u+w "$binary"

                # The list goes through a file, and its length is checked. A
                # command substitution in a `for` list skips the loop when the
                # tool fails, which would pass a bundle nothing looked at.
                listing="$(mktemp)"
                libraries="$(mktemp)"
                "$binutils"/otool -L "$binary" > "$listing"
                awk '/^[[:space:]]/ { print $1 }' "$listing" > "$libraries"
                if [ ! -s "$libraries" ]; then
                  echo "otool -L named no libraries in $binary" >&2
                  exit 1
                fi

                while IFS= read -r library; do
                  case "$library" in
                    /System/* | /usr/lib/* | @*) ;;
                    /nix/store/*/libiconv.2.dylib)
                      "$binutils"/install_name_tool -change "$library" /usr/lib/libiconv.2.dylib "$binary"
                      ;;
                    *)
                      echo "$binary loads $library, which a stock Mac does not ship" >&2
                      exit 1
                      ;;
                  esac
                done < "$libraries"

                # The rewrite is only useful if it took effect, so read the
                # load commands back instead of trusting the command above.
                "$binutils"/otool -L "$binary" > "$listing"
                if grep -qE '^[[:space:]]+/nix/store/' "$listing"; then
                  echo "$binary still loads a Nix store library" >&2
                  exit 1
                fi
              '';

              # The binary records its Nix store libraries in DT_NEEDED, so this
              # asset runs where that store is mounted.
              desktopTarball = pkgs.runCommandLocal "koe-desktop-${version}-${system}" { } ''
                mkdir -p staging
                install -m755 ${desktop}/bin/koe-desktop staging/koe-desktop
                install -m755 ${cli}/bin/koe staging/koe
                install -m644 ${./README.md} staging/README.md
                mkdir -p "$out"
                tar --format=ustar --sort=name --owner=0 --group=0 --numeric-owner \
                  --mtime=@0 --directory staging --create . \
                  | gzip -n > "$out/koe-desktop-${version}-${system}.tar.gz"
              '';
            in
            {
              koe-cli = cli;
              koe-desktop = desktop;
              default = desktop;
            }
            // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
              koe-desktop-app = desktopApp;
            }
            // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
              koe-desktop-tarball = desktopTarball;
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
              # nixpkgs marks the Swift toolchain broken on aarch64-linux.
              swift-format.enable = pkgs.stdenv.hostPlatform.isDarwin;
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
