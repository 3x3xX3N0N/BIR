//! Integrations — the coding agents that drive beads.
//!
//! Claude Code, Cursor, Codex, Gemini, Aider, Junie, OpenCode, and friends. Each
//! integrates through some combination of a settings file, a hook, and a
//! documentation file telling the agent how to use `bd`.
//!
//! **Absence is not a failure.** A user who does not use Cursor has no Cursor
//! problem. Report [`Finding::ok`] ("not configured") — or say nothing at all —
//! but never a warning. The signal-to-noise ratio of this family decides whether
//! anybody reads `bd doctor` output at all, and nine warnings about editors
//! somebody doesn't use will train them to skim past the one that matters.
//!
//! Warn only when an integration is **present but broken**: a hook installed
//! that points at a `bd` that isn't there, a settings file that references a
//! removed command, agent documentation that has drifted out of sync with the
//! CLI and is now actively instructing the agent to run commands that fail.
//!
//! Belongs here: per-agent presence and health, hook completeness, settings
//! validity, plugin installation, documentation drift.
//!
//! # How this file stays quiet
//!
//! Four checks, and a user with no agent integration at all sees **one line** —
//! the collapsed `ok  Integrations (4 checks)` the printer emits for a category
//! with nothing to say. There is no per-agent check, because a per-agent check is
//! a per-agent line, and nine green lines about editors you do not use are how a
//! diagnostic teaches you to stop reading it. Presence is rolled into a single
//! inventory finding that can only ever be `Ok`.
//!
//! Nothing here warns because something is *missing*. Each check first asks "is
//! this integration here at all?", and answers `Ok` if it is not. The warnings
//! are reserved for the three ways a *present* integration silently lies:
//!
//! * a hook whose command this `bd` does not have (it fails on every session
//!   start, forever, and the human never sees the error),
//! * a settings file that will not parse (the agent loads *no* hooks and says
//!   nothing),
//! * documentation that has drifted from the CLI, so the agent is being told to
//!   run commands that do not exist — it will keep trying, keep failing, and
//!   nobody will ever look.
//!
//! The last one is the reason this file is interesting. [`cli::build`] hands us
//! the real clap tree, so "does `bd cursor-hook` exist?" is not a guess: we ask
//! the parser the agent would actually hit. That check cannot be written upstream
//! and it is the one most likely to catch a genuine, invisible fault.
//!
//! # What is deliberately *not* here
//!
//! * **Git hooks.** `.git/hooks/pre-commit` is the Git family's (`git.rs`).
//! * **`bd` not being on `$PATH`.** A hook that runs a bare `bd` when there is no
//!   `bd` on `PATH` is broken — but the Runtime family already says so, for
//!   *everybody* rather than only for people with hooks, and its message names
//!   agent integrations explicitly. Repeating it here would be the ninth warning
//!   that teaches you to stop reading the first eight. A hook naming an *absolute*
//!   path to a `bd` that is gone is nobody else's finding, and is reported.
//! * **Scanning the home directory.** Rule 5: a check runs from a git hook. We
//!   `stat` a handful of exact paths under `$HOME`; we never walk it. And a
//!   home-level file only ever produces a finding when the *beads* part of it is
//!   broken — somebody else's global editor config is not this workspace's fault
//!   and must not be warned about once per repository.
//!
//! [`cli::build`]: crate::cli::build

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;

use super::super::{Category, Check, Dx, Finding};

pub fn checks() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(Inventory),
        Box::new(Hooks::new(home_dir())),
        Box::new(DocDrift),
        Box::new(DocMarkers),
    ]
}

// ---------------------------------------------------------------------------
// The agents we know about
// ---------------------------------------------------------------------------

/// One coding agent, and the files that betray it.
struct Agent {
    name: &'static str,
    /// Project-relative. Any one of these existing means the harness is in use
    /// here — *not* that beads is wired into it.
    markers: &'static [&'static str],
    /// Project-relative. Files that would carry beads wiring if there were any:
    /// instructions, rules, hook config. Beads is "wired in" when one of these
    /// exists *and* mentions beads.
    wiring: &'static [&'static str],
}

