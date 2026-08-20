{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShell = with pkgs; mkShell rec {
          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = [ 
            cargo
            rustc
            rustfmt
            pre-commit
            rustPackages.clippy
            libxkbcommon
            libxkbcommon.dev
            clang
            udev
            pam
            libinput
            llvmPackages.libclang
          ];

          RUST_SRC_PATH = rustPlatform.rustLibSrc;
          LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath buildInputs;
        };
      }
    );
}
