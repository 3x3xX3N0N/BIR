//! `bd mol …` — molecules: the lifecycle of a formula instance.
//!
//! A molecule is not a new kind of storage. It is an ordinary issue of type
//! [`IssueType::Molecule`] that groups a unit of work, plus the beads it contains
//! hung off it by `parent-child` edges. So this whole family is built on the
//! existing seam — `create_issue`, `list_issues` filtered by type,
//! `add_dependency` — and on the formula compiler for the two commands that
//! instantiate one.
//!
//! # The `seed` / `pour` split
//!
//! The two phases of the metaphor are kept **separate**, which is the design the
//! current args (`seed <template>` takes a formula name, `pour <id>` takes an
//! issue id) point straight at, and which matches upstream's phase language
//! (`seed` verifies the formula is cookable; `pour` instantiates it):
//!
//! * **`seed <template>`** compiles the named formula to prove it is well-formed
//!   and then plants a single **dormant** molecule container, recording the
//!   formula's *source* in the container's metadata. No steps yet. Because it
//!   lands exactly one issue through the single write path, it needs no
//!   `recompute_blocked`.
//! * **`pour <id>`** grows that container: it re-cooks the recorded source and
//!   materializes the steps as real `parent-child` children, wiring the
//!   inter-step edges. This is where a whole graph lands at once, so it
//!   recomputes the blocked cache exactly the way `bd cook` and `bd import` do.
//!
//! Pouring an already-poured molecule is refused rather than silently
//! duplicating its steps.
//!
//! `seed` binds `--var KEY=VALUE` (like `bd cook`) and records the bindings on
//! the molecule, so `pour` re-cooks with exactly what `seed` validated. A formula
//! with a required, default-less variable that is not supplied fails at `seed`
//! with an honest exit 1 naming the variable — before any molecule is planted.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bd_core::{
    Dependency, DependencyType, Issue, IssueFilter, IssueType, Priority, SortPolicy, Status,
    WispType,
};
use bd_formula::Bindings;
use bd_storage::{Field, IssuePatch, Storage};
use chrono::{Duration, Utc};
use serde_json::{Value, json};

use crate::cli::MolCmd;
use crate::commands::stub;
use crate::context::Ctx;
use crate::output::issue_json;

/// Where formulas live, relative to the workspace. Mirrors `formula.rs`.
const FORMULA_DIR: &str = "formulas";

/// `--var key=value` pairs into a map, the same rule `bd cook` uses. A malformed
/// pair is a usage error naming it, never a silent skip that would leave a
/// required variable unbound.
fn parse_vars(pairs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| anyhow!("--var must be KEY=VALUE, got {p:?}"))?;
        if k.is_empty() {
            bail!("--var has an empty key: {p:?}");
        }
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

/// A molecule with no children older than this counts as `mol stale`.
const STALE_AFTER_DAYS: i64 = 14;

/// The default TTL class of a hand-made wisp. Patrol (24h) is long enough that a
/// bead someone typed in the morning survives until they come back, short enough
/// to stay genuinely ephemeral, and `bd promote` rescues anything worth keeping.
/// A `--type` flag would let the caller choose; see the port notes.
const DEFAULT_WISP_TYPE: WispType = WispType::Patrol;

pub async fn mol(ctx: &Ctx, cmd: MolCmd) -> Result<()> {
    match cmd {
        MolCmd::Wisp { title } => wisp(ctx, &title).await,
        MolCmd::Seed { template, vars } => seed(ctx, &template, &vars).await,
        MolCmd::Pour { id } => pour(ctx, &id).await,
        MolCmd::Show { id } => show(ctx, &id).await,
        MolCmd::Ready => ready(ctx).await,
        MolCmd::Current => current(ctx).await,
        MolCmd::Stale => stale(ctx).await,
        MolCmd::Burn { id } => burn(ctx, &id).await,
        MolCmd::Squash { id } => squash(ctx, &id).await,
        MolCmd::Bond { ids } => bond(ctx, &ids).await,
        // Distilling an epic back into a reusable *formula* means writing a
        // `.formula.toml` with `{{var}}` placeholders — and doing that usefully
        // needs the `--var` substitutions and an `--output` path that
        // `MolCmd::Distill { id }` does not carry. Without variables it could only
        // emit a literal, un-parameterized formula, which is the one thing distill
        // exists *not* to produce. An honest exit 64 rather than a distill that
        // lies about being reusable. See the port notes for the args it needs.
        MolCmd::Distill { .. } => stub("mol distill", ctx),
    }
}

