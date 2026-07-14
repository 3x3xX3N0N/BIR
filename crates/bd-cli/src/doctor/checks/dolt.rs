//! Dolt Storage — only meaningful when the workspace's backend *is* Dolt.
//!
//! Read the backend from the locator (`dx.ctx.backend()`), never from a flag or
//! the environment. On a SQLite workspace every check here must return
//! [`Finding::ok`] with "not a dolt workspace" — **not** a warning. A user who
//! chose SQLite has no problem, and telling them ten times that they have no
//! Dolt server is noise that trains people to ignore doctor.
//!
//! The one that actually saves people: **a stale `dolt sql-server` still holding
//! the database lock.** The next `bd` invocation then fails with an error that
//! reads exactly like database corruption, and users reach for `rm -rf`.
//!
//! Belongs here: server reachable, schema present, working-set status, stale
//! locks and orphaned server processes, storage format, remote/origin agreement
//! with git, server-vs-embedded mode mismatch, issue count sanity, performance.
//!
//! # What this family may touch, and what it may not
//!
//! `bd-cli` does not depend on `bd-dolt`, and it has no MySQL client. So every
//! check here works from three things and nothing else: the backend on the
//! locator, the filesystem, and a `dolt` binary if the user has one. That rules
//! out the whole server-query half of upstream's Dolt suite (schema tables,
//! `dolt_status`, issue counts, phantom databases). Those are *not* stubbed out
//! as passing — they are simply not here, which is the honest form of "nobody
//! has written that yet".
//!
//! Two lines that are load-bearing, not stylistic:
//!
//! * **Nothing in this file ever writes inside `.dolt/`.** Not the noms `LOCK`,
//!   not the manifest, not a journal. Those are Dolt's, they are advisory or
//!   content-addressed, and removing one turns a recoverable workspace into an
//!   unrecoverable one. Repair here is confined to *bd's own* bookkeeping in
//!   `.beads/`: the pid file, the port file, and bd's lock files.
//! * **A live pid is not proof of a live server.** Windows recycles process ids
//!   aggressively, so "a process with id 4812 exists" is much weaker evidence
//!   than it looks — 4812 may now be a text editor. Staleness is therefore
//!   decided on the *identity* of the process (is it a `dolt`?) and, where a
//!   port is known, on whether anything is actually listening.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use async_trait::async_trait;
use bd_storage::Backend;

use super::super::{Category, Check, Dx, Finding, Repair};

pub fn checks() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(DoltBinary),
        Box::new(DoltDatabase),
        Box::new(StorageFormat),
        Box::new(Server),
        Box::new(LockFiles),
        Box::new(RemoteVsOrigin),
    ]
}

/// The single sentence a non-Dolt workspace ever hears from this family.
///
/// Every check says exactly this and says it as `Ok`, so the human renderer
/// collapses the whole family to one green line (see `print_human`: a category
/// in which everything is `Ok` gets a single row, never a wall of green).
const NOT_DOLT: &str = "not a dolt workspace";

// ---------------------------------------------------------------------------
// Where things are
// ---------------------------------------------------------------------------

/// The Dolt-relevant paths of a workspace that *is* Dolt-backed.
///
/// Constructed only through [`dolt_ws`], which is the one place the backend is
/// read. Holding one of these is proof the backend came from the locator on
/// disk and not from a flag.
struct DoltWs {
    beads: PathBuf,
}

impl DoltWs {
    /// `.beads/dolt` — the Dolt data directory.
    fn dolt(&self) -> PathBuf {
        self.beads.join("dolt")
    }

    /// `.beads/dolt/.dolt` — Dolt's own metadata. **Never written to here.**
    fn dot_dolt(&self) -> PathBuf {
        self.dolt().join(".dolt")
    }

    /// The noms manifest. Its second field is the storage format.
    fn manifest(&self) -> PathBuf {
        self.dot_dolt().join("noms").join("manifest")
    }

    fn repo_state(&self) -> PathBuf {
        self.dot_dolt().join("repo_state.json")
    }

    fn pid_file(&self) -> PathBuf {
        self.beads.join(PID_FILE)
    }

