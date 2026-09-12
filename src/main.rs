//! sqlitefs — FUSE mount entry point.
//!
//! The library crate ([`sqlitefs`]) holds the core and the FUSE trait impl
//! (`sqlitefs::fuse`); this binary only wires the CLI to the commands.

use std::{os::fd::AsRawFd, path::PathBuf};

use clap::{CommandFactory, Parser};
use fuser::MountOption;
use sqlitefs::{cli, fs::SqliteFs};

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// Mount a repo at `mount_point`, taking an exclusive flock for the lifetime.
fn mount_fs(db_path: &PathBuf, mount_point: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // Take an exclusive flock on the DB file for the mount's lifetime so two
    // processes can't mount the same repo concurrently (which would corrupt it).
    let lock = std::fs::File::open(db_path)?;
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("repo is already mounted elsewhere: {err}").into());
    }

    let fs = SqliteFs::open(db_path)?;
    let result = fuser::mount2(
        fs,
        mount_point,
        &[
            MountOption::FSName("sqlitefs".into()),
            MountOption::AutoUnmount,
            MountOption::AllowRoot,
            MountOption::DefaultPermissions,
        ],
    )
    .map_err(|e| format!("mount failed (is /dev/fuse accessible?): {e}").into());
    // lock is released when it drops at end of scope
    result
}

/// Parse the CLI and dispatch to the requested command.
fn main() {
    let cli = cli::Cli::parse();

    let result = match cli.command {
        cli::Command::Init { path } => cli::init_repo(&path),
        cli::Command::Mount {
            db_path,
            mount_point,
        } => mount_fs(&db_path, &mount_point),
        cli::Command::Fsck { db_path } => cli::fsck_repo(&db_path),
        cli::Command::ShowRef { db_path } => cli::show_ref(&db_path),
        cli::Command::RebuildIndex { db_path } => cli::rebuild_index(&db_path),
        cli::Command::Ls { db_path, path } => cli::ls(&db_path, path.as_deref()),
        cli::Command::Cat { db_path, path } => cli::cat(&db_path, &path),
        cli::Command::Cp {
            db_path,
            ref src,
            ref dst,
        } => cli::cp(&db_path, src, dst),
        cli::Command::Mv {
            db_path,
            ref src,
            ref dst,
        } => cli::mv(&db_path, src, dst),
        cli::Command::Completions { shell } => {
            let mut cmd = cli::Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, &name, &mut std::io::stdout());
            return;
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
