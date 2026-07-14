//! Keeping a workspace healthy: sweeping what has expired, deleting what is
//! genuinely dead, and refusing — precisely — what this backend cannot do.
//!
//! # Three answers, and they are not interchangeable
//!
//! * **Done.** `preflight`, `reclaim`, `gc`, `prune`, `purge`, `admin cleanup`.
//! * **Exit 2 — a real "no".** `backup`, and only `backup`. Upstream's backup is
//!   a *Dolt* backup: `backup init <path>` registers a backup remote, `backup
//!   sync` pushes to it, and what it preserves is the database — branches,
//!   commit history, working set. That is [`Cap::Remote`], so a store with no
//!   commit graph is not missing a feature, it is being asked the wrong question.
//! * **Exit 64 — not built.** `compact`, `migrate`, `rename-prefix`, `worktree`,
//!   `merge-slot`, `admin reset`.
//!
//! `compact` and `migrate` are deliberately **not** exit 2, tempting as it looks.
//! SQLite compacts (`VACUUM`) and SQLite has a schema. Neither is a commit-graph
//! feature, so "the sqlite backend cannot do that" would be a lie — and exit 2 is
//! supposed to be a *true, final* answer that nobody ever revisits. What they are
//! missing is a seam method; see each one's doc comment for the signature.
//!
//! # Previewing, and consenting
//!
//! Two switches, and they are not the same switch.
//!
//! **`--dry-run`** (and the global `--readonly`, which implies it) asks a sweep
//! to report what it *would* do and write nothing. Both exit 0: the caller asked
//! for no writes and got none, which is a success. A preview that exits 1 is not
//! a preview.
//!
//! **`--yes`** is consent, in writing, for `bd purge` — the one command here that
//! deletes real work. Purge asks a human when there is one; when stdout is a pipe
//! there is nobody to answer, and silence is not a yes. Without `--yes` a scripted
//! purge could not succeed *at all*, which made it a destructive command that
//! always failed — the worst of both, because the only way to run it was to stop
//! scripting it.

use std::io::{IsTerminal, Write};

use anyhow::Result;
use bd_core::{Issue, IssueFilter, Status};
use bd_storage::Storage;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;

use crate::cli::{AdminCmd, BackupCmd, MergeSlotCmd, WorktreeCmd};
use crate::commands::{Cap, require_cap, stub};
use crate::context::Ctx;
use crate::exit::{self, SilentExit};

/// Wave 4 owns the doctor checks; this stays a stub until the registry lands.
pub async fn doctor(ctx: &Ctx) -> Result<()> {
    stub("doctor", ctx)
}

// ---------------------------------------------------------------------------
// preflight
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    /// Worth saying out loud; does not stop you working.
    Warn,
    Fail,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Warn => "warn",
            Verdict::Fail => "fail",
        }
    }
}

struct Finding {
    check: &'static str,
    verdict: Verdict,
    detail: String,
}

fn finding(check: &'static str, verdict: Verdict, detail: impl Into<String>) -> Finding {
    Finding {
        check,
        verdict,
        detail: detail.into(),
    }
}

