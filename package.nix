# Based on lilyinstarlight's work on:
# https://github.com/lilyinstarlight/nixos-cosmic/blob/main/pkgs/cosmic-greeter/package.nix
{
  lib,
  rustPlatform,
  cmake,
  coreutils,
  just,
  libinput,
  linux-pam,
  stdenv,
  udev,
  xkeyboard_config,
  wayland,
  vulkan-loader,
  xorg,
  libGL,
  libxkbcommon,
  makeBinaryWrapper,
  pkg-config,
  cosmic-icons,
}:
rustPlatform.buildRustPackage {
  pname = "cosmic-greeter";
  version = "1.0.0-alpha.5.1";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.difference ./. (
      lib.fileset.unions [
        (lib.fileset.maybeMissing ./result)
        (lib.fileset.maybeMissing ./target)
        (lib.fileset.maybeMissing ./.git)
        (lib.fileset.fileFilter (
          file:
          builtins.elem file.name [
            "flake.nix"
            "flake.lock"
            "LICENSE"
            "README.md"
            ".gitignore"
            ".gitattributes"
            "package.nix"
          ]
        ) ./.)
      ]
    );
  };

  nativeBuildInputs = [
    cmake
    just
    makeBinaryWrapper
    pkg-config
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    libinput
    linux-pam
    udev
    wayland
    vulkan-loader
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libxcb
    libGL
    libxkbcommon
  ];

  useFetchCargoVendor = true;
  cargoHash = "sha256-nmkM/Jm2P5ftZFfzX+O1Fe6eobRbgBkajZsbyI67Zfw=";
  cargoBuildFlags = [ "--all" ];

  dontUseJustBuild = true;
  dontUseJustCheck = true;

  justFlags = [
    "--set"
    "prefix"
    (placeholder "out")
    "--set"
    "bin-src"
    "target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/cosmic-greeter"
    "--set"
    "daemon-src"
    "target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/cosmic-greeter-daemon"
  ];

  postPatch = ''
    substituteInPlace src/greeter.rs --replace-fail '/usr/bin/env' '${lib.getExe' coreutils "env"}'
  '';

  postInstall = ''
    wrapProgram "$out/bin/cosmic-greeter" \
      --set-default X11_BASE_RULES_XML "${xkeyboard_config}/share/X11/xkb/rules/base.xml" \
      --set-default X11_EXTRA_RULES_XML "${xkeyboard_config}/share/X11/xkb/rules/base.extras.xml" \
      --suffix XDG_DATA_DIRS : "${cosmic-icons}/share" \
      --prefix LD_LIBRARY_PATH : "${
        lib.makeLibraryPath [
          xorg.libX11
          xorg.libXcursor
          xorg.libXi
          xorg.libxcb
          libGL
          libxkbcommon
          wayland
          vulkan-loader
        ]
      }"
  '';
}
