//! `bd doctor` — diagnose a workspace, and optionally repair it.
//!
//! # The one rule that shapes everything here
//!
//! **Doctor runs on workspaces that are too broken to open.** That is not an
//! edge case, it is the job. The database may be corrupt, the locator may be
//! missing, `.beads/` may not exist at all. Every other command in this program
//! may assume a working store; this one may assume nothing.
//!
//! Three consequences, and they are not negotiable:
//!
//! 1. [`Dx::store`] returns `Option`, not `Result`. A check that needs a store
//!    and doesn't get one reports a **warning about itself** ("could not check")
//!    — it does not fail the run, and it does not report the *absence* of a store
//!    as the *absence* of the problem it was looking for.
//! 2. A check must not be able to end the run. Checks are run inside
//!    [`std::panic::catch_unwind`], and a panicking check becomes an `Error`
//!    finding naming itself. One agent's stray `unwrap` cannot take down the
//!    other hundred checks — on the exact broken input where you needed them.
//! 3. A check **never mutates anything**. Repair lives in [`Check::repair`],
//!    which only `--fix` calls, and only for findings that are not already `Ok`.
//!
//! # Status means something
//!
//! * [`Status::Ok`] — I looked, and it is fine.
//! * [`Status::Warn`] — either "this is untidy but works", or "**I could not
//!   determine the answer**". Those are the same colour on purpose: both mean
//!   *you have not been told this is fine*.
//! * [`Status::Error`] — I looked, and it is broken. This, and only this, makes
//!   `bd doctor` exit nonzero.
//!
//! The failure mode to design against is a check that hits an error, swallows
//! it, and returns `Ok`. That is worse than having no check, because it reports
//! as coverage. When in doubt: `Warn`.

pub mod checks;

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use bd_storage::Storage;
use futures::FutureExt;
use serde::Serialize;
use tokio::sync::OnceCell;

use crate::context::Ctx;
use crate::exit::{self, SilentExit};

// ---------------------------------------------------------------------------
// What a check says
// ---------------------------------------------------------------------------

/// Ordered by severity: `Ok` < `NotApplicable` < `Unknown` < `Warn` < `Error`.
///
/// Five, not three. The three-status version shipped for one wave and every
/// family that used it reported the same two holes, independently:
///
/// * **"Nothing here to check"** had no home. A SQLite user has no Dolt problem;
///   a user without git has no git problem. Reporting `Ok` inflates the count of
///   things that were *verified* with things that were *skipped* — and reporting
///   `Warn` puts a permanent yellow line in front of every user who simply
///   doesn't use the feature, which is how you teach people to ignore `doctor`.
/// * **"I could not determine the answer"** was folded into `Warn`, so `--fix`
///   would cheerfully try to repair a condition the check had just admitted it
///   could not diagnose.
///
/// The serialized names are the contract agents grep. They match [`as_str`] and
/// the field names in [`Counts`] — which they did not, for one embarrassing
/// wave: `--json` said `"warn"` while everything human-facing said `"warning"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Status {
    /// Looked; it is fine.
    #[serde(rename = "ok")]
    Ok,
    /// There is nothing here to check. Silent, and counted apart from `Ok` so
    /// that "18 ok" means eighteen things were actually verified.
    #[serde(rename = "n/a")]
    NotApplicable,
    /// Could not determine. **Never `Ok`** — a check that swallows an error and
    /// reports fine is worse than no check, because it reports as coverage.
    #[serde(rename = "unknown")]
    Unknown,
    /// Untidy; it works.
    #[serde(rename = "warning")]
    Warn,
    /// Broken. The only status that makes `bd doctor` exit nonzero.
    #[serde(rename = "error")]
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::NotApplicable => "n/a",
            Status::Unknown => "unknown",
            Status::Warn => "warning",
            Status::Error => "error",
        }
    }

    /// Worth printing to a human. `Ok` and `NotApplicable` are not: they are the
    /// silent majority, and a report that lists them buries the four lines that
    /// matter under sixty that don't.
    fn is_noteworthy(self) -> bool {
        matches!(self, Status::Unknown | Status::Warn | Status::Error)
    }

    /// Something `--fix` could conceivably act on. Explicitly **not** `Unknown`:
    /// asking a check to repair what it could not diagnose is exactly backwards,
    /// and the natural implementation of `repair()` trusts a finding that was
    /// never computed.
    fn is_actionable(self) -> bool {
        matches!(self, Status::Warn | Status::Error)
    }

    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "ok  ",
            Status::NotApplicable => "n/a ",
            Status::Unknown => "????",
            Status::Warn => "warn",
            Status::Error => "FAIL",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Status::Ok => "32",
            Status::NotApplicable => "90",
            Status::Unknown => "35",
            Status::Warn => "33",
            Status::Error => "31",
        }
    }
}

