//! Moving issues in and out, and the commands that need a commit graph.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};

use anyhow::{Context as _, Result, anyhow, bail};
use bd_core::{Comment, Dependency, Issue, IssueFilter, Status};
use bd_storage::{Field, IssuePatch};
use serde_json::{Value, json};


use crate::cli::{
    DoltCmd, DoltRemoteCmd, ExportArgs, FederationCmd, ImportArgs, RepoCmd, TrackerCmd, VcCmd,
};
use crate::commands::{Cap, require_cap, stub};
use crate::integrations;
use crate::context::Ctx;
use crate::exit::SilentExit;
use crate::output::export_record;

// ---------------------------------------------------------------------------
// Export / import
// ---------------------------------------------------------------------------

pub async fn export(ctx: &Ctx, a: ExportArgs) -> Result<()> {
    let store = ctx.store().await?;

    let mut f = IssueFilter::new();
    if a.open_only {
        f.statuses = vec![Status::Open, Status::InProgress];
    }
    let mut issues = store.list_issues(&f).await?;

    // `list_issues` returns columns, not relations: labels, edges, and comments
    // all come back empty. That is the right trade for a listing and the wrong
    // one for a backup, so they get hydrated here — an export that loses labels
    // is not a backup.
    //
    // Three batched queries for the whole export, not three per issue. Each of
    // these getters omits ids that carry nothing, so a miss is ordinary rather
    // than an error.
    let ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
    let mut labels: HashMap<String, Vec<String>> =
        store.labels_of(&ids).await?.into_iter().collect();
    let mut deps: HashMap<String, Vec<Dependency>> =
        store.dependencies_of_many(&ids).await?.into_iter().collect();
    let mut comments: HashMap<String, Vec<Comment>> =
        store.comments_of_many(&ids).await?.into_iter().collect();

    let mut w: Box<dyn Write> = match &a.output {
        Some(p) => Box::new(BufWriter::new(
            File::create(p).with_context(|| format!("cannot write {}", p.display()))?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout().lock())),
    };

    for issue in &mut issues {
        issue.labels = labels.remove(&issue.id).unwrap_or_default();
        issue.dependencies = deps.remove(&issue.id).unwrap_or_default();
        issue.comments = comments.remove(&issue.id).unwrap_or_default();
        writeln!(w, "{}", serde_json::to_string(&export_record(issue)?)?)?;
    }
    w.flush()?;

    if let Some(p) = &a.output {
        ctx.out.line(format!("Exported to {}", p.display()));
    }
    Ok(())
}

pub async fn import(ctx: &Ctx, a: ImportArgs) -> Result<()> {
    if !a.dry_run {
        ctx.ensure_writable("import")?;
    }
    let store = ctx.store().await?;

    let reader: Box<dyn BufRead> = match a.file.as_deref() {
        Some(p) if p != std::path::Path::new("-") => Box::new(BufReader::new(
            File::open(p).with_context(|| format!("cannot read {}", p.display()))?,
        )),
        _ => Box::new(BufReader::new(std::io::stdin().lock())),
    };

    let mut created = 0u64;
    let mut updated = 0u64;
    let mut skipped = 0u64;
    let mut edges = 0u64;
    // Two passes. Edges and comments alike can name an issue that appears later
    // in the file, and applying them as we go would fail on a perfectly valid
    // export.
    let mut pending_edges: Vec<Dependency> = Vec::new();
    let mut pending_comments: Vec<Comment> = Vec::new();

    for (n, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line)
            .with_context(|| format!("line {}: not valid JSON", n + 1))?;

        match v.get("_type").and_then(|t| t.as_str()) {
            // A record with no discriminator is an issue: that is what every
            // older export emitted.
            None | Some("issue") => {}
            Some(other) => {
                ctx.out.detail(format!("line {}: skipping _type={other}", n + 1));
                skipped += 1;
                continue;
            }
        }

        let issue: Issue = serde_json::from_value(v)
            .with_context(|| format!("line {}: not an issue", n + 1))?;
        // Permissive on purpose: these records come from a peer we trust, and
        // their custom type is not our typo.
        issue.validate_for_import()?;

        pending_edges.extend(issue.dependencies.iter().cloned());
        pending_comments.extend(issue.comments.iter().cloned());

        if a.dry_run {
            if store.get_issue(&issue.id).await?.is_some() {
                updated += 1;
            } else {
                created += 1;
            }
            continue;
        }

        if store.get_issue(&issue.id).await?.is_some() {
            store.update_issue(&issue.id, &patch_from(&issue)).await?;
            updated += 1;
        } else {
            store.create_issue(&issue).await?;
            created += 1;
        }
        for l in &issue.labels {
            store.add_label(&issue.id, l).await?;
        }
    }

    let comments = pending_comments.len() as u64;

    if !a.dry_run {
        for d in &pending_edges {
            match store.add_dependency(d).await {
                Ok(()) => edges += 1,
                // Re-importing the same file must be a no-op, not a failure.
                Err(bd_storage::Error::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        for c in &pending_comments {
            // `upsert_comment`, never `add_comment`: the latter mints a fresh id
            // and stamps *the importer* as the author, so re-importing a file
            // would duplicate every comment and misattribute all of them. Keying
            // on the incoming id keeps import idempotent and the original author
            // intact.
            store.upsert_comment(c).await.with_context(|| {
                format!("cannot import comment {} on issue {}", c.id, c.issue_id)
            })?;
        }
        // A bulk upsert lands closures and edges that no single write path saw
        // in order. The blocked cache is a fixpoint over the whole graph, so it
        // has to be recomputed once at the end or `bd ready` is quietly wrong.
        store.recompute_blocked().await?;
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "created": created,
            "updated": updated,
            "skipped": skipped,
            "dependencies": edges,
            "comments": comments,
            "dry_run": a.dry_run,
        }))?;
    } else {
        let prefix = if a.dry_run { "Would import" } else { "Imported" };
        ctx.out.line(format!(
            "{prefix}: {created} created, {updated} updated, {skipped} skipped, \
             {edges} edge(s), {comments} comment(s)"
        ));
    }
    Ok(())
}