    fn port_file(&self) -> PathBuf {
        self.beads.join(PORT_FILE)
    }
}

const PID_FILE: &str = "dolt-server.pid";
const PORT_FILE: &str = "dolt-server.port";

/// The gate. `None` means "this check has nothing to say here".
///
/// Note the shape: the backend comes from `ctx.backend()`, which reads the
/// locator that `bd init` wrote. A `--backend` flag could not reach this code
/// if it tried.
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
/// touches the store will fail until it is installed. Saying so once, clearly,
/// is worth more than the ten downstream failures it causes.
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

        let Some(path) = which_dolt() else {
            return Finding::error(self.name(), "no `dolt` on PATH")
                .detail(
                    "This workspace's backend is dolt, so every command that opens the store \
                     needs the dolt binary. Nothing is wrong with the workspace itself.",
                )
                .fix("install dolt — https://docs.dolthub.com/introduction/installation");
        };

        // Only now that we know it exists do we pay for a process. `dolt
        // version` touches no database and no lock; it is safe under rule 3.
        match run_cmd("dolt", &["version"], Duration::from_secs(5)).await {
            Some(out) if out.status.success() => {
                let v = first_line(&String::from_utf8_lossy(&out.stdout));
                Finding::ok(self.name(), if v.is_empty() { "found".into() } else { v })
                    .detail(path.display().to_string())
            }
            // On PATH but will not run: a broken install, a wrong architecture,
            // a shim that shells out to something that isn't there. That is not
            // `ok`, and it is not the workspace's fault either.
            _ => Finding::warn(self.name(), "`dolt` is on PATH but would not run")
                .detail(path.display().to_string())
                .fix("check the install: `dolt version`"),
        }
    }
}

/// `which`, without a crate. Splits `PATH`, and on Windows tries each `PATHEXT`.
fn which_dolt() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts = executable_suffixes();
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("dolt{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_suffixes() -> Vec<String> {
    if cfg!(windows) {
        let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
        let mut v: Vec<String> = raw
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect();
        // A bare `dolt` with no extension is still worth trying (a shell shim,
        // or a WSL-ish setup).
        v.push(String::new());
        v
    } else {
        vec![String::new()]
    }
}

// ---------------------------------------------------------------------------
// 2. Is there a dolt database on disk?
// ---------------------------------------------------------------------------

/// A Dolt-backed workspace whose `.beads/dolt/.dolt/` does not exist has not
/// been initialised — every store operation will fail. This is the one check in
/// the family that distinguishes "not set up" from "set up and broken", and the
/// others lean on it: they report *absence* as nothing, so that the missing
/// database is named exactly once.
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
            return Finding::error(self.name(), "the dolt database is missing")
                .detail(format!(
                    "the locator says this workspace is dolt-backed, but {} does not exist",
                    ws.dot_dolt().display()
                ))
                .fix("`bd init` for a new project, or restore .beads/dolt from your backup");
        }

        if !ws.manifest().is_file() {
            // `.dolt/` without a manifest: an interrupted init, or a partial
            // copy. Warn, not error — the data may still be recoverable, and
            // this port cannot tell.
            return Finding::warn(self.name(), "the dolt database looks half-initialised")
                .detail(format!(
                    "{} exists but {} does not",
                    ws.dot_dolt().display(),
                    ws.manifest().display()
                ))
                .fix("`dolt status` in .beads/dolt to see what dolt makes of it");
        }

        Finding::ok(self.name(), "present").detail(ws.dolt().display().to_string())
    }
}

// ---------------------------------------------------------------------------
// 3. Storage format
// ---------------------------------------------------------------------------

/// Dolt's noms manifest names the storage format in its second colon-separated
/// field. `__DOLT__` is the current one; `__LD_1__` is the pre-1.0 format, which
/// current Dolt will refuse to open until it is migrated. Reading it costs one
/// small file read and no lock — which is the whole reason to do it from disk
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
            // Saying it twice would double-count one problem.
            return Finding::ok(self.name(), "no dolt database to inspect");
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
                    .fix("`cd .beads/dolt && dolt migrate` — take a backup of .beads/dolt first")
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
/// recognise yields `None`, which the caller turns into a *warning*, never an
/// `ok` — being wrong about dolt's file format must not silently report a
/// legacy database as healthy.
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