/// Display grouping. The order of this enum *is* the display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Core,
    Data,
    Git,
    Dolt,
    Runtime,
    Performance,
    Integration,
    Federation,
    Metadata,
    Maintenance,
}

impl Category {
    pub fn title(self) -> &'static str {
        match self {
            Category::Core => "Core System",
            Category::Data => "Data & Config",
            Category::Git => "Git Integration",
            Category::Dolt => "Dolt Storage",
            Category::Runtime => "Runtime",
            Category::Performance => "Performance",
            Category::Integration => "Integrations",
            Category::Federation => "Federation",
            Category::Metadata => "Metadata",
            Category::Maintenance => "Maintenance",
        }
    }
}

/// What one check found.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub name: &'static str,
    pub status: Status,
    /// One line. Shown next to the check name.
    pub message: String,
    /// The evidence: the actual ids, paths, or counts. Optional, but a finding
    /// that says "3 issues are corrupt" without naming them is a bug report you
    /// cannot act on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the human should *do*. A command they can paste, ideally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<Category>,
}

impl Finding {
    pub fn ok(name: &'static str, message: impl Into<String>) -> Finding {
        Finding::new(name, Status::Ok, message)
    }

    /// Untidy, or undeterminable. Both are `Warn`.
    pub fn warn(name: &'static str, message: impl Into<String>) -> Finding {
        Finding::new(name, Status::Warn, message)
    }

    pub fn error(name: &'static str, message: impl Into<String>) -> Finding {
        Finding::new(name, Status::Error, message)
    }

    /// "I could not run." The check's precondition is missing *and that is a
    /// fault*: the store would not open, `git` is installed but refused.
    ///
    /// Deliberately not `Ok` — the check established nothing, and saying `Ok`
    /// here is how a diagnostic quietly stops diagnosing. Also deliberately not
    /// [`na`](Finding::na): "I could not look" and "there was nothing to look
    /// at" are different sentences, and only one of them is a problem.
    pub fn unknown(name: &'static str, why: impl Into<String>) -> Finding {
        Finding::new(name, Status::Unknown, "could not check").detail(why)
    }

    /// "There is nothing here to check." Not a fault, and not coverage either.
    ///
    /// A SQLite user has no Dolt problem; a user without git has no git problem;
    /// a `bd doctor` outside a workspace has no workspace to diagnose. Three
    /// separate families reached for this and had to invent it, each differently,
    /// because the seam did not offer it.
    pub fn na(name: &'static str, why: impl Into<String>) -> Finding {
        Finding::new(name, Status::NotApplicable, why)
    }

    fn new(name: &'static str, status: Status, message: impl Into<String>) -> Finding {
        Finding {
            name,
            status,
            message: message.into(),
            detail: None,
            fix: None,
            category: None,
        }
    }

    pub fn detail(mut self, d: impl Into<String>) -> Finding {
        self.detail = Some(d.into());
        self
    }

    pub fn fix(mut self, f: impl Into<String>) -> Finding {
        self.fix = Some(f.into());
        self
    }

