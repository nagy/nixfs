{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  rustPlatform ? pkgs.rustPlatform,
  pkg-config ? pkgs.pkg-config,
  fuse3 ? pkgs.fuse3,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "nixfs";
  version = "0-unstable-2026-06-19";

  src = lib.cleanSource ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
  };

  nativeBuildInputs = [
    pkg-config
  ];

  buildInputs = [
    fuse3
  ];

  meta = {
    license = lib.licenses.agpl3Plus;
    maintainers = with lib.maintainers; [ nagy ];
    mainProgram = "nixfs";
  };
})
