//! Creating, changing, and closing beads.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use bd_core::types::MAX_TITLE_LEN;
use bd_core::{Dependency, DependencyType, Issue, IssueType, Priority, Status, StatusCategory};
use bd_storage::{Field, IssuePatch, Storage};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::cli::{CloseArgs, CommentsCmd, CreateArgs, LabelCmd, QuickArgs, UpdateArgs};
use crate::commands::stub;
use crate::context::Ctx;
use crate::output::issue_json;

pub async fn create(ctx: &Ctx, a: CreateArgs) -> Result<()> {
    ctx.ensure_writable("create an issue")?;
    let store = ctx.store().await?;

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
    let store = ctx.store().await?;

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
    let store = ctx.store().await?;
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
    let store = ctx.store().await?;

    // An absent flag becomes `Field::Keep`, never `Field::Clear` -- so omitting
    // `--assignee` leaves the assignee alone rather than silently unassigning.
    // Clearing is only ever reachable by asking for it: `bd unclaim`,
    // `bd undefer`.
    let mut patch = IssuePatch {
        title: a.title,
        description: a.description.into(),
        design: a.design.into(),
        acceptance_criteria: a.acceptance.into(),
        notes: a.notes.into(),
        status: a.status,
        priority: a.priority,
        issue_type: a.issue_type,
        assignee: a.assignee.into(),
        estimated_minutes: a.estimate.into(),
        due_at: a.due.into(),
        defer_until: a.defer_until.into(),
        spec_id: a.spec_id.into(),
        external_ref: a.external_ref.into(),
        ..Default::default()
    };
    if let Some(m) = &a.metadata {
        patch.metadata = Field::Set(
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
    let store = ctx.store().await?;

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
    let store = ctx.store().await?;

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
    let store = ctx.store().await?;

    for id in ids {
        store.delete_issue(id).await?;
        ctx.out.line(format!("Deleted {id}"));
    }
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "deleted": ids }))?;
    }
    Ok(())
}

/// Hide an issue from `bd ready` until a time passes.
///
/// Deferring does not block the issue — nothing is waiting on it and the graph
/// is untouched. It is simply not yet due, so `bd ready` skips it and picks it
/// back up on its own once the clock catches up.
pub async fn defer(ctx: &Ctx, id: &str, until: Option<DateTime<Utc>>) -> Result<()> {
    ctx.ensure_writable("defer an issue")?;
    let store = ctx.store().await?;

    // No `--until` means "not now, ask me later": deferring indefinitely would
    // be a quiet way to lose work, so default to a day rather than to forever.
    let until = until.unwrap_or_else(|| Utc::now() + chrono::Duration::days(1));

    let patch = IssuePatch {
        defer_until: Field::Set(until),
        ..Default::default()
    };
    let issue = store.update_issue(id, &patch).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    } else {
        ctx.out
            .line(format!("Deferred {id} until {}", until.format("%Y-%m-%d %H:%M")));
    }
    Ok(())
}

/// Bring a deferred issue back into `bd ready` now.
pub async fn undefer(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("undefer an issue")?;
    let store = ctx.store().await?;

    // `Field::Clear`, not `Field::Set(now)`. Setting the deadline to the present
    // would *look* right -- the issue reappears in `bd ready` either way -- but
    // it leaves a defer_until behind, so the issue reads as "deferred, and the
    // deadline happened to pass" rather than "not deferred". The distinction
    // surfaces the moment anything asks which issues are deferred.
    let issue = store.update_issue(id, &IssuePatch::undefer()).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Undeferred {id}; it is claimable again"));
    }
    Ok(())
}

pub async fn assign(ctx: &Ctx, id: &str, assignee: &str) -> Result<()> {
    ctx.ensure_writable("assign an issue")?;
    let store = ctx.store().await?;
    let patch = IssuePatch {
        assignee: Field::Set(assignee.to_string()),
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
    let store = ctx.store().await?;
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
    let store = ctx.store().await?;
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

/// The text arrives as words, not as a string: clap collects a trailing
/// `Vec<String>` so that `bd comment x-1 needs a rebase` works unquoted.
pub async fn comment(ctx: &Ctx, id: &str, text: &[String]) -> Result<()> {
    ctx.ensure_writable("comment")?;
    let store = ctx.store().await?;
    let c = store.add_comment(id, &text.join(" ")).await?;
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
            let store = ctx.store().await?;
            let cs = store.list_comments(&id).await?;
            ctx.out.comments(&cs)
        }
        CommentsCmd::Add { id, text } => comment(ctx, &id, &text).await,
    }
}

