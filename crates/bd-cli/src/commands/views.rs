//! Reading the workspace: what exists, what is claimable, what is stuck.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use bd_core::{Dependency, DependencyType, Issue, IssueFilter, IssueType, SortPolicy, Status};
use bd_storage::Storage;
use chrono::{Duration, Utc};
use serde_json::{Value, json};

use crate::cli::{
    AuditCmd, BlockedArgs, CountArgs, EpicCmd, FilterArgs, KvCmd, ListArgs, QueryArgs, ReadyArgs,
    SearchArgs,
};
use crate::commands::{Cap, require_cap, stub};
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
pub async fn diff(ctx: &Ctx, _from: &str, _to: &str) -> Result<()> {
    require_cap(ctx, "diff", Cap::History)?;
    stub("diff", ctx)
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

// ---------------------------------------------------------------------------
// Shared machinery
//
// Three shapes of query keep recurring below, and each one is a trap if written
// the obvious way.
// ---------------------------------------------------------------------------

/// Every issue, keyed by id, in one query.
///
/// `IssueFilter` cannot name a set of ids, so a command that has ids and wants
/// issues must choose between one `get_issue` per id and one scan indexed in
/// memory. Anything whose id set is unbounded — an epic's children, both ends of
/// every edge — takes the scan. A `get_issue` loop there is an N+1 over the
/// whole workspace.
async fn index_all(store: &dyn Storage) -> Result<HashMap<String, Issue>> {
    let all = store.list_issues(&IssueFilter::new()).await?;
    Ok(all.into_iter().map(|i| (i.id.clone(), i)).collect())
}

/// Every edge in the graph.
///
/// The seam exposes edges one issue at a time, so this is one query per issue.
/// Out-edges only, deliberately: every edge is *some* issue's out-edge, so
/// walking all issues' out-edges enumerates the graph exactly once — asking for
/// dependents too would double the query count and return each edge twice.
///
/// This is why `lint` and `orphans` are batch commands and not something you run
/// in a loop. A `list_dependencies(&IssueFilter)` on the seam would collapse the
/// whole thing to one query.
async fn all_edges(store: &dyn Storage, ids: &[String]) -> Result<Vec<Dependency>> {
    let mut edges = Vec::new();
    for id in ids {
        edges.extend(store.dependencies_of(id).await?);
    }
    Ok(edges)
}

/// Attach labels to issues that came from `list_issues`, which does not hydrate
/// relations. One batched query for the whole set — `get_issue` per issue would
/// be an N+1 and is exactly what [`Storage::labels_of`] exists to prevent.
async fn hydrate_labels(store: &dyn Storage, issues: &mut [Issue]) -> Result<()> {
    if issues.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
    let by_id: HashMap<String, Vec<String>> = store.labels_of(&ids).await?.into_iter().collect();
    for i in issues.iter_mut() {
        if let Some(labels) = by_id.get(&i.id) {
            i.labels.clone_from(labels);
        }
    }
    Ok(())
}

/// The children of `id`: a child holds the edge, so they are its *dependents*.
///
/// `child --parent-child--> parent`. Reading this backwards gives you the
/// parent's siblings and no error message.
async fn child_ids(store: &dyn Storage, id: &str) -> Result<Vec<String>> {
    Ok(store
        .dependents_of(id)
        .await?
        .into_iter()
        .filter(|d| d.dep_type == DependencyType::ParentChild)
        .map(|d| d.issue_id)
        .collect())
}

/// Whether this issue still gates whatever depends on it — the `LIVE` predicate
/// the blocked-cache engine uses, restated. Pinned-ness counts both ways it can
/// be spelled.
fn is_live(i: &Issue) -> bool {
    !i.status.is_closed() && i.status != Status::Pinned && !i.pinned
}

fn by_priority_then_id(a: &Issue, b: &Issue) -> std::cmp::Ordering {
    a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id))
}

