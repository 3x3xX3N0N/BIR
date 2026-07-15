//! Maintenance — debris. Things that accumulate and are never cleaned up.
//!
//! Almost everything here is a **warning, not an error**. Debris is untidy; it
//! is not broken. The exception is anything actively holding a lock or actively
//! lying to another command (a corrupt manifest, a lock file whose owning
//! process is gone).
//!
//! Careful with the word "stale": a lock file whose PID is still alive is not
//! stale, it is *in use*, and deleting it corrupts a running session. Check that
//! the process is actually gone. On Windows a PID is reused aggressively, so
//! "a process with that id exists" is weaker evidence than it looks.
//!
//! Belongs here: stale lock files, leftover mail-queue files, stale molecules,
//! legacy hooks from older versions, test/patrol pollution left in a real
//! workspace, corrupt or contradictory manifests, vestigial sync worktrees.
//!
//! # How this family decides what may be deleted
//!
//! `--fix` deletes things. Getting that wrong destroys a live session or a
//! user's data, so the rules are stated once, here, and every [`Check::repair`]
//! below obeys them:
//!
//! 1. **Never delete something in use.** For a lock file that means: delete only
//!    when the recorded owner is *verifiably* not running. See [`Life`] — a
//!    liveness probe has three answers, and only one of them is "gone".
//! 2. **Never delete issue data.** [`TestPollution`] finds beads that look like
//!    test litter. It reports them and stops. `--fix` is not a licence to delete
//!    somebody's issues on a heuristic, however confident the heuristic feels.
//! 3. **Re-verify at repair time.** `run()` and `repair()` are separated by the
//!    whole rest of the run. A lock that was orphaned when we looked may be held
//!    by the time we act, so `repair()` re-derives its own target set from the
//!    filesystem and never trusts the finding it was handed.
//! 4. **Never descend.** Every scan here is a single shallow `read_dir`. That is
//!    partly speed (rule 5: doctor runs from a git hook) and partly safety —
//!    `.beads/dolt/` holds Dolt's own `LOCK` and manifest files, and deleting
//!    one of those is unrecoverable data loss. A scan that cannot see them
//!    cannot delete them.
//!
//! # What is deliberately not here
//!
//! * **Stale molecules.** Upstream's `CheckStaleMolecules` asks the store for
//!   epics whose children are all closed. `mol` and `swarm` are stubs in this
//!   port, nothing mints a molecule, and the store has no
//!   `epics_eligible_for_closure`. A check for debris that cannot exist would
//!   report `ok` on every run, forever — coverage theatre. When `mol` lands,
//!   this is where its debris check belongs.
//! * **The recursive artifact scan** (`CheckClassicArtifacts`). It walks the
//!   whole repository looking for stray `.beads/` directories. That is a
//!   filesystem walk in a command that has to be cheap enough to run from a
//!   pre-commit hook (rule 5), and most of what it finds is orchestrator-shaped
//!   debris this port has never created.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use async_trait::async_trait;
use bd_core::{Issue, IssueFilter};
use bd_storage::Locator;
use serde_json::Value;

use super::super::{Category, Check, Dx, Finding, Repair};

pub fn checks() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(LockFiles),
        Box::new(WorkspaceManifest),
        Box::new(LegacyQueueFiles),
        Box::new(InterruptedWrites),
        Box::new(LegacyGitHooks),
        Box::new(TestPollution),
    ]
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The `.beads` directory, found **without** going through the locator.
///
/// [`Dx::dir`] is `ctx.locator.dir`, and there is only a locator if
/// `workspace.json` parsed. Every check in this file has to work on a workspace
/// whose manifest is corrupt — that is the case they exist for — so they
/// rediscover the directory themselves and use `dx.dir` only when it is there
/// (it also honours `--db`, which discovery cannot see).
fn beads_dir(dx: &Dx<'_>) -> Option<PathBuf> {
    dx.dir.clone().or_else(|| Locator::discover(&dx.ctx.cwd))
}

