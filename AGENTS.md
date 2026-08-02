# sqlitefs

A versioned, content-addressed filesystem where a single SQLite file is both the repository and the mountable volume. Built in Rust.

## Project structure

```
.
├── CONTEXT.md           # Ubiquitous language / glossary
├── AGENTS.md            # This file (includes the remaining-work list at the end)
├── Cargo.toml
├── Cargo.lock
├── shell.nix            # Nix dev shell
├── default.nix          # Nix build
├── .gitignore
├── LICENSE              # AGPL-3.0
├── src/
│   ├── main.rs          # CLI (init + mount), writable FUSE + .sqlitefs/ snapshots + tag + rename + symlinks
│   ├── dag.rs           # CIDv1, dag-pb, dag-cbor, v1-2025 UnixFS DAG builder
│   └── cdc.rs           # Content-defined chunking via FastCDC v2020 + SHA-256
└── docs/
    └── adr/             # Architectural Decision Records
        ├── 0001-sqlite-canonical-store.md
        ├── 0002-dual-chunking-cdc-plus-unixfs.md
        ├── 0003-ipfs-native-dag.md
        └── 0004-live-fuse-with-snapshot-dir.md
```

## Key architectural decisions

See `docs/adr/` for the full record. Summary:

1. **SQLite is the canonical store** — the `.db` file IS the repository. Tables like `directory_index` and `block_index` are materialized views rebuildable from the chunk store.
2. **Dual chunking** — FastCDC for physical storage dedup, UnixFS v1-2025 for public IPFS CIDs. Mapped via `block_index`.
3. **IPFS-native DAG** — dag-pb directories, v1-2025 file layout, dag-cbor commits. Content-addressed with CIDv1 + SHA-256.
4. **Live writable FUSE** — writes in memory, hashed on `close()`, committed periodically and manually. Hidden `.sqlitefs/` directory for snapshot browsing.

## Tooling

- **Build**: `nix-shell --run "cargo build"` (also `cargo fmt`, `cargo clippy`)
- **Language**: Rust
- **Key crates (current)**: `clap` (CLI), `rusqlite` (SQLite), `fuser` (FUSE), `libc` (errnos), `sha2` (SHA-256), `fastcdc` (CDC chunking)
- **Key crates (planned)**: none pending
- **Chunking**: FastCDC (pluggable — may evaluate alternatives like rapidCDC later)
- **UnixFS v1-2025**: custom DAG builder (raw leaves, balanced dag-pb tree, per-block offsets). No existing mature Rust impl — validate against ipfs-chainer reference

## FUSE operations implemented

|Operation                                   |Status                                     |
|--------------------------------------------|-------------------------------------------|
|`lookup`, `getattr`, `readdir`              |✓                                          |
|`mkdir`, `rmdir`                            |✓ (auto-commit on completion)              |
|`create`, `open`, `write`, `release`, `read`|✓ (CDC + commit on release)                |
|`unlink`                                    |✓ (with auto-commit)                       |
|`rename`                                    |✓ (cross-directory, overwrite, auto-commit)|
|`symlink`, `readlink`                       |✓                                          |
|`forget`                                    |✓                                          |
|`getxattr`, `listxattr`                     |Stub (no xattrs)                           |
|`setattr` (truncate-to-zero)                |✓ (auto-commit; mode/uid/utimes ignored)   |
|`flush`, `fsync`                            |Not needed (no-op ok)                      |

## Snapshot model

- **Auto-commit**: every 60s if dirty, checked on `lookup`/`create`/`open`
- **Manual named snapshot**: `touch .sqlitefs/tag/<name>` creates a named ref
- **Snapshot browsing**: `.sqlitefs/refs/<name>/` (hidden from readdir)
- **Hash access** (`.sqlitefs/c/<hash-prefix>/`): ✓ (prefix lookup in commits table, cached)

## Remaining work

The numbered list below is the single source of truth for what remains. When an entry is finished, **remove it entirely** — never cross out, never mark done.

### 1. Performance

1. **Every mutation is O(entire repo)** — `mkdir`/`rmdir`/`unlink`/`rename`/`symlink`/`truncate` set `dirty_root = true` and immediately `commit_dirty_tree()`, which walks the full tree and rebuilds the entire snapshot tree from every ref. `cp -r` of many dirs is quadratic. Decide on one model: commit on a debounce/`fsync`, or lazily rebuild the snapshot tree on lookup.
2. **`block_chunks` bloat for intermediates** — maps every v1-2025 block, including balanced-tree intermediate nodes, to all their backing chunks. Each intermediate's span is a union of leaf spans — redundant, ~8× the rows it needs (O(N log N) instead of O(N)). Only leaves are ever assembled; map just those.

### 2. Durability & consistency

3. **WAL mode** — `Connection::open` defaults to the rollback journal. Use `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL` for an embedded DB behind a FUSE writer.
4. **No double-mount protection** — two `mount` processes on the same `.db` will corrupt it. `flock` on the db file (or a lock table row) in `SqliteFs::open`.

### 3. Process & testing

5. **No `fsck` command** — walk refs → verify DAG reachability, rebuild indexes. First real validation tool; also de-risks GC.
6. **Run the NixOS VM test with a debug build** — `default.nix`'s `buildRustPackage` defaults `cargoBuildType` to `release`, so `debug_assert!`s (in our code or in fuser's) are compiled out in the test. Switch the test to a debug build (`cargoBuildType = "debug"` or a separate test derivation) so future `debug_assert!`s are actually exercised. Note: fuser's existing asserts on WRITE/SETXATTR sizes and readdir buffers are unconditional `assert!`s (fire in release too); the read-reply path has no assert — consider adding a `debug_assert` for reply length ≤ requested size.

## Rules for agents

When working on this repo, agents MUST:

1. **Keep AGENTS.md and CONTEXT.md up to date** — before every commit, verify no updates are needed.
2. **Use Rust idioms from the user's conventions** — see the `rust-idioms` skill.
3. **Do not add dependencies lightly** — run through the ponytail filter first. Prefer stdlib, minimal crates.
4. **Treat `cargo fmt` as mandatory** before every commit.
5. **Build with `nix-shell --run "cargo build"`** — `cargo fmt && cargo clippy` too.
6. **Git commits without GPG signing** — use `--no-gpg-sign`.
7. **Keep the Remaining work list up to date** (at the end of this file) — add new outstanding work as numbered entries; when an entry is finished, remove it entirely (never cross out, never mark done).
