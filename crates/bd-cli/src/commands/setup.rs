//! Creating a workspace, and the commands that run before one exists.
//!
//! Most of this file has a reader who is not a person. beads' primary user is a
//! coding agent, and `prime` / `onboard` / `setup` are the only way one ever
//! learns the workflow — so for these commands the *output is the feature*, and
//! it is written to be pasted into a context window rather than admired.
//!
//! Two of them write files the user also writes (`CLAUDE.md`, `AGENTS.md`, git
//! hooks). Every such write goes through a marker: we replace exactly what we
//! wrote last time and refuse when we cannot tell. Losing a paragraph of
//! someone's own instructions to a tool that was supposed to help them is not a
//! bug you get to apologize for once.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bd_core::{Issue, IssueFilter, Status};
use bd_storage::{Backend, Identity, Locator};
use clap_complete::Shell;
use serde_json::{Value, json};

use crate::cli::{
    ConfigCmd, ExportArgs, HooksCmd, ImportArgs, InitArgs, MetricsCmd, UpgradeCmd,
};
use crate::commands::stub;
use crate::context::{Config, Ctx};
use crate::exit::{self, SilentExit};

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// The other place a concrete backend may be named (see [`crate::context`]).
///
/// It is legitimate *here* and nowhere else: at `init` there is nothing on disk
/// to contradict, so the flag decides. Afterwards the locator does, forever.
pub async fn init(ctx: &Ctx, a: InitArgs) -> Result<()> {
    ctx.ensure_writable("initialize a workspace")?;

    let root = match &a.path {
        Some(p) => {
            std::fs::create_dir_all(p)?;
            std::fs::canonicalize(p)?
        }
        None => ctx.cwd.clone(),
    };
    guard_existing(ctx, &root, a.force)?;

    if !matches!(a.backend, Backend::Sqlite | Backend::Dolt) {
        // Not a capability gap — a backend this port has not built. Exit 64, so
        // a script can tell "come back later" from "never".
        return stub(&format!("init --backend={}", a.backend), ctx);
    }

    let r = create_workspace(ctx, &root, a.prefix.clone(), a.backend).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "workspace": r.beads_dir,
            "backend": r.backend.as_str(),
            "prefix": r.prefix,
            "workspace_id": r.workspace_id,
        }))?;
    } else {
        ctx.out.line(format!(
            "Initialized a {} workspace at {}",
            r.backend,
            r.beads_dir.display()
        ));
        ctx.out.line(format!("Issue ids will look like {}-a3f2", r.prefix));
    }
    Ok(())
}

/// A fresh workspace id. Same shape as the one `bd_sqlite::init` mints — it is
/// how clones recognize each other, so the two backends must agree on it.
fn new_workspace_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// What creating a workspace produced.
///
/// `init` and `bootstrap` both need this, and `bootstrap` must not let `init`
/// print: two JSON documents on one stdout is a corrupt stream, so the work and
/// the rendering are separated here rather than reused by calling `init`.
struct InitReport {
    beads_dir: PathBuf,
    backend: Backend,
    prefix: String,
    workspace_id: String,
}

fn guard_existing(ctx: &Ctx, root: &Path, force: bool) -> Result<()> {
    let beads_dir = root.join(bd_storage::locator::BEADS_DIR);
    if !beads_dir.join(bd_storage::locator::LOCATOR_FILE).exists() {
        return Ok(());
    }
    if !force {
        bail!(
            "a beads workspace already exists at {} (use --force to re-initialize)",
            beads_dir.display()
        );
    }
    // --force rewrites the locator over a database that may hold work. Say so
    // out loud; the only thing worse than losing a workspace is losing it
    // quietly.
    ctx.out.warn(format!(
        "re-initializing over the existing workspace at {}",
        beads_dir.display()
    ));
    Ok(())
}

async fn create_workspace(
    ctx: &Ctx,
    root: &Path,
    prefix: Option<String>,
    backend: Backend,
) -> Result<InitReport> {
    let beads_dir = root.join(bd_storage::locator::BEADS_DIR);
    let prefix = prefix.unwrap_or_else(|| derive_prefix(root));
    let identity = Identity {
        actor: ctx.identity.actor.clone(),
        session: ctx.identity.session.clone(),
    };

    let locator = match backend {
        // `bd_sqlite::init` takes the project *root*, creates `.beads/` under it,
        // and writes the locator itself — preserving the workspace_id across a
        // re-init. Writing our own locator afterwards would rotate that id and
        // fork the workspace from itself, so we read the one it wrote.
        Backend::Sqlite => {
            let store = bd_sqlite::init(root, &prefix, identity).await?;
            store.close().await?;
            Locator::load(&beads_dir)?
        }
        // `bd_dolt::init` takes the `.beads` directory — which *is* the dolt
        // repository — and does not write a locator. So we write it, and we write
        // it **first**: a `dolt init` that fails halfway must still leave a
        // workspace that can say what it is, or `bd doctor` cannot diagnose it.
        Backend::Dolt => {
            // Before anything is written. A machine with no `dolt` must end up
            // with no workspace at all, rather than a `.beads/` that says "I am a
            // dolt workspace" over an empty hole. (Past this point a failure
            // *does* leave the locator, deliberately: a half-initialized
            // workspace that can still say what it is, is one `bd doctor` can
            // diagnose.)
            if !bd_dolt::dolt_available() {
                bail!(
                    "`bd init --backend=dolt` needs the `dolt` binary on PATH.\n\
                     Install it from https://github.com/dolthub/dolt (`brew install dolt`, \
                     `winget install DoltHub.Dolt`), or use the default sqlite backend."
                );
            }
            std::fs::create_dir_all(&beads_dir)?;
            let existing = Locator::load(&beads_dir).ok();
            let locator = Locator::new(
                Backend::Dolt,
                // Preserve the id across `--force`, for the same reason sqlite does.
                existing.map_or_else(new_workspace_id, |l| l.workspace_id),
                &beads_dir,
            );
            locator.save()?;
            let store = bd_dolt::init(&beads_dir, &prefix, identity).await?;
            store.close().await?;
            locator
        }
        other => bail!("init --backend={other} is not implemented"),
    };

    let config = Config {
        prefix: Some(prefix.clone()),
        ..Default::default()
    };
    config.save(&beads_dir)?;

    Ok(InitReport {
        beads_dir,
        backend,
        prefix,
        workspace_id: locator.workspace_id,
    })
}

