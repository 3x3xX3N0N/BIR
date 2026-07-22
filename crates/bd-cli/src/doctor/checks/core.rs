//! Core System — can the database be opened, read, and written at all.
//!
//! This family is the one that reports *the store itself* being broken. Every
//! other family, on failing to get a store, reports [`Finding::unknown`] about
//! itself and leaves the diagnosis to this one — otherwise a single unopenable
//! database produces a hundred identical errors and buries the actual cause.
//!
//! Belongs here: database presence, opens-at-all, integrity, size, schema
//! version and compatibility, filesystem permissions, `--readonly` sanity,
//! pending/incomplete migrations, fresh-clone state.
//!
//! # Two things this family does differently, and why
//!
//! **It does not trust [`Dx::dir`].** `Dx::dir` is `Some` only when the locator
//! *loaded*, so a `.beads/` whose `workspace.json` is missing or corrupt looks
//! exactly like no `.beads/` at all — which is the single state this family most
//! needs to tell apart. So it finds `.beads/` itself, by walking up from the
//! working directory the way `Locator::discover` does, and then asks separately
//! whether the locator inside it is readable. (Today `Ctx::build` refuses to
//! build over an unreadable locator, so `bd doctor` never even reaches us in
//! that state. That is a bug in the context, not a reason for the check to be
//! wrong; see the report accompanying this file.)
//!
//! **It reads the database file's own header.** The storage seam exposes no
//! `integrity_check`, no schema version, and no raw SQL — and it should not: it
//! is a backend-neutral seam and those are all SQLite words. But the 100-byte
//! SQLite header is a stable, documented, public file format, and reading it
//! costs one `open` and one `read`. It is what turns "the database will not
//! open: file is not a database" into "`.beads/beads.db` is a git-lfs pointer,
//! not a database" — and the second one ends the investigation.
//!
//! What it deliberately cannot do: `PRAGMA integrity_check`. See [`Integrity`].

use std::io::Read as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bd_core::IssueFilter;
use bd_storage::Backend;
use bd_storage::locator::{BEADS_DIR, LOCATOR_FILE};

use super::super::{Category, Check, Dx, Finding};

pub fn checks() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(Workspace),
        Box::new(Database),
        Box::new(Schema),
        Box::new(Integrity),
        Box::new(Permissions),
        Box::new(DatabaseSize),
        Box::new(FreshClone),
    ]
}

/// The database file, for the one backend this port implements. Kept in step
/// with `Locator::db_path`, which is not reachable without a loaded locator —
/// and an unloadable locator is precisely when we need the path most.
const DB_FILE: &str = "beads.db";

/// What `bd export` writes and the git hook keeps in step. A clone carries this
/// and not the database.
const JSONL_FILE: &str = "issues.jsonl";

/// Upstream's key, upstream's default. 0 disables the check.
const PRUNE_KEY: &str = "doctor.suggest_pruning_issue_count";
const PRUNE_DEFAULT: u64 = 5000;

// ---------------------------------------------------------------------------
// Finding the workspace without needing it to work
// ---------------------------------------------------------------------------

/// The `.beads` entry, found the way [`bd_storage::Locator::discover`] finds it
/// — but **without** requiring that it be a directory, or that the locator
/// inside it parse. Both of those are faults to report, not reasons to give up
/// and say "no workspace here".
fn find_beads(dx: &Dx<'_>) -> Option<PathBuf> {
    if let Some(d) = &dx.dir {
        return Some(d.clone());
    }
    let mut cur = Some(dx.ctx.cwd.as_path());
    while let Some(d) = cur {
        let candidate = d.join(BEADS_DIR);
        if candidate.exists() {
            return Some(candidate);
        }
        cur = d.parent();
    }
    None
}

fn db_path(dx: &Dx<'_>) -> Option<PathBuf> {
    find_beads(dx).map(|d| d.join(DB_FILE))
}

/// The honest way back from a database that is gone or unreadable.
///
/// Three things here were learned by running the suggestion instead of writing
/// it, and each one is the difference between a fix and a second bug report:
///
/// * The export is only mentioned when it **exists**. Telling someone to
///   `bd import` a file that is not there is worse than saying nothing.
/// * `--prefix` is carried over explicitly. `bd init` with no `--prefix` derives
///   one from the *directory name*, so the bare `bd init --force` silently
///   re-prefixes the workspace — every existing issue keeps its old id and every
///   new one gets a different one. That is a mess you cannot see until much later.
/// * `bd init` rewrites `.beads/config.yaml` **from defaults**, so a customised
///   lease or default priority is lost. It is usually in git, so say so.
fn restore_hint(dx: &Dx<'_>, beads: &Path) -> String {
    let init = match dx.ctx.config.prefix.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => format!("bd init --force --prefix {p}"),
        None => "bd init --force".to_string(),
    };
    if beads.join(JSONL_FILE).is_file() {
        format!(
            "restore {DB_FILE} from a backup, or rebuild it: `{init}` recreates the database (it \
             keeps the workspace id and deletes no rows), then `bd import .beads/{JSONL_FILE}` \
             puts the issues back. Note that init also rewrites .beads/config.yaml from defaults — \
             `git checkout .beads/config.yaml` afterwards if it is committed"
        )
    } else {
        format!(
            "restore {DB_FILE} from a backup or from git. There is no .beads/{JSONL_FILE} to \
             rebuild it from, so `{init}` would only give you an empty workspace"
        )
    }
}

