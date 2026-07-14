//! Dolt Storage — only meaningful when the workspace's backend *is* Dolt.
//!
//! Read the backend from the locator (`dx.ctx.backend()`), never from a flag or
//! the environment. On a SQLite workspace every check here returns
//! [`Finding::na`] — not `Ok`, and emphatically not a warning. A user who chose
//! SQLite has no Dolt problem; telling them so ten times is how you teach people
//! to ignore `bd doctor`, and calling it `ok` would inflate the count of things
//! that were *verified* with things that were *skipped*.
//!
//! # `.beads/` **is** the dolt repository
//!
//! This is the fact everything here turns on, and getting it wrong is not a
//! cosmetic error. An earlier version of this file was written by an agent who
//! could not read [`bd_dolt`] and guessed the layout: it looked for the database
//! in `.beads/dolt/.dolt/`, for a bare-number pid in `.beads/dolt-server.pid`,
//! and for a port in `.beads/dolt-server.port`. None of those paths exist. Every
//! check inspected nothing and reported a clean bill of health for a workspace it
//! had never looked at — the exact "reports as coverage" failure this whole
//! design is built against, and worse than having no checks at all.
//!
//! So: **no path, filename or record layout is spelled out in this file.** They
//! come from `bd-dolt`, which owns them:
//!
//! * [`bd_dolt::server::pidfile_path`] / [`bd_dolt::server::PID_FILE`] — the
//!   server record, `.beads/dolt-server.json`, holding `{pid, port}` **together**.
//! * [`bd_dolt::server::read_pidfile`] — the only thing that parses it.
//! * [`bd_dolt::server::LOG_FILE`], [`bd_dolt::server::PORT_ENV`].
//! * [`bd_dolt::which_dolt`] — the same PATH resolution the store itself uses, so
//!   doctor cannot claim to have found a `dolt` that `bd` will then fail to find.
//!
//! The only Dolt-owned paths named here are `.dolt/`, its `noms/manifest` and its
//! `repo_state.json`, which are Dolt's published on-disk format rather than
//! bd's bookkeeping.
//!
//! # The one that actually saves people
//!
//! **A stale or wedged `dolt sql-server` still holding the database lock.** The
//! next `bd` invocation fails with an error that reads exactly like database
//! corruption, and users reach for `rm -rf`. Since `.beads/` *is* the repository,
//! that `rm -rf` destroys the issues, the history and the database in one stroke.
//!
//! # What this family may touch, and what it may not
//!
//! Three lines that are load-bearing, not stylistic:
//!
//! * **Nothing in this file ever writes inside `.dolt/`.** Not the noms `LOCK`,
//!   not the manifest, not a journal. Those are Dolt's; the `LOCK` is *advisory*
//!   (the OS drops it when the holder dies, so its presence proves nothing) and
//!   removing it — or the manifest — turns a recoverable workspace into an
//!   unrecoverable one. Repair here is confined to *bd's own* bookkeeping at the
//!   top level of `.beads/`: the server record and bd's lock files.
//! * **A live pid is not proof of a live server.** Windows recycles process ids
//!   aggressively, so "a process with id 4812 exists" is much weaker evidence than
//!   it looks — 4812 may now be a text editor. Staleness is decided on the
//!   *identity* of the process (is it a `dolt`?) and on whether the recorded port
//!   is actually **serving**.
//! * **"Serving" means the server spoke, not merely that it accepted.** Dolt binds
//!   its listener slightly before it can answer, and a wedged server accepts and
//!   then resets. A connect-only probe reports that server as healthy — which is
//!   precisely the server this family exists to catch. See [`is_serving`].
//!
//! # Not here
//!
//! Everything that needs to *query* the server — schema tables, `dolt_status`,
//! issue counts, phantom databases — is absent, not stubbed as passing. `bd-cli`
//! has no MySQL client of its own, and opening the store to get one would mean
//! `bd doctor` could not run on the broken workspaces that are its entire job.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use async_trait::async_trait;
use bd_dolt::server::{LOG_FILE, PID_FILE, PORT_ENV, PidFile, pidfile_path, read_pidfile};
use bd_storage::Backend;

use super::super::{Category, Check, Dx, Finding, Repair};

pub fn checks() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(DoltBinary),
        Box::new(DoltDatabase),
        Box::new(StorageFormat),
        Box::new(Server),
        Box::new(RemoteVsOrigin),
    ]
}

/// The single sentence a non-Dolt workspace ever hears from this family.
///
/// Every check says exactly this and says it as [`Finding::na`], so the human
/// renderer collapses the whole family to one quiet grey line.
const NOT_DOLT: &str = "not a dolt workspace";

// ---------------------------------------------------------------------------
// Where things are — and every one of these comes from bd-dolt
// ---------------------------------------------------------------------------

/// The Dolt-relevant paths of a workspace that *is* Dolt-backed.
///
/// Constructed only through [`dolt_ws`], which is the one place the backend is
/// read. Holding one of these is proof the backend came from the locator on disk
/// and not from a flag.
struct DoltWs {
    /// The `.beads` directory — which **is** the dolt repository. There is no
    /// `.beads/dolt/`; do not add one.
    beads: PathBuf,
}

impl DoltWs {
    /// `.beads/.dolt` — Dolt's own metadata. **Never written to here.**
    fn dot_dolt(&self) -> PathBuf {
        self.beads.join(".dolt")
    }

    /// `.beads/dolt/.dolt` — where **upstream Go beads** keeps the database, and
    /// the *only* reason this path is named in this file.
    ///
    /// It is not a path we read, write, or repair. It exists so that a workspace
    /// created by the Go implementation gets told what is actually wrong with it
    /// instead of "your database is missing" — and, not incidentally, as a
    /// tombstone: this is the layout the previous version of this file inspected
    /// for *everything*, which is why it inspected nothing.
    fn upstream_dot_dolt(&self) -> PathBuf {
        self.beads.join("dolt").join(".dolt")
    }

    /// The noms manifest. Its second field is the storage format.
    fn manifest(&self) -> PathBuf {
        self.dot_dolt().join("noms").join("manifest")
    }

    fn repo_state(&self) -> PathBuf {
        self.dot_dolt().join("repo_state.json")
    }

    /// `.beads/dolt-server.json`. From `bd-dolt`, never spelled out here: a
    /// second copy of this filename is the bug that produced this file's rewrite.
    fn pid_file(&self) -> PathBuf {
        pidfile_path(&self.beads)
    }

    /// Where the server's stdout and stderr went. The first place to look when a
    /// server is wedged, and useless if we cannot name it.
    fn log_file(&self) -> PathBuf {
        self.beads.join(LOG_FILE)
    }
}