// ---------------------------------------------------------------------------
// Structure: children and epics
// ---------------------------------------------------------------------------

pub async fn children(ctx: &Ctx, id: &str) -> Result<()> {
    let store = ctx.store().await?;
    if store.get_issue(id).await?.is_none() {
        bail!("issue not found: {id}");
    }

    // A `get_issue` loop, and on purpose: the id set is the children of one
    // issue — small and already in hand — so a full-table scan to index it would
    // cost more than the loop it saved.
    let mut kids = Vec::new();
    for child in child_ids(store, id).await? {
        if let Some(i) = store.get_issue(&child).await? {
            kids.push(i);
        }
    }
    kids.sort_by(by_priority_then_id);
    ctx.out.issues(&kids)
}

pub async fn epic(ctx: &Ctx, cmd: EpicCmd) -> Result<()> {
    match cmd {
        EpicCmd::Status => epic_status(ctx).await,
        EpicCmd::CloseEligible => epic_close_eligible(ctx).await,
    }
}

/// Every open epic with its children, resolved.
///
/// A child edge pointing at an issue that does not exist is dropped here rather
/// than counted — a phantom child would quietly hold an epic open forever. `bd
/// lint` reports those edges by name.
async fn epic_rollup(store: &dyn Storage) -> Result<Vec<(Issue, Vec<Issue>)>> {
    let mut f = IssueFilter::new();
    f.issue_type = Some(IssueType::Epic);
    // A closed epic's progress is not news, it is history.
    f.exclude_statuses = vec![Status::Closed];
    let mut epics = store.list_issues(&f).await?;
    if epics.is_empty() {
        return Ok(Vec::new());
    }
    epics.sort_by(by_priority_then_id);

    let index = index_all(store).await?;
    let mut out = Vec::new();
    for e in epics {
        let mut kids: Vec<Issue> = child_ids(store, &e.id)
            .await?
            .iter()
            .filter_map(|c| index.get(c).cloned())
            .collect();
        kids.sort_by(by_priority_then_id);
        out.push((e, kids));
    }
    Ok(out)
}

fn percent_complete(closed: usize, total: usize) -> u32 {
    if total == 0 {
        return 0;
    }
    ((closed * 100) / total) as u32
}

async fn epic_status(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let rollup = epic_rollup(store).await?;

    if ctx.out.is_json() {
        let docs: Vec<Value> = rollup
            .iter()
            .map(|(e, kids)| {
                let closed = kids.iter().filter(|k| k.status.is_closed()).count();
                let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
                if let Some(o) = v.as_object_mut() {
                    // Beside the issue's own fields, never wrapping them: an agent
                    // that already parses `bd list --json` parses this unchanged.
                    let ids: Vec<&String> = kids.iter().map(|k| &k.id).collect();
                    o.insert("children".into(), json!(ids));
                    o.insert("children_total".into(), json!(kids.len()));
                    o.insert("children_closed".into(), json!(closed));
                    o.insert(
                        "percent_complete".into(),
                        json!(percent_complete(closed, kids.len())),
                    );
                }
                v
            })
            .collect();
        return ctx.out.json_value(&docs);
    }

    if rollup.is_empty() {
        ctx.out.line("No open epics.");
        return Ok(());
    }
    for (e, kids) in &rollup {
        let closed = kids.iter().filter(|k| k.status.is_closed()).count();
        ctx.out.line(format!(
            "{}  {:>3}%  {}/{} closed  {}",
            e.id,
            percent_complete(closed, kids.len()),
            closed,
            kids.len(),
            e.title
        ));
        // Only what is left: a rollup that lists the finished work too is a
        // status report nobody can skim.
        for k in kids.iter().filter(|k| !k.status.is_closed()) {
            ctx.out.line(format!(
                "    {}  P{}  {:<12}  {}",
                k.id,
                k.priority.0,
                k.status.as_str(),
                k.title
            ));
        }
        if kids.is_empty() {
            ctx.out.line("    (no children)");
        }
    }
    Ok(())
}

