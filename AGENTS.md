# sqlitefs

A versioned, content-addressed filesystem where a single SQLite file is both the repository and the mountable volume. Built in Rust.

## Project structure

```
.
├── CONTEXT.md           # Ubiquitous language / glossary
├── AGENTS.md            # This file
├── Cargo.toml
├── Cargo.lock
├── shell.nix            # Nix dev shell
├── default.nix          # Nix build
├── .gitignore
├── src/
│   └── main.rs          # CLI (init + mount subcommands), FUSE filesystem (read-only)
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
3. **IPFS-native DAG** — dag-pb directories, v1-2025 file layout, dag-cbor commits. Content-addressed with CIDv1 + BLAKE3.
4. **Live writable FUSE** — writes in memory, hashed on `close()`, committed periodically and manually. Hidden `.sqlitefs/` directory for snapshot browsing.

## Tooling

- **Language**: Rust
- **Key crates (current)**: `clap` (CLI), `rusqlite` (SQLite), `fuser` (FUSE), `libc` (errnos)
- **Key crates (planned)**: `cid`, `multihash`, `blake3`, `libipld`/`ipld-dagpb`, `serde_ipld_dagcbor`
- **Chunking**: FastCDC (pluggable — may evaluate alternatives like rapidCDC later)
- **UnixFS v1-2025**: custom DAG builder (no existing mature Rust impl — implement against spec, validate against ipfs-chainer reference)

## Rules for agents

When working on this repo, agents MUST:

1. **Keep AGENTS.md and CONTEXT.md up to date** — before every commit, verify no updates are needed.
2. **Use Rust idioms from the user's conventions** — see the `rust-idioms` skill.
3. **Do not add dependencies lightly** — run through the ponytail filter first. Prefer stdlib, minimal crates.
4. **Treat `cargo fmt` as mandatory** before every commit.
5. **Git commits without GPG signing** — use `--no-gpg-sign`.
