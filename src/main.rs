use std::path::PathBuf;

use clap::{Parser, Subcommand};

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

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Init { path } => init_repo(&path),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
