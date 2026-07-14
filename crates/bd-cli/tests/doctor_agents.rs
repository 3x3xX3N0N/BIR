//! The Integrations family of `bd doctor`.
//!
//! (Named `doctor_agents.rs`, not `doctor_agent_setup.rs`. cargo names the test
//! binary after the file, and Windows auto-elevates any executable whose name
//! contains "install", "setup", "update" or "patch". It has bitten this repo
//! once already.)
//!
//! The thing being tested is mostly **silence**. This family decides whether
//! anybody reads `bd doctor` output at all: a user who does not use Cursor has no
//! Cursor problem, and a warning that says otherwise costs more than the check is
//! worth. So the first and largest test asserts that a project with no agent
//! integration produces no output whatsoever, and the rest assert that the only
//! things that *do* speak up are integrations which are present and broken.

use std::path::{Path, PathBuf};

use clap::Parser as _;

use bd_cli::cli::Cli;
use bd_cli::context::{Ctx, Need};
use bd_cli::doctor::checks::agents::{self, Hooks};
use bd_cli::doctor::{Category, Check, Dx, Finding, Status};

// ---------------------------------------------------------------------------
// The one that matters: absence is not failure
// ---------------------------------------------------------------------------

/// A project with no coding agent configured must produce **zero** lines.
///
/// The human printer only prints findings that are not `Ok`, and collapses a
/// category with nothing to say into a single `ok  Integrations (4 checks)`. So
/// "every finding is Ok" is precisely "this family cost the user one line" — and
/// that number is the whole design constraint. Nine green lines about editors
/// somebody does not use is how the one warning that mattered stops being read.
#[tokio::test]
async fn a_project_with_no_agent_configured_says_nothing_at_all() {
    let tmp = tempdir("quiet");
    let ctx = ctx_at(&tmp).await;
    let dx = Dx::new(&ctx);

    for f in all(&dx).await {
        assert_eq!(
            f.status,
            Status::Ok,
            "{}: absence is not failure, but it warned: {} ({:?})",
            f.name,
            f.message,
            f.detail
        );
    }
    clean(&tmp);
}

/// And a project that *uses* an agent but has not wired beads into it is also
/// not a fault. It is a choice, and `bd doctor` does not get to nag about it.
#[tokio::test]
async fn an_agent_that_is_present_but_not_wired_to_beads_is_not_a_problem() {
    let tmp = tempdir("unwired");
    write(&tmp, "CLAUDE.md", "# My project\n\nAlways run the linter.\n");
    write(&tmp, ".cursor/rules/style.mdc", "Use tabs.\n");
    write(&tmp, "AGENTS.md", "# Agents\n\nBe careful.\n");

    let ctx = ctx_at(&tmp).await;
    let dx = Dx::new(&ctx);

    for f in all(&dx).await {
        assert_eq!(
            f.status,
            Status::Ok,
            "{}: an unwired harness is not broken: {}",
            f.name,
            f.message
        );
    }

    // It is *reported*, though — in the JSON, where an agent can read it. Just
    // never as a complaint.
    let inv = one("agent-integrations", &dx).await;
    assert!(inv.message.contains("wired into none"), "{}", inv.message);
    let detail = inv.detail.unwrap_or_default();
    assert!(detail.contains("claude"), "{detail}");
    assert!(detail.contains("cursor"), "{detail}");

    clean(&tmp);
}

// ---------------------------------------------------------------------------
// Documentation that has drifted away from the CLI
// ---------------------------------------------------------------------------

/// The nastiest failure this family can catch, because it has no symptom: the
/// docs tell the agent to run a command that does not exist, the agent runs it,
/// it fails, and the human never sees any of it. It just looks like the agent is
/// bad at using beads.
#[tokio::test]
async fn docs_that_name_a_command_this_bd_does_not_have_are_reported() {
    let tmp = tempdir("drift");
    write(
        &tmp,
        "CLAUDE.md",
        "# Project\n\
         \n\
         Before you start, run `bd cursor-hook sessionStart`.\n\
         \n\
         ```sh\n\
         bd prime --stealth --hook-json\n\
         bd cleanup --older-than 90\n\
         ```\n",
    );

    let ctx = ctx_at(&tmp).await;
    let dx = Dx::new(&ctx);
    let f = one("agent-docs-drift", &dx).await;

    assert_eq!(f.status, Status::Warn, "{}", f.message);
    let detail = f.detail.expect("the evidence is the point");
    // Named, with the line, or it is a bug report you cannot act on.
    assert!(detail.contains("cursor-hook"), "{detail}");
    assert!(detail.contains("cleanup"), "{detail}");
    assert!(detail.contains("--stealth"), "{detail}");
    assert!(detail.contains("CLAUDE.md:3"), "the line number is the fix: {detail}");

    clean(&tmp);
}

