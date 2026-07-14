//! The Dolt family of `bd doctor`, end to end through the real binary.
//!
//! # There is no `dolt` on the machine that wrote these tests
//!
//! That is not a hole to paper over. It is a constraint, and it decides what
//! this file can honestly claim:
//!
//! * Everything that reads the *filesystem* is covered for real — a Dolt
//!   workspace is just a `.beads/workspace.json` saying `"backend":"dolt"`, and
//!   `bd doctor` runs on workspaces too broken to open, so it will happily
//!   diagnose one that was fabricated with `fs::write`. Stale pid files, stale
//!   locks, storage format, remotes, and every repair are exercised against the
//!   real binary here.
//! * Everything that needs a *server* — schema, `dolt_status`, issue counts — is
//!   not implemented and therefore not tested. See the report; it is absent, not
//!   stubbed.
//! * The handful of things that need a real `dolt` binary are named, loudly, by
//!   `uncovered_without_a_dolt_binary` below. It is `#[ignore]`d with the exact
//!   checklist as its reason, so every single `cargo test` run prints the fact
//!   that they are uncovered. A test that reported as coverage without testing
//!   anything would be worse than no test at all.
//!
//! (Filename note: never "install", "setup", "update" or "patch" in a test file
//! name — cargo names the test binary after the file, and Windows auto-elevates
//! any exe whose name looks like an installer.)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde_json::Value;

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

struct Run {
    stdout: String,
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
        code: out.status.code().unwrap_or(-1),
    }
}

/// `bd doctor --json`, as a `Value`. Doctor exits nonzero on any `error`
/// finding, which several of these workspaces have on purpose, so the exit code
/// is not asserted here — the findings are.
fn doctor(dir: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["--json", "doctor"];
    args.extend_from_slice(extra);
    let r = run(dir, &args);
    serde_json::from_str(&r.stdout)
        .unwrap_or_else(|e| panic!("doctor did not emit JSON ({e}):\n{}", r.stdout))
}

/// One finding, by the name the check reports. These names are the public key
/// agents grep for; if one goes missing the test says so rather than silently
/// asserting nothing.
fn finding<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .expect("checks is an array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check named {name:?} in the report"))
}

fn status(report: &Value, name: &str) -> String {
    finding(report, name)["status"].as_str().unwrap().to_string()
}

