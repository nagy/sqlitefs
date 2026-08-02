mod cdc;
mod dag;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use clap::{Parser, Subcommand};
use fuser::{
    FileAttr, FileType, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, ReplyXattr, Request,
};
use libc::{ENOENT, ENOTDIR};

// ---------------------------------------------------------------------------
// Inode allocator
// ---------------------------------------------------------------------------

const ROOT_INODE: u64 = 1;
const SNAPSHOT_DIR_INODE: u64 = 2;
const SNAPSHOT_REFS_INODE: u64 = 3;
const SNAPSHOT_TAG_INODE: u64 = 4;
const SNAPSHOT_HASH_INODE: u64 = 5;

/// 60-second auto-commit interval.
const AUTO_COMMIT_SECS: u64 = 60;

#[derive(Default)]
struct InoGen {
    next: u64,
}

impl InoGen {
    fn new(start: u64) -> Self {
        Self { next: start }
    }
    fn next(&mut self) -> u64 {
        let n = self.next;
        self.next += 1;
        n
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "sqlitefs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new empty repository.
    Init { path: PathBuf },
    /// Mount a repository via FUSE.
    Mount {
        db_path: PathBuf,
        mount_point: PathBuf,
    },
}

fn init_repo(path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open(path)?;

    db.execute_batch(
        "
        CREATE TABLE chunks (
            hash BLOB PRIMARY KEY,
            data BLOB NOT NULL
        ) STRICT;

        CREATE TABLE refs (
            name TEXT PRIMARY KEY,
            commit_hash BLOB NOT NULL
        ) STRICT;

        CREATE TABLE directory_index (
            parent_cid BLOB NOT NULL,
            name        TEXT NOT NULL,
            child_cid   BLOB NOT NULL,
            PRIMARY KEY (parent_cid, name)
        ) STRICT;

        CREATE TABLE commits (
            hash      BLOB PRIMARY KEY,
            parent    BLOB,
            tree_cid  BLOB NOT NULL,
            timestamp INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE block_index (
            v1_2025_cid BLOB PRIMARY KEY,
            file_cid    BLOB NOT NULL,
            block_offset INTEGER NOT NULL,
            block_length INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE block_chunks (
            v1_2025_cid BLOB NOT NULL,
            chunk_hash  BLOB NOT NULL,
            PRIMARY KEY (v1_2025_cid, chunk_hash)
        ) STRICT;
        ",
    )?;

    let (empty_tree_data, empty_tree_cid) = dag::empty_dir();
    db.execute(
        "INSERT INTO chunks (hash, data) VALUES (?1, ?2)",
        rusqlite::params![empty_tree_cid.to_bytes(), empty_tree_data],
    )?;

    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (commit_data, commit_cid) = dag::encode_commit(None, &empty_tree_cid.to_bytes(), now);
    db.execute(
        "INSERT INTO chunks (hash, data) VALUES (?1, ?2)",
        rusqlite::params![commit_cid.to_bytes(), commit_data],
    )?;
    db.execute(
        "INSERT INTO commits (hash, parent, tree_cid, timestamp) VALUES (?1, NULL, ?2, ?3)",
        rusqlite::params![commit_cid.to_bytes(), empty_tree_cid.to_bytes(), now],
    )?;

    db.execute(
        "INSERT INTO refs (name, commit_hash) VALUES ('main', ?1)",
        rusqlite::params![commit_cid.to_bytes()],
    )?;

    let hash_hex: String = commit_cid
        .hash_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    println!(
        "Created repository at {} (commit {})",
        path.display(),
        &hash_hex[..8]
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// FUSE filesystem
// ---------------------------------------------------------------------------

fn make_attr(inode: u64, kind: FileType, size: u64) -> FileAttr {
    let (perm, nlink) = match kind {
        FileType::Directory => (0o755, 2),
        FileType::Symlink => (0o777, 1),
        _ => (0o644, 1),
    };
    FileAttr {
        ino: inode,
        size,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

enum Entry {
    Dir {
        children: HashMap<String, u64>,
        parent: u64,
    },
    File {
        size: u64,
        /// Manifest CID (content-addressed list of chunk hashes).
        manifest_cid: Option<Vec<u8>>,
        /// v1-2025 root CID (IPFS-compatible).
        v1_2025_cid: Option<Vec<u8>>,
    },
    Symlink {
        target: String,
    },
}

/// In-memory dirty file buffer, accumulated on writes, flushed to CDC chunks on release.
#[derive(Default)]
struct DirtyFile {
    data: Vec<u8>,
}

struct SqliteFs {
    db: rusqlite::Connection,
    entries: HashMap<u64, Entry>,
    ino_gen: InoGen,
    open_files: HashMap<(u64, u64), DirtyFile>,
    next_fh: u64,
    dirty_root: bool,
    last_auto_commit: std::time::Instant,
}

impl SqliteFs {
    fn open(db_path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let db = rusqlite::Connection::open(db_path)?;
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

    /// Walk DB refs/commits/directory_index and build the snapshot tree.
    fn load_snapshot_tree(
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

    /// Recursively load a directory from directory_index, building Entry trees.
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
            if let Some(hashes) = dag::decode_file_manifest(&data) {
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
            total
        } else {
            0
        };
        Entry::File {
            size,
            manifest_cid: Some(cid.to_vec()),
            v1_2025_cid: None,
        }
    }

    /// Resolve a hex hash prefix in .sqlitefs/c/ into a Dir with the commit's tree.
    fn resolve_hash_prefix(
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
    fn maybe_auto_commit(&mut self) {
        if !self.dirty_root {
            return;
        }
        if self.last_auto_commit.elapsed().as_secs() >= AUTO_COMMIT_SECS {
            let _ = self.commit_dirty_tree();
            self.last_auto_commit = std::time::Instant::now();
        }
    }

    /// Commit the dirty tree to the 'main' ref.
    fn commit_dirty_tree(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.commit_as_ref("main")
    }

    /// Create a named commit (ref) from the current dirty tree.
    fn commit_as_ref(&mut self, ref_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Build recursive DAG from the entry tree rooted at ROOT_INODE.
        let (tree_data, tree_cid) = self.build_dir_dag(ROOT_INODE)?;

        // Write the tree dag-pb node into chunks.
        let _ = self.db.execute(
            "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
            rusqlite::params![tree_cid.to_bytes(), tree_data],
        );

        // Get current commit for this ref (as parent).
        let old_commit: Option<Vec<u8>> = self
            .db
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
        let _ = self.db.execute(
            "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
            rusqlite::params![commit_cid.to_bytes(), commit_data],
        );
        let _ = self.db.execute(
            "INSERT INTO commits (hash, parent, tree_cid, timestamp) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![commit_cid.to_bytes(), old_commit, tree_cid.to_bytes(), now],
        );
        let _ = self.db.execute(
            "INSERT OR REPLACE INTO refs (name, commit_hash) VALUES (?1, ?2)",
            rusqlite::params![ref_name, commit_cid.to_bytes()],
        );

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
    fn store_empty_manifest(&mut self) -> Vec<u8> {
        let (manifest_data, mc) = dag::encode_file_manifest(&[]);
        let _ = self.db.execute(
            "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
            rusqlite::params![mc.to_bytes(), manifest_data],
        );
        mc.to_bytes()
    }

    /// Recursively build a dag-pb directory node for `inode` and its children.
    /// Returns (encoded_dag_pb, Cid). Also inserts file manifests + chunks into DB
    /// and populates directory_index.
    fn build_dir_dag(
        &mut self,
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
                    let (child_dag, child_cid) = self.build_dir_dag(*child_ino)?;
                    let _ = self.db.execute(
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
                    self.store_empty_manifest()
                }
                Some(Entry::Symlink { target }) => {
                    let (sym_dag, sym_cid) = dag::encode_symlink(target);
                    let _ = self.db.execute(
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

        // Populate directory_index for this node.
        for (name, child_cid) in &links {
            let _ = self.db.execute(
                "INSERT OR REPLACE INTO directory_index (parent_cid, name, child_cid) VALUES (?1, ?2, ?3)",
                rusqlite::params![dir_cid.to_bytes(), name, child_cid],
            );
        }

        Ok((dir_data, dir_cid))
    }
}

// ---------------------------------------------------------------------------
// FUSE trait impl — writable root + read-only .sqlitefs/
// ---------------------------------------------------------------------------

impl fuser::Filesystem for SqliteFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        self.maybe_auto_commit();
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        // Resolve .sqlitefs/c/<hash-prefix>/ — query commits table, build dir tree.
        if parent == SNAPSHOT_HASH_INODE {
            let hex = name_str.to_ascii_lowercase();
            let name_owned = name_str.to_string();
            let target_ino: u64 = {
                let Some(Entry::Dir { children, .. }) = self.entries.get(&parent) else {
                    reply.error(ENOTDIR);
                    return;
                };
                if let Some(&ino) = children.get(name_str) {
                    ino
                } else {
                    match Self::resolve_hash_prefix(
                        &self.db,
                        &mut self.ino_gen,
                        &mut self.entries,
                        &hex,
                        SNAPSHOT_HASH_INODE,
                    ) {
                        Some(ino) => {
                            // Cache the name→ino mapping in .sqlitefs/c/.
                            if let Some(Entry::Dir { children, .. }) =
                                self.entries.get_mut(&SNAPSHOT_HASH_INODE)
                            {
                                children.insert(name_owned.clone(), ino);
                            }
                            ino
                        }
                        None => {
                            reply.error(ENOENT);
                            return;
                        }
                    }
                }
            };
            let Some(child) = self.entries.get(&target_ino) else {
                reply.error(ENOENT);
                return;
            };
            let (kind, size) = match child {
                Entry::Dir { .. } => (FileType::Directory, 0),
                Entry::Symlink { .. } => (FileType::Symlink, 0),
                Entry::File { size, .. } => (FileType::RegularFile, *size),
            };
            reply.entry(
                &std::time::Duration::MAX,
                &make_attr(target_ino, kind, size),
                0,
            );
            return;
        }

        let Some(Entry::Dir { children, .. }) = self.entries.get(&parent) else {
            reply.error(ENOTDIR);
            return;
        };
        if let Some(&child_ino) = children.get(name_str) {
            let Some(child) = self.entries.get(&child_ino) else {
                reply.error(ENOENT);
                return;
            };
            let (kind, size) = match child {
                Entry::Dir { .. } => (FileType::Directory, 0),
                Entry::Symlink { .. } => (FileType::Symlink, 0),
                Entry::File { size, .. } => (FileType::RegularFile, *size),
            };
            reply.entry(
                &std::time::Duration::MAX,
                &make_attr(child_ino, kind, size),
                0,
            );
        } else {
            reply.error(ENOENT);
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(entry) = self.entries.get(&ino) else {
            reply.error(ENOENT);
            return;
        };
        let (kind, size) = match entry {
            Entry::Dir { .. } => (FileType::Directory, 0),
            Entry::Symlink { .. } => (FileType::Symlink, 0),
            Entry::File { size, .. } => (FileType::RegularFile, *size),
        };
        reply.attr(&std::time::Duration::MAX, &make_attr(ino, kind, size));
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // Only truncation is implemented; mode/uid/gid/utimes are silently ignored.
        #[allow(clippy::collapsible_if)]
        if let Some(0) = size {
            // Truncated-to-zero file is still a real (empty) file: store an empty
            // manifest so it doesn't vanish from the next commit. Compute it before
            // taking the mutable borrow below.
            let empty_manifest = self.store_empty_manifest();
            if let Some(Entry::File {
                size: s,
                manifest_cid,
                v1_2025_cid,
            }) = self.entries.get_mut(&ino)
            {
                *s = 0;
                *manifest_cid = Some(empty_manifest);
                *v1_2025_cid = None;
                // Clear any open file buffers for this inode (don't remove them;
                // a write may follow from the same open file handle, e.g. echo > file).
                for ((i, _), df) in self.open_files.iter_mut() {
                    if *i == ino {
                        df.data.clear();
                    }
                }
                self.dirty_root = true;
                if self.dirty_root {
                    let _ = self.commit_dirty_tree();
                }
            }
            // ponytail: non-zero truncation on committed files not implemented
        }
        let (kind, s) = match self.entries.get(&ino) {
            Some(Entry::Dir { .. }) => (FileType::Directory, 0),
            Some(Entry::Symlink { .. }) => (FileType::Symlink, 0),
            Some(Entry::File { size, .. }) => (FileType::RegularFile, *size),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        reply.attr(&std::time::Duration::MAX, &make_attr(ino, kind, s));
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if parent == SNAPSHOT_DIR_INODE
            || parent == SNAPSHOT_REFS_INODE
            || parent == SNAPSHOT_HASH_INODE
        {
            reply.error(libc::EROFS);
            return;
        }
        let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&parent) else {
            reply.error(ENOTDIR);
            return;
        };
        if children.contains_key(name_str) {
            reply.error(libc::EEXIST);
            return;
        }
        let ino = self.ino_gen.next();
        children.insert(name_str.to_string(), ino);
        self.entries.insert(
            ino,
            Entry::Dir {
                children: HashMap::new(),
                parent,
            },
        );
        self.dirty_root = true;
        if self.dirty_root {
            let _ = self.commit_dirty_tree();
        }
        reply.entry(
            &std::time::Duration::MAX,
            &make_attr(ino, FileType::Directory, 0),
            0,
        );
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if parent == SNAPSHOT_DIR_INODE
            || parent == SNAPSHOT_REFS_INODE
            || parent == SNAPSHOT_HASH_INODE
        {
            reply.error(libc::EROFS);
            return;
        }
        let ino = {
            let Some(Entry::Dir { children, .. }) = self.entries.get(&parent) else {
                reply.error(ENOTDIR);
                return;
            };
            let Some(&ino) = children.get(name_str) else {
                reply.error(ENOENT);
                return;
            };
            ino
        };
        let is_empty = if let Some(Entry::Dir { children, .. }) = self.entries.get(&ino) {
            children.is_empty()
        } else {
            reply.error(ENOTDIR);
            return;
        };
        if !is_empty {
            reply.error(libc::ENOTEMPTY);
            return;
        }
        let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&parent) else {
            reply.error(ENOTDIR);
            return;
        };
        children.remove(name_str);
        self.entries.remove(&ino);
        self.dirty_root = true;
        if self.dirty_root {
            let _ = self.commit_dirty_tree();
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        self.maybe_auto_commit();
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        // Touch .sqlitefs/tag/<name> → named snapshot.
        if parent == SNAPSHOT_TAG_INODE {
            let _ = self.commit_as_ref(name_str);
            // Reload snapshot tree so the new ref appears.
            #[allow(clippy::collapsible_if)]
            if let Ok(refs) =
                SqliteFs::load_snapshot_tree(&self.db, &mut self.ino_gen, &mut self.entries)
            {
                if let Some(Entry::Dir { children, .. }) =
                    self.entries.get_mut(&SNAPSHOT_REFS_INODE)
                {
                    *children = refs;
                }
            }
            let ino = self.ino_gen.next();
            self.entries.insert(
                ino,
                Entry::File {
                    size: 0,
                    manifest_cid: None,
                    v1_2025_cid: None,
                },
            );
            let fh = self.next_fh;
            self.next_fh += 1;
            self.open_files.insert((ino, fh), DirtyFile::default());
            reply.created(
                &std::time::Duration::MAX,
                &make_attr(ino, FileType::RegularFile, 0),
                0,
                fh,
                0,
            );
            return;
        }
        if parent == SNAPSHOT_DIR_INODE
            || parent == SNAPSHOT_REFS_INODE
            || parent == SNAPSHOT_HASH_INODE
        {
            reply.error(libc::EROFS);
            return;
        }
        let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&parent) else {
            reply.error(ENOTDIR);
            return;
        };
        if children.contains_key(name_str) {
            reply.error(libc::EEXIST);
            return;
        }
        let ino = self.ino_gen.next();
        children.insert(name_str.to_string(), ino);
        // Newly created empty file: store an empty manifest now so it is persisted
        // even if never written to (touch foo survives a commit). Compute it before
        // the mutable borrows below.
        let empty_manifest = self.store_empty_manifest();
        self.entries.insert(
            ino,
            Entry::File {
                size: 0,
                manifest_cid: Some(empty_manifest),
                v1_2025_cid: None,
            },
        );

        let fh = self.next_fh;
        self.next_fh += 1;
        self.open_files.insert((ino, fh), DirtyFile::default());

        reply.created(
            &std::time::Duration::MAX,
            &make_attr(ino, FileType::RegularFile, 0),
            0,
            fh,
            0,
        );
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        self.maybe_auto_commit();
        if !matches!(self.entries.get(&ino), Some(Entry::File { .. })) {
            reply.error(libc::EINVAL);
            return;
        }
        let fh = self.next_fh;
        self.next_fh += 1;
        self.open_files.insert((ino, fh), DirtyFile::default());
        reply.opened(fh, 0);
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(df) = self.open_files.get_mut(&(ino, fh)) else {
            reply.error(libc::EBADF);
            return;
        };
        let off = offset as usize;
        let end = off + data.len();
        if end > df.data.len() {
            df.data.resize(end, 0);
        }
        df.data[off..end].copy_from_slice(data);
        if let Some(Entry::File { size, .. }) = self.entries.get_mut(&ino) {
            *size = df.data.len() as u64;
        }
        reply.written(data.len() as u32);
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(df) = self.open_files.remove(&(ino, fh)) {
            if !df.data.is_empty() {
                let chunks = cdc::chunk_file(&df.data);
                let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|(h, _)| *h).collect();

                // Build cumulative offsets for CDC chunk span tracking.
                let mut cdc_spans: Vec<([u8; 32], usize, usize)> = Vec::new();
                let mut cdc_off: usize = 0;
                for (hash, data) in &chunks {
                    let end = cdc_off + data.len();
                    cdc_spans.push((*hash, cdc_off, end));
                    cdc_off = end;
                }

                for (hash, chunk_data) in &chunks {
                    let _ = self.db.execute(
                        "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                        rusqlite::params![hash.to_vec(), chunk_data],
                    );
                }
                // Create the file manifest (dag-cbor array of chunk hashes).
                let (manifest_data, mc) = dag::encode_file_manifest(&chunk_hashes);
                let _ = self.db.execute(
                    "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                    rusqlite::params![mc.to_bytes(), manifest_data],
                );

                // Build v1-2025 DAG (IPFS-compatible) from the assembled file.
                let v1_dag = dag::build_v1_2025_dag(&df.data);

                for block in &v1_dag.blocks {
                    let _ = self.db.execute(
                        "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                        rusqlite::params![block.cid.to_bytes(), block.data],
                    );
                    let _ = self.db.execute(
                        "INSERT OR IGNORE INTO block_index (v1_2025_cid, file_cid, block_offset, block_length) VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![block.cid.to_bytes(), mc.to_bytes(), block.file_offset as i64, block.data.len() as i64],
                    );

                    // Map backing CDC chunks for this v1-2025 block.
                    let block_start = block.file_offset as usize;
                    let block_end = block_start + block.data.len();
                    for &(ref cdc_hash, cdc_start, cdc_end) in &cdc_spans {
                        if cdc_end > block_start && cdc_start < block_end {
                            let _ = self.db.execute(
                                "INSERT OR IGNORE INTO block_chunks (v1_2025_cid, chunk_hash) VALUES (?1, ?2)",
                                rusqlite::params![block.cid.to_bytes(), cdc_hash.to_vec()],
                            );
                        }
                    }
                }

                if let Some(Entry::File {
                    manifest_cid,
                    v1_2025_cid,
                    ..
                }) = self.entries.get_mut(&ino)
                {
                    *manifest_cid = Some(mc.to_bytes());
                    *v1_2025_cid = Some(v1_dag.root.to_bytes());
                }
            }
            self.dirty_root = true;
            // Auto-commit after every file release that wrote data.
            if self.dirty_root {
                let _ = self.commit_dirty_tree();
            }
        }
        reply.ok();
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if parent == SNAPSHOT_DIR_INODE
            || parent == SNAPSHOT_REFS_INODE
            || parent == SNAPSHOT_HASH_INODE
        {
            reply.error(libc::EROFS);
            return;
        }
        let ino = {
            let Some(Entry::Dir { children, .. }) = self.entries.get(&parent) else {
                reply.error(ENOTDIR);
                return;
            };
            let Some(&ino) = children.get(name_str) else {
                reply.error(ENOENT);
                return;
            };
            ino
        };
        if matches!(self.entries.get(&ino), Some(Entry::Dir { .. })) {
            reply.error(libc::EISDIR);
            return;
        }
        let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&parent) else {
            reply.error(ENOTDIR);
            return;
        };
        children.remove(name_str);
        self.entries.remove(&ino);
        self.dirty_root = true;
        if self.dirty_root {
            let _ = self.commit_dirty_tree();
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // Read from dirty buffer if open and has data, otherwise from committed chunks.
        // Never reply with more than the requested `size` bytes.
        if let Some(df) = self.open_files.get(&(ino, fh))
            && !df.data.is_empty()
        {
            let off = offset as usize;
            if off >= df.data.len() {
                reply.data(&[]);
                return;
            }
            let end = off.saturating_add(size as usize).min(df.data.len());
            reply.data(&df.data[off..end]);
            return;
        }
        // Read committed file: resolve manifest → chunks → assemble.
        if let Some(Entry::File {
            size: file_size,
            manifest_cid: Some(cid),
            ..
        }) = self.entries.get(&ino)
        {
            let off = offset as usize;
            if off >= *file_size as usize {
                reply.data(&[]);
                return;
            }
            let Ok(manifest_data) = self.db.query_row(
                "SELECT data FROM chunks WHERE hash = ?1",
                rusqlite::params![cid],
                |r| r.get::<_, Vec<u8>>(0),
            ) else {
                reply.data(&[]);
                return;
            };
            if let Some(hashes) = dag::decode_file_manifest(&manifest_data) {
                let mut assembled = Vec::new();
                for h in &hashes {
                    if let Ok(chunk_data) = self.db.query_row(
                        "SELECT data FROM chunks WHERE hash = ?1",
                        rusqlite::params![h.to_vec()],
                        |r| r.get::<_, Vec<u8>>(0),
                    ) {
                        assembled.extend_from_slice(&chunk_data);
                    }
                }
                if off < assembled.len() {
                    let end = off.saturating_add(size as usize).min(assembled.len());
                    reply.data(&assembled[off..end]);
                    return;
                }
            }
        }
        reply.data(&[]);
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        let Some(Entry::Symlink { target }) = self.entries.get(&ino) else {
            reply.error(libc::EINVAL);
            return;
        };
        reply.data(target.as_bytes());
    }

    fn symlink(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        link: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some(target_str) = link.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if parent == SNAPSHOT_DIR_INODE
            || parent == SNAPSHOT_REFS_INODE
            || parent == SNAPSHOT_HASH_INODE
        {
            reply.error(libc::EROFS);
            return;
        }
        let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&parent) else {
            reply.error(ENOTDIR);
            return;
        };
        if children.contains_key(name_str) {
            reply.error(libc::EEXIST);
            return;
        }
        let ino = self.ino_gen.next();
        children.insert(name_str.to_string(), ino);
        self.entries.insert(
            ino,
            Entry::Symlink {
                target: target_str.to_string(),
            },
        );
        self.dirty_root = true;
        if self.dirty_root {
            let _ = self.commit_dirty_tree();
        }
        reply.entry(
            &std::time::Duration::MAX,
            &make_attr(ino, FileType::Symlink, 0),
            0,
        );
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some(newname_str) = newname.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if parent == SNAPSHOT_DIR_INODE
            || parent == SNAPSHOT_REFS_INODE
            || newparent == SNAPSHOT_DIR_INODE
            || newparent == SNAPSHOT_REFS_INODE
            || newparent == SNAPSHOT_HASH_INODE
        {
            reply.error(libc::EROFS);
            return;
        }
        // Remove inode from old parent.
        let ino = {
            let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&parent) else {
                reply.error(ENOTDIR);
                return;
            };
            let Some(&ino) = children.get(name_str) else {
                reply.error(ENOENT);
                return;
            };
            children.remove(name_str);
            ino
        };
        // Add to new parent (overwrite target if it exists).
        let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&newparent) else {
            // Re-insert into old parent if destination doesn't exist.
            let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&parent) else {
                reply.error(ENOTDIR);
                return;
            };
            children.insert(name_str.to_string(), ino);
            reply.error(ENOTDIR);
            return;
        };
        let old = children.insert(newname_str.to_string(), ino);
        if let Some(old_ino) = old {
            self.entries.remove(&old_ino);
        }
        // Update parent pointer if it's a directory.
        if let Some(Entry::Dir { parent: p, .. }) = self.entries.get_mut(&ino) {
            *p = newparent;
        }
        self.dirty_root = true;
        if self.dirty_root {
            let _ = self.commit_dirty_tree();
        }
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(Entry::Dir { children, parent }) = self.entries.get(&ino) else {
            reply.error(ENOTDIR);
            return;
        };
        let mut entries: Vec<(u64, FileType, &str)> = vec![
            (ino, FileType::Directory, "."),
            (*parent, FileType::Directory, ".."),
        ];
        let visible: Vec<_> = if ino == ROOT_INODE {
            children
                .iter()
                .filter(|(name, _)| *name != ".sqlitefs")
                .map(|(name, &child_ino)| {
                    let kind = match &self.entries[&child_ino] {
                        Entry::Dir { .. } => FileType::Directory,
                        Entry::Symlink { .. } => FileType::Symlink,
                        Entry::File { .. } => FileType::RegularFile,
                    };
                    (child_ino, kind, name.as_str())
                })
                .collect()
        } else {
            children
                .iter()
                .map(|(name, &child_ino)| {
                    let kind = match &self.entries[&child_ino] {
                        Entry::Dir { .. } => FileType::Directory,
                        Entry::Symlink { .. } => FileType::Symlink,
                        Entry::File { .. } => FileType::RegularFile,
                    };
                    (child_ino, kind, name.as_str())
                })
                .collect()
        };
        entries.extend(visible);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_possible_wrap
        )]
        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                break;
            }
        }
        reply.ok();
    }

    fn getxattr(
        &mut self,
        _req: &Request,
        _ino: u64,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(libc::ENODATA);
    }

    fn listxattr(&mut self, _req: &Request, _ino: u64, _size: u32, reply: ReplyXattr) {
        reply.data(b"");
    }

    fn forget(&mut self, _req: &Request, ino: u64, _nlookup: u64) {
        if ino > SNAPSHOT_HASH_INODE {
            self.entries.remove(&ino);
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn mount_fs(db_path: &PathBuf, mount_point: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let fs = SqliteFs::open(db_path)?;
    fuser::mount2(
        fs,
        mount_point,
        &[
            MountOption::FSName("sqlitefs".into()),
            MountOption::AutoUnmount,
            MountOption::AllowRoot,
        ],
    )
    .map_err(|e| format!("mount failed (is /dev/fuse accessible?): {e}").into())
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Init { path } => init_repo(&path),
        Command::Mount {
            db_path,
            mount_point,
        } => mount_fs(&db_path, &mount_point),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
