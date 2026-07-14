//! `bd cook` and `bd formula …` — the CLI over the formula compiler.
//!
//! The compiler ([`bd_formula`]) is pure: TOML in, a [`Plan`] of proto-issues
//! out. This file is the impure half — read the file, bind the `--var`s, and
//! turn the plan into real issues through `Storage`. The split is deliberate:
//! everything that could be wrong about *compiling* a formula is tested without a
//! database, and all that is left here is the mechanical translation of local
//! ids to real ones.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bd_core::{Dependency, DependencyType, Issue, Priority};
use bd_formula::{Bindings, Plan, cook as compile};
use serde_json::json;

use crate::cli::FormulaCmd;
use crate::context::Ctx;

// ---------------------------------------------------------------------------
// bd cook
// ---------------------------------------------------------------------------

/// Compile a formula and create the issues it describes.
///
/// `--dry-run` stops after compiling and prints the plan — which is the only
/// thing you want when writing a formula, and costs nothing because compilation
/// never touches the store.
pub async fn cook(ctx: &Ctx, path: PathBuf, vars: Vec<String>, dry_run: bool) -> Result<()> {
    let src = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read formula {}", path.display()))?;
    let formula = bd_formula::parse(&src).map_err(anyhow_from)?;
    let bindings = Bindings::bind(&formula, &parse_vars(&vars)?).map_err(anyhow_from)?;
    let plan = compile(&formula, &bindings).map_err(anyhow_from)?;

    if dry_run || ctx.readonly {
        return print_plan(ctx, &plan);
    }

    ctx.ensure_writable("cook a formula")?;
    let created = create_from_plan(ctx, &plan).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "formula": plan.formula,
            "created": created.len(),
            "ids": created.values().collect::<Vec<_>>(),
        }))?;
    } else {
        ctx.out.line(format!(
            "Cooked `{}`: created {} issue(s)",
            plan.formula,
            created.len()
        ));
    }
    Ok(())
}

/// Turn a compiled plan into issues and dependencies.
///
/// The plan is in dependency order, so a simple forward pass works: every issue
/// is created (and its local→real id recorded) before any edge that could name
/// it. The whole thing runs without a transaction because the seam has none to
/// offer — a failure part-way leaves a partial molecule, which `bd doctor`'s
/// orphan checks can see, rather than a silent rollback the user cannot.
async fn create_from_plan(ctx: &Ctx, plan: &Plan) -> Result<BTreeMap<String, String>> {
    let store = ctx.store().await?;
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
        // metadata is enough for downstream tooling to resume it.
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
    }

    // A cook lands a whole graph the way an import does — closed blockers and
    // new edges no single write path saw in order — so the blocked cache is
    // recomputed once, for the same reason import does it.
    store.recompute_blocked().await?;
    Ok(real)
}

fn print_plan(ctx: &Ctx, plan: &Plan) -> Result<()> {
    if ctx.out.is_json() {
        let issues: Vec<_> = plan
            .issues
            .iter()
            .map(|i| {
                json!({
                    "local_id": i.local_id,
                    "title": i.title,
                    "type": i.issue_type.as_str(),
                    "gate": i.gate.is_some(),
                })
            })
            .collect();
        let deps: Vec<_> = plan
            .deps
            .iter()
            .map(|d| json!({ "from": d.dependent, "to": d.prerequisite, "type": d.kind }))
            .collect();
        return ctx.out.json_value(&json!({
            "formula": plan.formula,
            "issues": issues,
            "deps": deps,
        }));
    }

    ctx.out.line(format!(
        "{} would create {} issue(s):",
        plan.formula,
        plan.issues.len()
    ));
    for i in &plan.issues {
        let kind = if i.gate.is_some() {
            " [gate]".to_string()
        } else {
            String::new()
        };
        ctx.out.line(format!("  {} — {}{kind}", i.local_id, i.title));
    }
    if !plan.deps.is_empty() {
        ctx.out.line(format!("and {} dependency(ies):", plan.deps.len()));
        for d in &plan.deps {
            ctx.out
                .line(format!("  {} {} {}", d.dependent, d.kind, d.prerequisite));
        }
    }
    Ok(())
}

/// `--var key=value` into a map. A malformed pair is a usage error naming what
/// was wrong, not a silent skip — a dropped `--var` sends the whole cook down the
/// "required variable not provided" path with no hint the value was even given.
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

// ---------------------------------------------------------------------------
// bd formula …
// ---------------------------------------------------------------------------

/// Where formulas live, relative to the workspace.
const FORMULA_DIR: &str = "formulas";