async fn epic_close_eligible(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let rollup = epic_rollup(store).await?;

    // An epic with no children is not "done", it is unstarted. Closing it would
    // be a lie about work that was never planned.
    let eligible: Vec<Issue> = rollup
        .into_iter()
        .filter(|(_, kids)| !kids.is_empty() && kids.iter().all(|k| k.status.is_closed()))
        .map(|(e, _)| e)
        .collect();

    if !ctx.out.is_json() && eligible.is_empty() {
        ctx.out.line("No epics are ready to close.");
        return Ok(());
    }
    ctx.out.issues(&eligible)
}

// ---------------------------------------------------------------------------
// Hygiene: stale, orphans, duplicates, lint
// ---------------------------------------------------------------------------

pub async fn stale(ctx: &Ctx, older_than: &str) -> Result<()> {
    let store = ctx.store().await?;
    let d = crate::parse::duration(older_than).map_err(|e| anyhow!("--older-than: {e}"))?;
    let cutoff = Utc::now() - d;

    let mut f = IssueFilter::new();
    // Closed issues are *supposed* to sit untouched. Only live work goes stale.
    f.statuses = OPEN_STATUSES.to_vec();
    f.updated_before = Some(cutoff);
    let mut issues = store.list_issues(&f).await?;

    // `SortPolicy::Oldest` orders by *created_at*, which is not what staleness
    // means — an issue filed last year and touched this morning is not stale.
    // Sorting here is safe only because this command applies no LIMIT: a limit
    // pushed down under one order and re-sorted under another returns the wrong
    // page.
    issues.sort_by_key(|i| i.updated_at);

    ctx.out.line(format!(
        "Untouched since {} ({older_than}):",
        cutoff.format("%Y-%m-%d %H:%M")
    ));
    ctx.out.issues(&issues)
}

pub async fn orphans(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let index = index_all(store).await?;
    let mut ids: Vec<String> = index.keys().cloned().collect();
    ids.sort();

    // Every issue, including closed ones: an edge from a closed issue still
    // connects the issue on the other end of it.
    let edges = all_edges(store, &ids).await?;
    let mut connected: HashSet<&str> = HashSet::new();
    for e in &edges {
        connected.insert(e.issue_id.as_str());
        connected.insert(e.depends_on_id.as_str());
    }

    // Closed and unconnected is not a loose end, it is just finished work.
    let mut orphans: Vec<Issue> = index
        .values()
        .filter(|i| !i.status.is_closed() && !connected.contains(i.id.as_str()))
        .cloned()
        .collect();
    orphans.sort_by(by_priority_then_id);
    ctx.out.issues(&orphans)
}

/// Titles that differ only in case, spacing, or punctuation are the same title
/// to a person: "Fix the parser!" and "fix   the  parser".
///
/// This is the *fuzzy* half of duplicate detection and it is deliberately dumb —
/// no edit distance, no stemming, no similarity threshold. Every match it makes
/// can be explained in one sentence, which is the property that matters when the
/// output is a list of things a human is about to merge.
fn normalize_title(t: &str) -> String {
    let mut s = String::with_capacity(t.len());
    for c in t.chars() {
        if c.is_alphanumeric() {
            s.extend(c.to_lowercase());
        } else if !s.ends_with(' ') && !s.is_empty() {
            s.push(' ');
        }
    }
    s.trim_end().to_string()
}

/// The candidate pool for both duplicate commands: live issues, with labels
/// hydrated because the content hash covers them.
async fn dup_candidates(store: &dyn Storage) -> Result<Vec<Issue>> {
    let mut f = IssueFilter::new();
    // Closed work is not a duplicate of anything; it is the thing that got done.
    f.exclude_statuses = vec![Status::Closed];
    let mut issues = store.list_issues(&f).await?;
    hydrate_labels(store, &mut issues).await?;
    Ok(issues)
}

