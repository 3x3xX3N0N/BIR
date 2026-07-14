//! Cooking: a [`Formula`] plus [`Bindings`] becomes a [`Plan`] of proto-issues
//! and the edges between them.
//!
//! This is the compiler. Its output is deliberately *not* issues — it is a
//! description of issues, with **local** ids (the step ids, suffixed where a step
//! fans out). The caller mints real ids, creates the issues, and translates the
//! local edges into real ones. Keeping cook pure is what lets every rule below be
//! tested with a string and an assertion.
//!
//! Three transformations happen here, and the order is load-bearing:
//!
//! 1. **Conditions** decide which steps exist at all. A dropped step is not just
//!    removed — every edge that pointed *through* it is rewired to what it
//!    depended on, so a `report needs deploy needs build` graph with `deploy`
//!    excluded becomes `report needs build`, never a dangling edge to a step
//!    that was never created.
//! 2. **Loops** fan one step body into N, binding an iteration variable in each.
//!    The bodies hang under a parent issue so the group stays legible and
//!    readiness propagates.
//! 3. **Gates** split a step into a wait and the work that follows it.
//!
//! What is deliberately refused, loudly, rather than half-done: `extends`,
//! `advice`, and every formula `type` but `workflow`. Weaving one graph into
//! another is a compiler's hard half, and a plausible-but-wrong weave is worse
//! than an honest [`Error::Unsupported`].

use std::collections::{BTreeMap, BTreeSet};

use bd_core::{IssueType, Priority};

use crate::eval;
use crate::types::{Formula, FormulaType, GateSpec, Step};
use crate::vars::Bindings;
use crate::{Error, Result};

/// A cooked formula: the issues to create and the edges between them, in an
/// order where every prerequisite is created before the thing that needs it.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The formula's name, for the caller to title a root/container issue.
    pub formula: String,
    /// Whether the caller should materialize each step as its own issue
    /// (`pour = true`) or keep them inline. Carried through untouched.
    pub pour: bool,
    /// Proto-issues, in dependency order: a prerequisite always precedes its
    /// dependents, so a caller creating them in sequence can resolve local ids
    /// to real ones as it goes.
    pub issues: Vec<ProtoIssue>,
    pub deps: Vec<ProtoDep>,
}

impl Plan {
    /// The proto-issue with this local id, if any.
    pub fn issue(&self, local_id: &str) -> Option<&ProtoIssue> {
        self.issues.iter().find(|i| i.local_id == local_id)
    }
}

/// An issue described but not yet created. `local_id` is unique within the plan
/// and is what [`ProtoDep`] references.
#[derive(Debug, Clone)]
pub struct ProtoIssue {
    pub local_id: String,
    pub title: String,
    pub description: String,
    pub notes: String,
    pub issue_type: IssueType,
    /// `None` means "let the store apply its default priority".
    pub priority: Option<Priority>,
    pub labels: Vec<String>,
    pub metadata: Option<serde_json::Value>,
    /// Present iff this proto-issue is the wait half of a gate. The caller
    /// records the wait's type/timeout wherever it keeps gate state.
    pub gate: Option<GateInfo>,
}

#[derive(Debug, Clone)]
pub struct GateInfo {
    pub await_type: String,
    pub await_id: Option<String>,
    pub timeout: Option<String>,
}

/// An edge. `dependent` is blocked until `prerequisite` closes — the same
/// direction the store uses (`issue_id` depends on `depends_on_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoDep {
    pub dependent: String,
    pub prerequisite: String,
    /// The edge kind, as a store string (`blocks`, `parent-child`). A string,
    /// not a `bd_core::DependencyType`, so this crate need not track every edge
    /// variant the store grows.
    pub kind: &'static str,
}

/// Cook a formula into a plan.
pub fn cook(formula: &Formula, bindings: &Bindings) -> Result<Plan> {
    refuse_unsupported(formula)?;

    // Which top-level steps survive their condition, and — for the edge
    // rewiring — what each excluded step depended on. Computed up front so that
    // wiring can bypass any chain of excluded steps in one resolve.
    let included = evaluate_conditions(&formula.steps, bindings)?;

    let mut plan = Plan {
        formula: formula.formula.clone(),
        pour: formula.pour,
        issues: Vec::new(),
        deps: Vec::new(),
    };

    // Emit issues first, recording each declared step's "outputs": the local ids
    // a dependent on that step should block on. A plain step outputs itself; a
    // loop outputs all its iterations; an excluded step outputs nothing.
    let mut outputs: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for step in &formula.steps {
        if !included.contains(step.id.as_str()) {
            continue;
        }
        let out = emit_step(step, bindings, &mut plan)?;
        outputs.insert(step.id.clone(), out);
    }

    // Now the edges. `resolve` turns a declared dependency id into the set of
    // real local ids it should block on, transparently bypassing excluded steps.
    for step in &formula.steps {
        if !included.contains(step.id.as_str()) {
            continue;
        }
        // The local ids that represent this step as a *dependent*. For a plain
        // step, itself; for a loop, every iteration (each blocks independently);
        // for a gate step, the work half (already blocked on its own gate).
        let dependents = outputs.get(&step.id).cloned().unwrap_or_default();
        for blocker in step.blockers() {
            for prereq in resolve(&blocker, &formula.steps, &included, &outputs) {
                for dep in &dependents {
                    if dep != &prereq {
                        plan.deps.push(ProtoDep {
                            dependent: dep.clone(),
                            prerequisite: prereq.clone(),
                            kind: "blocks",
                        });
                    }
                }
            }
        }
    }

    Ok(plan)
}

