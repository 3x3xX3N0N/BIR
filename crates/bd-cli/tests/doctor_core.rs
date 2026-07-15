//! The Core System family of `bd doctor`, against the real binary and real
//! broken workspaces.
//!
//! Doctor's *input* is broken workspaces, so a doctor test that only ever runs on
//! a healthy one has tested nothing. Every test below breaks the workspace in a
//! way somebody has actually shipped — a git-lfs pointer checked out where a
//! database should be, a file truncated by a full disk, a clone whose database
//! was (correctly) never committed — and then asserts on what the report says.
//!
//! Two properties are load-bearing and both are asserted here rather than
//! documented:
//!
//! * **"I could not check" is never "ok".** When the store will not open, the
//!   store-dependent checks must report a warning about *themselves*. A check
//!   that swallows the error and reports `ok` is worse than no check, because it
//!   reports as coverage.
//! * **The fix suggestions work.** Not "look plausible" — work. The fresh-clone
//!   test runs the exact commands the report prints and asserts the workspace
//!   comes back, with its issues *and* its id prefix. A fix that does not work is
//!   worse than no fix, and the only way to know is to run it.
//!
//! (The file is named `doctor_core`, not `doctor_install`: cargo names the test
//! binary after the file, and Windows auto-elevates any executable whose name
//! contains "install", "setup", "update" or "patch". It has bitten this repo
//! before.)

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

struct Ws(PathBuf);

impl Ws {
    /// An initialized workspace with one issue and an export beside the database
    /// — i.e. what a committed `.beads/` looks like.
    fn new(tag: &str) -> Ws {
        let ws = Ws::bare(tag);
        assert_eq!(ws.run(&["init", "--prefix", "acme"]).1, 0, "init");
        assert_eq!(ws.run(&["create", "a real issue"]).1, 0, "create");
        assert_eq!(ws.run(&["export", "-o", ".beads/issues.jsonl"]).1, 0, "export");
        ws
    }

    /// A directory with no beads in it at all.
    fn bare(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-doctor-core-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        Ws(std::fs::canonicalize(&p).unwrap())
    }

    /// (stdout, exit code)
    ///
    /// The child runs **in** the workspace rather than being pointed at it with
    /// `-C`. That is what a human does, and it is the only way a test of a fix
    /// suggestion is worth anything: the report says `bd import
    /// .beads/issues.jsonl`, and a relative path in that string is a promise
    /// about the working directory.
    fn run(&self, args: &[&str]) -> (String, i32) {
        let out = bd()
            .current_dir(&self.0)
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    /// The doctor report, parsed. (`bd doctor` exits 1 when anything is an error,
    /// and still prints the whole report — that is the point of it.)
    fn doctor(&self, extra: &[&str]) -> (Report, i32) {
        let mut args = vec!["--json", "doctor"];
        args.extend_from_slice(extra);
        let (out, code) = self.run(&args);
        let v: Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("bd doctor --json did not emit JSON ({e}): {out}"));
        (Report(v), code)
    }

    fn beads(&self) -> PathBuf {
        self.0.join(".beads")
    }

    fn db(&self) -> PathBuf {
        self.beads().join("beads.db")
    }

    /// Remove the database and the sidecars sqlite leaves beside it: a clone has
    /// none of the three.
    fn remove_db(&self) {
        for f in ["beads.db", "beads.db-wal", "beads.db-shm"] {
            std::fs::remove_file(self.beads().join(f)).ok();
        }
    }

    fn probe_files(&self) -> Vec<String> {
        std::fs::read_dir(self.beads())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".bd-doctor-probe"))
            .collect()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

struct Report(Value);

impl Report {
    fn find(&self, name: &str) -> &Value {
        self.0["checks"]
            .as_array()
            .expect("checks is an array")
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no check named {name} in the report: {}", self.0))
    }

    fn status(&self, name: &str) -> String {
        self.find(name)["status"].as_str().unwrap().to_string()
    }