    /// Nothing is wrong here.
    ///
    /// True for [`Status::NotApplicable`] as well as [`Status::Ok`]: a check with
    /// nothing to look at has not found a problem. The two are distinct in the
    /// *report* — `ok` must mean "verified", so the counts keep them apart — but
    /// to a caller asking "is anything wrong", they are the same answer.
    pub fn is_ok(&self) -> bool {
        !self.status.is_noteworthy()
    }
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// One diagnostic.
///
/// Implement this, add it to your family's `checks()` vector, and it appears in
/// `bd doctor`. Nothing else in the program needs to change — which is the whole
/// point: the registry is composed per-family, so no two authors ever edit the
/// same file to register a check.
#[async_trait]
pub trait Check: Send + Sync {
    /// Stable, human-facing. This is the key agents will grep for in `--json`,
    /// so changing it is a breaking change.
    fn name(&self) -> &'static str;

    fn category(&self) -> Category;

    /// Look, and report. **Must not mutate anything.**
    async fn run(&self, dx: &Dx<'_>) -> Finding;

    /// Repair what [`run`](Check::run) found. Only `--fix` calls this, and only
    /// for a finding that is [`Status::Warn`] or [`Status::Error`] — never for
    /// one that is `Unknown`, because repairing what you could not diagnose is
    /// how `--fix` becomes the bug it was run to cure.
    ///
    /// **Re-derive your state here. Do not trust `found`.** Time passed between
    /// `run()` and this call, and a lock whose owner was dead may now be held by
    /// a live process.
    ///
    /// The default is "I cannot fix this", the honest answer for most checks.
    async fn repair(&self, _dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        Ok(Repair::Unfixable)
    }
}

#[derive(Debug, Clone)]
pub enum Repair {
    /// Did something. The string is past tense, and names what changed.
    Did(String),
    /// **Could have, and chose not to** — with a reason the user must see.
    ///
    /// Not the same as [`Unfixable`](Repair::Unfixable), and the difference is
    /// not cosmetic. A repair that finds the lock it was about to delete is now
    /// held by a live process, or that the export it was about to overwrite
    /// contains a teammate's unimported issues, is doing its most important work
    /// *by refusing*. The seam offered only "did it" or "cannot do it", so the
    /// one family that needed this had to report a correct, protective refusal
    /// as a **failure**.
    Declined(String),
    /// This check has no automatic repair at all. Not a failure.
    Unfixable,
}

// ---------------------------------------------------------------------------
// What a check gets
// ---------------------------------------------------------------------------

/// The world, as far as a check is allowed to see it.
///
/// Note what is `Option` here. All of it. A doctor context whose fields were
/// `Result` would push every check into deciding whether a missing workspace is
/// *its* error to report — and they would all decide differently.
pub struct Dx<'a> {
    pub ctx: &'a Ctx,
    /// The `.beads` directory, if there is one. `None` is a legitimate state to
    /// diagnose, not a reason to stop.
    pub dir: Option<PathBuf>,
    /// The enclosing git repository root, if any. Beads does not require git.
    pub root: Option<PathBuf>,
    /// Set once, the first time anyone asks for a store: the reason it would not
    /// open, or `None` if it did. Without this, a hundred checks would each
    /// retry a failing database open.
    probe: OnceCell<Option<String>>,
}

impl<'a> Dx<'a> {
    pub fn new(ctx: &'a Ctx) -> Dx<'a> {
        Dx {
            // `beads_dir`, not `locator.dir`: a `.beads/` whose `workspace.json`
            // is corrupt has no locator, and it is the single state this command
            // most needs to see. Two families had to re-discover the directory
            // themselves because this used to hand them `None`.
            dir: ctx.beads_dir.clone(),
            root: git_root(&ctx.cwd),
            ctx,
            probe: OnceCell::new(),
        }
    }

    /// Why the workspace's locator would not load, if it wouldn't.
    ///
    /// `dir` is `Some` and this is `Some` together: there *is* a `.beads/`, and
    /// it cannot say what it is. That is a fault, and it is the Core family's to
    /// report.
    pub fn locator_error(&self) -> Option<&str> {
        self.ctx.locator_error.as_deref()
    }

    /// Why `.beads/config.yaml` would not parse, if it wouldn't.
    pub fn config_error(&self) -> Option<&str> {
        self.ctx.config_error.as_deref()
    }

    /// The store, or `None` if there isn't one or it would not open.
    ///
    /// A check that gets `None` here should report [`Finding::unknown`] with
    /// [`store_error`](Dx::store_error) as the detail — **not** `Ok`, and not a
    /// hard error either. "The database is unopenable" is somebody else's
    /// finding to report (see the Core family); your check's job is to say that
    /// *it* could not run.
    ///
    /// # Doctor does not start a Dolt server
    ///
    /// Opening a Dolt store spawns (or adopts) a `dolt sql-server` subprocess.
    /// `bd doctor` is the command you run *when a workspace is wedged* — and
    /// starting a second server against a locked database is the exact lock
    /// collision this command exists to diagnose, not cause. It also mutates the
    /// workspace mid-diagnosis (a server writes its log and pid record), which a
    /// read-only inspection must not do.
    ///
    /// So on a Dolt workspace this refuses to *open* a store, returning `None`
    /// with an explanatory [`store_error`]. If a store is already open (a prior
    /// command in the same process opened it), that one is used. SQLite has no
    /// such side effect and opens normally.
    ///
    /// [`store_error`]: Dx::store_error
    pub async fn store(&self) -> Option<&dyn Storage> {
        // An already-open store is free to use, whatever the backend.
        if let Some(s) = self.ctx.try_store() {
            return Some(s);
        }
        if self.ctx.backend() == Some(bd_storage::Backend::Dolt) {
            self.probe
                .get_or_init(|| async {
                    Some(
                        "doctor will not start a dolt sql-server to inspect the graph — that is \
                         the lock collision it exists to diagnose. Run a command that opens the \
                         workspace (e.g. `bd ready`) if you need this check."
                            .to_string(),
                    )
                })
                .await;
            return None;
        }
        // Unchecked: the version gate in `Ctx::store` refuses mismatched
        // databases, and examining what other commands refuse is the doctor's
        // whole job. The `schema` check reports the mismatch precisely; the
        // other checks run their queries and report what they see.
        self.probe
            .get_or_init(|| async {
                self.ctx.store_unchecked().await.err().map(|e| format!("{e:#}"))
            })
            .await;
        // `try_store` never opens: by here, the probe above already decided.
        self.ctx.try_store()
    }

    /// Why the store would not open. `None` if it opened, or if nobody has asked
    /// yet — call [`store`](Dx::store) first.
    pub fn store_error(&self) -> Option<&str> {
        self.probe.get().and_then(|o| o.as_deref())
    }

    /// A path inside `.beads/`, or `None` outside a workspace.
    pub fn beads_path(&self, rel: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(rel))
    }

