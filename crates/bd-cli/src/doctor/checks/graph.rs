//! Data & Config — the integrity of the issue graph itself.
//!
//! # The check that matters most in the entire program
//!
//! `is_blocked` is a **denormalized cache** of the dependency graph, maintained
//! incrementally by local write paths and computed to a *fixpoint* (blocked-ness
//! propagates transitively down `parent-child` edges). Anything that lands rows
//! without going through a local write path — a merge, a pull, an import, a
//! hand-edited database — leaves that cache stale **by definition**.
//!
//! When it is stale, nothing crashes. `bd ready` simply hands out the wrong
//! work: it offers issues that are blocked, or hides issues that are free. No
//! error, no exit code, no log line. It is the worst failure this system has,
//! and it is completely invisible from the outside.
//!
//! A blocked-consistency check is therefore not one check among many. It is the
//! only thing standing between a stale cache and an agent confidently doing work
//! it was supposed to be blocked from doing. Upstream reached the same
//! conclusion (`CheckBlockedConsistency`), which is corroboration, not
//! coincidence.
//!
//! It is also the family's one genuinely *repairable* check: the fix is to run
//! `recompute_blocked()`, which is idempotent and always safe.
//!
//! Belongs here: blocked-cache consistency, dependency cycles, orphaned
//! dependencies (edges pointing at issues that do not exist), orphaned issues,
//! parent/child coherence, duplicate ids, stale closed issues.
//!
//! # How the blocked check actually establishes anything
//!
//! It **re-derives** `is_blocked` from the edges, in Rust, and compares that to
//! what the database has **stored** in the column. The two computations share no
//! code: the stored value came from `bd_sqlite::blocked`'s iterated SQL
//! `UPDATE`s, the expected value from [`expected_blocked`] below. That is the
//! whole point. A check that called `recompute_blocked()` and then compared the
//! result to itself would agree with itself on every input, including a workspace
//! whose cache is catastrophically wrong — it would report as coverage while
//! proving nothing.
//!
//! The stored column is read back through the seam, not by naming a column:
//! `IssueFilter { is_blocked: Some(true) }` is pushed down to `is_blocked = 1`,
//! so `list_issues` with that filter *is* the cache, verbatim.
//!
//! ## The one place the two computations legitimately differ
//!
//! [`expected_blocked`] computes the **least** fixpoint: it seeds from nothing
//! and grows. `bd_sqlite`'s `recompute_all` seeds from *whatever is already in
//! the column* and iterates mark/unmark. On a graph whose `parent-child` edges
//! form a DAG those agree, because the fixpoint is unique. On a graph with a
//! **containment cycle** they do not: two issues that are each other's parent and
//! are both stored as blocked will each see the other blocked, so `unmark` never
//! fires and SQL stays at a non-least fixpoint *forever*, even after the real
//! blocker closes.
//!
//! That is not a reason to weaken this check — a containment cycle is corrupt
//! data and [`DependencyCycles`] reports it as an `Error`. It is the reason
//! [`BlockedCache::repair`] **re-verifies** after recomputing instead of
//! assuming success: on a cyclic graph `recompute_blocked()` cannot converge to
//! the truth, and a repair that reported "fixed" there would send the user home
//! believing a lying `bd ready` had been mended.
//!
//! # Cost
//!
//! Three full table reads (issues, the blocked subset, all edges) plus
//! `find_cycles()`, shared across every check in the family via one
//! [`OnceCell`] — so the family costs the same whether it has one check or
//! twenty. The derivation itself is `O(V + E)` with no database round-trips.
//! Nothing here samples: a consistency check that only looked at *some* of the
//! graph would report as coverage while missing exactly the row that matters.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bd_core::{Dependency, DependencyType, Issue, IssueFilter};
use bd_storage::Storage;
use tokio::sync::OnceCell;

use super::super::{Category, Check, Dx, Finding, Repair};

pub fn checks() -> Vec<Box<dyn Check>> {
    // One snapshot, shared by every check in the family. `checks()` is called
    // once per `bd doctor` run (from `registry()`), so the cell's lifetime is
    // exactly one run — it cannot leak a stale graph into the next one.
    let shared: Shared = Arc::new(OnceCell::new());
    vec![
        Box::new(BlockedCache(shared.clone())),
        Box::new(DependencyCycles),
        Box::new(OrphanedDependencies(shared.clone())),
        Box::new(ParentChildCoherence(shared.clone())),
        Box::new(DuplicateIssues(shared.clone())),
        Box::new(StuckConditionalPaths(shared)),
    ]
}

// ---------------------------------------------------------------------------
// The snapshot
// ---------------------------------------------------------------------------

/// The graph, plus the cache the graph is supposed to agree with.
struct Snapshot {
    issues: Vec<Issue>,
    /// Every edge, including any whose endpoints have gone missing — that is
    /// what the empty filter buys, and it is the only way to see a dangling one.
    edges: Vec<Dependency>,
    /// What the database *says* is blocked. Read back through the seam rather
    /// than re-derived, because re-deriving it here is how the check would end up
    /// comparing a computation to itself.
    stored_blocked: BTreeSet<String>,
}

type Shared = Arc<OnceCell<Result<Arc<Snapshot>, String>>>;

async fn load(store: &dyn Storage) -> Result<Snapshot, String> {
    let all = IssueFilter::default();
    let issues = store.list_issues(&all).await.map_err(|e| e.to_string())?;
    let edges = store.list_dependencies(&all).await.map_err(|e| e.to_string())?;
    let blocked = store
        .list_issues(&IssueFilter {
            is_blocked: Some(true),
            ..IssueFilter::default()
        })
        .await
        .map_err(|e| e.to_string())?;

    Ok(Snapshot {
        issues,
        edges,
        stored_blocked: blocked.into_iter().map(|i| i.id).collect(),
    })
}