// ---------------------------------------------------------------------------
// wisp
// ---------------------------------------------------------------------------

/// `bd mol wisp <title…>` — create an ephemeral bead.
///
/// A wisp is an ordinary row with `ephemeral = true` and a `wisp_type` that
/// declares its TTL. `bd promote` turns one into a real bead (clearing both), and
/// `bd gc` reaps expired ones. The title arrives as words so `bd mol wisp check
/// the logs` works unquoted, exactly like `bd comment`/`bd note`.
async fn wisp(ctx: &Ctx, title: &[String]) -> Result<()> {
    ctx.ensure_writable("create a wisp")?;
    let store = ctx.store().await?;

    let title = title.join(" ");
    let title = title.trim();
    if title.is_empty() {
        bail!("a wisp needs a title");
    }

    let prefix = ctx.prefix().await;
    let id = store.next_id(&prefix, title, "").await?;

    let mut issue = Issue::new(&id, title);
    issue.issue_type = IssueType::from(ctx.config.defaults.issue_type.clone());
    issue.priority = Priority::new(ctx.config.defaults.priority).unwrap_or_default();
    issue.created_by = ctx.identity.actor.clone();
    // Both halves together: ephemeral hides it from the commit graph, and the
    // wisp type is what `bd gc` reads to decide when it may be reaped.
    issue.ephemeral = true;
    issue.wisp_type = Some(DEFAULT_WISP_TYPE);
    issue.validate()?;

    let created = store.create_issue(&issue).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&created, &[], &[], &[]))?;
    } else {
        ctx.out
            .line(format!("Created wisp {} (ephemeral; `bd promote` to keep)", created.id));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// seed / pour
// ---------------------------------------------------------------------------

/// What `seed` records under an issue's `metadata`, so `pour` can grow the
/// molecule without going back to the filesystem.
const MOL_META_KEY: &str = "molecule";

/// `bd mol seed <template>` — plant a dormant molecule from a formula.
///
/// Compiles the formula first (parse → bind defaults → cook) purely to prove it
/// is well-formed and supported — an unsupported construct is a capability gap
/// (exit 2), a broken formula or missing required var is a plain failure (exit
/// 1). Only then is the container created, carrying the formula's source so
/// `pour` is self-contained.
async fn seed(ctx: &Ctx, template: &str, vars: &[String]) -> Result<()> {
    ctx.ensure_writable("seed a molecule")?;

    let path = resolve_formula(ctx, template)?;
    let src = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read formula {}", path.display()))?;
    let provided = parse_vars(vars)?;
    // Compile now, with the caller's bindings, to fail fast: a required variable
    // that was not supplied is caught here — an honest exit 1 naming it — rather
    // than at `pour` time against a molecule already planted.
    let formula = bd_formula::parse(&src).map_err(formula_err)?;
    let bindings = Bindings::bind(&formula, &provided).map_err(formula_err)?;
    let _plan = bd_formula::cook(&formula, &bindings).map_err(formula_err)?;

    let store = ctx.store().await?;
    let prefix = ctx.prefix().await;
    let id = store
        .next_id(&prefix, &formula.formula, &formula.description)
        .await?;

    let mut container = Issue::new(&id, &formula.formula);
    container.description = formula.description.clone();
    container.issue_type = IssueType::Molecule;
    container.created_by = ctx.identity.actor.clone();
    // The bindings are recorded next to the source, so `pour` re-cooks with
    // exactly what `seed` validated — a molecule carries its whole recipe.
    container.metadata = Some(json!({
        MOL_META_KEY: {
            "formula": formula.formula,
            "source": src,
            "vars": provided,
            "poured": false,
        }
    }));
    container.validate()?;

    // One issue through the single write path — the blocked cache stays correct
    // without a recompute. The whole graph lands later, at `pour`.
    let created = store.create_issue(&container).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "id": created.id,
            "formula": formula.formula,
            "poured": false,
            "title": created.title,
        }))?;
    } else {
        ctx.out.line(format!(
            "Seeded molecule {} from `{}` — run `bd mol pour {}` to materialize its steps",
            created.id, formula.formula, created.id
        ));
    }
    Ok(())
}