pub async fn label(ctx: &Ctx, cmd: LabelCmd) -> Result<()> {
    match cmd {
        LabelCmd::Add { id, labels } => {
            ctx.ensure_writable("add a label")?;
            let store = ctx.store().await?;
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
            let store = ctx.store().await?;
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
            let store = ctx.store().await?;
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
            let store = ctx.store().await?;
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
        LabelCmd::Propagate { id } => label_propagate(ctx, &id).await,
    }
}

/// Copy an issue's labels down to its `parent-child` children.
///
/// A label on an epic ("area:auth", "release:2.0") usually wants to be on the
/// work under it, and applying it by hand to every child is exactly the kind of
/// bookkeeping that rots. This applies the parent's labels to each direct child,
/// skipping any a child already has, and reports how many it added.
///
/// Direct children only, not the whole subtree: propagating recursively is a
/// different, heavier operation, and a caller who wants it can run this at each
/// level. A child is the `issue_id` of a `parent-child` edge whose
/// `depends_on_id` is the parent.
async fn label_propagate(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("propagate labels")?;
    let store = ctx.store().await?;
    let parent = require_issue(store, id).await?;

    if parent.labels.is_empty() {
        if ctx.out.is_json() {
            ctx.out.json_value(&json!({ "id": id, "propagated": 0, "children": 0 }))?;
        } else {
            ctx.out.line(format!("{id} has no labels to propagate"));
        }
        return Ok(());
    }

    let children: Vec<String> = store
        .dependents_of(id)
        .await?
        .into_iter()
        .filter(|d| d.dep_type == DependencyType::ParentChild)
        .map(|d| d.issue_id)
        .collect();

    // What each child already has, in one query, so a re-run is cheap and adds
    // nothing it need not.
    let existing: std::collections::HashMap<String, Vec<String>> =
        store.labels_of(&children).await?.into_iter().collect();

    let mut added = 0u64;
    for child in &children {
        let have = existing.get(child).cloned().unwrap_or_default();
        for label in &parent.labels {
            if !have.contains(label) {
                store.add_label(child, label).await?;
                added += 1;
            }
        }
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "id": id,
            "labels": parent.labels,
            "children": children.len(),
            "propagated": added,
        }))?;
    } else {
        ctx.out.line(format!(
            "Propagated {} label(s) to {} child(ren): {} addition(s)",
            parent.labels.len(),
            children.len(),
            added
        ));
    }
    Ok(())
}

/// The seam has no per-issue existence check, and every mutation below needs
/// one: `remove_label` on an unknown issue is a no-op DELETE, so without this a
/// typo'd id would report success and change nothing.
async fn require_issue(store: &dyn Storage, id: &str) -> Result<Issue> {
    store
        .get_issue(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("issue not found: {id}"))
}

pub async fn rename(ctx: &Ctx, id: &str, title: &str) -> Result<()> {
    ctx.ensure_writable("rename an issue")?;
    let store = ctx.store().await?;

    // `Issue::validate` guards the create path; an update never passes through
    // it, so the same two rules are enforced here or nowhere.
    let title = title.trim();
    if title.is_empty() {
        bail!("a title cannot be empty");
    }
    if title.chars().count() > MAX_TITLE_LEN {
        bail!("a title cannot exceed {MAX_TITLE_LEN} characters");
    }

    let patch = IssuePatch {
        title: Some(title.to_string()),
        ..Default::default()
    };
    let issue = store.update_issue(id, &patch).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Renamed {id}: {title}"));
    }
    Ok(())
}

/// `bd tag x-1 urgent -stale` adds `urgent` and removes `stale`.
///
/// The `-` prefix is why the argument carries `allow_hyphen_values` in the
/// command tree: clap would otherwise take `-stale` for an unknown flag and
/// refuse the whole invocation.
pub async fn tag(ctx: &Ctx, id: &str, tags: &[String]) -> Result<()> {
    ctx.ensure_writable("tag an issue")?;
    let store = ctx.store().await?;
    require_issue(store, id).await?;

    let mut added = Vec::new();
    let mut removed = Vec::new();
    for t in tags {
        match t.strip_prefix('-') {
            Some(rest) => {
                let rest = rest.trim();
                if rest.is_empty() {
                    bail!("`-` on its own is not a tag");
                }
                store.remove_label(id, rest).await?;
                removed.push(rest.to_string());
            }
            None => {
                let t = t.trim();
                if t.is_empty() {
                    bail!("an empty tag is not a tag");
                }
                store.add_label(id, t).await?;
                added.push(t.to_string());
            }
        }
    }

    if ctx.out.is_json() {
        // Re-read rather than report what we asked for: the resulting label set
        // is the answer, and it is not the same thing as the request (adding a
        // label twice is idempotent, removing an absent one is a no-op).
        let issue = require_issue(store, id).await?;
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    } else {
        if !added.is_empty() {
            ctx.out.line(format!("Tagged {id}: {}", added.join(", ")));
        }
        if !removed.is_empty() {
            ctx.out.line(format!("Untagged {id}: {}", removed.join(", ")));
        }
    }
    Ok(())
}

/// The text arrives as words for the same reason `bd comment`'s does: so that
/// `bd note x-1 flaky on windows` works unquoted.
pub async fn note(ctx: &Ctx, id: &str, text: &[String]) -> Result<()> {
    ctx.ensure_writable("add a note")?;
    let store = ctx.store().await?;
    let issue = append_note(store, id, &text.join(" ")).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Noted on {id}"));
    }
    Ok(())
}

/// Notes accumulate. A comment is its own record; notes are one column, so
/// appending means read-modify-write — `Field::Set` replaces the column outright,
/// and writing the new note alone would silently discard every note before it.
///
/// **This is not atomic.** Two agents noting the same bead at the same moment can
/// lose one of the notes, because the read and the write are separate round
/// trips. The seam has no append (`append_notes(id, text)`); until it does, the
/// window is real, and it is the reason a note is not a substitute for a comment.
async fn append_note(store: &dyn Storage, id: &str, text: &str) -> Result<Issue> {
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to note");
    }
    let issue = require_issue(store, id).await?;

    let notes = if issue.notes.trim().is_empty() {
        text.to_string()
    } else {
        format!("{}\n{text}", issue.notes.trim_end())
    };
    let patch = IssuePatch {
        notes: Field::Set(notes),
        ..Default::default()
    };
    Ok(store.update_issue(id, &patch).await?)
}