/// `my-project` -> `my-project`; `My Project!` -> `myproject`. Falls back to
/// `bd` rather than to an empty prefix, which would mint ids like `-a3f2`.
fn derive_prefix(root: &std::path::Path) -> String {
    let name: String = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bd")
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(12)
        .collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() { "bd".to_string() } else { name }
}

pub fn version(ctx: &Ctx) -> Result<()> {
    let v = env!("CARGO_PKG_VERSION");
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "version": v,
            "implementation": "rust",
            "backend": ctx.backend().map(|b| b.as_str()),
        }))?;
    } else {
        println!("bd {v} (rust)");
    }
    Ok(())
}

pub fn completion(shell: Shell) -> Result<()> {
    let mut cmd = crate::cli::build();
    clap_complete::generate(shell, &mut cmd, "bd", &mut std::io::stdout());
    Ok(())
}

// ---------------------------------------------------------------------------
// The agent's workflow, in three shapes
// ---------------------------------------------------------------------------

/// The loop an agent runs. Every one of these four lines is load-bearing: skip
/// step 2 and two agents work the same issue; skip step 4 and the dependents of
/// what you just finished stay blocked forever.
const LOOP: &[(&str, &str)] = &[
    ("bd ready --json", "pick one — it is unblocked and unclaimed"),
    (
        "bd update <id> --claim",
        "take it; the claim is a lease, so a dead agent frees its work",
    ),
    (
        "<do the work>",
        "bd comment <id> \"<finding>\" as you learn things",
    ),
    (
        "bd close <id> --reason done",
        "closing a blocker is what makes its dependents ready",
    ),
];

/// The commands worth spending context on. Deliberately short — `bd --help`
/// lists ~120 and an agent does not need 120.
const COMMANDS: &[(&str, &str)] = &[
    ("bd ready", "claimable work, most urgent first"),
    ("bd show <id>", "one issue in full, with its edges"),
    ("bd update <id> --claim", "take an issue for a lease"),
    (
        "bd create \"<title>\" -t task -p 1",
        "file work: -t bug|feature|task|epic|chore, -p 0..4 (P0 is most urgent)",
    ),
    ("bd q \"<title>\"", "quick capture; prints only the new id"),
    (
        "bd dep add <id> <blocker>",
        "an edge is how you say \"not yet\"",
    ),
    ("bd blocked", "what the graph is gating"),
    ("bd comment <id> \"<text>\"", "leave a finding on the issue"),
    (
        "bd close <id> --reason done",
        "reasons: done | wontfix | duplicate | failed",
    ),
    ("bd status", "counts for the whole workspace"),
];

/// The markdown `bd onboard` prints and `bd setup` writes.
///
/// Held to about fifteen lines on purpose. It lands in a file that is prepended
/// to every single agent turn, so every line here is paid for over and over —
/// and it competes with the user's own instructions for attention.
///
/// It is a *constant*: nothing about the workspace is interpolated into it. That
/// is what makes `bd setup` genuinely idempotent — a block that embedded, say,
/// the current ready count would rewrite the file on every run and show up in
/// every diff.
const BLOCK_BODY: &str = r#"## Issue tracking: beads (`bd`)

This repo tracks work in beads. Use it — do not keep a private TODO list, and do
not invent your own scratch file. Run `bd prime` before you start: it prints the
loop and the current state of the board in one screen.

**The loop.**

1. `bd ready --json` — work that is unblocked and unclaimed. Pick one.
2. `bd update <id> --claim` — take it. The claim is a lease and it expires, so an
   agent that dies does not hold its work hostage.
3. Do the work. `bd comment <id> "<what you learned>"` as you go.
4. `bd close <id> --reason done` — closing a blocker is what makes its dependents
   ready. Skip this and the next agent sees nothing to do.

**Filing.** `bd create "<title>" -t bug|feature|task|epic|chore -p 0..4` (P0 is
most urgent). Found something while working on something else? File it instead
of fixing it inline: `bd q "<title>"` prints just the new id.

**Dependencies are the point.** `bd dep add <issue> <blocker>` — beads decides
what is ready from the graph, so an edge is how you say "not yet". `bd blocked`
shows what is waiting and on what.

Every command takes `--json`. Never edit `.beads/` by hand.
"#;

const BEGIN: &str = "<!-- BEGIN BEADS -->";
const END: &str = "<!-- END BEADS -->";

fn managed_block() -> String {
    format!("{BEGIN}\n{BLOCK_BODY}{END}\n")
}

/// The snippet a user pastes into their agent's instructions file.
///
/// `bd setup` writes this for you; `onboard` is for when you would rather do it
/// yourself, or diff it first.
pub async fn onboard(ctx: &Ctx) -> Result<()> {
    let block = managed_block();
    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "markdown": block,
            "targets": ["CLAUDE.md", "AGENTS.md"],
            "hint": "`bd setup` writes this into the right file for you, idempotently",
        }));
    }
    // Straight to stdout, not through `out.line`: this is the payload, and
    // `--quiet` must not be able to swallow the only thing the command exists to
    // produce.
    print!("{block}");
    Ok(())
}

