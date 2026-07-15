//! `bd remember` / `recall` / `forget` / `memories` — durable agent notes.
//!
//! Memory is a small store of things an agent wants to carry across sessions: a
//! decision, a gotcha, a pointer. `remember` writes one, `recall <query>`
//! searches, `memories` lists, `forget <id>` deletes.
//!
//! `todo` and `human` are the other two small families that build on the graph:
//! `todo` is a lightweight personal checklist, and `human` is the queue of items
//! escalated for a person to answer. All three are grouped here because none is
//! large enough for its own file and they share the shape — a typed or labelled
//! bead, created and queried through the existing seam.
//!
//! # Storage substrate
//!
//! Upstream keeps memories in the config key/value table (`kv.memory.*`). That
//! path is closed here on purpose: the seam has no `delete_config`, so `forget`
//! would have nothing to call (it is the same wall `bd kv clear` hit), and config
//! keys would leak into `bd config list`. So all three families are **issues**,
//! reached through the storage seam that already exists.
//!
//! The one property that must hold (and is tested): none of these may show up in
//! `bd ready` or the default `bd list`. An agent's private note is not claimable
//! work. Two levers together guarantee it, and each is load-bearing:
//!
//! * A dedicated **issue type** (`memory`, `todo`) is the discriminator every
//!   query keys on, and keeps them out of `bd ready` (its excluded-type set) —
//!   but a type alone does *not* hide from `bd list`, which filters on status.
//! * A **custom status** (`memory`, `todo`) is what actually hides them from the
//!   default `bd list`: that view shows the open-ish built-in statuses only, and
//!   a custom status is in none of them. It also keeps them out of `bd ready`,
//!   which admits only `open`/`in_progress`. This is the term the test pins.
//!
//! `human` follows upstream and keys on the **`human` label** instead: it is a
//! *consumer* of a queue nothing in this port yet fills, so it must find whatever
//! a person (or an import) tagged, rather than minting its own hidden shape.

use anyhow::{Result, bail};
use bd_core::types::MAX_TITLE_LEN;
use bd_core::{Issue, IssueFilter, IssueType, Status};
use bd_storage::Storage;
use serde_json::json;

use crate::cli::{HumanCmd, TodoCmd};
use crate::context::Ctx;
use crate::output::issue_json;

/// The discriminator *and* the hider for memories. The type is what `recall`,
/// `memories` and `forget` query on; the status is what keeps a memory out of
/// `bd ready` and the default `bd list`.
const MEMORY_KIND: &str = "memory";
/// Likewise for todos. `todo done` moves the status to `closed`, which is still
/// outside the default list view — a finished todo is not backlog.
const TODO_KIND: &str = "todo";
/// The label that marks a bead as needing a person. Matches upstream so an
/// imported or hand-tagged queue is recognized.
const HUMAN_LABEL: &str = "human";

fn memory_type() -> IssueType {
    IssueType::Custom(MEMORY_KIND.to_string())
}

fn todo_type() -> IssueType {
    IssueType::Custom(TODO_KIND.to_string())
}

/// One-line, length-bounded rendering of a bead's text for a status message.
fn short(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let keep: String = one_line.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// The full text of a memory: its description when the note outran a title,
/// otherwise the title itself.
fn memory_text(i: &Issue) -> &str {
    if i.description.is_empty() {
        &i.title
    } else {
        &i.description
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

pub async fn remember(ctx: &Ctx, text: &[String]) -> Result<()> {
    ctx.ensure_writable("remember something")?;
    let store = ctx.store().await?;

    let text = text.join(" ");
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to remember");
    }

    let prefix = ctx.prefix().await;
    let id = store.next_id(&prefix, text, "").await?;

    let mut issue = Issue::new(&id, "");
    // A memory that outruns a title keeps its full text in the description, where
    // `recall`'s substring search still reaches it; the title carries a bounded
    // prefix so the row is valid and lists cleanly.
    if text.chars().count() > MAX_TITLE_LEN {
        issue.title = text.chars().take(MAX_TITLE_LEN).collect();
        issue.description = text.to_string();
    } else {
        issue.title = text.to_string();
    }
    issue.issue_type = memory_type();
    issue.status = Status::Custom(MEMORY_KIND.to_string());
    issue.created_by = ctx.identity.actor.clone();

    let created = store.create_issue(&issue).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&created, &[], &[], &[]))?;
    } else {
        ctx.out
            .line(format!("Remembered {}: {}", created.id, short(text, 80)));
    }
    Ok(())
}