pub async fn duplicate(ctx: &Ctx, id: &str, of: &str) -> Result<()> {
    associate(ctx, id, of, DependencyType::Duplicates).await
}

pub async fn supersede(ctx: &Ctx, id: &str, with: &str) -> Result<()> {
    associate(ctx, id, with, DependencyType::Supersedes).await
}

pub async fn link(ctx: &Ctx, from: &str, to: &str, link_type: Option<DependencyType>) -> Result<()> {
    associate(ctx, from, to, link_type.unwrap_or(DependencyType::Related)).await
}

/// The three association commands, which are one operation: add an edge that
/// records a relationship without gating anything.
///
/// The guard is the whole point. `bd ready` is computed from exactly the four
/// edge types [`DependencyType::affects_ready_work`] names, so an edge that
/// slipped into that set here would quietly make work unclaimable — marking a
/// duplicate would *block* the original, and nothing would say so. Blocking
/// edges have a command of their own; send the user there rather than growing a
/// second, unaudited way to create them.
async fn associate(ctx: &Ctx, from: &str, to: &str, dep_type: DependencyType) -> Result<()> {
    ctx.ensure_writable("link two issues")?;
    if dep_type.affects_ready_work() {
        bail!("`{dep_type}` gates readiness; use `bd dep add` for edges that block work");
    }
    let store = ctx.store().await?;

    // `Dependency::new` rejects a self-edge, and `add_dependency` rejects an
    // unknown id on either end — so neither is re-checked here.
    let mut dep = Dependency::new(from, to, dep_type)?;
    dep.created_by = ctx.identity.actor.clone();
    store.add_dependency(&dep).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&dep)?;
    } else {
        ctx.out
            .line(format!("{from} {} {to}", dep.dep_type.as_str()));
    }
    Ok(())
}

/// Keep a claim alive. A lease that is never renewed lapses, and the issue
/// returns to `bd ready` — which is the point of a lease, and why a long-running
/// agent has to say it is still here.
pub async fn heartbeat(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("renew a claim")?;
    let store = ctx.store().await?;

    let claim = store.renew_claim(id, ctx.lease()).await?;

    if ctx.out.is_json() {
        // Claim's own field names: this is not an issue and must not pretend to
        // be one.
        ctx.out.json_value(&json!({
            "issue_id": claim.issue_id,
            "holder": claim.holder,
            "expires_at": claim.expires_at,
        }))?;
    } else {
        ctx.out.line(format!(
            "Renewed {}'s claim on {id} until {}",
            claim.holder,
            claim.expires_at.format("%Y-%m-%d %H:%M")
        ));
    }
    Ok(())
}

/// The statuses a bead can hold.
///
/// These are the built-ins and only the built-ins. [`Status::Custom`] exists and
/// the domain resolves its category "against the workspace's status config" —
/// but the seam has no such config: there is no `list_statuses`, and inventing a
/// config key here would make this command's answer disagree with whatever the
/// integrator later chooses. So: report what is knowable, and say that is what
/// this is.
pub async fn statuses(ctx: &Ctx) -> Result<()> {
    let all = builtin_statuses();
    if ctx.out.is_json() {
        let docs: Vec<_> = all
            .iter()
            .map(|s| {
                json!({
                    "name": s.as_str(),
                    "category": category_name(s.category()),
                    "builtin": true,
                })
            })
            .collect();
        ctx.out.json_value(&docs)?;
    } else {
        // The same spelling as the JSON, deliberately: two renderings of one
        // fact that disagree are worse than either.
        for s in &all {
            ctx.out
                .line(format!("{:<12} {}", s.as_str(), category_name(s.category())));
        }
        ctx.out
            .line("\nBuilt-in statuses only; custom statuses are not stored in the workspace yet.");
    }
    Ok(())
}

/// `StatusCategory` has no `as_str`, and `{:?}` would print `Active` where the
/// JSON says `active`.
fn category_name(c: StatusCategory) -> &'static str {
    match c {
        StatusCategory::Active => "active",
        StatusCategory::Wip => "wip",
        StatusCategory::Done => "done",
        StatusCategory::Frozen => "frozen",
        StatusCategory::Unspecified => "unspecified",
    }
}

/// The issue types, and — the part worth knowing — which of them never surface
/// as claimable work.
pub async fn types(ctx: &Ctx) -> Result<()> {
    let all = builtin_types();
    if ctx.out.is_json() {
        let docs: Vec<_> = all
            .iter()
            .map(|t| {
                json!({
                    "name": t.as_str(),
                    "excluded_from_ready": t.excluded_from_ready(),
                    "builtin": true,
                })
            })
            .collect();
        ctx.out.json_value(&docs)?;
    } else {
        for t in &all {
            let note = if t.excluded_from_ready() {
                "  (infrastructure; never claimable work)"
            } else {
                ""
            };
            ctx.out.line(format!("{:<12}{note}", t.as_str()));
        }
        ctx.out
            .line("\nBuilt-in types only; any other type name is accepted and stored as-is.");
    }
    Ok(())
}

fn builtin_statuses() -> Vec<Status> {
    use Status::*;
    vec![Open, InProgress, Blocked, Deferred, Closed, Pinned, Hooked]
}

fn builtin_types() -> Vec<IssueType> {
    use IssueType::*;
    vec![
        Bug, Feature, Task, Epic, Chore, Decision, Message, Molecule, Gate, Spike, Story, Milestone,
        Event,
    ]
}

// ---------------------------------------------------------------------------
// bd edit
// ---------------------------------------------------------------------------

