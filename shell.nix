{
  pkgs ? import <nixpkgs> { },
}:
pkgs.mkShell {
  name = "sqlitefs";
  nativeBuildInputs = [
    pkgs.cargo
    pkgs.rustc
    pkgs.rustfmt
    pkgs.clippy
    pkgs.pkg-config
  ];
  buildInputs = [
    pkgs.sqlite
  ];
}