/// The gate. `None` means "this check has nothing to say here".
///
/// The backend comes from `ctx.backend()`, which reads the locator that `bd init`
/// wrote. A `--backend` flag could not reach this code if it tried.
fn dolt_ws(dx: &Dx<'_>) -> Option<DoltWs> {
    match dx.ctx.backend()? {
        Backend::Dolt => dx.dir.as_ref().map(|d| DoltWs { beads: d.clone() }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 1. Is there a dolt binary at all?
// ---------------------------------------------------------------------------

/// A Dolt workspace with no `dolt` on `PATH` is a real, diagnosable state: the
/// workspace is fine, the *machine* is missing a tool, and every command that
/// touches the store will fail until it is installed. Saying so once, clearly, is
/// worth more than the ten downstream failures it causes.
///
/// The resolution is [`bd_dolt::which_dolt`] and nothing else. Doctor once had its
/// own `which`, which tried every `PATHEXT` — so on a machine with only a
/// `dolt.cmd` shim, doctor would report `ok` about a binary `bd` itself could
/// never find. A diagnostic that disagrees with the thing it is diagnosing is not
/// a diagnostic.
struct DoltBinary;

#[async_trait]
impl Check for DoltBinary {
    fn name(&self) -> &'static str {
        "dolt binary"
    }

    fn category(&self) -> Category {
        Category::Dolt
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        if dolt_ws(dx).is_none() {
            return Finding::na(self.name(), NOT_DOLT);
        }

        let Some(path) = bd_dolt::which_dolt() else {
            return Finding::error(self.name(), "no `dolt` on PATH")
                .detail(
                    "This workspace's backend is dolt, so every command that opens the store \
                     needs the dolt binary. Nothing is wrong with the workspace itself.",
                )
                .fix("install dolt — https://docs.dolthub.com/introduction/installation");
        };

        // Only now that we know it exists do we pay for a process — and we run
        // *the path we resolved*, not a bare `dolt`, so we cannot report the
        // version of one binary and the location of another. `dolt version`
        // touches no database and takes no lock; it is safe under rule 3.
        match run_cmd(path.as_os_str(), &["version"], Duration::from_secs(5)).await {
            Some(out) if out.status.success() => {
                let v = first_line(&String::from_utf8_lossy(&out.stdout));
                Finding::ok(self.name(), if v.is_empty() { "found".into() } else { v })
                    .detail(path.display().to_string())
            }
            // On PATH but will not run: a broken install, a wrong architecture, a
            // shim that shells out to something that isn't there. Not `ok`, and
            // not the workspace's fault either.
            _ => Finding::warn(self.name(), "`dolt` is on PATH but would not run")
                .detail(path.display().to_string())
                .fix("check the install: `dolt version`"),
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Is there a dolt database on disk?
// ---------------------------------------------------------------------------

/// A Dolt-backed workspace with no `.beads/.dolt/` has not been initialised —
/// every store operation will fail, and `bd_dolt::open` says so in as many words.
/// This is the one check in the family that distinguishes "not set up" from "set
/// up and broken", and the others lean on it: they report the *absence* of the
/// database as [`Finding::na`], so the missing database is named exactly once.
struct DoltDatabase;

#[async_trait]
impl Check for DoltDatabase {
    fn name(&self) -> &'static str {
        "dolt database"
    }

    fn category(&self) -> Category {
        Category::Dolt
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(ws) = dolt_ws(dx) else {
            return Finding::na(self.name(), NOT_DOLT);
        };

        if !ws.dot_dolt().is_dir() {
            // Before blaming the user's backup: is the database simply somewhere
            // this binary does not look? Upstream Go beads keeps it in
            // `.beads/dolt/`, and a workspace created by it is not one this port
            // can open. "The database is missing" would be true and useless — the
            // database is *right there*, under a name we do not read.
            if ws.upstream_dot_dolt().is_dir() {
                return Finding::error(self.name(), "the dolt database is in the Go layout")
                    .detail(format!(
                        "{} does not exist, but {} does. This workspace was created by the Go \
                         implementation of beads, which keeps the dolt repository in \
                         `.beads/dolt/`; this port makes `.beads/` itself the repository. The data \
                         is intact — it is in a place this binary does not read.",
                        ws.dot_dolt().display(),
                        ws.upstream_dot_dolt().display()
                    ))
                    .fix(
                        "use the Go `bd` for this workspace, or export and re-import: \
                         `bd export --format=jsonl` with the Go binary, then `bd init \
                         --backend=dolt` and `bd import` with this one",
                    );
            }

            return Finding::error(self.name(), "the dolt database is missing")
                .detail(format!(
                    "the locator says this workspace is dolt-backed, but {} does not exist. \
                     The database is missing, not merely closed.",
                    ws.dot_dolt().display()
                ))
                .fix("`bd init --backend=dolt` for a new project, or restore .beads/ from a backup");
        }

        if !ws.manifest().is_file() {
            // `.dolt/` without a manifest: an interrupted `dolt init`, or a
            // partial copy. Warn, not error — the data may still be recoverable
            // and this port cannot tell.
            return Finding::warn(self.name(), "the dolt database looks half-initialised")
                .detail(format!(
                    "{} exists but {} does not",
                    ws.dot_dolt().display(),
                    ws.manifest().display()
                ))
                .fix("`dolt status` in .beads to see what dolt makes of it");
        }

        Finding::ok(self.name(), "present").detail(ws.dot_dolt().display().to_string())
    }
}

// ---------------------------------------------------------------------------
// 3. Storage format
// ---------------------------------------------------------------------------

/// Dolt's noms manifest names the storage format in its second colon-separated
/// field. `__DOLT__` is the current one; `__LD_1__` is the pre-1.0 format, which
/// current Dolt refuses to open until it is migrated. Reading it costs one small
/// file read and takes no lock — which is the whole reason to do it from disk
/// rather than by asking a server that may not be running.
struct StorageFormat;

#[derive(Debug, PartialEq, Eq)]
enum Format {
    /// The current format.
    Current,
    /// Old, or a development format. Carries the raw tag.
    Legacy(String),
    /// We read the manifest and did not recognise what we found. Not `ok`.
    Unrecognised(String),
}

#[async_trait]
impl Check for StorageFormat {
    fn name(&self) -> &'static str {
        "dolt storage format"
    }

    fn category(&self) -> Category {
        Category::Dolt
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(ws) = dolt_ws(dx) else {
            return Finding::na(self.name(), NOT_DOLT);
        };

        let manifest = ws.manifest();
        if !manifest.is_file() {
            // `dolt database` already reports this, with the right severity.
            // Saying it twice would double-count one problem — and saying `ok`
            // would claim we verified a format we never read.
            return Finding::na(self.name(), "no dolt database to inspect");
        }

        let raw = match std::fs::read_to_string(&manifest) {
            Ok(r) => r,
            Err(e) => {
                return Finding::unknown(
                    self.name(),
                    format!("cannot read {}: {e}", manifest.display()),
                );
            }
        };

        match classify_format(&raw) {
            Some(Format::Current) => Finding::ok(self.name(), "__DOLT__ (current)"),
            Some(Format::Legacy(tag)) => {
                Finding::warn(self.name(), format!("legacy storage format {tag}"))
                    .detail(format!(
                        "{} declares {tag}; current dolt reads only __DOLT__",
                        manifest.display()
                    ))
                    .fix("`cd .beads && dolt migrate` — take a backup of .beads/ first")
            }
            Some(Format::Unrecognised(tag)) => Finding::unknown(
                self.name(),
                format!("{} declares an unfamiliar format tag {tag}", manifest.display()),
            ),
            None => Finding::unknown(
                self.name(),
                format!("{} does not look like a noms manifest", manifest.display()),
            ),
        }
    }
}

/// The manifest is one line of colon-separated fields; the format tag is the
/// second, and always looks like `__SOMETHING__`.
///
/// Deliberately tolerant: rather than asserting the field index, it takes the
/// first `__…__` token among the leading fields. A manifest layout we do not
/// recognise yields `None`, which the caller turns into [`Status::Unknown`] —
/// never `ok`. Being wrong about dolt's file format must not silently report a
/// legacy database as healthy.
///
/// [`Status::Unknown`]: super::super::Status::Unknown
fn classify_format(manifest: &str) -> Option<Format> {
    let line = manifest.lines().next()?.trim();
    let tag = line
        .split(':')
        .take(4)
        .map(str::trim)
        .find(|f| f.len() >= 4 && f.starts_with("__") && f.ends_with("__"))?;

    Some(match tag {
        "__DOLT__" => Format::Current,
        "__LD_1__" | "__DOLT_DEV__" | "__DOLT_1__" => Format::Legacy(tag.to_string()),
        other => Format::Unrecognised(other.to_string()),
    })
}

// ---------------------------------------------------------------------------
// 4. The server — the check this family exists for
// ---------------------------------------------------------------------------

/// A `dolt sql-server` that died without cleaning up leaves `.beads/dolt-server.json`
/// behind. Worse, one that is *alive and wedged* holds dolt's database lock: the
/// next `bd` command fails with an error that reads **exactly** like database
/// corruption, and the user deletes `.beads/` to fix it — which, because `.beads/`
/// *is* the dolt repository, destroys every issue they have.
///
/// So this check's whole job is to be believed. It states, inside the finding
/// itself, that this is not corruption and that `.beads/.dolt/` must not be
/// deleted.
struct Server;

/// A record younger than this gets the benefit of the doubt when its port has
/// gone quiet.
///
/// Note what changed with the real layout: `bd-dolt` writes the record *after*
/// the server has answered on its port, not when it spawns. So "the record is
/// young and the port is silent" no longer means "still starting" — it means the
/// server was serving moments ago and is briefly not. Restarting, saturated, or
/// dying. Either way, calling that an `error` on the first sighting is crying
/// wolf; a second look a moment later settles it.
const STARTUP_GRACE: Duration = Duration::from_secs(30);

/// What is on disk, in one place. `bd-dolt` does the parsing; we only decide what
/// it means.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    /// No server record. Normal: bd starts one on demand.
    Absent,
    /// The file is there and we could not read it *at all* (permissions, I/O).
    /// **Not** the same as absent — treating an unreadable file as "no server" is
    /// how a diagnostic quietly stops diagnosing.
    Unreadable(String),
    /// The file is there and `bd-dolt` cannot parse it: a torn write, a
    /// hand-edit. Carries the raw bytes, because the evidence is the point.
    ///
    /// Worth cleaning even though `bd-dolt` tolerates it: `try_adopt` reads it as
    /// *absent* and, crucially, does **not** delete it — so it lingers forever.
    Corrupt(String),
    /// A record `bd-dolt` itself parsed. pid and port arrive **together**; there
    /// is no state in which we know one and not the other.
    Present(PidFile),
}

/// What the filesystem and the OS say. Gathered impurely, judged purely.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerFacts {
    record: Record,
    /// Age of the server record, if we could stat it.
    record_age: Option<Duration>,
    /// `None` when there was no pid to probe.
    liveness: Option<Liveness>,
    /// Is the *recorded* port serving MySQL? `None` when there was no port.
    serving: Option<bool>,
    /// `BD_DOLT_PORT`, and whether something is serving there. `None` when unset.
    env_port: Option<(u16, bool)>,
}

/// What the OS says about a process id.
///
/// The `image` is the point. On Windows, ids are reused within seconds, so
/// `Alive { image: None }` is barely evidence at all; `Alive { image: "dolt.exe" }`
/// is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Liveness {
    Alive { image: Option<String> },
    Dead,
    /// The probe failed. **Not** the same as `Dead` — treating a failed probe as
    /// "the process is gone" would have doctor cheerfully delete the record of a
    /// perfectly healthy server.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    /// No record and no `BD_DOLT_PORT`. Normal: bd starts a server on demand.
    NotTracked,
    /// `BD_DOLT_PORT` names a port that is serving. bd will **adopt** that server
    /// rather than start one, and will not touch our record — so the record is
    /// not even consulted here, exactly as `bd-dolt` does not consult it.
    Adopted { port: u16 },
    /// `BD_DOLT_PORT` is set and nothing is answering there. Not a fault: bd will
    /// start a server on precisely that port.
    EnvPortIdle { port: u16 },
    Running { pid: u32, port: u16 },
    /// Alive, is a dolt, not serving — but the record is younger than the grace.
    Starting { pid: u32, port: u16 },
    /// Alive, is a dolt, and **not serving** long past the grace. This is the
    /// wedged server, and it is holding the database lock.
    Wedged { pid: u32, port: u16 },
    /// The process is gone. **Stale.**
    Dead { pid: u32, port: u16 },
    /// A process with that id exists but it is not a dolt. On Windows this is the
    /// common case for a stale record. **Stale.**
    Reused { pid: u32, image: String },
    /// The record exists and `bd-dolt` cannot parse it. **Stale.**
    Corrupt { raw: String },
    /// We could not tell. Report `unknown`, and touch nothing.
    Undeterminable { why: String },
}