/// A `dolt sql-server` that died without cleaning up leaves `.beads/dolt-server.pid`
/// behind. The next `bd` command reads that file, tries to talk to a server that
/// is not there, and fails with an error that reads *exactly* like database
/// corruption. Users then delete `.beads/` — losing everything — to fix a
/// problem whose real remedy is removing a twelve-byte text file.
///
/// So this check's whole job is to be believed. It states, in the finding
/// itself, that this is not corruption and that `.beads/dolt/` must not be
/// deleted.
struct Server;

/// The startup race: a server whose pid file was written moments ago may simply
/// not be listening yet. Below this age, "not listening" is reported as
/// *starting*, not as wedged.
const STARTUP_GRACE: Duration = Duration::from_secs(30);

/// What the filesystem and the OS say. Gathered impurely, judged purely.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerFacts {
    /// `None`: no pid file. `Some(Err)`: there is one and it is not a number.
    pid: Option<Result<u32, String>>,
    /// Age of the pid file, if we could stat it.
    pid_age: Option<Duration>,
    liveness: Option<Liveness>,
    port: Option<u16>,
    /// `None` when there was no port to probe.
    listening: Option<bool>,
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
    /// "the process is gone" would have doctor cheerfully delete the pid file of
    /// a perfectly healthy server.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    /// No pid file. Normal: bd starts a server on demand.
    NotTracked,
    Running { pid: u32, port: u16 },
    /// Alive, is a dolt, but nothing is listening on its port, and it has had
    /// long enough to get there. This is the wedged server.
    Wedged { pid: u32, port: u16 },
    /// Alive, is a dolt, and the pid file is younger than the startup grace.
    Starting { pid: u32 },
    /// Alive, is a dolt, but we have no port for it.
    PortUnknown { pid: u32 },
    /// The process is gone. **Stale.**
    Dead { pid: u32 },
    /// A process with that id exists but it is not a dolt. On Windows this is
    /// the common case for a stale pid file. **Stale.**
    Reused { pid: u32, image: String },
    /// The pid file exists and is not a number. **Stale.**
    Corrupt { raw: String },
    /// We could not tell. Warn, and do not touch anything.
    Undeterminable { why: String },
}

impl Verdict {
    /// Only these three are safe to clean up automatically: in each, the process
    /// bd recorded is provably not serving this workspace.
    fn is_stale(&self) -> bool {
        matches!(
            self,
            Verdict::Dead { .. } | Verdict::Reused { .. } | Verdict::Corrupt { .. }
        )
    }
}