/// `bd mol pour <id>` — grow a seeded molecule into real work.
///
/// Re-cooks the formula source recorded at seed time and materializes each step
/// as a real child of the container, then recomputes the blocked cache so
/// `bd ready` is right on the very next call.
async fn pour(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("pour a molecule")?;
    let store = ctx.store().await?;

    let container = require_molecule(store, id).await?;
    let meta = container
        .metadata
        .as_ref()
        .and_then(|m| m.get(MOL_META_KEY))
        .ok_or_else(|| anyhow!("{id} is not a seeded molecule (use `bd mol seed`)"))?;

    if meta.get("poured").and_then(Value::as_bool).unwrap_or(false) {
        bail!("{id} has already been poured; its steps exist (see `bd mol show {id}`)");
    }
    let source = meta
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{id} has no recorded formula source to pour"))?;
    let formula_name = meta
        .get("formula")
        .and_then(Value::as_str)
        .unwrap_or(container.title.as_str())
        .to_string();

    // The bindings `seed` validated and recorded. An older molecule seeded before
    // `--var` existed simply has none, which binds to defaults — the previous
    // behaviour, preserved.
    let recorded: BTreeMap<String, String> = meta
        .get("vars")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let formula = bd_formula::parse(source).map_err(formula_err)?;
    let bindings = Bindings::bind(&formula, &recorded).map_err(formula_err)?;
    let plan = bd_formula::cook(&formula, &bindings).map_err(formula_err)?;

    let created = materialize(ctx, store, &container.id, &plan).await?;

    // Mark the container poured so a second pour cannot duplicate the work. The
    // recorded source and vars are carried through unchanged.
    let patch = IssuePatch {
        metadata: Field::Set(json!({
            MOL_META_KEY: {
                "formula": formula_name,
                "source": source,
                "vars": recorded,
                "poured": true,
            }
        })),
        ..Default::default()
    };
    store.update_issue(&container.id, &patch).await?;

    // A pour lands a whole graph the way a cook does — closed blockers and new
    // edges no single write path saw in order — so recompute once, for exactly
    // the reason `bd cook` and `bd import` do.
    store.recompute_blocked().await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "molecule": container.id,
            "formula": formula_name,
            "created": created.len(),
            "ids": created,
        }))?;
    } else {
        ctx.out.line(format!(
            "Poured {}: materialized {} step(s)",
            container.id,
            created.len()
        ));
    }
    Ok(())
}