/// The 60-second tour, for a human. Runs with no workspace — it is what you read
/// *before* you have one.
pub async fn quickstart(ctx: &Ctx) -> Result<()> {
    let existing = ctx.locator.as_ref().map(|l| l.dir.clone());

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "workspace": existing,
            "steps": [
                { "step": "create a workspace", "run": "bd init", "done": existing.is_some() },
                { "step": "teach your coding agent to use it", "run": "bd setup" },
                { "step": "file some work", "run": "bd create \"Write the parser\" -p 1" },
                { "step": "say what blocks what", "run": "bd dep add <issue> <blocker>" },
                { "step": "ask what to do", "run": "bd ready" },
            ],
            "for_agents": "bd prime",
        }));
    }

    ctx.out.line("beads in 60 seconds\n");
    ctx.out.line(
        "beads tracks work as a graph. You file issues, you say what blocks what, and\n\
         `bd ready` answers the only question that matters: what can be worked on now.\n",
    );
    match &existing {
        Some(dir) => ctx.out.line(format!(
            "1. Create a workspace\n     you already have one at {}\n",
            dir.display()
        )),
        None => ctx.out.line("1. Create a workspace\n     bd init\n"),
    }
    ctx.out.line(
        "2. Teach your coding agent to use it\n     \
         bd setup      writes a beads section into CLAUDE.md / AGENTS.md\n     \
         bd onboard    prints that section, if you would rather paste it yourself\n",
    );
    ctx.out.line(
        "3. File some work\n     \
         bd create \"Write the parser\" -p 1 -t task\n     \
         bd q \"Fix the flaky test\"      prints just the id\n",
    );
    ctx.out.line(
        "4. Say what blocks what\n     \
         bd dep add <issue> <blocker>\n",
    );
    ctx.out.line(
        "5. Ask what to do\n     \
         bd ready       claimable now\n     \
         bd blocked     and what each one is waiting on\n",
    );
    ctx.out.line(
        "Your agent should run `bd prime` before it starts work — that prints the loop\n\
         and the current state of the board together.\n\n\
         Every command takes --json.",
    );
    Ok(())
}

/// Everything an agent should have in context before it touches tracked work.
///
/// This is the command agents are told to run first, so it is the one place the
/// whole product has to explain itself. It is also pasted into a context window,
/// which is a budget: the workflow, the ten commands that matter, and the state
/// of the board — and nothing else.
pub async fn prime(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let stats = store.stats().await?;
    let next = store.ready_work(&IssueFilter::ready().with_limit(5)).await?;

    // What this actor already holds. An agent resuming after a crash needs to
    // find its own claims before it takes new ones — otherwise it abandons the
    // work it was in the middle of and lets the lease expire for nothing.
    let mine = store
        .list_issues(&IssueFilter {
            statuses: vec![Status::InProgress],
            assignee: Some(ctx.identity.actor.clone()),
            limit: Some(5),
            ..Default::default()
        })
        .await?;

    let loc = ctx.locator()?;

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "workspace": loc.dir,
            "backend": loc.backend.as_str(),
            "prefix": ctx.prefix().await,
            "actor": ctx.identity.actor,
            "loop": LOOP.iter().map(|(c, w)| json!({ "run": c, "why": w })).collect::<Vec<_>>(),
            "commands": COMMANDS.iter().map(|(c, w)| json!({ "run": c, "what": w })).collect::<Vec<_>>(),
            "state": {
                "ready": stats.ready,
                "in_progress": stats.in_progress,
                "blocked": stats.blocked,
                "open": stats.open,
                "closed": stats.closed,
                "total": stats.total,
            },
            "next": next.iter().map(brief).collect::<Vec<_>>(),
            "yours": mine.iter().map(brief).collect::<Vec<_>>(),
        }));
    }

    ctx.out.line(format!(
        "beads — {} ({}, prefix {}), you are {}\n",
        loc.dir.display(),
        loc.backend,
        ctx.prefix().await,
        ctx.identity.actor
    ));

    ctx.out.line("THE LOOP");
    for (i, (cmd, why)) in LOOP.iter().enumerate() {
        ctx.out.line(format!("  {}. {cmd:<28} {why}", i + 1));
    }

    ctx.out.line("\nCOMMANDS");
    for (cmd, what) in COMMANDS {
        ctx.out.line(format!("  {cmd:<34} {what}"));
    }

    ctx.out.line("\nSTATE");
    ctx.out.line(format!(
        "  {} ready   {} in progress   {} blocked   {} closed   ({} total)",
        stats.ready, stats.in_progress, stats.blocked, stats.closed, stats.total
    ));

    if !mine.is_empty() {
        ctx.out.line("\n  yours, already claimed — finish or unclaim these first:");
        for i in &mine {
            ctx.out.line(format!("    {}", one_line(i)));
        }
    }

    if next.is_empty() {
        ctx.out.line(
            "\n  Nothing is ready. `bd blocked` says what the graph is gating;\n  \
             `bd create \"<title>\"` files something new.",
        );
    } else {
        ctx.out.line("\n  ready now:");
        for i in &next {
            ctx.out.line(format!("    {}", one_line(i)));
        }
        if stats.ready > next.len() as u64 {
            ctx.out.line(format!(
                "    … and {} more (`bd ready`)",
                stats.ready - next.len() as u64
            ));
        }
    }
    Ok(())
}

fn one_line(i: &Issue) -> String {
    format!(
        "{:<12} P{}  {:<8} {}",
        i.id,
        i.priority.0,
        i.issue_type.as_str(),
        i.title
    )
}

fn brief(i: &Issue) -> Value {
    json!({
        "id": i.id,
        "title": i.title,
        "priority": i.priority.0,
        "issue_type": i.issue_type.as_str(),
        "status": i.status.as_str(),
    })
}

// ---------------------------------------------------------------------------
// setup — writing agent integration files
// ---------------------------------------------------------------------------

/// An agent harness, and the file it reads its standing instructions from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    /// Claude Code — `CLAUDE.md`.
    Claude,
    /// Codex, Factory, Cursor, and anything else that reads the `AGENTS.md`
    /// convention.
    Agents,
}

impl Recipe {
    pub fn parse(s: &str) -> Result<Recipe> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Recipe::Claude),
            "codex" | "factory" | "droid" | "cursor" | "agents" => Ok(Recipe::Agents),
            other => bail!(
                "unknown setup recipe: {other} (known: claude, codex, factory, cursor, agents)"
            ),
        }
    }

    pub fn file(self) -> &'static str {
        match self {
            Recipe::Claude => "CLAUDE.md",
            Recipe::Agents => "AGENTS.md",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Recipe::Claude => "claude",
            Recipe::Agents => "agents",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Created,
    /// The managed block was there and has been replaced.
    Updated,
    /// The file existed with no managed block; ours was added below their prose.
    Appended,
    Unchanged,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Created => "created",
            Action::Updated => "updated",
            Action::Appended => "appended",
            Action::Unchanged => "unchanged",
        }
    }
}