/// The content hash, computed — never the stored `content_hash` column.
///
/// That column is written at create and never refreshed on update, so for any
/// issue that was ever edited it is a hash of content the issue no longer has.
/// Comparing two of them would report a duplicate that stopped being one months
/// ago, and miss the one that became one yesterday.
fn content_key(i: &Issue) -> String {
    i.compute_content_hash()
}

pub async fn duplicates(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let issues = dup_candidates(store).await?;

    // Grouped by normalized title, not by content hash — identical content
    // implies an identical title, so the title groups are a superset and one
    // pass finds both kinds. Exactness is then reported per group rather than
    // silently assumed.
    let mut by_title: BTreeMap<String, Vec<Issue>> = BTreeMap::new();
    for i in issues {
        by_title.entry(normalize_title(&i.title)).or_default().push(i);
    }
    let mut groups: Vec<(String, Vec<Issue>)> = by_title
        .into_iter()
        .filter(|(t, v)| !t.is_empty() && v.len() > 1)
        .collect();
    for (_, v) in groups.iter_mut() {
        v.sort_by(by_priority_then_id);
    }

    let identical = |v: &[Issue]| {
        let first = content_key(&v[0]);
        v.iter().all(|i| content_key(i) == first)
    };

    if ctx.out.is_json() {
        let docs: Vec<Value> = groups
            .iter()
            .map(|(title, v)| {
                json!({
                    "title": title,
                    "identical_content": identical(v),
                    "issues": v,
                })
            })
            .collect();
        return ctx.out.json_value(&json!({ "groups": docs }));
    }

    if groups.is_empty() {
        ctx.out.line("No duplicate candidates.");
        return Ok(());
    }
    ctx.out
        .line(format!("{} possible duplicate group(s):", groups.len()));
    for (title, v) in &groups {
        let how = if identical(v) {
            "identical content"
        } else {
            "same title"
        };
        ctx.out.line(format!("\n  \"{title}\"  ({how})"));
        for i in v {
            ctx.out.line(format!(
                "    {}  P{}  {:<12}  {}",
                i.id,
                i.priority.0,
                i.status.as_str(),
                i.title
            ));
        }
    }
    Ok(())
}

