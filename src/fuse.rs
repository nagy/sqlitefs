//! FUSE frontend: the `fuser::Filesystem` trait impl on [`crate::fs::SqliteFs`].
//!
//! Writable root plus the read-only `.sqlitefs/` snapshot namespace.

use std::{collections::HashMap, ffi::OsStr, time::UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, ReplyXattr, Request,
};
use libc::{ENOENT, ENOTDIR};

use crate::{
    cdc, dag,
    fs::{
        DirtyFile, Entry, ROOT_INODE, SNAPSHOT_DIR_INODE, SNAPSHOT_HASH_INODE, SNAPSHOT_REFS_INODE,
        SNAPSHOT_TAG_INODE, SqliteFs,
    },
};

fn make_attr(inode: u64, kind: FileType, size: u64, mode: Option<u32>) -> FileAttr {
    let (perm, nlink) = match kind {
        FileType::Directory => (0o755, 2),
        FileType::Symlink => (0o777, 1),
        _ => (mode.unwrap_or(0o644) as u16, 1),
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

impl fuser::Filesystem for SqliteFs {
    /// Persist any dirty state on unmount.
    ///
    /// Commits the dirty root so nothing is lost when the filesystem is unmounted.
    fn destroy(&mut self) {
        if self.dirty_root {
            let _ = self.commit_dirty_tree();
        }
    }

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
                } else if let Some(ino) = Self::resolve_hash_prefix(
                    &self.db,
                    &mut self.ino_gen,
                    &mut self.entries,
                    &hex,
                    SNAPSHOT_HASH_INODE,
                ) {
                    // Cache the name→ino mapping in .sqlitefs/c/.
                    if let Some(Entry::Dir { children, .. }) =
                        self.entries.get_mut(&SNAPSHOT_HASH_INODE)
                    {
                        children.insert(name_owned.clone(), ino);
                    }
                    ino
                } else {
                    reply.error(ENOENT);
                    return;
                }
            };
            let Some(child) = self.entries.get(&target_ino) else {
                reply.error(ENOENT);
                return;
            };
            let (kind, size, mode) = match child {
                Entry::Dir { .. } => (FileType::Directory, 0, None),
                Entry::Symlink { .. } => (FileType::Symlink, 0, None),
                Entry::File { size, mode, .. } => (FileType::RegularFile, *size, Some(*mode)),
            };
            reply.entry(
                &std::time::Duration::MAX,
                &make_attr(target_ino, kind, size, mode),
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
            let (kind, size, mode) = match child {
                Entry::Dir { .. } => (FileType::Directory, 0, None),
                Entry::Symlink { .. } => (FileType::Symlink, 0, None),
                Entry::File { size, mode, .. } => (FileType::RegularFile, *size, Some(*mode)),
            };
            reply.entry(
                &std::time::Duration::MAX,
                &make_attr(child_ino, kind, size, mode),
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
        let (kind, size, mode) = match entry {
            Entry::Dir { .. } => (FileType::Directory, 0, None),
            Entry::Symlink { .. } => (FileType::Symlink, 0, None),
            Entry::File { size, mode, .. } => (FileType::RegularFile, *size, Some(*mode)),
        };
        reply.attr(&std::time::Duration::MAX, &make_attr(ino, kind, size, mode));
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
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
        // Truncate-to-zero and chmod are implemented; uid/gid/utimes are ignored.
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
                ..
            }) = self.entries.get_mut(&ino)
            {
                *s = 0;
                *manifest_cid = Some(empty_manifest);
                *v1_2025_cid = None;
                // Clear any open file buffers for this inode (don't remove them;
                // a write may follow from the same open file handle, e.g. echo > file).
                for ((i, _), df) in &mut self.open_files {
                    if *i == ino {
                        df.data.clear();
                    }
                }
                self.dirty_root = true;
                self.mark_dirty();
            }
            // ponytail: non-zero truncation on committed files not implemented
        }
        // Apply chmod (mode & 0o7777) to regular files. The mode lives in the
        // manifest, so re-store it with the new mode (new manifest CID → the
        // commit links it, and snapshots/remounts see the exec bit). Other kinds
        // keep their hardcoded perms. Mark dirty so the mode survives a commit.
        if let Some(m) = mode
            && let Some(Entry::File {
                mode: f,
                manifest_cid,
                ..
            }) = self.entries.get_mut(&ino)
        {
            let new_mode = m & 0o7777;
            if *f != new_mode {
                *f = new_mode;
                // Re-store the manifest with the new mode if we can decode the old one.
                if let Some(old_cid) = manifest_cid.clone()
                    && let Ok(old_data) = self.db.query_row(
                        "SELECT data FROM chunks WHERE hash = ?1",
                        rusqlite::params![old_cid],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    && let Some((new_data, new_cid)) =
                        dag::reencode_manifest_with_mode(&old_data, new_mode)
                {
                    let _ = self.db.execute(
                        "INSERT OR IGNORE INTO chunks (hash, data) VALUES (?1, ?2)",
                        rusqlite::params![new_cid.to_bytes(), new_data],
                    );
                    *manifest_cid = Some(new_cid.to_bytes());
                }
                self.dirty_root = true;
                self.mark_dirty();
            }
        }
        let (kind, s, m) = match self.entries.get(&ino) {
            Some(Entry::Dir { .. }) => (FileType::Directory, 0, None),
            Some(Entry::Symlink { .. }) => (FileType::Symlink, 0, None),
            Some(Entry::File { size, mode, .. }) => (FileType::RegularFile, *size, Some(*mode)),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        reply.attr(&std::time::Duration::MAX, &make_attr(ino, kind, s, m));
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
        self.mark_dirty();
        reply.entry(
            &std::time::Duration::MAX,
            &make_attr(ino, FileType::Directory, 0, None),
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
        self.mark_dirty();
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
                    mode: 0o644,
                    manifest_cid: None,
                    v1_2025_cid: None,
                },
            );
            let fh = self.next_fh;
            self.next_fh += 1;
            self.open_files.insert((ino, fh), DirtyFile::default());
            reply.created(
                &std::time::Duration::MAX,
                &make_attr(ino, FileType::RegularFile, 0, Some(0o644)),
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
                mode: 0o644,
                manifest_cid: Some(empty_manifest),
                v1_2025_cid: None,
            },
        );

        let fh = self.next_fh;
        self.next_fh += 1;
        self.open_files.insert((ino, fh), DirtyFile::default());

        reply.created(
            &std::time::Duration::MAX,
            &make_attr(ino, FileType::RegularFile, 0, Some(0o644)),
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
        // Negative or huge offsets would make `off as usize` wrap and resize a
        // gigantic buffer (OOM/panic). Reject them before use.
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
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

                // Preserve the file's mode across a re-write (chmod then edit).
                let cur_mode = match self.entries.get(&ino) {
                    Some(Entry::File { mode, .. }) => *mode,
                    _ => 0o644,
                };

                // All chunk/manifest/block writes happen in ONE transaction so a crash
                // mid-write cannot leave a partial file (some chunks but no manifest).
                let store_result = self.store_released_file(&df.data, &chunks, cur_mode);
                let Ok(mc) = store_result else {
                    reply.error(libc::EIO);
                    return;
                };
                let v1_dag = dag::build_v1_2025_dag(&df.data);

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
            // Commit is debounced: the auto-commit timer, a flush/fsync, or unmount
            // persists the write. Committing here would be O(entire repo) per op.
            self.mark_dirty();
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
        self.mark_dirty();
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
            if let Some((_, hashes, _)) = dag::decode_file_manifest(&manifest_data) {
                // Assemble only the chunks overlapping [off, end), not the whole file.
                // Walk cumulative offsets using cheap length() queries; fetch full
                // data only for chunks that intersect the requested window.
                let end = off.saturating_add(size as usize);
                let mut assembled = Vec::new();
                let mut pos: usize = 0;
                for h in &hashes {
                    let Ok(len) = self.db.query_row(
                        "SELECT length(data) FROM chunks WHERE hash = ?1",
                        rusqlite::params![h.to_vec()],
                        |r| r.get::<_, i64>(0),
                    ) else {
                        // Missing chunk: skip (best effort, matches old behavior).
                        continue;
                    };
                    let len = len.max(0) as usize;
                    let chunk_start = pos;
                    let chunk_end = pos + len;
                    pos = chunk_end;
                    if chunk_end <= off || chunk_start >= end {
                        continue;
                    }
                    if let Ok(chunk_data) = self.db.query_row(
                        "SELECT data FROM chunks WHERE hash = ?1",
                        rusqlite::params![h.to_vec()],
                        |r| r.get::<_, Vec<u8>>(0),
                    ) {
                        let s = off.saturating_sub(chunk_start).min(len);
                        let e = end.saturating_sub(chunk_start).min(len);
                        assembled.extend_from_slice(&chunk_data[s..e]);
                    }
                    if pos >= end {
                        break;
                    }
                }
                reply.data(&assembled);
                return;
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
        self.mark_dirty();
        reply.entry(
            &std::time::Duration::MAX,
            &make_attr(ino, FileType::Symlink, 0, None),
            0,
        );
    }

    /// Create a hard link at a new directory entry pointing at the same inode.
    ///
    /// The child keeps its manifest CID, so the DAG dedups and `st_ino` identity
    /// follows the content. Files only — dirs/symlinks are rejected (POSIX forbids
    /// hard-linking directories).
    fn link(
        &mut self,
        _req: &Request,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        match self.link_entry(ino, newparent, newname) {
            Ok((size, mode)) => reply.entry(
                &std::time::Duration::MAX,
                &make_attr(ino, FileType::RegularFile, size, Some(mode)),
                0,
            ),
            Err(err) => reply.error(err),
        }
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
        self.mark_dirty();
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

    fn forget(&mut self, _req: &Request, _ino: u64, _nlookup: u64) {
        // No-op. The kernel will never send another request referencing a forgotten
        // inode (unless it is re-looked-up), so there is no need to evict it.
        // Evicting would corrupt the parent's `children` map (a name would point at
        // a missing Entry), causing lookup ENOENT, mkdir EEXIST, and readdir panics
        // (self.entries[&child_ino]). The whole tree is resident in `entries` anyway.
    }
}

impl SqliteFs {
    /// Core hard-link logic, extracted for testing.
    ///
    /// Returns the linked file's `(size, mode)` for the reply, or a libc errno.
    pub(crate) fn link_entry(
        &mut self,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
    ) -> Result<(u64, u32), i32> {
        let Some(newname_str) = newname.to_str() else {
            return Err(libc::EINVAL);
        };
        if newparent == SNAPSHOT_DIR_INODE
            || newparent == SNAPSHOT_REFS_INODE
            || newparent == SNAPSHOT_TAG_INODE
            || newparent == SNAPSHOT_HASH_INODE
        {
            return Err(libc::EROFS);
        }
        if !matches!(self.entries.get(&ino), Some(Entry::File { .. })) {
            // ext4: EPERM for symlinks, EISDIR for directories.
            let err = match self.entries.get(&ino) {
                Some(Entry::Dir { .. }) => libc::EISDIR,
                _ => libc::EPERM,
            };
            return Err(err);
        }
        let Some(Entry::Dir { children, .. }) = self.entries.get_mut(&newparent) else {
            return Err(ENOTDIR);
        };
        if children.contains_key(newname_str) {
            return Err(libc::EEXIST);
        }
        children.insert(newname_str.to_string(), ino);
        self.dirty_root = true;
        self.mark_dirty();
        let (size, mode) = match self.entries.get(&ino) {
            Some(Entry::File { size, mode, .. }) => (*size, *mode),
            _ => (0, 0o644),
        };
        Ok((size, mode))
    }
}