struct Applied {
    recipe: Recipe,
    path: PathBuf,
    action: Action,
}

/// Wire beads into whatever agent harness this repo already uses.
///
/// `cli.rs` registers `setup` with no argument and it is frozen, so this detects
/// the harness rather than being told. The recipe machinery underneath
/// ([`Recipe`], [`setup_recipes`]) is public and takes an explicit list, so
/// `bd setup <recipe>` is a one-line change in `cli.rs` + `mod.rs` whenever the
/// integrator wants it.
pub async fn setup(ctx: &Ctx) -> Result<()> {
    setup_cmd(ctx, &[]).await
}

/// `bd setup [recipe…]`, ready for the day `cli.rs` can carry the argument.
///
/// Named recipes win; an empty list means "look at the repo and decide". Wiring
/// it up is two lines the integrator owns — `Setup { recipe: Vec<String> }` in
/// `cli.rs` and `C::Setup { recipe } => setup::setup_cmd(ctx, &recipe).await` in
/// `mod.rs` — and nothing in here has to change.
pub async fn setup_cmd(ctx: &Ctx, recipes: &[String]) -> Result<()> {
    let root = project_root(ctx);
    let recipes = match recipes.is_empty() {
        true => detect_recipes(&root),
        false => recipes
            .iter()
            .map(|s| Recipe::parse(s))
            .collect::<Result<Vec<_>>>()?,
    };
    setup_recipes(ctx, &root, &recipes)
}

pub fn setup_recipes(ctx: &Ctx, root: &Path, recipes: &[Recipe]) -> Result<()> {
    ctx.ensure_writable("write agent instructions")?;
    let applied = apply_recipes(root, recipes)?;

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "root": root,
            "files": applied.iter().map(applied_json).collect::<Vec<_>>(),
        }));
    }
    for a in &applied {
        ctx.out.line(format!(
            "{} {} ({})",
            match a.action {
                Action::Unchanged => "Already current:",
                _ => "Wrote the beads section to",
            },
            a.path.display(),
            a.action.as_str()
        ));
    }
    ctx.out.line(format!(
        "\nOnly the block between {BEGIN} and {END} is managed by bd — everything\n\
         outside it is yours and is never touched. Re-run `bd setup` after upgrading."
    ));
    Ok(())
}

fn applied_json(a: &Applied) -> Value {
    json!({
        "recipe": a.recipe.name(),
        "file": a.path,
        "action": a.action.as_str(),
    })
}

fn apply_recipes(root: &Path, recipes: &[Recipe]) -> Result<Vec<Applied>> {
    let mut applied: Vec<Applied> = Vec::new();
    for r in recipes {
        let path = root.join(r.file());
        // `codex` and `factory` are different harnesses that read the same file.
        // Applying both would splice the block, then splice it again.
        if applied.iter().any(|a| a.path == path) {
            continue;
        }
        let action = apply_recipe(&path)?;
        applied.push(Applied {
            recipe: *r,
            path,
            action,
        });
    }
    Ok(applied)
}

fn apply_recipe(path: &Path) -> Result<Action> {
    let block = managed_block();
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow!("cannot read {}: {e}", path.display())),
    };

    let (next, action) = match existing {
        None => (block, Action::Created),
        Some(cur) => {
            let had_block = cur.contains(BEGIN);
            let next = splice(&cur, &block)
                .with_context(|| format!("refusing to rewrite {}", path.display()))?;
            let action = if next == cur {
                Action::Unchanged
            } else if had_block {
                Action::Updated
            } else {
                Action::Appended
            };
            (next, action)
        }
    };

    if action != Action::Unchanged {
        write_atomic(path, &next)?;
    }
    Ok(action)
}

/// Put `block` into `existing`, replacing a previous copy of itself and touching
/// nothing else.
///
/// The dangerous case is a *half* pair of markers — a BEGIN with no END, or an
/// END with no BEGIN. That means someone edited the file by hand, and there is
/// no way to know where their text ends and ours began. Any guess we make is a
/// guess about which of their paragraphs to delete, so we refuse instead and let
/// them fix the markers. Refusing is the feature; the file is theirs.
fn splice(existing: &str, block: &str) -> Result<String> {
    let mut begin: Option<usize> = None;
    let mut end: Option<usize> = None; // byte offset just past the END line
    let mut orphan_end = false;
    let mut off = 0usize;

    // Line-wise so that CRLF files and indented markers still match: `trim`
    // eats the `\r` that a byte search for "-->\n" would trip over.
    for line in existing.split_inclusive('\n') {
        let next = off + line.len();
        match line.trim() {
            BEGIN if begin.is_none() => begin = Some(off),
            END => {
                if begin.is_some() {
                    if end.is_none() {
                        end = Some(next);
                    }
                } else {
                    orphan_end = true;
                }
            }
            _ => {}
        }
        off = next;
    }

    match (begin, end) {
        (Some(b), Some(e)) => {
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..b]);
            out.push_str(block); // always ends in a newline
            out.push_str(&existing[e..]);
            Ok(out)
        }
        (Some(_), None) => bail!(
            "found `{BEGIN}` with no matching `{END}`: beads cannot tell where its \
             block ends, and will not guess. Add the closing marker (or delete the \
             opening one) and run this again."
        ),
        (None, _) if orphan_end => bail!(
            "found `{END}` with no matching `{BEGIN}`: beads cannot tell where its \
             block begins, and will not guess. Fix the markers and run this again."
        ),
        // No markers at all: this file is entirely the user's. Add ours *below*
        // everything they wrote, without rewriting a byte of it.
        (None, _) => {
            let mut out = String::with_capacity(existing.len() + block.len() + 2);
            out.push_str(existing);
            if !existing.is_empty() {
                if !existing.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(block);
            Ok(out)
        }
    }
}

/// Write via a temporary and rename, so an interrupted `bd setup` cannot leave a
/// truncated CLAUDE.md where a whole one used to be.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("bd-tmp");
    std::fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("cannot replace {}: {e}", path.display()));
    }
    Ok(())
}

