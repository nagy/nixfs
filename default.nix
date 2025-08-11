{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  rustPlatform ? pkgs.rustPlatform,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "nixfs";
  version = "0-unstable-2025-08-11";

  src = lib.cleanSource ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
  };

  nativeBuildInputs = [
    pkgs.pkg-config
  ];

  buildInputs = [
    pkgs.fuse
  ];

  meta = {
    license = lib.licenses.agpl3Plus;
    maintainers = with lib.maintainers; [ nagy ];
    mainProgram = "nixfs";
  };
})