/// The four free-text sections of the edit document, in the order they are
/// written.
const EDIT_SECTIONS: [&str; 4] = ["description", "design", "acceptance", "notes"];

/// Open the issue in `$EDITOR`, and apply whatever comes back.
pub async fn edit(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("edit an issue")?;
    let store = ctx.store().await?;
    let issue = require_issue(store, id).await?;

    let before = render_edit_doc(&issue);
    let path = std::env::temp_dir().join(format!("bd-edit-{}-{}.md", issue.id, std::process::id()));
    std::fs::write(&path, &before)
        .with_context(|| format!("cannot write the edit buffer at {}", path.display()))?;

    // Read back before cleaning up, so a failing editor cannot leave the buffer
    // behind and a successful one cannot leave it lying around with the issue's
    // contents in /tmp.
    let edited = run_editor(&path).and_then(|()| {
        std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read back {}", path.display()))
    });
    let _ = std::fs::remove_file(&path);
    let after = edited?;

    // git's convention, and the only abort gesture an editor can express.
    if after.trim().is_empty() {
        bail!("aborted: the edit buffer came back empty; {id} is unchanged");
    }

    let doc = parse_edit_doc(&after)?;
    let (patch, add, remove) = doc.diff(&issue)?;

    if patch.is_empty() && add.is_empty() && remove.is_empty() {
        ctx.out.line(format!("No changes to {id}."));
        if ctx.out.is_json() {
            ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
        }
        return Ok(());
    }

    if !patch.is_empty() {
        store.update_issue(id, &patch).await?;
    }
    for l in &add {
        store.add_label(id, l).await?;
    }
    for l in &remove {
        store.remove_label(id, l).await?;
    }

    let issue = require_issue(store, id).await?;
    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&issue, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Updated {id}"));
    }
    Ok(())
}

/// `$VISUAL`, then `$EDITOR`, then the platform's last resort.
///
/// The value is a command line, not a path — `EDITOR="code -w"` is common — so
/// it is split on whitespace. That cannot express an editor whose path contains
/// a space; every tool that does this has the same hole, and quoting rules that
/// differ per platform would be a worse one.
fn editor_command() -> Vec<String> {
    for key in ["VISUAL", "EDITOR"] {
        let Some(raw) = std::env::var_os(key) else {
            continue;
        };
        let parts: Vec<String> = raw
            .to_string_lossy()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !parts.is_empty() {
            return parts;
        }
    }
    vec![if cfg!(windows) { "notepad" } else { "vi" }.to_string()]
}

/// Blocking on purpose: the editor owns the terminal until the human is done,
/// and there is nothing else for this process to do meanwhile.
fn run_editor(path: &Path) -> Result<()> {
    let cmd = editor_command();
    let (prog, args) = cmd.split_first().expect("editor_command is never empty");

    let status = Command::new(prog)
        .args(args)
        .arg(path)
        .status()
        .with_context(|| format!("cannot start the editor `{prog}` (set $EDITOR)"))?;
    if !status.success() {
        bail!("the editor `{prog}` exited with {status}; nothing was changed");
    }
    Ok(())
}

/// What the editor buffer can say. Every field is optional: deleting a line
/// means "leave this alone", which is the only forgiving reading — the
/// alternative is that a slip of the cursor clears a field.
#[derive(Debug, Default, PartialEq)]
struct EditDoc {
    title: Option<String>,
    status: Option<Status>,
    priority: Option<Priority>,
    issue_type: Option<IssueType>,
    assignee: Option<String>,
    labels: Option<Vec<String>>,
    sections: [String; 4],
}

impl EditDoc {
    fn section(&self, name: &str) -> &str {
        let i = EDIT_SECTIONS
            .iter()
            .position(|s| *s == name)
            .expect("section names are compile-time constants");
        &self.sections[i]
    }

    /// Only what actually changed. An edit that touched nothing must produce an
    /// empty patch, or every `bd edit` would bump `updated_at` and write an
    /// event for a file the user opened and closed.
    fn diff(&self, issue: &Issue) -> Result<(IssuePatch, Vec<String>, Vec<String>)> {
        let mut patch = IssuePatch::default();

        match self.title.as_deref().map(str::trim) {
            Some("") => bail!("a title cannot be empty"),
            Some(t) if t.chars().count() > MAX_TITLE_LEN => {
                bail!("a title cannot exceed {MAX_TITLE_LEN} characters")
            }
            Some(t) if t != issue.title => patch.title = Some(t.to_string()),
            _ => {}
        }
        if let Some(s) = &self.status
            && *s != issue.status
        {
            patch.status = Some(s.clone());
        }
        if let Some(p) = self.priority
            && p != issue.priority
        {
            patch.priority = Some(p);
        }
        if let Some(t) = &self.issue_type
            && *t != issue.issue_type
        {
            patch.issue_type = Some(t.clone());
        }
        if let Some(a) = self.assignee.as_deref().map(str::trim)
            && a != issue.assignee
        {
            // Emptying the assignee here is a real clear, not a keep: the user
            // deleted the name rather than deleting the line.
            patch.assignee = if a.is_empty() {
                Field::Clear
            } else {
                Field::Set(a.to_string())
            };
        }

        for (name, current) in [
            ("description", &issue.description),
            ("design", &issue.design),
            ("acceptance", &issue.acceptance_criteria),
            ("notes", &issue.notes),
        ] {
            let new = self.section(name);
            if new == current.trim() {
                continue;
            }
            let f = Field::Set(new.to_string());
            match name {
                "description" => patch.description = f,
                "design" => patch.design = f,
                "acceptance" => patch.acceptance_criteria = f,
                _ => patch.notes = f,
            }
        }

        // Labels are not on `IssuePatch` — they are their own seam calls — so
        // they come back as a diff for the caller to apply.
        let (mut add, mut remove) = (Vec::new(), Vec::new());
        if let Some(want) = &self.labels {
            add = want
                .iter()
                .filter(|l| !issue.labels.contains(l))
                .cloned()
                .collect();
            remove = issue
                .labels
                .iter()
                .filter(|l| !want.contains(l))
                .cloned()
                .collect();
        }
        Ok((patch, add, remove))
    }
}

