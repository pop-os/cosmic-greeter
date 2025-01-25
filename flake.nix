{
  inputs = {
    flake-parts.url = "github:hercules-ci/flake-parts";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];

      perSystem =
        { pkgs, self', ... }:
        {
          devShells.default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
            ];

            strictDeps = true;
          };
          packages = {
            default = self'.packages.cosmic-greeter;
            cosmic-greeter = pkgs.callPackage ./package.nix { };
          };
        };
    };
}
