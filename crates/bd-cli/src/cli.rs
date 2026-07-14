//! The whole command surface.
//!
//! Every command upstream has is registered here, even the ones this port has
//! not implemented. That is deliberate: a command that is *missing* is
//! indistinguishable from a typo, while a command that is *registered and
//! honest* tells you exactly where you stand (`exit 64`, see [`crate::exit`]).
//! `bd --help` is therefore the complete map, not a progress report.
//!
//! Note on grouping: clap 4's `help_heading` applies to arguments, not to
//! subcommands — there is no way to ask the builder for grouped command help.
//! So the root help is rendered from [`FAMILIES`] below, and `tests/cli.rs`
//! asserts that table and the real command tree never drift apart.

use std::path::PathBuf;

use bd_core::{DependencyType, IssueType, Priority, SortPolicy, Status};
use bd_storage::Backend;
use chrono::{DateTime, Duration, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::context::Need;
use crate::parse::{self, DepSpec};

#[derive(Parser, Debug)]
#[command(
    name = "bd",
    version,
    about = "beads — a dependency-aware issue tracker for coding agents",
    disable_help_subcommand = false,
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "Run `bd <command> --help` for a command's flags.\n\
                  Commands that exit 64 are registered but not ported yet — see PORT_STATUS.md."
)]
pub struct Cli {
    /// Run as if bd was started in this directory
    #[arg(short = 'C', long = "directory", global = true, value_name = "PATH", help_heading = "Global options")]
    pub directory: Option<PathBuf>,

    /// Workspace database (or the .beads directory holding it)
    #[arg(long, global = true, value_name = "PATH", help_heading = "Global options")]
    pub db: Option<PathBuf>,

    /// Who is acting; recorded on events and claims
    #[arg(long, global = true, env = "BEADS_ACTOR", value_name = "NAME", help_heading = "Global options")]
    pub actor: Option<String>,

    /// Emit JSON
    #[arg(long, global = true, help_heading = "Global options")]
    pub json: bool,

    /// Kept for compatibility with `--format=json`; prefer --json
    #[arg(long, global = true, hide = true, value_name = "FMT", help_heading = "Global options")]
    pub format: Option<String>,

    /// Refuse every write
    #[arg(long, global = true, help_heading = "Global options")]
    pub readonly: bool,

    /// Never emit ANSI color
    #[arg(long = "no-color", global = true, help_heading = "Global options")]
    pub no_color: bool,

    /// More detail on stderr (repeatable)
    #[arg(short, long, global = true, action = clap::ArgAction::Count, help_heading = "Global options")]
    pub verbose: u8,

    /// Only the essentials
    #[arg(short, long, global = true, conflicts_with = "verbose", help_heading = "Global options")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// `--format json` is the old spelling. Anything else it might name (yaml,
    /// table) does not exist here, so it is ignored rather than half-honored.
    pub fn json(&self) -> bool {
        self.json
            || self
                .format
                .as_deref()
                .is_some_and(|f| f.eq_ignore_ascii_case("json"))
    }
}

/// Build the root command with the grouped help this crate renders itself.
pub fn build() -> clap::Command {
    let cmd = Cli::command();
    let map = render_families(&cmd);
    // The template is a literal and `map` contains no braces, so a command's
    // help text cannot inject a placeholder into it.
    cmd
        // argv[0] is `bd.exe` on Windows; the help should still say `bd`.
        .bin_name("bd")
        .help_template(format!(
            "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{map}Options:\n{{options}}\n\n{{after-help}}"
        ))
}

// ---------------------------------------------------------------------------
// Families — the grouping shown by `bd --help`
// ---------------------------------------------------------------------------