/// One shallow `read_dir`, sorted, with unreadable entries skipped rather than
/// aborting the scan. Never recurses — see rule 4 in the module docs.
fn shallow(dir: &Path) -> std::io::Result<Vec<(String, PathBuf, std::fs::Metadata)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Ok(md) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        out.push((name, entry.path(), md));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// How long ago a file was last written. `None` when the filesystem will not say,
/// **or when the mtime is in the future** — a clock that disagrees with the file
/// is not evidence that anything is stale, and every caller here treats `None` as
/// "too new to touch".
fn age_of(md: &std::fs::Metadata) -> Option<Duration> {
    let t = md.modified().ok()?;
    SystemTime::now().duration_since(t).ok()
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// `None` is "we have no idea how old it is", which must never read as "old".
fn older_than(md: &std::fs::Metadata, limit: Duration) -> bool {
    age_of(md).is_some_and(|a| a > limit)
}

/// The first `n`, then "… and k more" — a detail block should be evidence, not a
/// wall.
fn sample(lines: &[String], n: usize) -> String {
    let mut out: Vec<String> = lines.iter().take(n).cloned().collect();
    if lines.len() > n {
        out.push(format!("… and {} more", lines.len() - n));
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------
// Is that process actually gone?
// ---------------------------------------------------------------------------

/// The answer to "is pid N running", which has **three** values.
///
/// The two-valued version of this question is the bug. "I could not tell" is not
/// "it is dead", and a repair that conflates them deletes a live session's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Life {
    Alive,
    /// Verifiably not running *on this host*. The only answer that authorizes a
    /// delete.
    Dead,
    /// The probe did not work — no `tasklist`, no `ps`, a permissions error.
    Unknown,
}

/// Probe the OS process table.
///
/// # The Windows caveat, stated plainly
///
/// Windows reuses PIDs aggressively, so `Alive` does **not** prove the process
/// that wrote the lock is the process we just found — a fresh, unrelated program
/// may have inherited the dead one's id.
///
/// That error is in the safe direction, and it is safe *by construction*: PID
/// reuse can only ever turn a `Dead` into an `Alive`, because reuse hands the id
/// to a process that exists. It can never turn an `Alive` into a `Dead`. So the
/// only mistake reuse can cause here is that we decline to clean up a lock that
/// was in fact abandoned — the user sees "held by pid 1234" and has to look. The
/// mistake it *cannot* cause is deleting the lock of a running `bd`.
///
/// The residual risk is the other one: a lock written on a **different machine**
/// against a shared filesystem. Its pid is meaningless here, and the beads lock
/// format records no hostname, so we would be checking a foreign pid against the
/// local process table. See [`LockFiles`] for how that is handled (it is not
/// handled by pretending; the lock is reported, not deleted).
fn liveness(pid: u32) -> Life {
    if pid == 0 {
        return Life::Unknown;
    }
    probe(pid)
}

/// Linux: `/proc/<pid>` is authoritative and costs no process.
#[cfg(target_os = "linux")]
fn probe(pid: u32) -> Life {
    if Path::new(&format!("/proc/{pid}")).exists() {
        Life::Alive
    } else {
        Life::Dead
    }
}

/// Windows: `tasklist`, in CSV so the answer is unambiguous.
///
/// A match prints one CSV row whose second field is the quoted pid; a miss prints
/// a localized `INFO:` line that begins with no quote. Both exit 0, so the exit
/// code says nothing and the *shape* of the output says everything. Anything else
/// — tasklist missing, a nonzero exit, an unparseable answer — is [`Life::Unknown`],
/// never [`Life::Dead`].
///
/// This costs a process spawn, which is why it only ever runs when a lock file
/// with a pid in it actually exists. The overwhelmingly common case (no locks at
/// all) spawns nothing.
#[cfg(windows)]
fn probe(pid: u32) -> Life {
    let out = std::process::Command::new("tasklist")
        .args(["/NH", "/FO", "CSV", "/FI", &format!("PID eq {pid}")])
        .output();
    let Ok(out) = out else { return Life::Unknown };
    if !out.status.success() {
        return Life::Unknown;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains(&format!("\"{pid}\"")) {
        return Life::Alive;
    }
    // No CSV row at all: tasklist looked and found nothing. Note this does not
    // read the `INFO: No tasks…` text, which is translated on a localized
    // Windows and would make the check silently useless there.
    if !text.lines().any(|l| l.trim_start().starts_with('"')) {
        return Life::Dead;
    }
    Life::Unknown
}

/// Other unix: `ps -p`, which is POSIX and present where `/proc` is not.
#[cfg(all(unix, not(target_os = "linux")))]
fn probe(pid: u32) -> Life {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output();
    let Ok(out) = out else { return Life::Unknown };
    let text = String::from_utf8_lossy(&out.stdout);
    if text.split_whitespace().any(|t| t == pid.to_string()) {
        Life::Alive
    } else if out.status.code() == Some(1) {
        // `ps -p` documents exit 1 for "no such process".
        Life::Dead
    } else {
        Life::Unknown
    }
}

#[cfg(not(any(windows, unix)))]
fn probe(_pid: u32) -> Life {
    Life::Unknown
}

// ---------------------------------------------------------------------------
// Lock files
// ---------------------------------------------------------------------------

const LOCKS: &str = "lock file debris";

/// A lock file's real state. Note that four of the six forbid deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lock {
    /// **Zero bytes — and that is not debris.** Beads truncates a lock file when
    /// it releases the lock and deliberately keeps the path: deleting the file
    /// after unlocking splits lock identity, because a waiter can hold a flock on
    /// the old inode while a new process creates a fresh file at the same path.
    /// An empty lock file is also what an *acquiring* process has for the few
    /// microseconds between `open` and writing its pid. Either way: leave it.
    Released,
    /// The owner is running. In use, not stale.
    Held(u32),
    /// The owner is verifiably gone. This is the one that lies to every
    /// subsequent `bd`, and the only one `--fix` removes.
    Orphaned(u32),
    /// There is a pid, but we could not find out whether it is running.
    Undetermined(u32),
    /// Non-empty, but no `pid=` line — some other tool's lock format, or a
    /// truncated write. We do not know who owns it, so we do not touch it.
    Anonymous,
    Unreadable(String),
}

fn is_lock_name(name: &str) -> bool {
    name.ends_with(".lock") || name.ends_with(".startlock")
}

/// A `key=value` line's value, trimmed. Beads writes `pid=<n>\nstarted=<rfc3339>\n`
/// and, when it can, `host=<name>`.
fn lock_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    body.lines()
        .filter_map(|l| l.trim().split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim())
}

fn lock_pid(body: &str) -> Option<u32> {
    lock_field(body, "pid")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|p| *p > 0)
}

/// This machine's name, for comparing against a lock's `host=`. `None` if it
/// cannot be determined — in which case any host-bearing lock is treated as
/// foreign, which is the safe direction (decline to delete).
fn local_host() -> Option<String> {
    #[cfg(windows)]
    let raw = std::env::var("COMPUTERNAME").ok();
    #[cfg(not(windows))]
    let raw = std::env::var("HOSTNAME").ok().or_else(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .ok()
    });
    raw.map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
}

/// Pure, so the interesting cases are unit-testable without spawning processes.
///
/// `local` is this machine's name (see [`local_host`]). It exists for the one
/// case a pid probe cannot handle: a lock written on **another machine** against
/// a shared or network filesystem. Its pid names a process in a process table we
/// cannot see, so probing it locally is meaningless — a foreign pid that happens
/// not to exist here would read as `Orphaned` and `--fix` would delete a lock
/// whose owner is alive elsewhere. A lock whose `host=` is not ours is therefore
/// [`Lock::Undetermined`] no matter what the local probe says: reported, never
/// deleted. A lock with no `host=` is the old format and stays pid-only.
fn classify(body: &str, local: Option<&str>, life: &dyn Fn(u32) -> Life) -> Lock {
    if body.trim().is_empty() {
        return Lock::Released;
    }
    let pid = match lock_pid(body) {
        None => return Lock::Anonymous,
        Some(pid) => pid,
    };
    // A recorded host that is not ours: the process is not in our table.
    if let Some(host) = lock_field(body, "host").map(|h| h.to_ascii_lowercase())
        && local.map(|l| l != host).unwrap_or(true)
    {
        return Lock::Undetermined(pid);
    }
    match life(pid) {
        Life::Alive => Lock::Held(pid),
        Life::Dead => Lock::Orphaned(pid),
        Life::Unknown => Lock::Undetermined(pid),
    }
}

struct Found {
    name: String,
    path: PathBuf,
    state: Lock,
    age: Option<Duration>,
}

fn scan_locks(beads: &Path, life: &dyn Fn(u32) -> Life) -> std::io::Result<Vec<Found>> {
    let mut out = Vec::new();
    let local = local_host();
    for (name, path, md) in shallow(beads)? {
        if !md.is_file() || !is_lock_name(&name) {
            continue;
        }
        let state = match std::fs::read_to_string(&path) {
            Ok(body) => classify(&body, local.as_deref(), life),
            Err(e) => Lock::Unreadable(e.to_string()),
        };
        out.push(Found {
            name,
            path,
            state,
            age: age_of(&md),
        });
    }
    Ok(out)
}

/// A lock whose owner has been running for longer than this is not *stale* — it
/// is suspicious. Say so, and still do not touch it.
const HELD_TOO_LONG: Duration = Duration::from_secs(24 * 60 * 60);

