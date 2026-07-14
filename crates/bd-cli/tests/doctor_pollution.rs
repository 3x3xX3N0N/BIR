//! Maintenance debris, end to end through the real binary.
//!
//! The property under test is not "doctor notices the mess". It is **what
//! `--fix` refuses to delete**. A cleanup that removes a live session's lock, or
//! the user's issues, is worse than no cleanup at all — so every test here that
//! matters is an assertion that a file is *still there* afterwards.
//!
//! (The filename deliberately avoids the words install/setup/update/patch:
//! cargo names the test binary after the file, and Windows auto-elevates any exe
//! whose name looks like an installer.)

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

/// A pid that cannot be running. Far above Linux's hard `pid_max` ceiling (2^22),
/// and `tasklist` answers "no tasks" for it on Windows.
///
/// The alternative — spawn a process, reap it, use its pid — is what a crashed
/// `bd` actually leaves behind, but it makes the test a coin flip on Windows,
/// where a freed pid can be handed straight to the next process that starts.
const DEAD_PID: u32 = 2_000_000_000;

struct Ws(PathBuf);

impl Ws {
    fn new(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-doctor-poll-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        let ws = Ws(std::fs::canonicalize(&p).unwrap());
        let (out, code) = ws.run(&["init", "--prefix", "t"]);
        assert_eq!(code, 0, "init failed: {out}");
        ws
    }

