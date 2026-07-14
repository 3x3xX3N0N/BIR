//! Creating, changing, and closing beads.

use anyhow::{Result, bail};
use bd_core::{Dependency, Issue, IssueType, Priority};
use bd_storage::IssuePatch;
use serde_json::json;

use crate::cli::{CloseArgs, CommentsCmd, CreateArgs, LabelCmd, QuickArgs, UpdateArgs};
use crate::context::Ctx;
use crate::output::issue_json;

pub async fn create(ctx: &Ctx, a: CreateArgs) -> Result<()> {
    ctx.ensure_writable("create an issue")?;
    let store = ctx.store()?;

    let description = a.description.unwrap_or_default();
    let prefix = ctx.prefix().await;
    // The id is minted by the store, not by us: only it can see what is already
    // taken, and only it can widen the id when the table grows.
    let id = store.next_id(&prefix, &a.title, &description).await?;

    let mut issue = Issue::new(&id, &a.title);
    issue.description = description;
    issue.design = a.design.unwrap_or_default();
    issue.acceptance_criteria = a.acceptance.unwrap_or_default();
    issue.notes = a.notes.unwrap_or_default();
    issue.priority = a
        .priority
        .unwrap_or(Priority::new(ctx.config.defaults.priority).unwrap_or_default());
    issue.issue_type = a
        .issue_type
        .unwrap_or_else(|| IssueType::from(ctx.config.defaults.issue_type.clone()));
    issue.assignee = a.assignee.unwrap_or_default();
    issue.created_by = ctx.identity.actor.clone();
    issue.defer_until = a.defer_until;
    issue.due_at = a.due;
    issue.estimated_minutes = a.estimate;
    issue.labels = a.labels.clone();
    issue.validate()?;

    let mut created = store.create_issue(&issue).await?;

    // The seam does not promise that `create_issue` persists hydrated labels,
    // and two backends could reasonably differ. Reconcile against what came
    // back rather than assuming — a silently dropped label is invisible.
    for l in &a.labels {
        if !created.labels.iter().any(|x| x == l) {
            store.add_label(&created.id, l).await?;
            created.labels.push(l.clone());
        }
    }

    for dep in &a.deps {
        let mut d = Dependency::new(&created.id, &dep.id, dep.dep_type.clone())?;
        d.created_by = ctx.identity.actor.clone();
        store.add_dependency(&d).await?;
        created.dependencies.push(d);
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&created, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Created issue: {}", created.id));
    }
    Ok(())
}

/// `bd q` — the scripting path. Prints the id and nothing else, so that
/// `ISSUE=$(bd q "...")` works without any parsing.
pub async fn quick(ctx: &Ctx, a: QuickArgs) -> Result<()> {
    ctx.ensure_writable("create an issue")?;
    let store = ctx.store()?;

    let prefix = ctx.prefix().await;
    let id = store.next_id(&prefix, &a.title, "").await?;

    let mut issue = Issue::new(&id, &a.title);
    issue.priority = a
        .priority
        .unwrap_or(Priority::new(ctx.config.defaults.priority).unwrap_or_default());
    issue.issue_type = a
        .issue_type
        .unwrap_or_else(|| IssueType::from(ctx.config.defaults.issue_type.clone()));
    issue.created_by = ctx.identity.actor.clone();
    issue.labels = a.labels.clone();
    issue.validate()?;

    let created = store.create_issue(&issue).await?;
    for l in &a.labels {
        if !created.labels.iter().any(|x| x == l) {
            store.add_label(&created.id, l).await?;
        }
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "id": created.id }))?;
    } else {
        // Deliberately not `out.line`: this is the command's *output*, not a
        // status message, so it survives --quiet.
        println!("{}", created.id);
    }
    Ok(())
}

