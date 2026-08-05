// fs.rs — the stateful core: in-memory entry tree, DAG build/commit, chunk store access.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use crate::dag;

// ---------------------------------------------------------------------------
// Inode allocator
// ---------------------------------------------------------------------------

pub(crate) const ROOT_INODE: u64 = 1;
pub(crate) const SNAPSHOT_DIR_INODE: u64 = 2;
pub(crate) const SNAPSHOT_REFS_INODE: u64 = 3;
pub(crate) const SNAPSHOT_TAG_INODE: u64 = 4;
pub(crate) const SNAPSHOT_HASH_INODE: u64 = 5;

/// 60-second auto-commit interval.
pub(crate) const AUTO_COMMIT_SECS: u64 = 60;

#[derive(Default)]
pub(crate) struct InoGen {
    next: u64,
}

impl InoGen {
    fn new(start: u64) -> Self {
        Self { next: start }
    }
    pub(crate) fn next(&mut self) -> u64 {
        let n = self.next;
        self.next += 1;
        n
    }
}

pub(crate) enum Entry {
    Dir {
        children: HashMap<String, u64>,
        parent: u64,
    },
    File {
        size: u64,
        /// Permission bits (mode & 0o7777, e.g. 0o755 for executables).
        mode: u32,
        /// Manifest CID (content-addressed list of chunk hashes).
        manifest_cid: Option<Vec<u8>>,
        /// v1-2025 root CID (IPFS-compatible).
        v1_2025_cid: Option<Vec<u8>>,
    },
    Symlink {
        target: String,
    },
}

#[derive(Default)]
pub(crate) struct DirtyFile {
    pub(crate) data: Vec<u8>,
}

/// Write an empty file manifest (dag-cbor `[]`) into `db` and return its CID bytes.
/// Works on a plain connection or a transaction (Transaction derefs to Connection).
pub(crate) fn store_empty_manifest(db: &rusqlite::Connection) -> Vec<u8> {
    let (manifest_data, mc) = dag::encode_file_manifest(&[], 0, 0o644);
    let _ = db.execute(
        "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
        rusqlite::params![mc.to_bytes(), manifest_data],
    );
    mc.to_bytes()
}

/// Write a file manifest + its chunks into `db` (a connection or transaction).
pub(crate) fn store_file_manifest(
    db: &rusqlite::Connection,
    chunk_hashes: &[[u8; 32]],
    size: u64,
    mode: u32,
) -> dag::Cid {
    let (manifest_data, mc) = dag::encode_file_manifest(chunk_hashes, size, mode);
    let _ = db.execute(
        "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
        rusqlite::params![mc.to_bytes(), manifest_data],
    );
    mc
}

pub(crate) struct SqliteFs {
    pub(crate) db: rusqlite::Connection,
    pub(crate) entries: HashMap<u64, Entry>,
    pub(crate) ino_gen: InoGen,
    pub(crate) open_files: HashMap<(u64, u64), DirtyFile>,
    pub(crate) next_fh: u64,
    pub(crate) dirty_root: bool,
    pub(crate) last_auto_commit: std::time::Instant,
}

