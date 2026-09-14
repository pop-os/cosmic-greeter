{ pkgs, rustToolchain }:
pkgs.mkShell {
  name = "cosmic-greeter-devshell";
  packages = builtins.attrValues {
    inherit
      rustToolchain
      ;
    inherit (pkgs)
      rust-analyzer-unwrapped
      nixd
      libxkbcommon
      dav1d
      libinput
      linux-pam
      gcc
      glibc
      cmake
      ;
    inherit (pkgs.llvmPackages)
      libclang
      llvm
      ;
  };

  nativeBuildInputs = [ pkgs.pkg-config ];

  RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
  LLVM_CONFIG_PATH = "${pkgs.llvmPackages.llvm}/bin/llvm-config";
  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  BINDGEN_EXTRA_CLANG_ARGS = "-I${pkgs.linux-pam}/include -I${pkgs.glibc.dev}/include";
}
