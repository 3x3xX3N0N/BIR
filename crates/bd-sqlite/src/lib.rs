//! SQLite backend.
//!
//! A complete beads store with no commit graph. Implementation lands next; this
//! is the public shape the CLI compiles against.

use bd_storage::{Backend, Identity, Locator, Result, Storage};
use std::path::Path;

/// Create a new workspace at `dir` and return an open store.
pub async fn init(_dir: &Path, _prefix: &str, _identity: Identity) -> Result<Box<dyn Storage>> {
    Err(bd_storage::Error::unsupported("init", "sqlite"))
}

/// Open the store described by `locator`.
pub async fn open(_locator: &Locator, _identity: Identity) -> Result<Box<dyn Storage>> {
    Err(bd_storage::Error::unsupported("open", "sqlite"))
}

pub const BACKEND: Backend = Backend::Sqlite;