fn human(bytes: u64) -> String {
    const K: u64 = 1024;
    match bytes {
        b if b < K => format!("{b} B"),
        b if b < K * K => format!("{:.1} KiB", b as f64 / K as f64),
        b if b < K * K * K => format!("{:.1} MiB", b as f64 / (K * K) as f64),
        b => format!("{:.1} GiB", b as f64 / (K * K * K) as f64),
    }
}

// ---------------------------------------------------------------------------
// The SQLite file header
//
// https://sqlite.org/fileformat2.html#the_database_header. A hundred bytes, a
// fixed layout, and it has not changed since 2004. Reading it is not a layering
// violation dressed up: it is the only way to distinguish "corrupt database"
// from "not a database" *before* handing the file to sqlx, which reports both
// as `file is not a database`.
// ---------------------------------------------------------------------------

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

#[derive(Debug, Clone, Copy)]
struct Header {
    page_size: u64,
    /// The database size in pages, as the header claims it — but only when the
    /// header says that claim is current. A stale value is not a fault; it is
    /// what every SQLite before 3.7 wrote, so believing it would invent
    /// corruption out of an old file.
    pages: Option<u64>,
}

// `PRAGMA user_version` (bytes 60..64) is deliberately *not* read out of the
// header here. It is the schema version stamp now, and the [`Schema`] check
// asks the open store for it — one code path for both backends, instead of a
// header peek that only ever worked for SQLite.

#[derive(Debug)]
enum Shape {
    Missing,
    /// A directory, or a socket, or something else that is not a file.
    NotAFile,
    Unreadable(String),
    /// Zero bytes. SQLite will *open* this quite happily and hand back a
    /// database with no tables in it, which is why it has to be caught here.
    Empty,
    /// It exists, it has bytes, and they are not a SQLite database. The string
    /// is a preview — usually the answer (`version https://git-lfs...`,
    /// `<<<<<<< HEAD`).
    Foreign(String),
    Sqlite(Header, u64),
}

fn be32(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[..4]);
    u32::from_be_bytes(a)
}

/// Whether SQLite's write-ahead-log sidecar exists beside `path`.
///
/// The WAL for `foo.db` is `foo.db-wal` (a suffix on the whole filename, not a
/// replaced extension), so `with_extension` would be wrong. Its presence means
/// the main file may legitimately be behind its own header page count.
fn wal_exists(path: &Path) -> bool {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    Path::new(&wal).exists()
}

fn inspect(path: &Path) -> Shape {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Shape::Missing,
        Err(e) => return Shape::Unreadable(e.to_string()),
    };
    if !meta.is_file() {
        return Shape::NotAFile;
    }

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Shape::Unreadable(e.to_string()),
    };
    let mut buf = [0u8; 100];
    let mut got = 0usize;
    loop {
        match file.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => {
                got += n;
                if got == buf.len() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Shape::Unreadable(e.to_string()),
        }
    }
    let head = &buf[..got];

    // Sample the length from the SAME handle, AFTER the header. A concurrent
    // checkpoint can only EXTEND the main file — sqlite writes the WAL frames
    // back (growing it) before truncating the WAL — so reading len after the
    // header means a race can only CLEAR a false "shorter than its header"
    // verdict, never manufacture one. Taking len from a separate earlier
    // `metadata()` call is exactly the TOCTOU that produced the false alarm.
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => return Shape::Unreadable(e.to_string()),
    };
    if len == 0 {
        return Shape::Empty;
    }

    if got < 100 || &head[..16] != SQLITE_MAGIC {
        return Shape::Foreign(preview(head));
    }

    let page_size = match u16::from_be_bytes([head[16], head[17]]) {
        // The one encoding quirk in the header: 1 means 65536, because 65536
        // does not fit in the two bytes it is stored in.
        1 => 65536,
        n => u64::from(n),
    };
    // Bytes 28..32 are only meaningful when the change counter (24..28) matches
    // the "version-valid-for" number (92..96).
    let pages = (be32(&head[24..]) == be32(&head[92..])).then(|| u64::from(be32(&head[28..])));

    Shape::Sqlite(Header { page_size, pages }, len)
}