    /// Everything the check said, in one string — message, detail and fix. The
    /// split between them is presentation; what matters is that the words a human
    /// needs are *somewhere*.
    fn text(&self, name: &str) -> String {
        let c = self.find(name);
        format!(
            "{} {} {}",
            c["message"].as_str().unwrap_or(""),
            c["detail"].as_str().unwrap_or(""),
            c["fix"].as_str().unwrap_or("")
        )
    }
}

/// The seven names are the keys agents grep for in `--json`. Renaming one is a
/// breaking change, so it has to be a deliberate act, not a refactor's fallout.
const CORE: [&str; 7] = [
    "workspace",
    "database",
    "schema",
    "integrity",
    "permissions",
    "database-size",
    "fresh-clone",
];

/// The checks that cannot run without a store. When the database will not open,
/// every one of these must say so about *itself* — see the "could not check"
/// tests below.
const NEEDS_STORE: [&str; 3] = ["schema", "integrity", "database-size"];

// ---------------------------------------------------------------------------
// The healthy case
// ---------------------------------------------------------------------------

#[test]
fn a_healthy_workspace_reports_every_core_check_ok_and_exits_zero() {
    let ws = Ws::new("healthy");
    let (r, code) = ws.doctor(&[]);

    assert_eq!(code, 0, "a healthy workspace must not fail doctor: {}", r.0);
    for name in CORE {
        assert_eq!(
            r.status(name),
            "ok",
            "{name} is not ok on a healthy workspace: {}",
            r.text(name)
        );
        assert_eq!(
            r.find(name)["category"], "core",
            "{name} escaped the Core category"
        );
    }
}

/// A doctor that litters is a doctor the pollution checks will later diagnose.
/// The writability probe has to create a file; it does not get to leave one.
#[test]
fn the_writability_probe_leaves_nothing_behind() {
    let ws = Ws::new("litter");
    let (_, code) = ws.doctor(&[]);
    assert_eq!(code, 0);
    assert!(
        ws.probe_files().is_empty(),
        "doctor left its write probe in .beads/: {:?}",
        ws.probe_files()
    );
}

/// `--readonly` means *do not write*. A check that wrote anyway "just to check
/// whether it could write" would be the exact bug the flag exists to prevent — so
/// it declines, and says it declined rather than claiming an answer it never got.
#[test]
fn readonly_refuses_the_write_probe_instead_of_writing_anyway() {
    let ws = Ws::new("ro");
    let (r, _) = ws.doctor(&["--readonly"]);

    assert_eq!(r.status("permissions"), "warning");
    assert!(
        r.text("permissions").contains("--readonly"),
        "the warning must say why it could not determine the answer: {}",
        r.text("permissions")
    );
    assert!(
        ws.probe_files().is_empty(),
        "--readonly doctor wrote a probe file anyway: {:?}",
        ws.probe_files()
    );
}

// ---------------------------------------------------------------------------
// No workspace
// ---------------------------------------------------------------------------

#[test]
fn no_beads_directory_is_an_error_that_names_the_command_to_run() {
    let ws = Ws::bare("empty");
    let (r, code) = ws.doctor(&[]);

    assert_eq!(code, 1, "doctor must not report success over a directory beads has never touched");
    assert_eq!(r.status("workspace"), "error");
    assert!(
        r.text("workspace").contains("bd init"),
        "the fix has to name the command: {}",
        r.text("workspace")
    );

    // And nothing else pretends to know anything.
    for name in ["database", "permissions", "fresh-clone"] {
        assert_ne!(
            r.status(name),
            "ok",
            "{name} claimed to be ok with no workspace at all: {}",
            r.text(name)
        );
    }
}

// ---------------------------------------------------------------------------
// The database will not open
// ---------------------------------------------------------------------------

