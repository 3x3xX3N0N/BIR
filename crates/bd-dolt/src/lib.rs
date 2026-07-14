//! The Dolt backend: a beads store with a commit graph.
//!
//! # What this crate is not
//!
//! It is **not** a Dolt binding. Dolt is a large Go program; Rust cannot link it
//! and there is no C API worth having. This crate contains no database.
//!
//! # What it is
//!
//! Three ordinary Rust things pointed at an external `dolt` binary:
//!
//! 1. [`server`] spawns `dolt sql-server` as a **subprocess** and supervises it.
//! 2. [`store`] talks to that server over the **MySQL wire protocol** (`sqlx`),
//!    because Dolt is MySQL-compatible. Reading and writing issues is just SQL.
//! 3. [`vc`] does branching, merging, and remotes by **calling SQL stored
//!    procedures** — `CALL DOLT_COMMIT()`, `CALL DOLT_MERGE()`, `CALL DOLT_PUSH()`.
//!
//! Point 3 is what makes this port possible at all. Dolt's version control is not
//! a Go API somebody would have to reimplement — it is exposed *as SQL*, over the
//! same connection as everything else. Upstream beads reaches it exactly this
//! way, which is why upstream also ships a build with no Go-linked Dolt.
//!
//! # What this backend unlocks, and what it can break
//!
//! SQLite returns `None` from the capability accessors; this backend returns
//! `Some`. That is the whole difference. `bd branch`, `bd dolt push`, `bd vc`
//! and `bd diff` are already-finished code that today exits 2 saying "sqlite has
//! no commit graph" — they light up here with no change above the seam.
//!
//! **And the hazard, stated once so nobody has to rediscover it:** `is_blocked`
//! is a denormalized cache of the dependency graph, maintained incrementally by
//! local write paths. A merge or a pull lands closed blockers and brand-new edges
//! that **no local write path ever saw**. The cache is therefore stale *by
//! definition* the moment rows arrive from elsewhere, and
//! [`Storage::recompute_blocked`] must run over the whole graph afterwards.
//!
//! Skip it and `bd ready` is quietly, confidently wrong after every sync: no
//! error, no crash, just the wrong work handed to the next agent. It is the worst
//! failure this system has, and it is invisible.
//!
//! [`Storage::recompute_blocked`]: bd_storage::Storage::recompute_blocked

pub mod server;
pub mod store;
pub mod vc;

use bd_storage::{Backend, Error, Identity, Locator, Result, Storage};
use sqlx::MySqlPool;
use std::path::{Path, PathBuf};

pub const BACKEND: Backend = Backend::Dolt;

/// The MySQL-dialect schema. The same tables as SQLite; different DDL.
pub const SCHEMA: &str = include_str!("schema.sql");

/// Config key holding the workspace's issue-id prefix.
pub const PREFIX_KEY: &str = "issue.prefix";

/// A beads store backed by a running `dolt sql-server`.
///
/// Defined here rather than in [`store`] because three modules implement traits
/// for it and none of them owns it: [`store`] implements [`Storage`], and [`vc`]
/// implements [`VersionControl`] and [`RemoteStore`].
pub struct DoltStore {
    pub(crate) pool: MySqlPool,
    pub(crate) identity: Identity,
    /// The server this store started, if it started one. Dropping the store must
    /// shut it down — an orphaned `dolt sql-server` holds the database lock and
    /// the next `bd` invocation fails for a reason that looks like corruption.
    ///
    /// Never read, and that is the point: it is held purely so that its `Drop`
    /// runs when the store dies. `#[allow]` rather than `_server`, because a
    /// leading underscore says "ignore me" about a field whose entire job is to
    /// be dropped at exactly the right moment.
    #[allow(dead_code)]
    pub(crate) server: Option<server::DoltServer>,
    /// The `.beads` directory.
    pub(crate) dir: PathBuf,
}

impl DoltStore {
    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Create a Dolt workspace and return an open store.
pub async fn init(_dir: &Path, _prefix: &str, _identity: Identity) -> Result<Box<dyn Storage>> {
    Err(not_built("init"))
}

/// Open an existing Dolt workspace.
pub async fn open(_locator: &Locator, _identity: Identity) -> Result<Box<dyn Storage>> {
    Err(not_built("open"))
}

fn not_built(op: &'static str) -> Error {
    Error::unsupported_hint(op, "dolt", "the dolt backend is not implemented yet")
}

/// Whether a `dolt` binary is on PATH.
///
/// Saying so plainly beats a connection timeout thirty seconds later. The binary
/// is a runtime dependency, not a vendored one — upstream resolves it the same
/// way (`exec.LookPath("dolt")`).
pub fn dolt_available() -> bool {
    which_dolt().is_some()
}

pub fn which_dolt() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "dolt.exe" } else { "dolt" };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
}

/// Skip an integration test when there is no `dolt` to run it against.
///
/// Loudly: a test that silently passes because it did nothing is worse than no
/// test, because it reports as coverage.
#[macro_export]
macro_rules! require_dolt {
    () => {
        if !$crate::dolt_available() {
            eprintln!(
                "SKIPPED: no `dolt` on PATH. This test is NOT covering anything. \
                 Install dolt (https://github.com/dolthub/dolt) to run it."
            );
            return;
        }
    };
}

// The capability accessors live on `impl Storage for DoltStore` in `store.rs`,
// and this is the entire reason the backend exists:
//
//     fn version_control(&self) -> Option<&dyn VersionControl> { Some(self) }
//     fn remote(&self)          -> Option<&dyn RemoteStore>    { Some(self) }
//     fn history(&self)         -> Option<&dyn HistoryViewer>  { Some(self) }
//
// Three lines. The CLI already routes on them, so returning `Some` is what turns
// `bd branch` from "exit 2, sqlite has no commit graph" into a working command —
// with no change anywhere above the seam. The traits themselves are implemented
// for `DoltStore` in `vc.rs`.