    pub fn in_workspace(&self) -> bool {
        self.dir.is_some()
    }
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct Opts {
    /// Apply repairs for anything not `Ok`.
    pub fix: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    path: Option<&'a Path>,
    checks: Vec<Finding>,
    ok: bool,
    counts: Counts,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    repairs: Vec<RepairRecord>,
}

/// The field names here are the same strings [`Status`] serializes to. They were
/// not, once, and `--json` shipped `"status": "warn"` next to
/// `"counts": {"warning": 3}` — six agents reported it independently.
#[derive(Serialize, Default, Clone, Copy)]
struct Counts {
    /// Verified fine. Does **not** include `n/a`: "18 ok" must mean eighteen
    /// things were actually looked at.
    ok: usize,
    #[serde(rename = "n/a")]
    na: usize,
    unknown: usize,
    warning: usize,
    error: usize,
}

#[derive(Serialize)]
struct RepairRecord {
    check: &'static str,
    /// `fixed` | `unfixable` | `failed`
    outcome: &'static str,
    message: String,
}

/// Run every registered check, print, and exit nonzero if anything is `Error`.
pub async fn run(ctx: &Ctx, opts: Opts) -> Result<()> {
    let dx = Dx::new(ctx);
    let registry = checks::registry();
    let mut findings = run_all(&registry, &dx).await;
    let mut repairs = Vec::new();

    if opts.fix {
        ctx.ensure_writable("run doctor --fix")?;
        repairs = repair_all(&registry, &findings, &dx).await;

        // Re-run, if anything actually changed. The report and — this is the
        // part that matters — the **exit code** must describe the world as it is
        // now, not as it was before we fixed it. Reporting the pre-repair
        // findings gave you "warn: 4 files not ignored" printed directly above
        // "fixed: added 4 patterns", and `bd doctor --fix` in CI would repair the
        // problem and then fail the build anyway.
        //
        // A *fresh* registry, not the one above: a family may cache a snapshot
        // of the world inside its own `checks()` (the graph family reads the
        // whole issue graph exactly once), and reusing it here would re-report
        // the pre-repair state no matter what the repair did.
        if repairs.iter().any(|r| r.outcome == "fixed") {
            findings = run_all(&checks::registry(), &Dx::new(ctx)).await;
        }
    }

    let counts = count(&findings);
    let failed = counts.error > 0;

    if ctx.out.is_json() {
        ctx.out.json_value(&Report {
            path: dx.dir.as_deref(),
            ok: !failed,
            counts,
            checks: findings,
            repairs,
        })?;
    } else {
        print_human(ctx, &dx, &findings, &repairs, counts);
    }

    if failed {
        // A real, determined problem with the workspace. Not a capability gap
        // (2) and not an unported command (64) — see `exit`.
        return Err(SilentExit(exit::FAILURE).into());
    }
    Ok(())
}

async fn run_all(registry: &[Box<dyn Check>], dx: &Dx<'_>) -> Vec<Finding> {
    let mut out = Vec::with_capacity(registry.len());
    for check in registry {
        out.push(run_one(check.as_ref(), dx).await);
    }
    out
}

/// One check, with a net under it.
///
/// `catch_unwind` is not defensive programming for its own sake. Doctor's input
/// is *broken workspaces*, which is exactly the input that turns a plausible
/// `unwrap` into a panic — and a panic here would take the other checks down
/// with it, on the one run where the user most needed them. Instead the panicking
/// check indicts itself and the rest still report.
async fn run_one(check: &dyn Check, dx: &Dx<'_>) -> Finding {
    let name = check.name();
    let category = check.category();
    let mut finding = match AssertUnwindSafe(check.run(dx)).catch_unwind().await {
        Ok(f) => f,
        Err(panic) => Finding::error(name, "the check itself panicked")
            .detail(panic_message(&panic))
            .fix("this is a bug in bd, not in your workspace — please report it"),
    };
    finding.category = Some(category);
    finding
}

fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panicked with a non-string payload".to_string()
    }
}