/// The whole state machine, as one pure function — because this is the logic
/// that decides whether doctor deletes a file, and it must be testable without
/// a dolt binary, a server, or a real process to kill.
fn assess(f: &ServerFacts) -> Verdict {
    let pid = match &f.pid {
        None => return Verdict::NotTracked,
        Some(Err(raw)) => return Verdict::Corrupt { raw: raw.clone() },
        Some(Ok(pid)) => *pid,
    };

    let liveness = match &f.liveness {
        None | Some(Liveness::Unknown(_)) => {
            let why = match &f.liveness {
                Some(Liveness::Unknown(w)) => w.clone(),
                _ => "the process was not probed".to_string(),
            };
            return Verdict::Undeterminable { why };
        }
        Some(l) => l,
    };

    let image = match liveness {
        Liveness::Dead => return Verdict::Dead { pid },
        Liveness::Alive { image } => image,
        Liveness::Unknown(_) => unreachable!("handled above"),
    };

    // A named process that is not a dolt means the id was recycled. A process
    // we could not name is *not* treated as recycled — absence of evidence is
    // not evidence, and the penalty for guessing wrong is deleting the pid file
    // of a live server.
    if let Some(image) = image
        && !is_dolt_image(image)
    {
        return Verdict::Reused {
            pid,
            image: image.clone(),
        };
    }

    let Some(port) = f.port else {
        return Verdict::PortUnknown { pid };
    };

    match f.listening {
        Some(true) => Verdict::Running { pid, port },
        Some(false) => {
            // Give a freshly-written pid file the benefit of the doubt: the
            // server may still be binding its socket.
            if f.pid_age.is_some_and(|a| a < STARTUP_GRACE) {
                Verdict::Starting { pid }
            } else {
                Verdict::Wedged { pid, port }
            }
        }
        None => Verdict::Undeterminable {
            why: format!("port {port} was not probed"),
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

        let facts = gather(&ws).await;
        let n = self.name();

        match assess(&facts) {
            Verdict::NotTracked => Finding::ok(n, "no server is running (bd starts one on demand)"),

            Verdict::Running { pid, port } => {
                Finding::ok(n, format!("running (pid {pid}, port {port})"))
            }

            Verdict::Starting { pid } => Finding::warn(n, "a dolt server appears to be starting")
                .detail(format!(
                    "pid {pid} is alive but nothing is listening yet; {} was written less than {}s ago",
                    ws.pid_file().display(),
                    STARTUP_GRACE.as_secs()
                ))
                .fix("re-run `bd doctor` in a moment"),

            // The one that is genuinely broken: something is holding the
            // database and will not talk to us.
            Verdict::Wedged { pid, port } => Finding::error(
                n,
                format!("a dolt server (pid {pid}) is running but not accepting connections"),
            )
            .detail(format!(
                "nothing is listening on 127.0.0.1:{port}, yet pid {pid} is alive and is a dolt \
                 process — it is most likely holding the database lock while wedged.\n\
                 This is NOT database corruption. Do not delete .beads/dolt/."
            ))
            .fix(format!(
                "stop it and let bd start a fresh one: kill {pid}, then remove {} and {}",
                ws.pid_file().display(),
                ws.port_file().display()
            )),

            Verdict::PortUnknown { pid } => {
                Finding::warn(n, format!("a dolt server (pid {pid}) is running on an unknown port"))
                    .detail(format!(
                        "{} exists but {} does not, so bd cannot reach the server it started",
                        ws.pid_file().display(),
                        ws.port_file().display()
                    ))
                    .fix(format!("stop pid {pid} and remove {}", ws.pid_file().display()))
            }

            // ---- the rm -rf scenario ----
            Verdict::Dead { pid } => Finding::warn(n, "a dead dolt server left its pid file behind")
                .detail(format!(
                    "{} names process {pid}, which no longer exists.\n\
                     This is NOT database corruption, and the next bd command may fail with an \
                     error that reads like it. Do not delete .beads/dolt/.",
                    ws.pid_file().display()
                ))
                .fix(fix_hint(&ws)),

            Verdict::Reused { pid, image } => {
                Finding::warn(n, "the recorded dolt server is gone (its pid was recycled)")
                    .detail(format!(
                        "{} names process {pid}, but that id now belongs to `{image}`, not dolt. \
                         The server bd recorded is gone.\n\
                         This is NOT database corruption. Do not delete .beads/dolt/.",
                        ws.pid_file().display()
                    ))
                    .fix(fix_hint(&ws))
            }

            Verdict::Corrupt { raw } => {
                Finding::warn(n, "the dolt server pid file is not a process id")
                    .detail(format!(
                        "{} contains {raw:?}",
                        ws.pid_file().display()
                    ))
                    .fix(fix_hint(&ws))
            }

            Verdict::Undeterminable { why } => Finding::unknown(n, why),
        }
    }

    /// Removes **bd's own** bookkeeping, and only when the state is re-confirmed
    /// stale at repair time. Two properties matter here:
    ///
    /// * It re-gathers. The finding it is handed was produced before `--fix`
    ///   asked to repair, and a server may have started in between. Acting on a
    ///   stale verdict is how a repair kills a healthy workspace.
    /// * It never goes near `.dolt/`. Dolt's own `LOCK` and manifest are its
    ///   business; deleting them is the unrecoverable mistake this whole check
    ///   is trying to talk the user *out of*.
    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(ws) = dolt_ws(dx) else {
            return Ok(Repair::Unfixable);
        };

        let verdict = assess(&gather(&ws).await);
        if !verdict.is_stale() {
            return Ok(Repair::Unfixable);
        }

        let mut removed = Vec::new();
        for path in [ws.pid_file(), ws.port_file()] {
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push(file_name(&path)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("cannot remove {}: {e}", path.display()),
            }
        }

        if removed.is_empty() {
            return Ok(Repair::Unfixable);
        }
        Ok(Repair::Did(format!(
            "removed the stale dolt server bookkeeping ({}); .beads/dolt/ was not touched",
            removed.join(", ")
        )))
    }
}