/// Lock files left behind by processes that are gone.
///
/// The only `Error` this family raises on a *file*, and it earns it: an orphaned
/// lock is not untidy, it is a false statement that the next `bd` will believe.
struct LockFiles;

#[async_trait]
impl Check for LockFiles {
    fn name(&self) -> &'static str {
        LOCKS
    }
    fn category(&self) -> Category {
        Category::Maintenance
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(beads) = beads_dir(dx) else {
            return Finding::ok(LOCKS, "no workspace, so no locks to leak");
        };
        let found = match scan_locks(&beads, &liveness) {
            Ok(f) => f,
            Err(e) => {
                return Finding::unknown(LOCKS, format!("cannot read {}: {e}", beads.display()));
            }
        };

        let mut orphaned = Vec::new();
        let mut murky = Vec::new();
        let mut held = Vec::new();
        for f in &found {
            let a = f.age.map(human).unwrap_or_else(|| "unknown age".into());
            match &f.state {
                Lock::Orphaned(pid) => {
                    orphaned.push(format!("{}: pid {pid} is not running (age {a})", f.name));
                }
                Lock::Undetermined(pid) => murky.push(format!(
                    "{}: pid {pid} — could not determine whether it is running (age {a})",
                    f.name
                )),
                Lock::Anonymous => murky.push(format!(
                    "{}: no pid recorded, so the owner cannot be identified (age {a})",
                    f.name
                )),
                Lock::Unreadable(why) => {
                    murky.push(format!("{}: cannot read it ({why})", f.name));
                }
                Lock::Held(pid) => held.push((f, *pid, a)),
                Lock::Released => {}
            }
        }

        if !orphaned.is_empty() {
            let mut detail = sample(&orphaned, 5);
            for (f, pid, a) in &held {
                detail.push_str(&format!("\n{}: held by live pid {pid} (age {a}) — left alone", f.name));
            }
            return Finding::error(
                LOCKS,
                format!(
                    "{} lock file(s) are held by a process that no longer exists",
                    orphaned.len()
                ),
            )
            .detail(detail)
            .fix("`bd doctor --fix` removes exactly these; it re-checks the owner immediately before deleting");
        }

        if !murky.is_empty() {
            return Finding::warn(
                LOCKS,
                format!("{} lock file(s) whose owner could not be identified", murky.len()),
            )
            .detail(sample(&murky, 5))
            .fix(
                "check by hand whether a bd or dolt process is still running, then delete the file \
                 yourself — bd will not delete a lock it cannot prove is abandoned",
            );
        }

        // A live owner is not a problem. A live owner that has held the lock for a
        // day probably is — but it is *its* problem, and killing it is not ours.
        let wedged: Vec<String> = held
            .iter()
            .filter(|(f, _, _)| f.age.is_some_and(|a| a > HELD_TOO_LONG))
            .map(|(f, pid, a)| format!("{}: held by live pid {pid} for {a}", f.name))
            .collect();
        if !wedged.is_empty() {
            return Finding::warn(
                LOCKS,
                format!("{} lock file(s) have been held by a live process for over a day", wedged.len()),
            )
            .detail(sample(&wedged, 5))
            .fix(
                "the owning process is still running, so this is not stale and bd will not remove it. \
                 If that process is wedged, stop it — the lock goes with it.",
            );
        }

        match held.len() {
            0 => Finding::ok(LOCKS, "no stale lock files"),
            n => Finding::ok(
                LOCKS,
                format!("{n} lock file(s), all held by running processes"),
            ),
        }
    }

    /// Deletes only [`Lock::Orphaned`], and re-derives that set from disk rather
    /// than trusting the finding: between `run()` and here, a new `bd` may have
    /// taken the very lock we were about to call abandoned.
    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(beads) = beads_dir(dx) else {
            return Ok(Repair::Unfixable);
        };
        let found = scan_locks(&beads, &liveness)?;

        let mut removed = Vec::new();
        for f in found {
            let Lock::Orphaned(pid) = f.state else { continue };
            match std::fs::remove_file(&f.path) {
                Ok(()) => removed.push(format!("{} (pid {pid})", f.name)),
                // A lock that vanished under us is a lock we do not have to
                // remove. Anything else is a real failure and must be reported as
                // one — a repair that silently fails is worse than no repair.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("cannot remove {}: {e}", f.path.display()),
            }
        }

        if removed.is_empty() {
            // Either the world changed, or what remains is a lock we are not
            // willing to guess about. Both are "no automatic repair", not "fixed".
            return Ok(Repair::Unfixable);
        }
        Ok(Repair::Did(format!(
            "removed {} orphaned lock file(s): {}",
            removed.len(),
            removed.join(", ")
        )))
    }
}

// ---------------------------------------------------------------------------
// The workspace manifest
// ---------------------------------------------------------------------------

const MANIFEST: &str = "workspace manifest";

/// What `.beads/workspace.json` says, versus what is actually on the disk next to
/// it.
///
/// This is the second `Error` in the family, and it is the *lying to another
/// command* one: every `bd` invocation reads this file to decide which engine
/// owns the data, so a manifest that names the wrong engine sends every command
/// to the wrong store.
///
/// **Reachability note.** Today `Ctx::build` hard-fails when `workspace.json` is
/// missing or unparseable, so `bd doctor` never starts and the `Missing`/`Corrupt`
/// arms below cannot fire through the CLI. That is a flaw in the seam, not in
/// this check (doctor's whole premise is that it runs on workspaces too broken to
/// open). The `Contradiction` arm — a manifest that parses and lies — is live
/// today, and the rest go live the moment the seam is fixed. The classification is
/// unit-tested either way.
struct WorkspaceManifest;

#[derive(Debug, PartialEq, Eq)]
enum ManifestState {
    Missing,
    Unreadable(String),
    Corrupt(String),
    /// Parses, but says something impossible.
    Wrong(String),
    /// Parses, and nothing on disk contradicts it.
    Coherent(String),
}

/// Which engines have left evidence of themselves in `.beads/`.
fn on_disk_backends(beads: &Path) -> Vec<&'static str> {
    let mut v = Vec::new();
    if beads.join("beads.db").is_file() {
        v.push("sqlite");
    }
    // Dolt keeps its database as a directory. Note we only *look*; nothing in
    // this file ever touches anything inside it.
    if beads.join("dolt").is_dir() || beads.join(".dolt").is_dir() {
        v.push("dolt");
    }
    v
}

fn read_manifest(beads: &Path) -> ManifestState {
    let path = beads.join(bd_storage::locator::LOCATOR_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ManifestState::Missing,
        Err(e) => return ManifestState::Unreadable(e.to_string()),
    };
    classify_manifest(&raw, &on_disk_backends(beads))
}

