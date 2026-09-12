//! CLI: repository init, integrity check (fsck), ref listing.

use std::{collections::HashMap, io::Write, path::PathBuf, time::UNIX_EPOCH};

use clap::{Parser, Subcommand};
use clap_complete::Shell;

use crate::{
    dag,
    fs::{Entry, ROOT_INODE, SqliteFs},
};

// CLI
// ---------------------------------------------------------------------------

/// The sqlitefs command line interface.
#[derive(Parser)]
#[command(name = "sqlitefs")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The subcommand to execute.
#[derive(Subcommand)]
pub enum Command {
    /// Create a new empty repository.
    Init { path: PathBuf },
    /// Mount a repository via FUSE.
    Mount {
        db_path: PathBuf,
        mount_point: PathBuf,
    },
    /// Verify repository integrity: every CID reachable from a ref resolves.
    Fsck { db_path: PathBuf },
    /// Rebuild the `directory_index` materialized view from the chunk store.
    ///
    /// Run once after upgrading a repo created by an older sqlitefs version.
    RebuildIndex { db_path: PathBuf },
    /// List all refs and the commit hash each points at (git-style).
    ShowRef { db_path: PathBuf },
    /// List entries under a path inside the repo (default: root).
    Ls {
        db_path: PathBuf,
        path: Option<String>,
    },
    /// Print a file from the repo to stdout.
    Cat { db_path: PathBuf, path: String },
    /// Copy between host filesystem and repo, or within the repo.
    ///
    /// Host paths are `./x`, `../x`, `/abs` or `~/x`; everything else resolves
    /// in the repo (falling back to the host). Repo→host extracts,
    /// host→repo imports (creating parent dirs), repo→repo copies by
    /// manifest link (chunks dedup).
    Cp {
        db_path: PathBuf,
        src: String,
        dst: String,
    },
    /// Rename (move) a path inside the repo.
    Mv {
        db_path: PathBuf,
        src: String,
        dst: String,
    },
    /// Generate shell completion scripts.
    Completions {
        /// The shell to generate completions for.
        #[arg(value_enum)]
        shell: Shell,
    },
}