/// Everything an imported record can set on an existing issue.
///
/// The record is **authoritative**: it carries the issue's whole state, so a
/// field it does not carry is a field the issue does not have, and gets cleared.
/// Using the `Option -> Keep` conversion here instead would mean an issue whose
/// due date was removed upstream keeps its old one forever, because no later
/// import would ever say "clear it" either.
fn patch_from(i: &Issue) -> IssuePatch {
    IssuePatch {
        title: Some(i.title.clone()),
        description: Field::Set(i.description.clone()),
        design: Field::Set(i.design.clone()),
        acceptance_criteria: Field::Set(i.acceptance_criteria.clone()),
        notes: Field::Set(i.notes.clone()),
        status: Some(i.status.clone()),
        priority: Some(i.priority),
        issue_type: Some(i.issue_type.clone()),
        assignee: Field::Set(i.assignee.clone()),
        estimated_minutes: Field::authoritative(i.estimated_minutes),
        due_at: Field::authoritative(i.due_at),
        defer_until: Field::authoritative(i.defer_until),
        close_reason: Field::Set(i.close_reason.clone()),
        metadata: Field::authoritative(i.metadata.clone()),
        spec_id: Field::Set(i.spec_id.clone()),
        // Both halves of the tracker join key, and both authoritative. An import
        // that restored `external_ref` but dropped `source_system` would leave a
        // bead no tracker recognizes — and the next sync would file a duplicate
        // of it upstream. An empty `source_system` in the record is a real value
        // ("no tracker owns this"), so it is written, not skipped.
        external_ref: Field::authoritative(i.external_ref.clone()),
        source_system: Field::Set(i.source_system.clone()),
        pinned: Some(i.pinned),
        // A record that no longer says `ephemeral` is a record of a bead that was
        // promoted upstream. Keeping the old flag would leave a bead the peer
        // considers real invisible to `bd ready` here, and still on gc's list.
        ephemeral: Some(i.ephemeral),
        wisp_type: Field::authoritative(i.wisp_type),
    }
}

// ---------------------------------------------------------------------------
// Commands that need a commit graph
// ---------------------------------------------------------------------------

/// With a name it switches branches, without one it lists them. Either way it
/// wants a commit graph, so the capability check comes first — on sqlite this
/// is exit 2 (an honest no), never exit 64.
pub async fn branch(ctx: &Ctx, name: Option<String>) -> Result<()> {
    require_cap(ctx, "branch", Cap::VersionControl)?;
    let store = ctx.store().await?;
    // `require_cap` already established this, but it did so from the *locator*,
    // which is deliberately allowed to answer without opening a database. Now
    // that one is open, the store is the authority.
    let vc = store
        .version_control()
        .ok_or_else(|| anyhow!("this backend has no commit graph"))?;

    match name {
        // With a name: switch to it, creating it if it does not exist. That is
        // what every user means by `bd branch feature-x`, and making them run a
        // separate create step first is a ceremony git itself abandoned.
        Some(name) => {
            ctx.ensure_writable("switch branches")?;
            let existing = vc.list_branches().await?;
            let created = !existing.contains(&name);
            if created {
                vc.create_branch(&name).await?;
            }
            vc.checkout(&name).await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({ "branch": name, "created": created }))?;
            } else if created {
                ctx.out.line(format!("created and switched to branch {name}"));
            } else {
                ctx.out.line(format!("switched to branch {name}"));
            }
        }
        None => {
            let current = vc.current_branch().await?;
            let branches = vc.list_branches().await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({
                    "current": current,
                    "branches": branches,
                }))?;
            } else {
                for b in &branches {
                    let mark = if *b == current { "*" } else { " " };
                    ctx.out.line(format!("{mark} {b}"));
                }
            }
        }
    }
    Ok(())
}

