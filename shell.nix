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
  ];

  # fuse3 provides fusermount3, needed at runtime by the pure-rust mount backend
  buildInputs = [
    pkgs.fuse3
  ];
}