pub fn init_repo(path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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
            file_mode   INTEGER,
            PRIMARY KEY (parent_cid, name)
        ) STRICT;

        CREATE TABLE commits (
            hash      BLOB PRIMARY KEY,
            parent    BLOB,
            tree_cid  BLOB NOT NULL,
            timestamp INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE block_index (
            v1_2025_cid BLOB NOT NULL,
            file_cid    BLOB NOT NULL,
            block_offset INTEGER NOT NULL,
            block_length INTEGER NOT NULL,
            PRIMARY KEY (file_cid, v1_2025_cid)
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
    // An empty dir has no directory_index rows; load_dir_recursive on the empty
    // tree just yields an empty Dir. Nothing to seed.

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

    let hash_hex: String =
        commit_cid
            .hash_bytes()
            .iter()
            .fold(String::with_capacity(64), |mut s, b| {
                use std::fmt::Write;
                let _ = write!(s, "{b:02x}");
                s
            });
    println!(
        "Created repository at {} (commit {})",
        path.display(),
        &hash_hex[..8]
    );
    Ok(())
}

/// Verify repository integrity: every CID reachable from a ref resolves.
///
/// Walks each ref's commit chain (commit, tree, dir, symlink, file manifest,
/// chunk) and checks every CID resolves in the chunks table. Prints each problem
/// and returns `Err` if any are found. Also the foundation for GC: chunks
/// reachable from refs are exactly the live set.
pub fn fsck_repo(db_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open(db_path)?;
    let mut errors: Vec<String> = Vec::new();
    let mut visited: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

    let chunk_exists = |cid: &[u8], db: &rusqlite::Connection| -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM chunks WHERE hash = ?1)",
            rusqlite::params![cid],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            != 0
    };

    // Walk a directory tree: verify the dir chunk and every child link exists.
    fn walk_tree(
        db: &rusqlite::Connection,
        cid: &[u8],
        errors: &mut Vec<String>,
        visited: &mut std::collections::HashSet<Vec<u8>>,
        chunk_exists: &dyn Fn(&[u8], &rusqlite::Connection) -> bool,
    ) {
        if !visited.insert(cid.to_vec()) {
            return;
        }
        if !chunk_exists(cid, db) {
            errors.push(format!("missing chunk for tree node {}", hex_cid(cid)));
            return;
        }
        let Ok(data) = db.query_row(
            "SELECT data FROM chunks WHERE hash = ?1",
            rusqlite::params![cid],
            |r| r.get::<_, Vec<u8>>(0),
        ) else {
            return;
        };
        // Symlink: nothing further to verify.
        if dag::decode_symlink(&data).is_some() {
            return;
        }
        // File manifest: verify every chunk hash resolves.
        if dag::is_file_manifest(&data) {
            if let Some((_, hashes, _)) = dag::decode_file_manifest(&data) {
                for h in &hashes {
                    if !chunk_exists(h, db) {
                        errors.push(format!(
                            "file {} references missing chunk {}",
                            hex_cid(cid),
                            hex_cid(h)
                        ));
                    }
                }
            }
            return;
        }
        // Directory: recurse into links.
        if let Some(links) = dag::decode_dir_links(&data) {
            for (_name, child_cid) in links {
                walk_tree(db, &child_cid, errors, visited, chunk_exists);
            }
        }
    }

    // For each ref, walk the commit chain then the tree.
    let mut stmt = db.prepare("SELECT name, commit_hash FROM refs")?;
    let refs: Vec<(String, Vec<u8>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    for (name, commit_hash) in refs {
        let mut current = Some(commit_hash);
        let mut chain: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        while let Some(hash) = current {
            if !chain.insert(hash.clone()) {
                errors.push(format!("ref {name}: commit cycle detected"));
                break;
            }
            if !chunk_exists(&hash, &db) {
                errors.push(format!("ref {name}: missing commit {}", hex_cid(&hash)));
                break;
            }
            let row: Option<(Option<Vec<u8>>, Vec<u8>)> = db
                .query_row(
                    "SELECT parent, tree_cid FROM commits WHERE hash = ?1",
                    rusqlite::params![hash],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            let Some((parent, tree_cid)) = row else {
                errors.push(format!(
                    "ref {name}: commit {} not in commits table",
                    hex_cid(&hash)
                ));
                break;
            };
            walk_tree(&db, &tree_cid, &mut errors, &mut visited, &chunk_exists);
            current = parent;
        }
    }

    if errors.is_empty() {
        println!("fsck: OK — all refs, commits and reachable objects resolve.");
        Ok(())
    } else {
        for e in &errors {
            eprintln!("fsck: {e}");
        }
        Err(format!("fsck: {} problem(s) found", errors.len()).into())
    }
}

/// Format a CID's bytes as hex for error messages.
pub fn hex_cid(cid: &[u8]) -> String {
    let mut s = String::with_capacity(cid.len() * 2);
    for b in cid {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Rebuild the `directory_index` materialized view from the chunk store.
///
/// `directory_index` is a derived cache (ADR-0001) and carries no data that
/// isn't reachable from refs; this is safe to run on any repo. Needed when the
/// view gains columns (e.g. `file_mode`) that old repos lack: `mount` reads the
/// view, so it must be upgraded before the schema change is used.
pub fn rebuild_index(db_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open(db_path)?;

    // Old repos created before the view had `file_mode`: recreate the table
    // with the new column. The unique constraint is added back after the walk
    // (INSERT OR IGNORE would otherwise skip re-inserting an existing row's
    // parent/name with the new column).
    db.execute_batch(
        "DROP TABLE IF EXISTS directory_index;
         CREATE TABLE directory_index (
             parent_cid BLOB NOT NULL,
             name        TEXT NOT NULL,
             child_cid   BLOB NOT NULL,
             file_mode   INTEGER
         ) STRICT;",
    )?;

    // Walk every ref's commit chain → tree, like fsck, and repopulate the view.
    let refs = list_refs(&db)?;
    let mut visited: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for (_name, commit_hash) in refs {
        let mut current = Some(commit_hash);
        while let Some(hash) = current {
            if !visited.insert(hash.clone()) {
                break;
            }
            let row: Option<(Option<Vec<u8>>, Vec<u8>)> = db
                .query_row(
                    "SELECT parent, tree_cid FROM commits WHERE hash = ?1",
                    rusqlite::params![hash],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            let Some((parent, tree_cid)) = row else {
                break;
            };
            walk_dir_for_index(&db, &tree_cid)?;
            current = parent;
        }
    }

    // Restore the uniqueness constraint.
    db.execute_batch(
        "CREATE UNIQUE INDEX idx_directory_index_pk ON directory_index (parent_cid, name);",
    )?;

    println!("rebuild-index: OK — directory_index rebuilt from refs.");
    Ok(())
}

/// Recursively insert `(parent, name, child, file_mode)` rows for a dir tree.
///
/// Mirrors `build_dir_dag`: child is a file if its chunk is a file manifest,
/// a directory if its chunk is a dir dag-pb, a symlink otherwise.
fn walk_dir_for_index(
    db: &rusqlite::Connection,
    cid: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let Ok(data) = db.query_row(
        "SELECT data FROM chunks WHERE hash = ?1",
        rusqlite::params![cid],
        |r| r.get::<_, Vec<u8>>(0),
    ) else {
        return Ok(());
    };
    let Some(links) = dag::decode_dir_links(&data) else {
        return Ok(());
    };
    for (name, child_cid) in links {
        let mode = match db.query_row(
            "SELECT data FROM chunks WHERE hash = ?1",
            rusqlite::params![child_cid],
            |r| r.get::<_, Vec<u8>>(0),
        ) {
            Ok(data) if dag::is_file_manifest(&data) => {
                dag::decode_file_manifest(&data).and_then(|(_, _, m)| m)
            }
            _ => None,
        };
        db.execute(
            "INSERT OR IGNORE INTO directory_index (parent_cid, name, child_cid, file_mode) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![cid, name, child_cid, mode],
        )?;
        if mode.is_none() {
            walk_dir_for_index(db, &child_cid)?;
        }
    }
    Ok(())
}

/// Read all refs as `(name, commit_hash)` sorted by name.
///
/// Testable core of show-ref.
#[allow(clippy::type_complexity)]
pub fn list_refs(
    db: &rusqlite::Connection,
) -> Result<Vec<(String, Vec<u8>)>, Box<dyn std::error::Error>> {
    let mut stmt = db.prepare("SELECT name, commit_hash FROM refs ORDER BY name")?;
    let refs = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    Ok(refs)
}

/// List all refs with the commit CID each points at.
///
/// Git show-ref style. Commit CIDs are printed in standard base32 `CIDv1` form
/// (`bafyrei…`).
pub fn show_ref(db_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open(db_path)?;
    for (name, hash) in list_refs(&db)? {
        let cid = dag::Cid::from_raw(dag::DAG_CBOR, hash[4..].try_into()?);
        println!("{} {}", cid.to_string_base32(), name);
    }
    Ok(())
}

// Repo file access: ls / cat / cp / mv
// ---------------------------------------------------------------------------

/// A node resolved from the repo tree (read-only, via `directory_index`).
pub enum Node {
    /// Directory links: `(name, child_cid)`.
    Dir(Vec<(String, Vec<u8>)>),
    /// File manifest bytes plus the file's logical size.
    File {
        size: u64,
        manifest: Vec<u8>,
    },
    Symlink(String),
}

/// Fetch a chunk by CID bytes.
///
/// # Errors
/// When the chunk is missing from the store.
pub fn load_chunk(db: &rusqlite::Connection, cid: &[u8]) -> Result<Vec<u8>, String> {
    db.query_row(
        "SELECT data FROM chunks WHERE hash = ?1",
        rusqlite::params![cid],
        |r| r.get(0),
    )
    .map_err(|_| format!("missing chunk {}", hex_cid(cid)))
}

/// Classify a chunk's bytes into a `Node`, loading the file size on demand.
///
/// # Errors
/// When the chunk bytes match no known DAG node type.
pub fn classify(db: &rusqlite::Connection, cid: &[u8], data: Vec<u8>) -> Result<Node, String> {
    if let Some(target) = dag::decode_symlink(&data) {
        return Ok(Node::Symlink(target));
    }
    if dag::is_dir_dag(&data) {
        return Ok(Node::Dir(dag::decode_dir_links(&data).unwrap_or_default()));
    }
    if let Some((size, hashes, _)) = dag::decode_file_manifest(&data) {
        let size = if let Some(sz) = size {
            sz
        } else {
            // Old format (no embedded size): sum chunk lengths.
            let mut total: u64 = 0;
            for h in &hashes {
                total += load_chunk(db, h)?.len() as u64;
            }
            total
        };
        return Ok(Node::File {
            size,
            manifest: data,
        });
    }
    Err(format!("unknown chunk type at {}", hex_cid(cid)))
}

/// Resolve a repo path against the `main` ref tree, read-only.
///
/// Walks `directory_index` like the FUSE mount does; never writes to the DB.
/// The `.sqlitefs` snapshot namespace is not reachable this way.
///
/// # Errors
/// When the `main` ref or its commit is missing, a chunk is missing, or the
/// path does not resolve.
pub fn resolve_ro(db: &rusqlite::Connection, path: &str) -> Result<Node, String> {
    resolve_ref_ro(db, "main", path)
}

/// Resolve a repo path against an arbitrary ref's tree, read-only.
///
/// Like [`resolve_ro`], but walks the tree the given ref points at (e.g. a
/// tagged snapshot), not just `main`.
///
/// # Errors
/// When the ref or its commit is missing, a chunk is missing, or the path does
/// not resolve.
pub fn resolve_ref_ro(
    db: &rusqlite::Connection,
    ref_name: &str,
    path: &str,
) -> Result<Node, String> {
    let commit_hash: Vec<u8> = db
        .query_row(
            "SELECT commit_hash FROM refs WHERE name = ?1",
            rusqlite::params![ref_name],
            |r| r.get(0),
        )
        .map_err(|_| format!("no '{ref_name}' ref in repository"))?;
    let tree_cid: Vec<u8> = db
        .query_row(
            "SELECT tree_cid FROM commits WHERE hash = ?1",
            rusqlite::params![commit_hash],
            |r| r.get(0),
        )
        .map_err(|_| format!("'{ref_name}' ref points at a missing commit"))?;

    let mut node = classify(db, &tree_cid, load_chunk(db, &tree_cid)?)?;
    for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
        let Node::Dir(links) = node else {
            return Err(format!("not a directory: {comp}"));
        };
        if comp == ".sqlitefs" {
            return Err("cannot access .sqlitefs/ via the CLI".to_string());
        }
        let Some((_, child_cid)) = links.iter().find(|(n, _)| n == comp) else {
            return Err(format!("no such path: {path}"));
        };
        let data = load_chunk(db, child_cid)?;
        node = classify(db, child_cid, data)?;
    }
    Ok(node)
}

/// Assemble a file's content from its manifest.
///
/// # Errors
/// When the manifest is not a file manifest or a chunk is missing.
pub fn read_file(db: &rusqlite::Connection, manifest: &[u8]) -> Result<Vec<u8>, String> {
    let Some((_, hashes, _)) = dag::decode_file_manifest(manifest) else {
        return Err("not a file manifest".to_string());
    };
    let mut out = Vec::new();
    for h in &hashes {
        out.extend_from_slice(&load_chunk(db, h)?);
    }
    Ok(out)
}

/// `sqlitefs ls <db> [path]` — list a repo directory.
///
/// # Errors
/// When the repo or path does not resolve.
pub fn ls(db_path: &PathBuf, path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open(db_path)?;
    let path = path.unwrap_or("");
    match resolve_ro(&db, path)? {
        Node::Dir(links) => {
            let mut links = links;
            links.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, cid) in &links {
                match load_chunk(&db, cid)
                    .ok()
                    .and_then(|d| classify(&db, cid, d).ok())
                {
                    Some(Node::Dir(_)) => println!("{name}/"),
                    Some(Node::Symlink(target)) => println!("{name} -> {target}"),
                    Some(Node::File { size, .. }) => println!("{name}  ({size} bytes)"),
                    None => println!("{name}  (unreadable)"),
                }
            }
        }
        Node::File { size, .. } => println!("{path}  ({size} bytes)"),
        Node::Symlink(target) => println!("{path} -> {target}"),
    }
    Ok(())
}

/// `sqlitefs cat <db> <path>` — print a repo file to stdout.
///
/// # Errors
/// When the repo or path does not resolve, or the path is not a file.
pub fn cat(db_path: &PathBuf, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open(db_path)?;
    match resolve_ro(&db, path)? {
        Node::File { manifest, .. } => {
            let data = read_file(&db, &manifest)?;
            std::io::stdout().write_all(&data)?;
        }
        Node::Dir(_) => return Err(format!("{path}: is a directory").into()),
        Node::Symlink(target) => {
            return Err(format!("{path}: is a symlink to {target} (not followed)").into());
        }
    }
    Ok(())
}

/// Direction inference for `cp`.
///
/// Paths styled as host paths (`/abs`, `./x`, `../x`, `~/x`) are always host
/// paths; other paths resolve in the repo first and fall back to the host if
/// a file of that name exists there. When both directions are possible, the
/// repo wins. `cp` then means:
///
/// - host src + repo dst → import (missing parent dirs are created)
/// - repo src + host dst → extract
/// - repo src + repo dst → in-repo copy (manifest link, chunks dedup)
fn host_styled(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with("./")
        || path.starts_with("../")
        || path.starts_with("~/")
        || path == "."
        || path == ".."
}

/// `sqlitefs cp <db> <src> <dst>` — import, extract, or in-repo copy.
///
/// # Errors
/// When a path is ambiguous (exists both in repo and on host), missing from
/// both, or the mutation/commit fails.
pub fn cp(db_path: &PathBuf, src: &str, dst: &str) -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open(db_path)?;
    if host_styled(src) {
        if !std::path::Path::new(src).is_file() {
            return Err(format!("{src}: no such host file").into());
        }
        if host_styled(dst) {
            return Err("both src and dst are host paths — nothing to do".into());
        }
        return import(db_path, src, dst);
    }
    if resolve_ro(&db, src).is_ok() {
        if host_styled(dst) {
            extract(&db, src, dst)
        } else {
            copy_in_repo(db_path, src, dst)
        }
    } else if std::path::Path::new(src).is_file() {
        import(db_path, src, dst)
    } else {
        Err(format!("{src}: no such path in repo or on host").into())
    }
}

/// Copy a host file into the repo at `dst`, then commit to `main`.
fn import(db_path: &PathBuf, src: &str, dst: &str) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(src)?;
    let mode = (std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(src)?.permissions())
        & 0o7777) as u32;
    let mut fs = SqliteFs::open(db_path)?;
    let (parent_ino, name) = resolve_parent_creating(&mut fs, dst)?;
    // Overwrite like GNU cp: drop any existing entry at dst.
    let _ = remove_child(&mut fs, parent_ino, &name);

    let (mc, v1_cid) = if data.is_empty() {
        (fs.store_empty_manifest(), None)
    } else {
        let chunks = crate::cdc::chunk_file(&data);
        let mc = fs.store_released_file(&data, &chunks, mode)?;
        let v1 = dag::build_v1_2025_dag(&data);
        (mc.to_bytes(), Some(v1.root.to_bytes()))
    };
    let ino = fs.ino_gen.next();
    fs.entries.insert(
        ino,
        Entry::File {
            size: data.len() as u64,
            mode,
            manifest_cid: Some(mc),
            v1_2025_cid: v1_cid,
        },
    );
    insert_child(&mut fs, parent_ino, &name, ino)?;
    fs.commit_dirty_tree()?;
    println!("imported {src} -> {dst}");
    Ok(())
}

