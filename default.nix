{
  rust-overlay ? import (import ./nix/flake-parse.nix "rust-overlay"),
  pkgs ? import (import ./nix/flake-parse.nix "nixpkgs") {
    system = builtins.currentSystem;
    overlays = [ rust-overlay ];
  },
}:
let
  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    extensions = [
      "rust-analyzer"
      "rust-src"
      "llvm-tools"
    ];
  };
in
{
  shell = import ./nix/shell.nix {
    inherit pkgs rustToolchain;
  };

  formatter = import ./nix/formatter.nix { inherit pkgs rustToolchain; };

  packages =
    let
      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };
    in
    rec {
      default = pkgs.callPackage ./nix/package.nix {
        inherit rustPlatform;
      };
      sessiond-seat = default;
    };
}