/// Turn a compiled plan into real child issues under `container_id`.
///
/// The plan is in dependency order, so a forward pass resolves every local id to
/// a real one before any edge names it — the same shape as `bd cook`'s
/// `create_from_plan`, with one addition: each plan issue that is not already a
/// `parent-child` child of another plan issue (a loop body, say) is hung under
/// the molecule container, so the container is the single root of the group.
async fn materialize(
    ctx: &Ctx,
    store: &dyn Storage,
    container_id: &str,
    plan: &bd_formula::Plan,
) -> Result<Vec<String>> {
    let prefix = ctx.prefix().await;
    let mut real: BTreeMap<String, String> = BTreeMap::new();

    for proto in &plan.issues {
        let id = store.next_id(&prefix, &proto.title, &proto.description).await?;

        let mut issue = Issue::new(&id, &proto.title);
        issue.description = proto.description.clone();
        issue.notes = proto.notes.clone();
        issue.issue_type = proto.issue_type.clone();
        issue.priority = proto
            .priority
            .unwrap_or(Priority::new(ctx.config.defaults.priority).unwrap_or_default());
        issue.created_by = ctx.identity.actor.clone();
        issue.labels = proto.labels.clone();
        issue.metadata = proto.metadata.clone();
        // A gate issue is a wait, not workable material; the store excludes the
        // `gate` type from ready-work on its own, so recording the policy in
        // metadata is enough for downstream tooling. Same shape as `bd cook`.
        if let Some(g) = &proto.gate {
            issue.metadata = Some(json!({
                "gate": { "await_type": g.await_type, "await_id": g.await_id, "timeout": g.timeout }
            }));
        }
        issue.validate()?;

        let made = store.create_issue(&issue).await?;
        for l in &proto.labels {
            if !made.labels.iter().any(|x| x == l) {
                store.add_label(&made.id, l).await?;
            }
        }
        real.insert(proto.local_id.clone(), made.id);
    }

    // Inter-step edges, and the set of plan issues that already have an in-plan
    // parent (so they are not double-parented onto the container).
    let mut has_parent: BTreeSet<String> = BTreeSet::new();
    for dep in &plan.deps {
        let from = real
            .get(&dep.dependent)
            .ok_or_else(|| anyhow!("plan edge names unknown issue `{}`", dep.dependent))?;
        let to = real
            .get(&dep.prerequisite)
            .ok_or_else(|| anyhow!("plan edge names unknown issue `{}`", dep.prerequisite))?;
        let mut d = Dependency::new(from, to, DependencyType::from(dep.kind.to_string()))?;
        d.created_by = ctx.identity.actor.clone();
        store.add_dependency(&d).await?;
        if dep.kind == DependencyType::ParentChild.as_str() {
            has_parent.insert(dep.dependent.clone());
        }
    }

    // Hang the plan's roots under the container. A child holds the parent-child
    // edge (`child --parent-child--> parent`), so the container is `depends_on`.
    for proto in &plan.issues {
        if has_parent.contains(&proto.local_id) {
            continue;
        }
        let child = &real[&proto.local_id];
        let mut d = Dependency::new(child, container_id, DependencyType::ParentChild)?;
        d.created_by = ctx.identity.actor.clone();
        store.add_dependency(&d).await?;
    }

    Ok(plan.issues.iter().map(|p| real[&p.local_id].clone()).collect())
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

/// `bd mol show <id>` — the molecule and its children.
async fn show(ctx: &Ctx, id: &str) -> Result<()> {
    let store = ctx.store().await?;
    let molecule = require_molecule(store, id).await?;

    let poured = molecule
        .metadata
        .as_ref()
        .and_then(|m| m.get(MOL_META_KEY))
        .and_then(|m| m.get("poured"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut kids = store.get_issues(&child_ids(store, id).await?).await?;
    kids.sort_by(by_priority_then_id);

    if ctx.out.is_json() {
        // The molecule's own fields, with its children *beside* them — the same
        // shape `bd epic status` uses, so an agent that parses one parses both.
        let mut v = serde_json::to_value(&molecule).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("children".into(), serde_json::to_value(&kids).unwrap_or(Value::Null));
            o.insert("children_total".into(), json!(kids.len()));
            o.insert(
                "children_closed".into(),
                json!(kids.iter().filter(|k| k.status.is_closed()).count()),
            );
            o.insert("poured".into(), json!(poured));
        }
        return ctx.out.json_value(&v);
    }

    ctx.out.line(format!("{}  {}", molecule.id, molecule.title));
    let closed = kids.iter().filter(|k| k.status.is_closed()).count();
    ctx.out.line(format!(
        "  molecule  poured: {poured}  steps: {}/{} closed",
        closed,
        kids.len()
    ));
    if kids.is_empty() {
        if poured {
            ctx.out.line("  (no steps)");
        } else {
            ctx.out
                .line(format!("  (not poured yet; run `bd mol pour {}`)", molecule.id));
        }
        return Ok(());
    }
    ctx.out.issues(&kids)
}

// ---------------------------------------------------------------------------
// ready / current / stale
// ---------------------------------------------------------------------------

/// `bd mol ready` — molecules with claimable work.
///
/// A molecule is ready when at least one of its children is in `bd ready` — i.e.
/// workable, unblocked, unheld, not deferred. The ready set is computed once and
/// then each molecule is asked whether it holds any of it.
async fn ready(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;

    let ready_ids: HashSet<String> = store
        .ready_work(&IssueFilter::ready())
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();

    let mut out = Vec::new();
    for m in open_molecules(store).await? {
        let kids = child_ids(store, &m.id).await?;
        if kids.iter().any(|k| ready_ids.contains(k)) {
            out.push(m);
        }
    }
    out.sort_by(by_priority_then_id);
    ctx.out.issues(&out)
}

/// `bd mol current` — molecules being worked right now.
///
/// A molecule is "current" when one of its children is `in_progress`. The
/// in-progress set is one query; each molecule is then checked against it.
async fn current(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;

    let mut f = IssueFilter::new();
    f.statuses = vec![Status::InProgress];
    let in_progress: HashSet<String> = store
        .list_issues(&f)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();

    let mut out = Vec::new();
    for m in open_molecules(store).await? {
        let kids = child_ids(store, &m.id).await?;
        if kids.iter().any(|k| in_progress.contains(k)) {
            out.push(m);
        }
    }
    out.sort_by(by_priority_then_id);

    if !ctx.out.is_json() && out.is_empty() {
        ctx.out.line("No molecules in progress.");
        return Ok(());
    }
    ctx.out.issues(&out)
}

/// `bd mol stale` — molecules nobody has touched in a while.
///
/// Least-recently-touched first, ordered in SQL (not in memory) for the same
/// reason `bd stale` is: a limit applied by the database under one order and
/// re-sorted here under another would silently return the wrong page.
async fn stale(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let cutoff = Utc::now() - Duration::days(STALE_AFTER_DAYS);

    let mut f = IssueFilter::new();
    f.issue_type = Some(IssueType::Molecule);
    f.exclude_statuses = vec![Status::Closed];
    f.updated_before = Some(cutoff);
    f.sort = SortPolicy::Updated;

    let issues = store.list_issues(&f).await?;

    ctx.out.line(format!(
        "Molecules untouched since {}:",
        cutoff.format("%Y-%m-%d %H:%M")
    ));
    ctx.out.issues(&issues)
}

// ---------------------------------------------------------------------------
// burn / squash
// ---------------------------------------------------------------------------

/// `bd mol burn <id>` — destroy a molecule and everything under it.
///
/// Unlike `squash`, burn keeps no trace: the container and its whole
/// `parent-child` subtree are deleted. Recomputes the blocked cache afterward,
/// because a bulk delete removes gating edges no single write path saw go.
async fn burn(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("burn a molecule")?;
    let store = ctx.store().await?;

    let molecule = require_molecule(store, id).await?;
    let descendants = descendants(store, &molecule.id).await?;

    for d in &descendants {
        store.delete_issue(d).await?;
    }
    store.delete_issue(&molecule.id).await?;
    store.recompute_blocked().await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "molecule": molecule.id,
            "deleted": descendants,
            "count": descendants.len() + 1,
        }))?;
    } else {
        ctx.out.line(format!(
            "Burned molecule {} and {} descendant(s); no digest kept",
            molecule.id,
            descendants.len()
        ));
    }
    Ok(())
}

