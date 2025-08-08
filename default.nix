{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  rustPlatform ? pkgs.rustPlatform,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "nixfs";
  version = "0-unstable-2025-08-07";

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
    description = "Implementation of plus codes, to be used as CLI tool or crate";
    homepage = "https://github.com/janne/pluscodes-rs";
    license = lib.licenses.mit;
    maintainers = with lib.maintainers; [ nagy ];
    mainProgram = "pluscodes";
  };
})