async fn repair_all(
    registry: &[Box<dyn Check>],
    findings: &[Finding],
    dx: &Dx<'_>,
) -> Vec<RepairRecord> {
    let mut out = Vec::new();
    for (check, found) in registry.iter().zip(findings) {
        // Only Warn and Error. Never `Unknown`: a check that could not determine
        // the answer has nothing for `--fix` to act on, and asking it to repair
        // anyway invites it to trust a finding that was never computed.
        if !found.status.is_actionable() {
            continue;
        }
        let rec = match AssertUnwindSafe(check.repair(dx, found)).catch_unwind().await {
            Ok(Ok(Repair::Did(what))) => RepairRecord {
                check: check.name(),
                outcome: "fixed",
                message: what,
            },
            // Declining is *doing the job*, and it must not read as a failure.
            Ok(Ok(Repair::Declined(why))) => RepairRecord {
                check: check.name(),
                outcome: "declined",
                message: why,
            },
            Ok(Ok(Repair::Unfixable)) => RepairRecord {
                check: check.name(),
                outcome: "unfixable",
                message: "no automatic repair; fix it by hand".to_string(),
            },
            // A repair that fails must say so. Reporting it as "fixed" would
            // send the user away believing a broken workspace is mended.
            Ok(Err(e)) => RepairRecord {
                check: check.name(),
                outcome: "failed",
                message: format!("{e:#}"),
            },
            Err(p) => RepairRecord {
                check: check.name(),
                outcome: "failed",
                message: format!("the repair panicked: {}", panic_message(&p)),
            },
        };
        out.push(rec);
    }
    out
}