/// `bd mol squash <id>` — collapse a molecule into one bead.
///
/// Writes a single closed **digest** bead summarizing the molecule's direct
/// steps, then deletes the container and its whole subtree — the digest replaces
/// the molecule. Unlike `burn`, the outcome survives; unlike leaving it open,
/// nothing is left to gate work.
async fn squash(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("squash a molecule")?;
    let store = ctx.store().await?;

    let molecule = require_molecule(store, id).await?;
    let direct = store.get_issues(&child_ids(store, &molecule.id).await?).await?;
    let all = descendants(store, &molecule.id).await?;

    let digest_body = digest(&molecule, &direct);
    let prefix = ctx.prefix().await;
    let did = store.next_id(&prefix, &molecule.title, &digest_body).await?;

    let mut digest_issue = Issue::new(&did, format!("Digest: {}", molecule.title));
    digest_issue.description = digest_body;
    digest_issue.issue_type = IssueType::Task;
    digest_issue.created_by = ctx.identity.actor.clone();
    digest_issue.validate()?;

    let made = store.create_issue(&digest_issue).await?;
    let closed = store
        .close_issue(&made.id, &format!("squashed {} step(s) from {}", direct.len(), molecule.id))
        .await?;

    for d in &all {
        store.delete_issue(d).await?;
    }
    store.delete_issue(&molecule.id).await?;
    store.recompute_blocked().await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "molecule": molecule.id,
            "digest": closed.id,
            "squashed": all.len() + 1,
        }))?;
    } else {
        ctx.out.line(format!(
            "Squashed molecule {} into digest {} ({} step(s))",
            molecule.id,
            closed.id,
            direct.len()
        ));
    }
    Ok(())
}

/// A plain-text summary of a molecule's steps and their outcomes.
fn digest(root: &Issue, children: &[Issue]) -> String {
    let mut s = String::new();
    s.push_str(&format!("Molecule: {}\n", root.title));
    let closed = children.iter().filter(|c| c.status.is_closed()).count();
    s.push_str(&format!("Completed: {}/{}\n\n", closed, children.len()));
    for (n, c) in children.iter().enumerate() {
        s.push_str(&format!("{}. [{}] {}\n", n + 1, c.status.as_str(), c.title));
        if !c.close_reason.is_empty() {
            s.push_str(&format!("   outcome: {}\n", c.close_reason));
        }
    }
    s
}