impl Verdict {
    /// Only these three are safe to clean up automatically: in each, the process
    /// bd recorded is provably not serving this workspace.
    ///
    /// `Wedged` is deliberately absent. Its record is *accurate* — there really is
    /// a dolt on that pid — and deleting it would only make the next `bd` start a
    /// second server against a database dolt has already locked, turning a
    /// diagnosable problem into a confusing one. Doctor does not kill processes.
    fn is_stale(&self) -> bool {
        matches!(
            self,
            Verdict::Dead { .. } | Verdict::Reused { .. } | Verdict::Corrupt { .. }
        )
    }
}

/// The whole state machine, as one pure function — because this is the logic that
/// decides whether doctor deletes a file, and it must be testable without a dolt
/// binary, a server, or a real process to kill.
fn assess(f: &ServerFacts) -> Verdict {
    // `BD_DOLT_PORT` first, and it wins outright. `bd_dolt::server::try_adopt`
    // consults *only* that port when it is set — it never opens the record — so a
    // doctor that diagnosed the record here would be diagnosing a file bd is about
    // to ignore.
    if let Some((port, serving)) = f.env_port {
        return if serving {
            Verdict::Adopted { port }
        } else {
            Verdict::EnvPortIdle { port }
        };
    }

    let rec = match &f.record {
        Record::Absent => return Verdict::NotTracked,
        Record::Unreadable(why) => {
            return Verdict::Undeterminable {
                why: format!("cannot read {PID_FILE}: {why}"),
            };
        }
        Record::Corrupt(raw) => return Verdict::Corrupt { raw: raw.clone() },
        Record::Present(rec) => *rec,
    };

    let liveness = match &f.liveness {
        None => {
            return Verdict::Undeterminable {
                why: "the process was not probed".to_string(),
            };
        }
        Some(Liveness::Unknown(why)) => {
            return Verdict::Undeterminable { why: why.clone() };
        }
        Some(l) => l,
    };

    let image = match liveness {
        // The recorded process is gone, so the record is false regardless of what
        // may now be listening on its port. (`bd-dolt` decides adoption on the
        // port alone; doctor has the *extra* evidence of the pid, and a record
        // naming a dead process is stale even if a stranger happens to answer
        // there — adopting a stranger's database is not a recovery.)
        Liveness::Dead => {
            return Verdict::Dead {
                pid: rec.pid,
                port: rec.port,
            };
        }
        Liveness::Alive { image } => image,
        Liveness::Unknown(_) => unreachable!("handled above"),
    };

    // A named process that is not a dolt means the id was recycled. A process we
    // could not name is *not* treated as recycled — absence of evidence is not
    // evidence, and the penalty for guessing wrong is deleting the record of a
    // live server.
    if let Some(image) = image
        && !is_dolt_image(image)
    {
        return Verdict::Reused {
            pid: rec.pid,
            image: image.clone(),
        };
    }

    match f.serving {
        Some(true) => Verdict::Running {
            pid: rec.pid,
            port: rec.port,
        },
        Some(false) => {
            if f.record_age.is_some_and(|a| a < STARTUP_GRACE) {
                Verdict::Starting {
                    pid: rec.pid,
                    port: rec.port,
                }
            } else {
                Verdict::Wedged {
                    pid: rec.pid,
                    port: rec.port,
                }
            }
        }
        None => Verdict::Undeterminable {
            why: format!("port {} was not probed", rec.port),
        },
    }
}