/// Pure: the whole point is that this is testable without a workspace.
fn classify_manifest(raw: &str, on_disk: &[&str]) -> ManifestState {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            let head: String = raw.chars().take(120).collect();
            return ManifestState::Corrupt(format!("not valid JSON: {e}\nit begins: {head:?}"));
        }
    };
    let Some(obj) = v.as_object() else {
        return ManifestState::Corrupt("the manifest is not a JSON object".into());
    };

    let backend = match obj.get("backend").and_then(|b| b.as_str()) {
        Some(b) => b,
        None => {
            return ManifestState::Corrupt(
                "no `backend` field: the manifest does not say which engine owns the data".into(),
            );
        }
    };
    if backend.parse::<bd_storage::Backend>().is_err() {
        return ManifestState::Corrupt(format!(
            "`backend` is {backend:?}, which is not an engine beads knows"
        ));
    }

    let id = obj.get("workspace_id").and_then(|i| i.as_str()).unwrap_or("");
    if id.is_empty() {
        return ManifestState::Wrong(
            "`workspace_id` is missing or empty — this workspace has no stable identity, so \
             anything that syncs or federates it cannot tell it apart from another"
                .into(),
        );
    }

    // The contradiction that matters: the manifest names an engine, and the only
    // database sitting next to it belongs to a *different* engine. Every command
    // will now open the wrong store, or fail trying.
    //
    // "No database at all" is deliberately not a contradiction here. That is the
    // Core family's finding (the store will not open); the manifest itself is
    // still telling the truth about what it is.
    if !on_disk.is_empty() && !on_disk.contains(&backend) && matches!(backend, "sqlite" | "dolt") {
        return ManifestState::Wrong(format!(
            "the manifest says the backend is {backend}, but the only database in .beads/ is {}",
            on_disk.join(" and ")
        ));
    }

    ManifestState::Coherent(backend.to_string())
}

#[async_trait]
impl Check for WorkspaceManifest {
    fn name(&self) -> &'static str {
        MANIFEST
    }
    fn category(&self) -> Category {
        Category::Maintenance
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(beads) = beads_dir(dx) else {
            return Finding::ok(MANIFEST, "no workspace here");
        };
        match read_manifest(&beads) {
            ManifestState::Coherent(b) => {
                Finding::ok(MANIFEST, format!("coherent ({b}, and the data on disk agrees)"))
            }
            ManifestState::Unreadable(why) => Finding::unknown(
                MANIFEST,
                format!("cannot read {}: {why}", beads.join("workspace.json").display()),
            ),
            ManifestState::Missing => Finding::warn(MANIFEST, "the workspace manifest is missing")
                .detail(format!(
                    "{} exists but has no workspace.json, so no bd command can tell what kind of \
                     workspace this is",
                    beads.display()
                ))
                .fix(
                    "restore .beads/workspace.json from git if it is tracked. `bd init --force` \
                     will write a new one, but it mints a NEW workspace id — anything that synced \
                     this workspace will see it as a different one.",
                ),
            ManifestState::Corrupt(why) => Finding::error(MANIFEST, "the workspace manifest is corrupt")
                .detail(why)
                .fix(
                    "restore .beads/workspace.json from git. bd will not rewrite it: the file \
                     carries the workspace id, and inventing a fresh one forks the workspace from \
                     itself.",
                ),
            ManifestState::Wrong(why) => {
                Finding::error(MANIFEST, "the workspace manifest contradicts what is on disk")
                    .detail(why)
                    .fix(
                        "fix .beads/workspace.json by hand — bd cannot know which of the two is \
                         the one you meant, and guessing wrong points every command at the wrong \
                         database",
                    )
            }
        }
    }

    // No repair, on purpose. Every possible fix here — minting a workspace id,
    // rewriting the backend — is a guess about which of two contradictory
    // statements is true, and the wrong guess silently abandons a database.
}

// ---------------------------------------------------------------------------
// Legacy merge-queue files
// ---------------------------------------------------------------------------

const QUEUE: &str = "legacy queue files";

/// `.beads/mq/*.json` — merge-queue entries written by an orchestrator that no
/// longer exists. Local-only, never committed, and read by nothing in this port.
///
/// Pure debris, which makes it the ideal `--fix` target: warn, delete on request,
/// and touch nothing that is not a `*.json` sitting directly in that one
/// directory.
struct LegacyQueueFiles;

fn queue_files(beads: &Path) -> Vec<PathBuf> {
    let mq = beads.join("mq");
    let Ok(entries) = shallow(&mq) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|(name, _, md)| md.is_file() && name.ends_with(".json"))
        .map(|(_, path, _)| path)
        .collect()
}

#[async_trait]
impl Check for LegacyQueueFiles {
    fn name(&self) -> &'static str {
        QUEUE
    }
    fn category(&self) -> Category {
        Category::Maintenance
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(beads) = beads_dir(dx) else {
            return Finding::ok(QUEUE, "no workspace here");
        };
        let files = queue_files(&beads);
        if files.is_empty() {
            return Finding::ok(QUEUE, "none");
        }
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        Finding::warn(
            QUEUE,
            format!("{} leftover file(s) in .beads/mq/", files.len()),
        )
        .detail(format!(
            "merge-queue entries from an orchestrator this port does not have. Nothing reads \
             them; they are local-only and were never committed.\n{}",
            sample(&names, 5)
        ))
        .fix("`bd doctor --fix` deletes them (or `rm -r .beads/mq`)")
    }

    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(beads) = beads_dir(dx) else {
            return Ok(Repair::Unfixable);
        };
        let files = queue_files(&beads);
        if files.is_empty() {
            return Ok(Repair::Unfixable);
        }
        let mut n = 0usize;
        for f in &files {
            match std::fs::remove_file(f) {
                Ok(()) => n += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("cannot remove {}: {e}", f.display()),
            }
        }
        // Only if we emptied it. `remove_dir` refuses a non-empty directory, which
        // is exactly the guard we want: whatever else is in there is not ours.
        let _ = std::fs::remove_dir(beads.join("mq"));
        Ok(Repair::Did(format!("removed {n} legacy merge-queue file(s) from .beads/mq/")))
    }
}

// ---------------------------------------------------------------------------
// Interrupted writes
// ---------------------------------------------------------------------------

const TMP: &str = "interrupted writes";

/// Every important file in `.beads/` is written to a temporary and renamed into
/// place, so that a crash mid-write cannot leave a half-written workspace. The
/// cost is that a crash *does* leave the temporary.
///
/// The age threshold is the whole safety argument: a write-then-rename takes
/// milliseconds, so a `.tmp` file older than this cannot be one that is still in
/// flight. Deleting a temporary out from under a live `Locator::save` would make
/// its rename fail.
const TMP_SETTLE: Duration = Duration::from_secs(300);

struct InterruptedWrites;

