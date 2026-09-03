# sqlitefs

A versioned, content-addressed filesystem where a single SQLite file is both the repository and the mountable volume.
Built in Rust.

## Project structure

```
.
├── README.org           # features, usage, development
├── CONTEXT.md           # ubiquitous language / glossary
├── AGENTS.md            # this file
├── Cargo.toml
├── Cargo.lock
├── flake.nix            # Nix flake: packages, devShells, checks (crate2nix)
├── flake.lock
├── test/
│   └── default.nix      # NixOS VM test (nixos-lib.runTest, wired via checks.sqlitefs)
├── .gitignore
├── LICENSE              # AGPL-3.0
├── src/
│   ├── main.rs          # entry: FUSE trait impl (writable root + .sqlitefs/ snapshots + tag + rename + symlinks + hard links)
│   ├── fs.rs            # stateful core: Entry tree, SqliteFs, DAG build/commit, chunk store
│   ├── cli.rs           # CLI (init, mount, fsck, rebuild-index, show-ref) + repo-level ops
│   ├── dag.rs           # CIDv1, dag-pb, dag-cbor, v1-2025 UnixFS DAG builder
│   └── cdc.rs           # content-defined chunking (FastCDC v2020 + SHA-256)
└── docs/
    └── adr/             # Architectural Decision Records (0001–0005)
```

## Key architectural decisions

See `docs/adr/` for the full record.
Summary:

1. **SQLite is the canonical store** — the `.db` file IS the repository.
   `directory_index` and `block_index` are materialized views rebuildable from the chunk store.
2. **Dual chunking** — FastCDC for physical dedup, UnixFS v1-2025 for public IPFS CIDs.
   Mapped via `block_index`.
3. **IPFS-native DAG** — dag-pb directories, v1-2025 file layout, dag-cbor commits.
   Content-addressed with CIDv1 + SHA-256.
4. **Live writable FUSE** — writes in memory, hashed on `close()`, committed periodically and manually.
   Hidden `.sqlitefs/` directory for snapshot browsing.

## Tooling

- **Build**: `nix build .#sqlitefs` (also `nix flake check`, `nix develop`).
  Dev-shell tools come from `devShells.default`; the VM test runs via `nix flake check`.
- **rustfmt.toml** (from rkyv, `max_width = 100`) — the unstable options (`imports_granularity = "Crate"`, `group_imports = "StdExternalCrate"`) need `rustfmt --edition 2024 --config 'imports_granularity=Crate,group_imports=StdExternalCrate'`.
- **Clippy**: pedantic by default via `[lints.clippy]` in Cargo.toml.
  Intentional `as` casts (varint/base32 encoding, FUSE offsets) are allowed there, so `cargo clippy` runs clean.

- **Language**: Rust.
- **Key crates (current)**: `clap` (CLI), `rusqlite` (SQLite), `fuser` (FUSE), `libc` (errnos), `sha2` (SHA-256), `fastcdc` (CDC chunking).
- **Key crates (planned)**: none pending.
- **Chunking**: FastCDC (pluggable — may evaluate rapidCDC later).
- **UnixFS v1-2025**: custom DAG builder (raw leaves, balanced dag-pb tree, per-block offsets).
  No mature Rust impl — validate against ipfs-chainer reference.

## FUSE operations implemented

|Operation                                   |Status                                     |
|--------------------------------------------|-------------------------------------------|
|`lookup`, `getattr`, `readdir`              |✓                                          |
|`mkdir`, `rmdir`                            |✓ (auto-commit)                            |
|`create`, `open`, `write`, `release`, `read`|✓ (CDC + commit on release)                |
|`unlink`                                    |✓ (auto-commit)                            |
|`rename`                                    |✓ (cross-directory, overwrite, auto-commit)|
|`symlink`, `readlink`                       |✓                                          |
|`link` (hard links for files)               |✓ (files only; new parent must be writable)|
|`forget`                                    |✓                                          |
|`getxattr`, `listxattr`                     |Stub (no xattrs)                           |
|`setattr` (truncate-to-zero, chmod)         |✓ (auto-commit; uid/gid/utimes ignored)    |
|`flush`, `fsync`                            |Not needed (no-op ok)                      |

## Snapshot model

- **Auto-commit**: every 60s if dirty, checked on `lookup`/`create`/`open`.
- **File modes**: mode is embedded in the file manifest (dag-cbor); chmod re-stores the manifest (new CID).
  `directory_index.file_mode` is a denormalized cache; `sqlitefs rebuild-index` recreates it from refs (run once when upgrading old repos).
- **Manual named snapshot**: `touch .sqlitefs/tag/<name>` creates a named ref.
- **Hard links**: `link` creates a second directory entry pointing at the same inode (same manifest CID → dedup).
  Files only; nlink always reported as 1 — see docs/adr/0005-hard-links.md.
- **Snapshot browsing**: `.sqlitefs/refs/<name>/` (hidden from readdir).
- **Hash access** (`.sqlitefs/c/<hash-prefix>/`): ✓ (prefix lookup in commits table, cached).

## Rules for agents

When working on this repo, agents MUST:

1. **Keep AGENTS.md and CONTEXT.md up to date** — before every commit, verify no updates are needed.
2. **Use Rust idioms from the user's conventions** — see the `rust-idioms` skill.
3. **Do not add dependencies lightly** — run through the ponytail filter first.
   Prefer stdlib, minimal crates.
4. **Treat `cargo fmt` as mandatory** before every commit.
5. **Build with `nix develop -c cargo build`** — `cargo fmt && cargo clippy` too.
6. **Git commits without GPG signing** — use `--no-gpg-sign`.
7. **Keep the Remaining work list up to date** — add new outstanding work as numbered entries.
   Remove finished entries entirely (never cross out, never mark done).
