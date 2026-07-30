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

#[allow(dead_code)]
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
    },
}

/// In-memory dirty file buffer, accumulated on writes, flushed to CDC chunks on release.
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

        // Build snapshot ref tree from DB.
        let mut refs_children = HashMap::new();
        if let Ok(mut stmt) = db.prepare("SELECT name, commit_hash FROM refs") {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            }) {
                for row in rows.flatten() {
                    let (name, commit_hash) = row;
                    let tree_cid: Option<Vec<u8>> = db
                        .query_row(
                            "SELECT tree_cid FROM commits WHERE hash = ?1",
                            rusqlite::params![commit_hash],
                            |r| r.get(0),
                        )
                        .ok();

                    let mut dir_children = HashMap::new();
                    if let Some(ref tc) = tree_cid {
                        if let Ok(mut cs) = db.prepare(
                            "SELECT name, child_cid FROM directory_index WHERE parent_cid = ?1",
                        ) {
                            if let Ok(rows) = cs.query_map(rusqlite::params![tc], |r| {
                                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                            }) {
                                for cr in rows.flatten() {
                                    let ino = ino_gen.next();
                                    entries.insert(
                                        ino,
                                        Entry::File {
                                            // ponytail: size from DAG would need UnixFS decoding
                                            size: 0,
                                        },
                                    );
                                    dir_children.insert(cr.0, ino);
                                }
                            }
                        }
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
            }
        }

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
        })
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
        // ponytail: no mkdir in .sqlitefs/ snapshot tree.
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
        // ponytail: no rmdir in .sqlitefs/ snapshot tree.
        if parent == SNAPSHOT_DIR_INODE || parent == SNAPSHOT_REFS_INODE {
            reply.error(libc::EROFS);
            return;
        }
        // Extract ino from children first (mutable borrow released immediately).
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
        let is_empty = match self.entries.get(&ino) {
            Some(Entry::Dir { children, .. }) => children.is_empty(),
            _ => {
                reply.error(ENOTDIR);
                return;
            }
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
        // ponytail: no create in snapshot tree.
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
        self.entries.insert(ino, Entry::File { size: 0 });

        let fh = self.next_fh;
        self.next_fh += 1;
        self.open_files
            .insert((ino, fh), DirtyFile { data: Vec::new() });

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
        self.open_files
            .insert((ino, fh), DirtyFile { data: Vec::new() });
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
        // Update file size.
        if let Some(Entry::File { size }) = self.entries.get_mut(&ino) {
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
                // CDC chunk and insert into DB.
                let chunks = cdc::chunk_file(&df.data);
                for (hash, chunk_data) in chunks {
                    // Only insert if not already present (dedup).
                    let _ = self.db.execute(
                        "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                        rusqlite::params![hash.to_vec(), chunk_data],
                    );
                }
                // ponytail: no commit created yet, no directory_index updated.
                // Data is in the chunk store but unreachable until a commit is made.
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
        // Extract ino first, then check type.
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
        // Read from dirty buffer if open, otherwise from committed state.
        // ponytail: committed-state reads not wired yet (needs file chunk manifest).
        if let Some(df) = self.open_files.get(&(ino, fh)) {
            let off = offset as usize;
            if off >= df.data.len() {
                reply.data(&[]);
                return;
            }
            reply.data(&df.data[off..]);
        } else {
            // ponytail: committed file reads from DAG not wired yet.
            reply.data(&[]);
        }
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

        // Root: hide .sqlitefs/ from readdir.
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
        // Only forget non-root, non-snapshot inodes.
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