/// Deliberately a table and not nine checks. Adding an agent here costs zero
/// lines of output for everybody who does not use it.
const AGENTS: &[Agent] = &[
    Agent {
        name: "claude",
        markers: &[".claude", "CLAUDE.md"],
        wiring: &[
            "CLAUDE.md",
            ".claude/CLAUDE.md",
            ".claude/settings.json",
            ".claude/settings.local.json",
        ],
    },
    Agent {
        name: "cursor",
        markers: &[".cursor", ".cursorrules"],
        wiring: &[".cursorrules", ".cursor/hooks.json", ".cursor/rules/beads.mdc"],
    },
    Agent {
        name: "codex",
        markers: &[".codex", "AGENTS.md"],
        wiring: &["AGENTS.md", ".codex/config.toml", ".codex/hooks.json"],
    },
    Agent {
        name: "gemini",
        markers: &[".gemini", "GEMINI.md"],
        wiring: &["GEMINI.md", ".gemini/settings.json"],
    },
    Agent {
        name: "aider",
        markers: &[".aider", ".aider.conf.yml"],
        wiring: &[".aider/BEADS.md", ".aider.conf.yml"],
    },
    Agent {
        name: "junie",
        markers: &[".junie"],
        wiring: &[".junie/guidelines.md", ".junie/mcp/mcp.json"],
    },
    Agent {
        name: "opencode",
        markers: &[".opencode", "opencode.json"],
        wiring: &["opencode.json", ".opencode/opencode.json", "AGENTS.md"],
    },
    Agent {
        name: "copilot",
        markers: &[".github/copilot-instructions.md"],
        wiring: &[".github/copilot-instructions.md"],
    },
    Agent {
        name: "factory",
        markers: &[".factory"],
        wiring: &[".factory/AGENTS.md", "AGENTS.md"],
    },
    Agent {
        name: "mux",
        markers: &[".mux"],
        wiring: &[".mux/AGENTS.md"],
    },
];

/// Files an agent reads its standing instructions from. The set `bd setup`
/// writes (`CLAUDE.md`, `AGENTS.md`) plus the conventions the other harnesses
/// use. A fixed list, not a glob: this runs in a git hook.
const DOC_FILES: &[&str] = &[
    "CLAUDE.md",
    "CLAUDE.local.md",
    "claude.local.md",
    ".claude/CLAUDE.md",
    "AGENTS.md",
    "AGENT.md",
    "GEMINI.md",
    ".cursorrules",
    ".cursor/rules/beads.mdc",
    ".github/copilot-instructions.md",
    ".junie/guidelines.md",
    ".aider/BEADS.md",
    ".windsurfrules",
    ".mux/AGENTS.md",
];

/// Files that can carry an executable hook command. Project-relative.
const HOOK_FILES: &[&str] = &[
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".cursor/hooks.json",
    ".codex/hooks.json",
    ".gemini/settings.json",
    ".junie/mcp/mcp.json",
    ".opencode/opencode.json",
    "opencode.json",
];

/// The same, under `$HOME`. Exactly four `stat`s, and a problem found in one of
/// them is only ever reported when it is *beads'* problem (see the module docs).
const HOME_HOOK_FILES: &[&str] = &[
    ".claude/settings.json",
    ".cursor/hooks.json",
    ".codex/hooks.json",
    ".gemini/settings.json",
];

/// The markers `bd setup` wraps its managed block in.
///
/// Duplicated from `commands::setup`, whose copy is private. The duplication is
/// load-bearing enough to be tested rather than trusted: `doctor_agents.rs` runs
/// the real `bd onboard` and asserts its output still carries both of these, so
/// if setup ever renames a marker, this file fails the build rather than
/// silently going blind. (The right fix is for `setup` to expose them — see the
/// hand-back note in the report.)
const BEGIN: &str = "<!-- BEGIN BEADS -->";
const END: &str = "<!-- END BEADS -->";

/// Nobody's agent instructions are two megabytes. A file this big is not agent
/// documentation, and scanning it would blow the budget a git hook has.
const MAX_DOC: u64 = 2 * 1024 * 1024;

// ---------------------------------------------------------------------------
// 1. Inventory — what is configured. Never a warning.
// ---------------------------------------------------------------------------

/// Which agent harnesses are in use here, and which of them have beads wired in.
///
/// This finding is **`Ok` by construction**. An agent you do not use is not a
/// fault; an agent you use but chose not to wire beads into is not a fault
/// either. The value is the inventory itself — it lands in `--json`, where an
/// agent can read it — and the guarantee that the human sees nothing.
struct Inventory;

#[async_trait]
impl Check for Inventory {
    fn name(&self) -> &'static str {
        "agent-integrations"
    }

    fn category(&self) -> Category {
        Category::Integration
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let root = project_root(dx);
        let mut files = Files::default();

        let mut in_use: Vec<&str> = Vec::new();
        let mut wired: Vec<&str> = Vec::new();

        for agent in AGENTS {
            let present = agent.markers.iter().any(|m| root.join(m).exists());
            let beads = agent
                .wiring
                .iter()
                .any(|w| files.text(&root.join(w)).is_some_and(mentions_beads));
            if present || beads {
                in_use.push(agent.name);
            }
            if beads {
                wired.push(agent.name);
            }
        }

        let name = self.name();
        if in_use.is_empty() {
            return Finding::ok(name, "no coding agent is configured in this project")
                .fix("`bd setup` writes the beads workflow into CLAUDE.md / AGENTS.md");
        }

        let unwired: Vec<&str> = in_use
            .iter()
            .copied()
            .filter(|a| !wired.contains(a))
            .collect();

        let message = if wired.is_empty() {
            format!(
                "{} agent harness(es) here, beads is wired into none of them",
                in_use.len()
            )
        } else {
            format!(
                "beads is wired into {} of {} agent harness(es) here",
                wired.len(),
                in_use.len()
            )
        };

        let mut detail = String::new();
        if !wired.is_empty() {
            detail.push_str(&format!("wired: {}", wired.join(", ")));
        }
        if !unwired.is_empty() {
            if !detail.is_empty() {
                detail.push('\n');
            }
            detail.push_str(&format!("in use, not wired: {}", unwired.join(", ")));
        }

        // `Ok`, with a `fix` — which the human printer never shows for an `Ok`
        // finding, and `--json` carries anyway. That asymmetry is the point: the
        // nudge is available to anything that goes looking for it, and invisible
        // to the person who did not ask.
        let mut f = Finding::ok(name, message).detail(detail);
        if !unwired.is_empty() {
            f = f.fix("`bd setup` wires beads into the harness this repo already uses");
        }
        f
    }
}