pub async fn find_duplicates(ctx: &Ctx, id: &str) -> Result<()> {
    let store = ctx.store().await?;
    // `get_issue` hydrates labels and out-edges, both of which this needs.
    let target = store
        .get_issue(id)
        .await?
        .ok_or_else(|| anyhow!("issue not found: {id}"))?;

    // Already declared, in either direction: `a --duplicates--> b` is a fact
    // somebody recorded, not a guess.
    let mut linked_ids: Vec<String> = target
        .dependencies
        .iter()
        .filter(|d| d.dep_type == DependencyType::Duplicates)
        .map(|d| d.depends_on_id.clone())
        .collect();
    linked_ids.extend(
        store
            .dependents_of(id)
            .await?
            .into_iter()
            .filter(|d| d.dep_type == DependencyType::Duplicates)
            .map(|d| d.issue_id),
    );
    linked_ids.sort();
    linked_ids.dedup();

    let mut linked = Vec::new();
    for l in &linked_ids {
        if let Some(i) = store.get_issue(l).await? {
            linked.push(i);
        }
    }

    let known: HashSet<&String> = linked_ids.iter().collect();
    let target_hash = content_key(&target);
    let target_title = normalize_title(&target.title);

    // (issue, how it matched). "content" is exact; "title" is the heuristic.
    let mut candidates: Vec<(Issue, &'static str)> = Vec::new();
    for i in dup_candidates(store).await? {
        if i.id == target.id || known.contains(&i.id) {
            continue;
        }
        let how = if content_key(&i) == target_hash {
            "content"
        } else if !target_title.is_empty() && normalize_title(&i.title) == target_title {
            "title"
        } else {
            continue;
        };
        candidates.push((i, how));
    }
    // Exact matches first; they are the ones worth acting on without reading.
    candidates.sort_by(|(a, ha), (b, hb)| ha.cmp(hb).then_with(|| by_priority_then_id(a, b)));

    if ctx.out.is_json() {
        let docs: Vec<Value> = candidates
            .iter()
            .map(|(i, how)| {
                let mut v = serde_json::to_value(i).unwrap_or(Value::Null);
                if let Some(o) = v.as_object_mut() {
                    o.insert("match".into(), json!(how));
                }
                v
            })
            .collect();
        return ctx.out.json_value(&json!({
            "id": target.id,
            "linked": linked,
            "candidates": docs,
        }));
    }

    if !linked.is_empty() {
        ctx.out.line(format!("{} is already linked as a duplicate:", target.id));
        for i in &linked {
            ctx.out.line(format!("  {}  {}", i.id, i.title));
        }
    }
    if candidates.is_empty() {
        ctx.out.line(format!("No duplicate candidates for {}.", target.id));
        return Ok(());
    }
    ctx.out
        .line(format!("{} candidate(s) for {}:", candidates.len(), target.id));
    for (i, how) in &candidates {
        let how = if *how == "content" {
            "identical content"
        } else {
            "same title"
        };
        ctx.out.line(format!(
            "  {}  P{}  {:<12}  [{how}]  {}",
            i.id,
            i.priority.0,
            i.status.as_str(),
            i.title
        ));
    }
    Ok(())
}

/// Problems `bd lint` looks for. Each one is a graph fact, not an opinion: a
/// lint that flags style makes a lint nobody runs twice.
pub async fn lint(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;

    let index = index_all(store).await?;
    let mut ids: Vec<String> = index.keys().cloned().collect();
    ids.sort(); // HashMap order is not an order; the report must be stable.
    let edges = all_edges(store, &ids).await?;

    let mut out_edges: HashMap<&str, Vec<&Dependency>> = HashMap::new();
    let mut child_count: HashMap<&str, usize> = HashMap::new();
    for e in &edges {
        out_edges.entry(e.issue_id.as_str()).or_default().push(e);
        if e.dep_type == DependencyType::ParentChild {
            *child_count.entry(e.depends_on_id.as_str()).or_default() += 1;
        }
    }

    let mut problems: Vec<Value> = Vec::new();

    for c in store.find_cycles().await? {
        problems.push(json!({
            "kind": "cycle",
            "issues": c,
            "detail": format!("dependency cycle: {}", c.join(" -> ")),
        }));
    }

    // An edge to an id that is not there. The source is always present (these
    // edges came from walking the issue table), so only the target can dangle.
    for e in &edges {
        if !index.contains_key(&e.depends_on_id) {
            problems.push(json!({
                "kind": "dangling_edge",
                "id": e.issue_id,
                "depends_on_id": e.depends_on_id,
                "type": e.dep_type.as_str(),
                "detail": format!(
                    "{} has a {} edge to {}, which does not exist",
                    e.issue_id, e.dep_type, e.depends_on_id
                ),
            }));
        }
    }

    // A `conditional-blocks` edge means "run me only if the target *fails*". If
    // the target closed successfully the failure path is moot, and the store
    // leaves the issue blocked forever rather than closing a bead nobody asked
    // it to close. That is by design and it is also a bead that will never move
    // again unless a human reaps it — which is precisely a lint finding.
    for e in &edges {
        if e.dep_type != DependencyType::ConditionalBlocks {
            continue;
        }
        let Some(t) = index.get(&e.depends_on_id) else {
            continue; // already reported as dangling.
        };
        if t.status.is_closed() && !bd_core::types::is_failure_close(&t.close_reason) {
            problems.push(json!({
                "kind": "stuck_conditional",
                "id": e.issue_id,
                "depends_on_id": e.depends_on_id,
                "detail": format!(
                    "{} runs only if {} fails, but {} closed successfully — it can never become ready",
                    e.issue_id, e.depends_on_id, e.depends_on_id
                ),
            }));
        }
    }

    // Marked blocked with nothing blocking it: the `is_blocked` cache is stale.
    //
    // Deliberately narrow. It flags only issues whose gating edges are *all*
    // plain `blocks` edges (or absent) and whose every target is dead, because
    // that is the one case the store's rule cannot explain no matter how the
    // rest of the graph resolves. Reproducing the full predicate here —
    // parent-child propagation, `waits-for` gates and their metadata — would put
    // a second copy of the blocked-cache engine in the CLI, and the copy is the
    // one that goes stale.
    let mut bf = IssueFilter::new();
    bf.is_blocked = Some(true);
    let mut marked_blocked = store.list_issues(&bf).await?;
    marked_blocked.sort_by(|a, b| a.id.cmp(&b.id));
    for i in &marked_blocked {
        let gating: Vec<&&Dependency> = out_edges
            .get(i.id.as_str())
            .map(|v| v.iter().filter(|d| d.dep_type.affects_ready_work()).collect())
            .unwrap_or_default();
        if gating.iter().any(|d| d.dep_type != DependencyType::Blocks) {
            continue;
        }
        let still_gated = gating
            .iter()
            .any(|d| index.get(&d.depends_on_id).is_some_and(is_live));
        if !still_gated {
            problems.push(json!({
                "kind": "stale_blocked",
                "id": i.id,
                "detail": format!(
                    "{} is marked blocked, but every issue that blocks it is closed (run `bd recompute-blocked`)",
                    i.id
                ),
            }));
        }
    }

    // An epic with no children is a plan nobody wrote down.
    for id in &ids {
        let i = &index[id];
        if i.issue_type == IssueType::Epic
            && !i.status.is_closed()
            && child_count.get(id.as_str()).copied().unwrap_or(0) == 0
        {
            problems.push(json!({
                "kind": "childless_epic",
                "id": i.id,
                "detail": format!("epic {} has no children", i.id),
            }));
        }
    }

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "ok": problems.is_empty(),
            "problems": problems,
        }));
    }
    if problems.is_empty() {
        ctx.out.line("No problems found.");
        return Ok(());
    }
    ctx.out.line(format!("{} problem(s):", problems.len()));
    for p in &problems {
        let kind = p["kind"].as_str().unwrap_or("problem");
        let detail = p["detail"].as_str().unwrap_or("");
        ctx.out.line(format!("  [{kind}] {detail}"));
    }
    // Exit 0 even with findings. Lint did not fail — it succeeded, and this is
    // what it found. `1` means beads broke, and a script that cannot tell those
    // apart cannot use either. Read `ok` from `--json`.
    Ok(())
}