/// Everything that has to be true before an agent starts work.
///
/// Runs in [`Need::Nothing`](crate::context::Need), so there may be no workspace
/// at all — and "there is no workspace" is precisely the thing a preflight exists
/// to tell you. It is therefore a *finding*, reported in the same shape as every
/// other finding, not an error that blows up on the way in.
///
/// Exits 1 if any check fails, so `bd preflight && bd ready` is a usable sentence.
pub async fn preflight(ctx: &Ctx) -> Result<()> {
    let mut findings = Vec::new();
    let now = Utc::now();

    match ctx.locator.as_ref() {
        Some(l) => findings.push(finding(
            "workspace",
            Verdict::Pass,
            format!("{} workspace at {}", l.backend, l.dir.display()),
        )),
        None => findings.push(finding(
            "workspace",
            Verdict::Fail,
            "no .beads directory here or above (run `bd init`)",
        )),
    }

    // Events and claims are stamped with the actor. "unknown" is the fallback
    // context.rs uses when nothing — flag, config, git — could say who you are,
    // and an audit trail full of `unknown` is an audit trail of nothing.
    if ctx.identity.actor.is_empty() || ctx.identity.actor == "unknown" {
        findings.push(finding(
            "actor",
            Verdict::Warn,
            "nobody knows who you are; set --actor, $BEADS_ACTOR, or git user.email",
        ));
    } else {
        findings.push(finding("actor", Verdict::Pass, ctx.identity.actor.clone()));
    }

    // Only ask for a store if there is a workspace to open one from. Opening is
    // lazy, so a missing workspace has cost nothing up to here.
    if ctx.locator.is_some() {
        match ctx.store().await {
            Err(e) => findings.push(finding("store", Verdict::Fail, format!("{e:#}"))),
            Ok(store) => {
                findings.push(finding(
                    "store",
                    Verdict::Pass,
                    format!("the {} store opens", store.backend()),
                ));

                match store.find_cycles().await {
                    Ok(c) if c.is_empty() => {
                        findings.push(finding("graph", Verdict::Pass, "acyclic"))
                    }
                    // A cycle is not cosmetic: every issue in it is blocked by
                    // itself, so the work in it can never become ready.
                    Ok(c) => findings.push(finding(
                        "graph",
                        Verdict::Fail,
                        format!("{} dependency cycle(s); `bd dep cycles` names them", c.len()),
                    )),
                    Err(e) => findings.push(finding(
                        "graph",
                        Verdict::Fail,
                        format!("cannot walk the graph: {e}"),
                    )),
                }

                match lapsed_leases(store, now).await {
                    Ok(l) if l.is_empty() => {
                        findings.push(finding("claims", Verdict::Pass, "no lapsed leases"))
                    }
                    Ok(l) => findings.push(finding(
                        "claims",
                        Verdict::Warn,
                        format!("{} lapsed lease(s) holding work hostage; run `bd reclaim`", l.len()),
                    )),
                    Err(e) => findings.push(finding(
                        "claims",
                        Verdict::Fail,
                        format!("cannot read claims: {e}"),
                    )),
                }

                match store.stats().await {
                    Ok(s) if s.ready == 0 => findings.push(finding(
                        "work",
                        Verdict::Warn,
                        "nothing is claimable right now (`bd blocked` may say why)",
                    )),
                    Ok(s) => findings.push(finding(
                        "work",
                        Verdict::Pass,
                        format!("{} issue(s) ready to claim", s.ready),
                    )),
                    Err(e) => findings.push(finding("work", Verdict::Fail, format!("{e}"))),
                }
            }
        }
    }

    let failed = findings.iter().any(|f| f.verdict == Verdict::Fail);

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "ok": !failed,
            "checks": findings.iter().map(|f| json!({
                "check": f.check,
                "status": f.verdict.as_str(),
                "detail": f.detail,
            })).collect::<Vec<_>>(),
        }))?;
    } else {
        for f in &findings {
            // Fixed-width verdict so the checks line up into a column you can
            // scan; the detail is where the information is.
            ctx.out.line(format!(
                "{:<5} {:<10} {}",
                f.verdict.as_str(),
                f.check,
                f.detail
            ));
        }
        if failed {
            ctx.out.line("\nnot ready to work");
        }
    }

    if failed {
        return Err(SilentExit(exit::FAILURE).into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sweeps: gc, prune, reclaim
// ---------------------------------------------------------------------------

/// Whether the garbage collector may reap this bead yet.
///
/// The TTL clock starts when the wisp was written, and [`WispType::ttl`] is the
/// only thing that knows how long each kind lives — an error wisp keeps a week
/// precisely so you can still read it after the fact, a ping keeps six hours.
///
/// [`WispType::ttl`]: bd_core::WispType::ttl
fn is_expired(i: &Issue, now: DateTime<Utc>) -> bool {
    i.wisp_type.is_some_and(|w| i.created_at + w.ttl() < now)
}

/// Ephemeral beads, sorted into what may be reaped and what may not.
struct Reapable {
    expired: Vec<String>,
    /// Ephemeral, but with no `wisp_type` — so **no declared TTL**, so nothing
    /// here can know when it dies. Inventing a default would delete somebody's
    /// bead on a schedule they never chose, so these are counted and reported and
    /// never touched.
    undated: u64,
}

async fn reapable(store: &dyn Storage, now: DateTime<Utc>) -> Result<Reapable> {
    let filter = IssueFilter {
        ephemeral: Some(true),
        ..IssueFilter::new()
    };
    let mut r = Reapable {
        expired: Vec::new(),
        undated: 0,
    };
    for i in store.list_issues(&filter).await? {
        match i.wisp_type {
            None => r.undated += 1,
            Some(_) if is_expired(&i, now) => r.expired.push(i.id),
            Some(_) => {}
        }
    }
    Ok(r)
}

/// Claims whose lease has lapsed, *without* touching them.
///
/// [`Storage::expire_claims`] is the only seam call that knows about lapsed
/// leases and it sweeps them, so a preview cannot use it. `IssueFilter` has no
/// lease predicate either, so the filter narrows to in-progress work and the
/// lease check happens here. In-progress work is small by construction — that is
/// what a claim *means* — so this is a short list, not a scan of the workspace.
///
/// The predicate mirrors the backend's exactly. If the two ever disagree,
/// `--readonly` starts previewing a different sweep than the one that runs.
async fn lapsed_leases(store: &dyn Storage, now: DateTime<Utc>) -> Result<Vec<String>> {
    let filter = IssueFilter {
        statuses: vec![Status::InProgress],
        ..IssueFilter::new()
    };
    Ok(store
        .list_issues(&filter)
        .await?
        .into_iter()
        .filter(|i| i.lease_expires_at.is_some_and(|e| e < now))
        .map(|i| i.id)
        .collect())
}

struct Swept {
    /// Reaped — or, under `--readonly`, reapable.
    reaped: Vec<String>,
    undated: u64,
    freed: Vec<String>,
    preview: bool,
}

/// `bd prune`: reap ephemeral beads whose TTL has lapsed.
///
/// No confirmation, and that is deliberate. This can only ever touch a row that
/// is *both* `ephemeral = 1` — a bead the system itself created as temporary,
/// which `bd ready` has never once shown to anyone — *and* past a TTL that the
/// bead's own type declared. It is structurally incapable of eating your work. A
/// garbage collector that will not collect garbage unless a human watches is a
/// garbage collector that never runs.
pub async fn prune(ctx: &Ctx, dry_run: bool) -> Result<()> {
    let swept = sweep(ctx, false, dry_run).await?;
    report_sweep(ctx, "prune", &swept)
}

/// `bd gc`: the wisp sweep *and* the lease sweep — "expired wisps, lapsed
/// leases", which is exactly what `bd gc --help` promises.
pub async fn gc(ctx: &Ctx, dry_run: bool) -> Result<()> {
    let swept = sweep(ctx, true, dry_run).await?;
    report_sweep(ctx, "gc", &swept)
}

async fn sweep(ctx: &Ctx, leases: bool, dry_run: bool) -> Result<Swept> {
    let store = ctx.store().await?;
    let now = Utc::now();
    let r = reapable(store, now).await?;

    // `--readonly` implies `--dry-run`; it is the global spelling of the same
    // request. Deliberately not `ensure_writable`, which *fails*: the caller
    // asked for no writes and got none, which is a success.
    if dry_run || ctx.readonly {
        return Ok(Swept {
            reaped: r.expired,
            undated: r.undated,
            freed: if leases {
                lapsed_leases(store, now).await?
            } else {
                Vec::new()
            },
            preview: true,
        });
    }

    for id in &r.expired {
        // A wisp can be the target of an edge. `delete_issue` cascades those and
        // recomputes the blocked cache, so the graph is never left pointing at a
        // bead that no longer exists.
        store.delete_issue(id).await?;
    }
    let freed = if leases {
        store.expire_claims().await?
    } else {
        Vec::new()
    };

    Ok(Swept {
        reaped: r.expired,
        undated: r.undated,
        freed,
        preview: false,
    })
}

fn report_sweep(ctx: &Ctx, cmd: &str, s: &Swept) -> Result<()> {
    if ctx.out.is_json() {
        // One shape for `gc` and `prune`, so an agent can parse either without
        // knowing which it ran. `prune` never frees leases; the key is still
        // there, empty.
        return ctx.out.json_value(&json!({
            "command": cmd,
            "dry_run": s.preview,
            "reaped": s.reaped,
            "reaped_count": s.reaped.len(),
            "leases_freed": s.freed,
            "leases_freed_count": s.freed.len(),
            "ephemeral_without_ttl": s.undated,
        }));
    }

    if s.reaped.is_empty() && s.freed.is_empty() {
        ctx.out.line("Nothing to collect.");
    }
    if !s.reaped.is_empty() {
        let verb = if s.preview { "Would reap" } else { "Reaped" };
        ctx.out
            .line(format!("{verb} {} expired wisp(s):", s.reaped.len()));
        for id in &s.reaped {
            ctx.out.line(format!("  {id}"));
        }
    }
    if !s.freed.is_empty() {
        let verb = if s.preview { "Would free" } else { "Freed" };
        ctx.out
            .line(format!("{verb} {} lapsed lease(s):", s.freed.len()));
        for id in &s.freed {
            ctx.out.line(format!("  {id}"));
        }
    }
    if s.preview {
        ctx.out.line("(dry run: nothing was written)");
    }
    if s.undated > 0 {
        ctx.out.warn(format!(
            "{} ephemeral bead(s) declare no wisp type, so they have no TTL and were left alone",
            s.undated
        ));
    }
    Ok(())
}

/// The reason claims are leases and not locks.
///
/// An agent that dies mid-task leaves an `in_progress` issue holding a lease
/// nobody will ever renew. Without this sweep that work is hostage forever: it is
/// not in `bd ready` (someone is "doing" it) and nobody is doing it.
pub async fn reclaim(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let now = Utc::now();

    let (freed, preview) = if ctx.readonly {
        (lapsed_leases(store, now).await?, true)
    } else {
        (store.expire_claims().await?, false)
    };

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "command": "reclaim",
            "dry_run": preview,
            "reclaimed": freed,
            "count": freed.len(),
        }));
    }
    if freed.is_empty() {
        ctx.out.line("No lapsed leases to reclaim.");
        return Ok(());
    }
    let verb = if preview { "Would reclaim" } else { "Reclaimed" };
    ctx.out
        .line(format!("{verb} {} issue(s) with lapsed leases:", freed.len()));
    for id in &freed {
        ctx.out.line(format!("  {id}"));
    }
    if preview {
        ctx.out.line("(--readonly: nothing was written)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// purge — the only command here that deletes real work
// ---------------------------------------------------------------------------

enum Answer {
    Yes,
    No,
    /// There is nobody at the other end of the prompt.
    NoOneToAsk,
}

/// Ask a human, if there is one.
///
/// `--json` and a non-terminal stdin both mean nobody will ever see the question.
/// Taking silence for consent there is how an agent deletes a year of closed work
/// because the `--yes` flag it passed does not exist.
fn ask(ctx: &Ctx, question: &str) -> Answer {
    if ctx.out.is_json() || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Answer::NoOneToAsk;
    }
    print!("{question} [y/N] ");
    if std::io::stdout().flush().is_err() {
        return Answer::NoOneToAsk;
    }
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Answer::NoOneToAsk;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Yes,
        _ => Answer::No,
    }
}

