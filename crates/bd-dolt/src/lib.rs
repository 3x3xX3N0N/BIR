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

/// Create a Dolt workspace in `dir` and return an open store.
///
/// `dir` is the `.beads` directory itself — it *is* the dolt repository. Do not
/// join `.beads` again; the caller already resolved it. (An earlier version of
/// the sqlite backend did join, and produced `.beads/.beads/beads.db`.)
pub async fn init(dir: &Path, prefix: &str, identity: Identity) -> Result<Box<dyn Storage>> {
    let dolt = require_dolt_binary()?;
    std::fs::create_dir_all(dir)
        .map_err(|e| other(format!("cannot create {}: {e}", dir.display())))?;

    // Identity first, and note the ordering is not incidental: `dolt init`
    // refuses to run without a `user.name`/`user.email`, and `dolt config
    // --local` has nowhere to write until `.dolt/` exists. `ensure_identity`
    // handles exactly that: `--global` before the repo, `--local` after.
    server::ensure_identity(dir, Some(&identity)).await?;

    if !dir.join(".dolt").is_dir() {
        run_dolt(&dolt, dir, &["init"]).await?;
    }

    let store = open_at(dir, identity).await?;
    store.set_config(PREFIX_KEY, prefix).await?;
    Ok(Box::new(store))
}

/// Open an existing Dolt workspace.
///
/// The backend comes from the locator (seam rule 3) — by the time we are here,
/// the workspace on disk has already said it is a Dolt one.
pub async fn open(locator: &Locator, identity: Identity) -> Result<Box<dyn Storage>> {
    require_dolt_binary()?;
    let dir = locator.dir.as_path();
    if !dir.join(".dolt").is_dir() {
        return Err(other(format!(
            "{} says this workspace is a dolt workspace, but there is no .dolt/ \
             directory in {}. The database is missing, not merely closed.",
            bd_storage::locator::LOCATOR_FILE,
            dir.display()
        )));
    }
    server::ensure_identity(dir, Some(&identity)).await?;
    Ok(Box::new(open_at(dir, identity).await?))
}

/// Start (or adopt) the server, find the database, apply the schema.
async fn open_at(dir: &Path, identity: Identity) -> Result<DoltStore> {
    let server = server::DoltServer::start(dir).await?;
    let db = discover_database(&server).await?;
    let pool = store::connect_pool(&server.dsn(&db)).await?;
    store::apply_schema(&pool).await?;
    Ok(DoltStore::new(pool, identity, Some(server), dir))
}

/// Ask the server what the database is called, rather than deriving it.
///
/// Dolt names a database after the directory holding it, mangling any character
/// that is not legal in a SQL identifier. Our directory is `.beads` — it starts
/// with a dot, which is exactly the sort of thing that gets mangled, and the
/// mangling rule is an implementation detail of a Go program we cannot run here.
///
/// So: do not guess. A wrong guess would fail at *connect* time with "unknown
/// database", on a machine we have no way to test on. `SHOW DATABASES` costs one
/// round trip and is right by construction.
async fn discover_database(server: &server::DoltServer) -> Result<String> {
    // No database in the DSN — we are asking *which* database.
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&server.dsn(""))
        .await
        .map_err(|e| other(format!("cannot reach the dolt server on port {}: {e}", server.port())))?;
    let all: Vec<String> = sqlx::query_scalar("SHOW DATABASES")
        .fetch_all(&pool)
        .await
        .map_err(|e| other(format!("SHOW DATABASES failed: {e}")))?;
    pool.close().await;

    let mut user: Vec<String> = all
        .into_iter()
        .filter(|d| !is_system_database(d))
        .collect();

    match user.len() {
        1 => Ok(user.pop().expect("length checked")),
        0 => Err(other(format!(
            "the dolt server in {} is serving no database. `dolt init` may not have run.",
            server.dir().display()
        ))),
        // An adopted server (a shared `dolt sql-server`, DoltLab) can serve
        // several. Guessing which one holds the issues is worse than saying so.
        _ => Err(other(format!(
            "the dolt server on port {} serves several databases and beads cannot tell \
             which is this workspace's: {}. Point beads at a server that serves only \
             this one.",
            server.port(),
            user.join(", ")
        ))),
    }
}

/// Dolt serves the MySQL system schemas plus a couple of its own.
fn is_system_database(name: &str) -> bool {
    const SYSTEM: &[&str] = &[
        "information_schema",
        "mysql",
        "performance_schema",
        "sys",
        // Dolt's own: the cluster-control schema, and the always-present
        // scratch database a server with no repo falls back on.
        "dolt_cluster",
    ];
    let lower = name.to_ascii_lowercase();
    SYSTEM.contains(&lower.as_str())
}

/// A missing `dolt` is a real, fixable failure — **not** a capability gap.
///
/// Exit 2 (`Unsupported`) means "this backend cannot do that, final answer, stop
/// asking". A Dolt backend on a machine with no `dolt` binary is not that: the
/// backend is perfectly capable and the machine is one download short. Saying
/// exit 2 here would tell the user to give up on something that `winget install
/// dolt` fixes.
fn require_dolt_binary() -> Result<PathBuf> {
    which_dolt().ok_or_else(|| {
        other(
            "this is a dolt workspace, but there is no `dolt` binary on PATH.\n\
             Install it from https://github.com/dolthub/dolt (or `brew install dolt`, \
             `winget install DoltHub.Dolt`) and try again.",
        )
    })
}

async fn run_dolt(dolt: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new(dolt)
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| other(format!("cannot run `dolt {}`: {e}", args.join(" "))))?;
    if !out.status.success() {
        return Err(other(format!(
            "`dolt {}` failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

fn other(msg: impl Into<String>) -> Error {
    Error::Other(bd_storage::error::anyhow_lite::Error(msg.into()))
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
