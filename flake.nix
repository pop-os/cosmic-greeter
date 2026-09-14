{
  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f (pkgsFor system));
    in
    {
      devShells = forAllSystems (pkgs: {
        default =
          let
            rustToolchain = pkgs.rust-bin.stable.latest.default.override {
              extensions = [
                "rust-analyzer"
                "rust-src"
                "llvm-tools"
              ];
            };
          in
          import ./nix/shell.nix { inherit pkgs rustToolchain; };
      });

      packages = forAllSystems (
        pkgs:
        let
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-analyzer"
              "rust-src"
              "llvm-tools"
            ];
          };
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        in
        {
          cosmic-greeter = pkgs.callPackage ./nix/package.nix {
            inherit rustPlatform;
          };
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.cosmic-greeter;
        }
      );

      formatter = forAllSystems (
        pkgs:
        let
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-analyzer"
              "rust-src"
              "llvm-tools"
            ];
          };
        in
        import ./nix/formatter.nix { inherit pkgs rustToolchain; }
      );
    };
}