/// Copy a repo file out to the host (read-only; no commit).
fn extract(
    db: &rusqlite::Connection,
    src: &str,
    dst: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match resolve_ro(db, src)? {
        Node::File { manifest, .. } => {
            let data = read_file(db, &manifest)?;
            std::fs::write(dst, data)?;
            println!("extracted {src} -> {dst}");
            Ok(())
        }
        Node::Dir(_) => Err(format!("{src}: is a directory (extract files individually)").into()),
        Node::Symlink(target) => Err(format!("{src}: is a symlink to {target}").into()),
    }
}

/// Resolve a repo path in the in-memory entry tree (mutating ops).
fn lookup_ino(fs: &SqliteFs, path: &str) -> Result<u64, String> {
    let mut cur = ROOT_INODE;
    for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
        if comp == ".sqlitefs" {
            return Err("cannot access .sqlitefs/ via the CLI".to_string());
        }
        let Some(Entry::Dir { children, .. }) = fs.entries.get(&cur) else {
            return Err(format!("not a directory: {comp}"));
        };
        cur = *children
            .get(comp)
            .ok_or_else(|| format!("no such path: {path}"))?;
    }
    Ok(cur)
}

/// Like `resolve_parent`, but creates missing intermediate directories
/// (importing `a/b/c.txt` into an empty repo works like `mkdir -p`).
fn resolve_parent_creating(fs: &mut SqliteFs, path: &str) -> Result<(u64, String), String> {
    let (parent, name) = path
        .rsplit_once('/')
        .map(|(p, n)| (p, n.to_string()))
        .unwrap_or(("", path.to_string()));
    let mut cur = ROOT_INODE;
    for comp in parent.split('/').filter(|c| !c.is_empty() && *c != ".") {
        if comp == ".sqlitefs" {
            return Err("cannot access .sqlitefs/ via the CLI".to_string());
        }
        let next = {
            let Some(Entry::Dir { children, .. }) = fs.entries.get(&cur) else {
                return Err(format!("not a directory: {comp}"));
            };
            children.get(comp).copied()
        };
        cur = match next {
            Some(ino) => ino,
            None => {
                let ino = fs.ino_gen.next();
                fs.entries.insert(
                    ino,
                    Entry::Dir {
                        children: HashMap::new(),
                        parent: cur,
                    },
                );
                insert_child(fs, cur, comp, ino)?;
                ino
            }
        };
    }
    if name.is_empty() || name == "." || name == ".." || name == ".sqlitefs" {
        return Err(format!("invalid target name: {path}"));
    }
    Ok((cur, name))
}