// ---------------------------------------------------------------------------
// The workspace itself
// ---------------------------------------------------------------------------

pub async fn info(ctx: &Ctx) -> Result<()> {
    let prefix = ctx.prefix().await;
    let loc = ctx.locator()?;
    let store = ctx.store().await?;
    let stats = store.stats().await?;

    // Asked of the open store, not of the backend enum: whether a capability is
    // there is the store's answer to give (seam rule 4).
    let caps = json!({
        "commit_graph": store.has_commit_graph(),
        "version_control": store.version_control().is_some(),
        "remote": store.remote().is_some(),
        "history": store.history().is_some(),
    });

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "backend": loc.backend.as_str(),
            "beads_dir": loc.dir,
            "db": loc.db_path(),
            "workspace_id": loc.workspace_id,
            "prefix": prefix,
            "actor": ctx.identity.actor,
            "stats": stats,
            "capabilities": caps,
        }));
    }

    ctx.out.line(format!("workspace: {}", loc.dir.display()));
    ctx.out.line(format!("database:  {}", loc.db_path().display()));
    ctx.out.line(format!("backend:   {}", loc.backend));
    ctx.out.line(format!("id:        {}", loc.workspace_id));
    ctx.out.line(format!("prefix:    {prefix}"));
    ctx.out.line(format!("actor:     {}", ctx.identity.actor));
    ctx.out.line(format!(
        "issues:    {} ({} open, {} in progress, {} blocked, {} closed, {} ready)",
        stats.total, stats.open, stats.in_progress, stats.blocked, stats.closed, stats.ready
    ));
    let cap_list: Vec<&str> = ["version_control", "remote", "history"]
        .into_iter()
        .filter(|c| caps[*c].as_bool().unwrap_or(false))
        .collect();
    ctx.out.line(format!(
        "supports:  {}",
        if cap_list.is_empty() {
            // Not a shortfall. SQLite has no commit graph; that is what it is.
            "core storage only (no commit graph)".to_string()
        } else {
            cap_list.join(", ")
        }
    ));
    Ok(())
}