/// The guard against this family's worst failure: a confident warning about a
/// command nobody ever wrote. Prose is not code. `bd doctor` reading "beads
/// decides what is ready" and reporting that `bd decides` does not exist would
/// discredit every other warning in the command.
#[tokio::test]
async fn prose_and_placeholders_never_produce_a_false_positive() {
    let tmp = tempdir("prose");
    write(
        &tmp,
        "AGENTS.md",
        "# Beads\n\
         \n\
         bd tracks work as a graph. Do not run bd manually; bd decides what is ready.\n\
         \n\
         1. `bd ready --json` — pick one.\n\
         2. `bd update <id> --claim` — take it.\n\
         3. `bd close <id> --reason done`.\n\
         \n\
         ```sh\n\
         bd create \"Write the parser\" -t task -p 1\n\
         bd dep add <id> <blocker>\n\
         bd ready --json | jq -r '.[].id'\n\
         ```\n\
         \n\
         Every command takes `--json`. See `bd --help`, or `bd hooks`.\n",
    );

    let ctx = ctx_at(&tmp).await;
    let dx = Dx::new(&ctx);
    let f = one("agent-docs-drift", &dx).await;

    assert_eq!(
        f.status,
        Status::Ok,
        "false positive — this is real documentation: {:?}",
        f.detail
    );
    clean(&tmp);
}

/// `bd setup` refuses to touch a file whose markers are unbalanced, and it is
/// right to: it cannot tell where the user's prose ends and its own block began.
/// But the refusal is silent and permanent — that file's beads section will never
/// be updated again, and will rot into the drift the check above looks for.
#[tokio::test]
async fn a_managed_block_bd_setup_can_no_longer_update_is_reported() {
    let tmp = tempdir("markers");
    write(
        &tmp,
        "CLAUDE.md",
        "# Mine\n\n<!-- BEGIN BEADS -->\nRun `bd ready`.\n\nSomeone deleted the end marker.\n",
    );

    let ctx = ctx_at(&tmp).await;
    let dx = Dx::new(&ctx);
    let f = one("agent-docs-markers", &dx).await;

    assert_eq!(f.status, Status::Warn);
    let detail = f.detail.expect("which file");
    assert!(detail.contains("CLAUDE.md"), "{detail}");
    assert!(f.fix.is_some(), "a warning with no way out is just noise");

    clean(&tmp);
}

/// A well-formed block is not a problem, and two of them are.
#[tokio::test]
async fn a_well_formed_block_is_quiet_and_a_duplicated_one_is_not() {
    let tmp = tempdir("blocks");
    write(
        &tmp,
        "CLAUDE.md",
        "# Mine\n\n<!-- BEGIN BEADS -->\nRun `bd ready`.\n<!-- END BEADS -->\n",
    );
    let ctx = ctx_at(&tmp).await;
    assert_eq!(
        one("agent-docs-markers", &Dx::new(&ctx)).await.status,
        Status::Ok
    );

    write(
        &tmp,
        "CLAUDE.md",
        "<!-- BEGIN BEADS -->\na\n<!-- END BEADS -->\n\n<!-- BEGIN BEADS -->\nb\n<!-- END BEADS -->\n",
    );
    let ctx = ctx_at(&tmp).await;
    assert_eq!(
        one("agent-docs-markers", &Dx::new(&ctx)).await.status,
        Status::Warn,
        "the agent would read the beads section twice"
    );

    clean(&tmp);
}

/// The markers above are duplicated from `commands::setup`, whose copies are
/// private. That duplication is a liability, so it is tested rather than trusted:
/// if `setup` ever renames a marker, this fails instead of the doctor check
/// silently going blind.
#[test]
fn the_markers_this_family_looks_for_are_the_ones_bd_setup_writes() {
    let tmp = tempdir("onboard");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", tmp.to_str().unwrap(), "onboard"])
        .output()
        .expect("run bd onboard");
    assert!(out.status.success(), "bd onboard failed");
    let block = String::from_utf8_lossy(&out.stdout);

    assert!(
        block.contains("<!-- BEGIN BEADS -->") && block.contains("<!-- END BEADS -->"),
        "`bd setup` changed its markers; doctor/checks/agents.rs still looks for the old ones \
         and has gone blind to every managed block:\n{block}"
    );
    clean(&tmp);
}

