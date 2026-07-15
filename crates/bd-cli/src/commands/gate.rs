//! `bd gate …` — gates: async waits, as issues.
//!
//! A gate is an ordinary issue of type [`IssueType::Gate`](bd_core::IssueType::Gate)
//! that something else blocks on. It exists because a step can depend on a
//! condition that is not another step's completion — a timer elapsing, a human
//! approving, CI going green. `bd cook` already emits gate issues from a
//! formula's `[steps.gate]`; this family is the manual side: create one, inspect
//! it, and resolve it (which closes it, unblocking whatever waited).
//!
//! Built entirely on the existing seam. A gate is `Gate`-typed and excluded from
//! ready-work by the store already, so nothing here needs a new capability —
//! `create_issue`, `close_issue`, `list_issues` filtered by type, and the
//! dependency edges do all of it.

use anyhow::Result;

use crate::cli::GateCmd;
use crate::commands::stub;
use crate::context::Ctx;

pub async fn gate(ctx: &Ctx, cmd: GateCmd) -> Result<()> {
    let name = match cmd {
        GateCmd::List => "gate list",
        GateCmd::Create { .. } => "gate create",
        GateCmd::Show { .. } => "gate show",
        GateCmd::Resolve { .. } => "gate resolve",
        GateCmd::Check { .. } => "gate check",
    };
    stub(name, ctx)
}
