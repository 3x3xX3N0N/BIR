//! Moving issues in and out, and the commands that need a commit graph.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};

use anyhow::{Context as _, Result};
use bd_core::{Dependency, Issue, IssueFilter, Status};
use bd_storage::IssuePatch;
use serde_json::{Value, json};

use crate::cli::{DoltCmd, DoltRemoteCmd, ExportArgs, ImportArgs, TrackerCmd, VcCmd};
use crate::commands::{Cap, require_cap, stub};
use crate::context::Ctx;
use crate::output::export_record;

// ---------------------------------------------------------------------------
// Export / import
// ---------------------------------------------------------------------------

pub async fn export(ctx: &Ctx, a: ExportArgs) -> Result<()> {
    let store = ctx.store()?;

    let mut f = IssueFilter::new();
    if a.open_only {
        f.statuses = vec![Status::Open, Status::InProgress];
    }
    let issues = store.list_issues(&f).await?;

    let mut w: Box<dyn Write> = match &a.output {
        Some(p) => Box::new(BufWriter::new(
            File::create(p).with_context(|| format!("cannot write {}", p.display()))?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout().lock())),
    };

    for issue in issues {
        // `list_issues` returns columns, not relations: labels, edges, and
        // comments come back empty. That is the right trade for a listing and
        // the wrong one for a backup, so re-read each issue in full here.
        // `get_issue` is the only way to see an issue's labels — the seam has no
        // per-issue label getter — and an export that loses labels is not a
        // backup. One query per issue is a fine price for that.
        let mut issue = store.get_issue(&issue.id).await?.unwrap_or(issue);
        if issue.dependencies.is_empty() {
            issue.dependencies = store.dependencies_of(&issue.id).await?;
        }
        if issue.comments.is_empty() {
            issue.comments = store.list_comments(&issue.id).await?;
        }
        writeln!(w, "{}", serde_json::to_string(&export_record(&issue)?)?)?;
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
    let store = ctx.store()?;

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
    let mut dropped_comments = 0u64;
    // Two passes: edges can point forward, at an issue that appears later in the
    // file. Adding them as we go would fail on a perfectly valid export.
    let mut pending: Vec<Dependency> = Vec::new();

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

        pending.extend(issue.dependencies.iter().cloned());
        // Comments are *not* restored. `add_comment` mints a fresh id and
        // attributes the comment to whoever is importing, so re-running an
        // import would duplicate every comment and relabel its author. The seam
        // has no comment upsert; until it does, idempotency wins — but say so
        // out loud rather than dropping data in silence.
        dropped_comments += issue.comments.len() as u64;

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

    if !a.dry_run {
        for d in &pending {
            match store.add_dependency(d).await {
                Ok(()) => edges += 1,
                // Re-importing the same file must be a no-op, not a failure.
                Err(bd_storage::Error::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
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
            "comments_not_imported": dropped_comments,
            "dry_run": a.dry_run,
        }))?;
    } else {
        let prefix = if a.dry_run { "Would import" } else { "Imported" };
        ctx.out.line(format!(
            "{prefix}: {created} created, {updated} updated, {skipped} skipped, {edges} edge(s)"
        ));
        if dropped_comments > 0 {
            ctx.out.warn(format!(
                "{dropped_comments} comment(s) were not imported (see PORT_STATUS.md: import has no comment upsert yet)"
            ));
        }
    }
    Ok(())
}

/// Everything an imported record can set on an existing issue.
fn patch_from(i: &Issue) -> IssuePatch {
    IssuePatch {
        title: Some(i.title.clone()),
        description: Some(i.description.clone()),
        design: Some(i.design.clone()),
        acceptance_criteria: Some(i.acceptance_criteria.clone()),
        notes: Some(i.notes.clone()),
        status: Some(i.status.clone()),
        priority: Some(i.priority),
        issue_type: Some(i.issue_type.clone()),
        assignee: Some(i.assignee.clone()),
        estimated_minutes: i.estimated_minutes,
        due_at: i.due_at,
        defer_until: i.defer_until,
        close_reason: (!i.close_reason.is_empty()).then(|| i.close_reason.clone()),
        metadata: i.metadata.clone(),
        spec_id: (!i.spec_id.is_empty()).then(|| i.spec_id.clone()),
        external_ref: i.external_ref.clone(),
        pinned: Some(i.pinned),
    }
}

// ---------------------------------------------------------------------------
// Commands that need a commit graph
// ---------------------------------------------------------------------------

pub fn branch(ctx: &Ctx) -> Result<()> {
    require_cap(ctx, "branch", Cap::VersionControl)?;
    stub("branch", ctx)
}

pub fn vc(ctx: &Ctx, cmd: VcCmd) -> Result<()> {
    let name = match cmd {
        VcCmd::Merge { .. } => "vc merge",
        VcCmd::Commit { .. } => "vc commit",
        VcCmd::Status => "vc status",
    };
    require_cap(ctx, name, Cap::VersionControl)?;
    stub(name, ctx)
}

pub fn dolt(ctx: &Ctx, cmd: DoltCmd) -> Result<()> {
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

pub fn tracker(ctx: &Ctx, tracker: &str, cmd: TrackerCmd) -> Result<()> {
    let verb = match cmd {
        TrackerCmd::Sync => "sync",
        TrackerCmd::Status => "status",
        TrackerCmd::Push => "push",
        TrackerCmd::Pull => "pull",
    };
    stub(&format!("{tracker} {verb}"), ctx)
}
