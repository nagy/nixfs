{
  pkgs ? import <nixpkgs> { },
}:

pkgs.mkShell {
  name = "nixfs";

  nativeBuildInputs = [
    pkgs.cargo
    pkgs.rustc
    pkgs.rustfmt
    pkgs.clippy
    pkgs.pkg-config
  ];

  buildInputs = [
    pkgs.fuse3
  ];
}
