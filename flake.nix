{
  description = "Versioned content-addressed filesystem in a single SQLite file (crate2nix per-crate derivations)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crate2nix.url = "github:nix-community/crate2nix";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  # We import the generated `Cargo.nix` from a derivation (IFD).
  nixConfig.allow-import-from-derivation = true;

  outputs =
    inputs@{ flake-parts, nixpkgs, crate2nix, treefmt-nix, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ treefmt-nix.flakeModule ];
      systems = [ "x86_64-linux" ];

      perSystem =
        {
          system,
          pkgs,
          lib,
          config,
          ...
        }:
        let
          # System libraries the binary links against at build and runtime.
          # fuse3 comes from fuser's `libfuse` default feature; sqlite from
          # rusqlite's libsqlite3-sys. Both are linked to the final binary.
          libInputs = [
            pkgs.fuse3
            pkgs.sqlite
          ];

          # Native tools needed by the bindgen/pkg-config build scripts.
          sysToolInputs = [ pkgs.pkg-config ];

          # link-arg flags so the binary finds system libs at runtime.
          rpathOpts = map (p: "-C link-arg=-Wl,-rpath,${p}") (
            lib.splitString ":" (lib.makeLibraryPath libInputs)
          );
          rpathFlags = lib.concatStringsSep " " rpathOpts;

          # ── Minimal IFD source ──
          # Only Cargo.toml + Cargo.lock + stub src/main.rs. Cargo metadata
          # refuses a manifest with zero targets, and we want the crate list to
          # stay stable (and cached) when only the crate source changes.
          ifdSrc = pkgs.runCommandLocal "sqlitefs-ifd-src" { } ''
            mkdir -p $out/src
            cp ${./Cargo.toml} $out/Cargo.toml
            cp ${./Cargo.lock} $out/Cargo.lock
            echo 'fn main() {}' > $out/src/main.rs
          '';

          # Generate Cargo.nix from the stub source (IFD).
          generated = crate2nix.tools.${system}.generatedCargoNix {
            name = "sqlitefs";
            src = ifdSrc;
          };

          # Full source for the actual build.
          fullSrc = lib.cleanSource ./.;

          # Overlay the full source onto the generated derivation's ./crate
          # directory. Cheap copy — no cargo/crate2nix re-run.
          overlaid = pkgs.runCommandLocal "sqlitefs-overlaid" { } ''
            mkdir -p $out
            cp -r ${generated}/* $out/
            chmod -R u+w $out/crate
            cp -r ${fullSrc}/* $out/crate/
          '';

          cargoNix = pkgs.callPackage "${overlaid}/default.nix" { };

          # Per-crate overrides for the system-linking crates plus the root
          # crate (rpath link args, shell-completion install). Shared between
          # the release package and the debug build.
          mkCrateOverrides =
            release:
            pkgs.defaultCrateOverrides
            // {
              "fuser" = attrs: {
                nativeBuildInputs = sysToolInputs;
                buildInputs = [ pkgs.fuse3 ];
              };
              "libsqlite3-sys" = attrs: {
                nativeBuildInputs = sysToolInputs;
                buildInputs = [ pkgs.sqlite ];
              };
              "sqlitefs" = attrs: {
                inherit release;
                nativeBuildInputs = [ pkgs.installShellFiles ];
                buildInputs = libInputs;
                extraRustcOpts = rpathOpts;
                postInstall = ''
                  installShellCompletion --cmd sqlitefs \
                    --bash <($out/bin/sqlitefs completions bash) \
                    --zsh <($out/bin/sqlitefs completions zsh) \
                    --fish <($out/bin/sqlitefs completions fish)
                '';
              };
            };

          # Release build: the actual package.
          sqlitefs = (cargoNix.rootCrate.build.override {
            crateOverrides = mkCrateOverrides true;
          }).overrideAttrs (old: {
            meta = old.meta // {
              description = "Versioned content-addressed filesystem in a single SQLite file";
              license = lib.licenses.agpl3Plus;
              mainProgram = "sqlitefs";
              platforms = lib.platforms.linux;
            };
          });

          # Debug build: only the release switch flips, so debug_assert!s are
          # exercised in the NixOS VM test while the main package stays release.
          sqlitefsDebug = cargoNix.rootCrate.build.override {
            crateOverrides = mkCrateOverrides false;
          };

          # The VM test needs a debug build of the *installed* binary.
          vmTest = import ./test/default.nix {
            inherit pkgs lib;
            sqlitefs = sqlitefsDebug;
          };
        in
        {
          packages.sqlitefs = sqlitefs;
          packages.default = config.packages.sqlitefs;

          checks.sqlitefs = vmTest;

          treefmt = {
            projectRootFile = "flake.nix";
            programs.rustfmt.enable = true;
            programs.rustfmt.edition = "2024";
            settings.formatter.rustfmt.options = lib.mkAfter [
              "--config"
              "max_width=100,comment_width=100,wrap_comments=true,group_imports=StdExternalCrate,imports_granularity=Crate,condense_wildcard_suffixes=true,error_on_line_overflow=true,error_on_unformatted=true,format_code_in_doc_comments=true,format_macro_matchers=true,format_macro_bodies=true,format_strings=true,hex_literal_case=Lower,normalize_comments=true,use_field_init_shorthand=true"
            ];
          };

          devShells.default = pkgs.mkShell {
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.rustc
              pkgs.cargo
              pkgs.rustfmt
              pkgs.clippy
              pkgs.installShellFiles
              config.treefmt.build.wrapper
            ];
            buildInputs = libInputs;
            RUSTFLAGS = rpathFlags;
            LD_LIBRARY_PATH = lib.makeLibraryPath libInputs;
          };
        };
    };
}