// ---------------------------------------------------------------------------
// bond
// ---------------------------------------------------------------------------

/// `bd mol bond <ids…>` — join molecules by association.
///
/// Chains the molecules with `related` edges so they read as one group. `related`
/// is deliberately *not* a gating edge — bonding two molecules must not make
/// either one block the other — so no blocked-cache recompute is needed.
async fn bond(ctx: &Ctx, ids: &[String]) -> Result<()> {
    ctx.ensure_writable("bond molecules")?;
    let store = ctx.store().await?;

    if ids.len() < 2 {
        bail!("bond needs at least two molecules");
    }
    for id in ids {
        require_molecule(store, id).await?;
    }

    let mut edges = 0usize;
    for pair in ids.windows(2) {
        let mut d = Dependency::new(&pair[1], &pair[0], DependencyType::Related)?;
        d.created_by = ctx.identity.actor.clone();
        store.add_dependency(&d).await?;
        edges += 1;
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "bonded": ids, "related_edges": edges }))?;
    } else {
        ctx.out.line(format!("Bonded {} molecule(s)", ids.len()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared machinery
// ---------------------------------------------------------------------------

/// The issue, required to exist and to be a molecule. `bd mol …` on a bug should
/// say so rather than quietly operating on the wrong kind of bead.
async fn require_molecule(store: &dyn Storage, id: &str) -> Result<Issue> {
    let issue = store
        .get_issue(id)
        .await?
        .ok_or_else(|| anyhow!("molecule not found: {id}"))?;
    if issue.issue_type != IssueType::Molecule {
        bail!("{id} is a {}, not a molecule", issue.issue_type);
    }
    Ok(issue)
}

/// Open (non-closed) molecule containers.
async fn open_molecules(store: &dyn Storage) -> Result<Vec<Issue>> {
    let mut f = IssueFilter::new();
    f.issue_type = Some(IssueType::Molecule);
    f.exclude_statuses = vec![Status::Closed];
    Ok(store.list_issues(&f).await?)
}

/// The direct children of `id`: a child holds the `parent-child` edge, so they
/// are its *dependents* (`child --parent-child--> parent`).
async fn child_ids(store: &dyn Storage, id: &str) -> Result<Vec<String>> {
    Ok(store
        .dependents_of(id)
        .await?
        .into_iter()
        .filter(|d| d.dep_type == DependencyType::ParentChild)
        .map(|d| d.issue_id)
        .collect())
}

/// Every issue below `root` in the `parent-child` subtree, root excluded. Guards
/// against cycles so a malformed graph cannot spin here forever.
async fn descendants(store: &dyn Storage, root: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue = child_ids(store, root).await?;
    while let Some(id) = queue.pop() {
        if id == root || !seen.insert(id.clone()) {
            continue;
        }
        queue.extend(child_ids(store, &id).await?);
        out.push(id);
    }
    Ok(out)
}

fn by_priority_then_id(a: &Issue, b: &Issue) -> std::cmp::Ordering {
    a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id))
}

/// A formula named `x` is `x`, `x.formula.toml`, or `x.toml` under the workspace
/// formula dir — or a path, if it looks like one. Mirrors `formula.rs`, which
/// owns the private original; this port draws the seam at the file, not the
/// function, so `mol` and `formula` do not depend on each other's internals.
fn resolve_formula(ctx: &Ctx, name: &str) -> Result<PathBuf> {
    let direct = Path::new(name);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let dir = ctx
        .beads_dir
        .as_ref()
        .map(|d| d.join(FORMULA_DIR))
        .ok_or_else(|| anyhow!("no beads workspace here (run `bd init`)"))?;
    for candidate in [
        format!("{name}.formula.toml"),
        format!("{name}.toml"),
        name.to_string(),
    ] {
        let p = dir.join(candidate);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("no formula named `{name}` in {}", dir.display())
}

/// Formula errors carry their own exit meaning: an `Unsupported` stays a
/// `bd_storage::Error::Unsupported` so the top-level reporter exits 2 for it, not
/// 1. Everything else is the author's or caller's to fix. Mirrors `formula.rs`.
fn formula_err(e: bd_formula::Error) -> anyhow::Error {
    match e {
        bd_formula::Error::Unsupported(what) => bd_storage::Error::unsupported_hint(
            "mol",
            "formula",
            format!("{what} is not built in this port yet"),
        )
        .into(),
        other => anyhow!(other.to_string()),
    }
}
