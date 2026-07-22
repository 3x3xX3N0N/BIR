//! The Git Integration family, against a real git repository and the real binary.
//!
//! Two properties are worth this much machinery:
//!
//! * **`--fix` untracks; it never deletes.** The repair for a committed database
//!   is `git rm --cached`, and the one way to get that wrong is to drop the
//!   `--cached`. That mistake destroys the user's issue database, and it would
//!   pass any test that only asserted "doctor is green afterwards". So the test
//!   asserts the file is *still on disk*.
//! * **Doctor and `bd hooks install` agree about what a beads hook is.** Doctor
//!   holds its own copy of the marker string (the real one is private to
//!   `commands::setup`). Rather than trust the copy, install a real hook with the
//!   real command and make doctor recognise it — the day the two drift, this
//!   fails.
//!
//! Everything here needs a `git` binary. Where there isn't one, the tests skip:
//! a machine without git is exactly the machine whose beads must still work.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Repo(PathBuf);

impl Repo {
    /// A git repository with a beads workspace in it, and nothing committed yet.
    fn new(tag: &str) -> Option<Repo> {
        if !git_available() {
            eprintln!("skipping {tag}: no git");
            return None;
        }
        let p = std::env::temp_dir().join(format!(
            "bd-doctor-git-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        let repo = Repo(std::fs::canonicalize(&p).unwrap());

        repo.git(&["init", "-q"]);
        // A repo with no identity cannot commit, and a repo that signs cannot
        // commit on a machine with no key. Neither is what is under test.
        repo.git(&["config", "user.email", "t@example.com"]);
        repo.git(&["config", "user.name", "t"]);
        repo.git(&["config", "commit.gpgsign", "false"]);

        assert_eq!(repo.bd(&["init", "--prefix", "t"]).1, 0, "bd init");
        Some(repo)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn git(&self, args: &[&str]) -> (String, i32) {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.0)
            .output()
            .expect("run git");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn bd(&self, args: &[&str]) -> (String, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", self.0.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            // The hooks check asks whether `bd` is on PATH, because the installed
            // hook does exactly that and silently does nothing when it is not.
            // Under `cargo test` the binary lives in target/debug and PATH knows
            // nothing about it, so put it there — which is also what a user who
            // installed bd has.
            .env("PATH", path_with_bd())
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    /// `bd doctor --json`, plus the exit code.
    fn doctor(&self, args: &[&str]) -> (Value, i32) {
        let mut a = vec!["doctor", "--json"];
        a.extend_from_slice(args);
        let (out, code) = self.bd(&a);
        let v: Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("bd doctor did not emit JSON ({e}): {out}"));
        (v, code)
    }

    fn commit_everything(&self) {
        self.git(&["add", "-A"]);
        let (out, code) = self.git(&["commit", "-q", "-m", "everything"]);
        assert_eq!(code, 0, "git commit: {out}");
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A canonicalized temp directory that removes itself on drop — for the tests
/// that need a NON-git workspace (so they cannot use [`Repo`], which runs
/// `git init`). Its name carries the thread id as well as the pid so two tests
/// in the same binary never collide on a shared pid, and the `Drop` guard means
/// a PANICKING test cleans up too, instead of leaking its workspace into
/// `%TEMP%` (bead warden-2ol — the suite had left ~12k `bd-*` dirs behind).
struct TempWs(PathBuf);

impl TempWs {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "bd-doctor-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        TempWs(std::fs::canonicalize(&p).unwrap())
    }
}

impl Drop for TempWs {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn path_with_bd() -> std::ffi::OsString {
    let bin = Path::new(env!("CARGO_BIN_EXE_bd")).parent().unwrap().to_path_buf();
    let mut dirs = vec![bin];
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(dirs).unwrap()
}

/// One check, by name. Panics rather than returning `None`: a check that
/// vanished from the registry is the failure, and a test that skips silently on
/// it is worse than no test.
fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .expect("checks[]")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check named {name} in {report:#}"))
}

/// The `--json` status string.
///
/// Note the spelling: serde renames `Status::Warn` to `"warning"`, while
/// `Status::as_str` — used nowhere in the JSON — says `"warning"`, and the
/// `counts` object uses `"warning"` again. Three spellings of one status; this
/// is the one that appears on a check.
fn status(report: &Value, name: &str) -> String {
    check(report, name)["status"].as_str().unwrap().to_string()
}

/// Every finding this family produced, so a new noisy check cannot be added
/// without a test noticing.
fn git_findings(report: &Value) -> Vec<(String, String)> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["category"] == "git")
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                c["status"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------

/// The headline. A committed database turns every `git pull` into a binary
/// conflict; `--fix` must untrack it and must not touch it.
#[test]
fn a_committed_database_is_found_untracked_and_not_deleted() {
    let Some(repo) = Repo::new("tracked") else { return };

    // Give the workspace a database with something in it, then commit the lot —
    // the exact mistake, made the exact way people make it.
    assert_eq!(repo.bd(&["create", "a task", "-p", "1"]).1, 0);
    assert!(repo.path(".beads/beads.db").is_file(), "the db should exist");
    repo.commit_everything();

    let tracked_before = repo.git(&["ls-files", "--", ".beads"]).0;
    assert!(tracked_before.contains("beads.db"), "the db is committed: {tracked_before}");

    let (before, code) = repo.doctor(&[]);
    assert_eq!(code, 0, "a tracked db is untidy, not broken — it must not fail the run");
    assert_eq!(status(&before, "git tracked runtime files"), "warning");
    assert!(
        check(&before, "git tracked runtime files")["detail"]
            .as_str()
            .unwrap()
            .contains("beads.db"),
        "the finding must name the file"
    );
    // Nothing ignores it, which is how it got committed in the first place.
    assert_eq!(status(&before, "git ignore rules"), "warning");

    let (fixed, _) = repo.doctor(&["--fix"]);
    let repairs = fixed["repairs"].as_array().expect("repairs[]");
    let untrack = repairs
        .iter()
        .find(|r| r["check"] == "git tracked runtime files")
        .expect("the tracked-files check must have been repaired");
    assert_eq!(untrack["outcome"], "fixed", "{untrack:#}");

    // The one assertion this whole file exists for. `git rm` without `--cached`
    // would pass every other check in this test and destroy the user's database.
    assert!(
        repo.path(".beads/beads.db").is_file(),
        "--fix DELETED THE DATABASE. `git rm --cached` untracks; `git rm` destroys"
    );
    // And the data in it survived, not just the inode.
    assert_eq!(repo.bd(&["list"]).1, 0, "the database still opens after --fix");

    let tracked_after = repo.git(&["ls-files", "--", ".beads"]).0;
    assert!(
        !tracked_after.contains("beads.db"),
        "the db is still tracked after --fix: {tracked_after}"
    );

    // And it stays fixed: the ignore rule is what stops the next `git add -A`
    // from putting it straight back.
    let ignore = std::fs::read_to_string(repo.path(".beads/.gitignore")).expect(".beads/.gitignore");
    assert!(ignore.contains("beads.db"), "--fix must write the pattern too: {ignore}");

    let (after, code) = repo.doctor(&[]);
    assert_eq!(code, 0);
    assert_eq!(status(&after, "git tracked runtime files"), "ok");
    assert_eq!(status(&after, "git ignore rules"), "ok");

    repo.commit_everything();
    assert_eq!(repo.git(&["status", "--porcelain"]).0, "", "the tree is clean afterwards");
}

/// An ignore rule anywhere git honours it is an ignore rule. Checking only
/// `.beads/.gitignore` — which is what upstream does — warns at users whose
/// setup is already correct.
#[test]
fn an_ignore_rule_in_the_project_gitignore_counts() {
    let Some(repo) = Repo::new("rootignore") else { return };
    std::fs::write(repo.path(".gitignore"), ".beads/*.db\n.beads/*.db-*\n").unwrap();

    let (report, code) = repo.doctor(&[]);
    assert_eq!(code, 0);
    assert_eq!(
        status(&report, "git ignore rules"),
        "ok",
        "a rule in the project .gitignore covers the database just as well"
    );
}

/// Seam rule 4, and the sentence at the top of the module: "you are not using
/// git" is not a problem with your workspace. Not one finding in this family may
/// be yellow just because there is no repository.
#[test]
fn outside_a_git_repository_the_family_is_silent() {
    let ws = TempWs::new("nogit");
    let dir = &ws.0;

    let bd = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    };
    assert_eq!(bd(&["init", "--prefix", "t"]).1, 0);

    let (out, code) = bd(&["doctor", "--json"]);
    assert_eq!(code, 0);
    let report: Value = serde_json::from_str(&out).unwrap();

    let findings = git_findings(&report);
    assert!(!findings.is_empty(), "the git family produced nothing at all");
    // `n/a` for the checks that need a repository, and `ok` for the one that
    // doesn't — `git conflict markers` reads `.beads/*.jsonl` directly, because
    // conflict markers outlive the merge that wrote them. Either way: silent.
    let noisy: Vec<&(String, String)> = findings
        .iter()
        .filter(|(_, s)| s != "n/a" && s != "ok")
        .collect();
    assert!(
        noisy.is_empty(),
        "not using git is not a fault, but these fired anyway: {noisy:?}"
    );
    // `ws` removes itself on drop (including if an assert above panicked).
}

/// Conflict markers are the one thing in this family that is genuinely *broken*:
/// the file is not valid JSONL, and any import of it fails or half-succeeds.
/// Error, and a nonzero exit.
#[test]
fn unresolved_conflict_markers_are_an_error() {
    let Some(repo) = Repo::new("conflict") else { return };
    std::fs::write(
        repo.path(".beads/issues.jsonl"),
        "<<<<<<< HEAD\n{\"id\":\"t-1\"}\n=======\n{\"id\":\"t-2\"}\n>>>>>>> theirs\n",
    )
    .unwrap();

    let (report, code) = repo.doctor(&[]);
    assert_eq!(status(&report, "git conflict markers"), "error");
    assert_eq!(code, 1, "a determined, broken workspace exits nonzero");
    assert!(
        check(&report, "git conflict markers")["detail"]
            .as_str()
            .unwrap()
            .contains("issues.jsonl")
    );

    // No automatic repair, and --fix must say so rather than claim a fix.
    let (fixed, _) = repo.doctor(&["--fix"]);
    let r = fixed["repairs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["check"] == "git conflict markers")
        .expect("conflict markers should appear in repairs");
    assert_eq!(r["outcome"], "unfixable", "resolving a merge is not doctor's to do");
}

/// A clean `.beads/issues.jsonl` is not a conflicted one. The marker match is
/// exact for a reason.
#[test]
fn a_healthy_workspace_is_green_across_the_family() {
    let Some(repo) = Repo::new("healthy") else { return };
    std::fs::write(repo.path(".gitignore"), ".beads/beads.db*\n").unwrap();
    assert_eq!(repo.bd(&["create", "a task", "-p", "1"]).1, 0);
    std::fs::write(repo.path(".beads/issues.jsonl"), "{\"id\":\"t-1\",\"title\":\"a\"}\n").unwrap();
    repo.commit_everything();

    let (report, code) = repo.doctor(&[]);
    assert_eq!(code, 0);
    let noisy: Vec<(String, String)> = git_findings(&report)
        .into_iter()
        .filter(|(_, s)| s != "ok")
        .collect();
    assert!(noisy.is_empty(), "a healthy workspace produced: {noisy:?}");
}

/// The mirror image of the tracked-database bug: the one file that *should* be
/// in git, and is not. A fresh clone of this repository gets no issues at all.
#[test]
fn an_untracked_issue_file_is_a_warning_but_an_absent_one_is_not() {
    let Some(repo) = Repo::new("issuedata") else { return };

    // Nothing exported yet. Absence is not failure.
    let (before, _) = repo.doctor(&[]);
    assert_eq!(status(&before, "git tracked issue data"), "ok");

    std::fs::write(repo.path(".beads/issues.jsonl"), "{\"id\":\"t-1\"}\n").unwrap();
    let (untracked, _) = repo.doctor(&[]);
    assert_eq!(
        status(&untracked, "git tracked issue data"),
        "warning",
        "the issue file exists and git does not have it"
    );

    repo.commit_everything();
    let (tracked, _) = repo.doctor(&[]);
    assert_eq!(status(&tracked, "git tracked issue data"), "ok");
}

/// Doctor keeps its own copy of the hook marker, because the real one is private
/// to `commands::setup`. This is the test that keeps the copy honest: install the
/// real hook with the real command, and make doctor recognise it.
#[test]
fn doctor_recognises_the_hooks_that_bd_hooks_install_actually_writes() {
    let Some(repo) = Repo::new("hooks") else { return };

    // Not installed is not a fault — hooks are opt-in.
    let (before, _) = repo.doctor(&[]);
    assert_eq!(status(&before, "git hooks"), "ok");
    assert_eq!(check(&before, "git hooks")["message"], "no beads hooks installed");

    let (out, code) = repo.bd(&["hooks", "install"]);
    assert_eq!(code, 0, "bd hooks install: {out}");

    let (after, _) = repo.doctor(&[]);
    assert_eq!(
        status(&after, "git hooks"),
        "ok",
        "doctor did not recognise the hooks bd just installed — the marker has drifted: {:#}",
        check(&after, "git hooks")
    );
    let msg = check(&after, "git hooks")["message"].as_str().unwrap();
    assert!(msg.contains("pre-commit") && msg.contains("post-merge"), "{msg}");
}

/// A hook carrying bd's marker but calling a command bd no longer has runs on
/// every commit and does nothing but print an error into it.
#[test]
fn a_stale_beads_hook_is_a_warning() {
    let Some(repo) = Repo::new("stalehook") else { return };
    let hooks = repo.path(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(
        hooks.join("pre-commit"),
        "#!/bin/sh\n# beads-managed-hook\nexec bd hook pre-commit\n",
    )
    .unwrap();

    let (report, _) = repo.doctor(&[]);
    assert_eq!(status(&report, "git hooks"), "warning");
    let detail = check(&report, "git hooks")["detail"].as_str().unwrap();
    assert!(detail.contains("stale"), "{detail}");
}

/// A hook beads did not write is not beads' business, and `bd hooks install`
/// already refuses to touch it. Doctor must not warn about it either.
#[test]
fn a_foreign_hook_is_left_alone_and_not_warned_about() {
    let Some(repo) = Repo::new("foreignhook") else { return };
    let hooks = repo.path(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexec cargo fmt --check\n").unwrap();

    let (report, _) = repo.doctor(&[]);
    assert_eq!(
        status(&report, "git hooks"),
        "ok",
        "somebody else's pre-commit hook is not a beads problem"
    );
}

/// An unfinished merge in the beads data: git is holding both sides, and the
/// database and the text form disagree about what the issues are.
#[test]
fn an_unmerged_beads_file_is_an_error() {
    let Some(repo) = Repo::new("unmerged") else { return };
    std::fs::write(repo.path(".gitignore"), ".beads/beads.db*\n").unwrap();
    std::fs::write(repo.path(".beads/issues.jsonl"), "{\"id\":\"t-1\"}\n").unwrap();
    repo.commit_everything();

    // Two branches touching the same line of the same file: the ordinary way a
    // beads workspace ends up mid-merge.
    repo.git(&["checkout", "-q", "-b", "theirs"]);
    std::fs::write(repo.path(".beads/issues.jsonl"), "{\"id\":\"t-2\"}\n").unwrap();
    repo.commit_everything();
    repo.git(&["checkout", "-q", "-"]);
    std::fs::write(repo.path(".beads/issues.jsonl"), "{\"id\":\"t-3\"}\n").unwrap();
    repo.commit_everything();

    let (_, code) = repo.git(&["merge", "theirs"]);
    assert_ne!(code, 0, "the merge was supposed to conflict");

    let (report, code) = repo.doctor(&[]);
    assert_eq!(status(&report, "git unmerged files"), "error");
    assert_eq!(code, 1);
    assert!(
        check(&report, "git unmerged files")["detail"]
            .as_str()
            .unwrap()
            .contains("issues.jsonl")
    );

    // And the family survives the half-merged repo rather than falling over in
    // it: no check may report `error` for a reason it did not determine.
    for (name, st) in git_findings(&report) {
        if name != "git unmerged files" && name != "git conflict markers" {
            assert_ne!(st, "error", "{name} errored on a mid-merge repo");
        }
    }

    repo.git(&["merge", "--abort"]);
}

/// `--fix` must not act on a check that never ran. `repair` is called for every
/// finding that is not `Ok` — and `Finding::unknown` is a *warning*, so a check
/// whose precondition was missing still gets its repair invoked. It must decline.
#[test]
fn fix_outside_a_repository_repairs_nothing() {
    let ws = TempWs::new("nofix");
    let dir = &ws.0;

    let bd = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    };
    assert_eq!(bd(&["init", "--prefix", "t"]).1, 0);

    let (out, code) = bd(&["doctor", "--fix", "--json"]);
    assert_eq!(code, 0);
    let report: Value = serde_json::from_str(&out).unwrap();

    // Nothing in this family may claim to have fixed anything, and no .gitignore
    // may appear in a workspace that has no git to ignore anything for.
    for r in report["repairs"].as_array().unwrap_or(&vec![]) {
        assert_ne!(r["outcome"], "fixed", "repaired something outside a repo: {r:#}");
    }
    assert!(!dir.join(".beads/.gitignore").exists(), "--fix wrote a .gitignore with no git");
    // `ws` removes itself on drop, even if an assert above panicked.
}
