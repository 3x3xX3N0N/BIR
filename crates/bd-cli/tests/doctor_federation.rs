//! The Federation family of `bd doctor`, through the real binary.
//!
//! This port has no peers, no remotes and no federation conflicts — see the
//! module docs on `doctor::checks::federation` for why the checks for those do
//! not exist. What it *does* have is `.beads/issues.jsonl`: the pre-commit hook
//! exports the database into it and the post-merge hook imports it back, so that
//! file is how issues travel between machines. These tests are about the ways
//! that transport rots, and — most importantly — about `--fix` not making it
//! worse.
//!
//! Every one of these runs offline by construction. No check in this family may
//! open a socket, and nothing here gives it the chance to.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

fn run(dir: &Path, args: &[&str]) -> (String, i32) {
    let out = bd()
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .env("BEADS_ACTOR", "tester")
        .output()
        .expect("run bd");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// A fresh workspace in its own directory.
fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bd-fed-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (out, code) = run(&dir, &["init", "--prefix", "fed"]);
    assert_eq!(code, 0, "init failed: {out}");
    dir
}

fn jsonl(dir: &Path) -> PathBuf {
    dir.join(".beads").join("issues.jsonl")
}

/// The whole doctor report, as JSON.
fn doctor(dir: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["--json", "doctor"];
    args.extend_from_slice(extra);
    let (out, _) = run(dir, &args);
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("doctor did not emit JSON ({e}): {out}"))
}

/// One check's finding, by name.
fn finding(report: &serde_json::Value, name: &str) -> serde_json::Value {
    report["checks"]
        .as_array()
        .expect("checks is an array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check named {name} in {report:#}"))
        .clone()
}

fn status(report: &serde_json::Value, name: &str) -> String {
    finding(report, name)["status"].as_str().unwrap().to_string()
}

/// Backdate a file's mtime. The staleness check reads the *recorded* timestamps
/// on this machine — the JSONL's mtime and the database's `updated_at` — and
/// never asks anyone else, so aging the file is the entire simulation.
fn backdate(path: &Path, days: u64) {
    let when = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();
}