fn fix_hint(ws: &DoltWs) -> String {
    format!(
        "`bd doctor --fix` — it removes {} and {}, and never touches .beads/dolt/",
        file_name(&ws.pid_file()),
        file_name(&ws.port_file())
    )
}

/// Everything impure, in one place: read two files, ask the OS about a process,
/// knock on a port. Nothing here decides anything.
async fn gather(ws: &DoltWs) -> ServerFacts {
    let pid_path = ws.pid_file();
    let pid = match std::fs::read_to_string(&pid_path) {
        Ok(raw) => Some(parse_pid(&raw)),
        Err(_) => None,
    };
    let pid_age = file_age(&pid_path);

    let liveness = match &pid {
        Some(Ok(pid)) => Some(probe_process(*pid).await),
        _ => None,
    };

    // Only probe the port if we might use the answer.
    let port = std::fs::read_to_string(ws.port_file())
        .ok()
        .and_then(|raw| parse_port(&raw));
    let listening = match (&liveness, port) {
        (Some(Liveness::Alive { .. }), Some(p)) => Some(is_listening(p).await),
        _ => None,
    };

    ServerFacts {
        pid,
        pid_age,
        liveness,
        port,
        listening,
    }
}

/// `Err` carries the raw text, because "the pid file says `<nul><nul><nul>`" is
/// the evidence, and a finding that omits it cannot be acted on.
fn parse_pid(raw: &str) -> Result<u32, String> {
    let t = raw.trim();
    match t.parse::<u32>() {
        Ok(0) => Err(t.to_string()),
        Ok(pid) => Ok(pid),
        Err(_) => Err(t.chars().take(64).collect()),
    }
}

fn parse_port(raw: &str) -> Option<u16> {
    raw.trim().parse::<u16>().ok().filter(|p| *p > 0)
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

/// Rule 5: never block on a handshake. A refused connection is instant; a
/// *dropped* packet is not, so the timeout is what bounds this.
async fn is_listening(port: u16) -> bool {
    tokio::task::spawn_blocking(move || {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
    })
    .await
    .unwrap_or(false)
}

// --- process probing, per platform -----------------------------------------

#[cfg(windows)]
async fn probe_process(pid: u32) -> Liveness {
    let filter = format!("PID eq {pid}");
    let args = ["/FI", filter.as_str(), "/NH", "/FO", "CSV"];
    match run_cmd("tasklist", &args, Duration::from_secs(5)).await {
        Some(out) => parse_tasklist(&String::from_utf8_lossy(&out.stdout)),
        None => Liveness::Unknown("could not run `tasklist`".to_string()),
    }
}

/// `tasklist /NH /FO CSV` prints one quoted row per match, and a banner when
/// there is none. Note that it exits 0 in *both* cases, so the exit code tells
/// you nothing and the text is the only signal.
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
    match run_cmd("ps", &args, Duration::from_secs(5)).await {
        Some(out) => {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !name.is_empty() {
                Liveness::Alive { image: Some(name) }
            } else {
                // `ps -p` exits nonzero precisely when there is no such process.
                Liveness::Dead
            }
        }
        None => Liveness::Unknown("could not run `ps`".to_string()),
    }
}

// ---------------------------------------------------------------------------
// 5. bd's own lock files
// ---------------------------------------------------------------------------

/// The lock files bd itself writes into `.beads/` while bringing Dolt up. A
/// crashed bootstrap leaves one behind, and the next bootstrap waits on a lock
/// nobody holds.
///
/// Scope, deliberately: only the top level of `.beads/`, only files bd wrote.
/// Dolt's `.dolt/noms/LOCK` is *not* here and never will be — it is an advisory
/// lock the OS releases on process death, so its mere presence proves nothing,
/// and deleting it can destroy the database.
struct LockFiles;