pub async fn show(ctx: &Ctx, ids: &[String]) -> Result<()> {
    let store = ctx.store()?;
    let mut docs = Vec::new();

    for (n, id) in ids.iter().enumerate() {
        let Some(issue) = store.get_issue(id).await? else {
            bail!("issue not found: {id}");
        };
        let depends_on = store.dependencies_of(id).await?;
        let dependents = store.dependents_of(id).await?;
        let comments = store.list_comments(id).await?;

        if ctx.out.is_json() {
            docs.push(issue_json(&issue, &depends_on, &dependents, &comments));
        } else {
            if n > 0 {
                ctx.out.line("");
            }
            ctx.out
                .issue_detail(&issue, &depends_on, &dependents, &comments)?;
        }
    }

    if ctx.out.is_json() {
        // One id in, one object out. An agent asking for a single issue should
        // not have to unwrap a one-element array.
        if docs.len() == 1 {
            ctx.out.json_value(&docs[0])?;
        } else {
            ctx.out.json_value(&docs)?;
        }
    }
    Ok(())
}

pub async fn update(ctx: &Ctx, a: UpdateArgs) -> Result<()> {
    ctx.ensure_writable("update an issue")?;
    let store = ctx.store()?;

    let mut patch = IssuePatch {
        title: a.title,
        description: a.description,
        design: a.design,
        acceptance_criteria: a.acceptance,
        notes: a.notes,
        status: a.status,
        priority: a.priority,
        issue_type: a.issue_type,
        assignee: a.assignee,
        estimated_minutes: a.estimate,
        due_at: a.due,
        defer_until: a.defer_until,
        spec_id: a.spec_id,
        external_ref: a.external_ref,
        ..Default::default()
    };
    if let Some(m) = &a.metadata {
        patch.metadata = Some(
            serde_json::from_str(m).map_err(|e| anyhow::anyhow!("--metadata is not JSON: {e}"))?,
        );
    }
    if a.pin {
        patch.pinned = Some(true);
    }
    if a.unpin {
        patch.pinned = Some(false);
    }

    if !a.claim && patch.is_empty() {
        bail!("nothing to update (pass --claim or a field to change)");
    }

    if a.claim {
        // A lease, not a lock: if this agent dies the issue comes back to
        // `bd ready` on its own.
        let lease = a.lease.unwrap_or_else(|| ctx.lease());
        let claim = store.claim_issue(&a.id, lease).await?;
        ctx.out.line(format!(
            "Claimed {} for {} until {}",
            claim.issue_id,
            claim.holder,
            claim.expires_at.format("%Y-%m-%d %H:%M")
        ));
    }

    if !patch.is_empty() {
        store.update_issue(&a.id, &patch).await?;
        ctx.out.line(format!("Updated {}", a.id));
    }

    if ctx.out.is_json() {
        let issue = store
            .get_issue(&a.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("issue not found: {}", a.id))?;
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    }
    Ok(())
}

pub async fn close(ctx: &Ctx, a: CloseArgs) -> Result<()> {
    ctx.ensure_writable("close an issue")?;
    let store = ctx.store()?;

    let mut closed = Vec::new();
    for id in &a.ids {
        // The reason is load-bearing: `conditional-blocks` dependents read it to
        // decide whether the failure path becomes ready.
        let issue = store.close_issue(id, &a.reason).await?;
        ctx.out.line(format!("Closed {id}: {}", a.reason));
        closed.push(issue);
    }
    if ctx.out.is_json() {
        ctx.out.json_value(&closed)?;
    }
    Ok(())
}

pub async fn reopen(ctx: &Ctx, ids: &[String]) -> Result<()> {
    ctx.ensure_writable("reopen an issue")?;
    let store = ctx.store()?;

    let mut reopened = Vec::new();
    for id in ids {
        let issue = store.reopen_issue(id).await?;
        ctx.out.line(format!("Reopened {id}"));
        reopened.push(issue);
    }
    if ctx.out.is_json() {
        ctx.out.json_value(&reopened)?;
    }
    Ok(())
}

pub async fn delete(ctx: &Ctx, ids: &[String]) -> Result<()> {
    ctx.ensure_writable("delete an issue")?;
    let store = ctx.store()?;

    for id in ids {
        store.delete_issue(id).await?;
        ctx.out.line(format!("Deleted {id}"));
    }
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "deleted": ids }))?;
    }
    Ok(())
}