/// Deliberately **not** `beads.db-wal`, `beads.db-shm` or `beads.db-journal`:
/// those are SQLite's live sidecars, they exist whenever the database is open,
/// and deleting one is data loss. Only files this port itself writes as
/// temporaries and renames away.
fn temp_files(beads: &Path, settled: bool) -> Vec<(String, PathBuf)> {
    let Ok(entries) = shallow(beads) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|(name, _, md)| {
            md.is_file()
                && (name.ends_with(".tmp") || name.ends_with(".bd-tmp"))
                && (!settled || older_than(md, TMP_SETTLE))
        })
        .map(|(name, path, _)| (name, path))
        .collect()
}

#[async_trait]
impl Check for InterruptedWrites {
    fn name(&self) -> &'static str {
        TMP
    }
    fn category(&self) -> Category {
        Category::Maintenance
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(beads) = beads_dir(dx) else {
            return Finding::ok(TMP, "no workspace here");
        };
        let stale = temp_files(&beads, true);
        if stale.is_empty() {
            return Finding::ok(TMP, "none");
        }
        let names: Vec<String> = stale.iter().map(|(n, _)| n.clone()).collect();
        Finding::warn(
            TMP,
            format!("{} temporary file(s) left behind in .beads/", stale.len()),
        )
        .detail(format!(
            "bd writes to a temporary and renames it into place; these are the ones a crash left \
             behind. They are inert — nothing reads them.\n{}",
            sample(&names, 5)
        ))
        .fix("`bd doctor --fix` deletes them")
    }

    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(beads) = beads_dir(dx) else {
            return Ok(Repair::Unfixable);
        };
        // `settled` again, and re-read from disk: a temporary that appeared since
        // `run()` belongs to a write that is happening right now.
        let stale = temp_files(&beads, true);
        if stale.is_empty() {
            return Ok(Repair::Unfixable);
        }
        let mut n = 0usize;
        for (_, path) in &stale {
            match std::fs::remove_file(path) {
                Ok(()) => n += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("cannot remove {}: {e}", path.display()),
            }
        }
        Ok(Repair::Did(format!("removed {n} leftover temporary file(s) from .beads/")))
    }
}

// ---------------------------------------------------------------------------
// Legacy git hooks
// ---------------------------------------------------------------------------

const HOOKS: &str = "legacy git hooks";

/// Hooks that call `bd hook <name>` — a command that no longer exists. It was
/// replaced by `bd hooks run <name>`, and the difference is one letter, so the
/// hook looks right, `bd hooks list` looks right, and every commit prints an
/// "unknown command" error.
///
/// Upstream only scans `*.legacy` sidecars (the ones Python's pre-commit
/// framework leaves). We scan every hook, because an old hand-written or
/// old-bd-installed `pre-commit` is broken in exactly the same way and is more
/// common.
struct LegacyGitHooks;

const REMOVED_HOOKS: &[&str] = &[
    "pre-commit",
    "post-merge",
    "pre-push",
    "post-checkout",
    "prepare-commit-msg",
    "post-commit",
    "pre-rebase",
];

/// Find a call to the *removed* `bd hook <name>` command.
///
/// A substring search for `"bd hook"` would be a bug: every hook this port
/// installs contains `bd hooks run <name>`, and `"bd hooks run"` contains
/// `"bd hook"`. So the match requires whitespace after `hook` — which `hooks`
/// does not have — and a known hook name after that.
fn calls_removed_hook(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut from = 0usize;
    while let Some(off) = text[from..].find("bd") {
        let at = from + off;
        from = at + 2;

        // A word boundary before `bd`, so `abd hook` and `xbd hook` do not match.
        if at > 0 {
            let prev = bytes[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'_' || prev == b'.' {
                continue;
            }
        }
        let rest = &text[at + 2..];
        let after_bd = rest.trim_start_matches([' ', '\t']);
        if after_bd.len() == rest.len() {
            continue; // no space after `bd`
        }
        let Some(after_hook) = after_bd.strip_prefix("hook") else {
            continue;
        };
        let name_start = after_hook.trim_start_matches([' ', '\t']);
        if name_start.len() == after_hook.len() {
            continue; // `hooks`, `hookfoo` — not the removed command
        }
        let name: String = name_start
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if REMOVED_HOOKS.contains(&name.as_str()) {
            return Some(format!("bd hook {name}"));
        }
    }
    None
}

/// Where git will actually look for hooks. `git rev-parse` knows about linked
/// worktrees (`.git` is a *file*) and about `core.hooksPath`; guessing
/// `.git/hooks` gets both wrong.
///
/// `Err` means "could not ask git" — a missing binary, which is a
/// [`Finding::unknown`], not an "all clear".
fn git_hooks_dir(cwd: &Path) -> std::result::Result<Option<PathBuf>, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Ok(None); // not a git repository: there are no hooks to be wrong
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Ok(None);
    }
    let p = PathBuf::from(s);
    Ok(Some(if p.is_absolute() { p } else { cwd.join(p) }))
}

/// A hook is a script. Anything much bigger than this is not one, and we are not
/// going to read a gigabyte to find out.
const MAX_HOOK_BYTES: u64 = 256 * 1024;

#[async_trait]
impl Check for LegacyGitHooks {
    fn name(&self) -> &'static str {
        HOOKS
    }
    fn category(&self) -> Category {
        Category::Maintenance
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let dir = match git_hooks_dir(&dx.ctx.cwd) {
            Err(why) => return Finding::unknown(HOOKS, why),
            Ok(None) => return Finding::ok(HOOKS, "not a git repository, so there are no hooks"),
            Ok(Some(d)) => d,
        };
        let entries = match shallow(&dir) {
            Ok(e) => e,
            // A hooks directory that does not exist is a repository with no hooks.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Finding::ok(HOOKS, "no hooks installed");
            }
            Err(e) => return Finding::unknown(HOOKS, format!("cannot read {}: {e}", dir.display())),
        };

        let mut bad = Vec::new();
        let mut unread = Vec::new();
        for (name, path, md) in entries {
            if !md.is_file() || name.ends_with(".sample") || md.len() > MAX_HOOK_BYTES {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    if let Some(call) = calls_removed_hook(&text) {
                        bad.push(format!("{name}: calls `{call}`"));
                    }
                }
                // Not text (a compiled hook), or unreadable. Either way we did not
                // look inside it, and saying "ok" would be claiming we had.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
                Err(e) => unread.push(format!("{name}: {e}")),
            }
        }

        if !bad.is_empty() {
            return Finding::warn(
                HOOKS,
                format!("{} git hook(s) call `bd hook`, a command bd no longer has", bad.len()),
            )
            .detail(format!(
                "{}\nThese fail at runtime with \"unknown command\" on every commit, while \
                 `bd hooks list` still shows green.",
                sample(&bad, 5)
            ))
            .fix(
                "edit each one: `bd hook <name>` became `bd hooks run <name>`. bd will not rewrite \
                 a hook it did not write — the file may be yours.",
            );
        }
        if !unread.is_empty() {
            return Finding::unknown(
                HOOKS,
                format!("could not read {} hook file(s):\n{}", unread.len(), sample(&unread, 3)),
            );
        }
        Finding::ok(HOOKS, "no hooks call the removed `bd hook` command")
    }

    // No repair. `bd hooks install` refuses to overwrite a hook bd did not write,
    // and this check would be a back door around that refusal: a hook we did not
    // author may have a dozen lines of somebody's own in it, and "fixing" it means
    // editing their file. Report, and let them look.
}