fn is_dolt_image(image: &str) -> bool {
    // "dolt", "dolt.exe", "/usr/local/bin/dolt" — and not "doltfoo".
    let name = image
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(image)
        .to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    stem == "dolt"
}

#[async_trait]
impl Check for Server {
    fn name(&self) -> &'static str {
        "dolt server"
    }

    fn category(&self) -> Category {
        Category::Dolt
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(ws) = dolt_ws(dx) else {
            return Finding::na(self.name(), NOT_DOLT);
        };

        let n = self.name();
        match assess(&gather(&ws).await) {
            Verdict::NotTracked => Finding::ok(n, "no server is running (bd starts one on demand)"),

            Verdict::Adopted { port } => Finding::ok(
                n,
                format!("adopting the server on 127.0.0.1:{port} ({PORT_ENV})"),
            )
            .detail(format!(
                "{PORT_ENV}={port} names a server that is answering; bd will use it rather than \
                 start one of its own, and will not stop it."
            )),

            Verdict::EnvPortIdle { port } => Finding::ok(
                n,
                format!("no server is running ({PORT_ENV}={port}; bd will start one there)"),
            ),

            Verdict::Running { pid, port } => {
                Finding::ok(n, format!("running (pid {pid}, port {port})"))
            }

            Verdict::Starting { pid, port } => {
                Finding::warn(n, "the dolt server is not answering just now")
                    .detail(format!(
                        "pid {pid} is alive and is a dolt, but nothing is serving on \
                         127.0.0.1:{port}. {} was written less than {}s ago, so the server was \
                         answering very recently — it is most likely restarting, not wedged.",
                        ws.pid_file().display(),
                        STARTUP_GRACE.as_secs()
                    ))
                    .fix("re-run `bd doctor` in a moment")
            }

            // The one that is genuinely broken: something is holding the database
            // and will not talk to us.
            Verdict::Wedged { pid, port } => Finding::error(
                n,
                format!("a dolt server (pid {pid}) is running but not serving"),
            )
            .detail(format!(
                "nothing is answering on 127.0.0.1:{port}, yet pid {pid} is alive and is a dolt \
                 process — it is holding the database lock while wedged, so the next `bd` command \
                 will fail with a lock error.\n\
                 This is NOT database corruption. Do not delete .beads/.dolt/ — it IS the \
                 database, and dolt's lock is advisory: removing it destroys nothing but does not \
                 help either.\n\
                 The server's own log is {}.",
                ws.log_file().display()
            ))
            .fix(format!(
                "stop it and let bd start a fresh one: kill {pid}, then remove {}",
                ws.pid_file().display()
            )),

            // ---- the rm -rf scenario ----
            Verdict::Dead { pid, port } => {
                Finding::warn(n, "a dead dolt server left its record behind")
                    .detail(format!(
                        "{} names process {pid} on port {port}, and that process no longer exists.\n\
                         This is NOT database corruption. Do not delete .beads/.dolt/ — it IS the \
                         database.",
                        ws.pid_file().display()
                    ))
                    .fix(fix_hint(&ws))
            }

            Verdict::Reused { pid, image } => {
                Finding::warn(n, "the recorded dolt server is gone (its pid was recycled)")
                    .detail(format!(
                        "{} names process {pid}, but that id now belongs to `{image}`, not dolt. \
                         The server bd recorded is gone.\n\
                         This is NOT database corruption. Do not delete .beads/.dolt/ — it IS the \
                         database.",
                        ws.pid_file().display()
                    ))
                    .fix(fix_hint(&ws))
            }

            Verdict::Corrupt { raw } => Finding::warn(n, "the dolt server record is unreadable")
                .detail(format!(
                    "{} contains {raw:?}, which is not a server record. bd ignores it, but it will \
                     never clean it up either.",
                    ws.pid_file().display()
                ))
                .fix(fix_hint(&ws)),

            Verdict::Undeterminable { why } => Finding::unknown(n, why),
        }
    }

    /// Removes **bd's own** bookkeeping, and only when the state is re-confirmed
    /// stale at repair time. Three properties matter here:
    ///
    /// * It re-gathers. The finding it is handed was produced before `--fix` asked
    ///   to repair, and a server may have started in between. Acting on a stale
    ///   verdict is how a repair kills a healthy workspace.
    /// * It **declines, out loud**, when the state is no longer stale. Saying
    ///   `Unfixable` there would be a lie — doctor is perfectly able to delete the
    ///   file and is choosing not to, which is the most important work it does.
    /// * It never goes near `.dolt/`. Dolt's own `LOCK` and manifest are its
    ///   business; deleting them is the unrecoverable mistake this whole check is
    ///   trying to talk the user *out of*.
    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(ws) = dolt_ws(dx) else {
            return Ok(Repair::Unfixable);
        };

        let verdict = assess(&gather(&ws).await);
        if !verdict.is_stale() {
            return Ok(Repair::Declined(decline(&verdict)));
        }

        let path = ws.pid_file();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(Repair::Did(format!(
                "removed the stale dolt server record ({}); .beads/.dolt/ was not touched",
                file_name(&path)
            ))),
            // Someone beat us to it between the re-gather and here. Nothing was
            // done, and reporting "fixed" for work we did not do is exactly the
            // dishonesty this seam exists to prevent.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Repair::Declined(format!(
                "{} was already gone by the time --fix ran; nothing to do",
                file_name(&path)
            ))),
            Err(e) => bail!("cannot remove {}: {e}", path.display()),
        }
    }
}

/// Why `--fix` is keeping its hands off a record it *could* delete.
fn decline(v: &Verdict) -> String {
    match v {
        Verdict::Running { pid, port } => format!(
            "the server came back: pid {pid} is serving on port {port}. Deleting its record now \
             would make the next `bd` start a second server against a locked database."
        ),
        Verdict::Starting { pid, .. } => format!(
            "pid {pid} is alive and answered very recently; it looks like it is restarting, not \
             stale. Re-run `bd doctor` in a moment."
        ),
        Verdict::Wedged { pid, port } => format!(
            "doctor does not kill processes. Pid {pid} is a live dolt holding the database lock \
             but not serving on port {port}; deleting its record would only make the next `bd` \
             start a second server that dolt's lock will reject. Kill {pid} yourself."
        ),
        Verdict::Adopted { port } => format!(
            "{PORT_ENV} points at a live server on port {port}; bd is adopting it, and it is not \
             ours to stop."
        ),
        Verdict::NotTracked | Verdict::EnvPortIdle { .. } => {
            "there is no server record left to remove".to_string()
        }
        Verdict::Undeterminable { why } => format!(
            "re-checking left the state undetermined ({why}), and repairing what cannot be \
             diagnosed is how --fix becomes the bug it was run to cure."
        ),
        // Unreachable: `is_stale()` sent these to the repair.
        Verdict::Dead { .. } | Verdict::Reused { .. } | Verdict::Corrupt { .. } => {
            "the record is stale after all".to_string()
        }
    }
}

fn fix_hint(ws: &DoltWs) -> String {
    format!(
        "`bd doctor --fix` — it removes {}, and never touches .beads/.dolt/",
        file_name(&ws.pid_file())
    )
}

/// Everything impure, in one place: read one file, ask the OS about a process,
/// knock on up to two ports. Nothing here decides anything.
async fn gather(ws: &DoltWs) -> ServerFacts {
    let path = ws.pid_file();
    let record = read_record(&path, &ws.beads);
    let record_age = file_age(&path);

    let (liveness, serving) = match &record {
        Record::Present(rec) => (
            Some(probe_process(rec.pid).await),
            Some(is_serving(rec.port).await),
        ),
        _ => (None, None),
    };

    // A workspace with no record of its own may still have a perfectly good
    // server: `BD_DOLT_PORT` points bd at one it did not start (docker, CI, a
    // dev's terminal), and bd will adopt whatever answers there.
    let env_port = match env_port() {
        Some(p) => Some((p, is_serving(p).await)),
        None => None,
    };

    ServerFacts {
        record,
        record_age,
        liveness,
        serving,
        env_port,
    }
}