pub async fn vc(ctx: &Ctx, cmd: VcCmd) -> Result<()> {
    let name = match cmd {
        VcCmd::Merge { .. } => "vc merge",
        VcCmd::Commit { .. } => "vc commit",
        VcCmd::Status => "vc status",
    };
    require_cap(ctx, name, Cap::VersionControl)?;
    stub(name, ctx)
}

pub async fn dolt(ctx: &Ctx, cmd: DoltCmd) -> Result<()> {
    // Every `bd dolt` subcommand presupposes the dolt backend, so the honest
    // answer on a sqlite workspace is the capability message, not "unbuilt".
    let (name, cap) = match &cmd {
        DoltCmd::Show => ("dolt show", Cap::VersionControl),
        DoltCmd::Set { .. } => ("dolt set", Cap::VersionControl),
        DoltCmd::Test => ("dolt test", Cap::VersionControl),
        DoltCmd::Commit { .. } => ("dolt commit", Cap::VersionControl),
        DoltCmd::Push { .. } => ("dolt push", Cap::Remote),
        DoltCmd::Pull { .. } => ("dolt pull", Cap::Remote),
        DoltCmd::Start => ("dolt start", Cap::VersionControl),
        DoltCmd::Stop => ("dolt stop", Cap::VersionControl),
        DoltCmd::Status => ("dolt status", Cap::VersionControl),
        DoltCmd::Killall => ("dolt killall", Cap::VersionControl),
        DoltCmd::CleanDatabases => ("dolt clean-databases", Cap::VersionControl),
        DoltCmd::Remote { cmd } => match cmd {
            DoltRemoteCmd::Add { .. } => ("dolt remote add", Cap::Remote),
            DoltRemoteCmd::List => ("dolt remote list", Cap::Remote),
            DoltRemoteCmd::Remove { .. } => ("dolt remote remove", Cap::Remote),
        },
    };
    require_cap(ctx, name, cap)?;
    stub(name, ctx)
}

/// Peer-to-peer sync: every peer keeps its own database and they exchange
/// updates over remotes.
///
/// That is a remote capability, not a missing feature. A backend with no commit
/// graph has nothing to push and nothing to pull, so on sqlite this is exit 2 —
/// a final answer — and never exit 64, which would advertise it as work someone
/// still owes you. (Upstream reaches the same conclusion from the other side:
/// its federation commands require the Dolt backend.)
pub async fn federation(ctx: &Ctx, cmd: FederationCmd) -> Result<()> {
    let name = match cmd {
        FederationCmd::Sync => "federation sync",
        FederationCmd::Status => "federation status",
        FederationCmd::AddPeer { .. } => "federation add-peer",
        FederationCmd::RemovePeer { .. } => "federation remove-peer",
        FederationCmd::ListPeers => "federation list-peers",
    };
    require_cap(ctx, name, Cap::Remote)?;
    stub(name, ctx)
}

// ---------------------------------------------------------------------------
// Mail — a shim, not a feature
// ---------------------------------------------------------------------------

/// Env vars that name the mail provider, in the order upstream checks them.
const MAIL_DELEGATE_ENV: [&str; 2] = ["BEADS_MAIL_DELEGATE", "BD_MAIL_DELEGATE"];
const MAIL_DELEGATE_KEY: &str = "mail.delegate";

