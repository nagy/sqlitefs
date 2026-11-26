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
      self,
      flake-parts,
      nixpkgs,
      crane,
      treefmt-nix,
      ...
    }:
    # The elisp package has no release tag, so its version carries the date
    # of the flake source.  A plain path import has no git metadata, so fall
    # back to the epoch instead of failing.
    let
      sourceInfo = self.sourceInfo or { };
      stamp =
        let
          date = sourceInfo.lastModifiedDate or "19700101000000";
        in
        if date == null then "19700101000000" else toString date;
      elispVersion = "0.1.0-unstable-${builtins.substring 0 4 stamp}-${builtins.substring 4 2 stamp}-${builtins.substring 6 2 stamp}";
    in
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
              # No description here: crane fills meta.description from the
              # `description' field in Cargo.toml.
              meta = {
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
              meta.description = "sqlitefs debug build (debug_assert!s live, used by the VM test)";
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
          # dynamic module in one elpa directory.  melpaBuild runs
          # package-build, byte-compiles through elpa2nix with warnings as
          # errors and generates the autoloads from the ;;;###autoload
          # cookies.  The module is a separate cargo build, so it enters as
          # a store path and lands in postInstall.  The ERT suite runs as an
          # install-check (installCheckPhase, not checkPhase: elpa2nix
          # compiles during install, so checkPhase would see no $out).
          # Wired into checks, so `nix flake check` covers the Emacs
          # integration end to end.
          # Overridable, so a consumer can swap the Emacs or the package set:
          #   inputs.sqlitefs.packages.${system}.emacs-sqlitefs.override {
          #     emacs = pkgs.emacs-nox;
          #   }
          emacs-sqlitefs = lib.makeOverridable (
            {
              # `melpaBuild' closes over the Emacs of its package set, so an
              # `emacs' argument on the build does nothing. Build the set
              # against the chosen Emacs instead.
              emacs ? pkgs.emacs,
              emacsPackages ? pkgs.emacsPackagesFor emacs,
              melpaBuild ? emacsPackages.melpaBuild,
              # The flake passes this, so a plain import stays explicit.
              elispVersion,
            }:
            melpaBuild {
              pname = "sqlitefs";
              inherit elispVersion;
              version = elispVersion;

              preferLocalBuild = true;

              # Emacs writes ~/.emacs.d during byte-compilation, so the build
              # and the install check get a private writable home.
              nativeBuildInputs = [ pkgs.writableTmpDirAsHomeHook ];

              # Elisp sources + ERT suite only.  The Rust tree stays out, and
              # the module is not in the source at all.
              src = lib.cleanSourceWith {
                src = ./.;
                filter =
                  path: type:
                  (type == "directory" && (lib.hasInfix "/emacs" path || lib.hasInfix "/test" path))
                  || (type == "regular" && (lib.hasInfix "/emacs/" path || lib.hasInfix "/test/" path));
              };

              # The elisp sits in emacs/, and package-build strips that common
              # prefix, so the installed package root stays flat.
              files = ''("emacs/*.el")'';

              # package-build writes its own `Package-Version' header from the
              # recipe version, so the source header keeps only the base version.

              # Byte-compilation warnings fail the build.
              turnCompilationWarningToError = true;

              # The elpa directory name carries the version, and the module
              # file does not exist yet, so a plain glob would vanish under
              # nullglob.  The loop resolves the directory first.
              postInstall = ''
                for dir in $out/share/emacs/site-lisp/elpa/sqlitefs-*; do
                  install -Dm555 ${sqlitefs-emacs}/lib/libsqlitefs.so \
                    "$dir/libsqlitefs.so"
                done
              '';

              # ERT suite runs against the *installed* package (compiled elisp
              # + module library + generated autoloads), with the sqlitefs CLI
              # from the module build as the repo fixture driver.  The suite
              # stays in the unpacked source, which melpaBuild leaves in place.
              doInstallCheck = true;
              installCheckPhase = ''
                runHook preInstallCheck
                export SQLITEFS_TEST_BIN=${sqlitefs-emacs}/bin/sqlitefs
                # Glob the suites so a new test file needs no flake edit.  printf
                # repeats "-l %s" per match.  failglob turns an empty glob into a
                # shell error, so the || guard fires even without `set -e'.
                # The unquoted $test_args must split into -l <file> pairs, which
                # stays safe because a test file name holds no space.
                shopt -s failglob
                test_args=$(printf -- "-l %s " test/*-test.el) || {
                  echo 'no tests matched test/*-test.el' >&2
                  exit 1
                }
                shopt -u failglob
                for dir in $out/share/emacs/site-lisp/elpa/sqlitefs-*; do
                  export SQLITEFS_TEST_MODULE=$dir/libsqlitefs.so
                  emacs --batch -L "$dir" $test_args -f ert-run-tests-batch-and-exit
                done
                runHook postInstallCheck
              '';

              meta = {
                description = "Emacs interface to sqlitefs repositories (module + elisp)";
                longDescription = ''
                  Read-only browsing of sqlitefs repositories from Emacs.  A
                  Dired-like tree browser, read-only file buffers and a ref list,
                  backed by a dynamic module that needs no FUSE mount.
                '';
                license = lib.licenses.agpl3Plus;
                homepage = "https://github.com/nagy/sqlitefs";
                maintainers = with lib.maintainers; [ nagy ];
                platforms = lib.platforms.linux;
              };
            }
          ) { inherit elispVersion; };

          # Lint gate for the elisp: checkdoc and package-lint catch what
          # byte-compilation cannot.  The unpatched source is linted, and
          # `test/' stays in scope so the suite keeps the same header,
          # docstring and prefix rules as the code under test.
          elispLint =
            pkgs.runCommandLocal "sqlitefs-elisp-lint"
              {
                nativeBuildInputs = [
                  (pkgs.emacsPackages.emacsWithPackages (p: [ p.package-lint ]))
                  pkgs.writableTmpDirAsHomeHook
                ];
                # `src' is pre-filtered to emacs/ and test/, and package-lint
                # needs a writable tree, so copy it out of the store first.
                src = emacs-sqlitefs.src;
                meta.description = "checkdoc and package-lint for the sqlitefs elisp";
              }
              ''
                cp -r "$src"/. .
                # package-lint assumes an online ELPA.  `package-lint-main-file'
                # keeps the test file attached to the package it tests instead
                # of making it its own package with a missing header.
                emacs --batch -l package-lint \
                  --eval '(setq package-lint-main-file "emacs/sqlitefs.el")' \
                  -f package-lint-batch-and-exit \
                  emacs/*.el test/*.el
                # checkdoc always exits zero; gate on its output instead.
                lint_log=$(emacs --batch -l checkdoc \
                  --eval '(setq checkdoc-arguments-in-order-flag t)' \
                  --eval '(mapc (lambda (f) (checkdoc-file f)) command-line-args-left)' \
                  emacs/*.el test/*.el 2>&1)
                if [ -n "$lint_log" ]; then printf '%s\n' "$lint_log"; exit 1; fi
                touch $out
              '';

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
          # runTest's `meta` submodule has no `description` option, so the
          # description is overlaid onto the test derivation here instead.
          vmTestRaw = import ./test/default.nix {
            inherit pkgs lib;
            sqlitefs = sqlitefsDebug;
          };
          vmTest = vmTestRaw // {
            meta = (vmTestRaw.meta or { }) // {
              description = "sqlitefs NixOS VM test (FUSE mount, snapshots, CLI interop)";
            };
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
          checks.lint = elispLint;
          checks.doctests = doctests;

          checks.default = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
              meta.description = "sqlitefs test suite";
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