/// Resolve a path to its parent dir inode plus the final component.
fn resolve_parent(fs: &SqliteFs, path: &str) -> Result<(u64, String), String> {
    let (parent, name) = path
        .rsplit_once('/')
        .map(|(p, n)| (p, n.to_string()))
        .unwrap_or(("", path.to_string()));
    let parent_ino = lookup_ino(fs, parent)?;
    if name.is_empty() || name == "." || name == ".." || name == ".sqlitefs" {
        return Err(format!("invalid target name: {path}"));
    }
    Ok((parent_ino, name))
}

/// Insert `child_ino` under `parent_ino` as `name`.
fn insert_child(
    fs: &mut SqliteFs,
    parent_ino: u64,
    name: &str,
    child_ino: u64,
) -> Result<(), String> {
    let Some(Entry::Dir { children, .. }) = fs.entries.get_mut(&parent_ino) else {
        return Err("destination parent is not a directory".to_string());
    };
    children.insert(name.to_string(), child_ino);
    Ok(())
}

/// Deep-clone an entry subtree with fresh inodes (in-repo copy).
///
/// Files copy by manifest reference — the clone shares chunks with the
/// original (content-addressed dedup, same as hard links in ADR-0005).
fn clone_entry(fs: &mut SqliteFs, ino: u64, parent: u64) -> Result<u64, String> {
    let new_ino = fs.ino_gen.next();
    // Snapshot the entry's data first: the recursive dir case needs `&mut`.
    enum Kind {
        Dir(Vec<(String, u64)>),
        File {
            size: u64,
            mode: u32,
            manifest_cid: Option<Vec<u8>>,
            v1_2025_cid: Option<Vec<u8>>,
        },
        Sym(String),
    }
    let kind = match fs.entries.get(&ino) {
        Some(Entry::Dir { children, .. }) => {
            Kind::Dir(children.iter().map(|(n, &c)| (n.clone(), c)).collect())
        }
        Some(Entry::File {
            size,
            mode,
            manifest_cid,
            v1_2025_cid,
        }) => Kind::File {
            size: *size,
            mode: *mode,
            manifest_cid: manifest_cid.clone(),
            v1_2025_cid: v1_2025_cid.clone(),
        },
        Some(Entry::Symlink { target }) => Kind::Sym(target.clone()),
        None => return Err(format!("missing inode {ino}")),
    };
    match kind {
        Kind::Dir(children) => {
            let mut cloned = std::collections::HashMap::new();
            for (name, child) in children {
                cloned.insert(name, clone_entry(fs, child, new_ino)?);
            }
            fs.entries.insert(
                new_ino,
                Entry::Dir {
                    children: cloned,
                    parent,
                },
            );
        }
        Kind::File {
            size,
            mode,
            manifest_cid,
            v1_2025_cid,
        } => {
            fs.entries.insert(
                new_ino,
                Entry::File {
                    size,
                    mode,
                    manifest_cid,
                    v1_2025_cid,
                },
            );
        }
        Kind::Sym(target) => {
            fs.entries.insert(new_ino, Entry::Symlink { target });
        }
    }
    Ok(new_ino)
}

