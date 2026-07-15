//! `bd gate …` — gates: async waits, as issues.
//!
//! A gate is an ordinary issue of type [`IssueType::Gate`] that something else
//! blocks on. It exists because a step can depend on a condition that is not
//! another step's completion — a timer elapsing, a human approving, CI going
//! green. `bd cook` already emits gate issues from a formula's `[steps.gate]`;
//! this family is the manual side: create one, inspect it, check it, and resolve
//! it (which closes it, unblocking whatever waited).
//!
//! Built entirely on the existing seam. A gate is `Gate`-typed and excluded from
//! ready-work by the store already, so nothing here needs a new capability —
//! `create_issue`, `close_issue`, `list_issues` filtered by type, and the
//! dependency edges do all of it.
//!
//! # Resolving is just closing
//!
//! `resolve` is `close_issue`, and that is the whole trick. The sqlite backend's
//! `close_issue` calls `blocked::recompute_affected` for exactly the set of
//! issues that had a blocking edge *into* the gate, re-fixpointing their
//! `is_blocked` cache inside the same transaction. So closing one gate makes its
//! dependents claimable on the very next `bd ready` — no extra
//! `recompute_blocked` call is needed, and adding one would only re-scan the
//! whole table for a change the write path already propagated.

use anyhow::{Result, anyhow, bail};
use bd_core::{Issue, IssueFilter, IssueType, Status};
use bd_storage::Storage;
use serde_json::json;

use crate::cli::GateCmd;
use crate::context::Ctx;
use crate::output::issue_json;

pub async fn gate(ctx: &Ctx, cmd: GateCmd) -> Result<()> {
    match cmd {
        GateCmd::List => list(ctx).await,
        GateCmd::Create { name } => create(ctx, &name).await,
        GateCmd::Show { id } => show(ctx, &id).await,
        GateCmd::Resolve { id } => resolve(ctx, &id).await,
        GateCmd::Check { id } => check(ctx, &id).await,
    }
}

/// Create a manual gate.
///
/// The wait policy — `timer`/`review`/`ci`, an await id, a timeout — would come
/// from flags this command's current args cannot express (see the note at the
/// end of this file). Absent those, the gate is a *manual* wait: a human (or a
/// script that knows the condition is met) resolves it with `bd gate resolve`.
/// The policy is stored in `metadata` under the same `{"gate":{…}}` shape
/// `bd cook` writes, so a hand-made gate and a cooked one are indistinguishable
/// to downstream tooling.
async fn create(ctx: &Ctx, name: &str) -> Result<()> {
    ctx.ensure_writable("create a gate")?;
    let store = ctx.store().await?;

    let prefix = ctx.prefix().await;
    let id = store.next_id(&prefix, name, "").await?;

    let mut issue = Issue::new(&id, name);
    issue.issue_type = IssueType::Gate;
    issue.created_by = ctx.identity.actor.clone();
    issue.metadata = Some(gate_metadata("manual", None, None));
    issue.validate()?;

    let created = store.create_issue(&issue).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&issue_json(&created, &[], &[], &[]))?;
    } else {
        ctx.out.line(format!(
            "Created gate {} (manual wait; resolve it with `bd gate resolve {}`)",
            created.id, created.id
        ));
    }
    Ok(())
}

/// Open gates: the waits still gating work. A resolved gate is closed, so the
/// default view excludes it — `bd gate list` answers "what am I still waiting
/// on", which is the only question with a standing answer.
async fn list(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let f = IssueFilter {
        issue_type: Some(IssueType::Gate),
        // There is no "not closed" predicate on the filter, only status sets, and
        // a gate can legitimately sit in any non-closed status while it waits, so
        // exclude the one terminal state rather than enumerating the rest.
        exclude_statuses: vec![Status::Closed],
        ..Default::default()
    };
    let gates = store.list_issues(&f).await?;
    ctx.out.issues(&gates)
}

/// The gate and what blocks on it. Read-only.
async fn show(ctx: &Ctx, id: &str) -> Result<()> {
    let store = ctx.store().await?;
    let gate = require_gate(store, id).await?;

    // `dependents_of` is the point of `show`: it is *what waits on this gate*.
    // `dependencies_of` is shown too for symmetry — a gate can itself wait on
    // something — but is usually empty.
    let depends_on = store.dependencies_of(id).await?;
    let dependents = store.dependents_of(id).await?;
    let comments = store.list_comments(id).await?;

    if ctx.out.is_json() {
        ctx.out
            .json_value(&issue_json(&gate, &depends_on, &dependents, &comments))?;
    } else {
        ctx.out
            .issue_detail(&gate, &depends_on, &dependents, &comments)?;
    }
    Ok(())
}

/// Report whether the gate is satisfied, without changing anything. Read-only,
/// so it works under `--readonly`. A gate is satisfied exactly when it is
/// closed (resolved); otherwise it is still waiting.
async fn check(ctx: &Ctx, id: &str) -> Result<()> {
    let store = ctx.store().await?;
    let gate = require_gate(store, id).await?;
    let satisfied = gate.status.is_closed();

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "id": gate.id,
            "status": gate.status.as_str(),
            "satisfied": satisfied,
            "state": if satisfied { "satisfied" } else { "waiting" },
        }))?;
    } else if satisfied {
        let how = if gate.close_reason.is_empty() {
            String::new()
        } else {
            format!(" ({})", gate.close_reason)
        };
        ctx.out.line(format!("Gate {id} is satisfied{how}."));
    } else {
        ctx.out.line(format!("Gate {id} is still waiting."));
    }
    Ok(())
}