/// The first line of whatever this actually is, made safe to print.
fn preview(head: &[u8]) -> String {
    let text: String = head
        .iter()
        .take_while(|b| **b != b'\n' && **b != b'\r')
        .map(|b| match b {
            0x20..=0x7e => char::from(*b),
            _ => '.',
        })
        .take(72)
        .collect();
    if text.trim_matches('.').is_empty() {
        format!("starts with {} binary bytes that are not a SQLite header", head.len())
    } else {
        format!("starts with: {text}")
    }
}

// ---------------------------------------------------------------------------
// workspace
// ---------------------------------------------------------------------------

/// Is there a `.beads/` here at all, and does its locator load.
///
/// Error, not warning, when there is none: `bd doctor` was asked a question
/// about a beads workspace and there is no beads workspace, and every other
/// check in the program is undeterminable in that state. Reporting "0 errors"
/// over a directory beads has never touched is the coverage lie the seam docs
/// are about.
struct Workspace;

#[async_trait]
impl Check for Workspace {
    fn name(&self) -> &'static str {
        "workspace"
    }
    fn category(&self) -> Category {
        Category::Core
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(beads) = find_beads(dx) else {
            return Finding::error("workspace", "no .beads/ directory")
                .detail(format!("searched upwards from {}", dx.ctx.cwd.display()))
                .fix("`bd init` creates one here (the id prefix is derived from the directory name; `--prefix` overrides it)");
        };

        if !beads.is_dir() {
            return Finding::error("workspace", ".beads exists but is not a directory")
                .detail(beads.display().to_string())
                .fix("move or delete it, then run `bd init`");
        }

        if let Some(l) = &dx.ctx.locator {
            return Finding::ok(
                "workspace",
                format!("{} workspace at {}", l.backend, beads.display()),
            )
            .detail(format!("workspace id: {}", l.workspace_id));
        }

        // `.beads/` is a directory and the locator did not load. Today this is
        // unreachable — `Ctx::build` fails before `bd doctor` runs, so the user
        // gets a bare `error: cannot read the workspace at ...` and none of the
        // hundred checks below. The check is written anyway: it is correct now,
        // and it starts working the moment the context stops dying.
        let file = beads.join(LOCATOR_FILE);
        let why = match std::fs::read_to_string(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                format!("{} does not exist", file.display())
            }
            Err(e) => format!("cannot read {}: {e}", file.display()),
            Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                Err(e) => format!("{} is not valid JSON: {e}", file.display()),
                Ok(_) => format!(
                    "{} parses as JSON but is not a locator (it needs `backend` and `workspace_id`)",
                    file.display()
                ),
            },
        };
        Finding::error("workspace", ".beads/ is here but the workspace will not load")
            .detail(why)
            .fix(format!(
                "`bd init --force` rewrites .beads/{LOCATOR_FILE} and leaves the database alone — \
                 but it can only preserve the workspace id if it can still read one, so a clone \
                 that shared this workspace may stop recognising it"
            ))
    }
}

// ---------------------------------------------------------------------------
// database
// ---------------------------------------------------------------------------

/// Does the store open — and if not, **why**.
///
/// The `why` is the point. It is the one string in the whole report that ends an
/// investigation instead of starting one, so it is never swallowed: whatever the
/// storage layer said goes into `detail` verbatim, and this check's own message
/// only ever *adds* to it.
///
/// This is also the check that first asks [`Dx::store`], which is what populates
/// the probe every other family reads. It always asks, on every path, so that a
/// `Core` check returning early can never leave the other eight families with an
/// empty `store_error()`.
///
/// It looks at the file **before** it opens it, though, and that order is not
/// incidental: opening a WAL database writes to the directory and can rewrite a
/// zero-length file into a valid empty one. Observe, then touch.
struct Database;

