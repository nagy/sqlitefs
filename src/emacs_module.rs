//! Emacs dynamic module frontend — read-only access to a repo, no FUSE mount.
//!
//! Exposes the repo to Elisp as native functions (prefix `sqlitefs-`):
//!
//! - `(sqlitefs-refs PATH)` → alist of `(REF-NAME . COMMIT-CID)`.
//! - `(sqlitefs-ls PATH DIR REF)` → list of `(NAME KIND SIZE MODE)`; REF nil means the default ref
//!   `main`.
//! - `(sqlitefs-read PATH FILE REF)` → file content (UTF-8 string).
//! - `(sqlitefs-log PATH REF)` → list of `(COMMIT-CID . TIMESTAMP)`, newest first, walking the
//!   parent chain of REF (nil means `main`).
//! - `(sqlitefs-catalog)` → list of `(NAME ARGS DOC)` lists, one per export.
//!
//! Defun args are all required (the emacs-macros contract registers a fixed
//! arity); pass nil to get the documented default. The elisp frontend wraps
//! these defaults for interactive use.
//!
//! Every call opens the `SQLite` file in read-only mode and resolves the
//! committed tree — no in-memory state, no writes, safe next to a live FUSE
//! mount or concurrent CLI use. Load with `(module-load "libsqlitefs.so")`
//! (built with `cargo build --features emacs`).
//!
//! # Catalog
//!
//! [`CATALOG`] lists every exported function (Lisp name, argument names, doc
//! first line) and is exposed to Lisp via `sqlitefs-catalog`. The ERT parity
//! test (`test/emacs-module-tests.el`) pins each entry to the actual module
//! surface. Add a catalog entry together with its `#[defun]`.
//!
//! Panics: the module only returns `Result`s; no unwinding crosses the FFI
//! boundary in the code below.

// Required by Emacs's dynamic-module loader (GPL compatibility check).
emacs::plugin_is_GPL_compatible!();

use emacs::{Env, Result, Value, defun};

use crate::{
    cli::{self, Node},
    dag::{self, DAG_CBOR},
};

#[emacs::module(
    name = "sqlitefs-module",
    defun_prefix = "sqlitefs",
    mod_in_name = false
)]
#[allow(clippy::unnecessary_wraps)] // the macro wraps `init` in a FFI callback
fn init(_: &Env) -> Result<()> {
    Ok(())
}

/// Open the repo DB read-only. Every defun is stateless: it opens, queries,
/// and drops the connection, so nothing can go stale after a commit.
fn open_ro(path: &str) -> Result<rusqlite::Connection> {
    Ok(rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

/// Render raw commit-hash bytes as a base32 `CIDv1` string (`bafyrei…`).
fn commit_cid_str(hash: &[u8]) -> Result<String> {
    let cid = dag::Cid::from_raw(DAG_CBOR, hash[4..].try_into()?);
    Ok(cid.to_string_base32())
}

/// One exported function — single source of truth for the `sqlitefs-*` Lisp API.
///
/// ARGS lists the Elisp argument names in order; `&optional` appears verbatim
/// where the defun takes optional arguments. The ERT parity test pins every
/// entry to the actual module surface: `fboundp`, argument count, and doc
/// first line. Add an entry together with its `#[defun]`.
struct CatalogEntry {
    name: &'static str,
    args: &'static [&'static str],
    doc: &'static str,
}

const fn entry(
    name: &'static str,
    args: &'static [&'static str],
    doc: &'static str,
) -> CatalogEntry {
    CatalogEntry { name, args, doc }
}

/// The catalog: one entry per `#[defun]` in this module, DOC verbatim from
/// the defun's doc-comment first line.
const CATALOG: &[CatalogEntry] = &[
    entry(
        "sqlitefs-refs",
        &["path"],
        "`(sqlitefs-refs PATH)` — all refs as an alist `(NAME . COMMIT-CID)`.",
    ),
    entry(
        "sqlitefs-ls",
        &["path", "dir", "ref"],
        "`(sqlitefs-ls PATH DIR REF)` — list DIR as `(NAME KIND SIZE MODE)`.",
    ),
    entry(
        "sqlitefs-read",
        &["path", "file", "ref"],
        "`(sqlitefs-read PATH FILE REF)` — FILE's content as a string.",
    ),
    entry(
        "sqlitefs-log",
        &["path", "ref"],
        "`(sqlitefs-log PATH REF)` — commit history as `(CID . TIMESTAMP)`.",
    ),
    entry(
        "sqlitefs-catalog",
        &[],
        "`(sqlitefs-catalog)` — the exported-function catalog as `(NAME ARGS DOC)` lists.",
    ),
];

/// Convert a string slice list into a Lisp list.
fn string_list<'e>(env: &'e Env, names: &[&'static str]) -> Result<Value<'e>> {
    let mut list = env.intern("nil")?;
    for name in names.iter().rev() {
        list = env.cons(*name, list)?;
    }
    Ok(list)
}

/// `(sqlitefs-catalog)` — the exported-function catalog as `(NAME ARGS DOC)` lists.
///
/// Single source of truth for the Lisp API surface; see [`CATALOG`].
#[defun(name = "catalog")]
fn catalog(env: &Env) -> Result<Value<'_>> {
    let mut list = env.intern("nil")?;
    for e in CATALOG.iter().rev() {
        let row = env.list((e.name, string_list(env, e.args)?, e.doc))?;
        list = env.cons(row, list)?;
    }
    Ok(list)
}

