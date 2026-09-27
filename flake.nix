{
  description = "rust project";

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
        { pkgs, system, ... }:
        let
          inherit (pkgs) lib;

          pkgsWithRust = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import inputs.rust-overlay) ];
          };

          rustToolchain = pkgsWithRust.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "clippy"
              "rustfmt"
            ];
          };

          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;
          src = craneLib.cleanCargoSource ./.;

          commonArgs = {
            inherit src;
            strictDeps = true;
            buildInputs = [ ] ++ lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          my-crate = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });

          nativeLibs = with pkgs; [ ];
        in
        {
          packages.default = my-crate;
          apps.default = {
            type = "app";
            program = lib.getExe my-crate;
          };

          devShells.default = craneLib.devShell {
            checks = inputs.self.checks.${system};
            packages =
              with pkgs;
              [
              ]
              ++ nativeLibs;

            env.LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath nativeLibs;
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