#[async_trait]
impl Check for Database {
    fn name(&self) -> &'static str {
        "database"
    }
    fn category(&self) -> Category {
        Category::Core
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        // The state of the file as it was *before* we opened anything.
        let shape = db_path(dx).map(|p| inspect(&p));

        // Always, on every path: this is the call that populates the probe.
        let opened = dx.store().await.is_some();
        let reported = dx
            .store_error()
            .unwrap_or("the store did not open and gave no reason")
            .to_string();

        let Some(beads) = find_beads(dx) else {
            return Finding::unknown(
                "database",
                "there is no .beads/ directory to hold one — see the `workspace` check",
            );
        };

        let Some(backend) = dx.ctx.backend() else {
            return Finding::unknown(
                "database",
                "the workspace would not load, so there is no backend to open — see the \
                 `workspace` check",
            );
        };

        // Everything below reasons about a *SQLite file*. On any other backend
        // that reasoning is not merely useless, it is wrong — it would look for
        // `.beads/beads.db` in a Dolt workspace and report the wrong fault with
        // total confidence.
        if backend != Backend::Sqlite {
            return if opened {
                Finding::ok("database", format!("{backend} workspace opens"))
            } else {
                Finding::error(
                    "database",
                    format!("this build of bd cannot open a {backend} workspace"),
                )
                .detail(reported)
                .fix(
                    "this is a port in progress, not a broken workspace. Use the beads build that \
                     owns this backend — or `bd export` from it and `bd import` into a fresh \
                     sqlite workspace",
                )
            };
        }

        let path = beads.join(DB_FILE);
        let shape = shape.unwrap_or_else(|| inspect(&path));

        if opened {
            // "It opened" is not the same as "it is fine", and the gap between
            // those two is a zero-byte file. SQLite opens one without a murmur
            // and hands back a database with no schema in it — so a check that
            // stopped at `opened` would put a green tick beside the emptiest
            // possible failure. (`schema` and `integrity` catch it too. This is
            // the one that says *why*.)
            if matches!(shape, Shape::Empty) {
                return Finding::error("database", "the database file is empty (0 bytes)")
                    .detail(format!(
                        "{}\n\nsqlite opened it anyway — an empty file is a valid database with no \
                         tables in it, which is why this is caught here and not by the open. Every \
                         query against it fails.",
                        path.display()
                    ))
                    .fix(restore_hint(dx, &beads));
            }
            let size = match &shape {
                Shape::Sqlite(_, len) => human(*len),
                // It opened, so it is a database; we just could not stat it.
                _ => "size unknown".to_string(),
            };
            return Finding::ok("database", format!("sqlite, opens, {size}"))
                .detail(path.display().to_string());
        }

        // It did not open. Say what is actually on disk, and keep the raw error.
        let where_ = path.display().to_string();
        match shape {
            Shape::Missing => Finding::error("database", "the database file does not exist")
                .detail(format!("{where_}\n\nthe store said: {reported}"))
                .fix(restore_hint(dx, &beads)),

            Shape::Empty => Finding::error("database", "the database file is empty (0 bytes)")
                .detail(format!(
                    "{where_}\n\nan empty file is not a database with no issues in it — it is a \
                     file with no schema, and every query against it fails.\n\nthe store said: \
                     {reported}"
                ))
                .fix(restore_hint(dx, &beads)),

            Shape::NotAFile => Finding::error("database", "the database path is not a file")
                .detail(format!("{where_}\n\nthe store said: {reported}"))
                .fix(format!(
                    "something else is occupying .beads/{DB_FILE}. Move it aside, then {}",
                    restore_hint(dx, &beads)
                )),

            Shape::Foreign(what) => {
                Finding::error("database", "the database file is not a SQLite database")
                    .detail(format!("{where_}\n{what}\n\nthe store said: {reported}"))
                    .fix(format!(
                        "a git-lfs pointer, a merge conflict, or a truncated checkout all look \
                         like this. Check `git status` and `git check-attr filter -- \
                         .beads/{DB_FILE}` first; then {}",
                        restore_hint(dx, &beads)
                    ))
            }

            Shape::Unreadable(e) => Finding::error("database", "the database file cannot be read")
                .detail(format!("{where_}\n{e}\n\nthe store said: {reported}"))
                .fix("check the ownership and mode of .beads/ — see the `permissions` check"),

            // A real SQLite file that sqlx still would not open: corruption, a
            // lock held by another process, a filesystem that cannot do WAL. We
            // do not know which, so we do not guess — the store's own words are
            // the finding.
            Shape::Sqlite(_, len) => {
                Finding::error("database", "the database will not open").detail(format!(
                    "{where_} ({}, a valid SQLite file)\n\nthe store said: {reported}",
                    human(len)
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// schema
// ---------------------------------------------------------------------------

/// Is this database the shape this binary expects.
///
/// Two determinations, both real:
///
/// 1. **Compare the version stamp.** Every database this port creates is
///    stamped ([`bd_storage::SCHEMA_VERSION`]; `PRAGMA user_version` on
///    SQLite, the `schema_meta` table on Dolt). Behind means `bd migrate`;
///    ahead means a newer bd wrote this and the fix is upgrading bd — each
///    refusal names its own next step, never the other's. A raw stamp of 0 is
///    a database from before stamping existed: v1 by definition, so it passes
///    — with a one-time nudge to run `bd migrate`, because an unstamped
///    database stops being cheap to identify the day a v2 schema ships.
/// 2. **Exercise the tables.** A database that opens but has no `issues` table
///    is the single most common "schema" fault there is — an empty file, a
///    database written by a different tool. The stamp can lie (another tool
///    can write a 1); the tables answering is a positive determination.
struct Schema;

#[async_trait]
impl Check for Schema {
    fn name(&self) -> &'static str {
        "schema"
    }
    fn category(&self) -> Category {
        Category::Core
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(store) = dx.store().await else {
            return Finding::unknown(
                "schema",
                dx.store_error()
                    .unwrap_or("the store would not open — see the `database` check"),
            );
        };

        // The stamp first: it is the cheap, precise answer, and when it
        // mismatches, the table exercise below would just fail noisily at
        // whatever query happens to hit the changed shape first.
        let speaks = bd_storage::SCHEMA_VERSION;
        let raw = match store.schema_version().await {
            Ok(v) => v,
            Err(e) => {
                return Finding::error("schema", "the schema version stamp does not answer")
                    .detail(e.to_string())
                    .fix(
                        "this database is not the shape this build of bd expects — export from \
                         whatever wrote it, then `bd import` into a fresh `bd init` workspace",
                    );
            }
        };
        let effective = bd_storage::effective_schema_version(raw);

        if effective < speaks {
            return Finding::error(
                "schema",
                format!("the database records schema v{effective}; this bd speaks v{speaks}"),
            )
            .fix("run `bd migrate` to bring the database up to date in place");
        }
        if effective > speaks {
            return Finding::error(
                "schema",
                format!(
                    "the database records schema v{effective}, newer than this build of bd \
                     (v{speaks})"
                ),
            )
            .detail(
                "a newer bd wrote this database; nothing here can tell you whether this build \
                 reads it safely",
            )
            .fix("upgrade bd — `bd migrate` cannot downgrade a database");
        }

        let incompatible = |table: &str, e: String| {
            Finding::error("schema", format!("the `{table}` table does not answer"))
                .detail(e)
                .fix(
                    "the version stamp matches but the tables do not — something other than bd \
                     has altered this database. Export from whatever wrote it, then `bd import` \
                     into a fresh `bd init` workspace",
                )
        };

        if let Err(e) = store.get_config("issue.prefix").await {
            return incompatible("config", e.to_string());
        }
        let count = match store.count_issues(&IssueFilter::default()).await {
            Ok(n) => n,
            Err(e) => return incompatible("issues", e.to_string()),
        };

        if raw == 0 {
            return Finding::warn(
                "schema",
                format!("pre-versioning database (v{speaks} by definition, but unstamped)"),
            )
            .detail(format!(
                "this database predates schema version stamping. It is v{speaks} — exactly one \
                 schema ever shipped unversioned — and everything works today; the stamp is what \
                 lets a future bd say `run bd migrate` instead of failing mid-query.\n\nits \
                 tables answer: {count} issues.",
            ))
            .fix("run `bd migrate` once to stamp it");
        }

        Finding::ok(
            "schema",
            format!("schema v{effective}; issues and config answer ({count} issues)"),
        )
    }
}

// ---------------------------------------------------------------------------
// integrity
// ---------------------------------------------------------------------------

/// Is the file structurally sound, and does its content read back.
///
/// # This is not `PRAGMA integrity_check`, and says so
///
/// It cannot be. The storage seam is backend-neutral and exposes no raw SQL, and
/// `bd-cli` does not depend on `sqlx` — which is the right shape for a seam: a
/// `Storage` trait with a `PRAGMA` on it is a SQLite trait wearing a disguise.
/// The honest missing piece is `Storage::check_integrity(&self) -> Result<Vec<String>>`,
/// and it is reported as missing rather than faked.
///
/// What is actually determined here, and it is not nothing:
///
/// * The file is a whole number of pages, and no shorter than its own header
///   says it is. That is the signature of a **truncated** database — a killed
///   `cp`, a bad rsync, a checkout that ran out of disk — and it is the
///   corruption people actually get.
/// * Every issue row and every label reads back. `stats()` aggregates over the
///   whole `issues` table, so a page that has gone bad underneath it surfaces
///   here as an error rather than as a wrong number.
///
/// A `Warn` from this check means "I could not look", never "it is probably
/// fine".
struct Integrity;

#[async_trait]
impl Check for Integrity {
    fn name(&self) -> &'static str {
        "integrity"
    }
    fn category(&self) -> Category {
        Category::Core
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(store) = dx.store().await else {
            return Finding::unknown(
                "integrity",
                dx.store_error()
                    .unwrap_or("the store would not open — see the `database` check"),
            );
        };

        // 1. The file's own arithmetic.
        let beads = find_beads(dx);
        if let Some(path) = db_path(dx)
            && let Shape::Sqlite(h, len) = inspect(&path)
        {
            let truncated = |msg: String, detail: String| {
                Finding::error("integrity", msg).detail(detail).fix(
                    beads
                        .as_deref()
                        .map(|b| restore_hint(dx, b))
                        .unwrap_or_else(|| "restore the database from a backup".to_string()),
                )
            };
            if !h.page_size.is_power_of_two() || !(512..=65536).contains(&h.page_size) {
                return truncated(
                    "the database header is corrupt".to_string(),
                    format!(
                        "{}: page size {} is not a power of two between 512 and 65536",
                        path.display(),
                        h.page_size
                    ),
                );
            }
            if len % h.page_size != 0 {
                return truncated(
                    "the database file is truncated".to_string(),
                    format!(
                        "{}: {len} bytes is not a whole number of {}-byte pages ({} bytes into \
                         page {})",
                        path.display(),
                        h.page_size,
                        len % h.page_size,
                        len / h.page_size + 1
                    ),
                );
            }
            if let Some(pages) = h.pages
                && pages.saturating_mul(h.page_size) > len
                && !wal_exists(&path)
            {
                // Only truncation when there is NO `-wal` sidecar to explain the
                // shortfall. In WAL mode the main file is legitimately behind its
                // own header — new pages live in the WAL until a checkpoint writes
                // them back — and `bd` can be killed (or the machine can crash)
                // mid-checkpoint leaving exactly a short main file plus a `-wal`.
                // The WAL is authoritative, so that is a recoverable state, not
                // corruption: the read-back probes below go THROUGH sqlite, which
                // applies the WAL, and are the real determination. Reporting
                // truncation here would tell a user with a perfectly good database
                // to restore from backup. (SQLite extends the main file to full
                // size and only THEN truncates the WAL, so "no `-wal`" genuinely
                // means the main file is whole — the check does not lose its teeth
                // for the real truncation it exists to catch.)
                return truncated(
                    "the database file is shorter than its header says".to_string(),
                    format!(
                        "{}: the header claims {pages} pages of {} bytes ({}), the file is {}",
                        path.display(),
                        h.page_size,
                        human(pages.saturating_mul(h.page_size)),
                        human(len)
                    ),
                );
            }
        }

        // 2. The content. Both of these walk whole tables.
        let stats = match store.stats().await {
            Ok(s) => s,
            Err(e) => {
                return Finding::error("integrity", "reading the issues table failed")
                    .detail(e.to_string())
                    .fix(
                        beads
                            .as_deref()
                            .map(|b| restore_hint(dx, b))
                            .unwrap_or_else(|| "restore the database from a backup".to_string()),
                    );
            }
        };
        let labels = match store.list_labels().await {
            Ok(l) => l.len(),
            Err(e) => {
                return Finding::error("integrity", "reading the labels table failed")
                    .detail(e.to_string())
                    .fix(
                        beads
                            .as_deref()
                            .map(|b| restore_hint(dx, b))
                            .unwrap_or_else(|| "restore the database from a backup".to_string()),
                    );
            }
        };

        Finding::ok(
            "integrity",
            format!("{} issues and {labels} labels read back", stats.total),
        )
        .detail(
            "a structural check of the file header plus a full read of the issues and labels \
             tables. This is not sqlite's `PRAGMA integrity_check` — the storage seam exposes no \
             way to run one",
        )
    }
}

// ---------------------------------------------------------------------------
// permissions
// ---------------------------------------------------------------------------

/// Can we actually write here.
///
/// # The one place this family touches the disk, and the fence around it
///
/// Rule 3 says a check never mutates anything. Answering "is this directory
/// writable" has exactly one reliable cross-platform implementation, and it is to
/// write something: mode bits lie on Windows, ACLs are not in the mode bits at
/// all, and a read-only *mount* shows through in neither. So this check creates a
/// dot-prefixed probe file inside `.beads/` and removes it again.
///
/// The fences:
///
/// * The database file itself is probed **without** writing — opening a file for
///   write does not modify it — so the expensive question is only asked of the
///   directory.
/// * Under `--readonly` the probe does not happen at all, and the check says so
///   with a `Warn` rather than claiming an answer it did not get. `--readonly`
///   means *do not write*, and a check that wrote anyway "just to check" would
///   be the exact bug `--readonly` exists to prevent.
/// * If the probe cannot be removed, that is reported, not swallowed. A doctor
///   that litters is a doctor the pollution checks will later diagnose.
struct Permissions;

#[async_trait]
impl Check for Permissions {
    fn name(&self) -> &'static str {
        "permissions"
    }
    fn category(&self) -> Category {
        Category::Core
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(beads) = find_beads(dx) else {
            return Finding::unknown(
                "permissions",
                "there is no .beads/ directory — see the `workspace` check",
            );
        };

        // The database file, without writing to it. `open(write)` does not
        // truncate and does not change a byte; it just asks the kernel.
        let db = beads.join(DB_FILE);
        if db.is_file()
            && let Err(e) = std::fs::OpenOptions::new().write(true).open(&db)
        {
            return Finding::error("permissions", "the database file is not writable")
                .detail(format!("{}: {e}", db.display()))
                .fix(format!(
                    "every bd command that is not a query needs to write here. Fix the file's \
                     mode or ownership (`chmod u+w {}`, or clear the read-only attribute on \
                     Windows)",
                    db.display()
                ));
        }

        if dx.ctx.readonly {
            return Finding::warn("permissions", "not fully checked under --readonly")
                .detail(format!(
                    "{} is readable, and the database file is not marked read-only. Whether \
                     .beads/ accepts *new* files was not determined: the only reliable way to \
                     find out is to create one, and --readonly means do not.",
                    beads.display()
                ))
                .fix("run `bd doctor` without --readonly to check this");
        }

        // The directory. SQLite in WAL mode creates `beads.db-wal` and
        // `beads.db-shm` beside the database — so a directory that will not take
        // new files breaks *reading*, not just writing, and that surprises
        // everybody the first time.
        let probe = beads.join(format!(".bd-doctor-probe-{}", std::process::id()));
        if let Err(e) = std::fs::write(&probe, b"") {
            return Finding::error("permissions", ".beads/ is not writable")
                .detail(format!(
                    "could not create {}: {e}\n\nsqlite creates {DB_FILE}-wal and {DB_FILE}-shm in \
                     this directory, so a read-only .beads/ breaks reading too, not only writing",
                    probe.display()
                ))
                .fix(format!(
                    "fix the mode or ownership of {} (`chmod u+w`, or check the ACL on Windows)",
                    beads.display()
                ));
        }
        if let Err(e) = std::fs::remove_file(&probe) {
            return Finding::warn("permissions", "left a probe file behind")
                .detail(format!(
                    "{} was created to test writability but could not be removed: {e}",
                    probe.display()
                ))
                .fix(format!("delete {} by hand", probe.display()));
        }

        Finding::ok("permissions", ".beads/ and the database are writable")
    }
}

// ---------------------------------------------------------------------------
// database-size
// ---------------------------------------------------------------------------

/// How big has this got.
///
/// Purely advisory, and **never** repaired: the only thing that makes a beads
/// database smaller is deleting issues, and a doctor that deletes issues because
/// it thought the file was large is not a doctor. `repair` stays `Unfixable` on
/// purpose.
struct DatabaseSize;

#[async_trait]
impl Check for DatabaseSize {
    fn name(&self) -> &'static str {
        "database-size"
    }
    fn category(&self) -> Category {
        Category::Core
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(store) = dx.store().await else {
            return Finding::unknown(
                "database-size",
                dx.store_error()
                    .unwrap_or("the store would not open — see the `database` check"),
            );
        };
        let stats = match store.stats().await {
            Ok(s) => s,
            Err(e) => {
                // The `integrity` check owns the diagnosis; this one just says
                // it could not count.
                return Finding::unknown("database-size", format!("could not count issues: {e}"));
            }
        };

        let on_disk = db_path(dx)
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len());
        let size = on_disk.map(human).unwrap_or_else(|| "size unknown".to_string());

        // Upstream's key and default, so a workspace shared between the two
        // implementations is tuned once. 0 disables.
        let threshold = match store.get_config(PRUNE_KEY).await {
            Ok(Some(v)) => v.trim().parse::<u64>().unwrap_or(PRUNE_DEFAULT),
            _ => PRUNE_DEFAULT,
        };

        if threshold > 0 && stats.closed > threshold {
            return Finding::warn(
                "database-size",
                format!("{} closed issues (over {threshold})", stats.closed),
            )
            .detail(format!(
                "{} issues total, {size} on disk. Closed issues are read by every query that does \
                 not exclude them, so a large tail of them is felt in `bd list` before it is felt \
                 anywhere else.",
                stats.total
            ))
            .fix(format!(
                "`bd purge --older-than 90d --yes` deletes closed issues permanently — run `bd \
                 export -o backup.jsonl` first, because there is no undo. To silence this instead: \
                 `bd config set {PRUNE_KEY} 0`"
            ));
        }

        Finding::ok(
            "database-size",
            format!("{} issues, {} closed, {size}", stats.total, stats.closed),
        )
    }
}