/// What an agent needs pasted into its context to pick up work: what it can
/// claim, what is already moving, and what just landed.
pub async fn context(ctx: &Ctx) -> Result<()> {
    /// Enough to orient, few enough to paste.
    const N: usize = 10;
    /// "Recent" for the purposes of "what did we just finish".
    const RECENT: i64 = 7;

    let store = ctx.store().await?;
    let stats = store.stats().await?;

    let mut rf = IssueFilter::ready();
    rf.limit = Some(N as u32);
    let mut ready = store.ready_work(&rf).await?;

    let mut wf = IssueFilter::new();
    wf.statuses = vec![Status::InProgress];
    wf.sort = SortPolicy::Priority;
    wf.limit = Some(N as u32);
    let mut in_progress = store.list_issues(&wf).await?;

    // No LIMIT here, and no sort policy orders by `closed_at` — so the window
    // does the bounding and the ordering happens in memory. Pushing a LIMIT down
    // under the default sort would hand back the *oldest-created* closes in the
    // window and call them recent.
    let mut cf = IssueFilter::new();
    cf.statuses = vec![Status::Closed];
    cf.closed_after = Some(Utc::now() - Duration::days(RECENT));
    let mut closed = store.list_issues(&cf).await?;
    closed.sort_by_key(|i| std::cmp::Reverse(i.closed_at));
    closed.truncate(N);

    // Labels are worth the one extra query here: they are how an agent decides
    // whether a bead is its department.
    for set in [&mut ready, &mut in_progress, &mut closed] {
        hydrate_labels(store, set).await?;
    }

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "stats": stats,
            "ready": ready,
            "in_progress": in_progress,
            "recently_closed": closed,
        }));
    }

    let line = |i: &Issue| {
        let labels = if i.labels.is_empty() {
            String::new()
        } else {
            format!("  [{}]", i.labels.join(", "))
        };
        format!("  {}  P{}  {}{labels}", i.id, i.priority.0, i.title)
    };

    ctx.out.line(format!(
        "{} issues: {} open, {} in progress, {} blocked, {} closed. {} ready to claim.",
        stats.total, stats.open, stats.in_progress, stats.blocked, stats.closed, stats.ready
    ));

    ctx.out.line(format!("\nReady ({}):", ready.len()));
    if ready.is_empty() {
        ctx.out.line("  (nothing claimable — see `bd blocked`)");
    }
    for i in &ready {
        ctx.out.line(line(i));
    }

    if !in_progress.is_empty() {
        ctx.out.line("\nIn progress:");
        for i in &in_progress {
            let who = if i.assignee.is_empty() {
                "unassigned".to_string()
            } else {
                format!("@{}", i.assignee)
            };
            ctx.out.line(format!("{}  ({who})", line(i)));
        }
    }
    if !closed.is_empty() {
        ctx.out.line(format!("\nClosed in the last {RECENT} days:"));
        for i in &closed {
            ctx.out.line(line(i));
        }
    }
    Ok(())
}

