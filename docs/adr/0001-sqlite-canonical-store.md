# SQLite is the canonical store, not a cache

We decided that the SQLite file **is** the repository. All DAG objects (chunks, commits, directory metadata, refs) live inside it. Tables like `directory_index` and `block_index` are materialized views rebuildable from the chunk store, but the chunk store itself lives in SQLite — not in external pack files.

**Why**: A single-file repository is easier to manage (copy, move, backup) than a directory of pack files. The `.db` IS the repository. The rebuildable-index pattern gives us the best of both: simple sync of the canonical data, plus fast FUSE lookups via SQLite indexes.

**Trade-off**: Syncing the SQLite file efficiently to remotes (Git hosting, cloud storage) requires either SQLite-level replication (sqlite-rsync, LiteFS) or exporting/pushing derived representations. We accept this as a later concern.

**Alternative considered**: The chat's final recommendation — pack files + commits on disk as files, SQLite as a disposable cache. Rejected because the user wanted a single self-contained file, and because the rebuildable-index pattern achieves equivalent resilience without external file management.