impl SqliteFs {
    pub(crate) fn open(db_path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let db = rusqlite::Connection::open(db_path)?;
        // WAL + relaxed sync: faster writes, better crash consistency for an
        // embedded DB behind a FUSE writer. journal_mode is persistent; setting it
        // here also upgrades pre-existing repos. Idempotent.
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        let mut ino_gen = InoGen::new(SNAPSHOT_HASH_INODE + 1);
        let mut entries = HashMap::new();

        // Wire up root directory (writable) + .sqlitefs/ (read-only snapshot tree).
        // Load the persisted 'main' ref tree into the writable root so that
        // remounting restores the working tree (not just the snapshot dir).
        let mut root_children = HashMap::from([(".sqlitefs".into(), SNAPSHOT_DIR_INODE)]);
        if let Ok(Some(main_commit)) = db.query_row(
            "SELECT commit_hash FROM refs WHERE name = 'main'",
            [],
            |r| r.get::<_, Option<Vec<u8>>>(0),
        ) && let Ok(main_tree) = db.query_row(
            "SELECT tree_cid FROM commits WHERE hash = ?1",
            rusqlite::params![main_commit],
            |r| r.get::<_, Vec<u8>>(0),
        ) {
            // Load persisted dirs/files into the root, excluding the snapshot dir.
            let child_rows: Vec<(String, Vec<u8>)> = db
                .prepare("SELECT name, child_cid FROM directory_index WHERE parent_cid = ?1")
                .ok()
                .map(|mut cs| {
                    cs.query_map(rusqlite::params![main_tree], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                    })
                    .ok()
                    .map(|rows| rows.filter_map(Result::ok).collect())
                    .unwrap_or_default()
                })
                .unwrap_or_default();
            for (child_name, child_cid) in child_rows {
                if child_name == ".sqlitefs" {
                    continue;
                }
                let child_ino = match Self::child_entry_from_cid(&db, &child_cid) {
                    Entry::Dir { .. } => Self::load_dir_recursive(
                        &db,
                        &mut ino_gen,
                        &mut entries,
                        &child_cid,
                        ROOT_INODE,
                    ),
                    entry => {
                        let child_ino = ino_gen.next();
                        entries.insert(child_ino, entry);
                        child_ino
                    }
                };
                root_children.insert(child_name, child_ino);
            }
        }

        entries.insert(
            ROOT_INODE,
            Entry::Dir {
                children: root_children,
                parent: ROOT_INODE,
            },
        );
        entries.insert(
            SNAPSHOT_DIR_INODE,
            Entry::Dir {
                children: HashMap::from([
                    ("refs".into(), SNAPSHOT_REFS_INODE),
                    ("tag".into(), SNAPSHOT_TAG_INODE),
                    ("c".into(), SNAPSHOT_HASH_INODE),
                ]),
                parent: ROOT_INODE,
            },
        );
        entries.insert(
            SNAPSHOT_TAG_INODE,
            Entry::Dir {
                children: HashMap::new(),
                parent: SNAPSHOT_DIR_INODE,
            },
        );
        entries.insert(
            SNAPSHOT_HASH_INODE,
            Entry::Dir {
                children: HashMap::new(),
                parent: SNAPSHOT_DIR_INODE,
            },
        );

        // Build snapshot tree from DB.
        let refs_children = Self::load_snapshot_tree(&db, &mut ino_gen, &mut entries)?;
        entries.insert(
            SNAPSHOT_REFS_INODE,
            Entry::Dir {
                children: refs_children,
                parent: SNAPSHOT_DIR_INODE,
            },
        );

