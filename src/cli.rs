// cli.rs — the CLI: repository init, integrity check (fsck), and ref listing.

use std::{path::PathBuf, time::UNIX_EPOCH};

use clap::{Parser, Subcommand};
use clap_complete::Shell;

use crate::dag;

// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "sqlitefs")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
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
    /// Run once after upgrading a repo created by an older sqlitefs version.
    RebuildIndex { db_path: PathBuf },
    /// List all refs and the commit hash each points at (git-style).
    ShowRef { db_path: PathBuf },
    /// Generate shell completion scripts.
    Completions {
        /// The shell to generate completions for.
        #[arg(value_enum)]
        shell: Shell,
    },
}

pub(crate) fn init_repo(path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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

/// Verify repository integrity: every CID reachable from a ref (commit, tree,
/// dir, symlink, file manifest, chunk) must resolve in the chunks table.
/// Prints each problem and returns Err if any are found. Also the foundation
/// for GC: chunks reachable from refs are exactly the live set.
pub(crate) fn fsck_repo(db_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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
pub(crate) fn hex_cid(cid: &[u8]) -> String {
    let mut s = String::with_capacity(cid.len() * 2);
    for b in cid {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Rebuild the `directory_index` materialized view from the chunk store.
/// `directory_index` is a derived cache (ADR-0001) and carries no data that
/// isn't reachable from refs; this is safe to run on any repo. Needed when the
/// view gains columns (e.g. `file_mode`) that old repos lack: `mount` reads the
/// view, so it must be upgraded before the schema change is used.
pub(crate) fn rebuild_index(db_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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

/// Recursively insert (parent, name, child, `file_mode`) rows for a dir tree.
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
            "INSERT OR IGNORE INTO directory_index (parent_cid, name, child_cid, file_mode) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![cid, name, child_cid, mode],
        )?;
        if mode.is_none() {
            walk_dir_for_index(db, &child_cid)?;
        }
    }
    Ok(())
}

/// Read all refs as (name, `commit_hash`) sorted by name. Testable core of show-ref.
/// Read all refs as (name, `commit_hash`) sorted by name. Testable core of show-ref.
#[allow(clippy::type_complexity)]
pub(crate) fn list_refs(
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

/// List all refs with the commit CID each points at, git show-ref style.
/// Commit CIDs are printed in standard base32 `CIDv1` form (`bafyrei…`).
pub(crate) fn show_ref(db_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open(db_path)?;
    for (name, hash) in list_refs(&db)? {
        let cid = dag::Cid::from_raw(dag::DAG_CBOR, hash[4..].try_into()?);
        println!("{} {}", cid.to_string_base32(), name);
    }
    Ok(())
}