/// The one that pays for the whole family: a git-lfs pointer checked out where
/// the database should be. sqlx says `file is not a database`, which sends people
/// looking for corruption. The file is not corrupt — it is a pointer, and `git
/// lfs pull` is the fix. The report has to say the word "git-lfs", and it can
/// only do that by looking at the bytes.
#[test]
fn a_git_lfs_pointer_is_named_as_one_not_reported_as_corruption() {
    let ws = Ws::new("lfs");
    ws.remove_db();
    std::fs::write(
        ws.db(),
        b"version https://git-lfs.github.com/spec/v1\noid sha256:9b2c\nsize 40960\n",
    )
    .unwrap();

    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 1);
    assert_eq!(r.status("database"), "error");

    let said = r.text("database");
    assert!(
        said.contains("git-lfs.github.com"),
        "the preview of the actual bytes is the finding; without it this is just \
         'not a database': {said}"
    );
    // The storage layer's own words are never swallowed: they are the most
    // valuable string in the report.
    assert!(
        said.contains("not a database"),
        "the store's error text was lost: {said}"
    );
}

/// The core family's contract with the other eight: when the store will not open,
/// *this* family reports the cause, and everyone else reports a warning about
/// themselves. An `ok` here would be a check that swallowed an error and reported
/// as coverage.
#[test]
fn a_store_that_will_not_open_produces_could_not_check_never_ok() {
    let ws = Ws::new("unknown");
    ws.remove_db();
    std::fs::write(ws.db(), b"<<<<<<< HEAD\nthis is a merge conflict, not a database\n").unwrap();

    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 1);

    // Exactly one check in the whole report explains the cause.
    assert_eq!(r.status("database"), "error");

    for name in NEEDS_STORE {
        assert_eq!(
            r.status(name),
            "unknown",
            "{name} must warn about itself when it cannot look, not report ok: {}",
            r.text(name)
        );
        assert!(
            r.text(name).contains("could not check"),
            "{name} must say it could not check: {}",
            r.text(name)
        );
    }

    // And the store's reason reaches every one of them, so nobody has to re-run
    // doctor to find out why their check was skipped.
    for name in NEEDS_STORE {
        assert!(
            r.text(name).contains("not a database"),
            "{name} dropped the reason it could not run: {}",
            r.text(name)
        );
    }
}

/// The emptiest possible failure, and the one a naive check waves through:
/// SQLite opens a zero-byte file without complaint and hands back a database with
/// no schema in it. "It opened" is not "it is fine".
#[test]
fn a_zero_byte_database_is_not_an_ok_database() {
    let ws = Ws::new("zero");
    ws.remove_db();
    std::fs::write(ws.db(), b"").unwrap();

    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 1);
    assert_eq!(
        r.status("database"),
        "error",
        "sqlite opens a 0-byte file, so a check that stops at `opened` passes this: {}",
        r.text("database")
    );
    assert!(r.text("database").contains("empty"), "{}", r.text("database"));
    // And the schema check independently notices there are no tables.
    assert_eq!(r.status("schema"), "error");
}

/// A database chopped off mid-page: a full disk, a killed `cp`, a bad rsync. It
/// *opens* — sqlite only reads the pages it needs — so nothing short of reading
/// the file's own arithmetic catches it before it bites.
#[test]
fn a_truncated_database_is_caught_even_though_it_opens() {
    let ws = Ws::new("trunc");
    // Give it enough rows that the file is several pages long.
    for i in 0..40 {
        assert_eq!(ws.run(&["create", &format!("issue {i}")]).1, 0);
    }
    ws.remove_db_sidecars_after_checkpoint();

    let bytes = std::fs::read(ws.db()).unwrap();
    assert!(bytes.len() > 4096, "the fixture needs more than one page");
    // Chop a fraction of a page off the end.
    std::fs::write(ws.db(), &bytes[..bytes.len() - 137]).unwrap();

    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 1, "a truncated database must fail doctor: {}", r.0);
    assert_eq!(
        r.status("integrity"),
        "error",
        "integrity waved through a file that is not a whole number of pages: {}",
        r.text("integrity")
    );
    assert!(
        r.text("integrity").contains("truncated"),
        "{}",
        r.text("integrity")
    );
}