/// `bd purge`: hard-delete closed issues older than a threshold.
///
/// The one genuinely dangerous command in this file. What it deletes is real
/// work, and by construction it is work nobody has looked at in months — so if it
/// deletes the wrong thing, nobody will notice that either, for months.
pub async fn purge(ctx: &Ctx, older_than: Duration, dry_run: bool, yes: bool) -> Result<()> {
    purge_closed(ctx, "purge", Some(older_than), dry_run, yes).await
}

async fn purge_closed(
    ctx: &Ctx,
    cmd: &str,
    older_than: Option<Duration>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let store = ctx.store().await?;

    let filter = IssueFilter {
        status: Some(Status::Closed),
        // Strict `closed_at < cutoff`, and a NULL `closed_at` matches nothing —
        // so a closed issue that arrived by import without a close timestamp is
        // never purged by age. Conservative in the right direction.
        closed_before: older_than.map(|d| Utc::now() - d),
        // `pinned` is the workspace's "do not delete" marker. A purge that
        // ignored it would eat the one thing somebody went out of their way to
        // protect.
        pinned: Some(false),
        ..IssueFilter::new()
    };
    let doomed: Vec<String> = store
        .list_issues(&filter)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();

    if doomed.is_empty() {
        if ctx.out.is_json() {
            return ctx.out.json_value(&json!({
                "command": cmd,
                "dry_run": false,
                "deleted": [],
                "count": 0,
            }));
        }
        ctx.out.line("Nothing to purge.");
        return Ok(());
    }

    // A preview, and an honest exit 0. Nothing was asked for and nothing was done.
    if dry_run || ctx.readonly {
        return report_purge(ctx, cmd, &doomed, true);
    }

    // `--yes` is consent in writing, and it is the *only* way a script can give
    // it: `ask` correctly refuses to read silence on a pipe as agreement. Without
    // it there is no path on which a scripted purge succeeds, which does not make
    // the command safe — it makes it useless, and a destructive command nobody
    // can run is one people work around.
    if !yes {
        match ask(
            ctx,
            &format!("Permanently delete {} closed issue(s)?", doomed.len()),
        ) {
            Answer::Yes => {}
            // Refusing is exit 1 with the list attached, deliberately loud.
            Answer::No => return refuse(ctx, cmd, &doomed, "aborted"),
            Answer::NoOneToAsk => return refuse(ctx, cmd, &doomed, "no_confirmation"),
        }
    }

    for id in &doomed {
        store.delete_issue(id).await?;
    }
    report_purge(ctx, cmd, &doomed, false)
}