/// Read the server record — and let **`bd-dolt` do the parsing**.
///
/// The raw bytes are read only to carry as evidence when `bd-dolt` says it cannot
/// parse them. Nothing here knows the record's shape, which is the entire point:
/// the last version of this file did, and it was wrong.
fn read_record(path: &Path, beads: &Path) -> Record {
    match std::fs::read(path) {
        Ok(bytes) => match read_pidfile(beads) {
            Some(rec) => Record::Present(rec),
            None => Record::Corrupt(snippet(&bytes)),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::Absent,
        // Permissions, a bad symlink, an I/O error. **Not** "no server".
        Err(e) => Record::Unreadable(e.to_string()),
    }
}

/// Enough of the bad file to see what it is, and not a byte more.
fn snippet(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(120)];
    String::from_utf8_lossy(head).into_owned()
}

/// `BD_DOLT_PORT`, if the user set one to something meaningful.
///
/// `0` is "let the OS pick", which is not an override — the same rule `bd-dolt`
/// applies. (Its `parse_port` is private; this is the one behaviour, rather than
/// path, that is still restated here.)
fn env_port() -> Option<u16> {
    std::env::var(PORT_ENV)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
}

fn file_age(p: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(p).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Is a MySQL server **answering** on this loopback port?
///
/// A TCP connect is not enough, and the gap between "accepted" and "answered" is
/// this family's entire subject. Dolt binds its listener slightly before it can
/// serve, and a *wedged* server accepts a connection and then resets it — so a
/// connect-only probe reports `Running` about precisely the server this check
/// exists to catch. A MySQL server speaks first; requiring its greeting settles
/// the question.
///
/// This is `bd-dolt`'s own probe, not a copy of it. Doctor carried a copy for one
/// wave and the copy was connect-only — a copy of *semantics*, which is the same
/// class of bug as the copied filename that put this whole family on the wrong
/// paths. Asking the same question the same way is the entire point.
async fn is_serving(port: u16) -> bool {
    bd_dolt::server::probe(port, bd_dolt::server::PROBE_TIMEOUT).await
}

// --- process probing, per platform -----------------------------------------

#[cfg(windows)]
async fn probe_process(pid: u32) -> Liveness {
    let filter = format!("PID eq {pid}");
    let args = ["/FI", filter.as_str(), "/NH", "/FO", "CSV"];
    match run_cmd("tasklist".as_ref(), &args, Duration::from_secs(5)).await {
        Some(out) => parse_tasklist(&String::from_utf8_lossy(&out.stdout)),
        None => Liveness::Unknown("could not run `tasklist`".to_string()),
    }
}

/// `tasklist /NH /FO CSV` prints one quoted row per match, and a banner when there
/// is none. Note that it exits 0 in *both* cases, so the exit code tells you
/// nothing and the text is the only signal.
#[cfg(any(windows, test))]
fn parse_tasklist(text: &str) -> Liveness {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("INFO:") {
            return Liveness::Dead;
        }
        if line.starts_with("ERROR") {
            return Liveness::Unknown(line.to_string());
        }
        if let Some(rest) = line.strip_prefix('"')
            && let Some(image) = rest.split('"').next()
        {
            return Liveness::Alive {
                image: Some(image.to_ascii_lowercase()),
            };
        }
    }
    // No rows, no banner: no such process.
    Liveness::Dead
}

#[cfg(unix)]
async fn probe_process(pid: u32) -> Liveness {
    // Linux: /proc is authoritative and costs no process at all.
    if Path::new("/proc").is_dir() {
        return match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            Ok(name) => Liveness::Alive {
                image: Some(name.trim().to_ascii_lowercase()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
            Err(e) => Liveness::Unknown(format!("cannot read /proc/{pid}/comm: {e}")),
        };
    }

    // macOS and the BSDs.
    let pid_s = pid.to_string();
    let args = ["-p", pid_s.as_str(), "-o", "comm="];
    match run_cmd("ps".as_ref(), &args, Duration::from_secs(5)).await {
        Some(out) => {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !name.is_empty() {
                Liveness::Alive {
                    image: Some(name.to_ascii_lowercase()),
                }
            } else {
                // `ps -p` exits nonzero precisely when there is no such process.
                Liveness::Dead
            }
        }
        None => Liveness::Unknown("could not run `ps`".to_string()),
    }
}
// ---------------------------------------------------------------------------
// 6. Dolt remotes vs the git origin
// ---------------------------------------------------------------------------

/// Beads syncs its issues through Dolt, and its code through git. Pointing a Dolt
/// remote at the git origin aims both at the same endpoint, and the two then fight
/// over the same refs.
///
/// Read from `.beads/.dolt/repo_state.json` rather than by querying `dolt_remotes`
/// on a server: it is a plain file, it needs no server, and it takes no lock.
struct RemoteVsOrigin;

#[async_trait]
impl Check for RemoteVsOrigin {
    fn name(&self) -> &'static str {
        "dolt remote vs git origin"
    }

    fn category(&self) -> Category {
        Category::Dolt
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(ws) = dolt_ws(dx) else {
            return Finding::na(self.name(), NOT_DOLT);
        };

        // No database at all: `dolt database` owns that finding, and there is
        // nothing here to compare. Not `ok` — we verified nothing.
        if !ws.dot_dolt().is_dir() {
            return Finding::na(self.name(), "no dolt database to inspect");
        }

        let state = ws.repo_state();
        let remotes = match std::fs::read_to_string(&state) {
            Err(_) => Vec::new(),
            Ok(raw) => match remotes_from_repo_state(&raw) {
                Some(r) => r,
                None => {
                    return Finding::unknown(
                        self.name(),
                        format!("cannot parse {}", state.display()),
                    );
                }
            },
        };

        if remotes.is_empty() {
            return Finding::ok(self.name(), "no dolt remotes configured");
        }

        // Beads does not require git. No repo, no origin, nothing to disagree —
        // and nothing verified, either.
        let Some(root) = &dx.root else {
            return Finding::na(self.name(), "not a git repository");
        };
        let Some(origin) = git_origin(root).await else {
            return Finding::na(self.name(), "no git origin to compare against");
        };

        let want = canonical_remote(&origin);
        let clashing: Vec<&str> = remotes
            .iter()
            .filter(|(_, url)| canonical_remote(url) == want)
            .map(|(name, _)| name.as_str())
            .collect();

        if clashing.is_empty() {
            return Finding::ok(
                self.name(),
                format!("{} dolt remote(s), none matching the git origin", remotes.len()),
            );
        }

        Finding::warn(
            self.name(),
            format!(
                "{} dolt remote(s) point at the git origin: {}",
                clashing.len(),
                clashing.join(", ")
            ),
        )
        .detail(format!(
            "git origin: {origin}\n\
             beads syncs issues through dolt and code through git; aiming both at one endpoint \
             makes them fight over the same refs."
        ))
        .fix(format!(
            "point the dolt remote somewhere else: `cd .beads && dolt remote remove {}`",
            clashing[0]
        ))
    }
}

/// `.dolt/repo_state.json` → `[(name, url)]`.
///
/// `None` means the file is not JSON we understand — which becomes `unknown`, not
/// `ok`. Returning "no remotes" for a file we failed to read would be a check that
/// reports as coverage while checking nothing.
fn remotes_from_repo_state(raw: &str) -> Option<Vec<(String, String)>> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    // A repo_state with no `remotes` key at all is normal (no remotes yet).
    let Some(remotes) = v.get("remotes") else {
        return Some(Vec::new());
    };
    let map = remotes.as_object()?;
    let mut out = Vec::new();
    for (name, entry) in map {
        if let Some(url) = entry.get("url").and_then(|u| u.as_str()) {
            out.push((name.clone(), url.to_string()));
        }
    }
    out.sort();
    Some(out)
}

