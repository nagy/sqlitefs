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

          # Emacs module build: same crate with the `emacs` feature on — the
          # cdylib (libsqlitefs.so) is the loadable dynamic module. The feature
          # adds the `emacs` crate to the dep graph, so this build carries its
          # own cargoArtifacts (a superset of the default set).
          cargoArtifactsEmacs = craneLib.buildDepsOnly {
            inherit src nativeBuildInputs;
            buildInputs = libInputs;
            cargoExtraArgs = "--features emacs";
          };

          sqlitefs-emacs = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifactsEmacs;
              pname = "sqlitefs-emacs";
              version = "0.1.0";
              cargoExtraArgs = "--features emacs";
              meta = {
                description = "Emacs dynamic module exposing the sqlitefs library";
                platforms = lib.platforms.linux;
              };
            }
          );

          # emacs-sqlitefs: the elisp package (emacs/sqlitefs.el) plus the
          # dynamic module in one share/emacs/site-lisp tree. Byte-compiles
          # with warnings-as-errors and generates autoloads; the ERT suite
          # runs as an install-check (installCheckPhase, not checkPhase:
          # stdenv runs checks before install, so plain checkPhase would see
          # an empty $out). Wired into checks, so `nix flake check` covers
          # the Emacs integration end to end.
          emacs-sqlitefs = pkgs.stdenv.mkDerivation {
            pname = "emacs-sqlitefs";
            version = "0.1.0";

            # Elisp sources + ERT suite only.
            src = lib.cleanSourceWith {
              src = ./.;
              filter =
                path: type:
                (type == "directory" && (lib.hasInfix "/emacs" path || lib.hasInfix "/test" path))
                || (type == "regular" && (lib.hasInfix "/emacs/" path || lib.hasInfix "/test/" path));
            };

            nativeBuildInputs = [
              pkgs.emacs
              sqlitefs-emacs
            ];

            buildPhase = ''
              runHook preBuild

              # Module library next to the elisp so `sqlitefs--locate-module'
              # finds it via `load-path' (also during byte-compilation).
              install -Dm555 ${sqlitefs-emacs}/lib/libsqlitefs.so \
                emacs/libsqlitefs.so

              # Byte-compile with warnings as errors.
              for file in emacs/*.el; do
                emacs --batch -L emacs \
                  --eval "(setq byte-compile-error-on-warn t)" \
                  -f batch-byte-compile "$file"
              done

              # Autoload stamps from the ;;;###autoload cookies.
              emacs --batch -L emacs -l loaddefs-gen \
                --eval '(loaddefs-generate "emacs" "emacs/loaddefs.el")'

              runHook postBuild
            '';

            installPhase = ''
              runHook preInstall
              mkdir -p $out/share/emacs/site-lisp
              # emacs/*.el covers the generated loaddefs.el too.
              install -m644 emacs/*.el emacs/*.elc \
                emacs/libsqlitefs.so -t $out/share/emacs/site-lisp
              runHook postInstall
            '';

            # ERT suite runs against the *installed* package (compiled elisp
            # + module library + generated autoloads), with the sqlitefs CLI
            # from the module build as the repo fixture driver.
            doInstallCheck = true;
            installCheckPhase = ''
              runHook preInstallCheck
              export SQLITEFS_TEST_BIN=${sqlitefs-emacs}/bin/sqlitefs
              export SQLITEFS_TEST_MODULE=$out/share/emacs/site-lisp/libsqlitefs.so
              emacs --batch -L $out/share/emacs/site-lisp \
                -l test/emacs-module-tests.el
              runHook postInstallCheck
            '';

            meta = {
              description = "Emacs interface to sqlitefs repositories (module + elisp)";
              platforms = lib.platforms.linux;
            };
          };

          # Static rustdoc HTML (what `cargo doc` writes to ./target/doc/)
          # installed under $out/share/doc. Reuses the same cargoArtifacts,
          # so only doc crates compile; --no-deps (crane's default) keeps
          # third-party crates out of the search index. Browse offline:
          # nix run nixpkgs#python3 -- -m http.server -d <doc-out>/share/doc
          docs = craneLib.cargoDoc {
            inherit cargoArtifacts;
            inherit (commonArgs) src nativeBuildInputs buildInputs;
            meta.description = "sqlitefs API documentation";
          };

          # Doctests: code blocks in doc comments compiled and run against
          # the library (`cargo test --doc`), same artifact set as above.
          doctests = craneLib.cargoDocTest {
            inherit cargoArtifacts;
            inherit (commonArgs) src nativeBuildInputs buildInputs;
            meta.description = "sqlitefs doctests";
          };

          # The VM test needs a debug build of the *installed* binary.
          vmTest = import ./test/default.nix {
            inherit pkgs lib;
            sqlitefs = sqlitefsDebug;
          };
        in
        {
          packages.sqlitefs = sqlitefs;
          packages.sqlitefs-emacs = sqlitefs-emacs;
          packages.emacs-sqlitefs = emacs-sqlitefs;
          packages.sqlitefs-doc = docs;
          packages.default = config.packages.sqlitefs;

          checks.sqlitefs = vmTest;
          checks.emacs-sqlitefs = emacs-sqlitefs;
          checks.doctests = doctests;

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
              pkgs.emacs
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
