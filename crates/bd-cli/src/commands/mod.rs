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
pub mod formula;
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

/// One arm per command, and nothing else: unpack the args clap parsed and hand
/// them to the family module that owns the command.
///
/// Every handler is `async` even where its body could not possibly await —
/// including the stubs. That is the point. A stub that graduates into a real
/// command *will* need the store, the store is async, and if the stub had been
/// sync the promotion would have had to come back here to add a `.await`. Then
/// this file is on the critical path of every agent at once, which is the
/// collision this arrangement exists to prevent.
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
        C::Edit { id } => issues::edit(ctx, &id).await,
        C::Restore { id } => issues::restore(ctx, &id).await,
        C::Rename { id, title } => issues::rename(ctx, &id, &title).await,
        C::Assign { id, assignee } => issues::assign(ctx, &id, &assignee).await,
        C::Unclaim { id } => issues::unclaim(ctx, &id).await,
        C::Priority { id, priority } => issues::priority(ctx, &id, priority).await,
        C::Tag { id, tags } => issues::tag(ctx, &id, &tags).await,
        C::Label { cmd } => issues::label(ctx, cmd).await,
        C::Comment { id, text } => issues::comment(ctx, &id, &text).await,
        C::Comments { cmd } => issues::comments(ctx, cmd).await,
        C::Note { id, text } => issues::note(ctx, &id, &text).await,
        C::Defer { id, until } => issues::defer(ctx, &id, until).await,
        C::Undefer { id } => issues::undefer(ctx, &id).await,
        C::Duplicate { id, of } => issues::duplicate(ctx, &id, &of).await,
        C::Supersede { id, with } => issues::supersede(ctx, &id, &with).await,
        C::Link { from, to, link_type } => issues::link(ctx, &from, &to, link_type).await,
        C::Heartbeat { id } => issues::heartbeat(ctx, &id).await,
        C::State { id } => issues::state(ctx, &id).await,
        C::SetState { id, state } => issues::set_state(ctx, &id, &state).await,
        C::Statuses => issues::statuses(ctx).await,
        C::Types => issues::types(ctx).await,
        C::Promote { id } => issues::promote(ctx, &id).await,
        C::Batch { file } => issues::batch(ctx, file).await,

        // ----- Views -----
        C::List(a) => views::list(ctx, a).await,
        C::Ready(a) => views::ready(ctx, a).await,
        C::Blocked(a) => views::blocked(ctx, a).await,
        C::Search(a) => views::search(ctx, a).await,
        C::Query(a) => views::query(ctx, a).await,
        C::Count(a) => views::count(ctx, a).await,
        C::Status => views::status(ctx).await,
        C::History { id } => views::history(ctx, &id).await,
        C::Children { id } => views::children(ctx, &id).await,
        C::Epic { cmd } => views::epic(ctx, cmd).await,
        C::Info => views::info(ctx).await,
        C::Stale { older_than } => views::stale(ctx, older_than).await,
        C::Orphans => views::orphans(ctx).await,
        C::Duplicates => views::duplicates(ctx).await,
        C::FindDuplicates { id } => views::find_duplicates(ctx, &id).await,
        C::Lint => views::lint(ctx).await,
        C::Diff { from, to } => views::diff(ctx, &from, &to).await,
        C::Sql { query } => views::sql(ctx, &query).await,
        C::Kv { cmd } => views::kv(ctx, cmd).await,
        C::Audit { cmd } => views::audit(ctx, cmd).await,
        C::Where => views::where_(ctx),
        C::Context => views::context(ctx).await,
        C::Ping => views::ping(ctx).await,

        // ----- Deps -----
        C::Dep { cmd } => deps::dep(ctx, cmd).await,
        C::Graph { cmd } => deps::graph(ctx, cmd).await,
        C::Flatten { id } => deps::flatten(ctx, &id).await,
        C::RecomputeBlocked => deps::recompute_blocked(ctx).await,

        // ----- Sync -----
        C::Dolt { cmd } => sync::dolt(ctx, cmd).await,
        C::Vc { cmd } => sync::vc(ctx, cmd).await,
        C::Branch { name } => sync::branch(ctx, name).await,
        C::Federation { cmd } => sync::federation(ctx, cmd).await,
        C::Repo { cmd } => sync::repo(ctx, cmd).await,
        C::Export(a) => sync::export(ctx, a).await,
        C::Import(a) => sync::import(ctx, a).await,
        C::Ado { cmd } => sync::tracker(ctx, "ado", cmd).await,
        C::Jira { cmd } => sync::tracker(ctx, "jira", cmd).await,
        C::Linear { cmd } => sync::tracker(ctx, "linear", cmd).await,
        C::Github { cmd } => sync::tracker(ctx, "github", cmd).await,
        C::Gitlab { cmd } => sync::tracker(ctx, "gitlab", cmd).await,
        C::Notion { cmd } => sync::tracker(ctx, "notion", cmd).await,
        C::Mail { id } => sync::mail(ctx, id).await,
        C::Ship {
            capability,
            force,
            dry_run,
        } => sync::ship(ctx, &capability, force, dry_run).await,

        // ----- Setup -----
        C::Init(a) => setup::init(ctx, a).await,
        C::Bootstrap => setup::bootstrap(ctx).await,
        C::Setup { recipe } => setup::setup_cmd(ctx, &recipe).await,
        C::Onboard => setup::onboard(ctx).await,
        C::Quickstart => setup::quickstart(ctx).await,
        C::Prime => setup::prime(ctx).await,
        C::Hooks { cmd } => setup::hooks(ctx, cmd).await,
        C::Config { cmd } => setup::config(ctx, cmd).await,
        C::Upgrade { cmd } => setup::upgrade(ctx, cmd).await,
        C::Version => setup::version(ctx),
        C::Metrics { cmd } => setup::metrics(ctx, cmd).await,
        C::Completion { shell } => setup::completion(shell),

        // ----- Maintenance -----
        C::Doctor { fix } => crate::doctor::run(ctx, crate::doctor::Opts { fix }).await,
        C::Preflight => maintenance::preflight(ctx).await,
        C::Gc { dry_run } => maintenance::gc(ctx, dry_run).await,
        C::Purge {
            older_than,
            dry_run,
            yes,
        } => maintenance::purge(ctx, older_than, dry_run, yes).await,
        C::Prune { dry_run } => maintenance::prune(ctx, dry_run).await,
        C::Compact => maintenance::compact(ctx).await,
        C::Backup { cmd } => maintenance::backup(ctx, cmd).await,
        C::Admin { cmd } => maintenance::admin(ctx, cmd).await,
        C::Migrate => maintenance::migrate(ctx).await,
        C::RenamePrefix { from, to } => maintenance::rename_prefix(ctx, &from, &to).await,
        C::Reclaim => maintenance::reclaim(ctx).await,
        C::Worktree { cmd } => maintenance::worktree(ctx, cmd).await,
        C::MergeSlot { cmd } => maintenance::merge_slot(ctx, cmd).await,

        // ----- Advanced -----
        C::Mol { cmd } => advanced::mol(ctx, cmd).await,
        C::Formula { cmd } => advanced::formula(ctx, cmd).await,
        C::Cook {
            formula,
            vars,
            dry_run,
        } => advanced::cook(ctx, formula, vars, dry_run).await,
        C::Swarm { cmd } => advanced::swarm(ctx, cmd).await,
        C::Gate { cmd } => advanced::gate(ctx, cmd).await,
        C::Rules { cmd } => advanced::rules(ctx, cmd).await,
        C::Todo { cmd } => advanced::todo(ctx, cmd).await,
        C::Human { cmd } => advanced::human(ctx, cmd).await,
        C::Remember { text } => advanced::remember(ctx, &text).await,
        C::Memories => advanced::memories(ctx).await,
        C::Forget { id } => advanced::forget(ctx, &id).await,
        C::Recall { text } => advanced::recall(ctx, &text).await,
    }
}
