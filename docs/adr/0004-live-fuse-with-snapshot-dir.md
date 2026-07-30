# Live writable FUSE mount with hidden snapshot directory

The repository is mounted as a writable FUSE filesystem. Writes go to an in-memory dirty state and are committed on `close()`-triggered hashing. Commits are created by explicit user action (manual snapshot) and periodic auto-commits (configurable interval, sensible default). A hidden `.sqlitefs/` directory — invisible in `readdir` but accessible by path — provides read-only access to past commits: `.sqlitefs/refs/<name>/` for named refs and `.sqlitefs/c/<hash-prefix>/` for direct commit access.

**Why**: This makes sqlitefs a live versioned filesystem, not a backup tool. Users interact with it through standard filesystem operations (mkdir, cp, rm). Snapshots are an ambient property — the system commits automatically, and the user can always browse history. This is the ZFS model applied to a single-file repository.

**Trade-off**: Dirty state is in-memory only — a crash loses uncommitted writes. Filesystem operations are slower than native due to FUSE round-trips and chunking on `close()`. The `.sqlitefs/` directory is inaccessible to tools that only do `readdir` traversal (they won't discover it), which is intentional for a clean namespace but may surprise users.

**Consequences**:
- The write path is: FUSE write → kernel page cache → on close → CDC chunking → chunk store insert → dirty tree update → periodic/manual commit → DAG reconstruction → SQLite commit
- Snapshots can be manually tagged (via a touch file or CLI) and are auto-named by timestamp otherwise
- `.sqlitefs/` is a virtual FUSE directory; it does not correspond to any DAG object
