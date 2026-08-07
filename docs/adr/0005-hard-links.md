# Hard links for file objects (link for files)

We decided to implement the FUSE `link` operation for regular files: a hard
link creates a new directory entry pointing at the *same inode*, i.e. the same
`Entry::File` object, which shares the same manifest CID.

**Why**: This is the lazy way to get the "same content ⇒ same inode" property
that a content-addressed filesystem wants, without making inode numbers
deterministic functions of CIDs. It is opt-in (identity exists where the user
creates it), kernel-sanctioned (`st_ino`/`-samefile` work without any
collision tables or hash-to-inode mapping), and free at commit time: the DAG
builder already emits a dag-pb link for each `(name, ino)` pair, and two names
pointing at the same CID is native dag-pb. A `ln` of identical content
therefore also *dedups at the storage level*.

**Why not CID-derived inodes**: mapping CIDs into the 63-bit inode space needs
either a hash (collision handling) or a `cid_inode` table; shared directory
inodes would break the single-`parent` invariant that `readdir`, `rename`, and
`rmdir` rely on, and would confuse dedup-detecting tools (rsync, `du`, some
`find` implementations treat `dev+ino` identity as "same directory"). POSIX
forbids hard-linking directories, so `link` avoids all of that.

**Trade-off**: nlink is reported as 1 even when a file has multiple links.
Correct link counts would require tracking `nlink` per inode in the entry
tree (and in the DAG's dag-pb Link entries, which carry a `Tsize` but no link
count — that is per-manifest metadata). Accepted: `ls -l` link counts are
rarely relied on in versioned snapshot trees; we can add a `link_count` field
later if it matters.

**Scope**: regular files only (including inside `.sqlitefs/refs/*` and the
writable root). Directories and symlinks are rejected with `EISDIR`/`EPERM`
respectively, matching ext4 behavior. Snapshot dirs themselves remain
read-only: creating a link *into* `.sqlitefs/refs`, `.sqlitefs/tag`,
`.sqlitefs/c` returns `EROFS`.