/// `sqlitefs mv <db> <src> <dst>` — rename inside the repo, then commit.
///
/// # Errors
/// When a path does not resolve, a directory would move into itself, or the
/// commit fails.
pub fn mv(db_path: &PathBuf, src: &str, dst: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut fs = SqliteFs::open(db_path)?;
    let (src_parent, src_name) = resolve_parent(&fs, src)?;
    let src_ino = lookup_ino(&fs, src)?;
    let (dst_parent, dst_name) = resolve_parent(&fs, dst)?;

    // Refuse to move a directory into itself.
    let mut anc = dst_parent;
    while anc != ROOT_INODE {
        if anc == src_ino {
            return Err("cannot move a directory into itself".into());
        }
        match fs.entries.get(&anc) {
            Some(Entry::Dir { parent, .. }) => anc = *parent,
            _ => break,
        }
    }

    remove_child(&mut fs, src_parent, &src_name)?;
    // Overwrite like FUSE rename / cp: drop any existing entry at dst.
    let _ = remove_child(&mut fs, dst_parent, &dst_name);
    insert_child(&mut fs, dst_parent, &dst_name, src_ino)?;
    fs.commit_dirty_tree()?;
    println!("moved {src} -> {dst}");
    Ok(())
}

/// `sqlitefs cp <db> <src> <dst>` in-repo variant — link-copy + commit.
///
/// # Errors
/// When a path does not resolve or the commit fails.
pub fn copy_in_repo(
    db_path: &PathBuf,
    src: &str,
    dst: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut fs = SqliteFs::open(db_path)?;
    let src_ino = lookup_ino(&fs, src)?;
    let (dst_parent, dst_name) = resolve_parent(&fs, dst)?;
    // Overwrite like GNU cp: drop any existing entry at dst.
    let _ = remove_child(&mut fs, dst_parent, &dst_name);
    let new_ino = clone_entry(&mut fs, src_ino, dst_parent)?;
    insert_child(&mut fs, dst_parent, &dst_name, new_ino)?;
    fs.commit_dirty_tree()?;
    println!("copied {src} -> {dst}");
    Ok(())
}

/// Remove `name` from `parent_ino`'s children.
fn remove_child(fs: &mut SqliteFs, parent_ino: u64, name: &str) -> Result<(), String> {
    let Some(Entry::Dir { children, .. }) = fs.entries.get_mut(&parent_ino) else {
        return Err("source parent is not a directory".to_string());
    };
    children
        .remove(name)
        .map(|_| ())
        .ok_or_else(|| format!("no such entry: {name}"))
}