fn report_purge(ctx: &Ctx, cmd: &str, ids: &[String], preview: bool) -> Result<()> {
    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "command": cmd,
            "dry_run": preview,
            "deleted": if preview { Vec::new() } else { ids.to_vec() },
            "would_delete": if preview { ids.to_vec() } else { Vec::new() },
            "count": ids.len(),
        }));
    }
    let verb = if preview { "Would delete" } else { "Deleted" };
    ctx.out.line(format!("{verb} {} closed issue(s):", ids.len()));
    for id in ids {
        ctx.out.line(format!("  {id}"));
    }
    if preview {
        ctx.out.line("(dry run: nothing was written)");
    }
    Ok(())
}

fn refuse(ctx: &Ctx, cmd: &str, ids: &[String], reason: &str) -> Result<()> {
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "error": "needs_confirmation",
            "reason": reason,
            "command": cmd,
            "count": ids.len(),
            "would_delete": ids,
            "hint": "pass --yes to consent, or --dry-run to preview",
        }))?;
    } else {
        eprintln!("`bd {cmd}` would permanently delete {} closed issue(s):", ids.len());
        for id in ids {
            eprintln!("  {id}");
        }
        eprintln!(
            "Nothing was deleted. Answer y at the prompt, pass --yes to consent in writing, \
             or --dry-run to preview."
        );
    }
    Err(SilentExit(exit::FAILURE).into())
}

