//! Command handlers, and the two ways a command can decline to do something.
//!
//! Those two ways are *not* the same thing and must never print the same
//! message:
//!
//! * [`stub`] — beads has this command; this port has not built it. Exit 64.
//! * [`require_cap`] — beads has this command and this port built it, but the
//!   workspace's backend cannot serve it. Exit 2. This is a legitimate answer,
//!   not a gap: SQLite genuinely has no commit graph.
//!
//! Blurring them would make the port's progress unreadable — every `bd branch`
//! on SQLite would look like unfinished work forever.

pub mod advanced;
pub mod deps;
pub mod issues;
pub mod maintenance;
pub mod setup;
pub mod sync;
pub mod views;

use anyhow::Result;
use serde_json::json;

use crate::cli::Commands;
use crate::context::Ctx;
use crate::exit::{self, SilentExit};

/// Registered in the command tree, not implemented yet.
pub fn stub(cmd: &str, ctx: &Ctx) -> Result<()> {
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "error": "not_implemented",
            "command": cmd,
            "see": "PORT_STATUS.md",
        }))?;
    } else {
        eprintln!("`bd {cmd}` is registered but not implemented in this Rust port yet.");
        eprintln!("(exit {}; see PORT_STATUS.md)", exit::NOT_IMPLEMENTED);
    }
    Err(SilentExit(exit::NOT_IMPLEMENTED).into())
}

/// The capability a command needs from the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    VersionControl,
    Remote,
    History,
}

impl Cap {
    fn name(self) -> &'static str {
        match self {
            Cap::VersionControl => "version_control",
            Cap::Remote => "remote",
            Cap::History => "history",
        }
    }

    fn needs(self) -> &'static str {
        // Every capability we model is, today, "a backend with a commit graph".
        "dolt"
    }
}