/// Which harnesses this repo already uses. A file that exists is proof; a config
/// directory is proof. Absent both, `AGENTS.md` is the cross-harness convention
/// and the safest single file to create.
fn detect_recipes(root: &Path) -> Vec<Recipe> {
    let mut v = Vec::new();
    if root.join("CLAUDE.md").exists() || root.join(".claude").is_dir() {
        v.push(Recipe::Claude);
    }
    if root.join("AGENTS.md").exists()
        || root.join(".codex").is_dir()
        || root.join(".factory").is_dir()
        || root.join(".cursor").is_dir()
    {
        v.push(Recipe::Agents);
    }
    if v.is_empty() {
        v.push(Recipe::Agents);
    }
    v
}

/// Where an agent's instructions file belongs: the top of the repo, not wherever
/// the agent happened to be standing when it ran this.
fn project_root(ctx: &Ctx) -> PathBuf {
    if let Some(l) = &ctx.locator
        && let Some(parent) = l.dir.parent()
    {
        return parent.to_path_buf();
    }
    git_root(&ctx.cwd).unwrap_or_else(|| ctx.cwd.clone())
}

// ---------------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------------

/// `init` + `setup`, for someone who has a repo and no beads.
///
/// Idempotent by construction: an existing workspace is kept (this is not
/// `init --force`) and the managed block is replaced rather than duplicated, so
/// running it twice is a no-op rather than a second workspace.
pub async fn bootstrap(ctx: &Ctx) -> Result<()> {
    ctx.ensure_writable("bootstrap a workspace")?;

    // Deliberately the repo root, not the cwd: `bd bootstrap` from `src/` should
    // not bury a workspace three levels down inside the project it is tracking.
    let root = project_root(ctx);
    let report = match ctx.locator.is_some() {
        true => None,
        false => Some(create_workspace(ctx, &root, None, Backend::Sqlite).await?),
    };
    let applied = apply_recipes(&root, &detect_recipes(&root))?;

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "root": root,
            "created": report.is_some(),
            "workspace": report.as_ref().map(|r| r.beads_dir.clone()),
            "backend": report.as_ref().map(|r| r.backend.as_str()),
            "prefix": report.as_ref().map(|r| r.prefix.clone()),
            "workspace_id": report.as_ref().map(|r| r.workspace_id.clone()),
            "files": applied.iter().map(applied_json).collect::<Vec<_>>(),
        }));
    }

    match &report {
        Some(r) => {
            ctx.out.line(format!(
                "Initialized a {} workspace at {}",
                r.backend,
                r.beads_dir.display()
            ));
            ctx.out.line(format!("Issue ids will look like {}-a3f2", r.prefix));
        }
        None => ctx.out.line(format!(
            "Kept the existing workspace at {}",
            ctx.locator()?.dir.display()
        )),
    }
    for a in &applied {
        ctx.out.line(format!(
            "Wrote the beads section to {} ({})",
            a.path.display(),
            a.action.as_str()
        ));
    }
    ctx.out.line(
        "\nNext:\n  \
         bd create \"<the first thing>\" -p 1     file some work\n  \
         bd prime                               what an agent should read before it starts",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

pub async fn config(ctx: &Ctx, cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Set { key, value } => {
            ctx.ensure_writable("set a config key")?;
            let store = ctx.store().await?;
            store.set_config(&key, &value).await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({ "key": key, "value": value }))?;
            } else {
                ctx.out.line(format!("{key} = {value}"));
            }
            Ok(())
        }
        ConfigCmd::Get { key } => {
            let store = ctx.store().await?;
            let v = store.get_config(&key).await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({ "key": key, "value": v }))?;
            } else {
                match v {
                    Some(v) => println!("{v}"),
                    None => bail!("no such config key: {key}"),
                }
            }
            Ok(())
        }
        ConfigCmd::List => {
            let store = ctx.store().await?;
            let entries = store.list_config().await?;
            if ctx.out.is_json() {
                let map: serde_json::Map<String, serde_json::Value> = entries
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::String(v)))
                    .collect();
                ctx.out.json_value(&map)?;
            } else if entries.is_empty() {
                ctx.out.line("No configuration set.");
            } else {
                for (k, v) in entries {
                    println!("{k} = {v}");
                }
            }
            Ok(())
        }
        ConfigCmd::Unset { .. } => stub("config unset", ctx),
        ConfigCmd::Validate => stub("config validate", ctx),
        ConfigCmd::Show => stub("config show", ctx),
    }
}

// ---------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------

const METRICS_KEY: &str = "metrics.enabled";

/// Said the same way in every mode, because the alternative is a tool that lets
/// you believe you turned off something it was never doing.
const NO_TELEMETRY: &str =
    "This port has no telemetry. There is no network code in bd: nothing is collected \
     and nothing is sent anywhere. `metrics.enabled` is a local config key and that is all.";