// ---------------------------------------------------------------------------
// A real "no": exit 2
// ---------------------------------------------------------------------------

/// Upstream's `bd backup` is a **Dolt** backup, not a file copy: `backup init
/// <path>` registers a backup remote (a directory, or a DoltHub URL), `backup
/// sync` pushes to it, and what it preserves is the *database* — tables,
/// branches, commit history, working set. That is [`Cap::Remote`] exactly, so on
/// a store with no commit graph the honest answer is a final "no" (exit 2), not
/// "not built yet".
///
/// A file-level snapshot of a SQLite workspace would be a *different feature*
/// wearing this one's name, and it should not be smuggled in under it. If we ever
/// want one it needs `Storage::backup_to(&self, dest: &Path) -> Result<()>`
/// (sqlite: `VACUUM INTO`, which is atomic and WAL-safe — plain file copies of a
/// live WAL database are not).
pub async fn backup(ctx: &Ctx, cmd: BackupCmd) -> Result<()> {
    let name = match cmd {
        BackupCmd::Status => "backup status",
        BackupCmd::Init { .. } => "backup init",
        BackupCmd::Sync => "backup sync",
        BackupCmd::Remove => "backup remove",
        BackupCmd::Restore { .. } => "backup restore",
    };

    let cap = require_cap(ctx, name, Cap::Remote);
    if cap.is_err() && !ctx.out.is_json() {
        // The capability message can only say "no". This says what to do instead.
        eprintln!(
            "`bd export` writes every issue as JSONL and `bd import` reads it back — that is the backup a store without a commit graph can give you."
        );
    }
    cap?;

    // Reachable only on a backend that *does* have a commit graph, where the work
    // is real and simply not done.
    stub(name, ctx)
}

