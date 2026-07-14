//! Reading the workspace: what exists, what is claimable, what is stuck.

use anyhow::Result;
use bd_core::{Issue, IssueFilter, Status};
use serde_json::json;

use crate::cli::{BlockedArgs, CountArgs, FilterArgs, ListArgs, QueryArgs, ReadyArgs, SearchArgs};
use crate::context::Ctx;

/// The statuses `bd list` shows when you do not ask for any.
///
/// There is no "not closed" predicate on [`IssueFilter`], only a set of
/// statuses, so "everything but closed" has to be spelled out. The cost: a
/// workspace's *custom* statuses are not in the default view. Ask for them by
/// name, or pass `--all`.
const OPEN_STATUSES: [Status; 6] = [
    Status::Open,
    Status::InProgress,
    Status::Blocked,
    Status::Deferred,
    Status::Pinned,
    Status::Hooked,
];

fn apply(filter: &mut IssueFilter, f: &FilterArgs) {
    filter.priority = f.priority;
    filter.min_priority = f.min_priority;
    filter.issue_type = f.issue_type.clone();
    filter.assignee = f.assignee.clone();
    filter.labels_all = f.labels.clone();
}

/// 0 means "no limit", which is not the same as "limit of zero".
fn limit(n: u32) -> Option<u32> {
    (n > 0).then_some(n)
}

pub async fn list(ctx: &Ctx, a: ListArgs) -> Result<()> {
    let store = ctx.store().await?;
    let mut f = IssueFilter::new();
    apply(&mut f, &a.filter);
    if !a.status.is_empty() {
        f.statuses = a.status.clone();
    } else if !a.all {
        f.statuses = OPEN_STATUSES.to_vec();
    }
    f.limit = limit(a.limit);
    f.offset = a.offset;
    f.sort = a.sort.unwrap_or_default();

    let issues = store.list_issues(&f).await?;
    ctx.out.issues(&issues)
}

pub async fn ready(ctx: &Ctx, a: ReadyArgs) -> Result<()> {
    let store = ctx.store().await?;
    let mut f = IssueFilter::ready();
    apply(&mut f, &a.filter);
    f.limit = limit(a.limit);
    f.sort = a.sort.unwrap_or_default();

    let issues = store.ready_work(&f).await?;
    ctx.out.issues(&issues)
}

pub async fn blocked(ctx: &Ctx, a: BlockedArgs) -> Result<()> {
    let store = ctx.store().await?;
    let mut f = IssueFilter::blocked();
    apply(&mut f, &a.filter);
    f.limit = limit(a.limit);

    let issues = store.blocked_work(&f).await?;
    ctx.out.issues(&issues)
}

pub async fn search(ctx: &Ctx, a: SearchArgs) -> Result<()> {
    let store = ctx.store().await?;
    let mut f = IssueFilter::new();
    apply(&mut f, &a.filter);
    f.text = Some(a.text.clone());
    if !a.all {
        f.statuses = OPEN_STATUSES.to_vec();
    }
    f.limit = limit(a.limit);

    let issues = store.list_issues(&f).await?;
    ctx.out.issues(&issues)
}

pub async fn query(ctx: &Ctx, a: QueryArgs) -> Result<()> {
    let store = ctx.store().await?;
    let q = bd_query::parse(&a.expr)?;

    let issues: Vec<Issue> = match q.as_filter() {
        // Fully expressible as SQL: let the database do all of it.
        Some(mut f) => {
            f.limit = limit(a.limit);
            store.list_issues(&f).await?
        }
        // Not expressible: shrink the candidate set in SQL with the hint (which
        // is never narrower than the query), then finish the job in memory. The
        // limit must be applied *after* matching, or it would truncate
        // candidates that the predicate would have rejected anyway.
        None => {
            let hint = q.filter_hint();
            let candidates = store.list_issues(&hint).await?;
            let mut kept: Vec<Issue> = candidates.into_iter().filter(|i| q.matches(i)).collect();
            if let Some(n) = limit(a.limit) {
                kept.truncate(n as usize);
            }
            kept
        }
    };
    ctx.out.issues(&issues)
}

pub async fn count(ctx: &Ctx, a: CountArgs) -> Result<()> {
    let store = ctx.store().await?;
    let mut f = IssueFilter::new();
    apply(&mut f, &a.filter);
    if !a.status.is_empty() {
        f.statuses = a.status.clone();
    } else if !a.all {
        f.statuses = OPEN_STATUSES.to_vec();
    }

    let n = store.count_issues(&f).await?;
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "count": n }))?;
    } else {
        println!("{n}");
    }
    Ok(())
}

pub async fn status(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let s = store.stats().await?;
    if ctx.out.is_json() {
        return ctx.out.json_value(&s);
    }

    ctx.out.line(format!("{} issues", s.total));
    ctx.out.line(format!(
        "  {} open  {} in progress  {} blocked  {} closed",
        s.open, s.in_progress, s.blocked, s.closed
    ));
    // The number an agent actually came for.
    ctx.out.line(format!("  {} ready to work", s.ready));
    if !s.by_priority.is_empty() {
        let by: Vec<String> = s
            .by_priority
            .iter()
            .map(|(p, n)| format!("P{p}: {n}"))
            .collect();
        ctx.out.line(format!("  {}", by.join("  ")));
    }
    if !s.by_type.is_empty() {
        let by: Vec<String> = s.by_type.iter().map(|(t, n)| format!("{t}: {n}")).collect();
        ctx.out.line(format!("  {}", by.join("  ")));
    }
    Ok(())
}

/// An issue's audit trail. Deliberately *not* a capability command: events are
/// core storage, so this works on every backend. `bd diff` is the one that
/// needs a commit graph.
pub async fn history(ctx: &Ctx, id: &str) -> Result<()> {
    let store = ctx.store().await?;
    let events = store.list_events(id).await?;
    ctx.out.events(&events)
}

/// Diffing two refs is time travel, which is [`HistoryViewer`]'s job — and a
/// backend without a commit graph has no refs to diff.
///
/// [`HistoryViewer`]: bd_storage::HistoryViewer
pub fn diff(ctx: &Ctx, _from: &str, _to: &str) -> Result<()> {
    crate::commands::require_cap(ctx, "diff", crate::commands::Cap::History)?;
    crate::commands::stub("diff", ctx)
}

/// Where the workspace is, and who beads thinks you are. Needs no database,
/// which is exactly why it is useful when the database is the problem.
pub fn where_(ctx: &Ctx) -> Result<()> {
    let loc = ctx.locator()?;
    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "beads_dir": loc.dir,
            "db": loc.db_path(),
            "backend": loc.backend.as_str(),
            "workspace_id": loc.workspace_id,
            "actor": ctx.identity.actor,
        }));
    }
    ctx.out.line(format!("workspace: {}", loc.dir.display()));
    ctx.out.line(format!("database:  {}", loc.db_path().display()));
    ctx.out.line(format!("backend:   {}", loc.backend));
    ctx.out.line(format!("actor:     {}", ctx.identity.actor));
    Ok(())
}
