{
  description = "solar controller bridge (Deye RS485 -> WebSocket)";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    process-compose-flake.url = "github:Platonic-Systems/process-compose-flake";
    services-flake.url = "github:juspay/services-flake";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];

      imports = [
        inputs.process-compose-flake.flakeModule
      ];

      perSystem =
        {
          pkgs,
          system,
          ...
        }:
        let
          inherit (pkgs) lib;

          # Native builds + devshell: host pkgs with the rust overlay.
          nativePkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import inputs.rust-overlay) ];
          };

          # Cross builds for the Pi: aarch64 musl -> fully static binary
          # that runs on Raspberry Pi OS trixie without any nix there.
          # NOTE: no rust-overlay here - under crossSystem the overlay would
          # dispatch on hostPlatform and yield aarch64 toolchain *binaries*
          # (exec format error on x86_64). The toolchain must come from
          # nativePkgs; only crane.mkLib uses this package set (for the
          # musl linker wrapper).
          armPkgs = import inputs.nixpkgs {
            inherit system;
            crossSystem.config = "aarch64-unknown-linux-musl";
          };

          nativeToolchain = nativePkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "clippy"
              "rustfmt"
            ];
          };
          # x86_64 binaries + aarch64-musl std: cross-capable toolchain.
          armToolchain = nativePkgs.rust-bin.stable.latest.default.override {
            targets = [ "aarch64-unknown-linux-musl" ];
          };

          craneLibNative = (inputs.crane.mkLib nativePkgs).overrideToolchain nativeToolchain;
          craneLibArm = (inputs.crane.mkLib armPkgs).overrideToolchain armToolchain;

          # cleanCargoSource drops maps/ - include_str!("../maps/...") needs it.
          src = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (craneLibNative.filterCargoSources path type) || (builtins.match ".*/maps/.*\\.toml$" path != null);
          };

          commonArgs = {
            inherit src;
            strictDeps = true;
            buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
          };

          nativeDeps = craneLibNative.buildDepsOnly commonArgs;
          native = craneLibNative.buildPackage (
            commonArgs
            // {
              cargoArtifacts = nativeDeps;
            }
          );

          armDeps = craneLibArm.buildDepsOnly (commonArgs // { doCheck = false; });
          solar-bridge-arm = craneLibArm.buildPackage (
            commonArgs
            // {
              cargoArtifacts = armDeps;
              doCheck = false; # cross binary can't run on x86_64
            }
          );
        in
        {
          packages.default = native;
          packages.solar-bridge-arm = solar-bridge-arm;

          apps.default = {
            type = "app";
            program = lib.getExe native;
          };

          devShells.default = craneLibNative.devShell {
            checks = inputs.self.checks.${system};

            packages = with pkgs; [
              just
              uv
            ];

            # env = {
            #   UV_PYTHON_PREFERENCE = "only-system";
            #   UV_PYTHON = pkgs.python313.interpreter;
            # };
            #
            # shellHook = ''
            #   uv sync --quiet
            #   source .venv/bin/activate
            # '';
          };

          process-compose."services" = {
            imports = [
              inputs.services-flake.processComposeModules.default
            ];

            # services.postgres."pg1" = {
            #   enable = true;
            # };
          };
        };
    };
}
