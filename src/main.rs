use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use clap::{Parser, Subcommand};
use fuser::{
    FileAttr, FileType, MountOption, ReplyAttr, ReplyDirectory, ReplyEntry, ReplyXattr, Request,
};
use libc::{ENOENT, ENOTDIR};

// ---------------------------------------------------------------------------
// Inode allocator — tiny, just counts up from 2 (1 = root)
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
        /// Path to the .db file.
        db_path: PathBuf,
        /// Mount point.
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

    println!("Created empty repository at {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// FUSE filesystem
// ---------------------------------------------------------------------------

fn make_attr(inode: u64, kind: FileType, size: u64) -> FileAttr {
    let (perm, nlink) = match kind {
        FileType::Directory => (0o555, 2),
        _ => (0o444, 1),
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
        uid: 0,
        gid: 0,
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

#[allow(dead_code)]
enum Entry {
    Dir { children: HashMap<String, u64> },
    File { cid: Vec<u8>, size: u64 },
}

#[allow(dead_code)]
struct SqliteFs {
    db: rusqlite::Connection,
    entries: HashMap<u64, Entry>,
    ino_gen: InoGen,
}

impl SqliteFs {
    fn open(db_path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let db = rusqlite::Connection::open(db_path)?;
        let mut ino_gen = InoGen::new(SNAPSHOT_REFS_INODE + 1);

        // Build the snapshot dir tree in-memory.
        let ref_names: Vec<String> = db
            .prepare("SELECT name FROM refs")?
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut entries = HashMap::new();

        // ponytail: ref dirs are empty shells for now; resolving actual
        // commit trees needs the DAG layer.
        let mut refs_children = HashMap::new();
        for name in ref_names {
            let ino = ino_gen.next();
            entries.insert(
                ino,
                Entry::Dir {
                    children: HashMap::new(),
                },
            );
            refs_children.insert(name, ino);
        }

        entries.insert(
            ROOT_INODE,
            Entry::Dir {
                children: HashMap::from([(".sqlitefs".into(), SNAPSHOT_DIR_INODE)]),
            },
        );
        entries.insert(
            SNAPSHOT_DIR_INODE,
            Entry::Dir {
                children: HashMap::from([("refs".into(), SNAPSHOT_REFS_INODE)]),
            },
        );
        entries.insert(
            SNAPSHOT_REFS_INODE,
            Entry::Dir {
                children: refs_children,
            },
        );

        Ok(Self {
            db,
            entries,
            ino_gen,
        })
    }

    #[allow(dead_code)]
    fn entry(&self, inode: u64) -> Option<&Entry> {
        self.entries.get(&inode)
    }
}

// ---------------------------------------------------------------------------
// FUSE trait impl
// ---------------------------------------------------------------------------

impl fuser::Filesystem for SqliteFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let Some(Entry::Dir { children }) = self.entries.get(&parent) else {
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

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        _size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        let Some(Entry::File { size, .. }) = self.entries.get(&ino) else {
            reply.error(libc::EINVAL);
            return;
        };
        if offset >= *size as i64 {
            reply.data(&[]);
            return;
        }
        // File content lives in the DAG / chunk store — not wired up yet.
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
        let Some(Entry::Dir { children }) = self.entries.get(&ino) else {
            reply.error(ENOTDIR);
            return;
        };

        // Determine parent inode for ".."
        let parent = match ino {
            ROOT_INODE | SNAPSHOT_DIR_INODE => ROOT_INODE,
            SNAPSHOT_REFS_INODE => SNAPSHOT_DIR_INODE,
            _ => ROOT_INODE, // ponytail: child entries don't know their parent yet
        };

        let mut entries: Vec<(u64, FileType, &str)> = vec![
            (ino, FileType::Directory, "."),
            (parent, FileType::Directory, ".."),
        ];

        // Root: hide .sqlitefs/ from readdir but it's accessible by path.
        let visible = if ino == ROOT_INODE {
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
                .collect::<Vec<_>>()
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
            MountOption::RO,
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