fn count(findings: &[Finding]) -> Counts {
    let mut c = Counts::default();
    for f in findings {
        match f.status {
            Status::Ok => c.ok += 1,
            Status::NotApplicable => c.na += 1,
            Status::Unknown => c.unknown += 1,
            Status::Warn => c.warning += 1,
            Status::Error => c.error += 1,
        }
    }
    c
}

fn print_human(
    ctx: &Ctx,
    dx: &Dx<'_>,
    findings: &[Finding],
    repairs: &[RepairRecord],
    counts: Counts,
) {
    let out = &ctx.out;
    match &dx.dir {
        Some(d) => out.line(format!("beads workspace: {}", d.display())),
        None => out.line("no beads workspace here (run `bd init`)"),
    }
    out.line("");

    // Group by category, in enum order. `BTreeMap` because `Category` is `Ord`
    // and the enum's declaration order *is* the display order.
    let mut by_cat: BTreeMap<Category, Vec<&Finding>> = BTreeMap::new();
    for f in findings {
        by_cat
            .entry(f.category.unwrap_or(Category::Core))
            .or_default()
            .push(f);
    }

    for (cat, group) in &by_cat {
        // A category with nothing to say gets one line, not a heading and a wall
        // of green. This is what lets a family register ten checks and still cost
        // the reader one line when all ten are fine — and it is why no family
        // needs a "roll-up" primitive.
        if !group.iter().any(|f| f.status.is_noteworthy()) {
            let checked = group.iter().filter(|f| f.status == Status::Ok).count();
            // A category that is entirely `n/a` (no git; not a dolt workspace)
            // says so quietly rather than claiming a clean bill of health.
            let (glyph, status) = if checked == 0 {
                ("n/a ", Status::NotApplicable)
            } else {
                ("ok  ", Status::Ok)
            };
            out.line(format!(
                "{}  {} ({})",
                out.paint(glyph, status.color()),
                cat.title(),
                summarize(group)
            ));
            continue;
        }

        out.line(format!("{}:", cat.title()));
        for f in group {
            if !f.status.is_noteworthy() {
                continue;
            }
            out.line(format!(
                "  {}  {}: {}",
                out.paint(f.status.glyph(), f.status.color()),
                f.name,
                f.message
            ));
            if let Some(d) = &f.detail {
                for line in d.lines() {
                    out.line(format!("        {line}"));
                }
            }
            if let Some(fix) = &f.fix {
                out.line(format!("        → {fix}"));
            }
        }
        out.line("");
    }

    if !repairs.is_empty() {
        out.line("repairs:");
        for r in repairs {
            let color = match r.outcome {
                "fixed" => "32",
                "failed" => "31",
                _ => "33",
            };
            out.line(format!(
                "  {}  {}: {}",
                out.paint(r.outcome, color),
                r.check,
                r.message
            ));
        }
        out.line("");
    }

    let mut parts = vec![format!("{} ok", counts.ok)];
    // Zeroes are noise. Only `error` is always shown, because "0 error" is the
    // sentence the reader came for.
    if counts.na > 0 {
        parts.push(format!("{} n/a", counts.na));
    }
    if counts.unknown > 0 {
        parts.push(format!("{} unknown", counts.unknown));
    }
    if counts.warning > 0 {
        parts.push(format!("{} warning", counts.warning));
    }
    parts.push(format!("{} error", counts.error));
    out.line(parts.join(", "));
}

