//! Keeping a workspace healthy. All of it is registered; none of it is ported.
//!
//! These handlers exist to name the exact subcommand in the stub message —
//! `bd backup restore` is a different piece of work from `bd backup status`,
//! and PORT_STATUS.md tracks them separately.

use anyhow::Result;

use crate::cli::{AdminCmd, BackupCmd, MergeSlotCmd, WorktreeCmd};
use crate::commands::stub;
use crate::context::Ctx;

pub fn doctor(ctx: &Ctx) -> Result<()> {
    stub("doctor", ctx)
}

pub fn backup(ctx: &Ctx, cmd: BackupCmd) -> Result<()> {
    let name = match cmd {
        BackupCmd::Status => "backup status",
        BackupCmd::Init { .. } => "backup init",
        BackupCmd::Sync => "backup sync",
        BackupCmd::Remove => "backup remove",
        BackupCmd::Restore { .. } => "backup restore",
    };
    stub(name, ctx)
}

pub fn admin(ctx: &Ctx, cmd: AdminCmd) -> Result<()> {
    let name = match cmd {
        AdminCmd::Cleanup => "admin cleanup",
        AdminCmd::Compact => "admin compact",
        AdminCmd::Reset => "admin reset",
    };
    stub(name, ctx)
}

pub fn worktree(ctx: &Ctx, cmd: WorktreeCmd) -> Result<()> {
    let name = match cmd {
        WorktreeCmd::Create { .. } => "worktree create",
        WorktreeCmd::List => "worktree list",
        WorktreeCmd::Remove { .. } => "worktree remove",
        WorktreeCmd::Info => "worktree info",
    };
    stub(name, ctx)
}

pub fn merge_slot(ctx: &Ctx, cmd: MergeSlotCmd) -> Result<()> {
    let name = match cmd {
        MergeSlotCmd::Create { .. } => "merge-slot create",
        MergeSlotCmd::Check => "merge-slot check",
        MergeSlotCmd::Acquire { .. } => "merge-slot acquire",
        MergeSlotCmd::Release { .. } => "merge-slot release",
    };
    stub(name, ctx)
}
