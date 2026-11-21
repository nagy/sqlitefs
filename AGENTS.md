# AGENTS.md — sqlitefs

## Purpose

`sqlitefs`: versioned, content-addressed filesystem where one SQLite file serves
as both the repository and the mountable volume. Rust edition 2024. License:
AGPL-3.0. Ships the `sqlitefs` binary, a library crate, a FUSE mount, a CLI and an
Emacs dynamic module.

## Layout

| Path | Ownership |
|---|---|
| `src/lib.rs` | Library crate: modules plus unit tests, shared by the binary and the Emacs module. |
| `src/main.rs` | Entry point: CLI parse and mount dispatch (`flock`, `fuser::mount`). |
| `src/fs.rs` | Stateful core: the `Entry` tree, `SqliteFs`, DAG build and commit, the chunk store. |
| `src/fuse.rs` | FUSE trait impl: writable root, `.sqlitefs/` snapshots, tags, rename, symlinks, hard links. |
| `src/emacs_module.rs` | Emacs dynamic module behind the `emacs` feature: read-only refs, ls, read, log, catalog. |
| `src/cli.rs` | CLI: `init`, `mount`, `fsck`, `rebuild-index`, `show-ref`, `ls`, `cat`, `cp`, `mv`, plus repo-level operations. |
| `src/dag.rs` | CIDv1, dag-pb, dag-cbor, the v1-2025 UnixFS DAG builder. |
| `src/cdc.rs` | Content-defined chunking: FastCDC v2020 plus SHA-256. |
| `emacs/sqlitefs.el` | Elisp frontend: dired-like repo browser and read-only visit buffers. |
| `test/default.nix` | NixOS VM test through `nixos-lib.runTest`, wired as `checks.sqlitefs`. |
| `test/emacs-module-tests.el` | ERT tests for the Emacs module. |
| `docs/adr/` | Decision records 0001 to 0005. |
| `CONTEXT.md` | Domain glossary. |

## Build and test

- `nix flake check` — build, clippy, tests, doctests, treefmt, the VM test and the
  Emacs integration check. Gate for a commit.
- `nix build .#sqlitefs` — the package.
- `nix build .#sqlitefs-doc` — static rustdoc HTML. Serve `share/doc/` with
  `python3 -m http.server` for an offline read. Doctests run as `checks.doctests`.
- `nix build .#sqlitefs-emacs` — the Emacs module cdylib.
- `nix build .#emacs-sqlitefs` — elisp, bytecode, autoloads and the `.so` in one
  site-lisp tree. It byte-compiles with warnings as errors and runs the ERT suite
  as an install check.
- `nix fmt` — treefmt drives rustfmt, taplo and nixfmt. The rustfmt options are
  CLI args in `flake.nix` (`max_width = 100`, `format_strings = true`,
  `imports_granularity = "Crate"`, `group_imports = "StdExternalCrate"`).
- `nix develop` — dev shell.
- `cargo build --features emacs` — build the module into
  `target/debug/libsqlitefs.so`.
- `emacs -Q --batch -l test/emacs-module-tests.el` — ERT suite in the dev shell.
  It needs the module built with the feature first.

## Architecture

### Canonical store

- The `.db` file is the repository. `directory_index` and `block_index` are
  materialized views, rebuildable from the chunk store.
- Dual chunking: FastCDC for physical dedup, UnixFS v1-2025 for public IPFS CIDs,
  mapped through `block_index`.
- IPFS-native DAG: dag-pb directories, the v1-2025 file layout, dag-cbor commits,
  content-addressed with CIDv1 and SHA-256.
- Live writable FUSE: writes stay in memory, hash on `close()`, and commit
  periodically and by hand. A hidden `.sqlitefs/` directory serves snapshot
  browsing.

### FUSE operations

| Operation | Status |
|---|---|
| `lookup`, `getattr`, `readdir` | Implemented |
| `mkdir`, `rmdir` | Implemented, auto-commit |
| `create`, `open`, `write`, `release`, `read` | Implemented, CDC plus commit on release |
| `unlink` | Implemented, auto-commit |
| `rename` | Implemented, cross-directory, overwrite, auto-commit |
| `symlink`, `readlink` | Implemented |
| `link` | Implemented for files. The new parent must be writable. |
| `forget` | Implemented |
| `getxattr`, `listxattr` | Stub, no xattrs |
| `setattr` | Implemented for truncate-to-zero and chmod, auto-commit. uid, gid and utimes are ignored. |
| `flush`, `fsync` | No-op |

### CLI without FUSE