// ---------------------------------------------------------------------------
// Not built yet: exit 64
// ---------------------------------------------------------------------------

/// Not exit 2, tempting as it is.
///
/// SQLite compacts with `VACUUM`; this is an operation the backend can perform
/// and the seam does not expose. Saying "the sqlite backend cannot compact" would
/// be false, and exit 2 is meant to be a true, final answer that nobody revisits.
///
/// Upstream's `compact` is a bigger thing again — semantic summarization of old
/// closed issues, with an LLM and a tier system — and `bd compact --dolt` is
/// where `DOLT_GC()` lives. This port's `cli.rs` gives it no flags at all, so
/// what it can honestly mean here is "reclaim space in the store".
///
/// Needs: `Storage::compact(&self) -> Result<CompactStats>`.
pub async fn compact(ctx: &Ctx) -> Result<()> {
    stub("compact", ctx)
}

/// Also not exit 2, and for a sharper reason: there is no schema *version*
/// anywhere in this port. bd-sqlite applies `schema.sql` once, entirely in
/// `CREATE TABLE IF NOT EXISTS`, and records no version anywhere — so `migrate`
/// has nothing to compare the database against. That is a gap in the port, not a
/// limit of the backend.
///
/// Needs, in order: a recorded schema version (`Storage::schema_version(&self) ->
/// Result<u32>`), then `Storage::migrate(&self) -> Result<u32>` returning the
/// version it arrived at.
pub async fn migrate(ctx: &Ctx) -> Result<()> {
    stub("migrate", ctx)
}

/// Rewriting every id in the workspace.
///
/// The seam has no way to change an issue's id, and this cannot be faked from
/// above it: `delete_issue` + `create_issue` would drop the issue's events,
/// comments and edges (`ON DELETE CASCADE`), which is not a rename — it is data
/// loss with a rename's name on it. The id is a foreign key in four other tables
/// and the rewrite has to be one transaction.
///
/// Needs: `Storage::rename_prefix(&self, from: &str, to: &str) -> Result<u64>`.
pub async fn rename_prefix(ctx: &Ctx, _from: &str, _to: &str) -> Result<()> {
    stub("rename-prefix", ctx)
}