// ---------------------------------------------------------------------------
// 2. Hooks — a hook that is installed and cannot run
// ---------------------------------------------------------------------------

/// The agent hooks that are installed, and whether they would actually run.
///
/// A hook is the one integration that fails *invisibly*. Nobody watches a
/// SessionStart hook: it either injected the board into the context window or it
/// did not, and the failure looks exactly like an agent that did not bother. So
/// the three ways it dies quietly all warn here — a command this `bd` does not
/// have, a `bd` at a path that no longer exists, and a settings file that will
/// not parse (which disables *every* hook in it, not just ours).
pub struct Hooks {
    home: Option<PathBuf>,
}

impl Hooks {
    /// `home` is where to look for user-level agent settings, or `None` to look
    /// only inside the project.
    ///
    /// It is a parameter rather than an `std::env` lookup for one reason: a check
    /// whose answer depends on whatever happens to be in the developer's own
    /// `~/.claude` is a check that cannot be tested, and an untestable check is
    /// how a diagnostic starts quietly lying. The tests inject a home directory
    /// they built; the registry passes the real one.
    pub fn new(home: Option<PathBuf>) -> Hooks {
        Hooks { home }
    }
}

#[async_trait]
impl Check for Hooks {
    fn name(&self) -> &'static str {
        "agent-hooks"
    }

    fn category(&self) -> Category {
        Category::Integration
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let name = self.name();
        let root = project_root(dx);
        let cli = crate::cli::build();
        let known = known_commands(&cli);

        let mut seen = 0usize; // hook files that exist
        let mut hooks = 0usize; // beads hook commands found in them
        let mut unparsable: Vec<String> = Vec::new();
        let mut broken: Vec<String> = Vec::new();

        let mut targets: Vec<(String, PathBuf, bool)> = HOOK_FILES
            .iter()
            .map(|f| ((*f).to_string(), root.join(f), true))
            .collect();
        if let Some(home) = &self.home {
            for f in HOME_HOOK_FILES {
                targets.push((format!("~/{f}"), home.join(f), false));
            }
        }

        for (label, path, ours) in targets {
            let text = match slurp(&path) {
                Slurp::Absent => continue,
                Slurp::Text(t) => t,
                // Unreadable or absurd. In the project that is worth saying; in
                // the home directory it is somebody else's business.
                Slurp::Bad(why) => {
                    if ours {
                        unparsable.push(format!("{label}: {why}"));
                    }
                    continue;
                }
            };
            seen += 1;

            let doc: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    // A malformed config is not just "untidy": the agent loads
                    // *no* hooks from it and reports nothing. We also cannot tell
                    // whether beads is in there — which is `Warn` by definition
                    // (see the seam docs: undeterminable is never `Ok`).
                    //
                    // Outside the project we only say so if the file at least
                    // mentions beads, because a broken global editor config is
                    // not this workspace's fault and must not be warned about
                    // once per repository.
                    if ours || mentions_beads(&text) {
                        unparsable.push(format!("{label}: not valid JSON ({e})"));
                    }
                    continue;
                }
            };

            for (site, cmd) in commands_in(&doc) {
                let Some(argv) = bd_argv(&cmd) else { continue };
                hooks += 1;
                let at = format!("{label} ({site}): `{cmd}`");

                // The program itself. A hook that hard-codes /old/path/bd is the
                // classic: it worked perfectly right up until the binary moved,
                // and it has been failing silently ever since.
                //
                // A *bare* `bd` that is not on PATH is the same failure — but it
                // is the Runtime family's to report, and it reports it for
                // everybody rather than only for people with hooks. Saying it
                // again here would be the ninth warning that teaches you to stop
                // reading the first eight.
                let prog = argv.program;
                if is_path(prog) && !Path::new(prog).is_file() {
                    broken.push(format!("{at}\n  {prog} does not exist"));
                    continue;
                }

                if let Some(why) = fault(&cli, &known, &argv, Strict::Hook) {
                    broken.push(format!("{at}\n  {why}"));
                }
            }
        }

        if seen == 0 {
            return Finding::ok(name, "no agent hooks are installed");
        }
        if hooks == 0 && unparsable.is_empty() {
            return Finding::ok(
                name,
                format!("{seen} agent config file(s), none of them carrying a beads hook"),
            );
        }

        if broken.is_empty() && unparsable.is_empty() {
            return Finding::ok(name, format!("{hooks} beads hook(s) installed and runnable"));
        }

        let mut detail: Vec<String> = Vec::new();
        detail.extend(unparsable.iter().cloned());
        detail.extend(broken.iter().cloned());

        let bad = broken.len();
        let message = match (bad, unparsable.len()) {
            (0, n) => format!("{n} agent config file(s) will not parse"),
            (n, 0) => format!("{n} installed agent hook(s) would fail every time they run"),
            (n, m) => format!("{n} agent hook(s) would fail; {m} config file(s) will not parse"),
        };

        Finding::warn(name, message)
            .detail(detail.join("\n"))
            .fix("fix the command in the settings file, or re-run the setup that installed it")
    }
}

