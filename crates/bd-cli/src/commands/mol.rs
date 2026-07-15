//! `bd mol …` — molecules: the lifecycle of a formula instance.
//!
//! A molecule is not a new kind of storage. It is an ordinary issue of type
//! [`IssueType::Molecule`](bd_core::IssueType::Molecule) that groups a unit of
//! work, plus the beads it contains hung off it by `parent-child` edges. So this
//! whole family is built on the existing seam — `create_issue`, `list_issues`
//! filtered by type, `add_dependency` — and on the formula compiler for the two
//! commands that instantiate one.
//!
//! The two that cook: `seed <template>` instantiates a molecule from a formula
//! (compile it, create the container, pour its steps as children), and `pour`
//! emits an already-seeded molecule's remaining work. Both go through
//! [`bd_formula`] exactly the way `bd cook` does.

use anyhow::Result;

use crate::cli::MolCmd;
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
