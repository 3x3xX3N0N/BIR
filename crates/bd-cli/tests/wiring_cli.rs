//! End-to-end tests for the commands that teach an agent to use beads — and,
//! more importantly, for the two that write files a human also writes.
//!
//! (Named `wiring_cli` rather than `setup_cli` for a stupid but real reason:
//! Windows' installer-detection heuristic auto-elevates any executable whose
//! name contains "setup", so `setup_cli-<hash>.exe` fails to launch with
//! ERROR_ELEVATION_REQUIRED before a single test runs.)
//!
//! `bd setup` edits CLAUDE.md and AGENTS.md; `bd hooks install` edits
//! `.git/hooks`. Both are files someone else already owns. A bug in either is
//! not a wrong answer you can re-run — it is deleted work. So the tests that
//! matter here are the destructive ones: run it twice, run it over prose, run it
//! over somebody else's hook, and assert that nothing was lost.

use std::path::{Path, PathBuf};
use std::process::Command;

const BEGIN: &str = "<!-- BEGIN BEADS -->";
const END: &str = "<!-- END BEADS -->";

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn run(dir: &Path, args: &[&str]) -> Run {
    let out = bd()
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .env("BEADS_ACTOR", "agent-7")
        .output()
        .expect("run bd");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        code: out.status.code().unwrap_or(-1),
    }
}

fn json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let r = run(dir, args);
    serde_json::from_str(&r.stdout)
        .unwrap_or_else(|e| panic!("bd {args:?} did not emit JSON ({e}): {}", r.stdout))
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-setup-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

// ---------------------------------------------------------------------------
// setup — the one that must never destroy anything
// ---------------------------------------------------------------------------

/// The test this whole file exists for.
///
/// A user's CLAUDE.md is *their* file. beads gets one delimited block in it and
/// nothing else: run `bd setup` twice and the prose above and below must come
/// back byte-for-byte, with exactly one block between them.
#[test]
fn setup_claude_twice_is_idempotent_and_never_touches_the_users_prose() {
    let dir = tempdir("claude");
    let claude = dir.join("CLAUDE.md");

    let above = "# Acme\n\nAlways run `make lint` before you commit.\nNever touch vendor/.\n";
    let below = "\n## House style\n\nTabs. We are not going to discuss it.\n";
    std::fs::write(&claude, format!("{above}{below}")).unwrap();

    // CLAUDE.md exists, so `bd setup` picks the claude recipe.
    let first = run(&dir, &["setup"]);
    assert_eq!(first.code, 0, "{}{}", first.stdout, first.stderr);
    let after_one = std::fs::read_to_string(&claude).unwrap();

    let second = run(&dir, &["setup"]);
    assert_eq!(second.code, 0, "{}{}", second.stdout, second.stderr);
    let after_two = std::fs::read_to_string(&claude).unwrap();

    // 1. Byte-identical: the second run added nothing and rewrote nothing.
    assert_eq!(
        after_one, after_two,
        "`bd setup` is not idempotent — the second run changed the file"
    );

    // 2. Exactly one block, not two.
    assert_eq!(
        after_two.matches(BEGIN).count(),
        1,
        "a second run appended a second beads block:\n{after_two}"
    );
    assert_eq!(after_two.matches(END).count(), 1);

    // 3. Every line the user wrote is still there, in order.
    assert!(after_two.starts_with(above), "the prose above was rewritten");
    for line in above.lines().chain(below.lines()) {
        assert!(
            after_two.contains(line),
            "`bd setup` lost a line of the user's own file: {line:?}"
        );
    }

    // 4. And the block actually says something.
    assert!(after_two.contains("bd ready"));
    assert!(after_two.contains("--claim"));

    // 5. The second run reports the truth rather than claiming a write.
    let doc = json(&dir, &["--json", "setup"]);
    assert_eq!(doc["files"][0]["recipe"], "claude");
    assert_eq!(doc["files"][0]["action"], "unchanged");

    std::fs::remove_dir_all(&dir).ok();
}

