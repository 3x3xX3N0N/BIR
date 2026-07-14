//! Molecules, formulas, swarms, gates, memory. Registered, not ported.
//!
//! These are the parts of beads that build *on top of* the graph, and every one
//! of them assumes the core below it works. Porting them before `bd ready` is
//! solid would be building the roof first.

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::{FormulaCmd, GateCmd, HumanCmd, MolCmd, RulesCmd, SwarmCmd, TodoCmd};
use crate::commands::stub;
use crate::context::Ctx;

pub async fn mol(ctx: &Ctx, cmd: MolCmd) -> Result<()> {
    let name = match cmd {
        MolCmd::Bond { .. } => "mol bond",
        MolCmd::Burn { .. } => "mol burn",
        MolCmd::Current => "mol current",
        MolCmd::Distill { .. } => "mol distill",
        MolCmd::Ready => "mol ready",
        MolCmd::Seed { .. } => "mol seed",
        MolCmd::Show { .. } => "mol show",
        MolCmd::Squash { .. } => "mol squash",
        MolCmd::Stale => "mol stale",
        MolCmd::Pour { .. } => "mol pour",
        MolCmd::Wisp { .. } => "mol wisp",
    };
    stub(name, ctx)
}

pub async fn formula(ctx: &Ctx, cmd: FormulaCmd) -> Result<()> {
    let name = match cmd {
        FormulaCmd::List => "formula list",
        FormulaCmd::Show { .. } => "formula show",
        FormulaCmd::Convert { .. } => "formula convert",
        FormulaCmd::Schema => "formula schema",
    };
    stub(name, ctx)
}

/// Running a formula is the formula DSL's whole reason to exist, so this is
/// unbuilt work (exit 64) rather than anything the backend could refuse.
pub async fn cook(ctx: &Ctx, _formula: PathBuf) -> Result<()> {
    stub("cook", ctx)
}

pub async fn swarm(ctx: &Ctx, cmd: SwarmCmd) -> Result<()> {
    let name = match cmd {
        SwarmCmd::Validate { .. } => "swarm validate",
        SwarmCmd::Status => "swarm status",
        SwarmCmd::Create { .. } => "swarm create",
        SwarmCmd::List => "swarm list",
    };
    stub(name, ctx)
}

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

pub async fn rules(ctx: &Ctx, cmd: RulesCmd) -> Result<()> {
    let name = match cmd {
        RulesCmd::Audit => "rules audit",
        RulesCmd::Compact => "rules compact",
    };
    stub(name, ctx)
}

pub async fn todo(ctx: &Ctx, cmd: TodoCmd) -> Result<()> {
    let name = match cmd {
        TodoCmd::Add { .. } => "todo add",
        TodoCmd::List => "todo list",
        TodoCmd::Done { .. } => "todo done",
    };
    stub(name, ctx)
}

pub async fn human(ctx: &Ctx, cmd: HumanCmd) -> Result<()> {
    let name = match cmd {
        HumanCmd::List => "human list",
        HumanCmd::Respond { .. } => "human respond",
        HumanCmd::Dismiss { .. } => "human dismiss",
        HumanCmd::Stats => "human stats",
    };
    stub(name, ctx)
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

pub async fn remember(ctx: &Ctx, _text: &[String]) -> Result<()> {
    stub("remember", ctx)
}

pub async fn memories(ctx: &Ctx) -> Result<()> {
    stub("memories", ctx)
}

pub async fn forget(ctx: &Ctx, _id: &str) -> Result<()> {
    stub("forget", ctx)
}

pub async fn recall(ctx: &Ctx, _text: &[String]) -> Result<()> {
    stub("recall", ctx)
}
