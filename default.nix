{
  pkgs ? import <nixpkgs> { },
  lib ? pkgs.lib,
  rustPlatform ? pkgs.rustPlatform,
  pkg-config ? pkgs.pkg-config,
  fuse3 ? pkgs.fuse3,
  sqlite ? pkgs.sqlite,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "sqlitefs";
  version = "0.1.0";

  src = lib.cleanSource ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
  };
  strictDeps = true;

  nativeBuildInputs = [
    pkg-config
    pkgs.installShellFiles
  ];

  buildInputs = [
    fuse3
    sqlite
  ];

  postInstall = ''
    installShellCompletion --cmd sqlitefs \
      --bash <($out/bin/sqlitefs completions bash) \
      --zsh <($out/bin/sqlitefs completions zsh) \
      --fish <($out/bin/sqlitefs completions fish)
  '';

  passthru.tests.sqlitefs = import ./test/default.nix {
    inherit pkgs lib;
    sqlitefs = finalAttrs.finalPackage;
  };

  meta = {
    description = "Versioned content-addressed filesystem in a single SQLite file";
    license = lib.licenses.agpl3Plus;
    mainProgram = "sqlitefs";
    platforms = lib.platforms.linux;
  };
})