fn render_edit_doc(i: &Issue) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# bd edit {} — save and quit to apply; empty the file to abort.\n",
        i.id
    ));
    s.push_str("# `#` comments and unknown keys are only read in this header.\n");
    s.push_str(&format!("title: {}\n", i.title));
    s.push_str(&format!("status: {}\n", i.status));
    s.push_str(&format!("priority: {}\n", i.priority.0));
    s.push_str(&format!("type: {}\n", i.issue_type));
    s.push_str(&format!("assignee: {}\n", i.assignee));
    s.push_str(&format!("labels: {}\n", i.labels.join(", ")));

    for (name, body) in EDIT_SECTIONS.iter().zip([
        &i.description,
        &i.design,
        &i.acceptance_criteria,
        &i.notes,
    ]) {
        s.push_str(&format!("\n--- {name} ---\n"));
        let body = body.trim();
        if !body.is_empty() {
            s.push_str(body);
            s.push('\n');
        }
    }
    s
}

/// A line is a section header only if it names one of the four sections.
///
/// That strictness is what makes the format safe for prose: a bare `---` rule or
/// a `--- 8< ---` in a description is body text, because it is not a section
/// name. Only a line that reads exactly `--- description ---` can be mistaken
/// for one, and that is a price worth the readability of the buffer.
fn section_header(line: &str) -> Option<usize> {
    let inner = line.trim().strip_prefix("---")?.strip_suffix("---")?.trim();
    EDIT_SECTIONS.iter().position(|s| *s == inner)
}

fn parse_edit_doc(src: &str) -> Result<EditDoc> {
    let mut doc = EditDoc::default();
    let mut lines: [Vec<&str>; 4] = Default::default();
    let mut current: Option<usize> = None;

    for (n, raw) in src.lines().enumerate() {
        if let Some(i) = section_header(raw) {
            current = Some(i);
            continue;
        }
        // Inside a section every line is literal: a description is prose, and
        // prose contains `#` headings and `key: value` lines.
        if let Some(i) = current {
            lines[i].push(raw);
            continue;
        }

        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            bail!("line {}: expected `key: value`, got `{line}`", n + 1);
        };
        let value = value.trim().to_string();
        match key.trim() {
            "title" => doc.title = Some(value),
            "status" => doc.status = Some(Status::from(value)),
            "priority" => {
                doc.priority = Some(
                    value
                        .parse()
                        .with_context(|| format!("line {}: bad priority", n + 1))?,
                )
            }
            "type" => doc.issue_type = Some(IssueType::from(value)),
            "assignee" => doc.assignee = Some(value),
            "labels" => {
                doc.labels = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_string)
                        .collect(),
                )
            }
            // A typo'd key must not be silently dropped: that is how an edit
            // reports success and changes nothing.
            other => bail!("line {}: unknown field `{other}`", n + 1),
        }
    }

    for (i, body) in lines.iter().enumerate() {
        doc.sections[i] = body.join("\n").trim().to_string();
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// bd batch
// ---------------------------------------------------------------------------

/// One change. The tag is `op`, so a batch document reads as a list of verbs.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum BatchOp {
    Create {
        title: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default, rename = "type")]
        issue_type: Option<IssueType>,
        #[serde(default)]
        priority: Option<Priority>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        labels: Vec<String>,
    },
    Update {
        id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        status: Option<Status>,
        #[serde(default)]
        priority: Option<Priority>,
        #[serde(default, rename = "type")]
        issue_type: Option<IssueType>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        design: Option<String>,
        #[serde(default)]
        acceptance: Option<String>,
        #[serde(default)]
        notes: Option<String>,
    },
    Close {
        id: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Reopen {
        id: String,
    },
    Delete {
        id: String,
    },
    Label {
        id: String,
        #[serde(default)]
        add: Vec<String>,
        #[serde(default)]
        remove: Vec<String>,
    },
    Comment {
        id: String,
        text: String,
    },
    Note {
        id: String,
        text: String,
    },
    Dep {
        from: String,
        to: String,
        #[serde(default, rename = "type")]
        dep_type: Option<DependencyType>,
    },
    Undep {
        from: String,
        to: String,
        /// Which edge. Defaults to `blocks`, mirroring `dep`.
        #[serde(default, rename = "type")]
        dep_type: Option<DependencyType>,
    },
}

impl BatchOp {
    fn verb(&self) -> &'static str {
        match self {
            BatchOp::Create { .. } => "create",
            BatchOp::Update { .. } => "update",
            BatchOp::Close { .. } => "close",
            BatchOp::Reopen { .. } => "reopen",
            BatchOp::Delete { .. } => "delete",
            BatchOp::Label { .. } => "label",
            BatchOp::Comment { .. } => "comment",
            BatchOp::Note { .. } => "note",
            BatchOp::Dep { .. } => "dep",
            BatchOp::Undep { .. } => "undep",
        }
    }
}

