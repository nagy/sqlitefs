{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  rustPlatform ? pkgs.rustPlatform,
  pkg-config ? pkgs.pkg-config,
  sqlite ? pkgs.sqlite,
}:
rustPlatform.buildRustPackage {
  pname = "sqlitefs";
  version = "0.1.0";
  src = lib.cleanSource ./.;
  cargoLock = { lockFile = ./Cargo.lock; };
  strictDeps = true;
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ sqlite ];
  meta = {
    description = "Versioned content-addressed filesystem in a single SQLite file";
    license = lib.licenses.agpl3Plus;
    mainProgram = "sqlitefs";
    platforms = lib.platforms.linux;
  };
}
