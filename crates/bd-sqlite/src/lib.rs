//! SQLite backend.
//!
//! A complete beads store with no commit graph. It implements the whole core
//! seam and none of the capability traits — see rule 4 in [`bd_storage`]: a
//! SQLite workspace is not a degraded Dolt workspace, it is a workspace that
//! cannot branch.
//!
//! The interesting code is in [`blocked`]. Everything else is bookkeeping.

pub mod blocked;
mod rows;
mod sqlfilter;
mod store;

#[cfg(test)]
mod tests;

use bd_storage::{Backend, Error, Identity, Locator, Result, Storage};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::path::Path;
use std::time::Duration;

pub use store::SqliteStore;

pub const BACKEND: Backend = Backend::Sqlite;

/// Config key holding the workspace's issue-id prefix (`bd`, in `bd-a3f2dd`).
pub const PREFIX_KEY: &str = "issue.prefix";

const SCHEMA: &str = include_str!("schema.sql");

/// Create a new workspace at `dir` and return an open store.
///
/// Idempotent: re-running against an existing workspace reuses its
/// `workspace_id` rather than minting a new one. That id is how clones recognize
/// each other, so quietly rotating it on a second `bd init` would fork a
/// workspace from itself.
pub async fn init(dir: &Path, prefix: &str, identity: Identity) -> Result<Box<dyn Storage>> {
    let beads_dir = dir.join(bd_storage::locator::BEADS_DIR);
    std::fs::create_dir_all(&beads_dir)
        .map_err(|e| Error::Db(format!("could not create {}: {e}", beads_dir.display())))?;

    let locator = match Locator::load(&beads_dir) {
        Ok(existing) => existing,
        Err(_) => Locator::new(Backend::Sqlite, uuid::Uuid::new_v4().to_string(), &beads_dir),
    };
    locator.save()?;

    let pool = connect(&locator.db_path()).await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await.map_err(db)?;

    sqlx::query(
        "INSERT INTO config (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(PREFIX_KEY)
    .bind(prefix)
    .execute(&pool)
    .await
    .map_err(db)?;

    Ok(Box::new(SqliteStore::new(pool, identity)))
}

/// Open the store described by `locator`.
pub async fn open(locator: &Locator, identity: Identity) -> Result<Box<dyn Storage>> {
    if locator.backend != Backend::Sqlite {
        return Err(Error::Db(format!(
            "workspace is a {} store, not sqlite",
            locator.backend
        )));
    }
    let path = locator.db_path();
    if !path.exists() {
        return Err(Error::NoWorkspace);
    }
    Ok(Box::new(SqliteStore::new(connect(&path).await?, identity)))
}

/// One connection, deliberately.
///
/// SQLite has exactly one writer. A larger pool does not buy write parallelism;
/// it buys `SQLITE_BUSY` — and in WAL mode a second concurrent write transaction
/// fails *immediately* with `SQLITE_BUSY_SNAPSHOT`, which `busy_timeout` does
/// not retry. Capping the pool at one makes in-process transactions queue rather
/// than race. The timeout is left in place for the writer we cannot serialize:
/// a second `bd` process on the same file.
async fn connect(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10));

    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(db)
}

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}