/// Apply many changes from one document.
///
/// **Not atomic, and it cannot be.** The seam exposes no transaction, so a
/// failure at operation 7 leaves the first six applied. The parse is therefore
/// done up front, in full: a malformed document changes nothing, and a store
/// failure says exactly where it stopped. Do not paper over the difference —
/// a caller that thinks this is a transaction will build on sand.
pub async fn batch(ctx: &Ctx, file: Option<PathBuf>) -> Result<()> {
    ctx.ensure_writable("apply a batch")?;

    let raw = read_batch_input(file.as_deref())?;
    let ops = parse_batch(&raw)?;
    if ops.is_empty() {
        bail!("no operations in the batch document");
    }

    let store = ctx.store().await?;
    let prefix = ctx.prefix().await;

    let mut created: Vec<String> = Vec::new();
    let mut applied = 0usize;
    for (n, op) in ops.iter().enumerate() {
        apply_batch_op(ctx, store, &prefix, op, &mut created)
            .await
            .with_context(|| {
                format!(
                    "batch operation {} (`{}`) failed; {applied} of {} were already applied",
                    n + 1,
                    op.verb(),
                    ops.len()
                )
            })?;
        applied += 1;
    }

    if ctx.out.is_json() {
        ctx.out
            .json_value(&json!({ "applied": applied, "created": created }))?;
    } else {
        for id in &created {
            ctx.out.line(format!("Created issue: {id}"));
        }
        ctx.out.line(format!("Applied {applied} operation(s)."));
    }
    Ok(())
}

fn read_batch_input(file: Option<&Path>) -> Result<String> {
    match file {
        Some(p) if p != Path::new("-") => {
            std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))
        }
        _ => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("cannot read the batch document from stdin")?;
            Ok(s)
        }
    }
}

/// JSONL, a JSON array, or a single JSON object — all three are things a caller
/// will reasonably hand a batch command, and refusing two of them would only be
/// a way of being right.
fn parse_batch(raw: &str) -> Result<Vec<BatchOp>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).context("the batch is not a JSON array of operations");
    }
    if let Ok(one) = serde_json::from_str::<BatchOp>(trimmed) {
        return Ok(vec![one]);
    }

    let mut ops = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        ops.push(
            serde_json::from_str(line)
                .with_context(|| format!("line {}: not a batch operation", n + 1))?,
        );
    }
    Ok(ops)
}

async fn apply_batch_op(
    ctx: &Ctx,
    store: &dyn Storage,
    prefix: &str,
    op: &BatchOp,
    created: &mut Vec<String>,
) -> Result<()> {
    match op {
        BatchOp::Create {
            title,
            description,
            issue_type,
            priority,
            assignee,
            labels,
        } => {
            let description = description.clone().unwrap_or_default();
            let id = store.next_id(prefix, title, &description).await?;

            let mut issue = Issue::new(&id, title);
            issue.description = description;
            issue.priority = priority
                .unwrap_or(Priority::new(ctx.config.defaults.priority).unwrap_or_default());
            issue.issue_type = issue_type
                .clone()
                .unwrap_or_else(|| IssueType::from(ctx.config.defaults.issue_type.clone()));
            issue.assignee = assignee.clone().unwrap_or_default();
            issue.created_by = ctx.identity.actor.clone();
            issue.labels = labels.clone();
            issue.validate()?;

            let made = store.create_issue(&issue).await?;
            for l in labels {
                if !made.labels.iter().any(|x| x == l) {
                    store.add_label(&made.id, l).await?;
                }
            }
            created.push(made.id);
        }
        BatchOp::Update {
            id,
            title,
            status,
            priority,
            issue_type,
            assignee,
            description,
            design,
            acceptance,
            notes,
        } => {
            let patch = IssuePatch {
                title: title.clone(),
                status: status.clone(),
                priority: *priority,
                issue_type: issue_type.clone(),
                assignee: assignee.clone().into(),
                description: description.clone().into(),
                design: design.clone().into(),
                acceptance_criteria: acceptance.clone().into(),
                notes: notes.clone().into(),
                ..Default::default()
            };
            if patch.is_empty() {
                bail!("nothing to update on {id}");
            }
            store.update_issue(id, &patch).await?;
        }
        BatchOp::Close { id, reason } => {
            store
                .close_issue(id, reason.as_deref().unwrap_or("done"))
                .await?;
        }
        BatchOp::Reopen { id } => {
            store.reopen_issue(id).await?;
        }
        BatchOp::Delete { id } => {
            store.delete_issue(id).await?;
        }
        BatchOp::Label { id, add, remove } => {
            require_issue(store, id).await?;
            for l in add {
                store.add_label(id, l).await?;
            }
            for l in remove {
                store.remove_label(id, l).await?;
            }
        }
        BatchOp::Comment { id, text } => {
            store.add_comment(id, text).await?;
        }
        BatchOp::Note { id, text } => {
            append_note(store, id, text).await?;
        }
        BatchOp::Dep {
            from,
            to,
            dep_type: link_type,
        } => {
            let mut dep = Dependency::new(
                from.as_str(),
                to.as_str(),
                link_type.clone().unwrap_or(DependencyType::Blocks),
            )?;
            dep.created_by = ctx.identity.actor.clone();
            store.add_dependency(&dep).await?;
        }
        BatchOp::Undep {
            from,
            to,
            dep_type,
        } => {
            // The type is not decoration. Two beads may hold several edges at
            // once, and a removal that ignored the type would take all of them.
            store
                .remove_dependency(
                    from,
                    to,
                    dep_type.as_ref().unwrap_or(&DependencyType::Blocks),
                )
                .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Registered, not ported
// ---------------------------------------------------------------------------
//
// The arguments are already threaded through from the command tree, so filling
// one of these in is writing a body — not touching the dispatch.
//
// These three are not *unwritten*; they are **unwritable on this seam**, and the
// difference matters to whoever picks them up. Each needs storage that does not
// exist yet, and every way of faking it lies:
//
// * `restore` — `delete_issue` is `DELETE FROM issues`. There is no tombstone to
//   restore from. The `deleted` event survives the row, but it carries only the
//   title, so "restoring" from it would resurrect a bead with no description, no
//   priority, no labels and no edges — a different bead wearing the same id.
//   Needs: soft delete on the seam.
//     `async fn delete_issue(&self, id: &str, mode: DeleteMode) -> Result<()>`
//     `async fn restore_issue(&self, id: &str) -> Result<Issue>`
//     `async fn list_deleted(&self) -> Result<Vec<Issue>>`
//   plus a `deleted_at` column and its exclusion from every default filter.
//
// * `state` / `set-state` — a workflow state is not a status. It is a position
//   in a state machine the workspace defines, with legal transitions; that
//   machine is the formula DSL (wave 5) and nothing stores it. `set-state` over
//   `Status` would compile, would be indistinguishable from `bd update --status`,
//   and would validate no transition at all — a command that lies.
//   Needs: the workflow definition, and per-issue state:
//     `async fn get_workflow(&self) -> Result<Option<Workflow>>`
//     `async fn set_workflow(&self, w: &Workflow) -> Result<()>`
//     `async fn transition(&self, id: &str, to: &str) -> Result<Issue>`  // rejects an illegal move
//   (`statuses` has the smaller half of the same gap: there is no
//   `list_statuses`, so it reports the built-ins and says so.)
//
// (`promote` used to be on this list. It needed `ephemeral` and `wisp_type` on
// `IssuePatch`; they exist now, and it is written below. `no_history` and
// `mol_type` remain unreachable for the reason `promote` was.)

pub async fn restore(ctx: &Ctx, _id: &str) -> Result<()> {
    stub("restore", ctx)
}

/// Show an issue's workflow state (its status). The read half of the custom-
/// status workflow: `set-state` moves an issue through states this port does not
/// hard-code, and `state` reads where it is.
pub async fn state(ctx: &Ctx, id: &str) -> Result<()> {
    let store = ctx.store().await?;
    let issue = require_issue(store, id).await?;
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "id": issue.id,
            "state": issue.status.as_str(),
            "category": format!("{:?}", issue.status.category()).to_lowercase(),
        }))?;
    } else {
        ctx.out.line(format!("{}: {}", issue.id, issue.status.as_str()));
    }
    Ok(())
}