/// The gate's condition is met: close it, which unblocks whatever depended on
/// it. See the module note — `close_issue` re-fixpoints the blocked cache for
/// the dependents on its own, so `bd ready` is correct on the next call with no
/// extra recompute here.
async fn resolve(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.ensure_writable("resolve a gate")?;
    let store = ctx.store().await?;
    let gate = require_gate(store, id).await?;

    // Idempotent: an already-resolved gate is not an error. Re-closing it would
    // bump `closed_at`/`updated_at` for a change that did not happen, so report
    // and stop.
    if gate.status.is_closed() {
        if ctx.out.is_json() {
            ctx.out.json_value(&json!({
                "id": gate.id,
                "satisfied": true,
                "already_resolved": true,
            }))?;
        } else {
            ctx.out.line(format!("Gate {id} is already resolved."));
        }
        return Ok(());
    }

    // Read the dependents *before* closing so we can report what was freed. Only
    // the edges that actually gate readiness count as "unblocked"; an
    // association edge into a gate (unusual, but legal) never held anything back.
    let dependents = store.dependents_of(id).await?;
    let freed: Vec<String> = dependents
        .iter()
        .filter(|d| d.dep_type.affects_ready_work())
        .map(|d| d.issue_id.clone())
        .collect();

    // "gate resolved" is deliberately not a failure phrase (see
    // `bd_core::is_failure_close`): a `blocks` dependent becomes ready, and a
    // `conditional-blocks` dependent — which runs only on failure — correctly
    // stays put.
    let closed = store.close_issue(id, "gate resolved").await?;

    if ctx.out.is_json() {
        let mut v = issue_json(&closed, &[], &[], &[]);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("unblocked".into(), json!(freed));
        }
        ctx.out.json_value(&v)?;
    } else {
        ctx.out.line(format!("Resolved gate {id}."));
        if !freed.is_empty() {
            ctx.out
                .line(format!("Unblocked {}: {}", freed.len(), freed.join(", ")));
        }
    }
    Ok(())
}

/// The `{"gate":{…}}` metadata blob, matching the shape `bd cook` writes from a
/// formula's `[steps.gate]` (see `crates/bd-cli/src/commands/formula.rs`). The
/// `await_id`/`timeout` halves are carried as JSON null when absent so the shape
/// is stable whether or not a policy was given.
fn gate_metadata(
    await_type: &str,
    await_id: Option<&str>,
    timeout: Option<&str>,
) -> serde_json::Value {
    json!({
        "gate": {
            "await_type": await_type,
            "await_id": await_id,
            "timeout": timeout,
        }
    })
}

/// Fetch an issue and insist it is a gate.
///
/// `gate show`/`check`/`resolve` are gate commands, and pointing one at an
/// ordinary issue is a mistake worth naming rather than silently serving — a
/// stray `gate resolve bd-12` should not become a back-door `bd close`. The
/// existence check is also load-bearing on its own: without it, `resolve` on a
/// typo'd id would sail into `close_issue`'s own not-found rather than a message
/// that says "gate".
async fn require_gate(store: &dyn Storage, id: &str) -> Result<Issue> {
    let issue = store
        .get_issue(id)
        .await?
        .ok_or_else(|| anyhow!("gate not found: {id}"))?;
    if issue.issue_type != IssueType::Gate {
        bail!("{id} is a {}, not a gate", issue.issue_type);
    }
    Ok(issue)
}

// ---------------------------------------------------------------------------
// Requested cli.rs flags (not addable from this file; for the integrator)
// ---------------------------------------------------------------------------
//
// `GateCmd::Create { name }` can express only a bare manual gate. To make a
// `gate create` carry the same policy a formula gate does, it wants:
//
//   Create {
//       name: String,
//       /// `manual` (default), `timer`, `review`, `ci`, … — the wait's kind.
//       #[arg(long = "type", value_name = "KIND")]
//       await_type: Option<String>,
//       /// A stable id for the wait, so a resumed run can find it again.
//       #[arg(long = "await-id", value_name = "ID")]
//       await_id: Option<String>,
//       /// How long before the wait gives up: `30m`, `2h`.
//       #[arg(long, value_name = "DURATION")]
//       timeout: Option<String>,
//       /// Issue(s) this gate should block: adds a `blocks` edge gate -> id.
//       #[arg(long = "blocks", value_name = "ID")]
//       blocks: Vec<String>,
//   }
//
// With those, `create` would pass `await_type`/`await_id`/`timeout` straight
// into `gate_metadata`, and add a `blocks` edge from the gate to each `--blocks`
// target so the gate is wired to the work in one step instead of a follow-up
// `bd dep add`. `gate_metadata` already takes all three, so only the wiring here
// and the arg struct in cli.rs would change.