pub async fn metrics(ctx: &Ctx, cmd: MetricsCmd) -> Result<()> {
    match cmd {
        MetricsCmd::On | MetricsCmd::Off => {
            let on = matches!(cmd, MetricsCmd::On);
            ctx.ensure_writable("change the metrics setting")?;
            let store = ctx.store().await?;
            store
                .set_config(METRICS_KEY, if on { "true" } else { "false" })
                .await?;

            if ctx.out.is_json() {
                ctx.out.json_value(&json!({
                    "key": METRICS_KEY,
                    "enabled": on,
                    "sends_data": false,
                    "note": NO_TELEMETRY,
                }))?;
            } else {
                ctx.out.line(format!("{METRICS_KEY} = {on}"));
                ctx.out.line(NO_TELEMETRY);
            }
            Ok(())
        }
        MetricsCmd::Example => {
            let store = ctx.store().await?;
            let enabled = store
                .get_config(METRICS_KEY)
                .await?
                .is_some_and(|v| v == "true");
            // Illustrative only — this is a shape, not a record of anything that
            // happened, and nothing assembles or transmits it.
            let example = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "implementation": "rust",
                "backend": ctx.backend().map(|b| b.as_str()),
                "command": "close",
                "duration_ms": 12,
                "issue_count": 42,
            });

            if ctx.out.is_json() {
                ctx.out.json_value(&json!({
                    "enabled": enabled,
                    "sends_data": false,
                    "note": NO_TELEMETRY,
                    "example_payload": example,
                }))?;
            } else {
                ctx.out.line(NO_TELEMETRY);
                ctx.out.line(format!("\n{METRICS_KEY} = {enabled}"));
                ctx.out
                    .line("\nIf bd did send something, this is the shape it would have:\n");
                println!("{}", serde_json::to_string_pretty(&example)?);
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// upgrade
// ---------------------------------------------------------------------------

/// The last version of `bd` this workspace was told about.
const ACKED_KEY: &str = "upgrade.acked_version";

/// Whether the tool under this workspace moved since anyone last looked.
///
/// The point is not the binary — you can see that with `bd version`. It is that
/// an agent primed against 0.1.0 may be carrying stale instructions, and this is
/// how a harness notices without a human in the loop.
pub async fn upgrade(ctx: &Ctx, cmd: UpgradeCmd) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let store = ctx.store().await?;
    let acked = store.get_config(ACKED_KEY).await?;
    let pending = acked.as_deref() != Some(current);

    match cmd {
        UpgradeCmd::Status | UpgradeCmd::Review => {
            let review = matches!(cmd, UpgradeCmd::Review);
            if ctx.out.is_json() {
                return ctx.out.json_value(&json!({
                    "version": current,
                    "acked_version": acked,
                    "pending": pending,
                    // Honest about the gap rather than inventing a changelog: the
                    // port ships no release notes, so "review" can only tell you
                    // the tool moved, not what moved in it.
                    "release_notes": Value::Null,
                    "next": if pending { "bd upgrade ack" } else { "nothing to do" },
                }));
            }

            match &acked {
                Some(v) if !pending => {
                    ctx.out.line(format!("bd {current} — acknowledged. Nothing to do."));
                    let _ = v;
                }
                Some(v) => {
                    ctx.out.line(format!(
                        "This workspace last acknowledged bd {v}; you are running {current}."
                    ));
                }
                None => ctx.out.line(format!(
                    "This workspace has never acknowledged a bd version; you are running {current}."
                )),
            }
            if pending && review {
                ctx.out.line(
                    "\nThis port bundles no release notes, so there is nothing to read here\n\
                     beyond the version change itself. What it means in practice: the workflow\n\
                     an agent was primed with may be stale — re-run `bd prime`, and `bd setup`\n\
                     to refresh the block in CLAUDE.md / AGENTS.md.",
                );
            }
            if pending {
                ctx.out.line("\nAcknowledge with: bd upgrade ack");
            }
            Ok(())
        }
        UpgradeCmd::Ack => {
            ctx.ensure_writable("acknowledge an upgrade")?;
            store.set_config(ACKED_KEY, current).await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({
                    "acked_version": current,
                    "previous": acked,
                }))?;
            } else {
                ctx.out.line(format!("Acknowledged bd {current}."));
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

/// The line that says a hook is ours. Its absence is the *only* thing standing
/// between someone's carefully written pre-commit hook and `bd hooks install`.
const HOOK_MARKER: &str = "beads-managed-hook";

/// The git-tracked text form of the database. The db is a cache of this, not the
/// other way round — which is why it is worth a hook.
const JSONL: &str = "issues.jsonl";

const KNOWN_HOOKS: &[&str] = &["pre-commit", "post-merge"];

pub async fn hooks(ctx: &Ctx, cmd: HooksCmd) -> Result<()> {
    match cmd {
        HooksCmd::Install => hooks_install(ctx),
        HooksCmd::Uninstall => hooks_uninstall(ctx),
        HooksCmd::List => hooks_list(ctx),
        HooksCmd::Run { hook } => hooks_run(ctx, &hook).await,
    }
}

fn hook_script(hook: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # {HOOK_MARKER} — written by `bd hooks install`, removed by `bd hooks uninstall`.\n\
         # Do not delete the line above: it is how bd knows this hook is its own and\n\
         # not yours. Without it bd will refuse to touch this file, which is correct.\n\
         #\n\
         # Keeps .beads/{JSONL} in step with the database, so git carries the issues.\n\
         # A missing bd exits 0 on purpose: a tool that is not installed must never be\n\
         # able to block a commit.\n\
         command -v bd >/dev/null 2>&1 || exit 0\n\
         exec bd hooks run {hook}\n"
    )
}

fn hooks_install(ctx: &Ctx) -> Result<()> {
    ctx.ensure_writable("install git hooks")?;
    let dir = require_hooks_dir(ctx)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;

    let mut installed: Vec<PathBuf> = Vec::new();
    let mut refused: Vec<PathBuf> = Vec::new();

    for hook in KNOWN_HOOKS {
        let path = dir.join(hook);
        // Someone else's hook. Overwriting it would destroy work we did not write
        // and cannot reconstruct — so we stop, and hand back the one line they
        // need to chain us in themselves. This branch is the whole point of the
        // marker.
        if let Ok(existing) = std::fs::read_to_string(&path)
            && !existing.contains(HOOK_MARKER)
        {
            refused.push(path);
            continue;
        }
        write_hook(&path, &hook_script(hook))?;
        installed.push(path);
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "hooks_dir": dir,
            "installed": installed,
            "refused": refused,
            "reason": if refused.is_empty() { Value::Null } else {
                json!("a hook already exists and was not written by bd")
            },
        }))?;
    } else {
        for p in &installed {
            ctx.out.line(format!("Installed {}", p.display()));
        }
        for p in &refused {
            ctx.out.warn(format!(
                "{} already exists and beads did not write it — left untouched",
                p.display()
            ));
        }
        if !refused.is_empty() {
            ctx.out.line(
                "\nTo chain beads into a hook you already have, add this line to it:\n  \
                 bd hooks run <hook-name>",
            );
        } else {
            ctx.out
                .line("\n`git commit` now keeps .beads/issues.jsonl in step with the database.");
        }
    }

    if refused.is_empty() {
        Ok(())
    } else {
        // Not a silent partial success: a script that asked for hooks and did not
        // get all of them has to be able to tell.
        Err(SilentExit(exit::FAILURE).into())
    }
}