pub async fn admin(ctx: &Ctx, cmd: AdminCmd) -> Result<()> {
    match cmd {
        // The same engine as `bd purge`, minus the age bound: upstream's
        // `admin cleanup` deletes *every* closed issue by default. Same
        // confirmation gate, for the same reason, and more of it — and no `--yes`
        // of its own, because `cli.rs` gives it none. Deleting every closed issue
        // in the workspace is not something to make scriptable by accident.
        AdminCmd::Cleanup => purge_closed(ctx, "admin cleanup", None, false, false).await,
        // The same missing seam method as `bd compact`; see there.
        AdminCmd::Compact => stub("admin compact", ctx),
        // Throwing the database away. This needs no seam method — it is a
        // directory to delete — but it does need a `--yes` flag, and `cli.rs`
        // gives it none. A prompt is not enough safety for the one command that
        // can destroy the entire workspace, and there is no way for anyone to
        // pass consent in writing. So it stays unbuilt until the flag exists.
        AdminCmd::Reset => stub("admin reset", ctx),
    }
}

/// Git worktrees: `bd worktree create` cuts a git worktree and decides how it
/// shares the `.beads` database with its parent.
///
/// Nothing here touches the storage seam — this is a *git* subsystem, and the
/// port has no git module at all (`context.rs` shells out for `user.email` and
/// that is the whole of it). Not blocked on the seam; blocked on that module.
pub async fn worktree(ctx: &Ctx, cmd: WorktreeCmd) -> Result<()> {
    let name = match cmd {
        WorktreeCmd::Create { .. } => "worktree create",
        WorktreeCmd::List => "worktree list",
        WorktreeCmd::Remove { .. } => "worktree remove",
        WorktreeCmd::Info => "worktree info",
    };
    stub(name, ctx)
}

/// A distributed lock, spelled as a bead: `<prefix>-merge-slot`, held by whoever
/// is resolving conflicts, with a priority-ordered queue of waiters in its
/// metadata.
///
/// `claim_issue` already gives the mutual-exclusion half — it is a real
/// compare-and-swap on the lease, and it fails with `AlreadyClaimed` — but it
/// gives nothing for the *queue*. Maintaining `metadata.waiters` means
/// `get_issue` then `update_issue`, and two agents racing through that pair lose
/// one waiter every time. A lock whose queue silently drops entrants is worse
/// than no lock: it hands the slot to two polecats at once, which is the exact
/// "monkey knife fight" the merge slot exists to prevent.
///
/// Needs an atomic read-modify-write on metadata, e.g.
/// `Storage::swap_metadata(&self, id: &str, expected: Option<&serde_json::Value>,
/// new: &serde_json::Value) -> Result<bool>` — false when the row moved under us.
pub async fn merge_slot(ctx: &Ctx, cmd: MergeSlotCmd) -> Result<()> {
    let name = match cmd {
        MergeSlotCmd::Create { .. } => "merge-slot create",
        MergeSlotCmd::Check => "merge-slot check",
        MergeSlotCmd::Acquire { .. } => "merge-slot acquire",
        MergeSlotCmd::Release { .. } => "merge-slot release",
    };
    stub(name, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bd_core::WispType;

    fn wisp(kind: WispType, age: Duration) -> Issue {
        Issue {
            wisp_type: Some(kind),
            ephemeral: true,
            created_at: Utc::now() - age,
            ..Issue::new("t-1", "a wisp")
        }
    }

    #[test]
    fn a_wisp_dies_on_its_own_types_schedule() {
        let now = Utc::now();
        // A ping keeps 6h; an error keeps 7d. Reaping the error on the ping's
        // clock would delete the forensics right when someone came looking.
        assert!(is_expired(&wisp(WispType::Ping, Duration::hours(7)), now));
        assert!(!is_expired(&wisp(WispType::Error, Duration::hours(7)), now));
        assert!(is_expired(&wisp(WispType::Error, Duration::days(8)), now));
    }

    #[test]
    fn an_ephemeral_bead_with_no_type_has_no_ttl_and_is_never_reaped() {
        let mut i = wisp(WispType::Ping, Duration::days(365));
        i.wisp_type = None;
        // No declared TTL means nothing can know when it dies. Not "reap it
        // anyway" — that would be inventing a policy on somebody else's data.
        assert!(!is_expired(&i, Utc::now()));
    }
}