pub async fn assign(ctx: &Ctx, id: &str, assignee: &str) -> Result<()> {
    ctx.ensure_writable("assign an issue")?;
    let store = ctx.store()?;
    let patch = IssuePatch {
        assignee: Some(assignee.to_string()),
        ..Default::default()
    };
    let issue = store.update_issue(id, &patch).await?;
    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Assigned {id} to {assignee}"));
    }
    Ok(())
}

pub async fn unclaim(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("release a claim")?;
    let store = ctx.store()?;
    store.release_claim(id).await?;
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "id": id, "claim": null }))?;
    } else {
        ctx.out.line(format!("Released the claim on {id}"));
    }
    Ok(())
}

pub async fn priority(ctx: &Ctx, id: &str, priority: Priority) -> Result<()> {
    ctx.ensure_writable("change a priority")?;
    let store = ctx.store()?;
    let patch = IssuePatch {
        priority: Some(priority),
        ..Default::default()
    };
    let issue = store.update_issue(id, &patch).await?;
    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("{id} is now {priority}"));
    }
    Ok(())
}

pub async fn comment(ctx: &Ctx, id: &str, text: &str) -> Result<()> {
    ctx.ensure_writable("comment")?;
    let store = ctx.store()?;
    let c = store.add_comment(id, text).await?;
    if ctx.out.is_json() {
        ctx.out.json_value(&c)?;
    } else {
        ctx.out.line(format!("Commented on {id}"));
    }
    Ok(())
}

pub async fn comments(ctx: &Ctx, cmd: CommentsCmd) -> Result<()> {
    match cmd {
        CommentsCmd::List { id } => {
            let store = ctx.store()?;
            let cs = store.list_comments(&id).await?;
            ctx.out.comments(&cs)
        }
        CommentsCmd::Add { id, text } => comment(ctx, &id, &text.join(" ")).await,
    }
}

pub async fn label(ctx: &Ctx, cmd: LabelCmd) -> Result<()> {
    match cmd {
        LabelCmd::Add { id, labels } => {
            ctx.ensure_writable("add a label")?;
            let store = ctx.store()?;
            if labels.is_empty() {
                bail!("no labels given");
            }
            for l in &labels {
                store.add_label(&id, l).await?;
            }
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({ "id": id, "added": labels }))?;
            } else {
                ctx.out
                    .line(format!("Labeled {id}: {}", labels.join(", ")));
            }
            Ok(())
        }
        LabelCmd::Remove { id, labels } => {
            ctx.ensure_writable("remove a label")?;
            let store = ctx.store()?;
            if labels.is_empty() {
                bail!("no labels given");
            }
            for l in &labels {
                store.remove_label(&id, l).await?;
            }
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({ "id": id, "removed": labels }))?;
            } else {
                ctx.out
                    .line(format!("Unlabeled {id}: {}", labels.join(", ")));
            }
            Ok(())
        }
        LabelCmd::List { id } => {
            let store = ctx.store()?;
            // There is no per-issue label getter on the seam: an issue's labels
            // are hydrated onto the issue itself.
            let issue = store
                .get_issue(&id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("issue not found: {id}"))?;
            if ctx.out.is_json() {
                ctx.out.json_value(&issue.labels)?;
            } else if issue.labels.is_empty() {
                ctx.out.line("No labels.");
            } else {
                for l in &issue.labels {
                    ctx.out.line(l);
                }
            }
            Ok(())
        }
        LabelCmd::ListAll => {
            let store = ctx.store()?;
            let labels = store.list_labels().await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&labels)?;
            } else if labels.is_empty() {
                ctx.out.line("No labels in this workspace.");
            } else {
                for l in &labels {
                    ctx.out.line(l);
                }
            }
            Ok(())
        }
        LabelCmd::Propagate { .. } => crate::commands::stub("label propagate", ctx),
    }
}
