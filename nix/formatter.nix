{ pkgs, rustToolchain }:
pkgs.writeShellApplication {
  name = "nix3-fmt-wrapper";
  runtimeInputs = builtins.attrValues {
    inherit (pkgs)
      nixfmt
      taplo
      fd
      ;
    inherit rustToolchain;
  };
  text = ''
    fd "$@" -t f -e nix -x nixfmt -q '{}'
    fd "$@" -t f -e toml -x taplo format '{}'
    cargo fmt
  '';
}