impl Ws {
    /// Close out the WAL so the database file on disk is the whole database.
    /// (Any clean `bd` exit checkpoints it; running one more command is the
    /// cheapest way to be sure.)
    fn remove_db_sidecars_after_checkpoint(&self) {
        assert_eq!(self.run(&["status"]).1, 0);
        std::fs::remove_file(self.beads().join("beads.db-wal")).ok();
        std::fs::remove_file(self.beads().join("beads.db-shm")).ok();
    }
}

// ---------------------------------------------------------------------------
// The fresh clone — and the fix, actually run
// ---------------------------------------------------------------------------

/// What `git clone` of a beads repo gives you: `.beads/` with the locator, the
/// config and the export, and no database (it is gitignored, and rightly).
/// Every command then fails with "no beads workspace found", which is a lie — the
/// workspace is right there and every issue is in the JSONL beside it.
#[test]
fn a_clone_that_was_never_initialized_is_diagnosed_as_one() {
    let ws = Ws::new("clone");
    ws.remove_db();

    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 1);
    assert_eq!(r.status("fresh-clone"), "warning");
    assert!(
        r.text("fresh-clone").contains("1 records") || r.text("fresh-clone").contains("1 record"),
        "the count of what is waiting is the whole point: {}",
        r.text("fresh-clone")
    );
    // The database check says the cause; fresh-clone says what to do about it.
    assert_eq!(r.status("database"), "error");
}

/// **A fix suggestion that does not work is worse than none.**
///
/// So this test does not check that the string looks plausible. It runs it, and
/// then asserts the workspace actually came back — with its issues *and* with its
/// id prefix, which is the part a bare `bd init --force` silently destroys (it
/// re-derives the prefix from the directory name, so every future issue gets a
/// different prefix from every existing one).
#[test]
fn the_fresh_clone_fix_is_a_recipe_that_actually_restores_the_workspace() {
    let ws = Ws::new("recover");
    let before = ws.run(&["--json", "list"]).0;
    ws.remove_db();

    let (r, _) = ws.doctor(&[]);
    let fix = r.find("fresh-clone")["fix"].as_str().unwrap().to_string();

    // The recipe names both steps, and carries the prefix over.
    assert!(
        fix.contains("bd init --force --prefix acme"),
        "the fix must carry the workspace's own prefix, or it renames the workspace: {fix}"
    );
    assert!(
        fix.contains("bd import .beads/issues.jsonl"),
        "the fix must restore the issues, not just the schema: {fix}"
    );

    // Now run exactly that.
    assert_eq!(ws.run(&["init", "--force", "--prefix", "acme"]).1, 0, "the fix's first step failed");
    assert_eq!(ws.run(&["import", ".beads/issues.jsonl"]).1, 0, "the fix's second step failed");

    // The issues are back, byte for byte.
    assert_eq!(
        ws.run(&["--json", "list"]).0,
        before,
        "the recipe restored the database but not the issues in it"
    );
    // And new issues still get the workspace's prefix.
    let (id, code) = ws.run(&["q", "filed after the recovery"]);
    assert_eq!(code, 0);
    assert!(
        id.starts_with("acme-"),
        "the recovery renamed the workspace: new issues now mint `{id}`, not `acme-...`"
    );
    // And doctor is clean.
    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 0, "doctor is still unhappy after its own fix: {}", r.0);
}