// ---------------------------------------------------------------------------
// Hooks: installed, and silently failing every time they run
// ---------------------------------------------------------------------------

/// A hook is the one integration that fails invisibly — nobody watches a
/// SessionStart hook, so a broken one looks exactly like an agent that did not
/// bother. `bd prime --stealth --hook-json` is what upstream's installer writes,
/// and in this port it is a usage error on every single session start.
#[tokio::test]
async fn a_hook_that_runs_a_command_this_bd_removed_is_reported() {
    let tmp = tempdir("hook-cmd");
    write(
        &tmp,
        ".claude/settings.json",
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"bd prime --stealth"}]}]}}"#,
    );
    write(
        &tmp,
        ".cursor/hooks.json",
        r#"{"hooks":{"sessionStart":[{"command":"bd cursor-hook sessionStart"}]}}"#,
    );

    let ctx = ctx_at(&tmp).await;
    let f = Hooks::new(None).run(&Dx::new(&ctx)).await;

    assert_eq!(f.status, Status::Warn, "{}", f.message);
    let detail = f.detail.expect("name the hooks");
    assert!(detail.contains("--stealth"), "{detail}");
    assert!(detail.contains("cursor-hook"), "{detail}");
    // Where it lives, and which event it is wired to.
    assert!(detail.contains(".claude/settings.json"), "{detail}");
    assert!(detail.contains("SessionStart"), "{detail}");

    clean(&tmp);
}

/// The classic: a hook that hard-codes a path to `bd`. It worked perfectly right
/// up until the binary moved, and it has been failing silently ever since.
#[tokio::test]
async fn a_hook_pointing_at_a_bd_that_is_not_there_any_more_is_reported() {
    let tmp = tempdir("hook-path");
    write(
        &tmp,
        ".claude/settings.json",
        r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"/opt/old/bin/bd prime"}]}]}}"#,
    );

    let ctx = ctx_at(&tmp).await;
    let f = Hooks::new(None).run(&Dx::new(&ctx)).await;

    assert_eq!(f.status, Status::Warn);
    let detail = f.detail.expect("name the path");
    assert!(detail.contains("/opt/old/bin/bd"), "{detail}");
    assert!(detail.contains("does not exist"), "{detail}");

    clean(&tmp);
}

/// A settings file that will not parse does not break *our* hook — it breaks
/// every hook in the file, and the agent says nothing about it. We also cannot
/// tell whether beads is in there, which is `Warn` by the seam's own rule:
/// undeterminable is never `Ok`.
#[tokio::test]
async fn a_settings_file_that_will_not_parse_is_a_warning_not_an_ok() {
    let tmp = tempdir("hook-json");
    write(
        &tmp,
        ".claude/settings.json",
        "{\n  \"hooks\": {\n    \"SessionStart\": [,]\n  }\n}\n",
    );

    let ctx = ctx_at(&tmp).await;
    let f = Hooks::new(None).run(&Dx::new(&ctx)).await;

    assert_eq!(f.status, Status::Warn);
    assert!(!f.is_ok(), "a file we could not read must never report as coverage");
    assert!(
        f.detail.unwrap_or_default().contains(".claude/settings.json"),
        "say which file"
    );
    clean(&tmp);
}

/// A hook that is installed and *works* is not a warning either. This is the
/// check that would catch the family crying wolf about its own happy path.
#[tokio::test]
async fn a_working_hook_is_quiet() {
    let tmp = tempdir("hook-ok");
    write(
        &tmp,
        ".claude/settings.json",
        &format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"command":"{} prime"}}]}}]}}}}"#,
            // The bd we are testing: an absolute path that certainly exists.
            env!("CARGO_BIN_EXE_bd").replace('\\', "\\\\")
        ),
    );

    let ctx = ctx_at(&tmp).await;
    let f = Hooks::new(None).run(&Dx::new(&ctx)).await;
    assert_eq!(f.status, Status::Ok, "{}: {:?}", f.message, f.detail);
    clean(&tmp);
}