// ---------------------------------------------------------------------------
// Test pollution
// ---------------------------------------------------------------------------

const POLLUTION: &str = "test pollution";

/// Beads that look like they were created by a test run against a real workspace.
///
/// This one **never repairs**. `--fix` is allowed to delete a lock file, because
/// a lock file is bookkeeping; it is not allowed to delete issues, because issues
/// are the product. Upstream's `--clean` deletes them after an interactive
/// confirmation and a JSONL backup — `doctor --fix` has no human in the loop, so
/// the honest thing is to hand over the ids and stop.
struct TestPollution;

const TEST_PREFIXES: &[&str] = &["test", "benchmark", "sample", "tmp", "temp", "debug", "dummy"];

/// `test-foo`, `Test issue 3`, `tmp_thing`. Not `template`, not `testing the
/// parser` — the separator is required, and it is what keeps this from matching
/// real work.
fn has_test_prefix(title_lower: &str) -> bool {
    TEST_PREFIXES.iter().any(|p| {
        title_lower
            .strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(['-', '_', ' ']))
    })
}

fn has_generic_test_title(title_lower: &str) -> bool {
    ["test issue", "issue for testing", "sample issue", "dummy issue"]
        .iter()
        .any(|p| title_lower.contains(p))
}

#[derive(Debug, Clone, PartialEq)]
struct Suspect {
    id: String,
    title: String,
    score: f32,
    reasons: Vec<String>,
}

/// The threshold. A single strong signal (a test-prefixed title) reaches it; no
/// combination of weak signals does.
const SUSPECT: f32 = 0.7;
const CONFIDENT: f32 = 0.9;