/// Absence is not failure. A workspace with no export beside it is not a fresh
/// clone — it is just a workspace — and warning about a missing `issues.jsonl`
/// would be warning about a feature the user has chosen not to use.
#[test]
fn a_workspace_without_an_export_is_not_a_fresh_clone() {
    let ws = Ws::new("noexport");
    std::fs::remove_file(ws.beads().join("issues.jsonl")).unwrap();

    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 0);
    assert_eq!(
        r.status("fresh-clone"),
        "ok",
        "a workspace that simply does not export is not a fault: {}",
        r.text("fresh-clone")
    );
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// A schema version *ahead* of this build means a newer bd wrote the database.
/// The doctor must say so as an error whose fix is "upgrade bd" — and must not
/// suggest `bd migrate`, which refuses to downgrade (and says that too).
#[test]
fn a_schema_version_from_a_newer_bd_is_an_error_whose_fix_is_upgrading_bd() {
    let ws = Ws::new("version");
    ws.remove_db_sidecars_after_checkpoint();

    // `PRAGMA user_version` lives at offset 60 of the SQLite header, big-endian.
    let mut bytes = std::fs::read(ws.db()).unwrap();
    bytes[60..64].copy_from_slice(&7u32.to_be_bytes());
    std::fs::write(ws.db(), &bytes).unwrap();

    // Ordinary commands refuse at the door — the version gate, not a SQL error.
    let out = bd()
        .current_dir(&ws.0)
        .args(["list"])
        .output()
        .expect("run bd");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("v7") && err.contains("Upgrade bd"),
        "the refusal must name the version and the fix: {err}"
    );

    // The doctor still examines what other commands refuse — that is its job.
    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 1, "a database this build cannot read is a failure");
    assert_eq!(r.status("schema"), "error");

    let said = r.text("schema");
    assert!(said.contains('7'), "the version itself is the evidence: {said}");
    assert!(
        said.contains("upgrade bd"),
        "the fix is upgrading bd, and the report must say so: {said}"
    );
    assert!(
        !said.contains("run `bd migrate`") && !said.contains("run 'bd migrate'"),
        "migrate cannot downgrade, so offering it here is a fix that does not work: {said}"
    );

    // And migrate itself refuses the downgrade, with the real fix in the error.
    let out = bd()
        .current_dir(&ws.0)
        .args(["migrate"])
        .output()
        .expect("run bd");
    assert_eq!(
        out.status.code(),
        Some(1),
        "migrating downward is refused, not attempted"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Upgrade bd"),
        "the refusal must point at the real fix"
    );
}

/// A database from before version stamping (raw 0) is v1 by definition — it
/// works today with no ceremony, and the doctor's only note is a one-time
/// warning to stamp it, with the exact command. Running that command makes the
/// warning go away. This is the upgrade story upstream never had: no chosen
/// master, no re-bootstrap, no coordination.
#[test]
fn a_preversioning_database_works_and_migrate_stamps_it() {
    let ws = Ws::new("unstamped");
    ws.remove_db_sidecars_after_checkpoint();

    // Zero the stamp: what every database created by bd 0.1.0 looks like.
    let mut bytes = std::fs::read(ws.db()).unwrap();
    bytes[60..64].copy_from_slice(&0u32.to_be_bytes());
    std::fs::write(ws.db(), &bytes).unwrap();

    // Unstamped is NOT broken: every command works (0 reads as v1).
    assert_eq!(ws.run(&["list"]).1, 0, "a pre-versioning database must work");

    let (r, code) = ws.doctor(&[]);
    assert_eq!(code, 0, "unstamped is a warning, never a failure");
    assert_eq!(r.status("schema"), "warning");
    assert!(
        r.text("schema").contains("bd migrate"),
        "the warning must carry its one-command fix: {}",
        r.text("schema")
    );

    let (out, code) = ws.run(&["migrate"]);
    assert_eq!(code, 0, "stamping must succeed: {out}");

    let (r, _) = ws.doctor(&[]);
    assert_eq!(
        r.status("schema"),
        "ok",
        "after the stamp the warning is gone: {}",
        r.text("schema")
    );

    // The stamp is really in the file header, not just in the report.
    ws.remove_db_sidecars_after_checkpoint();
    let bytes = std::fs::read(ws.db()).unwrap();
    assert_eq!(
        u32::from_be_bytes(bytes[60..64].try_into().unwrap()),
        1,
        "user_version must hold the stamped schema version"
    );
}