pub const FAMILIES: &[(&str, &[&str])] = &[
    (
        "Issues",
        &[
            "create", "q", "show", "update", "close", "reopen", "delete", "edit", "restore",
            "rename", "assign", "unclaim", "priority", "tag", "label", "comment", "comments",
            "note", "defer", "undefer", "duplicate", "supersede", "link", "heartbeat", "state",
            "set-state", "statuses", "types", "promote", "batch",
        ],
    ),
    (
        "Views",
        &[
            "list", "ready", "blocked", "search", "query", "count", "status", "history",
            "children", "epic", "info", "stale", "orphans", "duplicates", "find-duplicates",
            "lint", "diff", "sql", "kv", "audit", "where", "context", "ping",
        ],
    ),
    ("Deps", &["dep", "graph", "flatten", "recompute-blocked"]),
    (
        "Sync",
        &[
            "dolt", "vc", "branch", "federation", "repo", "export", "import", "ado", "jira",
            "linear", "github", "gitlab", "notion", "mail", "ship",
        ],
    ),
    (
        "Setup",
        &[
            "init", "bootstrap", "setup", "onboard", "quickstart", "prime", "hooks", "config",
            "upgrade", "version", "metrics", "completion",
        ],
    ),
    (
        "Maintenance",
        &[
            "doctor", "preflight", "gc", "purge", "prune", "compact", "backup", "admin", "migrate",
            "rename-prefix", "reclaim", "worktree", "merge-slot",
        ],
    ),
    (
        "Advanced",
        &[
            "mol", "formula", "cook", "swarm", "gate", "rules", "todo", "human", "remember",
            "memories", "forget", "recall",
        ],
    ),
];