/// Liveness: open the store and make it answer something.
///
/// The timings are the point. "The database is up" and "the database takes four
/// seconds to open" are different answers, and only one of them is visible from
/// an exit code.
pub async fn ping(ctx: &Ctx) -> Result<()> {
    let t0 = Instant::now();
    let store = ctx.store().await?;
    let open_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = Instant::now();
    let n = store.count_issues(&IssueFilter::new()).await?;
    let query_ms = t1.elapsed().as_secs_f64() * 1000.0;

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "ok": true,
            "backend": store.backend().as_str(),
            "issues": n,
            "open_ms": open_ms,
            "query_ms": query_ms,
        }));
    }
    ctx.out.line(format!(
        "ok  {}  {n} issue(s)  open {open_ms:.1}ms  query {query_ms:.1}ms",
        store.backend()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Registered, not ported
//
// Each of these is blocked on something the storage seam does not express. They
// stay stubs — exit 64 — rather than becoming commands that half-work. See
// PORT_STATUS.md.
// ---------------------------------------------------------------------------

/// Raw SQL cannot go through a backend-agnostic trait, and giving it one would
/// make every other backend a liar the moment it did not speak SQLite's dialect.
/// The seam has no `execute_sql`, on purpose.
pub async fn sql(ctx: &Ctx, _query: &str) -> Result<()> {
    stub("sql", ctx)
}

/// A workspace key/value store, which the seam does not have. `get_config` /
/// `set_config` are the config table, not a scratchpad: mapping `kv` onto them
/// would make `bd kv list` print config keys and `bd config list` print an
/// agent's scratch state. There is also no delete on the seam at all, so
/// `kv clear` has nothing to call.
pub async fn kv(ctx: &Ctx, cmd: KvCmd) -> Result<()> {
    let name = match cmd {
        KvCmd::Set { .. } => "kv set",
        KvCmd::Get { .. } => "kv get",
        KvCmd::Clear { .. } => "kv clear",
        KvCmd::List => "kv list",
    };
    stub(name, ctx)
}

/// Writing an audit record needs a seam that can write an [`Event`], and it
/// cannot: `list_events` reads the trail, nothing writes to it except the
/// mutation that produced the event, and [`EventType`] has no variant a free-text
/// audit record could use. Labelling one needs labels on events, which do not
/// exist either.
///
/// [`Event`]: bd_core::Event
/// [`EventType`]: bd_core::EventType
pub async fn audit(ctx: &Ctx, cmd: AuditCmd) -> Result<()> {
    let name = match cmd {
        AuditCmd::Record { .. } => "audit record",
        AuditCmd::Label { .. } => "audit label",
    };
    stub(name, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_that_differ_only_in_noise_normalize_alike() {
        assert_eq!(normalize_title("Fix the parser!"), "fix the parser");
        assert_eq!(normalize_title("fix   the  parser"), "fix the parser");
        assert_eq!(normalize_title("  FIX THE PARSER  "), "fix the parser");
        // But not titles a person would call different.
        assert_ne!(normalize_title("fix the parser"), normalize_title("fix the lexer"));
        assert_eq!(normalize_title("!!!"), "");
    }

    #[test]
    fn percent_complete_does_not_divide_by_zero() {
        assert_eq!(percent_complete(0, 0), 0);
        assert_eq!(percent_complete(3, 5), 60);
        assert_eq!(percent_complete(5, 5), 100);
    }

    #[test]
    fn liveness_counts_both_spellings_of_pinned() {
        let mut i = Issue::new("bd-1", "t");
        assert!(is_live(&i));
        i.pinned = true;
        assert!(!is_live(&i), "a pinned flag stops an issue gating others");
        i.pinned = false;
        i.status = Status::Pinned;
        assert!(!is_live(&i), "so does a pinned status");
        i.status = Status::Closed;
        assert!(!is_live(&i));
    }
}