/// A previous block is *replaced*, not stacked on. This is what an upgrade does,
/// and it is where an append-only implementation would quietly grow the file
/// forever.
#[test]
fn setup_replaces_a_stale_block_and_keeps_what_surrounds_it() {
    let dir = tempdir("stale");
    let claude = dir.join("CLAUDE.md");
    std::fs::write(
        &claude,
        format!("# Acme\n\nMine, above.\n\n{BEGIN}\nWILDLY-OUT-OF-DATE\n{END}\n\nMine, below.\n"),
    )
    .unwrap();

    assert_eq!(run(&dir, &["setup"]).code, 0);
    let after = std::fs::read_to_string(&claude).unwrap();

    assert!(!after.contains("WILDLY-OUT-OF-DATE"), "the stale block survived");
    assert_eq!(after.matches(BEGIN).count(), 1);
    assert!(after.starts_with("# Acme\n\nMine, above.\n\n"));
    assert!(after.ends_with("\nMine, below.\n"));

    std::fs::remove_dir_all(&dir).ok();
}

/// Half a pair of markers means someone hand-edited the file. There is no way to
/// know where their text ends, so the only safe move is to stop — and to leave
/// the file exactly as it was found.
#[test]
fn setup_refuses_a_file_with_unbalanced_markers_and_leaves_it_alone() {
    let dir = tempdir("unbalanced");
    let claude = dir.join("CLAUDE.md");
    let broken = format!("# Acme\n\n{BEGIN}\nsomeone deleted the closing marker\n\nMore prose.\n");
    std::fs::write(&claude, &broken).unwrap();

    let r = run(&dir, &["setup"]);
    assert_ne!(r.code, 0, "beads must refuse a file it cannot parse");
    assert!(
        r.stderr.contains("no matching"),
        "the error must say what is wrong: {}",
        r.stderr
    );
    assert_eq!(
        std::fs::read_to_string(&claude).unwrap(),
        broken,
        "a refused setup must not have written anything"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn setup_writes_agents_md_when_the_repo_names_no_harness() {
    let dir = tempdir("agents");
    let r = run(&dir, &["setup"]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);

    let agents = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
    assert_eq!(agents.matches(BEGIN).count(), 1);
    assert!(
        !dir.join("CLAUDE.md").exists(),
        "setup must not litter a repo with files for harnesses it does not use"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn setup_updates_every_harness_the_repo_already_has() {
    let dir = tempdir("both");
    std::fs::write(dir.join("CLAUDE.md"), "# c\n").unwrap();
    std::fs::write(dir.join("AGENTS.md"), "# a\n").unwrap();

    let doc = json(&dir, &["--json", "setup"]);
    let files = doc["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    for f in files {
        assert_eq!(f["action"], "appended");
    }
    for name in ["CLAUDE.md", "AGENTS.md"] {
        let t = std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(t.matches(BEGIN).count(), 1, "{name}");
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn setup_refuses_to_write_under_readonly() {
    let dir = tempdir("ro");
    let r = run(&dir, &["--readonly", "setup"]);
    assert_ne!(r.code, 0);
    assert!(!dir.join("AGENTS.md").exists(), "--readonly is a guard, not a hint");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The commands that only print
// ---------------------------------------------------------------------------

#[test]
fn onboard_and_quickstart_run_without_a_workspace() {
    let dir = tempdir("nows");

    let r = run(&dir, &["onboard"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains(BEGIN) && r.stdout.contains(END));
    assert!(r.stdout.contains("bd ready"));

    // What onboard prints is exactly what setup writes — otherwise pasting it by
    // hand and running `bd setup` later would produce two different blocks, and
    // the second would not recognize the first.
    run(&dir, &["setup"]);
    let written = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
    assert!(
        written.contains(r.stdout.trim()),
        "`bd onboard` and `bd setup` must emit the same block"
    );

    let r = run(&dir, &["quickstart"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("bd init"));
    assert!(r.stdout.contains("bd ready"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn onboard_emits_the_block_as_json_too() {
    let dir = tempdir("onboardjson");
    let doc = json(&dir, &["--json", "onboard"]);
    assert!(doc["markdown"].as_str().unwrap().contains(BEGIN));
    std::fs::remove_dir_all(&dir).ok();
}

/// `bd prime` is the command agents are told to run first, so it has to answer
/// two things at once: how the loop works, and what the board looks like now.
#[test]
fn prime_prints_the_loop_and_the_real_state_of_the_board() {
    let dir = tempdir("prime");
    assert_eq!(run(&dir, &["init", "--prefix", "t"]).code, 0);

    let blocker = run(&dir, &["q", "Write the schema", "-p", "1"]).stdout.trim().to_string();
    let gated = run(&dir, &["q", "Ship it"]).stdout.trim().to_string();
    assert_eq!(run(&dir, &["dep", "add", &gated, &blocker]).code, 0);

    let doc = json(&dir, &["--json", "prime"]);
    assert_eq!(doc["state"]["ready"], 1, "only the blocker is ready");
    assert_eq!(doc["state"]["blocked"], 1);
    assert_eq!(doc["next"][0]["id"], blocker.as_str());
    assert_eq!(doc["actor"], "agent-7");
    // The workflow travels with the state: an agent parsing --json must still
    // learn the loop, or it will parse the board and then invent a process.
    assert_eq!(doc["loop"].as_array().unwrap().len(), 4);
    assert!(doc["commands"].as_array().unwrap().len() >= 8);

    let human = run(&dir, &["prime"]);
    assert_eq!(human.code, 0, "{}", human.stderr);
    assert!(human.stdout.contains("THE LOOP"));
    assert!(human.stdout.contains("--claim"));
    assert!(human.stdout.contains(&blocker), "prime must name the ready work");

    // Claim it, and prime must now hand the agent back its own work first.
    assert_eq!(run(&dir, &["update", &blocker, "--claim"]).code, 0);
    let doc = json(&dir, &["--json", "prime"]);
    assert_eq!(doc["yours"][0]["id"], blocker.as_str());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn bootstrap_creates_a_workspace_and_wires_the_agent_up_in_one_step() {
    let dir = tempdir("bootstrap");

    let doc = json(&dir, &["--json", "bootstrap"]);
    assert_eq!(doc["created"], true);
    assert_eq!(doc["backend"], "sqlite");
    assert_eq!(doc["files"][0]["action"], "created");
    assert!(dir.join(".beads").join("workspace.json").exists());
    assert!(dir.join("AGENTS.md").exists());

    // Twice: an existing workspace is kept, not clobbered, and the block is not
    // duplicated. `bootstrap` is not a hidden `init --force`.
    let doc = json(&dir, &["--json", "bootstrap"]);
    assert_eq!(doc["created"], false, "bootstrap must not re-init over live data");
    assert_eq!(doc["files"][0]["action"], "unchanged");
    let agents = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
    assert_eq!(agents.matches(BEGIN).count(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// metrics and upgrade
// ---------------------------------------------------------------------------

#[test]
fn metrics_is_a_local_flag_and_says_so() {
    let dir = tempdir("metrics");
    assert_eq!(run(&dir, &["init", "--prefix", "m"]).code, 0);

    let doc = json(&dir, &["--json", "metrics", "on"]);
    assert_eq!(doc["enabled"], true);
    // The claim that matters: this port does not phone home, and must not let
    // anyone infer that it does.
    assert_eq!(doc["sends_data"], false);
    assert_eq!(run(&dir, &["config", "get", "metrics.enabled"]).stdout.trim(), "true");

    let doc = json(&dir, &["--json", "metrics", "off"]);
    assert_eq!(doc["enabled"], false);

    let r = run(&dir, &["metrics", "example"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("no telemetry"), "got: {}", r.stdout);
    let doc = json(&dir, &["--json", "metrics", "example"]);
    assert_eq!(doc["sends_data"], false);
    assert!(doc["example_payload"]["version"].is_string());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn upgrade_tracks_the_version_this_workspace_last_acknowledged() {
    let dir = tempdir("upgrade");
    assert_eq!(run(&dir, &["init", "--prefix", "u"]).code, 0);

    let doc = json(&dir, &["--json", "upgrade", "status"]);
    assert_eq!(doc["pending"], true, "a fresh workspace has acked nothing");
    assert!(doc["acked_version"].is_null());

    assert_eq!(run(&dir, &["upgrade", "ack"]).code, 0);

    let doc = json(&dir, &["--json", "upgrade", "status"]);
    assert_eq!(doc["pending"], false);
    assert_eq!(doc["acked_version"], doc["version"]);

    let r = run(&dir, &["upgrade", "review"]);
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("Nothing to do"));

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// hooks — the other place we can destroy something
// ---------------------------------------------------------------------------

/// A `.git` with a hooks dir, but no git plumbing. `bd hooks` falls back to the
/// on-disk layout when `git rev-parse` cannot answer, which is exactly this case.
fn fake_git(root: &Path) -> PathBuf {
    let hooks = root.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    hooks
}

#[test]
fn hooks_install_then_list_then_uninstall() {
    let dir = tempdir("hooks");
    assert_eq!(run(&dir, &["init", "--prefix", "h"]).code, 0);
    let hooks = fake_git(&dir);

    let r = run(&dir, &["hooks", "install"]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    let script = std::fs::read_to_string(hooks.join("pre-commit")).unwrap();
    assert!(script.contains("beads-managed-hook"));
    assert!(script.contains("bd hooks run pre-commit"));

    let doc = json(&dir, &["--json", "hooks", "list"]);
    let states: Vec<&str> = doc["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["state"].as_str().unwrap())
        .collect();
    assert!(states.iter().all(|s| *s == "beads"), "{states:?}");

    // Re-installing over our own hook is fine — that is how an upgrade lands.
    assert_eq!(run(&dir, &["hooks", "install"]).code, 0);

    assert_eq!(run(&dir, &["hooks", "uninstall"]).code, 0);
    assert!(!hooks.join("pre-commit").exists());

    std::fs::remove_dir_all(&dir).ok();
}

/// The one that would be unforgivable: someone has a pre-commit hook (husky, a
/// linter, a secret scanner) and `bd hooks install` overwrites it. It must not.
#[test]
fn hooks_install_never_overwrites_a_hook_beads_did_not_write() {
    let dir = tempdir("foreign");
    assert_eq!(run(&dir, &["init", "--prefix", "f"]).code, 0);
    let hooks = fake_git(&dir);

    let theirs = "#!/bin/sh\n# years of accumulated wisdom\nexec ./scripts/secret-scan.sh\n";
    std::fs::write(hooks.join("pre-commit"), theirs).unwrap();

    let r = run(&dir, &["hooks", "install"]);
    assert_eq!(
        std::fs::read_to_string(hooks.join("pre-commit")).unwrap(),
        theirs,
        "bd overwrote a hook it did not write"
    );
    assert_ne!(r.code, 0, "a partial install must not report success");
    assert!(
        r.stdout.contains("bd hooks run") || r.stderr.contains("bd hooks run"),
        "refusing is only half the job — say how to chain it in: {}{}",
        r.stdout,
        r.stderr
    );

    // The hook it *could* install, it did.
    assert!(hooks.join("post-merge").exists());

    // And uninstall is not a licence to delete either.
    assert_eq!(run(&dir, &["hooks", "uninstall"]).code, 0);
    assert_eq!(
        std::fs::read_to_string(hooks.join("pre-commit")).unwrap(),
        theirs,
        "uninstall deleted a foreign hook"
    );

    let doc = json(&dir, &["--json", "hooks", "list"]);
    let pre = &doc["hooks"][0];
    assert_eq!(pre["hook"], "pre-commit");
    assert_eq!(pre["state"], "foreign");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn hooks_outside_a_git_repo_say_so_plainly() {
    let dir = tempdir("nogit");
    assert_eq!(run(&dir, &["init", "--prefix", "n"]).code, 0);

    let r = run(&dir, &["hooks", "install"]);
    assert_ne!(r.code, 0);
    assert!(
        r.stderr.contains("no git repository"),
        "the message must name the actual problem: {}",
        r.stderr
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The hook's payload: the database becomes text that git can carry.
#[test]
fn the_pre_commit_hook_exports_the_database_to_jsonl() {
    let dir = tempdir("precommit");
    assert_eq!(run(&dir, &["init", "--prefix", "p"]).code, 0);
    // A real repo this time: the hook stages the file, and `git add` needs one.
    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return; // no git on PATH; the rest of the suite still covers the logic
    }

    let id = run(&dir, &["q", "Carry me through git"]).stdout.trim().to_string();

    let r = run(&dir, &["hooks", "run", "pre-commit"]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);

    let jsonl = std::fs::read_to_string(dir.join(".beads").join("issues.jsonl")).unwrap();
    assert!(jsonl.contains(&id), "the export is missing the issue");

    // Staged, not merely written: a file git does not know about is not a backup.
    let staged = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let staged = String::from_utf8_lossy(&staged.stdout);
    assert!(
        staged.contains("issues.jsonl"),
        "the hook must stage what it exported, or the commit will not carry it: {staged}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unknown_hook_name_is_an_error_not_a_shrug() {
    let dir = tempdir("unknownhook");
    assert_eq!(run(&dir, &["init", "--prefix", "x"]).code, 0);
    let r = run(&dir, &["hooks", "run", "pre-rebase"]);
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("pre-rebase"), "{}", r.stderr);
    std::fs::remove_dir_all(&dir).ok();
}