/// `bd mail` delegates to whatever actually handles mail here.
///
/// Beads has no mailbox. Mail belongs to the orchestrator — but agents working
/// in beads reach for `bd mail` anyway, so upstream bridges the gap by shelling
/// out to a configured provider (`gt mail`, typically), and so do we.
///
/// The provider owns stdout, including under `--json`: a shim that reformatted
/// its child's output would be lying about where the answer came from.
pub async fn mail(ctx: &Ctx, id: Option<String>) -> Result<()> {
    // Environment first, so a session can override a setting that is committed
    // to the workspace. Only then open the store — an unset env var is the
    // common case, but a store we never needed is a cost we never pay.
    let delegate = match MAIL_DELEGATE_ENV
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    {
        Some(v) => v,
        None => ctx
            .store()
            .await?
            .get_config(MAIL_DELEGATE_KEY)
            .await?
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no mail provider configured.\n\
                     Set one with `bd config set {MAIL_DELEGATE_KEY} \"gt mail\"`, \
                     or export {}.",
                    MAIL_DELEGATE_ENV[0]
                )
            })?,
    };

    // Whitespace splitting, not a shell: the provider is a command line the user
    // configured (`gt mail`), and handing it to a shell would make quoting and
    // metacharacters in an argument someone else's problem.
    let mut words = delegate.split_whitespace();
    let program = words
        .next()
        .ok_or_else(|| anyhow::anyhow!("{MAIL_DELEGATE_KEY} is empty"))?;

    let mut cmd = std::process::Command::new(program);
    cmd.args(words);
    cmd.args(id.as_deref());

    // Blocking on purpose: stdio is inherited, so the child owns the terminal
    // for as long as it runs, and we have nothing to do until it is done.
    let status = cmd
        .status()
        .with_context(|| format!("cannot run the mail provider `{delegate}`"))?;

    match status.code() {
        Some(0) => Ok(()),
        // The provider already said whatever it had to say, on its own stdio.
        // Carry its verdict out to the shell rather than flattening every
        // failure to 1 — a caller distinguishing them is the point of the shim.
        Some(code) => Err(SilentExit(code).into()),
        None => bail!("the mail provider `{delegate}` was killed by a signal"),
    }
}

// ---------------------------------------------------------------------------
// Registered, not ported
// ---------------------------------------------------------------------------

/// Every external tracker gets the same four verbs, so they get one handler.
/// The trackers themselves land in `integrations/`, one file each.
/// `bd jira sync`, `bd linear pull`, and the rest — one entry point for all six.
///
/// The tracker itself comes from the registry, and the HTTP client is injected,
/// so nothing here (or in any tracker) hard-codes a network call. That is what
/// makes the integrations testable without credentials.
pub async fn tracker(ctx: &Ctx, name: &str, cmd: TrackerCmd) -> Result<()> {
    let verb = match cmd {
        TrackerCmd::Sync => "sync",
        TrackerCmd::Status => "status",
        TrackerCmd::Push => "push",
        TrackerCmd::Pull => "pull",
    };

    let Some(t) = integrations::get(name) else {
        return stub(&format!("{name} {verb}"), ctx);
    };

    // `status` is the one verb that must work when the tracker is NOT set up --
    // it exists precisely to tell you what is missing. Asking for a token first
    // would make it useless for the only case anyone runs it in.
    if matches!(cmd, TrackerCmd::Status) {
        let st = t.status(ctx).await?;
        if ctx.out.is_json() {
            ctx.out.json_value(&serde_json::to_value(&st)?)?;
        } else if st.configured {
            ctx.out.line(format!("{name}: configured"));
            if let Some(d) = &st.detail {
                ctx.out.line(format!("  {d}"));
            }
        } else {
            ctx.out.line(format!("{name}: not configured"));
            for k in &st.missing {
                ctx.out.line(format!("  missing: {k}"));
            }
            ctx.out
                .line(format!("  token is read from ${}", t.secret_env()));
        }
        return Ok(());
    }

    // Every remaining verb writes the **local database**, and that includes
    // `pull`. Pull is read-only against the *remote*; the entire result of it is
    // written here. `bd jira pull --readonly` used to be exempt on the reasoning
    // that "pull is read-only", and it happily rewrote the workspace.
    //
    // Checked here and nowhere else. The guard belongs in front of the dispatch,
    // not inside six trackers that each have to remember it.
    ctx.ensure_writable(&format!("{name} {verb}"))?;

    // A tracker that is not yet built says so with exit 64, exactly like any
    // other unported command. It must not look like a configuration problem.
    let st = t.status(ctx).await?;
    if st.detail.as_deref() == Some("not implemented yet") {
        return stub(&format!("{name} {verb}"), ctx);
    }
    if !st.configured {
        bail!(
            "{name} is not configured (missing: {}). Set them with `bd config set`, \
             and put the token in ${} — never in .beads/config.yaml, which is committed.",
            st.missing.join(", "),
            t.secret_env()
        );
    }

    let http = integrations::http::RealHttp::new()?;
    let report = match cmd {
        TrackerCmd::Pull => t.pull(ctx, &http).await?,
        TrackerCmd::Push => t.push(ctx, &http).await?,
        TrackerCmd::Sync => t.sync(ctx, &http).await?,
        TrackerCmd::Status => unreachable!("handled above"),
    };

    // A pull can land issues and edges that no local write path saw in order, so
    // the blocked cache is stale by definition until a full recompute. Same
    // reason `import` does it.
    if matches!(cmd, TrackerCmd::Pull | TrackerCmd::Sync) {
        ctx.store().await?.recompute_blocked().await?;
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&serde_json::to_value(&report)?)?;
    } else {
        ctx.out.line(format!(
            "{name} {verb}: {} pulled ({} created, {} updated), {} pushed",
            report.pulled, report.created, report.updated, report.pushed
        ));
        for s in &report.skipped {
            ctx.out.warn(format!("skipped: {s}"));
        }
    }
    Ok(())
}

