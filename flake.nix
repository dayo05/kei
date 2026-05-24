{
  description = "Kei — Kickstart Environment Integrator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "kei";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config ];
          # Features enabled in the packaged binary. Slim builds via plain
          # `cargo build` can opt out by omitting these.
          buildFeatures = [ "discord" "maven" ];
          meta.mainProgram = "kei";
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rust-analyzer clippy git ];
        };
      });
}