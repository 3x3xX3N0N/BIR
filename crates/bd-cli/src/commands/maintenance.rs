//! Keeping a workspace healthy. All of it is registered; none of it is ported.
//!
//! These handlers exist to name the exact subcommand in the stub message —
//! `bd backup restore` is a different piece of work from `bd backup status`,
//! and PORT_STATUS.md tracks them separately.

use anyhow::Result;

use crate::cli::{AdminCmd, BackupCmd, MergeSlotCmd, WorktreeCmd};
use crate::commands::stub;
use crate::context::Ctx;

pub async fn doctor(ctx: &Ctx) -> Result<()> {
    stub("doctor", ctx)
}

pub async fn preflight(ctx: &Ctx) -> Result<()> {
    stub("preflight", ctx)
}

pub async fn gc(ctx: &Ctx) -> Result<()> {
    stub("gc", ctx)
}

pub async fn purge(ctx: &Ctx, _older_than: &str) -> Result<()> {
    stub("purge", ctx)
}

pub async fn prune(ctx: &Ctx) -> Result<()> {
    stub("prune", ctx)
}

pub async fn compact(ctx: &Ctx) -> Result<()> {
    stub("compact", ctx)
}

pub async fn migrate(ctx: &Ctx) -> Result<()> {
    stub("migrate", ctx)
}

pub async fn rename_prefix(ctx: &Ctx, _from: &str, _to: &str) -> Result<()> {
    stub("rename-prefix", ctx)
}

pub async fn reclaim(ctx: &Ctx) -> Result<()> {
    stub("reclaim", ctx)
}

pub async fn backup(ctx: &Ctx, cmd: BackupCmd) -> Result<()> {
    let name = match cmd {
        BackupCmd::Status => "backup status",
        BackupCmd::Init { .. } => "backup init",
        BackupCmd::Sync => "backup sync",
        BackupCmd::Remove => "backup remove",
        BackupCmd::Restore { .. } => "backup restore",
    };
    stub(name, ctx)
}

pub async fn admin(ctx: &Ctx, cmd: AdminCmd) -> Result<()> {
    let name = match cmd {
        AdminCmd::Cleanup => "admin cleanup",
        AdminCmd::Compact => "admin compact",
        AdminCmd::Reset => "admin reset",
    };
    stub(name, ctx)
}

pub async fn worktree(ctx: &Ctx, cmd: WorktreeCmd) -> Result<()> {
    let name = match cmd {
        WorktreeCmd::Create { .. } => "worktree create",
        WorktreeCmd::List => "worktree list",
        WorktreeCmd::Remove { .. } => "worktree remove",
        WorktreeCmd::Info => "worktree info",
    };
    stub(name, ctx)
}

pub async fn merge_slot(ctx: &Ctx, cmd: MergeSlotCmd) -> Result<()> {
    let name = match cmd {
        MergeSlotCmd::Create { .. } => "merge-slot create",
        MergeSlotCmd::Check => "merge-slot check",
        MergeSlotCmd::Acquire { .. } => "merge-slot acquire",
        MergeSlotCmd::Release { .. } => "merge-slot release",
    };
    stub(name, ctx)
}