fn hooks_uninstall(ctx: &Ctx) -> Result<()> {
    ctx.ensure_writable("remove git hooks")?;
    let dir = require_hooks_dir(ctx)?;

    let mut removed: Vec<PathBuf> = Vec::new();
    let mut kept: Vec<PathBuf> = Vec::new();
    for hook in KNOWN_HOOKS {
        let path = dir.join(hook);
        match std::fs::read_to_string(&path) {
            Ok(s) if s.contains(HOOK_MARKER) => {
                std::fs::remove_file(&path)
                    .with_context(|| format!("cannot remove {}", path.display()))?;
                removed.push(path);
            }
            // Not ours. `uninstall` is not a licence to delete.
            Ok(_) => kept.push(path),
            Err(_) => {}
        }
    }

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "hooks_dir": dir,
            "removed": removed,
            "left_alone": kept,
        }))?;
    } else {
        for p in &removed {
            ctx.out.line(format!("Removed {}", p.display()));
        }
        for p in &kept {
            ctx.out.line(format!(
                "Left {} alone — beads did not write it",
                p.display()
            ));
        }
        if removed.is_empty() && kept.is_empty() {
            ctx.out.line("No beads hooks are installed.");
        }
    }
    Ok(())
}

fn hooks_list(ctx: &Ctx) -> Result<()> {
    let dir = require_hooks_dir(ctx)?;
    let rows: Vec<(&str, &'static str)> = KNOWN_HOOKS
        .iter()
        .map(|h| {
            let state = match std::fs::read_to_string(dir.join(h)) {
                Ok(s) if s.contains(HOOK_MARKER) => "beads",
                Ok(_) => "foreign",
                Err(_) => "absent",
            };
            (*h, state)
        })
        .collect();

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "hooks_dir": dir,
            "hooks": rows.iter().map(|(h, s)| json!({ "hook": h, "state": s })).collect::<Vec<_>>(),
        }));
    }
    ctx.out.line(format!("{}\n", dir.display()));
    for (h, state) in rows {
        let note = match state {
            "beads" => "installed by bd",
            "foreign" => "exists, not written by bd — bd will not touch it",
            _ => "not installed",
        };
        ctx.out.line(format!("  {h:<12} {state:<8} {note}"));
    }
    Ok(())
}

async fn hooks_run(ctx: &Ctx, hook: &str) -> Result<()> {
    match hook {
        "pre-commit" => run_pre_commit(ctx).await,
        "post-merge" => run_post_merge(ctx).await,
        other => bail!(
            "bd has no {other} hook (it knows: {})",
            KNOWN_HOOKS.join(", ")
        ),
    }
}

/// Export the database to JSONL and stage it, so the commit carries the issues.
///
/// It calls the real exporter rather than serializing here. A second, hook-only
/// writer would drift from `bd export`, and the drift would surface as a corrupt
/// diff in someone's git history months later.
async fn run_pre_commit(ctx: &Ctx) -> Result<()> {
    let path = ctx.locator()?.dir.join(JSONL);
    crate::commands::sync::export(
        ctx,
        ExportArgs {
            output: Some(path.clone()),
            open_only: false,
        },
    )
    .await?;
    git_add(&ctx.cwd, &path)?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "hook": "pre-commit",
            "exported": path,
            "staged": true,
        }))?;
    } else {
        ctx.out.line(format!("Staged {}", path.display()));
    }
    Ok(())
}

/// The other half: a merge or a pull lands someone else's issues as text, and
/// they have to get back into the database or `bd ready` is answering from a
/// stale copy.
async fn run_post_merge(ctx: &Ctx) -> Result<()> {
    let path = ctx.locator()?.dir.join(JSONL);
    if !path.exists() {
        // A repo that never ran the pre-commit hook has no JSONL. Nothing to do
        // is not a failure — and a hook that fails on a clean pull is a hook
        // people delete.
        ctx.out
            .detail(format!("{} does not exist; nothing to import", path.display()));
        if ctx.out.is_json() {
            ctx.out
                .json_value(&json!({ "hook": "post-merge", "imported": false }))?;
        }
        return Ok(());
    }
    crate::commands::sync::import(
        ctx,
        ImportArgs {
            file: Some(path),
            dry_run: false,
        },
    )
    .await
}

fn require_hooks_dir(ctx: &Ctx) -> Result<PathBuf> {
    hooks_dir(&ctx.cwd).ok_or_else(|| {
        anyhow!(
            "no git repository at {} — `bd hooks` has nothing to install into",
            ctx.cwd.display()
        )
    })
}

/// Where git will actually look for hooks.
///
/// Two traps, both silent: in a linked worktree `.git` is a *file*, and
/// `core.hooksPath` can move the hooks directory somewhere else entirely. Write
/// to `.git/hooks` under either and the hook is installed exactly where git will
/// never look. `git rev-parse --git-path hooks` knows about both, so ask it
/// first and only fall back to the layout when git is not on PATH at all.
fn hooks_dir(cwd: &Path) -> Option<PathBuf> {
    if let Some(p) = git_path_hooks(cwd) {
        return Some(p);
    }
    git_dir(cwd).map(|d| d.join("hooks"))
}

fn git_path_hooks(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    // git answers relative to the cwd it was run in.
    Some(if p.is_absolute() { p } else { cwd.join(p) })
}

/// The `.git` directory, resolving a worktree's `.git` *file* to the common dir
/// that actually holds the hooks.
fn git_dir(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(dot);
        }
        if dot.is_file() {
            let raw = std::fs::read_to_string(&dot).ok()?;
            let rel = raw.trim().strip_prefix("gitdir:")?.trim();
            let gitdir = dir.join(rel);
            // A linked worktree's gitdir holds per-worktree state; the hooks live
            // in the shared dir it points at.
            return match std::fs::read_to_string(gitdir.join("commondir")) {
                Ok(c) => Some(gitdir.join(c.trim())),
                Err(_) => Some(gitdir),
            };
        }
        cur = dir.parent();
    }
    None
}

