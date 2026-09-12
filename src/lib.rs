//! sqlitefs — a versioned, content-addressed filesystem in a single SQLite file.
//!
//! Library crate: the entry tree, DAG building/committing, chunk store, and
//! CLI helpers. The FUSE frontend lives in the binary (`src/main.rs`); the
//! Emacs dynamic-module frontend is `emacs_module.rs` (feature `emacs` keeps
//! the default build free of that dependency).

pub mod cdc;
pub mod cli;
pub mod dag;
#[cfg(feature = "emacs")]
pub mod emacs_module;
pub mod fs;
pub mod fuse;
#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsStr, path::PathBuf};

    use crate::{
        cdc,
        cli::{init_repo, list_refs, rebuild_index},
        dag,
        fs::{
            Entry, ROOT_INODE, SNAPSHOT_REFS_INODE, SqliteFs, store_empty_manifest,
            store_file_manifest,
        },
    };

    fn temp_db_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sqlitefs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Init a repo and open it, verifying the schema + empty main ref exist.
    #[test]
    fn test_init_then_open() {
        let path = temp_db_path("init_open.db");
        init_repo(&path).unwrap();
        let fs = SqliteFs::open(&path).unwrap();
        // main ref must be present in the snapshot tree.
        let refs = match fs.entries.get(&SNAPSHOT_REFS_INODE) {
            Some(Entry::Dir { children, .. }) => children,
            other => panic!("refs inode should be a Dir, got {:?}", other.map(|_| ())),
        };
        assert!(refs.contains_key("main"));
        let _ = std::fs::remove_file(&path);
    }

    /// show-ref lists refs sorted by name: main after init, plus tagged refs.
    #[test]
    fn test_show_ref_lists_all_refs() {
        let path = temp_db_path("show_ref.db");
        init_repo(&path).unwrap();

        let db = rusqlite::Connection::open(&path).unwrap();
        let names: Vec<String> = list_refs(&db)
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["main"]);

        // Commit a second ref; both must appear, sorted.
        let mut fs = SqliteFs::open(&path).unwrap();
        fs.commit_as_ref("feature").unwrap();
        let names: Vec<String> = list_refs(&db)
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["feature", "main"]);

        // Each ref's hash is a valid dag-cbor commit CID (36 bytes, codec 0x71).
        // Its base32 string starts with the standard CIDv1 dag-cbor prefix `bafyrei`.
        for (_, hash) in list_refs(&db).unwrap() {
            assert_eq!(hash.len(), 36);
            assert_eq!(hash[1], 0x71); // dag-cbor
            let cid = dag::Cid::from_raw(dag::DAG_CBOR, hash[4..].try_into().unwrap());
            assert!(cid.to_string_base32().starts_with("bafyrei"));
        }

        let _ = std::fs::remove_file(&path);
    }

    /// Commit an in-memory tree and verify it round-trips through reopen.
    #[test]
    fn test_commit_roundtrip_persists_file() {
        let path = temp_db_path("commit_roundtrip.db");
        init_repo(&path).unwrap();
        let mut fs = SqliteFs::open(&path).unwrap();

        // Store a small file's chunks + manifest directly (release-path building blocks).
        let data = b"hello sqlitefs".to_vec();
        let chunks = cdc::chunk_file(&data);
        let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|(h, _)| *h).collect();
        for (h, c) in &chunks {
            fs.db
                .execute(
                    "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                    rusqlite::params![h.to_vec(), c],
                )
                .unwrap();
        }
        let mc = store_file_manifest(&fs.db, &chunk_hashes, data.len() as u64, 0o644);

        // Insert a File entry into the writable root.
        let ino = fs.ino_gen.next();
        fs.entries.insert(
            ino,
            Entry::File {
                size: data.len() as u64,
                mode: 0o644,
                manifest_cid: Some(mc.to_bytes()),
                v1_2025_cid: None,
            },
        );
        if let Some(Entry::Dir { children, .. }) = fs.entries.get_mut(&ROOT_INODE) {
            children.insert("hello.txt".to_string(), ino);
        }
        fs.dirty_root = true;

        fs.commit_as_ref("main").unwrap();

        // Reopen: the file must be back in the writable root with correct size.
        let fs2 = SqliteFs::open(&path).unwrap();
        let root_children = match fs2.entries.get(&ROOT_INODE) {
            Some(Entry::Dir { children, .. }) => children,
            other => panic!("root should be a Dir, got {:?}", other.map(|_| ())),
        };
        let hello_ino = root_children
            .get("hello.txt")
            .expect("hello.txt should survive commit + reopen");
        match fs2.entries.get(hello_ino) {
            Some(Entry::File {
                size, manifest_cid, ..
            }) => {
                assert_eq!(*size, data.len() as u64);
                assert_eq!(manifest_cid.as_deref(), Some(mc.to_bytes().as_slice()));
            }
            other => panic!("hello.txt should be a File, got {:?}", other.map(|_| ())),
        }

        let _ = std::fs::remove_file(&path);
    }

    /// Mode must survive commit + reopen: it's embedded in the manifest, which
    /// is what `child_entry_from_cid` decodes on load.
    #[test]
    fn test_mode_survives_commit_and_reopen() {
        let path = temp_db_path("mode_survives.db");
        init_repo(&path).unwrap();
        let mut fs = SqliteFs::open(&path).unwrap();

        let data = b"#!/bin/sh\necho hi\n".to_vec();
        let chunks = cdc::chunk_file(&data);
        let mc = fs.store_released_file(&data, &chunks, 0o755).unwrap();

        let ino = fs.ino_gen.next();
        fs.entries.insert(
            ino,
            Entry::File {
                size: data.len() as u64,
                mode: 0o755,
                manifest_cid: Some(mc.to_bytes()),
                v1_2025_cid: None,
            },
        );
        if let Some(Entry::Dir { children, .. }) = fs.entries.get_mut(&ROOT_INODE) {
            children.insert("run.sh".to_string(), ino);
        }
        fs.dirty_root = true;
        fs.commit_as_ref("main").unwrap();

        // Reopen (mount): mode comes back from the manifest chunk.
        let fs2 = SqliteFs::open(&path).unwrap();
        let root_children = match fs2.entries.get(&ROOT_INODE) {
            Some(Entry::Dir { children, .. }) => children,
            other => panic!("root should be a Dir, got {:?}", other.map(|_| ())),
        };
        let run_ino = root_children
            .get("run.sh")
            .expect("run.sh should survive commit + reopen");
        match fs2.entries.get(run_ino) {
            Some(Entry::File { mode, .. }) => assert_eq!(*mode, 0o755),
            other => panic!("run.sh should be a File, got {:?}", other.map(|_| ())),
        }

        let _ = std::fs::remove_file(&path);
    }

    /// the manifest must decode back to the file's size.
    #[test]
    fn test_store_released_file_roundtrip() {
        let path = temp_db_path("released.db");
        init_repo(&path).unwrap();
        let fs = SqliteFs::open(&path).unwrap();

        let data = b"the quick brown fox jumps over the lazy dog".repeat(100);
        let chunks = cdc::chunk_file(&data);
        let mc = fs.store_released_file(&data, &chunks, 0o755).unwrap();

        // Manifest decodes to (size, hashes) matching the input.
        let manifest_data: Vec<u8> = fs
            .db
            .query_row(
                "SELECT data FROM chunks WHERE hash = ?1",
                rusqlite::params![mc.to_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        let (size, hashes, mode) = dag::decode_file_manifest(&manifest_data).unwrap();
        assert_eq!(size, Some(data.len() as u64));
        assert_eq!(mode, Some(0o755));
        assert_eq!(hashes, chunk_hashes(&chunks));

        // All chunks are present in the store.
        for (h, _) in &chunks {
            let present: bool = fs
                .db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM chunks WHERE hash = ?1)",
                    rusqlite::params![h.to_vec()],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(present, "chunk {} missing", hex(h));
        }

        let _ = std::fs::remove_file(&path);
    }

    /// `block_chunks` must only map LEAF blocks — intermediate dag-pb nodes cover
    /// the union of their children's spans and would duplicate every leaf's rows.
    #[test]
    fn test_block_chunks_leaves_only() {
        let path = temp_db_path("block_chunks.db");
        init_repo(&path).unwrap();
        let fs = SqliteFs::open(&path).unwrap();

        // > 1 MiB so the DAG has multiple leaves + at least one intermediate.
        let data = vec![0x42u8; 3 * 1024 * 1024];
        let chunks = cdc::chunk_file(&data);
        fs.store_released_file(&data, &chunks, 0o644).unwrap();

        let dag = dag::build_v1_2025_dag(&data);
        let leaf_cids: std::collections::HashSet<Vec<u8>> = dag
            .blocks
            .iter()
            .filter(|b| b.cid.codec() == dag::RAW)
            .map(|b| b.cid.to_bytes())
            .collect();
        assert!(
            dag.blocks.iter().any(|b| b.cid.codec() != dag::RAW),
            "test data should produce intermediate blocks"
        );

        // Every block_chunks row's v1_2025_cid must be a leaf.
        let mut stmt = fs
            .db
            .prepare("SELECT DISTINCT v1_2025_cid FROM block_chunks")
            .unwrap();
        let rows: Vec<Vec<u8>> = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(!rows.is_empty(), "expected leaf rows in block_chunks");
        for cid in &rows {
            assert!(
                leaf_cids.contains(cid),
                "block_chunks contains non-leaf block"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    /// `rebuild_index` reconstructs `directory_index` (incl. `file_mode`) from refs;
    /// dropping it must not lose the mode, and remount must still work.
    #[test]
    fn test_rebuild_index_recovers_modes() {
        let path = temp_db_path("rebuild_index.db");
        init_repo(&path).unwrap();
        let mut fs = SqliteFs::open(&path).unwrap();

        let data = b"echo hi\n".to_vec();
        let chunks = cdc::chunk_file(&data);
        let mc = fs.store_released_file(&data, &chunks, 0o755).unwrap();
        let ino = fs.ino_gen.next();
        fs.entries.insert(
            ino,
            Entry::File {
                size: data.len() as u64,
                mode: 0o755,
                manifest_cid: Some(mc.to_bytes()),
                v1_2025_cid: None,
            },
        );
        if let Some(Entry::Dir { children, .. }) = fs.entries.get_mut(&ROOT_INODE) {
            children.insert("run.sh".to_string(), ino);
        }
        fs.dirty_root = true;
        fs.commit_as_ref("main").unwrap();

        // Simulate an old repo: drop the index and rebuild it.
        fs.db.execute_batch("DROP TABLE directory_index").unwrap();
        rebuild_index(&path).unwrap();

        // Mount again: the mode must be back.
        let fs2 = SqliteFs::open(&path).unwrap();
        let run_ino = match fs2.entries.get(&ROOT_INODE) {
            Some(Entry::Dir { children, .. }) => *children
                .get("run.sh")
                .expect("run.sh should be back after rebuild"),
            _ => panic!("root should be a Dir"),
        };
        match fs2.entries.get(&run_ino) {
            Some(Entry::File { mode, .. }) => assert_eq!(*mode, 0o755),
            other => panic!("run.sh should be a File, got {:?}", other.map(|_| ())),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_empty_manifest_roundtrip() {
        let path = temp_db_path("empty.db");
        init_repo(&path).unwrap();
        let fs = SqliteFs::open(&path).unwrap();
        let bytes = store_empty_manifest(&fs.db);
        let data: Vec<u8> = fs
            .db
            .query_row(
                "SELECT data FROM chunks WHERE hash = ?1",
                rusqlite::params![&bytes],
                |r| r.get(0),
            )
            .unwrap();
        let (size, hashes, mode) = dag::decode_file_manifest(&data).unwrap();
        assert_eq!(size, Some(0));
        assert_eq!(mode, Some(0o644));
        assert!(hashes.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// Two names pointing at the same inode share one manifest CID, and the
    /// hard link is visible in the directory tree + committed DAG.
    #[test]
    fn test_link_creates_shared_inode() {
        let path = temp_db_path("link.db");
        init_repo(&path).unwrap();
        let mut fs = SqliteFs::open(&path).unwrap();

        let dir_ino = fs.ino_gen.next();
        fs.entries.insert(
            dir_ino,
            Entry::Dir {
                children: HashMap::new(),
                parent: ROOT_INODE,
            },
        );
        let file_ino = fs.ino_gen.next();
        let manifest_cid = store_empty_manifest(&fs.db);
        fs.entries.insert(
            file_ino,
            Entry::File {
                size: 0,
                mode: 0o644,
                manifest_cid: Some(manifest_cid.clone()),
                v1_2025_cid: None,
            },
        );
        let mut root = match fs.entries.get_mut(&ROOT_INODE) {
            Some(Entry::Dir { children, .. }) => children,
            _ => panic!("root should be a Dir"),
        };
        root.insert("dir".to_string(), dir_ino);
        root.insert("orig".to_string(), file_ino);

        // Hard link dir/orig -> dir/link.
        let (size, mode) = fs
            .link_entry(file_ino, dir_ino, std::ffi::OsStr::new("link"))
            .unwrap();
        assert_eq!((size, mode), (0, 0o644));
        let dir = match fs.entries.get(&dir_ino) {
            Some(Entry::Dir { children, .. }) => children,
            _ => panic!("dir should be a Dir"),
        };
        assert_eq!(dir.get("link"), Some(&file_ino));

        // Both names resolve to the same inode -> same manifest CID.
        let cid_of = |fs: &SqliteFs, parent: u64, name: &str| {
            let Some(Entry::Dir { children, .. }) = fs.entries.get(&parent) else {
                panic!("expected dir");
            };
            let ino = *children.get(name).expect("child should exist");
            match fs.entries.get(&ino) {
                Some(Entry::File {
                    manifest_cid: Some(cid),
                    ..
                }) => cid.clone(),
                other => panic!("expected file, got {:?}", other.map(|_| ())),
            }
        };
        assert_eq!(cid_of(&fs, ROOT_INODE, "orig"), manifest_cid);
        assert_eq!(cid_of(&fs, dir_ino, "link"), manifest_cid);

        // Commit and verify the DAG has both names pointing at the same CID.
        fs.dirty_root = true;
        fs.commit_as_ref("main").unwrap();
        let main_commit: Vec<u8> = fs
            .db
            .query_row(
                "SELECT commit_hash FROM refs WHERE name = 'main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let main_cid: Vec<u8> = fs
            .db
            .query_row(
                "SELECT tree_cid FROM commits WHERE hash = ?1",
                rusqlite::params![main_commit],
                |r| r.get(0),
            )
            .unwrap();
        let children_of =
            |fs: &SqliteFs, parent_cid: &[u8]| -> std::collections::HashMap<String, Vec<u8>> {
                fs.db
                    .prepare("SELECT name, child_cid FROM directory_index WHERE parent_cid = ?1")
                    .unwrap()
                    .query_map(rusqlite::params![parent_cid], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .unwrap()
                    .filter_map(Result::ok)
                    .collect()
            };
        let root_children = children_of(&fs, &main_cid);
        assert_eq!(root_children.get("orig"), Some(&manifest_cid));
        // The hard link lives inside the dir subtree and points at the same CID.
        let dir_cid = root_children.get("dir").expect("dir should be in root");
        let dir_children = children_of(&fs, dir_cid);
        assert_eq!(dir_children.get("link"), Some(&manifest_cid));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_link_rejects_non_files() {
        let path = temp_db_path("link_reject.db");
        init_repo(&path).unwrap();
        let mut fs = SqliteFs::open(&path).unwrap();

        let dir_ino = fs.ino_gen.next();
        fs.entries.insert(
            dir_ino,
            Entry::Dir {
                children: HashMap::new(),
                parent: ROOT_INODE,
            },
        );

        // Directory target -> EISDIR.
        assert_eq!(
            fs.link_entry(dir_ino, ROOT_INODE, std::ffi::OsStr::new("d")),
            Err(libc::EISDIR)
        );

        // Symlink target -> EPERM.
        let sym_ino = fs.ino_gen.next();
        fs.entries.insert(
            sym_ino,
            Entry::Symlink {
                target: "t".to_string(),
            },
        );
        assert_eq!(
            fs.link_entry(sym_ino, ROOT_INODE, std::ffi::OsStr::new("s")),
            Err(libc::EPERM)
        );

        // Missing target -> EPERM (not a file, not a dir).
        assert_eq!(
            fs.link_entry(999, ROOT_INODE, std::ffi::OsStr::new("x")),
            Err(libc::EPERM)
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Read-only snapshot dirs reject hard links; duplicates are rejected too.
    #[test]
    fn test_link_rejects_snapshot_parent_and_duplicate() {
        let path = temp_db_path("link_reject2.db");
        init_repo(&path).unwrap();
        let mut fs = SqliteFs::open(&path).unwrap();

        let file_ino = fs.ino_gen.next();
        let manifest_cid = store_empty_manifest(&fs.db);
        fs.entries.insert(
            file_ino,
            Entry::File {
                size: 0,
                mode: 0o644,
                manifest_cid: Some(manifest_cid),
                v1_2025_cid: None,
            },
        );
        let mut root = match fs.entries.get_mut(&ROOT_INODE) {
            Some(Entry::Dir { children, .. }) => children,
            _ => panic!("root should be a Dir"),
        };
        root.insert("orig".to_string(), file_ino);

        // .sqlitefs/refs is read-only.
        assert_eq!(
            fs.link_entry(file_ino, SNAPSHOT_REFS_INODE, std::ffi::OsStr::new("x")),
            Err(libc::EROFS)
        );
        // Duplicate name -> EEXIST.
        assert_eq!(
            fs.link_entry(file_ino, ROOT_INODE, std::ffi::OsStr::new("orig")),
            Err(libc::EEXIST)
        );

        let _ = std::fs::remove_file(&path);
    }

    fn chunk_hashes(chunks: &[([u8; 32], Vec<u8>)]) -> Vec<[u8; 32]> {
        chunks.iter().map(|(h, _)| *h).collect()
    }

    fn hex(b: &[u8; 32]) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(64);
        for x in b {
            let _ = write!(s, "{x:02x}");
        }
        s
    }
}

#[cfg(test)]
mod cli_ro_tests {
    use std::path::PathBuf;

    use crate::{
        cdc,
        cli::{Node, init_repo},
        fs::{Entry, ROOT_INODE, SqliteFs},
    };

    fn temp_db_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sqlitefs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Build a repo with: main ref = dir containing hello.txt (file), sub/
    /// (dir with run.sh), link.txt (symlink to hello.txt); plus a second ref
    /// "v1" pointing at an empty tree.
    fn build_repo(path: &PathBuf) {
        init_repo(path).unwrap();
        let mut fs = SqliteFs::open(path).unwrap();

        // hello.txt
        let data = b"hello emacs\n".to_vec();
        let chunks = cdc::chunk_file(&data);
        let mc = fs.store_released_file(&data, &chunks, 0o644).unwrap();
        let f_ino = fs.ino_gen.next();
        fs.entries.insert(
            f_ino,
            Entry::File {
                size: data.len() as u64,
                mode: 0o644,
                manifest_cid: Some(mc.to_bytes()),
                v1_2025_cid: None,
            },
        );
        if let Some(Entry::Dir { children, .. }) = fs.entries.get_mut(&ROOT_INODE) {
            children.insert("hello.txt".to_string(), f_ino);
        }

        // sub/run.sh
        let script = b"#!/bin/sh\necho hi\n".to_vec();
        let chunks = cdc::chunk_file(&script);
        let mc = fs.store_released_file(&script, &chunks, 0o755).unwrap();
        let s_ino = fs.ino_gen.next();
        fs.entries.insert(
            s_ino,
            Entry::File {
                size: script.len() as u64,
                mode: 0o755,
                manifest_cid: Some(mc.to_bytes()),
                v1_2025_cid: None,
            },
        );
        let sub_ino = fs.ino_gen.next();
        fs.entries.insert(
            sub_ino,
            Entry::Dir {
                children: std::collections::HashMap::from([("run.sh".to_string(), s_ino)]),
                parent: ROOT_INODE,
            },
        );
        if let Some(Entry::Dir { children, .. }) = fs.entries.get_mut(&ROOT_INODE) {
            children.insert("sub".to_string(), sub_ino);
        }

        // link.txt -> hello.txt (commit_as_ref stores the symlink manifest)
        let sym_ino = fs.ino_gen.next();
        fs.entries.insert(
            sym_ino,
            Entry::Symlink {
                target: "hello.txt".to_string(),
            },
        );
        if let Some(Entry::Dir { children, .. }) = fs.entries.get_mut(&ROOT_INODE) {
            children.insert("link.txt".to_string(), sym_ino);
        }

        fs.dirty_root = true;
        fs.commit_as_ref("main").unwrap();

        // v1 = empty tree on top of nothing (new commit, empty dir).
        let mut fs2 = SqliteFs::open(path).unwrap();
        fs2.dirty_root = true;
        fs2.commit_as_ref("v1").unwrap();
    }

    #[test]
    fn test_resolve_ref_ro_root_is_dir() {
        let path = temp_db_path("resolve_root.db");
        build_repo(&path);
        let db = rusqlite::Connection::open(&path).unwrap();
        let node = crate::cli::resolve_ref_ro(&db, "main", "").unwrap();
        match node {
            Node::Dir(links) => {
                let names: Vec<&str> = links.iter().map(|(n, _)| n.as_str()).collect();
                assert!(names.contains(&"hello.txt"));
                assert!(names.contains(&"sub"));
                assert!(names.contains(&"link.txt"));
            }
            _ => panic!("root should be a dir"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_resolve_ref_ro_file_size_and_nested_dir() {
        let path = temp_db_path("resolve_file.db");
        build_repo(&path);
        let db = rusqlite::Connection::open(&path).unwrap();
        match crate::cli::resolve_ref_ro(&db, "main", "hello.txt").unwrap() {
            Node::File { size, manifest } => {
                assert_eq!(size, 12);
                assert!(!manifest.is_empty());
            }
            _ => panic!("hello.txt should be a file"),
        }
        match crate::cli::resolve_ref_ro(&db, "main", "sub/run.sh").unwrap() {
            Node::File { size, .. } => assert_eq!(size, 18),
            _ => panic!("sub/run.sh should be a file"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_resolve_ref_ro_symlink() {
        let path = temp_db_path("resolve_symlink.db");
        build_repo(&path);
        let db = rusqlite::Connection::open(&path).unwrap();
        match crate::cli::resolve_ref_ro(&db, "main", "link.txt").unwrap() {
            Node::Symlink(target) => assert_eq!(target, "hello.txt"),
            _ => panic!("link.txt should be a symlink"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_resolve_ref_ro_other_ref_and_errors() {
        let path = temp_db_path("resolve_refs.db");
        build_repo(&path);
        let db = rusqlite::Connection::open(&path).unwrap();

        // v1 was committed on top of main (reload keeps the tree), so it has
        // the same files — proving an arbitrary (non-main) ref is walkable.
        match crate::cli::resolve_ref_ro(&db, "v1", "hello.txt").unwrap() {
            Node::File { size, .. } => assert_eq!(size, 12),
            _ => panic!("v1 hello.txt should be a file"),
        }

        // Missing ref and missing path must error, not panic.
        assert!(crate::cli::resolve_ref_ro(&db, "nope", "").is_err());
        assert!(crate::cli::resolve_ref_ro(&db, "main", "no/such/file").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_file_roundtrip() {
        let path = temp_db_path("read_file.db");
        build_repo(&path);
        let db = rusqlite::Connection::open(&path).unwrap();
        let manifest = match crate::cli::resolve_ref_ro(&db, "main", "hello.txt").unwrap() {
            Node::File { manifest, .. } => manifest,
            _ => panic!("expected file"),
        };
        let content = crate::cli::read_file(&db, &manifest).unwrap();
        assert_eq!(content, b"hello emacs\n");
        let _ = std::fs::remove_file(&path);
    }
}