fn render_families(cmd: &clap::Command) -> String {
    let width = FAMILIES
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|n| n.len())
        .max()
        .unwrap_or(12);

    let mut out = String::new();
    for (family, names) in FAMILIES {
        out.push_str(family);
        out.push_str(":\n");
        for name in *names {
            let Some(sub) = cmd.get_subcommands().find(|s| s.get_name() == *name) else {
                continue; // the test catches this; help should not panic.
            };
            let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
            let aliases: Vec<_> = sub.get_visible_aliases().collect();
            let alias = if aliases.is_empty() {
                String::new()
            } else {
                format!(" [{}]", aliases.join(", "))
            };
            out.push_str(&format!("  {name:width$}  {about}{alias}\n"));
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum Commands {
    // ===================== Issues =====================
    /// Create an issue
    #[command(visible_alias = "new")]
    Create(CreateArgs),
    /// Quick capture: create an issue and print only its id
    Q(QuickArgs),
    /// Show an issue in full
    #[command(visible_alias = "view")]
    Show {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Change an issue's fields, or claim it
    Update(UpdateArgs),
    /// Close an issue, recording why
    #[command(visible_alias = "done")]
    Close(CloseArgs),
    /// Reopen a closed issue
    Reopen {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Delete an issue permanently
    Delete {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Edit an issue in $EDITOR
    Edit { id: String },
    /// Restore a deleted issue
    Restore { id: String },
    /// Retitle an issue
    Rename { id: String, title: String },
    /// Assign an issue to someone
    Assign { id: String, assignee: String },
    /// Release a claim on an issue
    Unclaim { id: String },
    /// Set an issue's priority
    Priority { id: String, priority: Priority },
    /// Add or remove tags
    /// Prefix a tag with `-` to remove it: `bd tag x-1 urgent -stale`
    Tag {
        id: String,
        #[arg(required = true, allow_hyphen_values = true)]
        tags: Vec<String>,
    },
    /// Labels on issues
    Label {
        #[command(subcommand)]
        cmd: LabelCmd,
    },
    /// Comment on an issue
    Comment {
        id: String,
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Read and write comments
    Comments {
        #[command(subcommand)]
        cmd: CommentsCmd,
    },
    /// Append to an issue's notes
    Note {
        id: String,
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Hide an issue from `bd ready` until later
    Defer {
        id: String,
        #[arg(long, value_name = "WHEN", value_parser = parse::when)]
        until: Option<DateTime<Utc>>,
    },
    /// Undo a defer
    Undefer { id: String },
    /// Mark an issue as a duplicate of another
    Duplicate {
        id: String,
        #[arg(long, value_name = "ID")]
        of: String,
    },
    /// Mark an issue as superseded by another
    Supersede {
        id: String,
        #[arg(long, value_name = "ID")]
        with: String,
    },
    /// Link two issues with a non-blocking edge
    ///
    /// The gating types (blocks, parent-child, conditional-blocks, waits-for) are
    /// refused here: they have `bd dep add`.
    Link {
        from: String,
        to: String,
        #[arg(long = "type", value_name = "TYPE", value_parser = parse::link_type)]
        link_type: Option<DependencyType>,
    },
    /// Keep a claim alive
    #[command(visible_alias = "hb")]
    Heartbeat { id: String },
    /// Show an issue's workflow state
    State { id: String },
    /// Move an issue to a workflow state
    SetState { id: String, state: String },
    /// List the workspace's statuses
    Statuses,
    /// List the workspace's issue types
    Types,
    /// Promote an issue (e.g. a wisp to a real bead)
    Promote { id: String },
    /// Apply many changes at once, from JSON
    Batch {
        /// JSONL of operations; `-` for stdin
        file: Option<PathBuf>,
    },

    // ===================== Views =====================
    /// List issues
    List(ListArgs),
    /// Work that is claimable right now
    Ready(ReadyArgs),
    /// Work the graph is gating
    Blocked(BlockedArgs),
    /// Substring search over titles and descriptions
    Search(SearchArgs),
    /// Run a query expression
    Query(QueryArgs),
    /// Count issues matching a filter
    Count(CountArgs),
    /// Workspace summary
    #[command(visible_alias = "stats")]
    Status,
    /// An issue's audit trail
    History { id: String },
    /// Children of an issue
    Children { id: String },
    /// Epic rollups
    Epic {
        #[command(subcommand)]
        cmd: EpicCmd,
    },
    /// Workspace metadata
    Info,
    /// Issues nobody has touched in a while
    Stale {
        #[arg(long, default_value = "14d", value_name = "DUR", value_parser = parse::duration)]
        older_than: Duration,
    },
    /// Issues with no edges
    Orphans,
    /// Issues that look like duplicates
    Duplicates,
    /// Search for duplicates of a specific issue
    #[command(visible_alias = "find-dups")]
    FindDuplicates { id: String },
    /// Check the workspace for problems
    Lint,
    /// Diff two refs
    Diff { from: String, to: String },
    /// Run SQL against the store
    Sql { query: String },
    /// Workspace key/value store
    Kv {
        #[command(subcommand)]
        cmd: KvCmd,
    },
    /// Audit records
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
    /// Where the workspace lives
    Where,
    /// Context for an agent picking up work
    Context,
    /// Check that the store answers
    Ping,

    // ===================== Deps =====================
    /// Dependencies between issues
    Dep {
        #[command(subcommand)]
        cmd: DepCmd,
    },
    /// The dependency graph
    Graph {
        #[command(subcommand)]
        cmd: Option<GraphCmd>,
    },
    /// Flatten a hierarchy into a work list
    Flatten { id: String },
    /// Recompute the blocked cache across the whole graph
    RecomputeBlocked,

    // ===================== Sync =====================
    /// The dolt backend
    Dolt {
        #[command(subcommand)]
        cmd: DoltCmd,
    },
    /// Version control over the issue database
    Vc {
        #[command(subcommand)]
        cmd: VcCmd,
    },
    /// Show or switch branches
    Branch { name: Option<String> },
    /// Federated workspaces
    Federation {
        #[command(subcommand)]
        cmd: FederationCmd,
    },
    /// Linked repositories
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    /// Export issues as JSONL
    Export(ExportArgs),
    /// Import issues from JSONL
    Import(ImportArgs),
    /// Azure DevOps
    Ado {
        #[command(subcommand)]
        cmd: TrackerCmd,
    },
    /// Jira
    Jira {
        #[command(subcommand)]
        cmd: TrackerCmd,
    },
    /// Linear
    Linear {
        #[command(subcommand)]
        cmd: TrackerCmd,
    },
    /// GitHub Issues
    Github {
        #[command(subcommand)]
        cmd: TrackerCmd,
    },
    /// GitLab Issues
    Gitlab {
        #[command(subcommand)]
        cmd: TrackerCmd,
    },
    /// Notion
    Notion {
        #[command(subcommand)]
        cmd: TrackerCmd,
    },
    /// Messages between agents
    Mail { id: Option<String> },
    /// Publish a capability, so other projects can depend on it
    ///
    /// Finds the issue labelled `export:<capability>` and, once it is closed,
    /// labels it `provides:<capability>`.
    Ship {
        /// The capability to publish
        capability: String,
        /// Ship it even though the work is not closed
        #[arg(long)]
        force: bool,
        /// Report what would happen, write nothing
        #[arg(long)]
        dry_run: bool,
    },

    // ===================== Setup =====================
    /// Create a workspace
    Init(InitArgs),
    /// Set up beads in an existing repo
    Bootstrap,
    /// Write the beads section into your agent's instructions file
    ///
    /// Names a harness (claude, codex, factory, cursor, agents) or, given none,
    /// detects the ones this repo already uses.
    Setup {
        #[arg(value_name = "RECIPE")]
        recipe: Vec<String>,
    },
    /// Onboard an agent into this workspace
    Onboard,
    /// The 60-second tour
    Quickstart,
    /// Prime an agent's context from the workspace
    Prime,
    /// Git hooks
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },
    /// Workspace configuration
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Schema and format upgrades
    Upgrade {
        #[command(subcommand)]
        cmd: UpgradeCmd,
    },
    /// Print the version
    Version,
    /// Usage metrics
    Metrics {
        #[command(subcommand)]
        cmd: MetricsCmd,
    },
    /// Generate a shell completion script
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    // ===================== Maintenance =====================
    /// Diagnose the workspace
    Doctor {
        /// Repair what can be repaired automatically
        #[arg(long)]
        fix: bool,
    },
    /// Check that everything needed is present
    Preflight,
    /// Collect garbage (expired wisps, lapsed leases)
    Gc {
        /// Report what would be collected, write nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete closed issues permanently
    Purge {
        #[arg(long, default_value = "90d", value_name = "DUR", value_parser = parse::duration)]
        older_than: Duration,
        /// Report what would be deleted, write nothing
        #[arg(long)]
        dry_run: bool,
        /// Consent, in writing. Required when nobody can answer the prompt —
        /// which is every script, hook and agent.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Prune ephemeral beads
    Prune {
        /// Report what would be pruned, write nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Compact the database
    Compact,
    /// Backups
    Backup {
        #[command(subcommand)]
        cmd: BackupCmd,
    },
    /// Administrative operations
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// Migrate the schema
    Migrate,
    /// Change the workspace's id prefix
    RenamePrefix { from: String, to: String },
    /// Reclaim issues whose leases have lapsed
    Reclaim,
    /// Git worktrees
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
    },
    /// Merge slots
    MergeSlot {
        #[command(subcommand)]
        cmd: MergeSlotCmd,
    },

    // ===================== Advanced =====================
    /// Molecules: compound units of work
    Mol {
        #[command(subcommand)]
        cmd: MolCmd,
    },
    /// Formulas: reusable workflow templates
    Formula {
        #[command(subcommand)]
        cmd: FormulaCmd,
    },
    /// Run a formula
    Cook { formula: PathBuf },
    /// Agent swarms
    Swarm {
        #[command(subcommand)]
        cmd: SwarmCmd,
    },
    /// Gates: wait for a condition
    Gate {
        #[command(subcommand)]
        cmd: GateCmd,
    },
    /// Workspace rules
    Rules {
        #[command(subcommand)]
        cmd: RulesCmd,
    },
    /// Lightweight personal todos
    Todo {
        #[command(subcommand)]
        cmd: TodoCmd,
    },
    /// Questions waiting on a human
    Human {
        #[command(subcommand)]
        cmd: HumanCmd,
    },
    /// Remember something
    Remember {
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// List memories
    Memories,
    /// Forget a memory
    Forget { id: String },
    /// Search memories
    Recall {
        #[arg(required = true)]
        text: Vec<String>,
    },
}

impl Commands {
    /// Whether this command must be standing in a workspace.
    ///
    /// There is deliberately no "…and opens the database" case. The store opens
    /// lazily on first use, so this does not need to know, and — more to the
    /// point — it does not need *updating* every time a command graduates from
    /// stub to real. That list existed once; forgetting to add yourself to it
    /// produced a command that compiled, passed its tests, and then failed at
    /// runtime claiming there was no workspace.
    pub fn need(&self) -> Need {
        use Commands::*;
        match self {
            // Run before, or without, a workspace.
            Init(_) | Version | Completion { .. } | Quickstart | Doctor { .. } | Preflight
            | Onboard | Setup { .. } | Bootstrap => Need::Nothing,

            _ => Need::Workspace,
        }
    }
}

// ---------------------------------------------------------------------------
// Argument groups
// ---------------------------------------------------------------------------

#[derive(Args, Debug, Clone)]
pub struct CreateArgs {
    /// Title
    pub title: String,
    /// Body text
    #[arg(short = 'd', long)]
    pub description: Option<String>,
    /// P0 (critical) through P4 (trivial)
    #[arg(short = 'p', long)]
    pub priority: Option<Priority>,
    /// bug, feature, task, epic, chore, decision, ... (custom types are allowed)
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    pub issue_type: Option<IssueType>,
    /// Who is on it
    #[arg(short = 'a', long)]
    pub assignee: Option<String>,
    /// Repeatable
    #[arg(short = 'l', long = "label", value_name = "LABEL")]
    pub labels: Vec<String>,
    /// How it should be built
    #[arg(long)]
    pub design: Option<String>,
    /// What "done" means
    #[arg(long)]
    pub acceptance: Option<String>,
    /// Free-form notes
    #[arg(long)]
    pub notes: Option<String>,
    /// Hide from `bd ready` until then: a date, an RFC 3339 time, or `3d`
    #[arg(long, value_name = "WHEN", value_parser = parse::when)]
    pub defer_until: Option<DateTime<Utc>>,
    /// When it is due
    #[arg(long, value_name = "WHEN", value_parser = parse::when)]
    pub due: Option<DateTime<Utc>>,
    /// Edge to add on creation: `<id>` or `<id>:<type>` (repeatable)
    #[arg(long = "deps", value_name = "ID[:TYPE]", value_parser = parse::dep_spec)]
    pub deps: Vec<DepSpec>,
    /// Estimated effort
    #[arg(long, value_name = "MINUTES")]
    pub estimate: Option<i32>,
}

#[derive(Args, Debug, Clone)]
pub struct QuickArgs {
    /// Title
    pub title: String,
    /// P0 (critical) through P4 (trivial)
    #[arg(short = 'p', long)]
    pub priority: Option<Priority>,
    /// bug, feature, task, epic, ...
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    pub issue_type: Option<IssueType>,
    /// Repeatable
    #[arg(short = 'l', long = "label", value_name = "LABEL")]
    pub labels: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct UpdateArgs {
    pub id: String,
    /// Take the issue for the length of a lease, exclusively
    #[arg(long)]
    pub claim: bool,
    /// How long to hold it (default: `claim.lease` from config, else 1h)
    #[arg(long, value_name = "DUR", value_parser = parse::duration, requires = "claim")]
    pub lease: Option<Duration>,
    /// New title
    #[arg(long)]
    pub title: Option<String>,
    /// New body text
    #[arg(short = 'd', long)]
    pub description: Option<String>,
    /// How it should be built
    #[arg(long)]
    pub design: Option<String>,
    /// What "done" means
    #[arg(long)]
    pub acceptance: Option<String>,
    /// Free-form notes
    #[arg(long)]
    pub notes: Option<String>,
    /// open, in_progress, blocked, deferred, ... (to close, prefer `bd close`)
    #[arg(long)]
    pub status: Option<Status>,
    /// P0 (critical) through P4 (trivial)
    #[arg(short = 'p', long)]
    pub priority: Option<Priority>,
    /// bug, feature, task, epic, ...
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    pub issue_type: Option<IssueType>,
    /// Who is on it
    #[arg(short = 'a', long)]
    pub assignee: Option<String>,
    /// Estimated effort
    #[arg(long, value_name = "MINUTES")]
    pub estimate: Option<i32>,
    /// When it is due
    #[arg(long, value_name = "WHEN", value_parser = parse::when)]
    pub due: Option<DateTime<Utc>>,
    /// Hide from `bd ready` until then
    #[arg(long, value_name = "WHEN", value_parser = parse::when)]
    pub defer_until: Option<DateTime<Utc>>,
    /// JSON object
    #[arg(long)]
    pub metadata: Option<String>,
    /// The spec this issue implements
    #[arg(long)]
    pub spec_id: Option<String>,
    /// Id in an external tracker
    #[arg(long)]
    pub external_ref: Option<String>,
    /// Keep out of `bd ready` indefinitely
    #[arg(long, conflicts_with = "unpin")]
    pub pin: bool,
    /// Undo a pin
    #[arg(long)]
    pub unpin: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CloseArgs {
    /// One or more issue ids
    #[arg(required = true)]
    pub ids: Vec<String>,
    /// Why. Words like "failed" or "wontfix" make `conditional-blocks`
    /// dependents ready, so this is data, not decoration.
    #[arg(long, default_value = "done")]
    pub reason: String,
}

/// Filters shared by the read commands.
#[derive(Args, Debug, Clone, Default)]
pub struct FilterArgs {
    /// Exactly this priority
    #[arg(short = 'p', long)]
    pub priority: Option<Priority>,
    /// Only issues at least this urgent (P1 includes P0)
    #[arg(long, value_name = "P")]
    pub min_priority: Option<Priority>,
    /// bug, feature, task, epic, ...
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    pub issue_type: Option<IssueType>,
    /// Whose work
    #[arg(short = 'a', long)]
    pub assignee: Option<String>,
    /// Must carry every one of these (repeatable)
    #[arg(short = 'l', long = "label", value_name = "LABEL")]
    pub labels: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    #[command(flatten)]
    pub filter: FilterArgs,
    /// Repeatable; defaults to everything except closed
    #[arg(long, value_name = "STATUS")]
    pub status: Vec<Status>,
    /// Include closed issues
    #[arg(long)]
    pub all: bool,
    /// 0 for no limit
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
    /// Skip this many
    #[arg(long)]
    pub offset: Option<u32>,
    /// How to order the results
    #[arg(long, value_name = "hybrid|priority|oldest|updated|closed")]
    pub sort: Option<SortPolicy>,
}

#[derive(Args, Debug, Clone)]
pub struct ReadyArgs {
    #[command(flatten)]
    pub filter: FilterArgs,
    /// 0 for no limit
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
    /// hybrid keeps urgent new work visible without starving old work
    #[arg(long, value_name = "hybrid|priority|oldest")]
    pub sort: Option<SortPolicy>,
}

#[derive(Args, Debug, Clone)]
pub struct BlockedArgs {
    #[command(flatten)]
    pub filter: FilterArgs,
    /// 0 for no limit
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
}

#[derive(Args, Debug, Clone)]
pub struct SearchArgs {
    /// Text to look for in titles and descriptions
    pub text: String,
    #[command(flatten)]
    pub filter: FilterArgs,
    /// Include closed issues
    #[arg(long)]
    pub all: bool,
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
}

#[derive(Args, Debug, Clone)]
pub struct QueryArgs {
    /// e.g. `status=open AND priority<=1 AND label=infra`
    pub expr: String,
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
}

#[derive(Args, Debug, Clone)]
pub struct CountArgs {
    #[command(flatten)]
    pub filter: FilterArgs,
    /// Repeatable; defaults to everything except closed
    #[arg(long, value_name = "STATUS")]
    pub status: Vec<Status>,
    /// Include closed issues
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ExportArgs {
    /// Write here instead of stdout
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Leave closed issues out
    #[arg(long)]
    pub open_only: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ImportArgs {
    /// JSONL file; omit or `-` for stdin
    pub file: Option<PathBuf>,
    /// Report what would change, write nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// Where to create the workspace (default: here)
    pub path: Option<PathBuf>,
    /// Id prefix (default: derived from the directory name)
    #[arg(long)]
    pub prefix: Option<String>,
    /// The engine that will own this workspace. Chosen once, here: from now on
    /// the locator on disk is the authority (storage rule 3).
    #[arg(long, default_value = "sqlite")]
    pub backend: Backend,
    /// Re-initialize over an existing workspace
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DepAddArgs {
    /// The issue that gains the edge
    pub issue: String,
    /// What it depends on
    pub depends_on: String,
    #[arg(long = "type", default_value = "blocks", value_name = "TYPE")]
    pub dep_type: DependencyType,
}

// ---------------------------------------------------------------------------
// Subcommand families
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum LabelCmd {
    /// Add labels to an issue
    Add {
        id: String,
        #[arg(required = true)]
        labels: Vec<String>,
    },
    /// Remove labels from an issue
    Remove {
        id: String,
        #[arg(required = true)]
        labels: Vec<String>,
    },
    /// Labels on one issue
    List { id: String },
    /// Every label in the workspace
    ListAll,
    /// Push an epic's labels down to its children
    Propagate { id: String },
}

#[derive(Subcommand, Debug)]
pub enum CommentsCmd {
    /// An issue's comments
    List { id: String },
    /// Add a comment
    Add {
        id: String,
        #[arg(required = true)]
        text: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum DepCmd {
    /// Add an edge
    Add(DepAddArgs),
    /// Remove one edge
    ///
    /// The type matters: two issues may be joined by several edges at once, and
    /// only the one named here is removed.
    #[command(visible_alias = "rm")]
    Remove {
        issue: String,
        depends_on: String,
        #[arg(long = "type", default_value = "blocks", value_name = "TYPE")]
        dep_type: DependencyType,
    },
    /// The edges into and out of an issue
    List { id: String },
    /// What an issue is waiting on, as a tree
    Tree {
        id: String,
        #[arg(long, default_value_t = 10)]
        depth: u32,
    },
    /// Every cycle in the graph
    Cycles,
    /// Relate two issues (a non-blocking edge)
    Relate { from: String, to: String },
    /// Remove a relation
    Unrelate { from: String, to: String },
}

#[derive(Subcommand, Debug)]
pub enum GraphCmd {
    /// Check the graph for problems
    Check,
}

#[derive(Subcommand, Debug)]
pub enum EpicCmd {
    /// Progress of every epic
    Status,
    /// Epics whose children are all closed
    CloseEligible,
}

#[derive(Subcommand, Debug)]
pub enum KvCmd {
    Set { key: String, value: String },
    Get { key: String },
    Clear { key: String },
    List,
}

#[derive(Subcommand, Debug)]
pub enum AuditCmd {
    /// Record an audit entry
    Record {
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Label an audit entry
    Label { entry: String, label: String },
}

#[derive(Subcommand, Debug)]
pub enum DoltCmd {
    /// Show the dolt configuration
    Show,
    Set { key: String, value: String },
    /// Check that dolt is usable
    Test,
    Commit {
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    Push {
        remote: Option<String>,
        branch: Option<String>,
    },
    Pull {
        remote: Option<String>,
        branch: Option<String>,
    },
    /// Start the dolt sql-server
    Start,
    /// Stop the dolt sql-server
    Stop,
    Status,
    /// Kill every dolt process
    Killall,
    CleanDatabases,
    Remote {
        #[command(subcommand)]
        cmd: DoltRemoteCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum DoltRemoteCmd {
    Add { name: String, url: String },
    List,
    Remove { name: String },
}

#[derive(Subcommand, Debug)]
pub enum VcCmd {
    /// Merge a branch into the current one
    Merge { branch: String },
    Commit {
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
    Status,
}

#[derive(Subcommand, Debug)]
pub enum FederationCmd {
    Sync,
    Status,
    AddPeer { name: String, url: String },
    RemovePeer { name: String },
    ListPeers,
}

#[derive(Subcommand, Debug)]
pub enum RepoCmd {
    Add { path: PathBuf },
    Remove { name: String },
    List,
    Sync,
}

/// Every external tracker exposes the same four verbs.
#[derive(Subcommand, Debug)]
pub enum TrackerCmd {
    Sync,
    Status,
    Push,
    Pull,
}

#[derive(Subcommand, Debug)]
pub enum HooksCmd {
    Install,
    Uninstall,
    List,
    Run { hook: String },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    Set { key: String, value: String },
    Get { key: String },
    List,
    Unset { key: String },
    /// Check the configuration for problems
    Validate,
    /// Show the effective configuration, with its sources
    Show,
}

#[derive(Subcommand, Debug)]
pub enum UpgradeCmd {
    Status,
    Review,
    Ack,
}

#[derive(Subcommand, Debug)]
pub enum MetricsCmd {
    On,
    Off,
    Example,
}

#[derive(Subcommand, Debug)]
pub enum BackupCmd {
    Status,
    Init { path: PathBuf },
    Sync,
    Remove,
    Restore { path: Option<PathBuf> },
}

#[derive(Subcommand, Debug)]
pub enum AdminCmd {
    Cleanup,
    Compact,
    /// Throw the database away and start over
    Reset,
}

#[derive(Subcommand, Debug)]
pub enum WorktreeCmd {
    Create { name: String },
    List,
    Remove { name: String },
    Info,
}

#[derive(Subcommand, Debug)]
pub enum MergeSlotCmd {
    Create { name: String },
    Check,
    Acquire { name: String },
    Release { name: String },
}

#[derive(Subcommand, Debug)]
pub enum MolCmd {
    /// Join molecules
    Bond { ids: Vec<String> },
    /// Destroy a molecule
    Burn { id: String },
    /// The molecule being worked
    Current,
    /// Extract the reusable part of a molecule
    Distill { id: String },
    /// Molecules ready to work
    Ready,
    /// Instantiate a molecule from a template
    Seed { template: String },
    Show { id: String },
    /// Collapse a molecule into one bead
    Squash { id: String },
    Stale,
    /// Emit a molecule's work
    Pour { id: String },
    /// Create an ephemeral bead
    Wisp { title: Vec<String> },
}

#[derive(Subcommand, Debug)]
pub enum FormulaCmd {
    List,
    Show { name: String },
    /// Convert a formula between formats
    Convert { path: PathBuf },
    /// The formula schema
    #[command(visible_alias = "primitives")]
    Schema,
}

#[derive(Subcommand, Debug)]
pub enum SwarmCmd {
    Validate { path: PathBuf },
    Status,
    Create { name: String },
    List,
}

#[derive(Subcommand, Debug)]
pub enum GateCmd {
    List,
    Create { name: String },
    Show { id: String },
    Resolve { id: String },
    Check { id: String },
}

#[derive(Subcommand, Debug)]
pub enum RulesCmd {
    Audit,
    Compact,
}

#[derive(Subcommand, Debug)]
pub enum TodoCmd {
    Add { text: Vec<String> },
    List,
    Done { id: String },
}

#[derive(Subcommand, Debug)]
pub enum HumanCmd {
    List,
    Respond { id: String, text: Vec<String> },
    Dismiss { id: String },
    Stats,
}