/// List every memory, or only those whose text matches `query`.
async fn list_memories(store: &dyn Storage, query: Option<&str>) -> Result<Vec<Issue>> {
    let mut f = IssueFilter::new();
    f.issue_type = Some(memory_type());
    if let Some(q) = query {
        // Same case-insensitive substring search `bd search` uses, over
        // title/description — which for a memory is exactly its content.
        f.text = Some(q.to_string());
    }
    Ok(store.list_issues(&f).await?)
}

pub async fn recall(ctx: &Ctx, text: &[String]) -> Result<()> {
    let store = ctx.store().await?;
    let query = text.join(" ");
    let query = query.trim();
    if query.is_empty() {
        bail!("nothing to recall");
    }
    let memories = list_memories(store, Some(query)).await?;
    ctx.out.issues(&memories)
}

pub async fn memories(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let memories = list_memories(store, None).await?;
    ctx.out.issues(&memories)
}

pub async fn forget(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("forget a memory")?;
    let store = ctx.store().await?;

    // Only a memory may be forgotten. Routing `forget` at an ordinary bead must
    // never quietly delete real work, so the type is checked before the delete.
    let issue = store
        .get_issue(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no memory with id {id}"))?;
    if issue.issue_type != memory_type() {
        bail!("{id} is not a memory (it is a {}); refusing to delete it", issue.issue_type);
    }

    store.delete_issue(id).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "id": id, "forgotten": true }))?;
    } else {
        ctx.out
            .line(format!("Forgot {id}: {}", short(memory_text(&issue), 80)));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Todo
// ---------------------------------------------------------------------------

pub async fn todo(ctx: &Ctx, cmd: TodoCmd) -> Result<()> {
    match cmd {
        TodoCmd::Add { text } => todo_add(ctx, &text).await,
        TodoCmd::List => todo_list(ctx).await,
        TodoCmd::Done { id } => todo_done(ctx, &id).await,
    }
}

async fn todo_add(ctx: &Ctx, text: &[String]) -> Result<()> {
    ctx.ensure_writable("add a todo")?;
    let store = ctx.store().await?;

    let title = text.join(" ");
    let title = title.trim();
    if title.is_empty() {
        bail!("a todo needs a title");
    }
    if title.chars().count() > MAX_TITLE_LEN {
        bail!("a todo title cannot exceed {MAX_TITLE_LEN} characters");
    }

    let prefix = ctx.prefix().await;
    let id = store.next_id(&prefix, title, "").await?;

    let mut issue = Issue::new(&id, title);
    issue.issue_type = todo_type();
    issue.status = Status::Custom(TODO_KIND.to_string());
    issue.created_by = ctx.identity.actor.clone();

    let created = store.create_issue(&issue).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&created, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Added todo {}: {}", created.id, title));
    }
    Ok(())
}

async fn todo_list(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let mut f = IssueFilter::new();
    f.issue_type = Some(todo_type());
    // Open todos only: a completed one is closed, and a checklist that keeps
    // showing finished items is a checklist nobody trusts.
    f.exclude_statuses = vec![Status::Closed];
    let todos = store.list_issues(&f).await?;
    ctx.out.issues(&todos)
}