/// A settings file with hooks in it, none of them beads', is somebody else's
/// business. We report that we looked, and nothing else.
#[tokio::test]
async fn hooks_that_are_not_ours_are_left_alone() {
    let tmp = tempdir("hook-theirs");
    write(
        &tmp,
        ".claude/settings.json",
        r#"{"hooks":{"PostToolUse":[{"hooks":[{"command":"npm run lint"}]}]}}"#,
    );

    let ctx = ctx_at(&tmp).await;
    let f = Hooks::new(None).run(&Dx::new(&ctx)).await;
    assert_eq!(f.status, Status::Ok);
    assert!(f.message.contains("none of them"), "{}", f.message);
    clean(&tmp);
}

/// The user-level branch. A beads hook in `~/.claude/settings.json` applies to
/// this repository too, so a broken one is worth saying — but *only* the beads
/// part of it. Somebody's global editor config is not this workspace's fault and
/// must not be warned about once per repository.
#[tokio::test]
async fn a_broken_beads_hook_in_the_home_directory_is_reported_but_nothing_else_is() {
    let tmp = tempdir("hook-home");
    let home = tmp.join("home");
    write(
        &home,
        ".claude/settings.json",
        r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"bd claude-hook"}]}]}}"#,
    );

    let ctx = ctx_at(&tmp).await;
    let f = Hooks::new(Some(home.clone())).run(&Dx::new(&ctx)).await;
    assert_eq!(f.status, Status::Warn);
    let detail = f.detail.unwrap_or_default();
    assert!(detail.contains("claude-hook"), "{detail}");
    assert!(detail.contains("~/.claude/settings.json"), "{detail}");

    // Now break the same file in a way that has nothing to do with beads. It is
    // still broken — and it is still none of our business.
    write(&home, ".claude/settings.json", "{ not json at all ");
    let f = Hooks::new(Some(home)).run(&Dx::new(&ctx)).await;
    assert_eq!(
        f.status,
        Status::Ok,
        "a broken global config with no beads in it must not warn once per repo: {:?}",
        f.detail
    );

    clean(&tmp);
}

// ---------------------------------------------------------------------------
// The registry itself
// ---------------------------------------------------------------------------

/// Check names are the key agents grep for in `--json`, so they are API. And the
/// count is the promise this file makes: four checks, one collapsed line.
#[test]
fn the_family_registers_exactly_the_checks_it_documents() {
    let names: Vec<&str> = agents::checks().iter().map(|c| c.name()).collect();
    assert_eq!(
        names,
        vec![
            "agent-integrations",
            "agent-hooks",
            "agent-docs-drift",
            "agent-docs-markers",
        ]
    );
    for c in agents::checks() {
        assert_eq!(c.category(), Category::Integration, "{}", c.name());
    }
}

/// Doctor's whole premise: it runs on workspaces too broken to open. Not one of
/// these checks touches the store, so all four must survive `dx.dir == None` —
/// which is also the state of every project that has not run `bd init` yet, and
/// therefore the state of everyone reading `bd doctor` for the first time.
#[tokio::test]
async fn every_check_survives_having_no_workspace_at_all() {
    let tmp = tempdir("noworkspace");
    let ctx = ctx_at(&tmp).await;
    let dx = Dx::new(&ctx);
    assert!(!dx.in_workspace(), "the point of the test");

    for f in all(&dx).await {
        assert_ne!(f.status, Status::Error, "{}: {}", f.name, f.message);
    }
    clean(&tmp);
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Every check in the family, with the home directory pinned to nothing so the
/// result depends on the temporary project and not on the machine it runs on.
async fn all(dx: &Dx<'_>) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in agents::checks() {
        if c.name() == "agent-hooks" {
            continue; // real `$HOME`; covered explicitly with an injected one
        }
        out.push(c.run(dx).await);
    }
    out.push(Hooks::new(None).run(dx).await);
    out
}

async fn one(name: &str, dx: &Dx<'_>) -> Finding {
    let checks = agents::checks();
    let check = checks
        .iter()
        .find(|c| c.name() == name)
        .unwrap_or_else(|| panic!("no check named {name}"));
    check.run(dx).await
}

async fn ctx_at(dir: &Path) -> Ctx {
    let cli = Cli::parse_from(["bd", "-C", dir.to_str().expect("utf-8 path"), "doctor"]);
    Ctx::build(&cli, Need::Nothing)
        .await
        .expect("doctor builds a context anywhere, workspace or not")
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(&path, content).expect("write");
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-doctor-agents-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).expect("mkdir");
    std::fs::canonicalize(&p).expect("canonicalize")
}

fn clean(p: &Path) {
    std::fs::remove_dir_all(p).ok();
}