    fn run(&self, args: &[&str]) -> (String, i32) {
        let out = bd()
            .args(["-C", self.0.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    /// `bd doctor --json`, parsed. Also returns the exit code, which is nonzero
    /// iff *some* check reported an error — not necessarily one of ours, so tests
    /// here lean on the named finding rather than the exit code wherever they can.
    fn doctor(&self, extra: &[&str]) -> (Value, i32) {
        let mut args = vec!["--json", "doctor"];
        args.extend_from_slice(extra);
        let (out, code) = self.run(&args);
        let v = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("bd doctor did not emit JSON ({e}): {out}"));
        (v, code)
    }

    fn beads(&self) -> PathBuf {
        self.0.join(".beads")
    }

    fn write(&self, rel: &str, body: &str) -> PathBuf {
        let p = self.beads().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, body).unwrap();
        p
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// One finding, by the name agents grep for.
fn finding<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .expect("checks is an array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check named {name:?} in {report:#}"))
}

fn status(report: &Value, name: &str) -> String {
    finding(report, name)["status"].as_str().unwrap().to_string()
}

/// What `--fix` reported doing for one check: `fixed` | `unfixable` | `failed`,
/// or `None` if it did not run a repair for it at all (the finding was `ok`).
fn repair_outcome(report: &Value, name: &str) -> Option<String> {
    report["repairs"]
        .as_array()?
        .iter()
        .find(|r| r["check"] == name)
        .map(|r| r["outcome"].as_str().unwrap_or_default().to_string())
}

fn exists(p: &Path) -> bool {
    p.exists()
}

// ---------------------------------------------------------------------------
// Absence is not failure
// ---------------------------------------------------------------------------

/// A clean workspace has no debris, and the family says so quietly. A doctor that
/// warns about a workspace with nothing wrong with it is a doctor people stop
/// reading.
#[test]
fn a_clean_workspace_has_no_debris() {
    let ws = Ws::new("clean");
    let (r, _) = ws.doctor(&[]);
    for name in [
        "lock file debris",
        "workspace manifest",
        "legacy queue files",
        "interrupted writes",
        "legacy git hooks",
        "test pollution",
    ] {
        assert_eq!(status(&r, name), "ok", "{name} warned about a clean workspace");
    }
}

/// Every check in the family is registered, categorized as Maintenance, and
/// carries the fields a reader needs. Nothing here should be able to vanish from
/// the report without a test noticing.
#[test]
fn the_family_reports_under_maintenance() {
    let ws = Ws::new("cat");
    let (r, _) = ws.doctor(&[]);
    for name in ["lock file debris", "test pollution"] {
        assert_eq!(finding(&r, name)["category"], "maintenance", "{name}");
    }
}

// ---------------------------------------------------------------------------
// Locks: the whole point of the family
// ---------------------------------------------------------------------------

/// A lock whose owner is verifiably gone still blocks the next `bd` that reads
/// it. That is not untidy, it is a false statement — so it is an `Error`, it
/// makes doctor exit nonzero, and `--fix` clears it.
#[test]
fn a_lock_whose_owner_is_gone_is_an_error_and_fix_removes_it() {
    let ws = Ws::new("orphan");
    let lock = ws.write(".sync.lock", &format!("pid={DEAD_PID}\nstarted=2026-07-14T09:00:00Z\n"));

    let (r, code) = ws.doctor(&[]);
    assert_eq!(status(&r, "lock file debris"), "error");
    assert_ne!(code, 0, "an orphaned lock must make `bd doctor` exit nonzero");
    let detail = finding(&r, "lock file debris")["detail"].as_str().unwrap();
    assert!(
        detail.contains(".sync.lock") && detail.contains(&DEAD_PID.to_string()),
        "the finding must name the file and the pid, or it cannot be acted on: {detail}"
    );
    assert!(exists(&lock), "run() must not mutate anything");

    let (r, _) = ws.doctor(&["--fix"]);
    assert_eq!(repair_outcome(&r, "lock file debris").as_deref(), Some("fixed"));
    assert!(!exists(&lock), "--fix must remove an orphaned lock");
}

/// **The one that matters.** The lock records a pid that is unmistakably alive —
/// this very test process. It is not stale, it is *in use*, and deleting it would
/// corrupt whatever session owns it. `--fix` must leave it exactly where it is.
#[test]
fn a_lock_held_by_a_live_process_is_never_deleted() {
    let ws = Ws::new("held");
    let alive = std::process::id();
    let lock = ws.write("dolt.bootstrap.lock", &format!("pid={alive}\nstarted=2026-07-14T09:00:00Z\n"));

    let (r, _) = ws.doctor(&[]);
    assert_eq!(
        status(&r, "lock file debris"),
        "ok",
        "a lock whose owner is running is in use, not stale"
    );

    let (r, _) = ws.doctor(&["--fix"]);
    assert!(
        exists(&lock),
        "--fix deleted a lock held by a live process — this is the failure the whole family exists to avoid"
    );
    // `ok` findings are never even offered to repair().
    assert_eq!(repair_outcome(&r, "lock file debris"), None);
}

/// Beads truncates a lock file when it releases the lock and keeps the path on
/// purpose: deleting it after unlocking splits lock identity between a waiter
/// holding the old inode and the next process creating a fresh file at the same
/// path. An empty lock file is *released*, not abandoned.
///
/// Upstream's check is age-only, so it calls this stale after an hour and its
/// `--fix` deletes it. That is the bug this test pins shut.
#[test]
fn a_released_lock_file_is_not_debris() {
    let ws = Ws::new("released");
    let lock = ws.write(".sync.lock", "");

    let (r, _) = ws.doctor(&[]);
    assert_eq!(status(&r, "lock file debris"), "ok");

    ws.doctor(&["--fix"]);
    assert!(exists(&lock), "an empty lock file is a released lock, not debris");
}

/// A lock in a format we do not recognize belongs to something we cannot identify.
/// Warn — never delete. "I could not tell" is not "it is dead".
#[test]
fn a_lock_with_no_identifiable_owner_is_reported_but_not_deleted() {
    let ws = Ws::new("anon");
    let lock = ws.write("bd.sock.startlock", "held by something that did not sign its name\n");

    let (r, _) = ws.doctor(&[]);
    assert_eq!(status(&r, "lock file debris"), "warning");

    let (r, _) = ws.doctor(&["--fix"]);
    assert!(exists(&lock), "bd must not delete a lock it cannot prove is abandoned");
    assert_eq!(
        repair_outcome(&r, "lock file debris").as_deref(),
        Some("unfixable"),
        "declining to repair must not be reported as having repaired"
    );
}

/// A live owner and a dead one, side by side. The dead one goes; the live one
/// stays. A repair that took the directory as a whole would take both.
#[test]
fn fix_removes_the_dead_lock_and_leaves_the_live_one() {
    let ws = Ws::new("mixed");
    let dead = ws.write(".sync.lock", &format!("pid={DEAD_PID}\n"));
    let live = ws.write("dolt.bootstrap.lock", &format!("pid={}\n", std::process::id()));

    let (r, _) = ws.doctor(&[]);
    assert_eq!(status(&r, "lock file debris"), "error");

    ws.doctor(&["--fix"]);
    assert!(!exists(&dead), "the orphaned lock should be gone");
    assert!(exists(&live), "the held lock must survive");
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

/// The manifest names the engine that owns the data. When it names the wrong one,
/// every command opens the wrong store — it is not debris, it is a lie, and bd
/// will not guess which half of the contradiction was meant.
#[test]
fn a_manifest_that_contradicts_the_disk_is_an_error_and_is_not_auto_fixed() {
    let ws = Ws::new("manifest");
    // Say sqlite, then take the sqlite database away and leave a dolt one.
    for f in ["beads.db", "beads.db-wal", "beads.db-shm"] {
        std::fs::remove_file(ws.beads().join(f)).ok();
    }
    std::fs::create_dir_all(ws.beads().join("dolt")).unwrap();

    let (r, code) = ws.doctor(&[]);
    assert_eq!(status(&r, "workspace manifest"), "error");
    assert_ne!(code, 0);

    let (r, _) = ws.doctor(&["--fix"]);
    assert_eq!(
        repair_outcome(&r, "workspace manifest").as_deref(),
        Some("unfixable"),
        "rewriting the manifest is a guess, and the wrong guess abandons a database"
    );
    assert!(
        exists(&ws.beads().join("workspace.json")),
        "--fix must not have touched the manifest"
    );
}

// ---------------------------------------------------------------------------
// Debris that is genuinely safe to delete
// ---------------------------------------------------------------------------

#[test]
fn legacy_queue_files_are_a_warning_and_fix_deletes_them() {
    let ws = Ws::new("mq");
    ws.write("mq/pr-17.json", "{}");
    ws.write("mq/pr-18.json", "{}");

    let (r, _) = ws.doctor(&[]);
    assert_eq!(status(&r, "legacy queue files"), "warning", "debris is untidy, not broken");

    let (r, _) = ws.doctor(&["--fix"]);
    assert_eq!(repair_outcome(&r, "legacy queue files").as_deref(), Some("fixed"));
    assert!(!exists(&ws.beads().join("mq")));
    // And it cleaned up the debris without touching anything that matters.
    assert!(exists(&ws.beads().join("workspace.json")));
    assert!(exists(&ws.beads().join("beads.db")));
}

/// A `.tmp` file is only debris once it is old enough that it cannot still be an
/// in-flight write-then-rename. A brand-new one belongs to somebody's `Locator::save`
/// happening right now, and deleting it makes their rename fail.
#[test]
fn a_temporary_that_may_still_be_in_flight_is_left_alone() {
    let ws = Ws::new("tmp");
    let tmp = ws.write("workspace.json.tmp", "{}");

    let (r, _) = ws.doctor(&[]);
    assert_eq!(
        status(&r, "interrupted writes"),
        "ok",
        "a fresh .tmp is a write in progress, not debris"
    );

    ws.doctor(&["--fix"]);
    assert!(exists(&tmp), "--fix must not delete a temporary that could be in flight");
}

// ---------------------------------------------------------------------------
// Issues are not debris
// ---------------------------------------------------------------------------

/// `--fix` may delete a lock file, because a lock file is bookkeeping. It may not
/// delete issues, because issues are the product — and this is a *heuristic*.
/// Upstream deletes them (after a prompt and a backup); doctor has no human in
/// the loop, so it hands over the ids and stops.
#[test]
fn fix_never_deletes_the_issues_it_thinks_are_test_data() {
    let ws = Ws::new("pollution");
    let (id, code) = ws.run(&["q", "test-scratch-issue"]);
    assert_eq!(code, 0, "quick-create failed: {id}");

    let (r, _) = ws.doctor(&[]);
    assert_eq!(
        status(&r, "test pollution"),
        "warning",
        "a test-titled issue is untidy, never an error"
    );
    let detail = finding(&r, "test pollution")["detail"].as_str().unwrap();
    assert!(detail.contains(&id), "the finding must name the issue: {detail}");

    let (r, _) = ws.doctor(&["--fix"]);
    assert_eq!(
        repair_outcome(&r, "test pollution").as_deref(),
        Some("unfixable"),
        "doctor --fix must not delete issues on a heuristic"
    );

    let (out, code) = ws.run(&["show", &id]);
    assert_eq!(code, 0, "--fix deleted the user's issue: {out}");
}

/// Real work is not flagged. A pollution check with false positives gets muted,
/// and a muted check finds nothing.
#[test]
fn real_work_is_not_mistaken_for_test_data() {
    let ws = Ws::new("realwork");
    for title in [
        "Fix the flaky integration test",
        "Add tests for the parser",
        "Template the release notes",
    ] {
        let (out, code) = ws.run(&["q", title]);
        assert_eq!(code, 0, "{out}");
    }
    let (r, _) = ws.doctor(&[]);
    assert_eq!(status(&r, "test pollution"), "ok");
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// `bd hook <name>` was replaced by `bd hooks run <name>`. The difference is one
/// letter, so the hook looks fine, `bd hooks list` looks fine, and every commit
/// prints "unknown command".
///
/// bd will not rewrite the file: it may be the user's, and `bd hooks install`
/// already refuses to touch a hook it did not write. Reporting it is the fix.
#[test]
fn a_hook_calling_the_removed_command_is_reported_but_never_rewritten() {
    let ws = Ws::new("hooks");
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws.0)
        .status();
    let Ok(st) = git else {
        eprintln!("skipping: git is not on PATH");
        return;
    };
    if !st.success() {
        eprintln!("skipping: git init failed");
        return;
    }

    let hooks = ws.0.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    let body = "#!/bin/sh\n# mine, hand-written\nmake lint || exit 1\nexec bd hook pre-commit\n";
    std::fs::write(&hook, body).unwrap();

    let (r, _) = ws.doctor(&[]);
    assert_eq!(status(&r, "legacy git hooks"), "warning");
    assert!(
        finding(&r, "legacy git hooks")["detail"]
            .as_str()
            .unwrap()
            .contains("pre-commit")
    );

    ws.doctor(&["--fix"]);
    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        body,
        "bd rewrote a hook it did not author"
    );
}

/// The hook this port installs itself contains the string `bd hooks run`, which
/// contains `bd hook`. A substring match would flag every correctly-installed
/// hook in existence.
#[test]
fn the_hook_we_install_is_not_flagged_as_legacy() {
    let ws = Ws::new("goodhook");
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws.0)
        .status();
    if !git.is_ok_and(|s| s.success()) {
        eprintln!("skipping: git is unavailable");
        return;
    }
    let (out, code) = ws.run(&["hooks", "install"]);
    assert_eq!(code, 0, "hooks install failed: {out}");

    let (r, _) = ws.doctor(&[]);
    assert_eq!(status(&r, "legacy git hooks"), "ok");
}