/// "7 checks" / "3 checks, 4 n/a" — so a collapsed green line never implies it
/// verified things it actually skipped.
fn summarize(group: &[&Finding]) -> String {
    let ok = group.iter().filter(|f| f.status == Status::Ok).count();
    let na = group.len() - ok;
    match (ok, na) {
        (0, n) => format!("{n} n/a"),
        (o, 0) => format!("{o} checks"),
        (o, n) => format!("{o} checks, {n} n/a"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Panicky;

    #[async_trait]
    impl Check for Panicky {
        fn name(&self) -> &'static str {
            "panicky"
        }
        fn category(&self) -> Category {
            Category::Core
        }
        async fn run(&self, _dx: &Dx<'_>) -> Finding {
            panic!("an unwrap on a corrupt workspace")
        }
    }

    struct Fine;

    #[async_trait]
    impl Check for Fine {
        fn name(&self) -> &'static str {
            "fine"
        }
        fn category(&self) -> Category {
            Category::Core
        }
        async fn run(&self, _dx: &Dx<'_>) -> Finding {
            Finding::ok("fine", "all good")
        }
    }

    /// The property the whole runner exists for: doctor's input is broken
    /// workspaces, so a check *will* eventually panic on one. When it does it
    /// must indict itself and leave the other checks standing — a doctor that
    /// dies on the first bad check is useless exactly when it is needed.
    #[tokio::test]
    async fn a_panicking_check_becomes_a_finding_and_does_not_take_the_run_down() {
        use clap::Parser as _;
        let cli = crate::cli::Cli::parse_from(["bd", "doctor"]);
        let ctx = Ctx::build(&cli, crate::context::Need::Nothing)
            .await
            .unwrap();
        let dx = Dx::new(&ctx);

        let bad = run_one(&Panicky, &dx).await;
        assert_eq!(bad.status, Status::Error);
        assert!(
            bad.detail.unwrap().contains("an unwrap on a corrupt workspace"),
            "the panic message is the evidence; losing it makes the bug unfindable"
        );

        // And the runner keeps going.
        let good = run_one(&Fine, &dx).await;
        assert_eq!(good.status, Status::Ok);
    }

    /// The three states that are all "not Ok" and are all *different*.
    ///
    /// An undeterminable check that reports `Ok` is worse than no check, because
    /// it reports as coverage. But it is not a *warning* either — nothing is
    /// wrong, we just don't know — and above all `--fix` must not try to repair
    /// it, because a repair that trusts a finding which was never computed is how
    /// `--fix` becomes the bug it was run to cure.
    #[test]
    fn unknown_na_and_ok_are_three_different_things() {
        let unknown = Finding::unknown("x", "the database would not open");
        assert_eq!(unknown.status, Status::Unknown);
        assert!(!unknown.is_ok());
        assert!(
            !unknown.status.is_actionable(),
            "--fix must never repair what a check could not diagnose"
        );

        let na = Finding::na("x", "not a dolt workspace");
        assert_eq!(na.status, Status::NotApplicable);
        assert!(!na.status.is_actionable());
        assert!(
            !na.status.is_noteworthy(),
            "a user who does not use the feature must not be shown a line about it"
        );

        // And the two statuses `--fix` *may* act on.
        assert!(Status::Warn.is_actionable());
        assert!(Status::Error.is_actionable());
    }

    /// The serialized names are the contract agents grep, and they must agree
    /// with the human-facing strings. They did not, once: `--json` emitted
    /// `"status": "warn"` beside `"counts": {"warning": 3}`.
    #[test]
    fn the_json_status_and_the_human_status_are_the_same_word() {
        for s in [
            Status::Ok,
            Status::NotApplicable,
            Status::Unknown,
            Status::Warn,
            Status::Error,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(
                json.trim_matches('"'),
                s.as_str(),
                "{s:?} serializes as {json} but as_str() says {:?}",
                s.as_str()
            );
        }
    }
}
