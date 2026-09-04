{
  description = "Versioned content-addressed filesystem in a single SQLite file (crane)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{
      flake-parts,
      nixpkgs,
      crane,
      treefmt-nix,
      ...
    }:
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
          craneLib = crane.mkLib pkgs;

          # System libraries the binary links against at build and runtime.
          # fuse3 comes from fuser's `libfuse` default feature; sqlite from
          # rusqlite's libsqlite3-sys. Both are linked to the final binary.
          libInputs = [
            pkgs.fuse3
            pkgs.sqlite
          ];

          # Native tools needed by the bindgen/pkg-config build scripts, plus
          # installShellFiles for the shell-completion install below.
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.installShellFiles
          ];

          # Source as seen by cargo: everything tracked by git.
          src = craneLib.cleanCargoSource ./.;

          # Cargo.lock has no network access during the build; fetch deps
          # first, then feed them to cargo through the registry cache.
          cargoArtifacts = craneLib.buildDepsOnly {
            inherit src nativeBuildInputs;
            buildInputs = libInputs;
          };

          # Debug builds (the VM test) need the deps compiled in dev profile.
          cargoArtifactsDebug = craneLib.buildDepsOnly {
            inherit src nativeBuildInputs;
            buildInputs = libInputs;
            env.CARGO_PROFILE = "dev";
          };

          commonArgs = {
            inherit src nativeBuildInputs;
            buildInputs = libInputs;
          };

          completionInstall = ''
            installShellCompletion --cmd sqlitefs \
              --bash <($out/bin/sqlitefs completions bash) \
              --zsh <($out/bin/sqlitefs completions zsh) \
              --fish <($out/bin/sqlitefs completions fish)
          '';

          # Release build: the actual package.
          sqlitefs = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              postInstall = completionInstall;
              meta = {
                description = "Versioned content-addressed filesystem in a single SQLite file";
                license = lib.licenses.agpl3Plus;
                mainProgram = "sqlitefs";
                platforms = lib.platforms.linux;
              };
            }
          );

          # Debug build: only CARGO_PROFILE flips, so debug_assert!s are
          # exercised in the NixOS VM test while the main package stays release.
          sqlitefsDebug = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifactsDebug;
              postInstall = completionInstall;
              env.CARGO_PROFILE = "dev";
            }
          );

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

          checks.default = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
            }
          );

          treefmt = {
            projectRootFile = "flake.nix";
            programs.rustfmt.enable = true;
            programs.rustfmt.edition = "2024";
            programs.taplo.enable = true;
            programs.nixfmt.enable = true;
            settings.formatter.rustfmt.options = lib.mkAfter [
              "--config"
              "max_width=100,comment_width=100,wrap_comments=true,group_imports=StdExternalCrate,imports_granularity=Crate,condense_wildcard_suffixes=true,error_on_line_overflow=true,error_on_unformatted=true,format_code_in_doc_comments=true,format_macro_matchers=true,format_macro_bodies=true,format_strings=true,hex_literal_case=Lower,normalize_comments=true,use_field_init_shorthand=true"
            ];
          };

          devShells.default = craneLib.devShell {
            packages = [
              pkgs.rustfmt
              pkgs.clippy
              pkgs.installShellFiles
              pkgs.pkg-config
              config.treefmt.build.wrapper
            ];
            nativeBuildInputs = nativeBuildInputs;
            buildInputs = libInputs;
            # fuser's build.sh shells out to `pkg-config` for fuse3 and needs the
            # `fuse3.pc` on PKG_CONFIG_PATH. The devShell must expose both, else
            # `cargo build`/`cargo doc` fail with "pkg-config command could not be found".
            PKG_CONFIG_PATH = "${lib.makeSearchPath "lib/pkgconfig" libInputs}";
            LD_LIBRARY_PATH = lib.makeLibraryPath libInputs;
            RUSTFLAGS = lib.concatStringsSep " " (
              map (p: "-C link-arg=-Wl,-rpath,${p}") (lib.splitString ":" (lib.makeLibraryPath libInputs))
            );
          };
        };
    };
}