/// Multi-repo hydration: pull issues from sibling beads workspaces into this
/// database so one query spans all of them.
///
/// Stubbed because two things it needs do not exist yet, and faking either would
/// produce a command that succeeds while doing nothing:
///
/// 1. **A place to keep the list.** Upstream stores it in `.beads/config.yaml`
///    under `repos:`. Our [`Config`](crate::context::Config) has no such field,
///    and `Config::save` round-trips through the struct — so a `repos:` key
///    written behind its back would survive exactly until the next `bd config`
///    write silently dropped it. A registry that quietly deletes itself is worse
///    than no registry.
/// 2. **A second store.** `sync` has to read another workspace's database, and
///    the seam hands out exactly one store, chosen by this workspace's locator.
///    Opening a second one from here would mean naming a concrete backend
///    outside the single place allowed to (storage rule 1).
///
/// Both are one-liners for whoever owns those files; neither is ours.
pub async fn repo(ctx: &Ctx, cmd: RepoCmd) -> Result<()> {
    let name = match cmd {
        RepoCmd::Add { .. } => "repo add",
        RepoCmd::Remove { .. } => "repo remove",
        RepoCmd::List => "repo list",
        RepoCmd::Sync => "repo sync",
    };
    stub(name, ctx)
}

/// Publish a capability so other projects can depend on it: find the issue
/// labelled `export:<capability>`, check it is closed, and label it
/// `provides:<capability>`.
///
/// The `provides:` label is a *promise to other repos*, so the closed check is
/// the whole command. Shipping a capability whose work is still open advertises
/// something that does not exist yet, and the projects that believed you find
/// out at their own build time. `--force` exists because there are real reasons
/// to make that promise early — but it must be said out loud, not defaulted to.
pub async fn ship(ctx: &Ctx, capability: &str, force: bool, dry_run: bool) -> Result<()> {
    if !dry_run {
        ctx.ensure_writable("ship a capability")?;
    }
    let store = ctx.store().await?;

    let capability = capability.trim();
    if capability.is_empty() {
        bail!("a capability needs a name");
    }
    let export = format!("export:{capability}");
    let provides = format!("provides:{capability}");

    // Pushed down: `labels_all` is a subquery on the labels table, not a scan.
    let issues = store
        .list_issues(&IssueFilter {
            labels_all: vec![export.clone()],
            ..IssueFilter::new()
        })
        .await?;

    if issues.is_empty() {
        bail!(
            "nothing is labelled `{export}`, so there is no work to ship.\n\
             Label the issue that delivers it: `bd label add <id> {export}`"
        );
    }

    let open: Vec<&Issue> = issues.iter().filter(|i| !i.status.is_closed()).collect();
    if !open.is_empty() && !force {
        let ids: Vec<&str> = open.iter().map(|i| i.id.as_str()).collect();
        bail!(
            "`{capability}` is not finished: {} is still open.\n\
             A `provides:` label is a promise other repos build against — close the work, \
             or pass --force to make the promise anyway.",
            ids.join(", ")
        );
    }

    if !dry_run {
        for i in &issues {
            store.add_label(&i.id, &provides).await?;
        }
    }

    let ids: Vec<&str> = issues.iter().map(|i| i.id.as_str()).collect();
    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "capability": capability,
            "label": provides,
            "issues": ids,
            "unfinished": open.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            "forced": force && !open.is_empty(),
            "dry_run": dry_run,
        }));
    }
    let verb = if dry_run { "Would ship" } else { "Shipped" };
    ctx.out
        .line(format!("{verb} `{capability}` ({}): {}", provides, ids.join(", ")));
    if !open.is_empty() {
        ctx.out.warn(format!(
            "{} issue(s) are still open; `{capability}` was published anyway (--force)",
            open.len()
        ));
    }
    Ok(())
}