/// Refuse honestly when the backend cannot do this.
///
/// Prefers the open store — that is the contract ([`bd_storage::Storage`]'s
/// capability accessors). Falls back to the locator when no store is open,
/// which is not a second source of truth: rule 3 makes the locator *the*
/// authority on which engine owns a workspace, and
/// [`bd_storage::Backend::has_commit_graph`] exists precisely to answer this
/// without opening anything.
pub fn require_cap(ctx: &Ctx, cmd: &str, cap: Cap) -> Result<()> {
    let available = match ctx.try_store() {
        Some(store) => match cap {
            Cap::VersionControl => store.version_control().is_some(),
            Cap::Remote => store.remote().is_some(),
            Cap::History => store.history().is_some(),
        },
        None => ctx.backend().is_some_and(|b| b.has_commit_graph()),
    };
    if available {
        return Ok(());
    }

    let backend = ctx
        .backend()
        .map(|b| b.to_string())
        .unwrap_or_else(|| "none".to_string());
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "error": "unsupported_backend",
            "command": cmd,
            "backend": backend,
            "requires": cap.needs(),
            "capability": cap.name(),
        }))?;
    } else {
        eprintln!(
            "`bd {cmd}` needs the {} backend; this workspace is {backend}.",
            cap.needs()
        );
        eprintln!(
            "{backend} has no commit graph — that is a property of the backend, not a missing feature."
        );
    }
    Err(SilentExit(exit::CAPABILITY).into())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub async fn dispatch(cmd: Commands, ctx: &Ctx) -> Result<()> {
    use Commands as C;
    match cmd {
        // ----- Issues -----
        C::Create(a) => issues::create(ctx, a).await,
        C::Q(a) => issues::quick(ctx, a).await,
        C::Show { ids } => issues::show(ctx, &ids).await,
        C::Update(a) => issues::update(ctx, a).await,
        C::Close(a) => issues::close(ctx, a).await,
        C::Reopen { ids } => issues::reopen(ctx, &ids).await,
        C::Delete { ids } => issues::delete(ctx, &ids).await,
        C::Assign { id, assignee } => issues::assign(ctx, &id, &assignee).await,
        C::Unclaim { id } => issues::unclaim(ctx, &id).await,
        C::Priority { id, priority } => issues::priority(ctx, &id, priority).await,
        C::Comment { id, text } => issues::comment(ctx, &id, &text.join(" ")).await,
        C::Comments { cmd } => issues::comments(ctx, cmd).await,
        C::Label { cmd } => issues::label(ctx, cmd).await,
        C::Edit { .. } => stub("edit", ctx),
        C::Restore { .. } => stub("restore", ctx),
        C::Rename { .. } => stub("rename", ctx),
        C::Tag { .. } => stub("tag", ctx),
        C::Note { .. } => stub("note", ctx),
        C::Defer { .. } => stub("defer", ctx),
        C::Undefer { .. } => stub("undefer", ctx),
        C::Duplicate { .. } => stub("duplicate", ctx),
        C::Supersede { .. } => stub("supersede", ctx),
        C::Link { .. } => stub("link", ctx),
        C::Heartbeat { .. } => stub("heartbeat", ctx),
        C::State { .. } => stub("state", ctx),
        C::SetState { .. } => stub("set-state", ctx),
        C::Statuses => stub("statuses", ctx),
        C::Types => stub("types", ctx),
        C::Promote { .. } => stub("promote", ctx),
        C::Batch { .. } => stub("batch", ctx),

        // ----- Views -----
        C::List(a) => views::list(ctx, a).await,
        C::Ready(a) => views::ready(ctx, a).await,
        C::Blocked(a) => views::blocked(ctx, a).await,
        C::Search(a) => views::search(ctx, a).await,
        C::Query(a) => views::query(ctx, a).await,
        C::Count(a) => views::count(ctx, a).await,
        C::Status => views::status(ctx).await,
        C::History { id } => views::history(ctx, &id).await,
        C::Where => views::where_(ctx),
        C::Children { .. } => stub("children", ctx),
        C::Epic { cmd } => match cmd {
            crate::cli::EpicCmd::Status => stub("epic status", ctx),
            crate::cli::EpicCmd::CloseEligible => stub("epic close-eligible", ctx),
        },
        C::Info => stub("info", ctx),
        C::Stale { .. } => stub("stale", ctx),
        C::Orphans => stub("orphans", ctx),
        C::Duplicates => stub("duplicates", ctx),
        C::FindDuplicates { .. } => stub("find-duplicates", ctx),
        C::Lint => stub("lint", ctx),
        C::Diff { from, to } => views::diff(ctx, &from, &to),
        C::Sql { .. } => stub("sql", ctx),
        C::Kv { cmd } => {
            use crate::cli::KvCmd as K;
            match cmd {
                K::Set { .. } => stub("kv set", ctx),
                K::Get { .. } => stub("kv get", ctx),
                K::Clear { .. } => stub("kv clear", ctx),
                K::List => stub("kv list", ctx),
            }
        }
        C::Audit { cmd } => {
            use crate::cli::AuditCmd as A;
            match cmd {
                A::Record { .. } => stub("audit record", ctx),
                A::Label { .. } => stub("audit label", ctx),
            }
        }
        C::Context => stub("context", ctx),
        C::Ping => stub("ping", ctx),

        // ----- Deps -----
        C::Dep { cmd } => deps::dep(ctx, cmd).await,
        C::Graph { cmd } => match cmd {
            Some(crate::cli::GraphCmd::Check) => stub("graph check", ctx),
            None => stub("graph", ctx),
        },
        C::Flatten { .. } => stub("flatten", ctx),
        C::RecomputeBlocked => deps::recompute_blocked(ctx).await,

        // ----- Sync -----
        C::Export(a) => sync::export(ctx, a).await,
        C::Import(a) => sync::import(ctx, a).await,
        C::Dolt { cmd } => sync::dolt(ctx, cmd),
        C::Vc { cmd } => sync::vc(ctx, cmd),
        C::Branch { .. } => sync::branch(ctx),
        C::Federation { cmd } => {
            use crate::cli::FederationCmd as F;
            match cmd {
                F::Sync => stub("federation sync", ctx),
                F::Status => stub("federation status", ctx),
                F::AddPeer { .. } => stub("federation add-peer", ctx),
                F::RemovePeer { .. } => stub("federation remove-peer", ctx),
                F::ListPeers => stub("federation list-peers", ctx),
            }
        }
        C::Repo { cmd } => {
            use crate::cli::RepoCmd as R;
            match cmd {
                R::Add { .. } => stub("repo add", ctx),
                R::Remove { .. } => stub("repo remove", ctx),
                R::List => stub("repo list", ctx),
                R::Sync => stub("repo sync", ctx),
            }
        }
        C::Ado { cmd } => sync::tracker(ctx, "ado", cmd),
        C::Jira { cmd } => sync::tracker(ctx, "jira", cmd),
        C::Linear { cmd } => sync::tracker(ctx, "linear", cmd),
        C::Github { cmd } => sync::tracker(ctx, "github", cmd),
        C::Gitlab { cmd } => sync::tracker(ctx, "gitlab", cmd),
        C::Notion { cmd } => sync::tracker(ctx, "notion", cmd),
        C::Mail { .. } => stub("mail", ctx),
        C::Ship => stub("ship", ctx),

        // ----- Setup -----
        C::Init(a) => setup::init(ctx, a).await,
        C::Version => setup::version(ctx),
        C::Completion { shell } => setup::completion(shell),
        C::Config { cmd } => setup::config(ctx, cmd).await,
        C::Bootstrap => stub("bootstrap", ctx),
        C::Setup => stub("setup", ctx),
        C::Onboard => stub("onboard", ctx),
        C::Quickstart => stub("quickstart", ctx),
        C::Prime => stub("prime", ctx),
        C::Hooks { cmd } => {
            use crate::cli::HooksCmd as H;
            match cmd {
                H::Install => stub("hooks install", ctx),
                H::Uninstall => stub("hooks uninstall", ctx),
                H::List => stub("hooks list", ctx),
                H::Run { .. } => stub("hooks run", ctx),
            }
        }
        C::Upgrade { cmd } => {
            use crate::cli::UpgradeCmd as U;
            match cmd {
                U::Status => stub("upgrade status", ctx),
                U::Review => stub("upgrade review", ctx),
                U::Ack => stub("upgrade ack", ctx),
            }
        }
        C::Metrics { cmd } => {
            use crate::cli::MetricsCmd as M;
            match cmd {
                M::On => stub("metrics on", ctx),
                M::Off => stub("metrics off", ctx),
                M::Example => stub("metrics example", ctx),
            }
        }

        // ----- Maintenance -----
        C::Doctor => maintenance::doctor(ctx),
        C::Preflight => stub("preflight", ctx),
        C::Gc => stub("gc", ctx),
        C::Purge { .. } => stub("purge", ctx),
        C::Prune => stub("prune", ctx),
        C::Compact => stub("compact", ctx),
        C::Backup { cmd } => maintenance::backup(ctx, cmd),
        C::Admin { cmd } => maintenance::admin(ctx, cmd),
        C::Migrate => stub("migrate", ctx),
        C::RenamePrefix { .. } => stub("rename-prefix", ctx),
        C::Reclaim => stub("reclaim", ctx),
        C::Worktree { cmd } => maintenance::worktree(ctx, cmd),
        C::MergeSlot { cmd } => maintenance::merge_slot(ctx, cmd),

        // ----- Advanced -----
        C::Mol { cmd } => advanced::mol(ctx, cmd),
        C::Formula { cmd } => advanced::formula(ctx, cmd),
        C::Cook { .. } => stub("cook", ctx),
        C::Swarm { cmd } => advanced::swarm(ctx, cmd),
        C::Gate { cmd } => advanced::gate(ctx, cmd),
        C::Rules { cmd } => advanced::rules(ctx, cmd),
        C::Todo { cmd } => advanced::todo(ctx, cmd),
        C::Human { cmd } => advanced::human(ctx, cmd),
        C::Remember { .. } => stub("remember", ctx),
        C::Memories => stub("memories", ctx),
        C::Forget { .. } => stub("forget", ctx),
        C::Recall { .. } => stub("recall", ctx),
    }
}