// ---------------------------------------------------------------------------
// Speed
// ---------------------------------------------------------------------------

/// `bd doctor` is meant to be runnable from a git hook, which means nobody may
/// ever pay for it with their attention. The bound is deliberately loose — this
/// is a debug binary on a shared CI box — because what it is really guarding
/// against is a check that starts walking the whole graph or the whole
/// filesystem.
#[test]
fn the_whole_report_is_fast_enough_for_a_git_hook() {
    let ws = Ws::new("fast");

    // 200 issues in one process, not 200 processes: this test is measuring
    // doctor, and spawning the fixture would drown the thing being measured.
    let records: String = (0..200)
        .map(|i| {
            let now = chrono::Utc::now().to_rfc3339();
            serde_json::json!({
                "_type": "issue",
                "id": format!("acme-f{i:03}"),
                "title": format!("issue {i}"),
                "status": if i % 3 == 0 { "closed" } else { "open" },
                "priority": 2,
                "issue_type": "task",
                "created_at": now,
                "updated_at": now,
            })
            .to_string()
        })
        .fold(String::new(), |mut acc, r| {
            acc.push_str(&r);
            acc.push('\n');
            acc
        });
    let path = ws.0.join("bulk.jsonl");
    std::fs::write(&path, records).unwrap();
    let (out, code) = ws.run(&["import", "bulk.jsonl"]);
    assert_eq!(code, 0, "the fixture would not import: {out}");
    // Keep the committed export in step, or the (correct) staleness checks in
    // other families fire and this test starts failing for someone else's
    // reasons.
    assert_eq!(ws.run(&["export", "-o", ".beads/issues.jsonl"]).1, 0, "export");

    let start = std::time::Instant::now();
    let (r, code) = ws.doctor(&[]);
    let took = start.elapsed();

    assert_eq!(code, 0, "doctor failed on a healthy 200-issue workspace: {}", r.0);
    // Deliberately loose: a debug binary on a shared CI box. What this is really
    // guarding is a check that starts walking the whole graph or the filesystem.
    assert!(
        took < std::time::Duration::from_secs(15),
        "doctor took {took:?} over 200 issues — something in it is not O(small)"
    );
}

// ---------------------------------------------------------------------------
// The report itself
// ---------------------------------------------------------------------------

/// `--json` is the agent-facing surface, and a finding that reports a fault with
/// no evidence and no advice is a bug report nobody can act on.
#[test]
fn every_core_finding_that_is_not_ok_carries_evidence_and_advice() {
    let ws = Ws::new("shape");
    ws.remove_db();
    std::fs::write(ws.db(), b"not a database, not even close").unwrap();

    let (r, _) = ws.doctor(&[]);
    for name in CORE {
        let c = r.find(name);
        if c["status"] == "ok" {
            continue;
        }
        assert!(
            c["detail"].is_string(),
            "{name} reported a problem with no evidence: {c}"
        );
        // "could not check" findings have nothing to advise — they did not get
        // far enough to have an opinion. Everything else must.
        if c["message"] != "could not check" {
            assert!(
                c["fix"].is_string(),
                "{name} reported a problem and left the user nowhere to go: {c}"
            );
        }
    }
}

/// The path in the report is the workspace it actually looked at. An agent
/// running doctor in a subdirectory needs to know which `.beads/` answered.
#[test]
fn the_report_names_the_workspace_it_examined() {
    let ws = Ws::new("path");
    let nested = ws.0.join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();

    let out = bd()
        .current_dir(&nested)
        .args(["--json", "doctor"])
        .output()
        .expect("run bd");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    let path = v["path"].as_str().expect("the report names a path");

    assert!(
        Path::new(path).ends_with(".beads"),
        "doctor run from a subdirectory pointed at {path}"
    );
    assert!(!v["checks"].as_array().unwrap().is_empty());
}