pub async fn formula(ctx: &Ctx, cmd: FormulaCmd) -> Result<()> {
    match cmd {
        FormulaCmd::List => list(ctx),
        FormulaCmd::Show { name } => show(ctx, &name),
        FormulaCmd::Schema => schema(ctx),
        // `convert` translated between the old JSON and new TOML formats. This
        // port only ever spoke TOML, so there is nothing to convert *from* —
        // it stays an honest exit 64 rather than a no-op that looks like success.
        FormulaCmd::Convert { .. } => crate::commands::stub("formula convert", ctx),
    }
}

fn formula_dir(ctx: &Ctx) -> Option<PathBuf> {
    ctx.beads_dir.as_ref().map(|d| d.join(FORMULA_DIR))
}

fn list(ctx: &Ctx) -> Result<()> {
    let Some(dir) = formula_dir(ctx) else {
        bail!("no beads workspace here (run `bd init`)");
    };
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("toml")
                && let Ok(src) = std::fs::read_to_string(&p)
                && let Ok(f) = bd_formula::parse(&src)
            {
                found.push((f.formula, f.description));
            }
        }
    }
    found.sort();

    if ctx.out.is_json() {
        let arr: Vec<_> = found
            .iter()
            .map(|(n, d)| json!({ "formula": n, "description": d }))
            .collect();
        return ctx.out.json_value(&arr);
    }
    if found.is_empty() {
        ctx.out
            .line(format!("no formulas in {}", dir.display()));
    } else {
        for (n, d) in &found {
            ctx.out.line(format!("{n}  {d}"));
        }
    }
    Ok(())
}

fn show(ctx: &Ctx, name: &str) -> Result<()> {
    let path = resolve_formula(ctx, name)?;
    let src = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let f = bd_formula::parse(&src).map_err(anyhow_from)?;

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "formula": f.formula,
            "description": f.description,
            "type": f.kind.as_str(),
            "vars": f.vars.keys().collect::<Vec<_>>(),
            "steps": f.steps.iter().map(|s| &s.id).collect::<Vec<_>>(),
        }));
    }
    ctx.out.line(format!("{} — {}", f.formula, f.description));
    ctx.out.line(format!("type: {}", f.kind.as_str()));
    if !f.vars.is_empty() {
        ctx.out.line("vars:");
        for (name, def) in &f.vars {
            let req = if def.required { " (required)" } else { "" };
            ctx.out.line(format!("  {name}{req}  {}", def.description));
        }
    }
    ctx.out.line("steps:");
    for s in &f.steps {
        ctx.out.line(format!("  {} — {}", s.id, s.title));
    }
    Ok(())
}

/// A formula named `x` is `x`, `x.formula.toml`, or `x.toml` under the workspace
/// formula dir — or a path, if it looks like one.
fn resolve_formula(ctx: &Ctx, name: &str) -> Result<PathBuf> {
    let direct = Path::new(name);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let dir =
        formula_dir(ctx).ok_or_else(|| anyhow!("no beads workspace here (run `bd init`)"))?;
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

fn schema(ctx: &Ctx) -> Result<()> {
    // The compiler is the source of truth for what a formula may contain, so the
    // schema is described from the same vocabulary the parser accepts rather than
    // from a separate document that could drift out of step with it.
    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "version": 1,
            "types": ["workflow", "expansion", "aspect", "convoy"],
            "supported_types": ["workflow"],
            "step_fields": [
                "id", "title", "description", "notes", "type", "priority",
                "labels", "metadata", "needs", "depends_on", "condition", "loop", "gate"
            ],
            "primitives": {
                "condition": "{{var}} == value  (==, !=, >, >=, <, <=)",
                "loop": "range = \"1..3\" | count = 3, with var and body",
                "gate": "type/await_id/timeout — splits into a wait issue"
            }
        }));
    }
    ctx.out.line("formula schema, version 1");
    ctx.out.line("");
    ctx.out.line("A formula has: formula, description, version=1, type, vars, steps.");
    ctx.out.line("A step has: id, title, description, needs/depends_on, and optionally");
    ctx.out.line("  condition  — include the step only if `{{var}} == value` holds");
    ctx.out.line("  loop       — range=\"1..3\" or count=N, with var and body (fans out)");
    ctx.out.line("  gate       — type/await_id/timeout (splits into a wait issue)");
    ctx.out.line("");
    ctx.out
        .line("Only `type = workflow` cooks so far; extends and advice are not yet built.");
    Ok(())
}

/// Formula errors are the author's or caller's to fix, and carry their own exit
/// meaning — but the CLI speaks `anyhow`, so they cross over here. An
/// `Unsupported` keeps its identity as a `bd_storage::Error::Unsupported` so the
/// top-level reporter still exits 2 for it, not 1.
fn anyhow_from(e: bd_formula::Error) -> anyhow::Error {
    match e {
        bd_formula::Error::Unsupported(what) => bd_storage::Error::unsupported_hint(
            "cook",
            "formula",
            format!("{what} is not built in this port yet"),
        )
        .into(),
        other => anyhow!(other.to_string()),
    }
}
