{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };

        inherit (pkgs) lib;

        # We're going to build python wheels as well, so we need a python
        python = pkgs.python3;

        # What the extension needs to be built and then imported in a devshell
        pythonEnv = python.withPackages (ps: with ps; [numpy]);

        craneLib = (crane.mkLib pkgs).overrideToolchain (
          p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
        );
        # The scanner profiles are `include_bytes!`d into the crate, and a Rust
        # source filter drops them, so they are put back
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            (craneLib.fileset.commonCargoSources ./.)
            ./profiles
          ];
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          env = {
            PYO3_PYTHON = python.interpreter;
            PYO3_BUILD_EXTENSION_MODULE = true;
          };
          nativeBuildInputs = with pkgs;
            [python]
            ++ lib.optionals stdenv.hostPlatform.isDarwin [libiconv];
          buildInputs = [];
        };

        # Every feature, so the checks below can lint and test the Python bindings without
        # rebuilding pyo3 and numpy from scratch each time
        cargoArtifacts = craneLib.buildDepsOnly (
          commonArgs
          // {
            cargoExtraArgs = "--all-features";
          }
        );

        nkscan-cli = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            doCheck = false; # handled by nextest
            cargoExtraArgs = "--features=cli";
          }
        );
      in {
        # Checks for nix flake check
        checks = {
          nkscan = nkscan-cli;

          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              # --all-features so `src/python.rs` is linted too; it is not in any other check
              cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
            }
          );

          crate-fmt = craneLib.cargoFmt {
            inherit src;
          };

          toml-fmt = craneLib.taploFmt {
            src = pkgs.lib.sources.sourceFilesBySuffices src [".toml"];
          };

          nextest = craneLib.cargoNextest (
            commonArgs
            // {
              inherit cargoArtifacts;
              # Deliberately not --all-features. PYO3_BUILD_EXTENSION_MODULE tells pyo3 to leave
              # the Python symbols for the host interpreter to resolve, which is right for the
              # cdylib the wheel ships and impossible for a test executable, which has no
              # interpreter to link against. Nothing is lost: the bindings have no Rust tests, and
              # clippy checks them with every feature because checking never links.
              partitions = 1;
              partitionType = "count";
              cargoNextestPartitionsExtraArgs = "--no-tests=pass";
            }
          );
        };
        # Devshell
        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = with pkgs; [
            cargo-outdated
            maturin
            pythonEnv
          ];
        };

        # Flake entrypoint
        apps.default = flake-utils.lib.mkApp {
          drv = nkscan-cli;
        };

        # Package output
        packages = {
          default = nkscan-cli;
        };
      }
    );
}
