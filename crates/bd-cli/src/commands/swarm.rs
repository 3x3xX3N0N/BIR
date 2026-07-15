//! `bd swarm …` and `bd rules …` — multi-agent coordination.
//!
//! A swarm is a workflow where several agents work in parallel and a coordinator
//! reconciles them — a formula of type `convoy`, once that cooks. Today the
//! commands validate a swarm spec, list swarms, and report status; the
//! coordination itself rides on the same issue graph everything else does.
//!
//! `rules audit`/`compact` live here too because they are the same shape of thing
//! — reading the workspace's own configuration/convention state and reporting or
//! tidying it — and neither is large enough to want its own file yet.

use anyhow::Result;

use crate::cli::{RulesCmd, SwarmCmd};
use crate::commands::stub;
use crate::context::Ctx;

pub async fn swarm(ctx: &Ctx, cmd: SwarmCmd) -> Result<()> {
    let name = match cmd {
        SwarmCmd::Validate { .. } => "swarm validate",
        SwarmCmd::Status => "swarm status",
        SwarmCmd::Create { .. } => "swarm create",
        SwarmCmd::List => "swarm list",
    };
    stub(name, ctx)
}

pub async fn rules(ctx: &Ctx, cmd: RulesCmd) -> Result<()> {
    let name = match cmd {
        RulesCmd::Audit => "rules audit",
        RulesCmd::Compact => "rules compact",
    };
    stub(name, ctx)
}