#[derive(Debug, PartialEq, Eq)]
struct StaleLock {
    name: String,
    age: Duration,
    threshold: Duration,
}

/// How long each lock may legitimately be held. A bootstrap that has not
/// finished in five minutes is not slow, it is dead.
fn threshold_for(name: &str) -> Option<Duration> {
    if name == "dolt.bootstrap.lock" {
        return Some(Duration::from_secs(5 * 60));
    }
    if name == "dolt-server.lock" {
        return Some(Duration::from_secs(5 * 60));
    }
    if name.ends_with(".startlock") {
        // Start-up locks are held across one spawn. Thirty seconds is already
        // generous.
        return Some(Duration::from_secs(30));
    }
    None
}

/// Pure enough to test: the fs read is one `read_dir`, and the judgement is age
/// against threshold.
fn stale_locks(beads: &Path) -> Vec<StaleLock> {
    let Ok(entries) = std::fs::read_dir(beads) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(threshold) = threshold_for(&name) else {
            continue;
        };
        if !e.path().is_file() {
            continue;
        }
        let Some(age) = file_age(&e.path()) else {
            continue;
        };
        if age > threshold {
            out.push(StaleLock {
                name,
                age,
                threshold,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[async_trait]
impl Check for LockFiles {
    fn name(&self) -> &'static str {
        "dolt lock files"
    }

    fn category(&self) -> Category {
        Category::Dolt
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(ws) = dolt_ws(dx) else {
            return Finding::na(self.name(), NOT_DOLT);
        };

        let stale = stale_locks(&ws.beads);
        if stale.is_empty() {
            return Finding::ok(self.name(), "no stale lock files");
        }

        let detail = stale
            .iter()
            .map(|s| {
                format!(
                    "{}: held {}s (a live one is never held more than {}s)",
                    s.name,
                    s.age.as_secs(),
                    s.threshold.as_secs()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        Finding::warn(
            self.name(),
            format!("{} stale lock file(s) from a crashed bd", stale.len()),
        )
        .detail(detail)
        .fix("`bd doctor --fix` — it removes only bd's own locks in .beads/, never anything in .beads/dolt/")
    }

    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(ws) = dolt_ws(dx) else {
            return Ok(Repair::Unfixable);
        };

        // Re-check: a lock that became legitimate between run and repair (a
        // bootstrap that just started) must not be yanked out from under it.
        let stale = stale_locks(&ws.beads);
        if stale.is_empty() {
            return Ok(Repair::Unfixable);
        }

        let mut removed = Vec::new();
        for s in &stale {
            let path = ws.beads.join(&s.name);
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push(s.name.clone()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("cannot remove {}: {e}", path.display()),
            }
        }
        if removed.is_empty() {
            return Ok(Repair::Unfixable);
        }
        Ok(Repair::Did(format!(
            "removed {} stale lock file(s): {}",
            removed.len(),
            removed.join(", ")
        )))
    }
}

// ---------------------------------------------------------------------------
// 6. Dolt remotes vs the git origin
// ---------------------------------------------------------------------------

/// Beads syncs its issues through Dolt, and its code through git. Pointing a
/// Dolt remote at the git origin aims both at the same endpoint, and the two
/// then fight over the same refs.
///
/// Read from `.dolt/repo_state.json` rather than by querying `dolt_remotes` on
/// a server: it is a plain file, it needs no server, and it takes no lock.
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

        let state = ws.repo_state();
        let remotes = match std::fs::read_to_string(&state) {
            // No repo_state.json means no dolt database (or a very old one).
            // `dolt database` owns that finding; here it is simply nothing.
            Err(_) => return Finding::ok(self.name(), "no dolt remotes configured"),
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

        // Beads does not require git. No repo, no origin, nothing to disagree.
        let Some(root) = &dx.root else {
            return Finding::ok(self.name(), "not a git repository");
        };
        let Some(origin) = git_origin(root).await else {
            return Finding::ok(self.name(), "no git origin configured");
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
            "point the dolt remote somewhere else: `cd .beads/dolt && dolt remote remove {}`",
            clashing[0]
        ))
    }
}

/// `.dolt/repo_state.json` → `[(name, url)]`.
///
/// `None` means the file is not JSON we understand — which becomes a *warning*,
/// not an `ok`. Returning "no remotes" for a file we failed to read would be a
/// check that reports as coverage while checking nothing.
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
/// scheme, credentials, `.git`, trailing slash, and scp-style `git@host:path`
/// all have to go, or `git@github.com:acme/x.git` and
/// `https://github.com/acme/x` read as two different places when they are one.
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
    let out = run_cmd("git", &args, Duration::from_secs(5)).await?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// Rule 5, enforced in one place: a check may shell out, but it may not hang.
/// A timed-out or unspawnable command yields `None`, which callers must turn
/// into a *warning*, never an `ok`.
async fn run_cmd(program: &str, args: &[&str], limit: Duration) -> Option<std::process::Output> {
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

    // --- the state machine that decides whether we delete a file ------------

    fn facts(pid: Option<Result<u32, String>>, liveness: Option<Liveness>) -> ServerFacts {
        ServerFacts {
            pid,
            pid_age: Some(Duration::from_secs(3600)),
            liveness,
            port: Some(3306),
            listening: Some(false),
        }
    }

    fn alive(image: &str) -> Option<Liveness> {
        Some(Liveness::Alive {
            image: Some(image.to_string()),
        })
    }

    #[test]
    fn no_pid_file_is_not_a_problem() {
        // Absence is not failure: bd starts a server on demand.
        let v = assess(&ServerFacts {
            pid: None,
            pid_age: None,
            liveness: None,
            port: None,
            listening: None,
        });
        assert_eq!(v, Verdict::NotTracked);
        assert!(!v.is_stale());
    }

    #[test]
    fn a_dead_process_is_stale_and_repairable() {
        let v = assess(&facts(Some(Ok(4812)), Some(Liveness::Dead)));
        assert_eq!(v, Verdict::Dead { pid: 4812 });
        assert!(v.is_stale(), "this is the case that saves people from rm -rf");
    }

    /// The Windows trap. A pid file naming 4812 plus "a process with id 4812
    /// exists" is *not* evidence the server is alive — Windows recycles ids
    /// within seconds. Identity is what settles it.
    #[test]
    fn a_recycled_pid_is_stale_not_running() {
        let v = assess(&facts(Some(Ok(4812)), alive("code.exe")));
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
    /// name the process, we must NOT conclude the pid was recycled. Guessing
    /// wrong here deletes the pid file of a healthy, running server.
    #[test]
    fn an_unnamed_live_process_is_never_assumed_stale() {
        let v = assess(&ServerFacts {
            listening: Some(true),
            ..facts(Some(Ok(4812)), Some(Liveness::Alive { image: None }))
        });
        assert_eq!(v, Verdict::Running { pid: 4812, port: 3306 });
        assert!(!v.is_stale());
    }

    /// A probe that failed is not a process that died. `Unknown` must never
    /// reach a repair.
    #[test]
    fn a_failed_probe_is_undeterminable_not_dead() {
        let v = assess(&facts(
            Some(Ok(4812)),
            Some(Liveness::Unknown("tasklist is not on PATH".into())),
        ));
        assert!(matches!(v, Verdict::Undeterminable { .. }));
        assert!(
            !v.is_stale(),
            "swallowing a probe failure into `dead` would have --fix delete a live server's pid file"
        );
    }

    #[test]
    fn a_live_dolt_that_nobody_can_reach_is_wedged() {
        let v = assess(&facts(Some(Ok(4812)), alive("dolt.exe")));
        assert_eq!(v, Verdict::Wedged { pid: 4812, port: 3306 });
        // Wedged is NOT auto-repairable: doctor does not kill processes.
        assert!(!v.is_stale());
    }

    /// The startup race: a pid file written two seconds ago whose server has not
    /// bound its socket yet is starting, not wedged. Without this, `bd doctor`
    /// run immediately after `bd init` reports a false error.
    #[test]
    fn a_young_pid_file_gets_the_benefit_of_the_doubt() {
        let v = assess(&ServerFacts {
            pid_age: Some(Duration::from_secs(2)),
            ..facts(Some(Ok(4812)), alive("dolt"))
        });
        assert_eq!(v, Verdict::Starting { pid: 4812 });
    }

    #[test]
    fn a_reachable_server_is_running() {
        let v = assess(&ServerFacts {
            listening: Some(true),
            ..facts(Some(Ok(4812)), alive("dolt"))
        });
        assert_eq!(v, Verdict::Running { pid: 4812, port: 3306 });
    }

    #[test]
    fn a_live_dolt_with_no_port_file_is_a_warning() {
        let v = assess(&ServerFacts {
            port: None,
            listening: None,
            ..facts(Some(Ok(4812)), alive("dolt"))
        });
        assert_eq!(v, Verdict::PortUnknown { pid: 4812 });
    }

    #[test]
    fn a_corrupt_pid_file_is_stale() {
        let v = assess(&facts(Some(Err("\u{0}\u{0}".into())), None));
        assert!(v.is_stale());
        assert!(matches!(v, Verdict::Corrupt { .. }));
    }

    // --- pid/port parsing ---------------------------------------------------

    #[test]
    fn pid_parsing_keeps_the_evidence() {
        assert_eq!(parse_pid("4812\n"), Ok(4812));
        assert_eq!(parse_pid("  4812  "), Ok(4812));
        // A zero pid is not a pid.
        assert_eq!(parse_pid("0"), Err("0".to_string()));
        assert_eq!(parse_pid(""), Err(String::new()));
        // The raw text survives into the finding, so the user can see *why*.
        assert_eq!(parse_pid("not a pid"), Err("not a pid".to_string()));
        // A truncated write is the realistic corruption, and it must not panic.
        assert!(parse_pid("48\u{0}\u{0}").is_err());
    }

    #[test]
    fn port_parsing() {
        assert_eq!(parse_port("3306\n"), Some(3306));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("70000"), None);
        assert_eq!(parse_port("garbage"), None);
    }

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

    /// Being wrong about dolt's file format must degrade to a warning, never to
    /// a silent "healthy". This is the check's whole safety property.
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

    // --- lock file staleness ------------------------------------------------

    #[test]
    fn lock_thresholds_cover_bds_files_and_nothing_of_dolts() {
        assert!(threshold_for("dolt.bootstrap.lock").is_some());
        assert!(threshold_for("dolt-server.lock").is_some());
        assert!(threshold_for("bd.sock.startlock").is_some());
        // Not ours: the sync family's, and dolt's own.
        assert!(threshold_for(".sync.lock").is_none());
        assert!(threshold_for("LOCK").is_none());
        assert!(threshold_for("manifest").is_none());
        assert!(threshold_for("beads.db").is_none());
    }

    #[test]
    fn a_fresh_lock_is_not_stale_and_an_old_one_is() {
        let dir = tmp("locks");
        let lock = dir.join("dolt.bootstrap.lock");
        std::fs::write(&lock, "1234").unwrap();

        // Just written: a bootstrap may legitimately be in flight.
        assert!(stale_locks(&dir).is_empty());

        // Backdate it past the threshold.
        backdate(&lock, Duration::from_secs(10 * 60));
        let stale = stale_locks(&dir);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].name, "dolt.bootstrap.lock");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `read_dir` that fails (no `.beads/` at all) is not "there are stale
    /// locks" and is not a panic either — doctor's input is broken workspaces.
    #[test]
    fn a_missing_directory_yields_no_locks_rather_than_an_error() {
        assert!(stale_locks(Path::new("/definitely/not/here/.beads")).is_empty());
    }

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
    /// workspace every one of these checks is `Ok`. Ten warnings about a Dolt
    /// server the user never asked for is how you teach people to stop reading
    /// `bd doctor`.
    #[tokio::test]
    async fn every_check_is_ok_and_silent_on_a_sqlite_workspace() {
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
        // pid file, an ancient bootstrap lock. None of it is any of our business
        // here.
        std::fs::write(beads.join(PID_FILE), "999999").unwrap();
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
            assert!(
                f.is_ok(),
                "{} must be ok on a sqlite workspace, said: {} / {:?}",
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

    /// A workspace with no `.beads/` at all — doctor's hardest input. Nothing
    /// here may panic, and nothing may claim a Dolt problem.
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