/// Refuse what is parsed but not built, each with its own reason so a user does
/// not go hunting for a mistake in a correct formula.
fn refuse_unsupported(formula: &Formula) -> Result<()> {
    if !formula.extends.is_empty() {
        return Err(Error::Unsupported(
            "`extends` (formula inheritance)".into(),
        ));
    }
    if !formula.advice.is_empty() {
        return Err(Error::Unsupported(
            "`advice` (aspect-oriented step insertion)".into(),
        ));
    }
    if formula.kind != FormulaType::Workflow {
        return Err(Error::Unsupported(format!(
            "the `{}` formula type (only `workflow` cooks so far)",
            formula.kind.as_str()
        )));
    }
    Ok(())
}

/// Evaluate every top-level step's condition, returning the ids that survive.
fn evaluate_conditions<'a>(
    steps: &'a [Step],
    bindings: &Bindings,
) -> Result<BTreeSet<&'a str>> {
    let mut included = BTreeSet::new();
    for s in steps {
        let keep = match &s.condition {
            Some(c) => eval::holds(c, bindings)?,
            None => true,
        };
        if keep {
            included.insert(s.id.as_str());
        }
    }
    Ok(included)
}

/// The local ids a dependency on `id` resolves to.
///
/// If `id` is an included step, its recorded outputs. If it was excluded by its
/// condition, the union of resolving *its* blockers — so a dependency chain
/// through several dropped steps collapses to the first surviving ancestors,
/// and the graph never points at an issue that was never made.
fn resolve(
    id: &str,
    steps: &[Step],
    included: &BTreeSet<&str>,
    outputs: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    if included.contains(id) {
        return outputs.get(id).cloned().unwrap_or_default();
    }
    // Excluded: bypass to what it needed. Find the step and recurse.
    let Some(step) = steps.iter().find(|s| s.id == id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for blocker in step.blockers() {
        for r in resolve(&blocker, steps, included, outputs) {
            if !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// Emit the issue(s) for one included step, returning the local ids that
/// represent it as a dependency target.
fn emit_step(step: &Step, bindings: &Bindings, plan: &mut Plan) -> Result<Vec<String>> {
    // A loop fans the body out; everything else is a single (possibly gated)
    // issue. The two paths do not overlap — a step is one or the other.
    if let Some(loop_spec) = &step.loop_spec {
        return emit_loop(step, loop_spec, bindings, plan);
    }
    emit_single(step, &step.id, bindings, plan)
}

/// One step → one work issue, plus a gate issue if it waits. Returns the local
/// id a dependent should block on (the work issue, which already blocks on its
/// own gate).
fn emit_single(
    step: &Step,
    local_id: &str,
    bindings: &Bindings,
    plan: &mut Plan,
) -> Result<Vec<String>> {
    let work = build_issue(step, local_id, bindings)?;

    // A gate is split off *before* the work issue is pushed, so the plan stays
    // in dependency order: the wait exists before the thing that waits on it.
    if let Some(gate) = &step.gate {
        let gate_id = format!("{local_id}.gate");
        plan.issues.push(gate_issue(&gate_id, step, gate, bindings)?);
        plan.deps.push(ProtoDep {
            dependent: local_id.to_string(),
            prerequisite: gate_id,
            kind: "blocks",
        });
    }

    plan.issues.push(work);
    Ok(vec![local_id.to_string()])
}

/// A loop step → one parent container plus N body issues, each with its
/// iteration variable bound. Returns the iteration ids: a dependent on the loop
/// blocks on every iteration, because "needs the loop" means "needs them all".
fn emit_loop(
    step: &Step,
    loop_spec: &crate::types::LoopSpec,
    bindings: &Bindings,
    plan: &mut Plan,
) -> Result<Vec<String>> {
    let values = loop_values(loop_spec)?;

    // The container. Its title is the outer step's, substituted. It groups the
    // iterations; each iteration is its child. The container is not itself a
    // dependency target — dependents block on the iterations.
    let parent_id = step.id.clone();
    let mut parent = build_issue(step, &parent_id, bindings)?;
    // The container has no gate and no loop of its own; strip any inherited type
    // guess down to a plain grouping issue only if the author left it default.
    if step.issue_type.is_none() {
        parent.issue_type = IssueType::Task;
    }
    plan.issues.push(parent);

    let mut iteration_ids = Vec::new();
    for value in values {
        let scoped = bindings.with_loop_var(&loop_spec.var, value.to_string());
        // Body-internal edges resolve within the same iteration, so record each
        // body step's per-iteration local id as we go.
        let mut body_local: BTreeMap<&str, String> = BTreeMap::new();
        for body in &loop_spec.body {
            let local = format!("{}.{}#{value}", step.id, body.id);
            body_local.insert(body.id.as_str(), local.clone());

            let issue = build_issue(body, &local, &scoped)?;
            plan.issues.push(issue);

            // Each body issue is a child of the container.
            plan.deps.push(ProtoDep {
                dependent: local.clone(),
                prerequisite: parent_id.clone(),
                kind: "parent-child",
            });
            iteration_ids.push(local);
        }
        // Body steps that need a sibling body step: same iteration only.
        for body in &loop_spec.body {
            let here = &body_local[body.id.as_str()];
            for dep in body.blockers() {
                if let Some(target) = body_local.get(dep.as_str()) {
                    plan.deps.push(ProtoDep {
                        dependent: here.clone(),
                        prerequisite: target.clone(),
                        kind: "blocks",
                    });
                }
                // A body step needing an *outer* step is resolved by the caller
                // loop's edge pass, which only knows the loop's own `needs`. Body
                // → outer edges are intentionally out of scope until a real
                // formula needs them; flagging rather than silently dropping.
                else if !loop_spec.body.iter().any(|b| b.id == dep) {
                    return Err(Error::Unsupported(format!(
                        "loop body step `{}` needs `{dep}` outside the loop; only \
                         same-iteration dependencies are built",
                        body.id
                    )));
                }
            }
        }
    }
    Ok(iteration_ids)
}

/// Build one proto-issue from a step, substituting variables in its text.
fn build_issue(step: &Step, local_id: &str, bindings: &Bindings) -> Result<ProtoIssue> {
    let priority = match step.priority {
        Some(p) => Some(Priority::new(p).map_err(|e| Error::Invalid(e.to_string()))?),
        None => None,
    };
    let labels = step
        .labels
        .iter()
        .map(|l| bindings.substitute(l))
        .collect::<Result<Vec<_>>>()?;

    Ok(ProtoIssue {
        local_id: local_id.to_string(),
        title: bindings.substitute(&step.title)?,
        description: bindings.substitute(&step.description)?,
        notes: bindings.substitute(&step.notes)?,
        issue_type: step
            .issue_type
            .clone()
            .map(IssueType::from)
            .unwrap_or(IssueType::Task),
        priority,
        labels,
        metadata: step.metadata.clone(),
        gate: None,
    })
}

/// The wait half of a gated step: a `gate`-typed issue capturing the condition.
fn gate_issue(
    gate_id: &str,
    step: &Step,
    gate: &GateSpec,
    bindings: &Bindings,
) -> Result<ProtoIssue> {
    Ok(ProtoIssue {
        local_id: gate_id.to_string(),
        title: format!("Gate: {}", bindings.substitute(&step.title)?),
        description: String::new(),
        notes: String::new(),
        issue_type: IssueType::Gate,
        priority: None,
        labels: Vec::new(),
        metadata: None,
        gate: Some(GateInfo {
            await_type: gate.await_type.clone(),
            await_id: gate.await_id.clone(),
            timeout: gate.timeout.clone(),
        }),
    })
}

/// The concrete values a loop iterates over.
///
/// `range = "a..b"` is inclusive of both ends — `1..3` is 1, 2, 3 — because the
/// formulas and their smoke tests read it that way ("Move 1/2/3"), and a
/// half-open range here would silently drop the last iteration. `count = n` is
/// 1..=n.
fn loop_values(loop_spec: &crate::types::LoopSpec) -> Result<Vec<i64>> {
    if let Some(count) = loop_spec.count {
        if count < 1 {
            return Err(Error::Invalid(format!("loop count {count} is not positive")));
        }
        return Ok((1..=count).collect());
    }
    let range = loop_spec.range.as_deref().unwrap_or_default();
    let (lo, hi) = range
        .split_once("..")
        .ok_or_else(|| Error::Invalid(format!("loop range {range:?} is not `lo..hi`")))?;
    let lo: i64 = lo
        .trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("loop range start {lo:?} is not a number")))?;
    let hi: i64 = hi
        .trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("loop range end {hi:?} is not a number")))?;
    if hi < lo {
        return Err(Error::Invalid(format!("loop range {range:?} runs backwards")));
    }
    Ok((lo..=hi).collect())
}
