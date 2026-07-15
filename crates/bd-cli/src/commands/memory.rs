//! `bd remember` / `recall` / `forget` / `memories` — durable agent notes.
//!
//! Memory is a small key/value-ish store of things an agent wants to carry
//! across sessions: a decision, a gotcha, a pointer. `remember` writes one,
//! `recall <query>` searches, `memories` lists, `forget <id>` deletes.
//!
//! `todo` and `human` are the other two small families that build on the graph:
//! `todo` is a lightweight personal checklist, and `human` is the queue of items
//! escalated for a person to answer. All three are grouped here because none is
//! large enough for its own file and they share the shape — a typed or labelled
//! bead, created and queried through the existing seam.

use anyhow::Result;

use crate::cli::{HumanCmd, TodoCmd};
use crate::commands::stub;
use crate::context::Ctx;

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