/// Move an issue to a workflow state — a built-in status or a workspace-custom
/// one. `Status::from` resolves a known name (`open`, `in_progress`, …) and
/// carries anything else as [`Status::Custom`], so `bd set-state x reviewing`
/// works without the port having to know what "reviewing" means.
///
/// Goes through `update_issue`, so it emits the same `status_changed` event and
/// recomputes readiness exactly as `bd close`/`bd start` do — a custom state that
/// happens to be closed-ish still gates work correctly.
pub async fn set_state(ctx: &Ctx, id: &str, state: &str) -> Result<()> {
    ctx.ensure_writable("set an issue's state")?;
    let store = ctx.store().await?;
    require_issue(store, id).await?;

    let state = state.trim();
    if state.is_empty() {
        bail!("a state name cannot be empty");
    }
    let patch = IssuePatch {
        status: Some(Status::from(state.to_string())),
        ..Default::default()
    };
    let updated = store.update_issue(id, &patch).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "id": updated.id, "state": updated.status.as_str() }))?;
    } else {
        ctx.out
            .line(format!("{} is now {}", updated.id, updated.status.as_str()));
    }
    Ok(())
}

/// `bd promote`: a wisp becomes a real bead.
///
/// A wisp is an ordinary row with `ephemeral = 1` and a `wisp_type` that declares
/// its TTL. Promotion clears both, and it has to clear *both*: an ephemeral bead
/// is invisible to `bd ready`, and a bead that still names a wisp type still
/// carries that type's TTL — so a half-promotion yields either work nobody can
/// find or work `bd gc` deletes out from under whoever claimed it.
///
/// It is an update rather than a create-and-delete for the reason renames are:
/// the id is a foreign key, and minting a new one would cascade away every edge,
/// comment and label pointing at the old one.
pub async fn promote(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("promote an issue")?;
    let store = ctx.store().await?;
    let issue = require_issue(store, id).await?;

    // Not an error worth a special case, but not a silent success either: a
    // caller that promotes the wrong id should hear about it.
    if !issue.ephemeral && issue.wisp_type.is_none() {
        bail!("{id} is already a real bead; there is nothing to promote");
    }

    let promoted = store.update_issue(id, &IssuePatch::promote()).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&promoted, &[], &[], &[]))?;
    } else {
        ctx.out
            .line(format!("Promoted {id}; it is a real bead now, and claimable"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue() -> Issue {
        let mut i = Issue::new("t-1", "Fix the thing");
        i.description = "A body.\n\n---\n\n# A heading\nkey: not a header".to_string();
        i.priority = Priority::HIGH;
        i.issue_type = IssueType::Bug;
        i.assignee = "alice".to_string();
        i.labels = vec!["alpha".into(), "beta".into()];
        i
    }

    #[test]
    fn an_edit_buffer_round_trips_through_prose() {
        // The trap this guards: a description is prose, and prose contains `---`
        // rules, `#` headings and `key: value` lines. If any of those parsed as
        // structure, editing a description would corrupt it.
        let before = render_edit_doc(&issue());
        let doc = parse_edit_doc(&before).expect("our own buffer must parse");
        assert_eq!(doc.title.as_deref(), Some("Fix the thing"));
        assert_eq!(doc.priority, Some(Priority::HIGH));
        assert_eq!(doc.issue_type, Some(IssueType::Bug));
        assert_eq!(
            doc.labels,
            Some(vec!["alpha".to_string(), "beta".to_string()])
        );
        assert_eq!(doc.section("description"), issue().description);

        // And it is a fixpoint: an untouched buffer produces no patch at all.
        let (patch, add, remove) = doc.diff(&issue()).unwrap();
        assert!(patch.is_empty(), "an unedited buffer must not write: {patch:?}");
        assert!(add.is_empty() && remove.is_empty());
    }

    #[test]
    fn an_edit_applies_only_what_changed() {
        let src = "title: A better title\nstatus: in_progress\npriority: 0\ntype: bug\n\
                   assignee: \nlabels: beta, gamma\n\n--- description ---\nnew body\n";
        let doc = parse_edit_doc(src).unwrap();
        let (patch, add, remove) = doc.diff(&issue()).unwrap();

        assert_eq!(patch.title.as_deref(), Some("A better title"));
        assert_eq!(patch.status, Some(Status::InProgress));
        assert_eq!(patch.priority, Some(Priority::CRITICAL));
        // The type did not change, so it must not be in the patch.
        assert_eq!(patch.issue_type, None);
        // Emptied by hand, not deleted: that is a clear, not a keep.
        assert_eq!(patch.assignee, Field::Clear);
        assert_eq!(patch.description, Field::Set("new body".into()));
        // Deleted sections come back empty, which is a real edit.
        assert_eq!(patch.notes, Field::Keep);
        assert_eq!(add, vec!["gamma".to_string()]);
        assert_eq!(remove, vec!["alpha".to_string()]);
    }

    #[test]
    fn a_typo_in_the_header_is_an_error_not_a_shrug() {
        // Silently ignoring `titel:` would report success and change nothing.
        assert!(parse_edit_doc("titel: oops\n").is_err());
        assert!(parse_edit_doc("title: ok\npriority: urgent\n").is_err());
        assert!(parse_edit_doc("title: \n").unwrap().diff(&issue()).is_err());
    }

    #[test]
    fn association_edges_never_gate_work() {
        // If this ever fails, `bd duplicate` starts blocking the issue it marks.
        for t in [
            DependencyType::Duplicates,
            DependencyType::Supersedes,
            DependencyType::Related,
        ] {
            assert!(!t.affects_ready_work(), "{t} would gate `bd ready`");
        }
    }

    #[test]
    fn a_batch_reads_jsonl_an_array_and_a_lone_object() {
        let jsonl = "{\"op\":\"close\",\"id\":\"t-1\"}\n\n{\"op\":\"reopen\",\"id\":\"t-2\"}\n";
        let ops = parse_batch(jsonl).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[0],
            BatchOp::Close {
                id: "t-1".into(),
                reason: None
            }
        );

        let array = r#"[{"op":"note","id":"t-1","text":"hi"}]"#;
        assert_eq!(parse_batch(array).unwrap().len(), 1);

        let one = "{\n  \"op\": \"delete\",\n  \"id\": \"t-9\"\n}\n";
        assert_eq!(parse_batch(one).unwrap(), vec![BatchOp::Delete { id: "t-9".into() }]);
    }

    #[test]
    fn a_batch_rejects_a_misspelled_field() {
        // Not pedantry: `{"op":"close","id":"t-1","resaon":"done"}` would
        // otherwise close the issue with the wrong reason, and a
        // conditional-blocks dependent reads that reason to decide what runs.
        assert!(parse_batch(r#"{"op":"close","id":"t-1","resaon":"x"}"#).is_err());
        assert!(parse_batch(r#"{"op":"nonesuch","id":"t-1"}"#).is_err());
    }

    #[test]
    fn the_human_and_json_spellings_of_a_category_agree() {
        // `category_name` is hand-written; the domain's is derived. They are one
        // fact, and a rename on either side must not let them drift apart.
        for s in builtin_statuses() {
            assert_eq!(json!(s.category()), json!(category_name(s.category())));
        }
    }

    #[test]
    fn the_builtin_lists_are_exhaustive() {
        // These lists are hand-written because the domain offers no
        // `all_builtin()`. The matches below are what turn "someone added a
        // variant to bd-core" into a compile error here rather than a command
        // that quietly under-reports.
        for s in builtin_statuses() {
            match s {
                Status::Open
                | Status::InProgress
                | Status::Blocked
                | Status::Deferred
                | Status::Closed
                | Status::Pinned
                | Status::Hooked
                | Status::Custom(_) => {}
            }
        }
        for t in builtin_types() {
            match t {
                IssueType::Bug
                | IssueType::Feature
                | IssueType::Task
                | IssueType::Epic
                | IssueType::Chore
                | IssueType::Decision
                | IssueType::Message
                | IssueType::Molecule
                | IssueType::Gate
                | IssueType::Spike
                | IssueType::Story
                | IssueType::Milestone
                | IssueType::Event
                | IssueType::Custom(_) => {}
            }
        }
        assert_eq!(builtin_statuses().len(), 7);
        assert_eq!(builtin_types().len(), 13);
    }
}
