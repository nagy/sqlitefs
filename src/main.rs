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
    /// Open file handles: (inode, fh) → dirty buffer.
    open_files: HashMap<(u64, u64), DirtyFile>,
    next_fh: u64,
    /// Has the root tree changed since the last commit?
    dirty_root: bool,
}

impl SqliteFs {
    fn open(db_path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let db = rusqlite::Connection::open(db_path)?;
        let mut ino_gen = InoGen::new(SNAPSHOT_REFS_INODE + 1);
        let mut entries = HashMap::new();

        // Wire up root directory (writable) + .sqlitefs/ (read-only snapshot tree).
        entries.insert(
            ROOT_INODE,
            Entry::Dir {
                children: HashMap::from([(".sqlitefs".into(), SNAPSHOT_DIR_INODE)]),
                parent: ROOT_INODE,
            },
        );
        entries.insert(
            SNAPSHOT_DIR_INODE,
            Entry::Dir {
                children: HashMap::from([("refs".into(), SNAPSHOT_REFS_INODE)]),
                parent: ROOT_INODE,
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
            let mut dir_children = HashMap::new();
            let mut cs =
                db.prepare("SELECT name, child_cid FROM directory_index WHERE parent_cid = ?1")?;
            let child_rows: Vec<(String, Vec<u8>)> = cs
                .query_map(rusqlite::params![tree_cid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?
                .filter_map(Result::ok)
                .collect();
            for (child_name, child_cid) in child_rows {
                let ino = ino_gen.next();
                // Look up file size from manifest.
                let size = Self::file_size_from_cid(db, &child_cid);
                entries.insert(
                    ino,
                    Entry::File {
                        size,
                        manifest_cid: Some(child_cid),
                        v1_2025_cid: None,
                    },
                );
                dir_children.insert(child_name, ino);
            }
            let ref_ino = ino_gen.next();
            entries.insert(
                ref_ino,
                Entry::Dir {
                    children: dir_children,
                    parent: SNAPSHOT_REFS_INODE,
                },
            );
            refs_children.insert(name, ref_ino);
        }
        Ok(refs_children)
    }

    fn file_size_from_cid(db: &rusqlite::Connection, cid: &[u8]) -> u64 {
        if let Ok(data) = db.query_row(
            "SELECT data FROM chunks WHERE hash = ?1",
            rusqlite::params![cid],
            |r| r.get::<_, Vec<u8>>(0),
        ) {
            if let Some(hashes) = dag::decode_file_manifest(&data) {
                let mut total: u64 = 0;
                for h in hashes {
                    if let Ok(chunk_data) = db.query_row(
                        "SELECT data FROM chunks WHERE hash = ?1",
                        rusqlite::params![h.to_vec()],
                        |r| r.get::<_, Vec<u8>>(0),
                    ) {
                        total += chunk_data.len() as u64;
                    }
                }
                return total;
            }
        }
        0
    }

    /// Rebuild the DAG for the root tree, create a commit, and update the snapshot view.
    fn commit_dirty_tree(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Build recursive DAG from the entry tree rooted at ROOT_INODE.
        let (tree_data, tree_cid) = self.build_dir_dag(ROOT_INODE)?;

        // Write the tree dag-pb node into chunks.
        let _ = self.db.execute(
            "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
            rusqlite::params![tree_cid.to_bytes(), tree_data],
        );

        // Get current commit for parent ref.
        let old_commit: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT commit_hash FROM refs WHERE name = 'main'",
                [],
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
            "UPDATE refs SET commit_hash = ?1 WHERE name = 'main'",
            rusqlite::params![commit_cid.to_bytes()],
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
                    manifest_cid: Some(ref cid),
                    ..
                }) => cid.clone(),
                Some(Entry::File {
                    manifest_cid: None, ..
                }) => {
                    // ponytail: file was created but never written to; skip.
                    continue;
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
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
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
            Entry::File { size, .. } => (FileType::RegularFile, *size),
        };
        reply.attr(&std::time::Duration::MAX, &make_attr(ino, kind, size));
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
        if parent == SNAPSHOT_DIR_INODE || parent == SNAPSHOT_REFS_INODE {
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
        if parent == SNAPSHOT_DIR_INODE || parent == SNAPSHOT_REFS_INODE {
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
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if parent == SNAPSHOT_DIR_INODE || parent == SNAPSHOT_REFS_INODE {
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
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
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
        if parent == SNAPSHOT_DIR_INODE || parent == SNAPSHOT_REFS_INODE {
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
        _size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // Read from dirty buffer if open, otherwise from committed chunks.
        if let Some(df) = self.open_files.get(&(ino, fh)) {
            let off = offset as usize;
            if off >= df.data.len() {
                reply.data(&[]);
                return;
            }
            reply.data(&df.data[off..]);
            return;
        }
        // Read committed file: resolve manifest → chunks → assemble.
        if let Some(Entry::File {
            size,
            manifest_cid: Some(ref cid),
            ..
        }) = self.entries.get(&ino)
        {
            let off = offset as usize;
            if off >= *size as usize {
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
                    reply.data(&assembled[off..]);
                    return;
                }
            }
        }
        reply.data(&[]);
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
        if ino > SNAPSHOT_REFS_INODE {
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