fn add_issue(dir: &Path, title: &str) -> String {
    let (out, code) = run(dir, &["--json", "create", title]);
    assert_eq!(code, 0, "create failed: {out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    v["id"].as_str().expect("create returns an id").to_string()
}

fn export(dir: &Path) {
    let (out, code) = run(dir, &["export", "-o", jsonl(dir).to_str().unwrap()]);
    assert_eq!(code, 0, "export failed: {out}");
}

// ---------------------------------------------------------------------------
// Absence is not failure
// ---------------------------------------------------------------------------

/// A workspace with no federation has no federation *problem*. All three checks
/// must be quiet — and doctor must still exit 0, because a warning here would
/// fire on every clean workspace in existence and teach people to ignore the
/// whole report.
#[test]
fn a_plain_workspace_has_no_federation_findings() {
    let dir = workspace("clean");
    add_issue(&dir, "ordinary work");

    let report = doctor(&dir, &[]);
    for check in [
        "federation config",
        "federation sync staleness",
        "federation kv store",
    ] {
        assert_eq!(
            status(&report, check),
            "ok",
            "{check} must be quiet on a workspace that does not federate: {:#}",
            finding(&report, check)
        );
    }
    assert_eq!(report["ok"], true);
}

/// No `issues.jsonl` at all means the user does not carry issues in git. That is
/// an ordinary way to run beads, not a broken sync.
#[test]
fn no_jsonl_is_not_a_stale_sync() {
    let dir = workspace("nojsonl");
    add_issue(&dir, "local only");
    assert!(!jsonl(&dir).exists());

    assert_eq!(status(&doctor(&dir, &[]), "federation sync staleness"), "ok");
}

// ---------------------------------------------------------------------------
// Sync staleness
// ---------------------------------------------------------------------------

/// The bug this family exists for: the hooks stopped running, the export is
/// weeks behind, and nothing else in the program would ever say so. `bd ready`
/// still works. The issues just quietly stop leaving the machine.
#[test]
fn an_export_that_stopped_running_weeks_ago_is_a_warning_that_names_the_issues() {
    let dir = workspace("stale");
    export(&dir);
    backdate(&jsonl(&dir), 30);
    let id = add_issue(&dir, "written after the export stopped");

    let f = finding(&doctor(&dir, &[]), "federation sync staleness");
    assert_eq!(f["status"], "warning", "{f:#}");

    let detail = f["detail"].as_str().unwrap();
    assert!(
        detail.contains(&id),
        "the finding must name the issue that is not being exported, or it is a \
         bug report you cannot act on: {detail}"
    );
    assert!(
        f["fix"].as_str().unwrap().contains("bd hooks install"),
        "the fix should point at the thing that stops it happening again"
    );
}

/// A lag inside the ordinary edit-then-commit window is not a finding. The
/// pre-commit hook re-exports on every commit, so the JSONL is *supposed* to
/// trail the database between commits — warning on that would cry wolf on the
/// happy path, every single run.
#[test]
fn the_ordinary_gap_between_editing_and_committing_is_not_stale() {
    let dir = workspace("fresh");
    export(&dir);
    backdate(&jsonl(&dir), 2);
    add_issue(&dir, "edited just now, not committed yet");

    assert_eq!(status(&doctor(&dir, &[]), "federation sync staleness"), "ok");
}

/// A stale export is untidy, not broken. It must never fail the run — `bd doctor`
/// is meant to be usable in a git hook, and a nonzero exit here would block
/// commits over a file the commit itself is about to rewrite.
#[test]
fn staleness_never_fails_the_run() {
    let dir = workspace("exit");
    export(&dir);
    backdate(&jsonl(&dir), 60);
    add_issue(&dir, "late");

    let (_, code) = run(&dir, &["doctor"]);
    assert_eq!(code, 0, "a warning must not make doctor exit nonzero");
}

// ---------------------------------------------------------------------------
// --fix, and the data loss it must not cause
// ---------------------------------------------------------------------------

/// The happy repair: the database is ahead, the JSONL holds nothing we do not
/// have, so re-exporting is safe and puts the issues back where git will carry
/// them.
#[test]
fn fix_re_exports_when_nothing_can_be_lost() {
    let dir = workspace("fix");
    export(&dir);
    backdate(&jsonl(&dir), 30);
    let id = add_issue(&dir, "needs to reach the team");

    let report = doctor(&dir, &["--fix"]);
    let repair = report["repairs"]
        .as_array()
        .expect("repairs")
        .iter()
        .find(|r| r["check"] == "federation sync staleness")
        .unwrap_or_else(|| panic!("no repair recorded: {report:#}"))
        .clone();
    assert_eq!(repair["outcome"], "fixed", "{repair:#}");

    let text = std::fs::read_to_string(jsonl(&dir)).unwrap();
    assert!(
        text.contains(&id),
        "the repair must actually put the new issue into the file git carries"
    );
    assert_eq!(status(&doctor(&dir, &[]), "federation sync staleness"), "ok");
}

/// **The one that matters.** The hook pair is installed and removed together, so
/// "my export never ran" and "my import never ran" are the *same* workspace: a
/// pull can land a teammate's issues in the JSONL while the database has never
/// seen them. Re-exporting there would overwrite the file and destroy their work
/// — `--fix` would become the very bug it was run to cure.
///
/// So it must refuse, keep the file byte-for-byte, and say why.
#[test]
fn fix_refuses_to_overwrite_issues_the_database_has_never_imported() {
    let dir = workspace("noclobber");
    export(&dir);
    add_issue(&dir, "mine, made locally");

    // A teammate's issue, arriving the way a `git pull` would deliver it: as a
    // line in the JSONL that the database has never imported.
    let theirs = serde_json::json!({
        "_type": "issue",
        "id": "fed-theirs",
        "title": "landed by a pull, never imported",
        "status": "open",
        "priority": 2,
        "issue_type": "task",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    });
    let mut text = std::fs::read_to_string(jsonl(&dir)).unwrap();
    text.push_str(&format!("{theirs}\n"));
    std::fs::write(jsonl(&dir), &text).unwrap();
    backdate(&jsonl(&dir), 30);

    let before = std::fs::read_to_string(jsonl(&dir)).unwrap();

    let report = doctor(&dir, &["--fix"]);
    let repair = report["repairs"]
        .as_array()
        .expect("repairs")
        .iter()
        .find(|r| r["check"] == "federation sync staleness")
        .unwrap_or_else(|| panic!("no repair recorded: {report:#}"))
        .clone();

    assert_ne!(
        repair["outcome"], "fixed",
        "a repair that destroyed a teammate's issues must never report success: {repair:#}"
    );
    let msg = repair["message"].as_str().unwrap();
    assert!(
        msg.contains("fed-theirs") && msg.contains("import"),
        "the refusal has to name what would be lost and the command that saves it: {msg}"
    );

    let after = std::fs::read_to_string(jsonl(&dir)).unwrap();
    assert_eq!(
        before, after,
        "the file git carries must be byte-for-byte untouched"
    );
    assert!(after.contains("fed-theirs"), "their issue is still there");
}

// ---------------------------------------------------------------------------
// Federation configured in a build that has no federation
// ---------------------------------------------------------------------------

/// The silent one. `Config` is `#[serde(default)]` with no `deny_unknown_fields`,
/// so a peer URL written into `config.yaml` parses cleanly and is discarded. The
/// user believes they configured federation. They did not, and nothing else in
/// the program will ever mention it.
#[test]
fn federation_config_that_this_build_discards_is_reported() {
    let dir = workspace("cfg");
    let cfg = dir.join(".beads").join("config.yaml");
    let mut text = std::fs::read_to_string(&cfg).unwrap_or_default();
    text.push_str("federation:\n  remote: dolthub://acme/beads\n  sovereignty: T2\n");
    std::fs::write(&cfg, text).unwrap();

    let f = finding(&doctor(&dir, &[]), "federation config");
    assert_eq!(f["status"], "warning", "{f:#}");
    assert!(f["detail"].as_str().unwrap().contains("federation"));
    // Untidy, not broken.
    let (_, code) = run(&dir, &["doctor"]);
    assert_eq!(code, 0);
}

/// A `repos:` list is upstream's other federation surface, and it is dropped
/// just as silently.
#[test]
fn an_upstream_repos_list_is_reported_too() {
    let dir = workspace("repos");
    let cfg = dir.join(".beads").join("config.yaml");
    let mut text = std::fs::read_to_string(&cfg).unwrap_or_default();
    text.push_str("repos:\n  - path: ../other\n");
    std::fs::write(&cfg, text).unwrap();

    assert_eq!(status(&doctor(&dir, &[]), "federation config"), "warning");
}

// ---------------------------------------------------------------------------
// Key-value
// ---------------------------------------------------------------------------

/// `bd config set` takes any key, including the `kv.` namespace upstream's
/// key-value store lives in. Here that namespace is a dead end: `bd kv` is
/// unimplemented, `bd config unset` is unimplemented, and `bd export` carries
/// issues rather than config — so the data can be neither read back, removed,
/// nor transported. Saying "1 KV pair stored" and calling it `ok`, as upstream
/// does, would be a lie in this build.
#[test]
fn kv_keys_this_build_cannot_reach_are_reported() {
    let dir = workspace("kv");
    let (out, code) = run(&dir, &["config", "set", "kv.memory.plan", "ship it"]);
    assert_eq!(code, 0, "config set failed: {out}");

    let f = finding(&doctor(&dir, &[]), "federation kv store");
    assert_eq!(f["status"], "warning", "{f:#}");
    assert!(
        f["detail"].as_str().unwrap().contains("kv.memory.plan"),
        "name the key, or the user cannot find it: {f:#}"
    );
}

/// Ordinary config keys are not key-value data and must not be mistaken for it.
#[test]
fn ordinary_config_keys_are_not_kv_data() {
    let dir = workspace("cfgkeys");
    let (_, code) = run(&dir, &["config", "set", "jira.url", "https://example.invalid"]);
    assert_eq!(code, 0);

    assert_eq!(status(&doctor(&dir, &[]), "federation kv store"), "ok");
}
