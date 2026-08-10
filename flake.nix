{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks.flakeModule
      ];

      perSystem = { pkgs, config, ... }: {
        devShells.default = pkgs.mkShellNoCC {
          inputsFrom = [ config.pre-commit.devShell ];

          packages = [ ];
        };

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
          programs = {
            nixfmt.enable = true;
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