/// `(sqlitefs-refs PATH)` — all refs as an alist `(NAME . COMMIT-CID)`.
///
/// # Errors
/// When the repo cannot be opened or a commit hash is malformed.
#[defun(name = "refs")]
#[allow(clippy::needless_pass_by_value)] // `#[defun]` converts elisp args by value
fn refs(env: &Env, path: String) -> Result<Value<'_>> {
    let db = open_ro(&path)?;
    let mut list = env.intern("nil")?;
    for (name, hash) in cli::list_refs(&db)
        .map_err(|e| emacs::Error::msg(e.to_string()))?
        .into_iter()
        .rev()
    {
        let cid = commit_cid_str(&hash)?;
        let pair = env.cons(name.as_str(), cid.as_str())?;
        list = env.cons(pair, list)?;
    }
    Ok(list)
}

/// `(sqlitefs-ls PATH DIR REF)` — list DIR as `(NAME KIND SIZE MODE)`.
///
/// KIND is `dir`, `file`, or `symlink`; MODE is the octal permission bits for
/// files, nil otherwise. REF nil means the default ref `main`.
///
/// # Errors
/// When the repo, ref, or path cannot be resolved, or DIR is not a directory.
#[defun(name = "ls")]
#[allow(clippy::needless_pass_by_value)] // `#[defun]` converts elisp args by value
fn ls(env: &Env, path: String, dir: String, ref_: Option<String>) -> Result<Value<'_>> {
    let db = open_ro(&path)?;
    let ref_name = ref_.as_deref().unwrap_or("main");
    let node = cli::resolve_ref_ro(&db, ref_name, &dir).map_err(emacs::Error::msg)?;
    let Node::Dir(links) = node else {
        return Err(emacs::Error::msg(format!("not a directory: {dir}")));
    };
    let mut list = env.intern("nil")?;
    for (name, cid) in links.iter().rev() {
        let data = cli::load_chunk(&db, cid).map_err(emacs::Error::msg)?;
        let (kind, size, mode) = match cli::classify(&db, cid, data).map_err(emacs::Error::msg)? {
            Node::Dir(_) => ("dir", 0, None),
            Node::File { size, manifest } => {
                let mode = dag::decode_file_manifest(&manifest).and_then(|(_, _, m)| m);
                ("file", size, mode)
            }
            Node::Symlink(_) => ("symlink", 0, None),
        };
        list = env.cons(env.list((name.as_str(), kind, size, mode))?, list)?;
    }
    Ok(list)
}

/// `(sqlitefs-read PATH FILE REF)` — FILE's content as a string.
///
/// REF nil means the default ref `main`. Content must be valid UTF-8 (Elisp
/// strings are); binary files error out.
///
/// # Errors
/// When the path does not resolve to a file, or the content is not UTF-8.
#[defun(name = "read")]
#[allow(clippy::needless_pass_by_value)] // `#[defun]` converts elisp args by value
fn read(path: String, file: String, ref_: Option<String>) -> Result<String> {
    let db = open_ro(&path)?;
    let ref_name = ref_.as_deref().unwrap_or("main");
    let node = cli::resolve_ref_ro(&db, ref_name, &file).map_err(emacs::Error::msg)?;
    let Node::File { manifest, .. } = node else {
        return Err(emacs::Error::msg(format!("not a file: {file}")));
    };
    let bytes = cli::read_file(&db, &manifest).map_err(emacs::Error::msg)?;
    String::from_utf8(bytes).map_err(|_| emacs::Error::msg(format!("not valid UTF-8: {file}")))
}

/// `(sqlitefs-log PATH REF)` — commit history as `(CID . TIMESTAMP)`.
///
/// Walks the parent chain of REF (nil means the default ref `main`), newest
/// first.
///
/// # Errors
/// When the ref is missing or the commit chain is broken.
#[defun(name = "log")]
#[allow(clippy::needless_pass_by_value)] // `#[defun]` converts elisp args by value
fn log(env: &Env, path: String, ref_: Option<String>) -> Result<Value<'_>> {
    let db = open_ro(&path)?;
    let ref_name = ref_.as_deref().unwrap_or("main");
    let mut hash: Vec<u8> = db
        .query_row(
            "SELECT commit_hash FROM refs WHERE name = ?1",
            rusqlite::params![ref_name],
            |r| r.get(0),
        )
        .map_err(emacs::Error::msg)?;
    let mut list = env.intern("nil")?;
    loop {
        let (parent, timestamp): (Option<Vec<u8>>, u64) = db
            .query_row(
                "SELECT parent, timestamp FROM commits WHERE hash = ?1",
                rusqlite::params![hash],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .map_err(emacs::Error::msg)?;
        let cid = commit_cid_str(&hash)?;
        list = env.cons(env.cons(cid.as_str(), timestamp)?, list)?;
        match parent {
            Some(p) => hash = p,
            None => break,
        }
    }
    Ok(list)
}