`sqlitefs ls|cat|cp|mv <db> <path>` reads the committed `main` tree directly
through `refs`, `commits` and `directory_index`. `cp` and `mv` reuse the
`SqliteFs` entry tree and commit per command. The `.sqlitefs/` namespace stays
unreachable. A CLI mutation commits to the DB, but a concurrent FUSE mount keeps
its own in-memory tree and sees no CLI change until a remount.

### Emacs module

The `emacs` feature compiles the cdylib into an Emacs dynamic module. Every
function stays stateless: it opens the DB with `SQLITE_OPEN_READ_ONLY` and
resolves the committed tree, so it runs safely next to a live FUSE mount. Every
defun argument is required, and nil selects the documented default, which the
Elisp frontend wraps.

- `(sqlitefs-refs PATH)` — alist of `(NAME . COMMIT-CID)`.
- `(sqlitefs-ls PATH DIR REF)` — list of `(NAME KIND SIZE MODE)`.
- `(sqlitefs-read PATH FILE REF)` — file content, UTF-8 only.
- `(sqlitefs-log PATH REF)` — `(CID . TIMESTAMP)` history, newest first.
- `(sqlitefs-catalog)` — the exported-function catalog as `(NAME ARGS DOC)` lists.

The catalog in `emacs_module.rs` is the single source of truth for the API
surface, and the ERT parity test pins every entry through `fboundp`, arity and
the doc first line. Add a catalog entry together with its `#[defun]`.

The Elisp frontend `emacs/sqlitefs.el` holds `sqlitefs-dired`, a dired-like
browser, `sqlitefs-visit`, a read-only file buffer, and `sqlitefs-show-refs`. It
locates the module library through `load-path`, which the Nix package fills with
the `.so` beside the elisp, and falls back to `target/{debug,release}` for a dev
checkout.

### Snapshot model

- Auto-commit every 60s while dirty, checked on `lookup`, `create` and `open`.
- The file mode lives in the file manifest (dag-cbor), and chmod re-stores the
  manifest with a new CID. `directory_index.file_mode` is a denormalized cache,
  and `sqlitefs rebuild-index` recreates it from refs. Run that once when
  upgrading an old repo.
- `touch .sqlitefs/tag/<name>` creates a manual named snapshot.
- `link` creates a second directory entry for one inode, so both names hold one
  manifest CID and dedup follows. Files only, and nlink always reports 1. See
  `docs/adr/0005-hard-links.md`.
- Snapshot browsing lives at `.sqlitefs/refs/<name>/`, hidden from readdir.
- Hash access at `.sqlitefs/c/<hash-prefix>/` works through a prefix lookup in
  the commits table, cached.

## Rules

- Keep `AGENTS.md` and `CONTEXT.md` current. Check both before every commit.
- Add a dependency only after the ponytail filter. Prefer the standard library and
  a minimal crate.
- Keep the Open work list current. Add new work as a numbered entry and remove a
  finished entry entirely. Do not cross it out or mark it done.
- Keep clippy clean at the pedantic level set by `[lints.clippy]` in
  `Cargo.toml`. The intentional `as` casts for varint and base32 encoding and for
  FUSE offsets carry their allow there.
- Keep the module read-only. A write path through the module breaks the safety
  argument next to a live mount.

## Conventions

- Follow the `rust-idioms` skill for a Rust style question.
- Rustfmt and clippy stay mandatory before a commit: `cargo fmt` and
  `cargo clippy`.
- Format through `nix fmt` for a tree-wide pass. Do not run a formatter
  standalone.
- Commit without GPG signing: `git commit --no-gpg-sign`.
- Name a chunking change in the commit body. FastCDC and UnixFS answer different
  questions, and a reader needs to know which one moved.

## Dependencies

| Dependency | Required? | Why |
|---|---|---|
| `clap` 4 | required | CLI parse, with `clap_complete` for the shell completions. |
| `rusqlite` 0.31 | required | SQLite is the store. |
| `fuser` 0.15 | required | FUSE operations. `libc` supplies the errnos. |
| `fastcdc` 4 | required | Content-defined chunking for physical dedup. |
| `sha2` | required | SHA-256 for the CID and the chunk hash. |

- UnixFS v1-2025 uses a custom DAG builder in `src/dag.rs`: raw leaves, a balanced
  dag-pb tree, per-block offsets. No mature Rust implementation exists, so
  validate against the `ipfs-chainer` reference.
- The chunker stays pluggable. FastCDC serves today, and rapidCDC may replace it
  after an evaluation.

## Open work

- Evaluate rapidCDC against FastCDC.