fn git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn git_add(cwd: &Path, path: &Path) -> Result<()> {
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("--")
        .arg(path)
        .current_dir(cwd)
        .output()
        .context("cannot run git")?;
    if !out.status.success() {
        bail!(
            "git add {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn write_hook(path: &Path, script: &str) -> Result<()> {
    std::fs::write(path, script).with_context(|| format!("cannot write {}", path.display()))?;
    // A hook that is not executable is a hook git silently skips — on unix. On
    // Windows git runs it through sh regardless, and there is no mode to set.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot make {} executable", path.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn block() -> String {
        managed_block()
    }

    #[test]
    fn splicing_into_an_empty_file_is_just_the_block() {
        assert_eq!(splice("", &block()).unwrap(), block());
    }

    #[test]
    fn a_file_with_no_markers_keeps_every_byte_and_gains_a_block() {
        let prose = "# My project\n\nAlways run the linter.\n";
        let out = splice(prose, &block()).unwrap();
        assert!(out.starts_with(prose), "the user's prose must survive verbatim");
        assert!(out.ends_with(&block()));
    }

    /// The property the whole design exists for.
    #[test]
    fn splicing_twice_is_the_same_as_splicing_once() {
        let prose = "# My project\n\nAlways run the linter.\n";
        let once = splice(prose, &block()).unwrap();
        let twice = splice(&once, &block()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(twice.matches(BEGIN).count(), 1);
        assert_eq!(twice.matches(END).count(), 1);
    }

    #[test]
    fn an_old_block_is_replaced_not_appended_and_the_prose_around_it_survives() {
        let file = format!(
            "# Mine\n\nAbove.\n\n{BEGIN}\nstale beads text\n{END}\n\nBelow — still mine.\n"
        );
        let out = splice(&file, &block()).unwrap();
        assert!(out.starts_with("# Mine\n\nAbove.\n\n"));
        assert!(out.ends_with("\nBelow — still mine.\n"));
        assert!(!out.contains("stale beads text"));
        assert_eq!(out.matches(BEGIN).count(), 1);
    }

    #[test]
    fn a_file_with_no_trailing_newline_is_not_run_into_the_block() {
        let out = splice("no newline here", &block()).unwrap();
        assert!(out.starts_with("no newline here\n\n"));
        assert_eq!(out.matches(BEGIN).count(), 1);
        // And it settles: a second pass changes nothing.
        assert_eq!(splice(&out, &block()).unwrap(), out);
    }

    #[test]
    fn crlf_markers_still_match_rather_than_producing_a_second_block() {
        let file = format!("# Mine\r\n\r\n{BEGIN}\r\nSTALE-BEADS-TEXT\r\n{END}\r\n");
        let out = splice(&file, &block()).unwrap();
        assert_eq!(out.matches(BEGIN).count(), 1, "CRLF markers must be found");
        assert!(out.starts_with("# Mine\r\n\r\n"));
        // A sentinel that cannot occur in the block itself: a short word like
        // "old" is a substring of "hold", and the assertion would pass on a bug.
        assert!(!out.contains("STALE-BEADS-TEXT"));
    }

    /// Half a pair of markers means someone hand-edited the file and we cannot
    /// know where their text ends. Guessing would eat it.
    #[test]
    fn unbalanced_markers_are_refused_rather_than_guessed_at() {
        let orphan_begin = format!("# Mine\n\n{BEGIN}\nsomething\n");
        assert!(splice(&orphan_begin, &block()).is_err());

        let orphan_end = format!("# Mine\n\nsomething\n{END}\n");
        assert!(splice(&orphan_end, &block()).is_err());
    }

    #[test]
    fn recipes_map_the_harnesses_onto_the_two_files_that_exist() {
        assert_eq!(Recipe::parse("claude").unwrap().file(), "CLAUDE.md");
        assert_eq!(Recipe::parse("CLAUDE").unwrap().file(), "CLAUDE.md");
        assert_eq!(Recipe::parse("codex").unwrap().file(), "AGENTS.md");
        assert_eq!(Recipe::parse("factory").unwrap().file(), "AGENTS.md");
        assert!(Recipe::parse("emacs").is_err());
    }

    #[test]
    fn two_recipes_naming_one_file_write_it_once() {
        let dir = std::env::temp_dir().join(format!("bd-setup-dedup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // codex and factory are both AGENTS.md; applying both must not splice twice.
        let applied = apply_recipes(&dir, &[Recipe::Agents, Recipe::Agents]).unwrap();
        assert_eq!(applied.len(), 1);
        let text = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert_eq!(text.matches(BEGIN).count(), 1);

        // And a second run is a no-op, not a second block.
        let again = apply_recipes(&dir, &[Recipe::Agents]).unwrap();
        assert_eq!(again[0].action, Action::Unchanged);
        assert_eq!(std::fs::read_to_string(dir.join("AGENTS.md")).unwrap(), text);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detection_prefers_the_file_that_is_already_there() {
        let dir = std::env::temp_dir().join(format!("bd-setup-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing to go on: AGENTS.md, the cross-harness convention.
        assert_eq!(detect_recipes(&dir), vec![Recipe::Agents]);

        std::fs::write(dir.join("CLAUDE.md"), "# hi\n").unwrap();
        assert_eq!(detect_recipes(&dir), vec![Recipe::Claude]);

        std::fs::write(dir.join("AGENTS.md"), "# hi\n").unwrap();
        assert_eq!(detect_recipes(&dir), vec![Recipe::Claude, Recipe::Agents]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_managed_block_is_a_constant_so_setup_can_be_idempotent() {
        // If anything about the workspace leaked into the block, `bd setup` would
        // rewrite the file on every run and show up in every diff.
        assert_eq!(managed_block(), managed_block());
        assert!(managed_block().starts_with(BEGIN));
        assert!(managed_block().ends_with("-->\n"));
    }

    #[test]
    fn the_installed_hook_carries_the_marker_that_protects_foreign_hooks() {
        for h in KNOWN_HOOKS {
            let s = hook_script(h);
            assert!(s.contains(HOOK_MARKER), "{h} must be identifiable as ours");
            assert!(s.contains(&format!("bd hooks run {h}")));
        }
    }
}
