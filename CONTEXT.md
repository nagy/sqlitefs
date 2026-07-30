# sqlitefs

A versioned, content-addressed filesystem where a single SQLite file is both the repository and the mountable volume. The repository stores filesystem trees as IPFS-compatible DAG objects, supports writable FUSE mounts with periodic snapshots, and deduplicates file data using content-defined chunking.

## Language

**Repository**:
A single `.db` file containing the entire versioned filesystem — chunk store, DAG metadata, commit history, and refs.
_Avoid_: Database, archive, image

**Ref**:
A named mutable pointer to a commit CID (e.g., `main`, `melpa-snapshot`). Stored in the `refs` table. Commits are immutable; refs are the only mutable state.
_Avoid_: Branch, tag, head

**Commit**:
An immutable, content-addressed DAG object containing `{parent, tree, timestamp}`. Stored as dag-cbor. Chainable via `parent` field to form history.
_Avoid_: Snapshot, version, revision

**Tree**:
The root directory of a commit — a UnixFS dag-pb directory node. All filesystem structure radiates from here.
_Avoid_: Root, filesystem root

**Physical chunk**:
A variable-size byte segment produced by FastCDC (or another content-defined chunking algorithm). Stored once in the chunk store, addressed by its SHA-256 hash. The deduplication unit.
_Avoid_: Raw block, data segment, slice

**Virtual block**:
A 1 MiB fixed-size byte segment as defined by the IPFS UnixFS v1-2025 profile. Assembled on demand from physical chunks. The CID of a virtual block is publicly addressable on IPFS.
_Avoid_: IPFS block, leaf block

**Block index**:
SQLite table mapping virtual block CIDs to backing physical chunks. Rebuildable from the chunk store and file metadata.
_Avoid_: CID mapping table

**Chunk store**:
The set of all physical chunks stored in SQLite, indexed by SHA-256 hash. The foundation of truth — if all other tables are lost, the full filesystem tree can be reconstructed by walking the DAG from commit root CIDs.
_Avoid_: Blob store, CAS

**Directory index**:
SQLite table `directory_index` mapping `(parent_cid, name, child_cid)`. A materialized view of the DAG directory structure for fast path lookups. Rebuildable.
_Avoid_: Inode table, path cache

**Dirty state**:
In-memory filesystem modifications between commits. Accumulated as FUSE writes, hashed on `close()`. Lost on crash. Only committed state is persistent.
_Avoid_: Working tree, staging area, uncommitted changes