// ---------------------------------------------------------------------------
// 3. Documentation drift — the docs tell the agent to run a command that is gone
// ---------------------------------------------------------------------------

/// Agent documentation that has drifted out of sync with this CLI.
///
/// The nastiest failure in the family, because it has no symptom. `CLAUDE.md`
/// says "run `bd cursor-hook`"; `bd` has no such command; the agent runs it, gets
/// a usage error, works around it, and the human never sees any of it. It just
/// looks like the agent is bad at using beads.
///
/// So we ask clap. [`crate::cli::build`] is the same tree the agent's shell will
/// hit, which makes this the one check in the family that can be *certain*: not
/// "this looks unusual" but "this command does not exist".
///
/// Only code — fenced blocks and backticked spans — is scanned. Prose is not:
/// "bd tracks work as a graph" must never be read as a reference to a `bd tracks`
/// command, and a false positive here poisons the whole family.
struct DocDrift;

#[async_trait]
impl Check for DocDrift {
    fn name(&self) -> &'static str {
        "agent-docs-drift"
    }

    fn category(&self) -> Category {
        Category::Integration
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let name = self.name();
        let cli = crate::cli::build();
        let known = known_commands(&cli);

        let mut read = 0usize;
        let mut unreadable: Vec<String> = Vec::new();
        // (file, line, command) -> why. Ordered, and deduped: one stale command
        // repeated eight times in one file is one thing to fix, not eight.
        let mut drift: BTreeMap<(String, String), (usize, String)> = BTreeMap::new();

        for (label, path) in doc_files(&project_root(dx)) {
            let text = match slurp(&path) {
                Slurp::Absent => continue,
                Slurp::Bad(why) => {
                    unreadable.push(format!("{label}: {why}"));
                    continue;
                }
                Slurp::Text(t) => t,
            };
            read += 1;

            for (line, code) in code_spans(&text) {
                for argv in bd_invocations(&code) {
                    if let Some(why) = fault(&cli, &known, &argv, Strict::Doc) {
                        drift
                            .entry((label.clone(), argv.render()))
                            .or_insert((line, why));
                    }
                }
            }
        }

        // Absence is not failure. No agent docs is a legitimate way to use beads.
        if read == 0 && unreadable.is_empty() {
            return Finding::ok(name, "no agent documentation in this project");
        }
        if !unreadable.is_empty() {
            return Finding::unknown(name, unreadable.join("\n"));
        }
        if drift.is_empty() {
            return Finding::ok(
                name,
                format!("{read} agent doc file(s), every `bd` command in them exists"),
            );
        }

        let mut detail: Vec<String> = drift
            .iter()
            .take(12)
            .map(|((file, cmd), (line, why))| format!("{file}:{line}  `{cmd}` — {why}"))
            .collect();
        if drift.len() > 12 {
            detail.push(format!("… and {} more", drift.len() - 12));
        }

        Finding::warn(
            name,
            format!(
                "{} command(s) in your agent docs do not exist in this bd",
                drift.len()
            ),
        )
        .detail(detail.join("\n"))
        .fix("your agent is being told to run these; correct the docs (`bd setup` refreshes the managed block)")
    }
}

// ---------------------------------------------------------------------------
// 4. The managed block — `bd setup` has locked itself out
// ---------------------------------------------------------------------------

/// The `<!-- BEGIN BEADS -->` block in an agent doc file, and whether `bd setup`
/// can still update it.
///
/// `setup` refuses to touch a file whose markers are unbalanced — correctly: it
/// cannot tell where the user's prose ends and its own block began, and any guess
/// it made would be a guess about which of their paragraphs to delete. But the
/// refusal is *quiet*, and its consequence is permanent: that file's beads
/// section will never be updated again, and it will silently rot into the drift
/// the check above is looking for.
struct DocMarkers;