/// Enough normalisation to tell "the same remote" from "a different remote":
/// scheme, credentials, `.git`, trailing slash, and scp-style `git@host:path` all
/// have to go, or `git@github.com:acme/x.git` and `https://github.com/acme/x` read
/// as two different places when they are one.
fn canonical_remote(url: &str) -> String {
    let s = url.trim().trim_end_matches('/');

    // scheme://
    let s = match s.split_once("://") {
        Some((_, rest)) => rest,
        None => s,
    };
    // user[:pass]@host — drop the credentials, keep the host.
    let s = match s.split_once('@') {
        Some((_, rest)) => rest,
        None => s,
    };
    // scp-style `host:org/repo` → `host/org/repo`. Guard against a port
    // (`host:22/x`), which is not the same thing.
    let s = match s.split_once(':') {
        Some((host, rest)) if !rest.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
            format!("{host}/{rest}")
        }
        _ => s.to_string(),
    };

    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    s.trim_end_matches('/').to_ascii_lowercase()
}

async fn git_origin(root: &Path) -> Option<String> {
    let root_s = root.to_string_lossy().into_owned();
    let args = ["-C", root_s.as_str(), "remote", "get-url", "origin"];
    let out = run_cmd("git".as_ref(), &args, Duration::from_secs(5)).await?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// Rule 5, enforced in one place: a check may shell out, but it may not hang. A
/// timed-out or unspawnable command yields `None`, which callers must turn into a
/// warning or an `unknown` — never an `ok`.
///
/// Takes an `&OsStr` so a caller can run a *resolved path* rather than a bare
/// name; `dolt binary` does exactly that, so it cannot report the version of one
/// binary next to the location of another.
async fn run_cmd(
    program: &std::ffi::OsStr,
    args: &[&str],
    limit: Duration,
) -> Option<std::process::Output> {
    let fut = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(out)) => Some(out),
        _ => None,
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    // -----------------------------------------------------------------------
    // The layout. These are the assertions the last version of this file could
    // not have made, and their absence is what let it inspect paths that never
    // existed while reporting a clean bill of health.
    // -----------------------------------------------------------------------

    /// `.beads/` **is** the dolt repository. Every path this family reads is
    /// anchored there, and the two bd owns come from `bd-dolt`.
    #[test]
    fn every_path_agrees_with_bd_dolt() {
        let ws = DoltWs {
            beads: PathBuf::from("/w/.beads"),
        };

        // bd's own bookkeeping: whatever bd-dolt says, and nothing else. If
        // bd-dolt renames the record, this follows it for free — which is the
        // whole point.
        assert_eq!(ws.pid_file(), bd_dolt::server::pidfile_path(Path::new("/w/.beads")));
        assert_eq!(ws.pid_file().file_name().unwrap(), PID_FILE);
        assert_eq!(ws.log_file().file_name().unwrap(), LOG_FILE);

        // Dolt's own: directly under `.beads`, never under a `.beads/dolt/` that
        // does not exist.
        assert_eq!(ws.dot_dolt(), Path::new("/w/.beads/.dolt"));
        assert_eq!(ws.manifest(), Path::new("/w/.beads/.dolt/noms/manifest"));
        assert_eq!(ws.repo_state(), Path::new("/w/.beads/.dolt/repo_state.json"));
    }

    /// The record `bd-dolt` writes is the record doctor reads — pid and port in
    /// one JSON object, parsed by `bd-dolt` and by nothing else.
    #[test]
    fn the_server_record_is_read_by_bd_dolt_and_not_reimplemented() {
        let dir = tmp("record");
        let path = pidfile_path(&dir);

        assert_eq!(read_record(&path, &dir), Record::Absent);

        // The real thing, serialized by the real type.
        let rec = PidFile {
            pid: 4812,
            port: 51234,
        };
        std::fs::write(&path, serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(read_record(&path, &dir), Record::Present(rec));

        // A torn write. bd-dolt reads this as *absent* and — importantly — never
        // deletes it, so doctor is the only thing that will ever clean it up.
        std::fs::write(&path, "{ trunca").unwrap();
        assert_eq!(
            read_record(&path, &dir),
            Record::Corrupt("{ trunca".to_string())
        );

        // The old layout: a bare pid, which is what doctor used to *write* its
        // assumptions against. It is not a record, and it must not be mistaken
        // for one.
        std::fs::write(&path, "4812").unwrap();
        assert!(matches!(read_record(&path, &dir), Record::Corrupt(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- the state machine that decides whether we delete a file ------------

    fn rec(pid: u32, port: u16) -> Record {
        Record::Present(PidFile { pid, port })
    }

    /// A record an hour old, whose port is silent. The interesting axis is then
    /// the process.
    fn facts(record: Record, liveness: Option<Liveness>) -> ServerFacts {
        ServerFacts {
            record,
            record_age: Some(Duration::from_secs(3600)),
            liveness,
            serving: Some(false),
            env_port: None,
        }
    }

    fn alive(image: &str) -> Option<Liveness> {
        Some(Liveness::Alive {
            image: Some(image.to_string()),
        })
    }

    #[test]
    fn no_record_is_not_a_problem() {
        // Absence is not failure: bd starts a server on demand.
        let v = assess(&ServerFacts {
            record: Record::Absent,
            record_age: None,
            liveness: None,
            serving: None,
            env_port: None,
        });
        assert_eq!(v, Verdict::NotTracked);
        assert!(!v.is_stale());
    }

    #[test]
    fn a_dead_process_is_stale_and_repairable() {
        let v = assess(&facts(rec(4812, 51234), Some(Liveness::Dead)));
        assert_eq!(
            v,
            Verdict::Dead {
                pid: 4812,
                port: 51234
            }
        );
        assert!(v.is_stale(), "this is the case that saves people from rm -rf");
    }

    /// The Windows trap. A record naming 4812 plus "a process with id 4812 exists"
    /// is *not* evidence the server is alive — Windows recycles ids within
    /// seconds. Identity is what settles it.
    #[test]
    fn a_recycled_pid_is_stale_not_running() {
        let v = assess(&facts(rec(4812, 51234), alive("code.exe")));
        assert_eq!(
            v,
            Verdict::Reused {
                pid: 4812,
                image: "code.exe".to_string()
            }
        );
        assert!(v.is_stale());
    }

    /// The mirror of the above, and the more dangerous direction: when we cannot
    /// name the process, we must NOT conclude the pid was recycled. Guessing wrong
    /// here deletes the record of a healthy, running server.
    #[test]
    fn an_unnamed_live_process_is_never_assumed_stale() {
        let v = assess(&ServerFacts {
            serving: Some(true),
            ..facts(rec(4812, 51234), Some(Liveness::Alive { image: None }))
        });
        assert_eq!(
            v,
            Verdict::Running {
                pid: 4812,
                port: 51234
            }
        );
        assert!(!v.is_stale());
    }

    /// A probe that failed is not a process that died. `Unknown` must never reach
    /// a repair — and the seam guarantees that, because `Status::Unknown` is not
    /// actionable.
    #[test]
    fn a_failed_probe_is_undeterminable_not_dead() {
        let v = assess(&facts(
            rec(4812, 51234),
            Some(Liveness::Unknown("tasklist is not on PATH".into())),
        ));
        assert!(matches!(v, Verdict::Undeterminable { .. }));
        assert!(
            !v.is_stale(),
            "swallowing a probe failure into `dead` would have --fix delete a live server's record"
        );
    }

    /// A record we cannot *read* is not a record that is not *there*. Reporting
    /// `NotTracked` for an unreadable file would say "no server is running" about a
    /// workspace we never managed to look at.
    #[test]
    fn an_unreadable_record_is_undeterminable_not_absent() {
        let v = assess(&facts(
            Record::Unreadable("permission denied".into()),
            None,
        ));
        assert!(matches!(v, Verdict::Undeterminable { .. }));
        assert!(!v.is_stale());
    }

    #[test]
    fn a_live_dolt_that_is_not_serving_is_wedged() {
        let v = assess(&facts(rec(4812, 51234), alive("dolt.exe")));
        assert_eq!(
            v,
            Verdict::Wedged {
                pid: 4812,
                port: 51234
            }
        );
        // Wedged is NOT auto-repairable: doctor does not kill processes, and the
        // record is *accurate* — there really is a dolt on that pid.
        assert!(!v.is_stale());
    }

    /// The grace window. bd-dolt writes the record only *after* the server has
    /// answered, so a young record whose port has gone quiet means the server was
    /// serving moments ago — restarting, not wedged. Calling that an `error` on
    /// first sighting is crying wolf.
    #[test]
    fn a_young_record_gets_the_benefit_of_the_doubt() {
        let v = assess(&ServerFacts {
            record_age: Some(Duration::from_secs(2)),
            ..facts(rec(4812, 51234), alive("dolt"))
        });
        assert_eq!(
            v,
            Verdict::Starting {
                pid: 4812,
                port: 51234
            }
        );
        assert!(!v.is_stale());
    }

    #[test]
    fn a_serving_server_is_running() {
        let v = assess(&ServerFacts {
            serving: Some(true),
            ..facts(rec(4812, 51234), alive("dolt"))
        });
        assert_eq!(
            v,
            Verdict::Running {
                pid: 4812,
                port: 51234
            }
        );
    }

    /// There is no such thing as a known pid with an unknown port. They are one
    /// JSON object, written together and read together — which is exactly what the
    /// old `PortUnknown` verdict, and the separate `.port` file it was invented
    /// for, got wrong.
    #[test]
    fn the_pid_and_the_port_are_never_apart() {
        let dir = tmp("together");
        let path = pidfile_path(&dir);
        std::fs::write(&path, r#"{"pid":4812,"port":51234}"#).unwrap();

        match read_record(&path, &dir) {
            Record::Present(r) => {
                assert_eq!(r.pid, 4812);
                assert_eq!(r.port, 51234);
            }
            other => panic!("expected a record, got {other:?}"),
        }

        // Half a record is no record.
        std::fs::write(&path, r#"{"pid":4812}"#).unwrap();
        assert!(matches!(read_record(&path, &dir), Record::Corrupt(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_record_is_stale() {
        let v = assess(&facts(Record::Corrupt("\u{0}\u{0}".into()), None));
        assert!(v.is_stale());
        assert!(matches!(v, Verdict::Corrupt { .. }));
    }

    // --- BD_DOLT_PORT --------------------------------------------------------

    /// When the user points bd at a server with `BD_DOLT_PORT`, `bd-dolt` adopts
    /// whatever answers there and never opens the record. Doctor must route the
    /// same way — otherwise it diagnoses a file bd is about to ignore, which is the
    /// same class of mistake as diagnosing a path that does not exist.
    #[test]
    fn an_env_port_that_is_serving_is_adopted_and_the_record_is_not_consulted() {
        let v = assess(&ServerFacts {
            env_port: Some((3306, true)),
            // A record that would otherwise read as stale. It is irrelevant here.
            ..facts(rec(4812, 51234), Some(Liveness::Dead))
        });
        assert_eq!(v, Verdict::Adopted { port: 3306 });
        assert!(
            !v.is_stale(),
            "an adopted server is not ours to stop, and its record is not ours to delete"
        );
    }

    #[test]
    fn an_env_port_with_nothing_on_it_is_where_bd_will_start_one() {
        let v = assess(&ServerFacts {
            env_port: Some((3306, false)),
            ..facts(Record::Absent, None)
        });
        assert_eq!(v, Verdict::EnvPortIdle { port: 3306 });
        assert!(!v.is_stale());
    }

    #[test]
    fn a_zero_env_port_is_not_an_override() {
        // `0` means "let the OS pick", which is bd-dolt's rule too.
        unsafe { std::env::set_var(PORT_ENV, "0") };
        assert_eq!(env_port(), None);
        unsafe { std::env::set_var(PORT_ENV, "notaport") };
        assert_eq!(env_port(), None);
        unsafe { std::env::set_var(PORT_ENV, " 3307 ") };
        assert_eq!(env_port(), Some(3307));
        unsafe { std::env::remove_var(PORT_ENV) };
        assert_eq!(env_port(), None);
    }

    // --- what --fix says when it will not act -------------------------------

    /// The state that used to have no honest answer. `run()` found a dead server,
    /// `--fix` re-checked and found it alive — and the seam offered only "did it"
    /// or "cannot do it", so a correct, protective refusal had to be reported as a
    /// *failure*. It now declines, out loud, with the reason.
    #[test]
    fn a_repair_that_refuses_says_why_rather_than_claiming_it_cannot() {
        let back = decline(&Verdict::Running {
            pid: 4812,
            port: 51234,
        });
        assert!(back.contains("second server"), "got: {back}");

        let wedged = decline(&Verdict::Wedged {
            pid: 4812,
            port: 51234,
        });
        assert!(
            wedged.contains("does not kill processes"),
            "the refusal must say what it will not do, and why: {wedged}"
        );
        assert!(wedged.contains("4812"), "and name the process: {wedged}");

        let unknown = decline(&Verdict::Undeterminable {
            why: "tasklist would not run".into(),
        });
        assert!(unknown.contains("tasklist would not run"), "got: {unknown}");
    }

    // --- process identity ----------------------------------------------------

    #[test]
    fn only_a_dolt_is_a_dolt() {
        assert!(is_dolt_image("dolt"));
        assert!(is_dolt_image("dolt.exe"));
        assert!(is_dolt_image("DOLT.EXE"));
        assert!(is_dolt_image("/usr/local/bin/dolt"));
        assert!(is_dolt_image(r"C:\tools\dolt.exe"));
        // The near-misses that a substring check would wave through.
        assert!(!is_dolt_image("doltgres"));
        assert!(!is_dolt_image("code.exe"));
        assert!(!is_dolt_image("my-dolt-wrapper"));
    }

    // --- tasklist parsing (the Windows liveness probe) ----------------------

    #[test]
    fn tasklist_reports_the_image_name() {
        let out = "\"dolt.exe\",\"4812\",\"Console\",\"1\",\"84,320 K\"\r\n";
        assert_eq!(
            parse_tasklist(out),
            Liveness::Alive {
                image: Some("dolt.exe".to_string())
            }
        );
    }

    #[test]
    fn tasklist_banner_means_dead() {
        let out = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert_eq!(parse_tasklist(out), Liveness::Dead);
        // And so does saying nothing at all.
        assert_eq!(parse_tasklist(""), Liveness::Dead);
    }

    /// An out-of-range pid makes tasklist complain rather than answer. That is a
    /// failed probe, not a dead process.
    #[test]
    fn tasklist_error_is_unknown_not_dead() {
        let out = "ERROR: Invalid argument/option - 'PID eq 99999999999'.\r\n";
        assert!(matches!(parse_tasklist(out), Liveness::Unknown(_)));
    }

    // --- the serving probe ---------------------------------------------------

    /// The distinction the whole `Running`/`Wedged` split rests on: a server that
    /// **accepts and says nothing** is not serving. Dolt binds its listener before
    /// it can answer, and a wedged one accepts and resets — so a connect-only probe
    /// would report the wedged server as healthy, which is the single thing this
    /// family exists to prevent.
    #[tokio::test]
    async fn serving_means_the_server_spoke_not_merely_that_it_accepted() {
        let greeting = FakeServer::start(true);
        assert!(is_serving(greeting.port).await);

        let silent = FakeServer::start(false);
        assert!(
            !is_serving(silent.port).await,
            "a bound-but-silent listener is exactly the wedged server; calling it Running is the bug"
        );

        // Nothing there at all.
        let dead = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(!is_serving(dead).await);
    }

    /// A loopback listener that is not dolt. Everything about the port probe can be
    /// tested against this, and none of it needs a real dolt — which matters,
    /// because there is none on this machine.
    struct FakeServer {
        port: u16,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeServer {
        /// `greet: true` behaves like a MySQL server (speaks first). `false` binds,
        /// accepts, and stays silent past the prober's read timeout.
        fn start(greet: bool) -> FakeServer {
            use std::io::Write as _;
            use std::sync::atomic::{AtomicBool, Ordering};

            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let flag = stop.clone();

            let thread = std::thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut sock, _)) => {
                            if greet {
                                let _ = sock.write_all(b"\x0a5.7.9-fake-dolt\0");
                                let _ = sock.flush();
                            } else {
                                std::thread::sleep(Duration::from_millis(600));
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });

            FakeServer {
                port,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    // --- storage format -----------------------------------------------------

    #[test]
    fn the_current_storage_format_is_recognised() {
        let m = "5:__DOLT__:qtnpkc6r0b7egk3t1cvdd0s7fh0m8b3v:t9hcvrb9khhgqcltcbd9m5j5j3fgvhqf:0";
        assert_eq!(classify_format(m), Some(Format::Current));
    }

    #[test]
    fn the_legacy_storage_format_is_a_warning_not_a_pass() {
        assert_eq!(
            classify_format("4:__LD_1__:abc:def"),
            Some(Format::Legacy("__LD_1__".to_string()))
        );
    }

    /// Being wrong about dolt's file format must degrade to `unknown`, never to a
    /// silent "healthy". This is the check's whole safety property.
    #[test]
    fn an_unfamiliar_manifest_is_never_reported_as_healthy() {
        assert!(matches!(
            classify_format("7:__FUTURE__:abc"),
            Some(Format::Unrecognised(_))
        ));
        assert_eq!(classify_format(""), None);
        assert_eq!(classify_format("this is not a manifest"), None);
        assert_eq!(classify_format("5:nope:abc:def"), None);
    }

    // The lock sweeper used to live here. It now lives in the Maintenance
    // family (`checks/pollution.rs`), which owns every lock file under
    // `.beads/` — because two checks that both matched `dolt.bootstrap.lock`
    // meant two repairs racing to unlink the same file. The safety property
    // that the sweeper never touches a file bd-dolt or dolt owns moved with it,
    // and is asserted there against `bd-dolt`'s own constants.

    // --- remotes ------------------------------------------------------------

    #[test]
    fn remotes_are_read_out_of_repo_state() {
        let raw = r#"{
          "head": "refs/heads/main",
          "remotes": {
            "origin": {"name":"origin","url":"https://doltremoteapi.dolthub.com/acme/beads","fetch_specs":[],"params":{}},
            "peer":   {"name":"peer","url":"file:///srv/beads","fetch_specs":[],"params":{}}
          }
        }"#;
        let r = remotes_from_repo_state(raw).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, "origin");
        assert_eq!(r[1].0, "peer");
    }

    #[test]
    fn a_repo_state_without_remotes_has_no_remotes() {
        assert_eq!(
            remotes_from_repo_state(r#"{"head":"refs/heads/main"}"#),
            Some(Vec::new())
        );
    }

    /// Unparseable is not empty. An empty list would be reported as "no remotes,
    /// all fine" — coverage over a file we could not read.
    #[test]
    fn an_unparseable_repo_state_is_none_not_empty() {
        assert_eq!(remotes_from_repo_state("{ not json"), None);
        assert_eq!(remotes_from_repo_state(r#"{"remotes": 7}"#), None);
    }

    #[test]
    fn the_same_remote_written_two_ways_compares_equal() {
        let a = canonical_remote("git@github.com:acme/beads.git");
        let b = canonical_remote("https://github.com/acme/beads");
        let c = canonical_remote("https://user:token@github.com/acme/beads.git/");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a, "github.com/acme/beads");
    }

    #[test]
    fn different_remotes_stay_different() {
        assert_ne!(
            canonical_remote("https://github.com/acme/beads"),
            canonical_remote("https://github.com/acme/other")
        );
        // A port is not an scp-style path separator.
        assert_eq!(
            canonical_remote("ssh://git@host:22/acme/beads.git"),
            "host:22/acme/beads"
        );
    }

    // --- the SQLite path, which we *can* test end to end --------------------

    /// The family's central rule, asserted rather than documented: on a SQLite
    /// workspace every one of these checks is `n/a` — silent, and counted apart
    /// from `ok` so that "18 ok" means eighteen things were actually verified.
    #[tokio::test]
    async fn every_check_is_na_and_silent_on_a_sqlite_workspace() {
        use clap::Parser as _;

        let dir = tmp("sqlite-ws");
        let beads = dir.join(".beads");
        std::fs::create_dir_all(&beads).unwrap();
        std::fs::write(
            beads.join("workspace.json"),
            r#"{"backend":"sqlite","workspace_id":"w1"}"#,
        )
        .unwrap();

        // Plant everything that would make a *dolt* workspace scream: a stale
        // server record, an ancient bootstrap lock. None of it is any of our
        // business here.
        std::fs::write(pidfile_path(&beads), r#"{"pid":999999,"port":3306}"#).unwrap();
        let lock = beads.join("dolt.bootstrap.lock");
        std::fs::write(&lock, "").unwrap();
        backdate(&lock, Duration::from_secs(3600));

        let cli = crate::cli::Cli::parse_from(["bd", "-C", dir.to_str().unwrap(), "doctor"]);
        let ctx = crate::context::Ctx::build(&cli, crate::context::Need::Nothing)
            .await
            .unwrap();
        let dx = Dx::new(&ctx);

        for check in checks() {
            let f = check.run(&dx).await;
            assert_eq!(
                f.status,
                Status::NotApplicable,
                "{} must be n/a on a sqlite workspace, said: {} / {:?}",
                check.name(),
                f.message,
                f.detail
            );
            assert_eq!(f.message, NOT_DOLT, "{}", check.name());
            // And it must not offer to fix anything it did not find.
            assert!(f.fix.is_none(), "{}", check.name());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A workspace with no `.beads/` at all — doctor's hardest input. Nothing here
    /// may panic, and nothing may claim a Dolt problem.
    #[tokio::test]
    async fn no_workspace_at_all_is_not_a_dolt_problem() {
        use clap::Parser as _;

        let dir = tmp("no-ws");
        let cli = crate::cli::Cli::parse_from(["bd", "-C", dir.to_str().unwrap(), "doctor"]);
        let ctx = crate::context::Ctx::build(&cli, crate::context::Need::Nothing)
            .await
            .unwrap();
        let dx = Dx::new(&ctx);

        for check in checks() {
            let f = check.run(&dx).await;
            assert!(f.is_ok(), "{}: {}", check.name(), f.message);
            assert_eq!(f.message, NOT_DOLT);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- helpers ------------------------------------------------------------

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "bd-doctor-dolt-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    /// Push a file's mtime into the past, so staleness can be tested without
    /// sleeping for five minutes.
    fn backdate(p: &Path, by: Duration) {
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() - by).unwrap();
    }
}
