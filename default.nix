{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  rustPlatform ? pkgs.rustPlatform,
  pkg-config ? pkgs.pkg-config,
  fuse3 ? pkgs.fuse3,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "nixfs";
  version = "0-unstable-2026-07-07";

  strictDeps = true;

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

  passthru.tests = let
    nixos-lib = import (pkgs.path + "/nixos/lib") { inherit (pkgs) lib; };
  in {
    nixfs = nixos-lib.runTest {
      hostPkgs = pkgs;

      name = "nixfs";

      nodes.machine = { pkgs, ... }: {
        environment.systemPackages = [ finalAttrs.finalPackage pkgs.hello pkgs.fuse3 ];
        boot.kernelModules = [ "fuse" ];
        nix.settings.experimental-features = [ "nix-command" ];
        nix.nixPath = [ "nixpkgs=${pkgs.path}" ];
        virtualisation.diskSize = 1024;
      };

      testScript = ''
        machine.succeed("mkdir -p /tmp/mnt")
        machine.succeed("nixfs /tmp/mnt > /dev/null 2>&1 &")
        machine.wait_until_succeeds("test -L /tmp/mnt/hello")
        machine.succeed("readlink /tmp/mnt/hello | grep '/nix/store/'")
        machine.succeed("$(readlink /tmp/mnt/hello)/bin/hello | grep 'Hello'")
        machine.succeed("fusermount3 -u /tmp/mnt")
      '';
    };
  };

  meta = {
    description = "FUSE filesystem exposing Nix package attributes as virtual symlinks";
    longDescription = ''
      nixfs is a FUSE filesystem that maps Nix package attributes to virtual
      symlinks. Mount at /nixfs (or any path), then access e.g. /nixfs/vim to
      get a symlink pointing to the Nix store path of `<nixpkgs>.vim`.

      Derivation attributes are resolved lazily: `lookup` checks existence
      via `nix eval`, and `readlink` triggers `nix-build` on first access,
      so store paths are materialized only when actually used.
    '';
    license = lib.licenses.agpl3Plus;
    homepage = "https://github.com/nagy/nixfs";
    maintainers = with lib.maintainers; [ nagy ];
    mainProgram = "nixfs";
    platforms = lib.platforms.linux;
  };
})