#[async_trait]
impl Check for DocMarkers {
    fn name(&self) -> &'static str {
        "agent-docs-markers"
    }

    fn category(&self) -> Category {
        Category::Integration
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let name = self.name();
        let mut blocks = 0usize;
        let mut bad: Vec<String> = Vec::new();

        for (label, path) in doc_files(&project_root(dx)) {
            let Slurp::Text(text) = slurp(&path) else {
                // Unreadable files are the drift check's finding to report; two
                // checks shouting about one file is the noise this family exists
                // to avoid.
                continue;
            };
            let begins = text.matches(BEGIN).count();
            let ends = text.matches(END).count();

            match (begins, ends) {
                (0, 0) => {}
                (1, 1) => blocks += 1,
                (b, 0) if b > 0 => bad.push(format!(
                    "{label}: `{BEGIN}` with no `{END}` — `bd setup` cannot tell where \
                     its block ends and will refuse to update this file"
                )),
                (0, e) if e > 0 => bad.push(format!(
                    "{label}: `{END}` with no `{BEGIN}` — `bd setup` cannot tell where \
                     its block begins and will refuse to update this file"
                )),
                (b, e) => bad.push(format!(
                    "{label}: {b} beads blocks and {e} end markers — the agent reads the \
                     beads section more than once, and `bd setup` will only refresh the first"
                )),
            }
        }

        if bad.is_empty() {
            return match blocks {
                0 => Finding::ok(name, "no beads block in the agent docs"),
                n => Finding::ok(name, format!("{n} beads block(s), well-formed")),
            };
        }

        Finding::warn(
            name,
            format!("{} agent doc file(s) `bd setup` can no longer update", bad.len()),
        )
        .detail(bad.join("\n"))
        .fix("balance the markers by hand (bd will not guess), then re-run `bd setup`")
    }
}

// ---------------------------------------------------------------------------
// Asking clap whether a command exists
// ---------------------------------------------------------------------------

/// A `bd …` invocation, split out of some text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Argv<'a> {
    /// How `bd` was named: `bd`, `bd.exe`, `/usr/local/bin/bd`, …
    program: &'a str,
    args: Vec<&'a str>,
}

impl Argv<'_> {
    fn render(&self) -> String {
        let mut s = String::from("bd");
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }

    /// Can this be handed to clap verbatim? Only if no token is shell syntax, a
    /// quoted string, or a `<placeholder>` — anything we would have to *guess*
    /// the expansion of, we do not full-parse, and fall back to checking the
    /// subcommand name alone.
    fn clean(&self) -> bool {
        self.args.iter().all(|t| {
            !t.is_empty()
                && !t.chars().any(|c| {
                    matches!(
                        c,
                        '<' | '>' | '$' | '"' | '\'' | '`' | '\\' | '{' | '}' | '(' | ')' | '*'
                            | '?' | '[' | ']' | '~' | '%'
                    )
                })
        })
    }
}

/// How harshly to judge. A doc that says "run `bd hooks`" is *fine* — it is
/// naming a command, not running one. A settings file that says `bd hooks` is
/// broken: that is the literal argv, and it exits with a usage error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strict {
    Doc,
    Hook,
}

/// What is wrong with this invocation, or `None` if this `bd` would run it.
///
/// Deliberately biased towards silence. A false positive in this family is worse
/// than a miss: it is the ninth warning about an editor you do not use, and it is
/// how the one warning that mattered stopped being read.
fn fault(cli: &clap::Command, known: &BTreeSet<String>, argv: &Argv<'_>, how: Strict) -> Option<String> {
    // The subcommand. Scan *all* the tokens rather than taking the first one:
    // `bd --actor agent-7 ready` is a real thing to write, and taking the first
    // non-flag token would accuse `agent-7` of not being a command.
    if !argv.args.iter().any(|t| known.contains(*t)) {
        let candidate = argv
            .args
            .iter()
            .find(|t| plausible_command(t))
            .copied()?; // no candidate at all (`bd --help`) — nothing to say
        return Some(format!("this bd has no `{candidate}` command"));
    }

    if !argv.clean() {
        // Placeholders and quoting. The subcommand exists, and we will not guess
        // at what `<id>` expands to.
        return None;
    }

    let mut full: Vec<&str> = Vec::with_capacity(argv.args.len() + 1);
    full.push("bd");
    full.extend(argv.args.iter().copied());

    let err = cli.clone().try_get_matches_from(full).err()?;
    use clap::error::ErrorKind as K;
    let report = match err.kind() {
        K::InvalidSubcommand | K::UnknownArgument | K::InvalidValue | K::ValueValidation => true,
        // A doc naming a command without its arguments is documentation, not a
        // fault: "see `bd hooks`" is a sentence. A *hook* whose command is `bd
        // hooks` is a hook that prints usage and exits nonzero, every time.
        K::MissingRequiredArgument
        | K::MissingSubcommand
        | K::DisplayHelpOnMissingArgumentOrSubcommand
        | K::TooManyValues
        | K::TooFewValues
        | K::WrongNumberOfValues
        | K::ArgumentConflict => how == Strict::Hook,
        _ => false,
    };
    if !report {
        return None;
    }

    // clap renders "you gave me no subcommand" as the entire help text, which is
    // not a sentence anyone wants in a diagnostic.
    if matches!(
        err.kind(),
        K::MissingSubcommand | K::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        return Some("needs a subcommand: as written this prints help and exits nonzero".to_string());
    }

    // Otherwise clap's first line is exactly the sentence we want; the rest is
    // usage and a colour reset.
    let rendered = err.render().to_string();
    let first = rendered
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("this bd would reject it")
        .trim()
        .trim_start_matches("error: ")
        .to_string();
    Some(first)
}