/// The snapshot, or the [`Finding`] this check owes the user instead.
///
/// No store is **not** this family's finding to report — the Core family owns
/// diagnosing an unopenable database. All we may say is that *we* could not run,
/// and [`Finding::unknown`] is a warning, never an ok: a check that swallowed
/// this and returned `Ok` would report as coverage while having looked at
/// nothing.
async fn snapshot(cell: &Shared, dx: &Dx<'_>, name: &'static str) -> Result<Arc<Snapshot>, Finding> {
    let Some(store) = dx.store().await else {
        return Err(Finding::unknown(
            name,
            dx.store_error()
                .unwrap_or("there is no workspace here")
                .to_string(),
        ));
    };
    match cell.get_or_init(|| async { load(store).await.map(Arc::new) }).await {
        Ok(s) => Ok(s.clone()),
        Err(e) => Err(Finding::unknown(name, format!("cannot read the graph: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// The derivation
// ---------------------------------------------------------------------------

/// An issue in a state where the graph may gate it.
///
/// Closed and pinned beads are never blocked — a pinned bead is deliberately out
/// of the running, and blocking a closed one is meaningless. Pinned-ness is
/// expressible two ways (as a status and as a flag) and both must count, or a
/// bead pinned one way behaves differently from a bead pinned the other. This
/// mirrors `LIVE` in `bd_sqlite::blocked` exactly; the two must not drift.
fn live(i: &Issue) -> bool {
    i.status.as_str() != "closed" && i.status.as_str() != "pinned" && !i.pinned
}

/// A view of the graph that answers the questions the fixpoint asks.
struct Adjacency<'a> {
    by_id: HashMap<&'a str, &'a Issue>,
    /// Edges *out of* each issue: what it depends on.
    out: HashMap<&'a str, Vec<&'a Dependency>>,
    /// parent -> its children. A `parent-child` edge is stored child-first
    /// (`issue_id` is the child, `depends_on_id` the parent), so this map is the
    /// edge set reversed — and getting that backwards silently inverts every
    /// propagation in the system.
    children: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> Adjacency<'a> {
    fn new(issues: &'a [Issue], edges: &'a [Dependency]) -> Adjacency<'a> {
        let mut adj = Adjacency {
            by_id: issues.iter().map(|i| (i.id.as_str(), i)).collect(),
            out: HashMap::new(),
            children: HashMap::new(),
        };
        for e in edges {
            adj.out.entry(e.issue_id.as_str()).or_default().push(e);
            if e.dep_type == DependencyType::ParentChild {
                adj.children
                    .entry(e.depends_on_id.as_str())
                    .or_default()
                    .push(e.issue_id.as_str());
            }
        }
        adj
    }

    fn get(&self, id: &str) -> Option<&'a Issue> {
        self.by_id.get(id).copied()
    }

    fn out_edges(&self, id: &str) -> &[&'a Dependency] {
        self.out.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    fn kids(&self, id: &str) -> &[&'a str] {
        self.children.get(id).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// What `is_blocked` **should** be, derived from the edges and nothing else.
///
/// The least fixpoint of the rule in `bd_sqlite::blocked`:
///
/// 1. a live `blocks` target gates you;
/// 2. a `conditional-blocks` target gates you while it is live, and *keeps*
///    gating you if it closed **successfully** — the failure path it guards is
///    moot, and the store deliberately leaves the bead stuck and visible rather
///    than closing a bead nobody asked it to close;
/// 3. a blocked parent gates you, transitively, all the way down;
/// 4. a `waits-for` gate over a spawner's children gates you until they are done.
///
/// Rule 3 is why this is a fixpoint and not a pass. It is computed here as a BFS
/// down the containment tree from the base-blocked set, which is monotone — so it
/// terminates even on a corrupt graph whose `parent-child` edges contain a cycle,
/// where the SQL's mark/unmark loop would burn its whole iteration budget.
///
/// Targets that do not exist gate nothing. That is not charity: the SQL joins
/// `issues`, so a dangling edge produces no row and blocks no one. A check that
/// treated a dangling `blocks` edge as a live gate would disagree with the
/// database on every import and report a stale cache that was not stale. The
/// dangling edge is still a real problem — see [`OrphanedDependencies`], which is
/// the check that says so.
fn expected_blocked(issues: &[Issue], edges: &[Dependency]) -> BTreeSet<String> {
    let adj = Adjacency::new(issues, edges);

    let mut blocked: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<&str> = Vec::new();

    for i in issues {
        if live(i) && base_blocked(i, &adj) {
            blocked.insert(i.id.clone());
            queue.push(i.id.as_str());
        }
    }

    // Rule 3, to a fixpoint. `blocked.insert` is the visited set, so a
    // containment cycle costs one extra visit per node, not an infinite loop.
    let mut head = 0;
    while head < queue.len() {
        let parent = queue[head];
        head += 1;
        for &child in adj.kids(parent) {
            let Some(c) = adj.get(child) else { continue };
            if live(c) && blocked.insert(child.to_string()) {
                queue.push(child);
            }
        }
    }

    blocked
}

/// Rules 1, 2 and 4 — everything except the propagation down the tree.
fn base_blocked(i: &Issue, adj: &Adjacency<'_>) -> bool {
    adj.out_edges(&i.id).iter().any(|e| match &e.dep_type {
        DependencyType::Blocks => adj.get(&e.depends_on_id).is_some_and(live),

        // `B conditional-blocks A` = "run B only if A fails". So B is gated while
        // A is open, and *stays* gated if A closed successfully. Read from
        // `close_reason` through bd-core, never from the derived
        // `close_is_failure` column: that column is itself a cache, and a check
        // that trusted it could not see it go stale.
        DependencyType::ConditionalBlocks => adj.get(&e.depends_on_id).is_some_and(|t| {
            live(t) || (t.status.as_str() == "closed" && !closed_in_failure(t))
        }),

        DependencyType::WaitsFor => waits_for_gate_is_shut(e, adj),

        // Everything else is an association. It exists to be traversed and
        // displayed; it must never gate work.
        _ => false,
    })
}

fn closed_in_failure(i: &Issue) -> bool {
    bd_core::types::is_failure_close(&i.close_reason)
}

/// A `waits-for` edge names a **spawner**, and the gate is over the *spawner's
/// children* — not over the spawner itself. By default every child must be done;
/// an edge whose metadata says `{"gate":"any-children"}` opens as soon as one
/// child closes.
///
/// Malformed metadata falls back to the default gate rather than erroring, which
/// is what the SQL's `json_valid` guard and `COALESCE` do. Silently treating it
/// as `any-children` would unblock a waiter that should still be waiting.
fn waits_for_gate_is_shut(e: &Dependency, adj: &Adjacency<'_>) -> bool {
    let kids = adj.kids(&e.depends_on_id);
    if !kids.iter().any(|k| adj.get(k).is_some_and(live)) {
        // No live children (or no children at all): the gate is open.
        return false;
    }
    if gate_is_any_children(&e.metadata)
        && kids
            .iter()
            .any(|k| adj.get(k).is_some_and(|c| c.status.as_str() == "closed"))
    {
        return false;
    }
    true
}

fn gate_is_any_children(metadata: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return false;
    };
    v.get("gate").and_then(|g| g.as_str()) == Some("any-children")
}

/// The two ways the cache can lie, kept apart because they are opposite bugs.
struct Disagreement {
    /// Should be blocked; the cache says free. `bd ready` **hands these out** —
    /// an agent starts work whose blocker is still open.
    offered_but_blocked: Vec<String>,
    /// Should be free; the cache says blocked. `bd ready` **hides these** — the
    /// work is claimable and nobody is told.
    hidden_but_free: Vec<String>,
}

impl Disagreement {
    fn between(expected: &BTreeSet<String>, stored: &BTreeSet<String>) -> Disagreement {
        Disagreement {
            offered_but_blocked: expected.difference(stored).cloned().collect(),
            hidden_but_free: stored.difference(expected).cloned().collect(),
        }
    }

    fn len(&self) -> usize {
        self.offered_but_blocked.len() + self.hidden_but_free.len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn ids(&self) -> Vec<String> {
        let mut v = self.offered_but_blocked.clone();
        v.extend(self.hidden_but_free.iter().cloned());
        v
    }
}

// ---------------------------------------------------------------------------
// 1. The one that justifies the command
// ---------------------------------------------------------------------------

pub struct BlockedCache(Shared);

#[async_trait]
impl Check for BlockedCache {
    fn name(&self) -> &'static str {
        "blocked-cache"
    }

    fn category(&self) -> Category {
        Category::Data
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let snap = match snapshot(&self.0, dx, self.name()).await {
            Ok(s) => s,
            Err(f) => return f,
        };

        let expected = expected_blocked(&snap.issues, &snap.edges);
        let bad = Disagreement::between(&expected, &snap.stored_blocked);

        if bad.is_empty() {
            return Finding::ok(
                self.name(),
                format!(
                    "is_blocked agrees with the graph ({} issue(s), {} edge(s))",
                    snap.issues.len(),
                    snap.edges.len()
                ),
            );
        }

        // An `Error`, not a warning, and deliberately harsher than upstream —
        // which calls this a warning. A stale cache is not untidy. `bd ready` is
        // *giving the wrong answer* and every command still exits 0, which is the
        // precise definition of broken. If `bd doctor` will not fail a git hook
        // over a lying `bd ready`, there is nothing it should fail over.
        Finding::error(
            self.name(),
            format!(
                "{} issue(s) have a stale is_blocked flag — `bd ready` is handing out the wrong work",
                bad.len()
            ),
        )
        .detail(describe(&bad))
        .fix("bd doctor --fix  (or `bd recompute-blocked`) — idempotent, and always safe")
    }

    /// The one repair in this family that is obvious and always safe.
    ///
    /// It re-verifies afterwards rather than trusting the recompute. That is not
    /// belt-and-braces: on a graph with a `parent-child` cycle the SQL fixpoint
    /// seeds from the column it is trying to fix and provably cannot converge to
    /// the truth (see the module docs), so `recompute_blocked()` can return
    /// happily while `bd ready` still lies. Reporting that as "fixed" would be
    /// the single most harmful thing this command could do.
    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(store) = dx.store().await else {
            // We never established there was anything wrong — we established that
            // we could not look. There is nothing to repair.
            return Ok(Repair::Unfixable);
        };

        let changed = store.recompute_blocked().await?;

        let after = load(store).await.map_err(|e| anyhow!(e))?;
        let expected = expected_blocked(&after.issues, &after.edges);
        let still = Disagreement::between(&expected, &after.stored_blocked);
        if !still.is_empty() {
            return Err(anyhow!(
                "recompute rewrote {changed} row(s), but {} are still wrong ({}). \
                 A full recompute cannot converge while the containment graph has a \
                 cycle — see the `dependency-cycles` check, and break the cycle first",
                still.len(),
                preview(&still.ids())
            ));
        }

        Ok(Repair::Did(format!(
            "rebuilt the blocked cache from the dependency graph; {changed} row(s) changed"
        )))
    }
}

fn describe(bad: &Disagreement) -> String {
    let mut out = Vec::new();
    if !bad.offered_but_blocked.is_empty() {
        out.push(format!(
            "blocked, but the cache says free — `bd ready` is offering these: {}",
            preview(&bad.offered_but_blocked)
        ));
    }
    if !bad.hidden_but_free.is_empty() {
        out.push(format!(
            "free, but the cache says blocked — `bd ready` is hiding these: {}",
            preview(&bad.hidden_but_free)
        ));
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------
// 2. Cycles
// ---------------------------------------------------------------------------

pub struct DependencyCycles;

#[async_trait]
impl Check for DependencyCycles {
    fn name(&self) -> &'static str {
        "dependency-cycles"
    }

    fn category(&self) -> Category {
        Category::Data
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(store) = dx.store().await else {
            return Finding::unknown(
                self.name(),
                dx.store_error()
                    .unwrap_or("there is no workspace here")
                    .to_string(),
            );
        };

        let cycles = match store.find_cycles().await {
            Ok(c) => c,
            Err(e) => return Finding::unknown(self.name(), format!("cannot walk the graph: {e}")),
        };

        if cycles.is_empty() {
            return Finding::ok(self.name(), "the dependency graph is acyclic");
        }

        // Every issue in a cycle is transitively blocked by itself and can never
        // become ready. It is also what costs the `is_blocked` fixpoint its whole
        // iteration budget, and what makes `bd dep tree` non-terminating. The
        // write path refuses to create one, so a cycle here means the rows came
        // from somewhere else — an import, a merge, another implementation.
        Finding::error(
            self.name(),
            format!(
                "{} dependency cycle(s): every issue in one is blocked by itself, forever",
                cycles.len()
            ),
        )
        .detail(preview(
            &cycles.iter().map(|c| c.join(" → ")).collect::<Vec<_>>(),
        ))
        // Deliberately not auto-fixed. Breaking a cycle means *deleting an edge*,
        // and only the author knows which of them was the mistake. A doctor that
        // guessed would destroy a real dependency to make its own report green.
        .fix("break each cycle by hand: `bd dep remove <issue> <depends-on> --type <type>`")
    }
}

// ---------------------------------------------------------------------------
// 3. Edges into the void
// ---------------------------------------------------------------------------

/// Cross-rig references. The exporter injects these deliberately and they name
/// issues that are *supposed* to live somewhere else, so they are not orphans.
const EXTERNAL_REF_PREFIX: &str = "external:";

pub struct OrphanedDependencies(Shared);

#[async_trait]
impl Check for OrphanedDependencies {
    fn name(&self) -> &'static str {
        "orphaned-dependencies"
    }

    fn category(&self) -> Category {
        Category::Data
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let snap = match snapshot(&self.0, dx, self.name()).await {
            Ok(s) => s,
            Err(f) => return f,
        };

        let ids: BTreeSet<&str> = snap.issues.iter().map(|i| i.id.as_str()).collect();
        let dangling: Vec<String> = snap
            .edges
            .iter()
            .filter(|e| {
                let src_gone = !ids.contains(e.issue_id.as_str());
                let dst_gone = !ids.contains(e.depends_on_id.as_str())
                    && !e.depends_on_id.starts_with(EXTERNAL_REF_PREFIX);
                src_gone || dst_gone
            })
            .map(|e| format!("{} → {} ({})", e.issue_id, e.depends_on_id, e.dep_type))
            .collect();

        if dangling.is_empty() {
            return Finding::ok(self.name(), "every edge lands on an issue that exists");
        }

        // Why this is an error and not untidiness: the `is_blocked` derivation
        // *joins* the target row. A dangling `blocks` edge therefore gates
        // nothing — it is a lock the user believes they set, silently doing
        // nothing, and `bd ready` will hand out the bead it was supposed to hold.
        // The graph is the product; an edge into the void is a hole in it.
        Finding::error(
            self.name(),
            format!(
                "{} edge(s) point at an issue that does not exist — the gate they \
                 look like is not there",
                dangling.len()
            ),
        )
        .detail(preview(&dangling))
        // Not auto-fixed: the repair is a DELETE, and the edge is the only record
        // that the relationship was ever intended. Restoring the missing issue is
        // just as likely to be the right answer as dropping the edge, and doctor
        // does not get to choose between them.
        .fix("`bd dep remove <issue> <depends-on> --type <type>` if the edge is junk — \
              but if the *issue* is what went missing, re-import it instead")
    }
}

// ---------------------------------------------------------------------------
// 4. Containment
// ---------------------------------------------------------------------------

pub struct ParentChildCoherence(Shared);

#[async_trait]
impl Check for ParentChildCoherence {
    fn name(&self) -> &'static str {
        "parent-child-coherence"
    }

    fn category(&self) -> Category {
        Category::Data
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let snap = match snapshot(&self.0, dx, self.name()).await {
            Ok(s) => s,
            Err(f) => return f,
        };
        let adj = Adjacency::new(&snap.issues, &snap.edges);

        let mut parents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &snap.edges {
            if e.dep_type == DependencyType::ParentChild {
                parents
                    .entry(e.issue_id.as_str())
                    .or_default()
                    .push(e.depends_on_id.as_str());
            }
        }

        // Containment is a tree. Two parents makes `bd list --parent` (a
        // recursive descent) count the subtree twice, and makes blocked-ness
        // arrive from two directions at once — so an issue can be gated by an
        // epic its author never put it in.
        let multi: Vec<String> = parents
            .iter()
            .filter(|(_, ps)| ps.len() > 1)
            .map(|(c, ps)| format!("{c} has {} parents: {}", ps.len(), ps.join(", ")))
            .collect();

        // A closed parent with open children: the epic reports done and the work
        // is not. Nothing blocks — a closed parent propagates no blocked-ness —
        // so this is invisible everywhere except here.
        let mut abandoned: Vec<String> = Vec::new();
        for (child, ps) in &parents {
            let Some(c) = adj.get(child) else { continue };
            if c.status.is_closed() {
                continue;
            }
            for p in ps {
                if adj.get(p).is_some_and(|pi| pi.status.is_closed()) {
                    abandoned.push(format!("{child} is open under a closed parent {p}"));
                }
            }
        }

        if multi.is_empty() && abandoned.is_empty() {
            return Finding::ok(self.name(), "the containment tree is coherent");
        }

        // Only mention what is actually wrong. A message that leads with
        // "0 issue(s) with two parents" buries the finding the user has, behind
        // the one they do not.
        let mut said: Vec<String> = Vec::new();
        let mut detail: Vec<String> = Vec::new();
        if !multi.is_empty() {
            said.push(format!("{} issue(s) have more than one parent", multi.len()));
            detail.push(preview(&multi));
        }
        if !abandoned.is_empty() {
            said.push(format!(
                "{} open issue(s) sit under a closed parent",
                abandoned.len()
            ));
            detail.push(preview(&abandoned));
        }

        // A warning: the workspace works, the answers are just misleading. Which
        // parent is the real one, and whether the epic was closed early, are both
        // decisions only a human has the context to make.
        Finding::warn(self.name(), said.join("; "))
            .detail(detail.join("\n"))
            .fix("`bd dep remove <child> <wrong-parent> --type parent-child`, or reopen the parent")
    }
}

// ---------------------------------------------------------------------------
// 5. The same bead, twice
// ---------------------------------------------------------------------------

pub struct DuplicateIssues(Shared);

#[async_trait]
impl Check for DuplicateIssues {
    fn name(&self) -> &'static str {
        "duplicate-issues"
    }

    fn category(&self) -> Category {
        Category::Data
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let snap = match snapshot(&self.0, dx, self.name()).await {
            Ok(s) => s,
            Err(f) => return f,
        };

        // `Issue::compute_content_hash` is bd-core's own answer to "are these the
        // same bead", and it is the answer the import path already uses to dedupe
        // across clones. Reusing it means doctor cannot disagree with import about
        // what a duplicate is.
        //
        // Closed issues are excluded: two beads that describe the same finished
        // work are history, not a problem to act on.
        let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for i in &snap.issues {
            if i.status.is_closed() {
                continue;
            }
            groups
                .entry(i.compute_content_hash())
                .or_default()
                .push(i.id.as_str());
        }
        groups.retain(|_, ids| ids.len() > 1);

        if groups.is_empty() {
            return Finding::ok(self.name(), "no two open issues describe the same work");
        }

        let dupes: usize = groups.values().map(|g| g.len() - 1).sum();
        Finding::warn(
            self.name(),
            format!(
                "{dupes} duplicate issue(s) across {} group(s) — two agents will do the same work",
                groups.len()
            ),
        )
        .detail(preview(
            &groups.values().map(|g| g.join(" = ")).collect::<Vec<_>>(),
        ))
        // Not auto-fixed. Which of two identical beads is the real one is a
        // question about intent, and the wrong answer silently deletes work.
        .fix("close the extras: `bd close <id> --reason duplicate`")
    }
}

// ---------------------------------------------------------------------------
// 6. Work that can never become ready
// ---------------------------------------------------------------------------

pub struct StuckConditionalPaths(Shared);

#[async_trait]
impl Check for StuckConditionalPaths {
    fn name(&self) -> &'static str {
        "stuck-conditional-paths"
    }

    fn category(&self) -> Category {
        Category::Data
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let snap = match snapshot(&self.0, dx, self.name()).await {
            Ok(s) => s,
            Err(f) => return f,
        };
        let adj = Adjacency::new(&snap.issues, &snap.edges);

        // `B conditional-blocks A` means "run B only if A fails". When A closes
        // *successfully* the failure path is moot — and the store deliberately
        // leaves B blocked rather than closing a bead the user never asked it to
        // close (see the `bd_sqlite::blocked` docs, which say so explicitly and
        // then say "`bd blocked` will show it").
        //
        // It will. Forever. Nothing will ever move B again, no command errors, and
        // it looks exactly like ordinary blocked work. This check is the thing that
        // says "that one is not waiting, it is stranded".
        let mut stranded: Vec<String> = Vec::new();
        for e in &snap.edges {
            if e.dep_type != DependencyType::ConditionalBlocks {
                continue;
            }
            let (Some(waiter), Some(subject)) =
                (adj.get(&e.issue_id), adj.get(&e.depends_on_id))
            else {
                continue; // orphaned-dependencies owns this one.
            };
            if !live(waiter) {
                continue;
            }
            if subject.status.as_str() == "closed" && !closed_in_failure(subject) {
                stranded.push(format!(
                    "{} waits for {} to fail, but {} closed successfully ({:?})",
                    waiter.id,
                    subject.id,
                    subject.id,
                    subject.close_reason
                ));
            }
        }

        if stranded.is_empty() {
            return Finding::ok(self.name(), "no work is stranded behind a failure path");
        }

        Finding::warn(
            self.name(),
            format!(
                "{} issue(s) wait on a failure that can no longer happen — they will \
                 never become ready",
                stranded.len()
            ),
        )
        .detail(preview(&stranded))
        .fix("reap them (`bd close <id> --reason \"not needed\"`), or reopen the subject if it \
              really did fail")
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// A finding that says "3 issues are corrupt" without naming them is a bug report
/// you cannot act on. But a finding that names nine thousand of them is a wall.
const MAX_DETAIL_ITEMS: usize = 20;

fn preview(items: &[String]) -> String {
    if items.len() <= MAX_DETAIL_ITEMS {
        return items.join("\n");
    }
    format!(
        "{}\n… and {} more",
        items[..MAX_DETAIL_ITEMS].join("\n"),
        items.len() - MAX_DETAIL_ITEMS
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;
    use bd_storage::Identity;

    // -- a real workspace, on a real disk, through the real write paths --------
    //
    // These are unit tests only in the sense that they live next to the code. The
    // store is genuine SQLite and every edge below goes through the same
    // `add_dependency` / `close_issue` that the CLI calls, so the `is_blocked`
    // column they read back is the *real* one, maintained by the *real* fixpoint.
    // That is what makes `derives_exactly_what_the_sql_fixpoint_derives` a
    // differential test and not a tautology: two independent implementations of
    // the same rule, over the same graph, must agree.

    struct Tmp(std::path::PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let dir = std::env::temp_dir().join(format!(
                "bd-doctor-graph-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(std::fs::canonicalize(&dir).unwrap())
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    async fn store_at(dir: &std::path::Path) -> Box<dyn Storage> {
        bd_sqlite::init(dir, "t", Identity::new("tester"))
            .await
            .unwrap()
    }

    async fn issue(s: &dyn Storage, id: &str) {
        s.create_issue(&Issue::new(id, id)).await.unwrap();
    }

    async fn edge(s: &dyn Storage, from: &str, to: &str, ty: &str) {
        let d = Dependency::new(from, to, DependencyType::from(ty.to_string())).unwrap();
        s.add_dependency(&d).await.unwrap();
    }

    /// The graph every gating construct in the system appears in, exactly once.
    ///
    /// * `t-e` blocks `t-d`; `t-c` is a child of `t-d`, `t-b` of `t-c`, `t-a` of
    ///   `t-b` — the transitive containment chain the whole fixpoint exists for.
    /// * `t-rollback` conditional-blocks `t-deploy`, which closed **successfully**
    ///   — so the rollback is blocked forever.
    /// * `t-repair` conditional-blocks `t-flaky`, which closed **failing** — so
    ///   the repair is free.
    /// * `t-collect` waits-for the spawner `t-spawn`, which still has a live child.
    /// * `t-pin` is pinned and gated: pinned beads are never blocked.
    async fn everything(s: &dyn Storage) {
        for id in [
            "t-a", "t-b", "t-c", "t-d", "t-e", "t-deploy", "t-rollback", "t-flaky", "t-repair",
            "t-spawn", "t-kid", "t-collect", "t-pin", "t-pinblocker",
        ] {
            issue(s, id).await;
        }

        edge(s, "t-d", "t-e", "blocks").await;
        edge(s, "t-c", "t-d", "parent-child").await;
        edge(s, "t-b", "t-c", "parent-child").await;
        edge(s, "t-a", "t-b", "parent-child").await;

        edge(s, "t-rollback", "t-deploy", "conditional-blocks").await;
        edge(s, "t-repair", "t-flaky", "conditional-blocks").await;

        edge(s, "t-kid", "t-spawn", "parent-child").await;
        edge(s, "t-collect", "t-spawn", "waits-for").await;

        edge(s, "t-pin", "t-pinblocker", "blocks").await;
        s.update_issue(
            "t-pin",
            &bd_storage::IssuePatch {
                pinned: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        s.close_issue("t-deploy", "done").await.unwrap();
        s.close_issue("t-flaky", "failed").await.unwrap();
    }

    /// **The differential test.**
    ///
    /// `expected_blocked` and `bd_sqlite::blocked` share no code — one is a BFS in
    /// Rust, the other is a loop of SQL `UPDATE`s — and they must produce the same
    /// answer over every gating construct the system has. If they do not, one of
    /// them is wrong about what `bd ready` means, and this is the only place that
    /// would ever notice.
    #[tokio::test]
    async fn derives_exactly_what_the_sql_fixpoint_derives() {
        let tmp = Tmp::new("differential");
        let s = store_at(&tmp.0).await;
        everything(s.as_ref()).await;

        let snap = load(s.as_ref()).await.unwrap();
        let expected = expected_blocked(&snap.issues, &snap.edges);

        // First: the derivation is not vacuous. If it decided nothing was ever
        // blocked it would also "agree" with an empty cache, and this whole file
        // would be a very long way of asserting `true`.
        assert_eq!(
            expected,
            [
                "t-a",        // child of a blocked parent, three levels down
                "t-b",        //
                "t-c",        //
                "t-collect",  // its spawner still has a live child
                "t-d",        // t-e blocks it
                "t-rollback", // its subject closed *successfully*: stranded
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>(),
            "the derivation itself is wrong, never mind the cache"
        );

        assert_eq!(
            expected, snap.stored_blocked,
            "the Rust derivation and the SQL fixpoint disagree about who is blocked"
        );
        assert!(
            Disagreement::between(&expected, &snap.stored_blocked).is_empty(),
            "a workspace built entirely through the write paths must be consistent"
        );

        // And the store agrees that it is at the fixpoint: a full recompute over a
        // cache the incremental path maintained correctly changes nothing.
        assert_eq!(s.recompute_blocked().await.unwrap(), 0);
    }

    /// **The test that proves the check can fail.**
    ///
    /// A merge, a pull or a hand-edited database lands rows without going through
    /// a write path, so the `is_blocked` column keeps whatever it had. Here that
    /// is reproduced exactly: the graph is real, the *stored* cache is real, and
    /// then two rows of the cache are corrupted the way a merge corrupts them —
    /// one issue that is truly blocked is marked free, and one that is truly free
    /// is marked blocked.
    ///
    /// Both directions matter and they are opposite bugs: the first makes
    /// `bd ready` hand an agent a bead whose blocker is still open; the second
    /// makes it hide claimable work. A check that recomputed `is_blocked` and then
    /// compared the recomputation to itself would sail through this test — which is
    /// why the comparison takes the stored set as an *input* rather than deriving
    /// it twice.
    #[tokio::test]
    async fn a_cache_stale_the_way_a_merge_leaves_it_is_caught_in_both_directions() {
        let tmp = Tmp::new("stale");
        let s = store_at(&tmp.0).await;
        everything(s.as_ref()).await;

        let snap = load(s.as_ref()).await.unwrap();
        let expected = expected_blocked(&snap.issues, &snap.edges);

        // Sanity: before we corrupt anything, there is nothing to find. Without
        // this the test could "pass" against a check that reports staleness on
        // every workspace.
        assert!(Disagreement::between(&expected, &snap.stored_blocked).is_empty());

        let mut merged = snap.stored_blocked.clone();
        // The merge landed `t-e blocks t-d` but never recomputed, so t-d's row
        // still carries the is_blocked=0 it had before the edge existed.
        assert!(merged.remove("t-d"), "t-d must have been stored as blocked");
        // And it landed the close of some blocker on t-e, whose row still carries
        // the is_blocked=1 it had while that blocker was open.
        assert!(merged.insert("t-e".to_string()), "t-e must have been free");

        let bad = Disagreement::between(&expected, &merged);

        assert_eq!(
            bad.offered_but_blocked,
            vec!["t-d".to_string()],
            "t-d is gated by t-e and the cache says it is free: `bd ready` would hand it out"
        );
        assert_eq!(
            bad.hidden_but_free,
            vec!["t-e".to_string()],
            "t-e is claimable and the cache says it is blocked: `bd ready` would hide it"
        );
        assert_eq!(bad.len(), 2);

        // The finding has to *name* them. "2 issues are corrupt" is a bug report
        // nobody can act on.
        let detail = describe(&bad);
        assert!(detail.contains("t-d"), "{detail}");
        assert!(detail.contains("t-e"), "{detail}");
    }

    /// The repair is the point of the check. It must actually converge — and the
    /// store must agree, from the other side, that it did.
    #[tokio::test]
    async fn recompute_is_the_repair_and_it_is_idempotent() {
        let tmp = Tmp::new("repair");
        let s = store_at(&tmp.0).await;
        everything(s.as_ref()).await;

        // Twice, because a repair that is not idempotent cannot be run from a
        // hook, and `bd doctor --fix` will absolutely be run from a hook.
        assert_eq!(s.recompute_blocked().await.unwrap(), 0);
        assert_eq!(s.recompute_blocked().await.unwrap(), 0);

        let snap = load(s.as_ref()).await.unwrap();
        assert!(
            Disagreement::between(&expected_blocked(&snap.issues, &snap.edges), &snap.stored_blocked)
                .is_empty()
        );
    }

    /// A dangling `blocks` edge gates nothing, because the SQL joins the target
    /// row and a missing row produces no join. The derivation must make the same
    /// call, or doctor reports a stale cache on every workspace that has one — and
    /// then `--fix` "repairs" it by rewriting the column to the value it already
    /// had, forever.
    #[test]
    fn an_edge_into_the_void_gates_nobody() {
        let issues = vec![Issue::new("t-1", "the one that survived")];
        let edges = vec![
            Dependency::new("t-1", "t-gone", DependencyType::Blocks).unwrap(),
            Dependency::new("t-1", "t-gone-too", DependencyType::ParentChild).unwrap(),
        ];
        assert!(
            expected_blocked(&issues, &edges).is_empty(),
            "a gate whose subject does not exist is not a gate"
        );
    }

    /// The `waits-for` gate is over the *spawner's children*, and the metadata
    /// decides whether one closed child is enough. Malformed metadata must fall
    /// back to the strict gate: guessing `any-children` would unblock a waiter
    /// that is still waiting.
    #[test]
    fn the_waits_for_gate_reads_its_metadata_and_distrusts_it() {
        assert!(gate_is_any_children(r#"{"gate":"any-children"}"#));
        assert!(!gate_is_any_children(r#"{"gate":"all-children"}"#));
        assert!(!gate_is_any_children("{}"));
        assert!(!gate_is_any_children(""));
        assert!(!gate_is_any_children("not json at all"));
        assert!(!gate_is_any_children("[1,2,3]"));
    }

    /// Pinned-ness is expressible as a status *and* as a flag, and the derivation
    /// has to honour both — a bead pinned one way must not behave differently from
    /// a bead pinned the other.
    #[test]
    fn a_pinned_bead_is_never_blocked_whichever_way_it_was_pinned() {
        let blocker = Issue::new("t-blocker", "still open");
        let by_flag = Issue {
            pinned: true,
            ..Issue::new("t-flag", "pinned by flag")
        };
        let by_status = Issue {
            status: bd_core::Status::Pinned,
            ..Issue::new("t-status", "pinned by status")
        };
        let issues = vec![blocker, by_flag, by_status];
        let edges = vec![
            Dependency::new("t-flag", "t-blocker", DependencyType::Blocks).unwrap(),
            Dependency::new("t-status", "t-blocker", DependencyType::Blocks).unwrap(),
        ];
        assert!(expected_blocked(&issues, &edges).is_empty());
    }

    /// A containment cycle is corrupt data, and the derivation's job on corrupt
    /// data is to *terminate*. The SQL fixpoint burns its whole iteration budget
    /// here; a BFS with a visited set simply stops.
    #[test]
    fn a_containment_cycle_terminates_instead_of_spinning() {
        let issues = vec![
            Issue::new("t-x", "the blocker"),
            Issue::new("t-a", "a"),
            Issue::new("t-b", "b"),
        ];
        let edges = vec![
            Dependency::new("t-a", "t-x", DependencyType::Blocks).unwrap(),
            Dependency::new("t-a", "t-b", DependencyType::ParentChild).unwrap(),
            Dependency::new("t-b", "t-a", DependencyType::ParentChild).unwrap(),
        ];
        let got = expected_blocked(&issues, &edges);
        // t-a is gated by t-x, and blocked-ness propagates around the loop to t-b.
        assert_eq!(
            got,
            ["t-a", "t-b"].iter().map(|s| s.to_string()).collect::<BTreeSet<_>>()
        );
    }

    /// Association edges must never gate work. This is the predicate that decides
    /// what `bd ready` shows, so a single edge type leaking into it changes what
    /// the whole product does.
    #[test]
    fn associations_do_not_gate() {
        let issues = vec![Issue::new("t-1", "one"), Issue::new("t-2", "two")];
        let mut edges = Vec::new();
        for ty in [
            DependencyType::Related,
            DependencyType::DiscoveredFrom,
            DependencyType::Duplicates,
            DependencyType::Supersedes,
            DependencyType::Tracks,
        ] {
            edges.push(Dependency::new("t-1", "t-2", ty).unwrap());
        }
        assert!(expected_blocked(&issues, &edges).is_empty());

        // …and the same pair, with a real gate on top, does block — so this is not
        // passing because the derivation never blocks anything.
        edges.push(Dependency::new("t-1", "t-2", DependencyType::Blocks).unwrap());
        assert_eq!(expected_blocked(&issues, &edges).len(), 1);
    }

    // -- the checks, through the seam ----------------------------------------

    async fn dx_ctx(dir: &std::path::Path) -> crate::context::Ctx {
        use clap::Parser as _;
        let cli = crate::cli::Cli::parse_from(["bd", "-C", dir.to_str().unwrap(), "doctor"]);
        crate::context::Ctx::build(&cli, crate::context::Need::Nothing)
            .await
            .unwrap()
    }

    fn finding<'a>(fs: &'a [Finding], name: &str) -> &'a Finding {
        fs.iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{name} is not registered"))
    }

    /// Every check in the family, run against a healthy workspace through the real
    /// `Dx`, must be `Ok` — and there must be six of them, so that a family that
    /// quietly stopped registering a check cannot pass by having nothing to say.
    #[tokio::test]
    async fn the_family_is_clean_on_a_healthy_workspace() {
        let tmp = Tmp::new("clean");
        {
            let s = store_at(&tmp.0).await;
            everything(s.as_ref()).await;
            s.close().await.unwrap();
        }

        let ctx = dx_ctx(&tmp.0).await;
        let dx = Dx::new(&ctx);

        let registry = checks();
        assert_eq!(registry.len(), 6);

        let mut findings = Vec::new();
        for c in &registry {
            findings.push(c.run(&dx).await);
        }

        // t-rollback is stranded behind a successful close, on purpose — that is
        // the one thing `everything()` builds that is *supposed* to warn.
        let stuck = finding(&findings, "stuck-conditional-paths");
        assert_eq!(stuck.status, Status::Warn);
        assert!(stuck.detail.as_ref().unwrap().contains("t-rollback"));

        for f in &findings {
            if f.name == "stuck-conditional-paths" {
                continue;
            }
            assert!(
                f.is_ok(),
                "{} should be ok on a healthy workspace: {} / {:?}",
                f.name,
                f.message,
                f.detail
            );
        }
    }

    /// Doctor runs on workspaces too broken to open — that is the job. Every check
    /// here needs a store, and not getting one is a warning **about itself**, never
    /// an `Ok`. An undeterminable check that reports `Ok` is worse than no check,
    /// because it reports as coverage.
    #[tokio::test]
    async fn with_no_workspace_every_check_says_it_could_not_look() {
        let tmp = Tmp::new("nostore"); // created, but never `bd init`ed
        let ctx = dx_ctx(&tmp.0).await;
        let dx = Dx::new(&ctx);

        for c in checks() {
            let f = c.run(&dx).await;
            assert!(
                !f.is_ok(),
                "{} reported ok with no database to look at",
                c.name()
            );
            assert_eq!(f.status, Status::Unknown);
            assert_eq!(f.message, "could not check");
            assert!(f.detail.is_some(), "{} owes a reason", c.name());

            // And it must not offer to repair what it never diagnosed.
            assert!(
                matches!(c.repair(&dx, &f).await, Ok(Repair::Unfixable)),
                "{} tried to repair a workspace it could not even read",
                c.name()
            );
        }
    }
}
