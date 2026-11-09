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
            # The viewer is one codebase with two targets: the native window and the wasm
            # build trunk serves at /app (board-b0o). Only std for the target is needed;
            # wasm-bindgen-cli arrives with trunk (board-fcg pins it for `nix build .#wasm`).
            targets = [ "wasm32-unknown-unknown" ];
          };
          # x86_64 binaries + aarch64-musl std: cross-capable toolchain.
          armToolchain = nativePkgs.rust-bin.stable.latest.default.override {
            targets = [ "aarch64-unknown-linux-musl" ];
          };

          craneLibNative = (inputs.crane.mkLib nativePkgs).overrideToolchain nativeToolchain;
          craneLibArm = (inputs.crane.mkLib armPkgs).overrideToolchain armToolchain;

          # cleanCargoSource drops maps/ - include_str!("../maps/...") needs it. The viewer's
          # index.html is kept for a future in-Nix wasm build (board-fcg).
          sourceFilter =
            path: type:
            (craneLibNative.filterCargoSources path type)
            || (builtins.match ".*/maps/.*\\.toml$" path != null)
            || (builtins.match ".*/tests/fixtures/.*\\.(xml|json)$" path != null)
            || (builtins.match ".*/viewer/index\\.html$" path != null);

          src = lib.cleanSourceWith {
            filter = sourceFilter;
            src = ./.;
          };

          commonArgs = {
            inherit src;
            strictDeps = true;
            buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
          };


          nativeDeps = craneLibNative.buildDepsOnly commonArgs;
          # The viewer's bundle is embedded into the binary (board-b0o), so both packages take
          # it from the wasm output rather than from a dirty working tree.
          embedViewer = ''
            rm -rf viewer/dist
            mkdir -p viewer/dist
            cp -r ${wasm}/dist/. viewer/dist/
          '';

          native = craneLibNative.buildPackage (
            commonArgs
            // {
              cargoArtifacts = nativeDeps;
              preBuild = embedViewer;
            }
          );

          # The viewer, built for the browser, from tracked sources only (board-fcg).
          #
          # No trunk here: trunk insists on downloading its own wasm-bindgen-cli, and the CLI
          # must match the crate version exactly. nixpkgs carries 0.2.126, the viewer's
          # wasm-bindgen family is pinned to it, so the CLI comes from the store and the build
          # stays offline.
          wasm = craneLibNative.buildPackage (
            commonArgs
            // {
              pname = "solar-viewer-wasm";
              version = "0.1.0";
              doCheck = false;
              # Nothing here is a host binary to install; the output is assembled by hand.
              doNotPostBuildInstallCargoBinaries = true;
              nativeBuildInputs = [
                nativePkgs.binaryen
                nativePkgs.wasm-bindgen-cli_0_2_126
              ];
              # crane's default build command targets the host and the whole workspace; this
              # needs one package for wasm32. The CLI version must match the crate, hence the
              # nixpkgs attribute rather than trunk's download.
              buildPhaseCargoCommand = "cargo build --profile release --locked --target wasm32-unknown-unknown -p viewer";
              installPhaseCommand = ''
                mkdir -p $out/dist
                wasm-bindgen --target web --no-typescript --out-dir $out/dist \
                  --out-name viewer target/wasm32-unknown-unknown/release/viewer.wasm
                wasm-opt -Oz -o $out/dist/viewer_bg.wasm $out/dist/viewer_bg.wasm
                cat > $out/dist/index.html <<'HTML'
                <!doctype html>
                <html lang="en">
                  <head>
                    <meta charset="utf-8" />
                    <title>solar plan</title>
                    <style>
                      html, body { margin: 0; height: 100%; background: #121216; overflow: hidden; }
                      #the_canvas_id { display: block; width: 100vw; height: 100vh; outline: none; }
                    </style>
                  </head>
                  <body>
                    <canvas id="the_canvas_id" tabindex="0"></canvas>
                    <script type="module">
                      import init from './viewer.js';
                      init({ module_or_path: './viewer_bg.wasm' });
                    </script>
                  </body>
                </html>
                HTML
              '';
            }
          );

          armDeps = craneLibArm.buildDepsOnly (commonArgs // { doCheck = false; });
          solar-bridge-arm = craneLibArm.buildPackage (
            commonArgs
            // {
              cargoArtifacts = armDeps;
              preBuild = embedViewer;
              doCheck = false; # cross binary can't run on x86_64
            }
          );
        in
        {
          packages.default = native;
          packages.solar-bridge-arm = solar-bridge-arm;
          packages.wasm = wasm;

          apps.default = {
            type = "app";
            program = lib.getExe native;
          };

          devShells.default = craneLibNative.devShell {
            checks = inputs.self.checks.${system};

            packages = with pkgs; [
              just
              trunk
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