/// Every subcommand name this `bd` answers to, aliases included.
fn known_commands(cli: &clap::Command) -> BTreeSet<String> {
    let mut s = BTreeSet::new();
    for sub in cli.get_subcommands() {
        s.insert(sub.get_name().to_string());
        for a in sub.get_all_aliases() {
            s.insert(a.to_string());
        }
    }
    s
}

/// Could this token be a subcommand name at all? Rejects `<id>`, `--json`,
/// `"a title"`, `$ID`, `SessionStart` — everything that is obviously not a
/// command, so that we never accuse one of being a missing one.
fn plausible_command(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 32
        && t.starts_with(|c: char| c.is_ascii_lowercase())
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Was `bd` named by path rather than by name?
fn is_path(prog: &str) -> bool {
    prog.contains('/') || prog.contains('\\')
}

/// Is `bd` on this token: `bd`, `bd.exe`, or a path ending in either.
fn is_bd(t: &str) -> bool {
    let base = t.rsplit(['/', '\\']).next().unwrap_or(t);
    base.eq_ignore_ascii_case("bd") || base.eq_ignore_ascii_case("bd.exe")
}

/// Every `bd …` invocation in a chunk of text.
///
/// Handles the shapes that actually appear in documentation and hook config:
/// `$ bd ready`, `bd ready | jq`, `bd export && git add …`. An invocation ends at
/// a shell separator, because `git add` is not a `bd` argument.
fn bd_invocations(text: &str) -> Vec<Argv<'_>> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if !is_bd(tokens[i]) {
            i += 1;
            continue;
        }
        let program = tokens[i];
        let mut args: Vec<&str> = Vec::new();
        i += 1;
        while i < tokens.len() {
            let t = tokens[i];
            if matches!(t, "|" | "||" | "&&" | ";" | "&" | ">" | ">>" | "<" | "#") || is_bd(t) {
                break;
            }
            // A trailing separator glued to the token: `bd ready|jq`.
            if let Some(cut) = t.find(['|', ';', '&', '>']) {
                if cut > 0 {
                    args.push(&t[..cut]);
                }
                i += 1;
                break;
            }
            args.push(t);
            i += 1;
        }
        out.push(Argv { program, args });
    }
    out
}

/// The single `bd …` invocation in a hook's `command` string, if it is one.
fn bd_argv(cmd: &str) -> Option<Argv<'_>> {
    let argv = bd_invocations(cmd).into_iter().next()?;
    // `sh -c "... bd prime ..."` still counts — it is still a bd invocation that
    // will fail — but a command that merely *mentions* bd in a path (say
    // `/opt/bdtools/run`) does not, and `is_bd` already rejects that.
    Some(argv)
}

// ---------------------------------------------------------------------------
// Reading markdown without believing prose
// ---------------------------------------------------------------------------