        Ok(Self {
            db,
            entries,
            ino_gen,
            open_files: HashMap::new(),
            next_fh: 0,
            dirty_root: false,
            last_auto_commit: std::time::Instant::now(),
        })
    }

    /// Walk DB `refs/commits/directory_index` and build the snapshot tree.
    pub(crate) fn load_snapshot_tree(
        db: &rusqlite::Connection,
        ino_gen: &mut InoGen,
        entries: &mut HashMap<u64, Entry>,
    ) -> Result<HashMap<String, u64>, Box<dyn std::error::Error>> {
        let mut refs_children = HashMap::new();
        let mut stmt = db.prepare("SELECT name, commit_hash FROM refs")?;
        let rows: Vec<(String, Vec<u8>)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .filter_map(Result::ok)
            .collect();
        for (name, commit_hash) in rows {
            let tree_cid: Vec<u8> = db.query_row(
                "SELECT tree_cid FROM commits WHERE hash = ?1",
                rusqlite::params![commit_hash],
                |r| r.get(0),
            )?;
            let ref_ino =
                Self::load_dir_recursive(db, ino_gen, entries, &tree_cid, SNAPSHOT_REFS_INODE);
            refs_children.insert(name, ref_ino);
        }
        Ok(refs_children)
    }

    /// Recursively load a directory from `directory_index`, building Entry trees.
    fn load_dir_recursive(
        db: &rusqlite::Connection,
        ino_gen: &mut InoGen,
        entries: &mut HashMap<u64, Entry>,
        parent_cid: &[u8],
        parent_inode: u64,
    ) -> u64 {
        let ino = ino_gen.next();
        let mut children = HashMap::new();

        let mut cs = db
            .prepare("SELECT name, child_cid FROM directory_index WHERE parent_cid = ?1")
            .ok();
        if let Some(ref mut cs) = cs {
            let child_rows: Vec<(String, Vec<u8>)> = cs
                .query_map(rusqlite::params![parent_cid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .ok()
                .map(|rows| rows.filter_map(Result::ok).collect())
                .unwrap_or_default();

            for (child_name, child_cid) in child_rows {
                let child_ino = match Self::child_entry_from_cid(db, &child_cid) {
                    Entry::Dir { .. } => {
                        // Recurse into subdirectories.
                        Self::load_dir_recursive(db, ino_gen, entries, &child_cid, ino)
                    }
                    entry => {
                        let child_ino = ino_gen.next();
                        entries.insert(child_ino, entry);
                        child_ino
                    }
                };
                children.insert(child_name, child_ino);
            }
        }

        entries.insert(
            ino,
            Entry::Dir {
                children,
                parent: parent_inode,
            },
        );
        ino
    }

    /// Load a child CID from chunks — returns a File, Dir, or Symlink entry.
    fn child_entry_from_cid(db: &rusqlite::Connection, cid: &[u8]) -> Entry {
        let data: Vec<u8> = db
            .query_row(
                "SELECT data FROM chunks WHERE hash = ?1",
                rusqlite::params![cid],
                |r| r.get(0),
            )
            .unwrap_or_default();
        if let Some(target) = dag::decode_symlink(&data) {
            return Entry::Symlink { target };
        }
        if dag::is_dir_dag(&data) {
            return Entry::Dir {
                children: HashMap::new(),
                parent: 0, // filled in by load_dir_recursive
            };
        }
        let size = if dag::is_file_manifest(&data) {
            let mut total: u64 = 0;
            if let Some((embedded_size, hashes, _)) = dag::decode_file_manifest(&data) {
                match embedded_size {
                    Some(sz) => total = sz,
                    // Old format (no embedded size): sum chunk lengths.
                    None => {
                        for h in &hashes {
                            if let Ok(chunk_data) = db.query_row(
                                "SELECT data FROM chunks WHERE hash = ?1",
                                rusqlite::params![h.to_vec()],
                                |r| r.get::<_, Vec<u8>>(0),
                            ) {
                                total += chunk_data.len() as u64;
                            }
                        }
                    }
                }
            }
            total
        } else {
            0
        };
        Entry::File {
            size,
            mode: dag::decode_file_manifest(&data)
                .and_then(|(_, _, m)| m)
                .unwrap_or(0o644),
            manifest_cid: Some(cid.to_vec()),
            v1_2025_cid: None,
        }
    }

    /// Resolve a hex hash prefix in .sqlitefs/c/ into a Dir with the commit's tree.
    pub(crate) fn resolve_hash_prefix(
        db: &rusqlite::Connection,
        ino_gen: &mut InoGen,
        entries: &mut HashMap<u64, Entry>,
        hex_prefix: &str,
        parent: u64,
    ) -> Option<u64> {
        // Match prefix against commit hashes. Use LIKE with the hex-encoded hash.
        let mut stmt = db
            .prepare(
                "SELECT hash, tree_cid, timestamp FROM commits WHERE lower(hex(hash)) LIKE ?1 ORDER BY timestamp DESC LIMIT 2",
            )
            .ok()?;
        let rows: Vec<(Vec<u8>, Vec<u8>, i64)> = stmt
            .query_map(rusqlite::params![format!("{hex_prefix}%")], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .ok()?
            .filter_map(Result::ok)
            .collect();
        if rows.len() != 1 {
            return None; // ambiguous or not found
        }
        let (_commit_hash, tree_cid, _timestamp) = &rows[0];

        let dir_ino = Self::load_dir_recursive(db, ino_gen, entries, tree_cid, parent);
        Some(dir_ino)
    }

    /// Auto-commit if dirty and interval elapsed. Called from every FUSE op.
    /// Record that the tree changed and reset the debounce timer.
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty_root = true;
        self.last_auto_commit = std::time::Instant::now();
    }

    /// Debounced auto-commit: commit if dirty and no mutation for 60s.
    /// Called from lookup/create/open — a long-running bulk copy keeps bumping
    /// the timer via mark_dirty, so we commit once after it settles, not per op.
    pub(crate) fn maybe_auto_commit(&mut self) {
        if !self.dirty_root {
            return;
        }
        if self.last_auto_commit.elapsed().as_secs() >= AUTO_COMMIT_SECS {
            let _ = self.commit_dirty_tree();
            self.last_auto_commit = std::time::Instant::now();
        }
    }

    /// Commit the dirty tree to the 'main' ref.
    pub(crate) fn commit_dirty_tree(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.commit_as_ref("main")
    }

    /// Create a named commit (ref) from the current dirty tree.
    pub(crate) fn commit_as_ref(
        &mut self,
        ref_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Everything from the DAG build to the ref update happens in ONE transaction:
        // a crash mid-commit can no longer leave orphan chunks/commits or a ref
        // pointing at a missing commit. Single-threaded FUSE ops → unchecked is fine.
        let tx = self.db.unchecked_transaction()?;

        // Build recursive DAG from the entry tree rooted at ROOT_INODE.
        let (tree_data, tree_cid) = self.build_dir_dag(&tx, ROOT_INODE)?;

        // Write the tree dag-pb node into chunks.
        let _ = tx.execute(
            "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
            rusqlite::params![tree_cid.to_bytes(), tree_data],
        );

        // Get current commit for this ref (as parent).
        let old_commit: Option<Vec<u8>> = tx
            .query_row(
                "SELECT commit_hash FROM refs WHERE name = ?1",
                rusqlite::params![ref_name],
                |r| r.get(0),
            )
            .ok();
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (commit_data, commit_cid) =
            dag::encode_commit(old_commit.as_deref(), &tree_cid.to_bytes(), now);
        let _ = tx.execute(
            "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
            rusqlite::params![commit_cid.to_bytes(), commit_data],
        );
        let _ = tx.execute(
            "INSERT INTO commits (hash, parent, tree_cid, timestamp) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![commit_cid.to_bytes(), old_commit, tree_cid.to_bytes(), now],
        );
        let _ = tx.execute(
            "INSERT OR REPLACE INTO refs (name, commit_hash) VALUES (?1, ?2)",
            rusqlite::params![ref_name, commit_cid.to_bytes()],
        );

        tx.commit()?;

        // Rebuild snapshot tree so .sqlitefs/refs/main/ shows the new commit.
        let refs_children =
            Self::load_snapshot_tree(&self.db, &mut self.ino_gen, &mut self.entries)?;
        if let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&SNAPSHOT_REFS_INODE) {
            *children = refs_children;
        }
        self.dirty_root = false;
        Ok(())
    }

    /// Create and store an empty file manifest (dag-cbor `[]`), returning its bytes.
    /// Empty files are real files — they need a manifest CID so `build_dir_dag` links
    /// them instead of skipping them (otherwise `touch foo` loses `foo` on commit).
    pub(crate) fn store_empty_manifest(&mut self) -> Vec<u8> {
        store_empty_manifest(&self.db)
    }

    /// Store a released file's chunks + manifest + v1-2025 blocks in ONE transaction,
    /// returning the manifest CID. A crash mid-write rolls back, so no partial file.
    /// `mode` is persisted into the manifest so a chmod survives commit/remount.
    pub(crate) fn store_released_file(
        &self,
        data: &[u8],
        chunks: &[([u8; 32], Vec<u8>)],
        mode: u32,
    ) -> Result<dag::Cid, Box<dyn std::error::Error>> {
        let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|(h, _)| *h).collect();

        // Build cumulative offsets for CDC chunk span tracking.
        let mut cdc_spans: Vec<([u8; 32], usize, usize)> = Vec::new();
        let mut cdc_off: usize = 0;
        for (hash, chunk_data) in chunks {
            let end = cdc_off + chunk_data.len();
            cdc_spans.push((*hash, cdc_off, end));
            cdc_off = end;
        }

        let tx = self.db.unchecked_transaction()?;

        for (hash, chunk_data) in chunks {
            let _ = tx.execute(
                "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                rusqlite::params![hash.to_vec(), chunk_data],
            );
        }
        // Create the file manifest (dag-cbor array of chunk hashes + size + mode).
        let mc = store_file_manifest(&tx, &chunk_hashes, data.len() as u64, mode);

        // Build v1-2025 DAG (IPFS-compatible) from the assembled file.
        let v1_dag = dag::build_v1_2025_dag(data);

        for block in &v1_dag.blocks {
            let _ = tx.execute(
                "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                rusqlite::params![block.cid.to_bytes(), block.data],
            );
            let _ = tx.execute(
                "INSERT OR IGNORE INTO block_index (v1_2025_cid, file_cid, block_offset, block_length) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![block.cid.to_bytes(), mc.to_bytes(), block.file_offset as i64, block.data.len() as i64],
            );

            // Map backing CDC chunks only for LEAF blocks (RAW codec). Intermediate
            // dag-pb nodes cover the union of their children's spans, so mapping
            // them would duplicate every leaf's rows (O(N^2/128) total); leaf rows
            // are sufficient to assemble any block's content on demand.
            if block.cid.codec() != dag::RAW {
                continue;
            }
            let block_start = block.file_offset as usize;
            let block_end = block_start + block.data.len();
            for &(ref cdc_hash, cdc_start, cdc_end) in &cdc_spans {
                if cdc_end > block_start && cdc_start < block_end {
                    let _ = tx.execute(
                        "INSERT OR IGNORE INTO block_chunks (v1_2025_cid, chunk_hash) VALUES (?1, ?2)",
                        rusqlite::params![block.cid.to_bytes(), cdc_hash.to_vec()],
                    );
                }
            }
        }

        tx.commit()?;
        Ok(mc)
    }

    /// Recursively build a dag-pb directory node for `inode` and its children.
    /// Returns (`encoded_dag_pb`, Cid). Also inserts file manifests + chunks into DB
    /// and populates `directory_index`.
    fn build_dir_dag(
        &self,
        tx: &rusqlite::Transaction,
        inode: u64,
    ) -> Result<(Vec<u8>, dag::Cid), Box<dyn std::error::Error>> {
        let children_names: Vec<(String, u64)> = match self.entries.get(&inode) {
            Some(Entry::Dir { children, .. }) => {
                children.iter().map(|(n, &i)| (n.clone(), i)).collect()
            }
            _ => return Ok(dag::empty_dir()),
        };

        let mut links: Vec<(String, Vec<u8>)> = Vec::new();
        for (name, child_ino) in &children_names {
            if *name == ".sqlitefs" {
                // ponytail: skip snapshot dir — not committed into the DAG
                continue;
            }
            let child_cid = match self.entries.get(child_ino) {
                Some(Entry::Dir { .. }) => {
                    let (child_dag, child_cid) = self.build_dir_dag(tx, *child_ino)?;
                    let _ = tx.execute(
                        "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                        rusqlite::params![child_cid.to_bytes(), child_dag],
                    );
                    child_cid.to_bytes()
                }
                Some(Entry::File {
                    manifest_cid: Some(cid),
                    ..
                }) => cid.clone(),
                Some(Entry::File {
                    manifest_cid: None, ..
                }) => {
                    // Empty file (created but never written, or truncated): link a
                    // real empty manifest so the file is persisted, not skipped.
                    store_empty_manifest(tx)
                }
                Some(Entry::Symlink { target }) => {
                    let (sym_dag, sym_cid) = dag::encode_symlink(target);
                    let _ = tx.execute(
                        "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                        rusqlite::params![sym_cid.to_bytes(), sym_dag],
                    );
                    sym_cid.to_bytes()
                }
                None => continue,
            };
            links.push((name.clone(), child_cid));
        }

        // Sort for deterministic DAG construction.
        links.sort_by(|a, b| a.0.cmp(&b.0));

        let link_refs: Vec<(&str, &[u8])> = links
            .iter()
            .map(|(n, c)| (n.as_str(), c.as_slice()))
            .collect();
        let (dir_data, dir_cid) = dag::dir_with_links(&link_refs);

        // Populate directory_index for this node. file_mode is the mode of a
        // direct file child (NULL for dirs/symlinks). It's derived from the
        // manifest chunk, so rebuilding the index from refs restores it.
        for (name, child_cid) in &links {
            let _ = tx.execute(
                "INSERT OR REPLACE INTO directory_index (parent_cid, name, child_cid, file_mode) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![dir_cid.to_bytes(), name, child_cid, None::<i64>],
            );
        }

        Ok((dir_data, dir_cid))
    }
}