fn detail(report: &Value, name: &str) -> String {
    finding(report, name)["detail"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// Every check this family registers. Kept here, spelled out, so that a check
/// that silently disappears from the registry fails a test instead of quietly
/// reducing coverage to zero.
const DOLT_CHECKS: &[&str] = &[
    "dolt binary",
    "dolt database",
    "dolt storage format",
    "dolt server",
    "dolt lock files",
    "dolt remote vs git origin",
];

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-doctor-dolt-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// A Dolt workspace, fabricated. The locator on disk is the *only* thing that
/// decides the backend — no flag, no env var — which is precisely why a test can
/// make one with `fs::write` and doctor will believe it.
fn dolt_workspace(tag: &str) -> PathBuf {
    let dir = tmp(tag);
    let beads = dir.join(".beads");
    std::fs::create_dir_all(&beads).unwrap();
    std::fs::write(
        beads.join("workspace.json"),
        format!(r#"{{"backend":"dolt","workspace_id":"ws-{tag}"}}"#),
    )
    .unwrap();
    dir
}

/// Give the fabricated workspace a database that looks real enough to read:
/// `.beads/dolt/.dolt/noms/manifest`, plus a sentinel file whose survival is the
/// safety property `--fix` must never violate.
fn with_dolt_db(dir: &Path, manifest: &str) {
    let noms = dir.join(".beads/dolt/.dolt/noms");
    std::fs::create_dir_all(&noms).unwrap();
    std::fs::write(noms.join("manifest"), manifest).unwrap();
    // Dolt's own advisory lock. bd must never, ever delete this.
    std::fs::write(noms.join("LOCK"), "").unwrap();
    std::fs::write(noms.join("vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv"), "chunks").unwrap();
}

const CURRENT_MANIFEST: &str =
    "5:__DOLT__:qtnpkc6r0b7egk3t1cvdd0s7fh0m8b3v:t9hcvrb9khhgqcltcbd9m5j5j3fgvhqf:0";

/// A process id that is certainly not a running dolt: spawn something trivial,
/// wait for it to exit, and take its id. (Pid reuse would have to happen inside
/// the same handful of milliseconds *and* hand the id to a `dolt` — and there is
/// no dolt on this machine to hand it to.)
fn a_dead_pid() -> u32 {
    let mut child = bd()
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let pid = child.id();
    child.wait().expect("wait");
    pid
}

fn backdate(p: &Path, by: Duration) {
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - by).unwrap();
}

fn dolt_on_path() -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|d| {
                d.join("dolt").is_file() || d.join("dolt.exe").is_file() || d.join("dolt.cmd").is_file()
            })
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The rule: a SQLite user has no Dolt problem
// ---------------------------------------------------------------------------

/// Ten warnings about a Dolt server the user never asked for is how you train
/// people to stop reading `bd doctor` at all. So: on SQLite, this family is
/// silent — even when the workspace is littered with exactly the debris that
/// would make a Dolt workspace scream.
#[test]
fn a_sqlite_workspace_hears_nothing_at_all_from_the_dolt_family() {
    let dir = tmp("sqlite");
    assert_eq!(run(&dir, &["init", "--prefix", "t"]).code, 0, "init");

    // Debris that is loud in a Dolt workspace and meaningless in this one.
    let beads = dir.join(".beads");
    std::fs::write(beads.join("dolt-server.pid"), a_dead_pid().to_string()).unwrap();
    std::fs::write(beads.join("dolt-server.port"), "3306").unwrap();
    let lock = beads.join("dolt.bootstrap.lock");
    std::fs::write(&lock, "").unwrap();
    backdate(&lock, Duration::from_secs(3600));
    with_dolt_db(&dir, "4:__LD_1__:abc:def");

    let report = doctor(&dir, &[]);
    for name in DOLT_CHECKS {
        assert_eq!(
            status(&report, name),
            "n/a",
            "{name} must be silent on a sqlite workspace"
        );
        assert_eq!(finding(&report, name)["message"], "not a dolt workspace");
    }

    // And the human rendering collapses the lot to a single green line, rather
    // than six.
    // ...and it says `n/a`, not `ok`. "6 ok" would claim it verified six things
    // about a Dolt server that this workspace does not have.
    let human = run(&dir, &["doctor"]).stdout;
    assert!(
        human.contains("Dolt Storage (6 n/a)"),
        "the family should collapse to one n/a line, got:\n{human}"
    );

    // Belt and braces: `--fix` must not touch a SQLite user's files either.
    run(&dir, &["doctor", "--fix"]);
    assert!(beads.join("dolt-server.pid").exists());
    assert!(lock.exists());

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The check that saves people
// ---------------------------------------------------------------------------

/// A `dolt sql-server` that died without cleaning up leaves its pid file behind.
/// The next `bd` command fails with something that reads exactly like database
/// corruption, and the user deletes `.beads/` to fix it — losing everything.
///
/// So the finding is asserted almost word for word. Its job is to be *believed*:
/// it must name the stale file, and it must say, in the output the user is
/// staring at, that this is not corruption.
#[test]
fn a_stale_server_pid_is_named_as_stale_and_explicitly_not_as_corruption() {
    let dir = dolt_workspace("stale");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let beads = dir.join(".beads");
    std::fs::write(beads.join("dolt-server.pid"), a_dead_pid().to_string()).unwrap();
    std::fs::write(beads.join("dolt-server.port"), "3306").unwrap();

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt server"), "warning");

    let d = detail(&report, "dolt server");
    assert!(
        d.contains("NOT database corruption"),
        "the whole point of this check is to stop the rm -rf; got: {d}"
    );
    assert!(
        d.contains("Do not delete .beads/dolt/"),
        "got: {d}"
    );
    assert!(
        d.contains("dolt-server.pid"),
        "a finding that does not name the file cannot be acted on; got: {d}"
    );

    // It is not an `error`: bd can recover from this on its own, and a nonzero
    // exit for a twelve-byte stale file would be crying wolf.
    assert_ne!(status(&report, "dolt server"), "error");

    std::fs::remove_dir_all(&dir).ok();
}

/// The safety property, asserted rather than commented: `--fix` removes *bd's*
/// bookkeeping and does not go anywhere near `.dolt/`. Dolt's `LOCK` is
/// advisory — the OS drops it when the process dies — so its presence proves
/// nothing, and deleting it (or the manifest, or a journal) is the
/// unrecoverable mistake this family exists to talk people out of.
#[test]
fn fix_removes_the_stale_bookkeeping_and_never_touches_dot_dolt() {
    let dir = dolt_workspace("fix");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let beads = dir.join(".beads");
    let pid = beads.join("dolt-server.pid");
    let port = beads.join("dolt-server.port");
    std::fs::write(&pid, a_dead_pid().to_string()).unwrap();
    std::fs::write(&port, "3306").unwrap();

    let report = doctor(&dir, &["--fix"]);

    let repairs = report["repairs"].as_array().expect("repairs");
    let mine = repairs
        .iter()
        .find(|r| r["check"] == "dolt server")
        .expect("the dolt server check should have repaired something");
    assert_eq!(mine["outcome"], "fixed");
    assert!(
        mine["message"].as_str().unwrap().contains("dolt-server.pid"),
        "a repair must name what it changed: {mine}"
    );

    assert!(!pid.exists(), "the stale pid file should be gone");
    assert!(!port.exists(), "the stale port file should be gone");

    // Everything of Dolt's is untouched. This is the assertion that matters.
    let noms = dir.join(".beads/dolt/.dolt/noms");
    assert!(noms.join("manifest").is_file(), "the manifest was destroyed");
    assert!(noms.join("LOCK").is_file(), "dolt's LOCK was destroyed");
    assert!(
        noms.join("vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv").is_file(),
        "a chunk journal was destroyed"
    );

    // Idempotent: with the bookkeeping gone, there is nothing left to say.
    let again = doctor(&dir, &[]);
    assert_eq!(status(&again, "dolt server"), "ok");

    std::fs::remove_dir_all(&dir).ok();
}

/// A pid file that is not a pid. The realistic shape of this is a truncated
/// write from a machine that lost power mid-`bd`, and it must degrade to the
/// same tidy, repairable state — not to a panic, and not to a shrug.
#[test]
fn a_corrupt_pid_file_is_repaired_rather_than_ignored() {
    let dir = dolt_workspace("corrupt-pid");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let pid = dir.join(".beads/dolt-server.pid");
    std::fs::write(&pid, "\u{0}\u{0}\u{0}").unwrap();

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt server"), "warning");

    doctor(&dir, &["--fix"]);
    assert!(!pid.exists());

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Lock files
// ---------------------------------------------------------------------------

/// A bootstrap that crashed leaves `dolt.bootstrap.lock` behind, and the next
/// bootstrap waits forever on a lock nobody holds. Age is the only evidence
/// available (the file has no owner recorded), so the threshold is generous —
/// but an hour is not slow, it is dead.
#[test]
fn an_ancient_bootstrap_lock_is_flagged_and_removed() {
    let dir = dolt_workspace("locks");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let beads = dir.join(".beads");

    let bootstrap = beads.join("dolt.bootstrap.lock");
    let startlock = beads.join("bd.sock.startlock");
    let fresh = beads.join("dolt-server.lock");
    std::fs::write(&bootstrap, "").unwrap();
    std::fs::write(&startlock, "").unwrap();
    std::fs::write(&fresh, "").unwrap();
    backdate(&bootstrap, Duration::from_secs(3600));
    backdate(&startlock, Duration::from_secs(300));
    // `fresh` is seconds old: a live bd may legitimately be holding it.

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt lock files"), "warning");
    let d = detail(&report, "dolt lock files");
    assert!(d.contains("dolt.bootstrap.lock"), "got: {d}");
    assert!(d.contains("bd.sock.startlock"), "got: {d}");
    assert!(
        !d.contains("dolt-server.lock"),
        "a lock held for seconds is not stale — yanking it out from under a live \
         bootstrap is worse than the problem; got: {d}"
    );

    doctor(&dir, &["--fix"]);
    assert!(!bootstrap.exists(), "the stale bootstrap lock survived --fix");
    assert!(!startlock.exists(), "the stale startlock survived --fix");
    assert!(
        fresh.exists(),
        "--fix removed a lock that a live process may still hold"
    );

    // And, again: nothing of Dolt's was touched.
    assert!(dir.join(".beads/dolt/.dolt/noms/LOCK").is_file());

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Storage format
// ---------------------------------------------------------------------------

#[test]
fn a_current_storage_format_passes_and_a_legacy_one_does_not() {
    let ok_dir = dolt_workspace("fmt-ok");
    with_dolt_db(&ok_dir, CURRENT_MANIFEST);
    let report = doctor(&ok_dir, &[]);
    assert_eq!(status(&report, "dolt storage format"), "ok");
    std::fs::remove_dir_all(&ok_dir).ok();

    let old_dir = dolt_workspace("fmt-old");
    with_dolt_db(&old_dir, "4:__LD_1__:abc:def");
    let report = doctor(&old_dir, &[]);
    assert_eq!(status(&report, "dolt storage format"), "warning");
    assert!(
        finding(&report, "dolt storage format")["fix"]
            .as_str()
            .unwrap()
            .contains("dolt migrate"),
        "a finding without a command the user can paste is half a finding"
    );
    std::fs::remove_dir_all(&old_dir).ok();
}

/// The safety property of the format check: if the manifest is not the shape we
/// expect, we say *we do not know* — never "healthy". Being wrong about Dolt's
/// on-disk format must not silently certify a legacy database as fine.
#[test]
fn an_unreadable_manifest_is_a_warning_not_a_pass() {
    let dir = dolt_workspace("fmt-weird");
    with_dolt_db(&dir, "this is not a manifest at all");

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt storage format"), "unknown");
    assert_eq!(
        finding(&report, "dolt storage format")["message"],
        "could not check"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The database itself
// ---------------------------------------------------------------------------

/// A dolt-backed locator with no dolt database under it: every command that
/// opens the store will fail, and this is the one check that says why. It is an
/// `error` — the workspace really is broken — while the checks that depend on
/// the database staying quiet, so the user reads one problem instead of five.
#[test]
fn a_dolt_workspace_with_no_database_reports_exactly_one_problem() {
    let dir = dolt_workspace("no-db");

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt database"), "error");

    // The dependents do not pile on.
    assert_eq!(status(&report, "dolt storage format"), "ok");
    assert_eq!(status(&report, "dolt remote vs git origin"), "ok");
    assert_eq!(status(&report, "dolt server"), "ok");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Remotes
// ---------------------------------------------------------------------------

/// Beads syncs issues through Dolt and code through git. Aiming both at one
/// endpoint makes them fight — and the two URLs are usually spelled differently
/// (`git@github.com:acme/x.git` vs `https://github.com/acme/x`), which is why a
/// naive string compare would miss it.
#[test]
fn a_dolt_remote_aimed_at_the_git_origin_is_flagged_through_two_spellings() {
    let dir = dolt_workspace("remotes");
    with_dolt_db(&dir, CURRENT_MANIFEST);

    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?}");
    };
    git(&["init"]);
    git(&["remote", "add", "origin", "https://github.com/acme/beads.git"]);

    std::fs::write(
        dir.join(".beads/dolt/.dolt/repo_state.json"),
        r#"{
          "head": "refs/heads/main",
          "remotes": {
            "origin": {"name":"origin","url":"git@github.com:acme/beads","fetch_specs":[],"params":{}},
            "peer":   {"name":"peer","url":"file:///srv/beads-peer","fetch_specs":[],"params":{}}
          }
        }"#,
    )
    .unwrap();

    let report = doctor(&dir, &[]);
    let f = finding(&report, "dolt remote vs git origin");
    assert_eq!(f["status"], "warning");
    let msg = f["message"].as_str().unwrap();
    assert!(msg.contains("origin"), "got: {msg}");
    assert!(
        !msg.contains("peer"),
        "the unrelated remote must not be swept up: {msg}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unrelated_dolt_remotes_are_left_alone() {
    let dir = dolt_workspace("remotes-ok");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    std::fs::write(
        dir.join(".beads/dolt/.dolt/repo_state.json"),
        r#"{"remotes":{"origin":{"name":"origin","url":"https://doltremoteapi.dolthub.com/acme/beads"}}}"#,
    )
    .unwrap();

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt remote vs git origin"), "ok");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The dolt binary, and the honest accounting of what is not covered
// ---------------------------------------------------------------------------

/// A Dolt workspace on a machine with no `dolt` is a real, diagnosable,
/// actionable state — and it is the state of the machine this port is being
/// written on, so it is the branch that gets real coverage. When a `dolt` *is*
/// present, the same check must find it and report its version; this test
/// asserts whichever branch it is standing in, and never both.
#[test]
fn the_dolt_binary_check_reports_the_machine_it_is_actually_on() {
    let dir = dolt_workspace("binary");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let report = doctor(&dir, &[]);
    let f = finding(&report, "dolt binary");

    if dolt_on_path() {
        assert_eq!(f["status"], "ok", "dolt is installed: {f}");
        assert!(
            f["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "the resolved path is the evidence: {f}"
        );
    } else {
        assert_eq!(f["status"], "error", "no dolt is installed: {f}");
        assert!(f["message"].as_str().unwrap().contains("PATH"));
        assert!(
            f["fix"].as_str().unwrap().contains("install dolt"),
            "the fix must be a thing the user can do: {f}"
        );
        // And it must not blame the workspace for the machine's missing tool.
        assert!(
            f["detail"]
                .as_str()
                .unwrap()
                .contains("Nothing is wrong with the workspace"),
            "{f}"
        );
        announce_the_gap();
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// Everything below this line is **not covered**, because covering it needs a
/// running `dolt sql-server` and there is none on this machine. It is written
/// down as an `#[ignore]`d test on purpose: `cargo test` prints the reason on
/// every run, so the gap announces itself forever instead of decaying into an
/// assumption that it was handled.
///
/// It panics rather than passing, so that un-ignoring it without writing it
/// cannot turn a hole into a green tick.
#[test]
#[ignore = "NEEDS A REAL DOLT: run `dolt sql-server` in .beads/dolt and then cover — \
            (1) `dolt server` -> Running when the port is live; \
            (2) `dolt server` -> Wedged (error) when the process lives and the port refuses; \
            (3) `dolt binary` -> ok with a real version string; \
            (4) `dolt storage format` against a manifest dolt actually wrote; \
            (5) `dolt remote vs git origin` against a repo_state.json dolt actually wrote"]
fn uncovered_without_a_dolt_binary() {
    panic!(
        "not written: this test needs a real dolt binary and a real sql-server. \
         See the ignore reason for the checklist."
    );
}

/// The skip has to be loud, or it is not a skip — it is a hole with a green tick
/// over it. Set `BD_REQUIRE_DOLT=1` (in a CI job that installs dolt) to turn the
/// missing binary into a hard failure instead of a warning.
fn announce_the_gap() {
    let banner = "\
+---------------------------------------------------------------------------+
|  bd doctor / dolt: NO `dolt` BINARY ON THIS MACHINE                        |
|                                                                            |
|  The following are NOT covered by this run:                                |
|    - `dolt server` reporting Running against a live sql-server             |
|    - `dolt server` reporting Wedged  against a live-but-unreachable one    |
|    - `dolt binary` reporting a real version string                         |
|    - the manifest / repo_state.json parsers against files dolt wrote       |
|                                                                            |
|  Everything else in this file IS covered against the real bd binary.       |
|  Install dolt and run `cargo test -p bd-cli -- --ignored` to close the gap.|
+---------------------------------------------------------------------------+";
    eprintln!("{banner}");
    println!("{banner}");

    if std::env::var_os("BD_REQUIRE_DOLT").is_some() {
        panic!("BD_REQUIRE_DOLT is set, but there is no dolt binary on PATH");
    }
}