// ---------------------------------------------------------------------------
// fresh-clone
// ---------------------------------------------------------------------------

/// A workspace that was cloned but never initialized.
///
/// The shape: `.beads/` is in git, so a clone gets `workspace.json`, `config.yaml`
/// and `issues.jsonl` — but the database is (rightly) ignored, so it is not there.
/// Every command fails with "no beads workspace found", which is a lie: the
/// workspace is right there, and every issue in it is sitting in the JSONL.
///
/// Getting this one wrong in the *other* direction is worse than not having it,
/// so it fires only when there is genuinely something to restore.
struct FreshClone;

#[async_trait]
impl Check for FreshClone {
    fn name(&self) -> &'static str {
        "fresh-clone"
    }
    fn category(&self) -> Category {
        Category::Core
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(beads) = find_beads(dx) else {
            return Finding::unknown(
                "fresh-clone",
                "there is no .beads/ directory — see the `workspace` check",
            );
        };

        // A database with bytes in it is not a fresh clone, whatever else may be
        // wrong with it.
        if !matches!(inspect(&beads.join(DB_FILE)), Shape::Missing | Shape::Empty) {
            return Finding::ok("fresh-clone", "the database is present");
        }

        let jsonl = beads.join(JSONL_FILE);
        let Ok(raw) = std::fs::read_to_string(&jsonl) else {
            // No export waiting. The database is still missing — that is the
            // `database` check's finding, not this one's. Rule 4: absence of an
            // export is not a fault.
            return Finding::ok(
                "fresh-clone",
                format!("no .beads/{JSONL_FILE} waiting to be restored"),
            );
        };

        let records = raw.lines().filter(|l| !l.trim().is_empty()).count();
        if records == 0 {
            return Finding::ok("fresh-clone", format!(".beads/{JSONL_FILE} is empty"));
        }

        Finding::warn(
            "fresh-clone",
            format!("cloned but never initialized — {records} records are waiting"),
        )
        .detail(format!(
            "{} holds {records} exported records and there is no database beside it. This is what \
             a fresh `git clone` of a beads workspace looks like: .beads/ is committed, the \
             database is not.",
            jsonl.display()
        ))
        // The same recipe the `database` check gives, from the same place: two
        // checks that disagree about how to recover the same workspace is how a
        // user ends up running both.
        .fix(restore_hint(dx, &beads))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "bd-doctor-core-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The header parse is the load-bearing part of half this family, and it is
    /// the part that would be silently wrong: an off-by-four in the offsets
    /// gives you a page size of nonsense and an invented "truncated database".
    #[test]
    fn a_real_sqlite_header_parses() {
        let dir = tmp("hdr");
        let path = dir.join("beads.db");

        // A hand-built, minimal, *valid* header: magic, 4096-byte pages, one
        // page, a change counter that matches version-valid-for (so the page
        // count is believable), user_version 0.
        let mut db = vec![0u8; 4096];
        db[..16].copy_from_slice(SQLITE_MAGIC);
        db[16..18].copy_from_slice(&4096u16.to_be_bytes());
        db[24..28].copy_from_slice(&7u32.to_be_bytes()); // change counter
        db[28..32].copy_from_slice(&1u32.to_be_bytes()); // size in pages
        db[92..96].copy_from_slice(&7u32.to_be_bytes()); // version-valid-for
        std::fs::write(&path, &db).unwrap();

        match inspect(&path) {
            Shape::Sqlite(h, len) => {
                assert_eq!(h.page_size, 4096);
                assert_eq!(h.pages, Some(1));
                assert_eq!(len, 4096);
            }
            other => panic!("a valid header did not parse: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The page count is only meaningful when the header says it is current.
    /// Believing a stale one invents corruption in files written by any SQLite
    /// older than 3.7 — which is the one failure mode a corruption check must
    /// not have.
    #[test]
    fn a_stale_page_count_is_not_believed() {
        let dir = tmp("stale");
        let path = dir.join("beads.db");
        let mut db = vec![0u8; 4096];
        db[..16].copy_from_slice(SQLITE_MAGIC);
        db[16..18].copy_from_slice(&4096u16.to_be_bytes());
        db[24..28].copy_from_slice(&9u32.to_be_bytes());
        db[28..32].copy_from_slice(&999u32.to_be_bytes()); // a lie
        db[92..96].copy_from_slice(&2u32.to_be_bytes()); // ...and known to be stale
        std::fs::write(&path, &db).unwrap();

        match inspect(&path) {
            Shape::Sqlite(h, _) => assert_eq!(h.pages, None, "a stale page count was believed"),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The finding that ends an investigation: a git-lfs pointer where a
    /// database should be. sqlx says "file is not a database"; we say what it is.
    #[test]
    fn a_foreign_file_is_previewed_not_just_rejected() {
        let dir = tmp("lfs");
        let path = dir.join("beads.db");
        std::fs::write(
            &path,
            b"version https://git-lfs.github.com/spec/v1\noid sha256:deadbeef\nsize 4096\n",
        )
        .unwrap();

        match inspect(&path) {
            Shape::Foreign(what) => assert!(
                what.contains("git-lfs"),
                "the preview lost the one word that explains the fault: {what}"
            ),
            other => panic!("an lfs pointer was not reported as foreign: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Zero bytes is the trap: sqlite opens it without complaint and hands back
    /// a database with no tables, so a check that only asked "did it open" would
    /// pass it.
    #[test]
    fn an_empty_file_is_not_a_database() {
        let dir = tmp("empty");
        let path = dir.join("beads.db");
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(inspect(&path), Shape::Empty));
        assert!(matches!(inspect(&dir.join("nope.db")), Shape::Missing));
        assert!(matches!(inspect(&dir), Shape::NotAFile));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every name in this family is a documented key that agents grep for in
    /// `--json`. They are asserted here so that renaming one has to be a
    /// deliberate act.
    #[test]
    fn the_family_registers_what_it_claims_to() {
        let names: Vec<&str> = checks().iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            vec![
                "workspace",
                "database",
                "schema",
                "integrity",
                "permissions",
                "database-size",
                "fresh-clone",
            ]
        );
        assert!(checks().iter().all(|c| c.category() == Category::Core));
    }

    #[test]
    fn sizes_read_like_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1024 * 1024 * 3 / 2), "1.5 MiB");
    }
}