/// The code in a markdown file: fenced blocks, and backticked spans. With line
/// numbers, because "your docs are wrong" is not actionable and
/// "CLAUDE.md:42 is wrong" is.
///
/// Prose is *not* returned, and that is the whole point. `bd doctor` reading
/// "beads decides what is ready from the graph" and reporting that `bd decides`
/// is not a command would be exactly the kind of confident nonsense that gets a
/// diagnostic ignored.
fn code_spans(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fenced = false;

    for (n, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.push((n + 1, line.to_string()));
            continue;
        }
        // Inline spans: the odd-indexed pieces between backticks.
        if line.contains('`') {
            for (i, piece) in line.split('`').enumerate() {
                if i % 2 == 1 && !piece.trim().is_empty() {
                    out.push((n + 1, piece.to_string()));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Filesystem, carefully
// ---------------------------------------------------------------------------

enum Slurp {
    Absent,
    Text(String),
    /// Present, and we could not read it. Never silently an `Absent`: "I could
    /// not look" is not "there is nothing there" (seam rule 2).
    Bad(String),
}

fn slurp(path: &Path) -> Slurp {
    match std::fs::metadata(path) {
        Err(_) => return Slurp::Absent,
        Ok(m) if m.is_dir() => return Slurp::Absent,
        Ok(m) if m.len() > MAX_DOC => {
            return Slurp::Bad(format!("{} bytes — too large to scan", m.len()));
        }
        Ok(_) => {}
    }
    match std::fs::read_to_string(path) {
        Ok(t) => Slurp::Text(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Slurp::Absent,
        Err(e) => Slurp::Bad(e.to_string()),
    }
}

/// A tiny cache, so that `AGENTS.md` is not read three times because codex,
/// opencode and factory all read it.
#[derive(Default)]
struct Files {
    seen: BTreeMap<PathBuf, Option<String>>,
}

impl Files {
    fn text(&mut self, path: &Path) -> Option<&str> {
        self.seen
            .entry(path.to_path_buf())
            .or_insert_with(|| match slurp(path) {
                Slurp::Text(t) => Some(t),
                _ => None,
            })
            .as_deref()
    }
}

/// Does this file wire beads into anything?
///
/// Note the scrub. In a markdown file `bd` almost always arrives as `` `bd` `` —
/// glued to a backtick — and a naive token scan sees "\`bd" and says no. That is
/// the whole difference between "beads is wired into Claude here" and a silent,
/// wrong "not configured".
fn mentions_beads(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("beads") {
        return true;
    }
    let scrubbed: String = lower
        .chars()
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() => c,
            '-' | '_' | '.' | '/' | '\\' => c,
            _ => ' ',
        })
        .collect();
    scrubbed.split_whitespace().any(is_bd)
}

/// The agent doc files that exist, deduplicated by identity.
///
/// The dedup is not decoration: on Windows and macOS the filesystem is
/// case-insensitive, so `claude.local.md` and `CLAUDE.local.md` are the same file
/// and would otherwise be reported as two.
fn doc_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out = Vec::new();
    for rel in DOC_FILES {
        let path = root.join(rel);
        if !path.is_file() {
            continue;
        }
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            out.push(((*rel).to_string(), path));
        }
    }
    out
}

/// Where the project's agent files live: beside `.beads/`, else the repo root,
/// else where we are standing. The same rule `commands::setup` uses to decide
/// where to *write* them — a doctor that looked somewhere else would report on
/// files `bd setup` does not manage.
fn project_root(dx: &Dx<'_>) -> PathBuf {
    if let Some(dir) = &dx.dir
        && let Some(parent) = dir.parent()
    {
        return parent.to_path_buf();
    }
    if let Some(root) = &dx.root {
        return root.clone();
    }
    dx.ctx.cwd.clone()
}

fn home_dir() -> Option<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(v) = std::env::var_os(key)
            && !v.is_empty()
        {
            return Some(PathBuf::from(v));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Walking a settings file without knowing its shape
// ---------------------------------------------------------------------------

/// Every `"command": "…"` string in a JSON document, at any depth, with the path
/// it was found at.
///
/// Shape-agnostic on purpose. Claude nests hooks as
/// `hooks.SessionStart[0].hooks[0].command`, Cursor as `hooks.sessionStart[0].command`,
/// and both have changed shape at least once. A walker that only knows one of
/// them reports "no hooks installed" for the other — which is the exact failure
/// this family is not allowed to have: `Ok` that means "I did not look".
fn commands_in(doc: &Value) -> Vec<(String, String)> {
    fn walk(v: &Value, at: &str, out: &mut Vec<(String, String)>) {
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    let path = if at.is_empty() {
                        k.clone()
                    } else {
                        format!("{at}.{k}")
                    };
                    if k == "command"
                        && let Value::String(s) = child
                    {
                        out.push((path.clone(), s.clone()));
                    }
                    walk(child, &path, out);
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    walk(child, &format!("{at}[{i}]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(doc, "", &mut out);
    out
}

// ---------------------------------------------------------------------------
// Tests for the parts that decide whether this family is trusted
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn argv<'a>(cmd: &'a str) -> Argv<'a> {
        bd_invocations(cmd).into_iter().next().expect("an invocation")
    }

    fn judge(cmd: &str, how: Strict) -> Option<String> {
        let cli = crate::cli::build();
        let known = known_commands(&cli);
        fault(&cli, &known, &argv(cmd), how)
    }

    /// The property the whole family rests on. One false positive here — one
    /// warning about a command that is perfectly fine — and every warning in
    /// `bd doctor` is worth a little less.
    #[test]
    fn commands_that_exist_are_never_reported() {
        for cmd in [
            "bd ready --json",
            "bd update <id> --claim",
            "bd create \"<title>\" -t task -p 1",
            "bd q \"Fix the flaky test\"",
            "bd dep add <id> <blocker>",
            "bd close <id> --reason done",
            "bd comment <id> \"<finding>\"",
            "bd blocked",
            "bd status",
            "bd prime",
            "bd doctor --fix",
            "bd hooks run pre-commit",
            // Aliases are commands too.
            "bd new \"a title\"",
            "bd done bd-1",
            "bd stats",
            // A global flag before the subcommand must not be mistaken for one.
            "bd --actor agent-7 ready",
            "bd --json ready",
            // Naming a command without its arguments is documentation.
            "bd hooks",
            "bd dep",
            // Nothing to judge.
            "bd --help",
            "bd",
        ] {
            assert_eq!(judge(cmd, Strict::Doc), None, "false positive on `{cmd}`");
        }
    }

    /// The failure this check exists for: documentation left behind by another
    /// beads (or another version of this one) that names commands this binary
    /// does not have. The agent runs them, they fail, and nobody is told.
    #[test]
    fn commands_that_do_not_exist_are_reported() {
        for cmd in [
            "bd cursor-hook sessionStart",
            "bd claude-hook",
            "bd cleanup --older-than 90",
            "bd setup-claude",
        ] {
            let why = judge(cmd, Strict::Doc).unwrap_or_else(|| panic!("missed `{cmd}`"));
            assert!(why.contains("no"), "unhelpful message for `{cmd}`: {why}");
        }
    }

    /// A subcommand that exists with a flag that does not. `bd prime --stealth`
    /// is what upstream's hooks install, and in this port it is a usage error on
    /// every single session start.
    #[test]
    fn a_real_command_with_a_flag_this_bd_removed_is_reported() {
        assert!(judge("bd prime --stealth --hook-json", Strict::Doc).is_some());
        assert!(judge("bd ready --nonsense", Strict::Doc).is_some());
    }

    /// Docs and hooks are judged differently, on purpose. `bd hooks` in prose is
    /// a reference; `bd hooks` as a hook's command is a usage error every time it
    /// fires.
    #[test]
    fn a_hook_is_judged_more_harshly_than_a_sentence() {
        assert_eq!(judge("bd hooks", Strict::Doc), None);
        assert!(judge("bd hooks", Strict::Hook).is_some());
    }

    /// Prose is not code. This is the guard against the family's worst failure
    /// mode — a confident warning about a command nobody ever wrote.
    #[test]
    fn prose_is_never_mistaken_for_a_command() {
        let doc = "\
# My project

bd tracks work as a graph, and bd decides what is ready. Do not run bd manually.

Run `bd ready` to see what is claimable.

```sh
bd create \"Write the parser\" -p 1
```
";
        let spans = code_spans(doc);
        let found: Vec<String> = spans
            .iter()
            .flat_map(|(_, c)| bd_invocations(c))
            .map(|a| a.render())
            .collect();
        assert_eq!(
            found,
            vec!["bd ready", "bd create \"Write the parser\" -p 1"],
            "only code should be scanned; prose must be invisible"
        );
    }

    #[test]
    fn a_fenced_block_and_an_inline_span_are_both_code() {
        let spans = code_spans("a `bd ready` b\n\n```\nbd blocked\n```\n");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0], (1, "bd ready".to_string()));
        assert_eq!(spans[1], (4, "bd blocked".to_string()));
    }

    #[test]
    fn a_pipeline_stops_at_the_pipe() {
        let a = argv("bd ready --json | jq -r '.[].id'");
        assert_eq!(a.args, vec!["ready", "--json"]);

        let a = argv("bd export && git add .beads");
        assert_eq!(a.args, vec!["export"]);
    }

    #[test]
    fn bd_is_recognised_however_it_is_named() {
        assert!(is_bd("bd"));
        assert!(is_bd("bd.exe"));
        assert!(is_bd("/usr/local/bin/bd"));
        assert!(is_bd(r"C:\tools\bd.exe"));
        // And these are not bd.
        assert!(!is_bd("bdtool"));
        assert!(!is_bd("bd-a3f2"));
        assert!(!is_bd("/opt/bdtools/run"));
    }

    /// Both hook shapes, and any shape either vendor invents next. A walker that
    /// only understood Claude's nesting would report Cursor as "no hooks
    /// installed" — an `Ok` that means "I did not look".
    #[test]
    fn hook_commands_are_found_whatever_the_settings_file_looks_like() {
        let claude: Value = serde_json::from_str(
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"bd prime"}]}]}}"#,
        )
        .unwrap();
        let found = commands_in(&claude);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "bd prime");
        assert!(found[0].0.contains("SessionStart"), "the site is the evidence");

        let cursor: Value = serde_json::from_str(
            r#"{"hooks":{"sessionStart":[{"command":"bd cursor-hook sessionStart"}]}}"#,
        )
        .unwrap();
        assert_eq!(commands_in(&cursor)[0].1, "bd cursor-hook sessionStart");

        // An MCP server entry is not a bd hook, and must not be counted as one.
        let mcp: Value =
            serde_json::from_str(r#"{"mcpServers":{"beads":{"command":"uvx","args":["beads-mcp"]}}}"#)
                .unwrap();
        assert!(bd_argv(&commands_in(&mcp)[0].1).is_none());
    }

    #[test]
    fn a_token_that_could_not_be_a_command_is_never_accused_of_being_a_missing_one() {
        for t in ["<id>", "--json", "$ID", "SessionStart", "\"title\"", "-p", ""] {
            assert!(!plausible_command(t), "{t} is not a command name");
        }
        for t in ["ready", "dep", "recompute-blocked", "q"] {
            assert!(plausible_command(t), "{t} is a command name");
        }
    }

    #[test]
    fn a_file_that_mentions_beads_is_wired_and_one_that_does_not_is_not() {
        assert!(mentions_beads("Run `bd ready` first."));
        assert!(mentions_beads("This repo tracks work in BEADS."));
        assert!(!mentions_beads("# My project\n\nAlways run the linter.\n"));
    }
}