async fn todo_done(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("complete a todo")?;
    let store = ctx.store().await?;

    // As with `forget`: `todo done` must not double as a way to close arbitrary
    // beads, so the type is verified before the close.
    let issue = store
        .get_issue(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no todo with id {id}"))?;
    if issue.issue_type != todo_type() {
        bail!("{id} is not a todo (it is a {}); use `bd close` for a real bead", issue.issue_type);
    }
    if issue.status.is_closed() {
        bail!("{id} is already done");
    }

    let closed = store.close_issue(id, "Completed").await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&closed, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Completed {id}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Human — the queue of beads escalated for a person
// ---------------------------------------------------------------------------
//
// This is a *consumer*. Nothing in this port creates a human-queue item yet —
// `bd mail` shells out to an external provider rather than filing a bead, and no
// gate or swarm path tags one — so in practice the queue is whatever a person or
// an import labelled `human`. `list`/`stats` are therefore honestly empty on a
// fresh workspace; `respond`/`dismiss` act on an id a caller already has, so they
// are real regardless of who filled the queue.

pub async fn human(ctx: &Ctx, cmd: HumanCmd) -> Result<()> {
    match cmd {
        HumanCmd::List => human_list(ctx).await,
        HumanCmd::Respond { id, text } => human_respond(ctx, &id, &text).await,
        HumanCmd::Dismiss { id } => human_dismiss(ctx, &id).await,
        HumanCmd::Stats => human_stats(ctx).await,
    }
}

fn human_filter() -> IssueFilter {
    let mut f = IssueFilter::new();
    f.labels_all = vec![HUMAN_LABEL.to_string()];
    f
}

async fn human_list(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let mut f = human_filter();
    // The queue is what is still waiting; a bead already answered or dismissed is
    // closed, and belongs to `human stats`, not the pending list.
    f.exclude_statuses = vec![Status::Closed];
    let items = store.list_issues(&f).await?;
    ctx.out.issues(&items)
}

/// Shared by `respond` and `dismiss`: fetch the bead, refuse a closed one, and
/// warn (not fail) when it was never actually in the human queue.
async fn human_target(ctx: &Ctx, store: &dyn Storage, id: &str) -> Result<Issue> {
    let issue = store
        .get_issue(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("issue not found: {id}"))?;
    if issue.status.is_closed() {
        bail!("{id} is already closed");
    }
    if !issue.labels.iter().any(|l| l == HUMAN_LABEL) {
        // A warning, not an error: acting on a bead that was not formally in the
        // queue is unusual but legitimate, and refusing it would be its own trap.
        ctx.out
            .warn(format!("{id} is not labelled `{HUMAN_LABEL}`; responding anyway"));
    }
    Ok(issue)
}

async fn human_respond(ctx: &Ctx, id: &str, text: &[String]) -> Result<()> {
    ctx.ensure_writable("respond to a human-queue item")?;
    let store = ctx.store().await?;

    let response = text.join(" ");
    let response = response.trim();
    if response.is_empty() {
        bail!("a response cannot be empty");
    }

    human_target(ctx, store, id).await?;

    // The answer is recorded as a comment — a first-class, attributed record —
    // and then the bead is closed with a reason `human stats` can read back.
    store.add_comment(id, &format!("Response: {response}")).await?;
    let closed = store.close_issue(id, "Responded").await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&closed, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Responded to {id} and closed it"));
    }
    Ok(())
}

async fn human_dismiss(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("dismiss a human-queue item")?;
    let store = ctx.store().await?;

    human_target(ctx, store, id).await?;

    // "Dismissed" so `human stats` can tell an answered bead from a discarded one
    // by its close reason.
    let closed = store.close_issue(id, "Dismissed").await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&closed, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!("Dismissed {id}"));
    }
    Ok(())
}

async fn human_stats(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    // Every human bead, in every state — the counts below partition it.
    let items = store.list_issues(&human_filter()).await?;

    let total = items.len();
    let mut pending = 0usize;
    let mut dismissed = 0usize;
    let mut responded = 0usize;
    for i in &items {
        if !i.status.is_closed() {
            pending += 1;
        } else if i.close_reason.to_lowercase().contains("dismiss") {
            dismissed += 1;
        } else {
            responded += 1;
        }
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "total": total,
            "pending": pending,
            "responded": responded,
            "dismissed": dismissed,
        }))?;
    } else {
        ctx.out.line(format!("Human queue: {total} total"));
        ctx.out.line(format!("  {pending} pending"));
        ctx.out.line(format!("  {responded} responded"));
        ctx.out.line(format!("  {dismissed} dismissed"));
    }
    Ok(())
}