/// Score every issue, and keep the ones over the line.
///
/// Two of upstream's signals are gone, and both were false-positive machines:
///
/// * **Sequential id** (`^[a-z]+-\d+$`, +0.4). Every id that upstream's own Go
///   implementation mints is sequential — `bd-1`, `bd-2` — so in any workspace
///   imported from it, this fires on *everything*. Combined with upstream's
///   rapid-creation signal (+0.3) it crosses the threshold on its own, which
///   means importing a real Go workspace flags the entire backlog as test litter.
/// * **Id prefix `test-`** (upstream's SQL `id LIKE 'test-%'`). The id prefix is
///   the *workspace* prefix: `bd init` in a directory called `test` gives every
///   issue in the project an id starting with `test-`.
///
/// What is left is about the title, which is where a test actually gives itself
/// away.
fn score_issues(issues: &[Issue]) -> Vec<Suspect> {
    // Rapid creation is a supporting signal only: it can never push an issue over
    // the line by itself (0.3), nor with the description signals (0.3 + 0.2 =
    // 0.5). That matters — a bulk import or a planning session legitimately
    // creates fifty issues in a minute, and none of them are test litter.
    let mut per_minute: BTreeMap<i64, usize> = BTreeMap::new();
    for i in issues {
        *per_minute.entry(i.created_at.timestamp() / 60).or_default() += 1;
    }

    let mut out = Vec::new();
    for i in issues {
        let title = i.title.to_lowercase();
        let mut score = 0.0f32;
        let mut reasons = Vec::new();

        if has_test_prefix(&title) {
            score += 0.7;
            reasons.push("the title starts with a test prefix".to_string());
        }
        if has_generic_test_title(&title) {
            score += 0.5;
            reasons.push("generic test title".to_string());
        }
        let desc = i.description.trim();
        if desc.is_empty() {
            score += 0.2;
            reasons.push("no description".to_string());
        } else if desc.len() < 20 {
            score += 0.1;
            reasons.push("a very short description".to_string());
        }
        let cluster = per_minute
            .get(&(i.created_at.timestamp() / 60))
            .copied()
            .unwrap_or(0);
        if cluster >= 10 {
            score += 0.3;
            reasons.push(format!("created in the same minute as {} others", cluster - 1));
        }

        if score >= SUSPECT {
            out.push(Suspect {
                id: i.id.clone(),
                title: i.title.clone(),
                score,
                reasons,
            });
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    out
}

#[async_trait]
impl Check for TestPollution {
    fn name(&self) -> &'static str {
        POLLUTION
    }
    fn category(&self) -> Category {
        Category::Maintenance
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(store) = dx.store().await else {
            // Not `ok`. We established nothing.
            return Finding::unknown(
                POLLUTION,
                dx.store_error()
                    .unwrap_or("there is no database to look in")
                    .to_string(),
            );
        };

        // Ephemeral beads are *supposed* to be transient; `bd gc` reaps them.
        // Pushed down, so this is one indexed query and not a scan of the wisps.
        let filter = IssueFilter {
            ephemeral: Some(false),
            ..Default::default()
        };
        let issues = match store.list_issues(&filter).await {
            Ok(i) => i,
            Err(e) => return Finding::unknown(POLLUTION, format!("cannot list issues: {e}")),
        };

        let suspects = score_issues(&issues);
        if suspects.is_empty() {
            return Finding::ok(POLLUTION, "no issues look like test data");
        }

        let confident = suspects.iter().filter(|s| s.score >= CONFIDENT).count();
        let lines: Vec<String> = suspects
            .iter()
            .map(|s| {
                format!(
                    "{}  ({:.1})  {:?} — {}",
                    s.id,
                    s.score,
                    s.title,
                    s.reasons.join("; ")
                )
            })
            .collect();

        let msg = match confident {
            0 => format!("{} issue(s) look like test data", suspects.len()),
            n => format!(
                "{} issue(s) look like test data, {n} of them strongly",
                suspects.len()
            ),
        };
        Finding::warn(POLLUTION, msg)
            .detail(sample(&lines, 5))
            .fix(
                "look at them (`bd show <id>`) and delete the ones that are litter \
                 (`bd delete <id>`). `bd doctor --fix` will not do this for you: these are issues, \
                 and a heuristic does not get to delete your data.",
            )
    }

    // Intentionally no repair. See the struct docs — this is the one place in the
    // family where the obvious `--fix` is the wrong thing to build.
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bd_core::Issue;
    use chrono::{TimeZone, Utc};

    fn issue(id: &str, title: &str, desc: &str, minute: i64) -> Issue {
        let mut i = Issue::new(id, title);
        i.description = desc.to_string();
        i.created_at = Utc.timestamp_opt(minute * 60, 0).unwrap();
        i
    }

    // --- locks ------------------------------------------------------------

    /// The property the whole family is judged on. A lock whose owner is alive is
    /// *in use*; calling it stale and deleting it destroys a running session.
    #[test]
    fn a_lock_whose_owner_is_alive_is_held_not_stale() {
        let body = "pid=4242\nstarted=2026-07-14T12:00:00Z\n";
        assert_eq!(classify(body, None, &|_| Life::Alive), Lock::Held(4242));
        assert_eq!(classify(body, None, &|_| Life::Dead), Lock::Orphaned(4242));
    }

    /// "I could not tell" must never collapse into "it is dead". On a machine
    /// with no `tasklist` — or a lock written on another host — this is the
    /// answer, and it must not authorize a delete.
    #[test]
    fn an_undeterminable_owner_is_not_a_dead_one() {
        let body = "pid=4242\n";
        let state = classify(body, None, &|_| Life::Unknown);
        assert_eq!(state, Lock::Undetermined(4242));
        assert!(
            !matches!(state, Lock::Orphaned(_)),
            "only Orphaned is ever deleted, and this is not it"
        );
    }

    /// An empty lock file is a *released* lock, not debris: beads truncates on
    /// release and keeps the path on purpose. It is also what an acquiring process
    /// has for a moment before it writes its pid. Deleting either is a bug —
    /// which is precisely the bug in upstream's age-only check.
    #[test]
    fn an_empty_lock_file_is_released_and_never_touched() {
        assert_eq!(classify("", None, &|_| Life::Dead), Lock::Released);
        assert_eq!(classify("\n  \n", None, &|_| Life::Dead), Lock::Released);
    }

    #[test]
    fn a_lock_with_no_pid_is_anonymous_rather_than_assumed_dead() {
        assert_eq!(classify("locked by something else\n", None, &|_| Life::Dead), Lock::Anonymous);
        assert_eq!(lock_pid("pid=0\n"), None, "pid 0 is not a process");
        assert_eq!(lock_pid("pid=abc\n"), None);
        assert_eq!(lock_pid("pid=17\nstarted=x\n"), Some(17));
    }

    /// The network-share bug. A lock written by another machine records its own
    /// `host=`, and its pid names a process in a table we cannot see. Probing it
    /// locally is meaningless: the pid may not exist here even though the owner is
    /// alive over there. So a foreign host is `Undetermined` — reported, never
    /// deleted — **even when the local probe says the pid is dead**, which is the
    /// exact case that used to authorize `--fix` to delete a live lock.
    #[test]
    fn a_lock_from_another_machine_is_never_called_orphaned() {
        let body = "pid=4242\nhost=build-server-07\nstarted=2026-07-14T12:00:00Z\n";
        // We are "my-laptop"; the lock is from "build-server-07". Even though the
        // local probe reports Dead, it must not be Orphaned.
        let state = classify(body, Some("my-laptop"), &|_| Life::Dead);
        assert_eq!(state, Lock::Undetermined(4242));
        assert!(
            !matches!(state, Lock::Orphaned(_)),
            "a foreign-host lock must never be deletable, whatever the local pid probe says"
        );

        // Same machine (case-insensitively): fall back to the pid probe as before.
        let ours = "pid=4242\nhost=My-Laptop\n";
        assert_eq!(classify(ours, Some("my-laptop"), &|_| Life::Dead), Lock::Orphaned(4242));
        assert_eq!(classify(ours, Some("my-laptop"), &|_| Life::Alive), Lock::Held(4242));

        // And if we cannot determine our own host, treat any host-bearing lock as
        // foreign — the safe direction.
        assert_eq!(classify(body, None, &|_| Life::Dead), Lock::Undetermined(4242));
    }

    #[test]
    fn lock_names_cover_both_shapes_beads_writes() {
        assert!(is_lock_name(".sync.lock"));
        assert!(is_lock_name("dolt.bootstrap.lock"));
        assert!(is_lock_name(".linear-sync.lock"));
        assert!(is_lock_name("bd.sock.startlock"));
        assert!(!is_lock_name("beads.db"));
        assert!(!is_lock_name("issues.jsonl"));
        // Never the database's own sidecars.
        assert!(!is_lock_name("beads.db-wal"));
    }

    /// The one probe answer that authorizes deletion has to be a real answer.
    #[test]
    fn our_own_process_is_alive() {
        assert_eq!(liveness(std::process::id()), Life::Alive);
        assert_eq!(liveness(0), Life::Unknown, "pid 0 is not askable");
    }

    // --- manifest ---------------------------------------------------------

    #[test]
    fn a_coherent_manifest_is_ok() {
        let raw = r#"{"backend":"sqlite","workspace_id":"ws-1"}"#;
        assert_eq!(
            classify_manifest(raw, &["sqlite"]),
            ManifestState::Coherent("sqlite".into())
        );
        // No database yet is not a contradiction — the manifest is still telling
        // the truth. The missing store is the Core family's finding, not ours.
        assert_eq!(
            classify_manifest(raw, &[]),
            ManifestState::Coherent("sqlite".into())
        );
    }

    /// The `Error` case: the manifest names one engine and the data belongs to
    /// another, so every command opens the wrong store.
    #[test]
    fn a_manifest_that_names_the_wrong_engine_is_an_error() {
        let raw = r#"{"backend":"sqlite","workspace_id":"ws-1"}"#;
        let ManifestState::Wrong(why) = classify_manifest(raw, &["dolt"]) else {
            panic!("a manifest that contradicts the disk must be Wrong");
        };
        assert!(why.contains("sqlite") && why.contains("dolt"), "{why}");
    }

    #[test]
    fn a_corrupt_manifest_names_what_is_wrong_with_it() {
        assert!(matches!(
            classify_manifest("{not json", &[]),
            ManifestState::Corrupt(_)
        ));
        assert!(matches!(
            classify_manifest("[]", &[]),
            ManifestState::Corrupt(_)
        ));
        assert!(matches!(
            classify_manifest(r#"{"workspace_id":"ws-1"}"#, &[]),
            ManifestState::Corrupt(_),
        ));
        assert!(
            matches!(
                classify_manifest(r#"{"backend":"cuneiform","workspace_id":"w"}"#, &[]),
                ManifestState::Corrupt(_)
            ),
            "a backend beads has never heard of is a corrupt manifest, not a coherent one"
        );
    }

    #[test]
    fn a_workspace_with_no_id_cannot_be_told_apart_from_another() {
        assert!(matches!(
            classify_manifest(r#"{"backend":"sqlite","workspace_id":""}"#, &["sqlite"]),
            ManifestState::Wrong(_)
        ));
    }

    // --- legacy hooks -----------------------------------------------------

    /// The one that makes the check worth having: `bd hooks run` *contains*
    /// `bd hook`, so a substring test flags every correct hook this port installs.
    #[test]
    fn the_hook_matcher_does_not_flag_the_hook_we_install() {
        let ours = "#!/bin/sh\ncommand -v bd >/dev/null 2>&1 || exit 0\nexec bd hooks run pre-commit\n";
        assert_eq!(calls_removed_hook(ours), None);
    }

    #[test]
    fn the_hook_matcher_finds_the_removed_command() {
        assert_eq!(
            calls_removed_hook("#!/bin/sh\nexec bd hook pre-commit\n").as_deref(),
            Some("bd hook pre-commit")
        );
        assert_eq!(
            calls_removed_hook("bd  hook   post-merge --quiet").as_deref(),
            Some("bd hook post-merge")
        );
        // A word that merely ends in "bd" is not bd.
        assert_eq!(calls_removed_hook("abd hook pre-push"), None);
        // A hook name bd never had.
        assert_eq!(calls_removed_hook("bd hook frobnicate"), None);
        assert_eq!(calls_removed_hook("bd hooks list"), None);
    }

    // --- test pollution ---------------------------------------------------

    #[test]
    fn a_test_titled_issue_is_flagged_on_its_title_alone() {
        let issues = vec![
            issue("t-1", "test-create-then-close", "", 100),
            issue("t-2", "Test Issue 3", "", 100),
            issue("t-3", "tmp_scratch", "", 100),
        ];
        let s = score_issues(&issues);
        assert_eq!(s.len(), 3, "got {s:?}");
        assert!(s.iter().all(|s| s.score >= SUSPECT));
    }

    /// The signal has to be *specific*, or the check is noise and gets ignored.
    #[test]
    fn real_work_that_merely_mentions_testing_is_left_alone() {
        let issues = vec![
            issue("bd-1", "Fix the flaky integration test", "It hangs on CI about one run in five.", 100),
            issue("bd-2", "Template the release notes", "", 100),
            issue("bd-3", "Add tests for the parser", "", 100),
            issue("bd-4", "Temperature conversion is off by one", "", 100),
        ];
        assert!(score_issues(&issues).is_empty(), "{:?}", score_issues(&issues));
    }

    /// Upstream flags a whole backlog here: fifty issues filed in one minute, each
    /// with a short description, cross its threshold on the rapid-creation +
    /// sequential-id signals alone. A planning session and a bulk import both look
    /// exactly like that, and neither is test litter.
    #[test]
    fn a_bulk_import_is_not_test_pollution() {
        let issues: Vec<Issue> = (0..50)
            .map(|n| issue(&format!("bd-{n}"), &format!("Ship the {n}th thing"), "", 100))
            .collect();
        assert!(
            score_issues(&issues).is_empty(),
            "fifty real issues created in one minute must not be flagged"
        );
    }

    #[test]
    fn a_clean_workspace_is_quiet() {
        assert!(score_issues(&[]).is_empty());
    }

    #[test]
    fn the_strongest_suspects_are_reported_first() {
        let issues = vec![
            issue("t-1", "test-a", "a description that is comfortably long", 1),
            issue("t-2", "test issue for the parser", "", 2),
        ];
        let s = score_issues(&issues);
        assert_eq!(s[0].id, "t-2", "the higher score must come first: {s:?}");
        assert!(s[0].score >= CONFIDENT);
    }

    // --- misc -------------------------------------------------------------

    #[test]
    fn a_temporary_that_might_still_be_in_flight_is_not_deleted() {
        let dir = std::env::temp_dir().join(format!("bd-poll-tmp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workspace.json.tmp"), "{}").unwrap();

        // Fresh: a write-then-rename in progress. Never a repair target.
        assert!(temp_files(&dir, true).is_empty(), "a brand-new .tmp is in flight");
        // The same file, without the settle guard, is the one we would find later.
        assert_eq!(temp_files(&dir, false).len(), 1);

        // And the database's live sidecars are never temporaries.
        std::fs::write(dir.join("beads.db-wal"), "x").unwrap();
        std::fs::write(dir.join("beads.db-shm"), "x").unwrap();
        assert_eq!(temp_files(&dir, false).len(), 1, "sqlite's sidecars are not debris");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_age_never_reads_as_old() {
        let dir = std::env::temp_dir().join(format!("bd-poll-age-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x");
        std::fs::write(&f, "x").unwrap();
        let md = std::fs::metadata(&f).unwrap();
        assert!(!older_than(&md, Duration::from_secs(1)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_check_here_is_a_maintenance_check_with_a_distinct_name() {
        let mut names = std::collections::HashSet::new();
        for c in checks() {
            assert_eq!(c.category(), Category::Maintenance, "{}", c.name());
            assert!(names.insert(c.name()), "duplicate name: {}", c.name());
        }
        assert_eq!(names.len(), 6);
    }

    /// The safety property, moved here from the Dolt family along with ownership
    /// of the sweeper.
    ///
    /// This family deletes lock files. `.beads/` **is** the dolt repository, so
    /// the sweeper is walking the root of a live database — and it must not match
    /// a single file that `bd-dolt` or dolt itself writes there. The names come
    /// from `bd-dolt`, not from a copy of them, so this stays true through a
    /// rename. It is the assertion that would catch a future widening of the rule
    /// to, say, `starts_with("dolt-server")` — which would delete the record of a
    /// *running* server, and the next `bd` would start a second one that dolt's
    /// own lock then rejects.
    #[test]
    fn the_sweeper_matches_nothing_that_bd_dolt_or_dolt_owns() {
        // bd-dolt's own files, by its own constants.
        assert!(!is_lock_name(bd_dolt::server::PID_FILE));
        assert!(!is_lock_name(bd_dolt::server::LOG_FILE));

        // Dolt's. `noms/LOCK` is the one that matters most: it is *advisory* —
        // the OS releases it on process death — so its presence proves nothing,
        // and deleting it can destroy the database.
        for name in ["LOCK", "manifest", "repo_state.json", ".dolt", ".doltignore"] {
            assert!(!is_lock_name(name), "{name} is dolt's, not debris");
        }

        // And beads' own.
        for name in ["workspace.json", "config.yaml", "beads.db", "issues.jsonl"] {
            assert!(!is_lock_name(name), "{name} is not debris");
        }

        // What it *does* match — including the two the Dolt family used to
        // duplicate, which is why that duplicate is gone.
        for name in ["dolt.bootstrap.lock", "dolt-server.lock", "bd.sock.startlock"] {
            assert!(is_lock_name(name), "{name} is a lock and should be swept");
        }
    }
}
