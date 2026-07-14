//! The Dolt family of `bd doctor`, end to end through the real binary.
//!
//! # There is no `dolt` on the machine that wrote these tests
//!
//! That is not a hole to paper over. It is a constraint, and it decides what this
//! file can honestly claim:
//!
//! * Everything that reads the *filesystem* is covered for real. A Dolt workspace
//!   is a `.beads/workspace.json` saying `"backend":"dolt"`, and `bd doctor` runs
//!   on workspaces too broken to open — so it will happily diagnose one that was
//!   fabricated with `fs::write`. Stale server records, stale locks, storage
//!   format, remotes, and every repair are exercised against the real binary here.
//! * Everything that needs a *port* is covered too, with a fake MySQL server: a
//!   listener that speaks first is indistinguishable from dolt as far as doctor's
//!   probe is concerned, and a listener that accepts and stays *silent* is exactly
//!   the wedged server the probe must not be fooled by. Both are here.
//! * Everything that needs a live process **named `dolt`** — `Running`, `Wedged`,
//!   `Starting` — is not covered, and cannot be without a dolt binary. It is named,
//!   loudly, by `uncovered_without_a_dolt_binary` below, which is `#[ignore]`d with
//!   the exact checklist as its reason so that every `cargo test` run prints the
//!   fact that they are uncovered. A test that reported as coverage without testing
//!   anything would be worse than no test at all.
//!
//! # Every path here comes from `bd-dolt`
//!
//! The previous version of this file hard-coded `.beads/dolt/.dolt/`,
//! `.beads/dolt-server.pid` and `.beads/dolt-server.port` — none of which exist.
//! The tests passed, because they were fabricating the same fictional workspace the
//! checks were inspecting. Two wrongs agreeing is not a test. So the record's name
//! and shape come from [`bd_dolt::server`] here as well, and `.beads/` **is** the
//! dolt repository.
//!
//! (Filename note: never "install", "setup", "update" or "patch" in a test file
//! name — cargo names the test binary after the file, and Windows auto-elevates any
//! exe whose name looks like an installer.)

use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use bd_dolt::server::{PORT_ENV, PidFile, pidfile_path};
use serde_json::Value;

fn bd() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_bd"));
    // A developer (or a CI job) with BD_DOLT_PORT set would otherwise send every
    // one of these workspaces down the adopt path, and the tests would quietly stop
    // testing what they say they test.
    c.env_remove(PORT_ENV);
    c
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn run(dir: &Path, args: &[&str]) -> Run {
    run_env(dir, args, &[])
}

fn run_env(dir: &Path, args: &[&str], env: &[(&str, String)]) -> Run {
    let mut cmd = bd();
    cmd.args(["-C", dir.to_str().unwrap()])
        .args(args)
        .env("BEADS_ACTOR", "agent-7");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run bd");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        code: out.status.code().unwrap_or(-1),
    }
}

/// `bd doctor --json`, as a `Value`. Doctor exits nonzero on any `error` finding,
/// which several of these workspaces have on purpose, so the exit code is not
/// asserted here — the findings are.
fn doctor(dir: &Path, extra: &[&str]) -> Value {
    doctor_env(dir, extra, &[])
}

fn doctor_env(dir: &Path, extra: &[&str], env: &[(&str, String)]) -> Value {
    let mut args = vec!["--json", "doctor"];
    args.extend_from_slice(extra);
    let r = run_env(dir, &args, env);
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

fn message(report: &Value, name: &str) -> String {
    finding(report, name)["message"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// Every check this family registers. Spelled out so that a check which silently
/// disappears from the registry fails a test instead of quietly reducing coverage
/// to zero.
///
/// `dolt lock files` is deliberately NOT here. Lock debris under `.beads/` is
/// owned by the Maintenance family (`checks/pollution.rs`) — this family had a
/// second check matching the same files with the same repair, so
/// `dolt.bootstrap.lock` was diagnosed twice and two repairs raced to unlink it.
const DOLT_CHECKS: &[&str] = &[
    "dolt binary",
    "dolt database",
    "dolt storage format",
    "dolt server",
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

fn beads(dir: &Path) -> PathBuf {
    dir.join(".beads")
}

/// Give the fabricated workspace a database that looks real enough to read.
///
/// **`.beads/` is the dolt repository**, so the database is `.beads/.dolt/` — not
/// `.beads/dolt/.dolt/`, which is what this helper used to create and what the
/// checks used to look for. Both were wrong, and they were wrong *together*, which
/// is how the tests stayed green over checks that inspected nothing.
///
/// The `LOCK` and the chunk file are sentinels: their survival is the safety
/// property `--fix` must never violate.
fn with_dolt_db(dir: &Path, manifest: &str) {
    let noms = beads(dir).join(".dolt/noms");
    std::fs::create_dir_all(&noms).unwrap();
    std::fs::write(noms.join("manifest"), manifest).unwrap();
    // Dolt's own advisory lock. bd must never, ever delete this.
    std::fs::write(noms.join("LOCK"), "").unwrap();
    std::fs::write(noms.join("vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv"), "chunks").unwrap();
}

/// Everything of Dolt's, still there. This is the assertion that matters.
fn database_is_intact(dir: &Path) {
    let noms = beads(dir).join(".dolt/noms");
    assert!(noms.join("manifest").is_file(), "the manifest was destroyed");
    assert!(noms.join("LOCK").is_file(), "dolt's LOCK was destroyed");
    assert!(
        noms.join("vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv").is_file(),
        "a chunk journal was destroyed"
    );
}

const CURRENT_MANIFEST: &str =
    "5:__DOLT__:qtnpkc6r0b7egk3t1cvdd0s7fh0m8b3v:t9hcvrb9khhgqcltcbd9m5j5j3fgvhqf:0";

/// Write the server record exactly as `bd-dolt` writes it: its type, its
/// serializer, its filename. A test that hand-rolled the JSON would be free to
/// hand-roll it *wrong*, which is the bug this whole file is a response to.
fn write_record(dir: &Path, pid: u32, port: u16) -> PathBuf {
    let path = pidfile_path(&beads(dir));
    std::fs::write(&path, serde_json::to_string(&PidFile { pid, port }).unwrap()).unwrap();
    path
}

/// A process id that is certainly not a running dolt: spawn something trivial,
/// wait for it to exit, and take its id. (Pid reuse would have to happen inside the
/// same handful of milliseconds *and* hand the id to a `dolt` — and there is no
/// dolt on this machine to hand it to.)
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

/// A port that nothing is listening on.
fn a_dead_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn backdate(p: &Path, by: Duration) {
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - by).unwrap();
}

/// A git repository at `dir`, optionally with an origin.
///
/// Explicit, because the alternative — relying on whether the system temp directory
/// happens to sit inside somebody's git repo — decides which branch of the check
/// runs, and a test that does not know which branch it is on is not a test.
fn git_init(dir: &Path, origin: Option<&str>) {
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
    if let Some(url) = origin {
        git(&["remote", "add", "origin", url]);
    }
}

/// A loopback server that is not dolt.
///
/// `greet: true` speaks first, like every MySQL server — which is all doctor's
/// probe asks of it. `greet: false` binds, accepts, and stays silent: the exact
/// state a *wedged* dolt is in, and the one a connect-only probe would wave through
/// as healthy.
struct FakeServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeServer {
    fn start(greet: bool) -> FakeServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();

        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        if greet {
                            let _ = sock.write_all(b"\x0a5.7.9-fake-dolt\0");
                            let _ = sock.flush();
                        } else {
                            // Hold it open, silent, past the prober's read timeout.
                            std::thread::sleep(Duration::from_millis(800));
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        FakeServer {
            port,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// The rule: a SQLite user has no Dolt problem
// ---------------------------------------------------------------------------

/// Ten warnings about a Dolt server the user never asked for is how you train
/// people to stop reading `bd doctor` at all. So: on SQLite, this family is silent
/// — even when the workspace is littered with exactly the debris that would make a
/// Dolt workspace scream.
#[test]
fn a_sqlite_workspace_hears_nothing_at_all_from_the_dolt_family() {
    let dir = tmp("sqlite");
    assert_eq!(run(&dir, &["init", "--prefix", "t"]).code, 0, "init");

    // Debris that is loud in a Dolt workspace and meaningless in this one.
    let record = write_record(&dir, a_dead_pid(), 3306);
    let lock = beads(&dir).join("dolt.bootstrap.lock");
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
        assert_eq!(message(&report, name), "not a dolt workspace");
    }

    // The human rendering collapses the lot to a single line — and it says `n/a`,
    // not `ok`. "5 ok" would claim it verified five things about a Dolt server this
    // workspace does not have.
    let human = run(&dir, &["doctor"]).stdout;
    let expected = format!("Dolt Storage ({} n/a)", DOLT_CHECKS.len());
    assert!(
        human.contains(&expected),
        "the family should collapse to one `{expected}` line, got:\n{human}"
    );

    // Belt and braces: `--fix` must not touch a SQLite user's files either.
    run(&dir, &["doctor", "--fix"]);
    assert!(record.exists());
    assert!(lock.exists());

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The check that saves people
// ---------------------------------------------------------------------------

/// A `dolt sql-server` that died without cleaning up leaves its record behind. The
/// next `bd` command fails with something that reads exactly like database
/// corruption, and the user deletes `.beads/` to fix it — which, because `.beads/`
/// **is** the dolt repository, destroys every issue they have.
///
/// So the finding is asserted almost word for word. Its job is to be *believed*: it
/// must name the stale file, and it must say — in the output the user is staring at
/// — that this is not corruption and which directory not to delete. The old version
/// of this check told them to spare `.beads/dolt/`, a directory that does not
/// exist, leaving `.beads/.dolt/` (the actual database) unguarded.
#[test]
fn a_stale_server_record_is_named_as_stale_and_explicitly_not_as_corruption() {
    let dir = dolt_workspace("stale");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    write_record(&dir, a_dead_pid(), a_dead_port());

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt server"), "warning");

    let d = detail(&report, "dolt server");
    assert!(
        d.contains("NOT database corruption"),
        "the whole point of this check is to stop the rm -rf; got: {d}"
    );
    assert!(
        d.contains(".beads/.dolt/"),
        "it must name the directory that IS the database, not one that never existed; got: {d}"
    );
    assert!(
        d.contains("Do not delete"),
        "got: {d}"
    );
    assert!(
        d.contains("dolt-server.json"),
        "a finding that does not name the file cannot be acted on; got: {d}"
    );

    // It is not an `error`: bd can recover from this on its own, and a nonzero exit
    // for a stale record would be crying wolf.
    assert_ne!(status(&report, "dolt server"), "error");

    std::fs::remove_dir_all(&dir).ok();
}

/// The safety property, asserted rather than commented: `--fix` removes *bd's*
/// bookkeeping and does not go anywhere near `.dolt/`. Dolt's `LOCK` is advisory —
/// the OS drops it when the process dies — so its presence proves nothing, and
/// deleting it (or the manifest, or a journal) is the unrecoverable mistake this
/// family exists to talk people out of.
#[test]
fn fix_removes_the_stale_record_and_never_touches_dot_dolt() {
    let dir = dolt_workspace("fix");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let record = write_record(&dir, a_dead_pid(), a_dead_port());

    let report = doctor(&dir, &["--fix"]);

    let repairs = report["repairs"].as_array().expect("repairs");
    let mine = repairs
        .iter()
        .find(|r| r["check"] == "dolt server")
        .expect("the dolt server check should have repaired something");
    assert_eq!(mine["outcome"], "fixed");
    assert!(
        mine["message"].as_str().unwrap().contains("dolt-server.json"),
        "a repair must name what it changed: {mine}"
    );

    assert!(!record.exists(), "the stale record should be gone");
    database_is_intact(&dir);

    // Idempotent: with the bookkeeping gone, there is nothing left to say.
    let again = doctor(&dir, &[]);
    assert_eq!(status(&again, "dolt server"), "ok");

    std::fs::remove_dir_all(&dir).ok();
}

/// A record that is not a record. The realistic shape is a truncated write from a
/// machine that lost power mid-`bd`, and it must degrade to the same tidy,
/// repairable state — not to a panic, and not to a shrug.
///
/// This one is doctor's alone to fix: `bd_dolt::server::read_pidfile` reads an
/// unparseable record as *absent* and never deletes it, so without this check it
/// would sit in `.beads/` forever.
#[test]
fn a_corrupt_server_record_is_repaired_rather_than_ignored() {
    let dir = dolt_workspace("corrupt-record");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let record = pidfile_path(&beads(&dir));
    std::fs::write(&record, "{ \u{0}\u{0}truncated").unwrap();

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt server"), "warning");

    doctor(&dir, &["--fix"]);
    assert!(!record.exists());
    database_is_intact(&dir);

    std::fs::remove_dir_all(&dir).ok();
}

/// The old on-disk shape — a bare pid, in a file called `dolt-server.pid` — is not
/// a record bd will ever write again. A `dolt-server.json` containing a bare number
/// is therefore garbage, and must be treated as such rather than parsed by some
/// leftover fallback.
#[test]
fn a_bare_pid_is_not_a_server_record() {
    let dir = dolt_workspace("bare-pid");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let record = pidfile_path(&beads(&dir));
    // A live pid, in the old format. If anything still parsed this, it would report
    // a running server that does not exist.
    std::fs::write(&record, std::process::id().to_string()).unwrap();

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt server"), "warning");
    assert!(
        detail(&report, "dolt server").contains("not a server record"),
        "got: {}",
        detail(&report, "dolt server")
    );

    doctor(&dir, &["--fix"]);
    assert!(!record.exists());

    std::fs::remove_dir_all(&dir).ok();
}

/// The Windows trap, end to end. A record naming a pid that is alive but belongs to
/// something that is *not* dolt is stale — ids get recycled within seconds — and
/// the finding must say so rather than reporting a healthy server.
///
/// The live non-dolt process here is the test binary itself, which is about as
/// certainly-not-dolt as a process gets.
#[test]
fn a_recycled_pid_is_reported_as_gone_not_as_running() {
    let dir = dolt_workspace("recycled");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let record = write_record(&dir, std::process::id(), a_dead_port());

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt server"), "warning");
    assert!(
        message(&report, "dolt server").contains("recycled"),
        "got: {}",
        message(&report, "dolt server")
    );
    let d = detail(&report, "dolt server");
    assert!(
        d.contains("not dolt"),
        "the identity of the process is the evidence; got: {d}"
    );
    assert!(d.contains("NOT database corruption"), "got: {d}");

    doctor(&dir, &["--fix"]);
    assert!(!record.exists(), "a recycled pid's record is stale");
    database_is_intact(&dir);

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The port probe — the Running / Wedged distinction, without a dolt
// ---------------------------------------------------------------------------

/// `BD_DOLT_PORT` points bd at a server it did not start (docker, CI, a dev's
/// terminal). `bd_dolt::server::try_adopt` then consults *only* that port and never
/// opens the server record — so doctor must route the same way, or it will
/// confidently diagnose a file bd is about to ignore.
///
/// This is also the one place the port probe itself can be exercised end to end
/// without a dolt: a listener that speaks first is, to the probe, a dolt.
#[test]
fn a_server_on_bd_dolt_port_is_reported_as_adopted() {
    let dir = dolt_workspace("adopt");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    // A record that would otherwise read as stale. Under BD_DOLT_PORT it is
    // irrelevant, exactly as it is to bd itself.
    let record = write_record(&dir, a_dead_pid(), a_dead_port());

    let server = FakeServer::start(true);
    let env = [(PORT_ENV, server.port.to_string())];

    let report = doctor_env(&dir, &[], &env);
    assert_eq!(status(&report, "dolt server"), "ok");
    let m = message(&report, "dolt server");
    assert!(m.contains(&server.port.to_string()), "got: {m}");
    assert!(m.contains("adopting"), "got: {m}");

    // And `--fix` must not delete the record of a server it is adopting — nor stop
    // the server, which is not ours.
    doctor_env(&dir, &["--fix"], &env);
    assert!(record.exists(), "--fix deleted a record bd is not even using");

    std::fs::remove_dir_all(&dir).ok();
}

/// The distinction the whole family rests on, and the one a connect-only probe gets
/// wrong: **a listener that accepts and says nothing is not a server.** That is the
/// state a wedged dolt is in, and calling it healthy is the single failure this
/// check exists to prevent.
///
/// A real dolt would additionally be a live process named `dolt`, which would make
/// this `Wedged`; here there is no such process, so what is asserted is the half
/// that *is* reachable — that a silent listener is not mistaken for a live server.
#[test]
fn a_listener_that_never_speaks_is_not_a_live_server() {
    let dir = dolt_workspace("silent");
    with_dolt_db(&dir, CURRENT_MANIFEST);

    let silent = FakeServer::start(false);
    let env = [(PORT_ENV, silent.port.to_string())];

    let report = doctor_env(&dir, &[], &env);
    // Not "adopting": nothing is serving there, whatever the TCP layer says.
    assert_eq!(status(&report, "dolt server"), "ok");
    let m = message(&report, "dolt server");
    assert!(
        !m.contains("adopting"),
        "a bound-but-silent listener was mistaken for a live server: {m}"
    );
    assert!(
        m.contains("no server is running"),
        "got: {m}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// The lock-debris test lived here. Lock files under `.beads/` are owned by the
// Maintenance family now (`checks/pollution.rs`): this family had a second check
// matching the same names with the same repair, so `dolt.bootstrap.lock` was
// diagnosed twice and two repairs raced to unlink it. The property that the
// sweeper never touches a file bd-dolt or dolt owns moved with it, and is
// asserted there against bd-dolt's own constants.

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
    let fix = finding(&report, "dolt storage format")["fix"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        fix.contains("dolt migrate"),
        "a finding without a command the user can paste is half a finding: {fix}"
    );
    assert!(
        fix.contains("cd .beads &&"),
        "the command must be runnable *where the repository actually is*; got: {fix}"
    );
    std::fs::remove_dir_all(&old_dir).ok();
}

/// The safety property of the format check: if the manifest is not the shape we
/// expect, we say *we do not know* — never "healthy". Being wrong about Dolt's
/// on-disk format must not silently certify a legacy database as fine.
#[test]
fn an_unreadable_manifest_is_unknown_not_a_pass() {
    let dir = dolt_workspace("fmt-weird");
    with_dolt_db(&dir, "this is not a manifest at all");

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt storage format"), "unknown");
    assert_eq!(message(&report, "dolt storage format"), "could not check");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The database itself
// ---------------------------------------------------------------------------

/// A dolt-backed locator with no dolt database under it: every command that opens
/// the store will fail, and this is the one check that says why. It is an `error` —
/// the workspace really is broken — while the checks that depend on the database
/// report `n/a`, so the user reads one problem instead of five.
///
/// `n/a`, note, and not `ok`. They verified nothing; claiming `ok` would count them
/// among the things this run actually checked.
#[test]
fn a_dolt_workspace_with_no_database_reports_exactly_one_problem() {
    let dir = dolt_workspace("no-db");

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt database"), "error");
    assert!(
        detail(&report, "dolt database").contains(".dolt"),
        "the error must name the directory that is missing"
    );

    // The dependents do not pile on, and they do not claim coverage either.
    assert_eq!(status(&report, "dolt storage format"), "n/a");
    assert_eq!(status(&report, "dolt remote vs git origin"), "n/a");
    // The server check *did* look: there is no record and no BD_DOLT_PORT. That is
    // a real, verified answer.
    assert_eq!(status(&report, "dolt server"), "ok");

    std::fs::remove_dir_all(&dir).ok();
}

/// The workspace this whole rewrite exists because of.
///
/// Upstream Go beads keeps its dolt repository in `.beads/dolt/`; this port makes
/// `.beads/` itself the repository. A workspace made by the Go binary therefore has
/// its database in a place this one does not read — and the previous version of
/// this check looked *only* in that place, which is exactly how it managed to
/// inspect nothing and report a clean bill of health.
///
/// Told plainly, this is a diagnosable state with a way out. Told as "the dolt
/// database is missing", it is a user deleting a directory that contains all their
/// issues.
#[test]
fn a_go_beads_workspace_is_named_as_such_rather_than_called_missing() {
    let dir = dolt_workspace("go-layout");

    // Upstream's layout, exactly: the repository under `.beads/dolt/`.
    let noms = beads(&dir).join("dolt/.dolt/noms");
    std::fs::create_dir_all(&noms).unwrap();
    std::fs::write(noms.join("manifest"), CURRENT_MANIFEST).unwrap();

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt database"), "error");

    let d = detail(&report, "dolt database");
    assert!(
        d.contains("Go implementation"),
        "the user must be told their data is intact and merely elsewhere; got: {d}"
    );
    assert!(
        d.contains("intact"),
        "a message that does not say the data is safe is a message that gets it deleted; got: {d}"
    );
    let fix = finding(&report, "dolt database")["fix"].as_str().unwrap();
    assert!(
        fix.contains("export") && fix.contains("import"),
        "the way out has to be a thing they can actually do: {fix}"
    );

    // And --fix must not lay a finger on it.
    doctor(&dir, &["--fix"]);
    assert!(
        noms.join("manifest").is_file(),
        "--fix destroyed a Go-beads database it does not even know how to read"
    );

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
    git_init(&dir, Some("https://github.com/acme/beads.git"));

    std::fs::write(
        beads(&dir).join(".dolt/repo_state.json"),
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

/// The other direction: a dolt remote that points somewhere else entirely is not a
/// problem, and must not be reported as one.
///
/// Note the `git init` — without it this test would have been standing in the "no
/// git repository" branch while claiming to assert the comparison, which is what it
/// used to do. A test that passes down a path it never meant to take is a test that
/// is not testing.
#[test]
fn unrelated_dolt_remotes_are_left_alone() {
    let dir = dolt_workspace("remotes-ok");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    git_init(&dir, Some("https://github.com/acme/beads.git"));
    std::fs::write(
        beads(&dir).join(".dolt/repo_state.json"),
        r#"{"remotes":{"origin":{"name":"origin","url":"https://doltremoteapi.dolthub.com/acme/beads"}}}"#,
    )
    .unwrap();

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt remote vs git origin"), "ok");
    assert!(
        message(&report, "dolt remote vs git origin").contains("none matching"),
        "got: {}",
        message(&report, "dolt remote vs git origin")
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// No git origin, so there is nothing for the dolt remote to collide with — and
/// nothing this check verified either. `n/a`, not `ok`: "18 ok" has to mean
/// eighteen things were actually looked at, or the number is worthless.
#[test]
fn a_dolt_remote_with_no_git_origin_to_compare_against_is_not_applicable() {
    let dir = dolt_workspace("remotes-nogit");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    git_init(&dir, None);
    std::fs::write(
        beads(&dir).join(".dolt/repo_state.json"),
        r#"{"remotes":{"origin":{"name":"origin","url":"https://doltremoteapi.dolthub.com/acme/beads"}}}"#,
    )
    .unwrap();

    let report = doctor(&dir, &[]);
    assert_eq!(status(&report, "dolt remote vs git origin"), "n/a");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// `bd init --backend=dolt`
// ---------------------------------------------------------------------------

/// A machine with no `dolt` must end up with **no workspace at all**, rather than a
/// `.beads/` that says "I am a dolt workspace" over an empty hole. `bd init` checks
/// before it writes anything, and the refusal has to be one a human can act on.
#[test]
fn init_backend_dolt_refuses_cleanly_when_there_is_no_dolt() {
    if bd_dolt::dolt_available() {
        eprintln!(
            "SKIPPED: `init_backend_dolt_refuses_cleanly_when_there_is_no_dolt` is NOT covering \
             anything on this machine — there IS a dolt on PATH, so init succeeds instead of \
             refusing. This test only has meaning where dolt is absent."
        );
        return;
    }

    let dir = tmp("init-dolt");
    let r = run(&dir, &["init", "--backend", "dolt", "--prefix", "t"]);

    assert_ne!(r.code, 0, "init must fail without a dolt binary");
    let said = format!("{}{}", r.stdout, r.stderr);
    assert!(
        said.contains("dolt") && said.contains("PATH"),
        "the refusal must say what is missing and where: {said}"
    );
    assert!(
        said.contains("sqlite"),
        "and it must offer the way out: {said}"
    );

    // The part that matters: nothing was written. A `.beads/` claiming a dolt
    // backend with no database under it is a workspace every later command fails on.
    assert!(
        !beads(&dir).exists(),
        "a failed dolt init left a workspace behind"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The dolt binary, and the honest accounting of what is not covered
// ---------------------------------------------------------------------------

/// A Dolt workspace on a machine with no `dolt` is a real, diagnosable, actionable
/// state — and it is the state of the machine this port is being written on, so it
/// is the branch that gets real coverage. When a `dolt` *is* present, the same check
/// must find it and report its version; this test asserts whichever branch it is
/// standing in, and never both.
///
/// `bd_dolt::dolt_available` decides which, rather than a second PATH walk of this
/// file's own: doctor resolves `dolt` through `bd_dolt::which_dolt`, and a test that
/// resolved it differently could pass while doctor and the store disagreed.
#[test]
fn the_dolt_binary_check_reports_the_machine_it_is_actually_on() {
    let dir = dolt_workspace("binary");
    with_dolt_db(&dir, CURRENT_MANIFEST);
    let report = doctor(&dir, &[]);
    let f = finding(&report, "dolt binary");

    if bd_dolt::dolt_available() {
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

/// Everything below this line is **not covered**, because covering it needs a live
/// process named `dolt` and there is none on this machine. It is written down as an
/// `#[ignore]`d test on purpose: `cargo test` prints the reason on every run, so the
/// gap announces itself forever instead of decaying into an assumption that it was
/// handled.
///
/// It panics rather than passing, so that un-ignoring it without writing it cannot
/// turn a hole into a green tick.
#[test]
#[ignore = "NEEDS A REAL DOLT: `dolt sql-server` serving .beads, then cover — \
            (1) `dolt server` -> Running: a live `dolt` process whose recorded port answers; \
            (2) `dolt server` -> Wedged (error): a live `dolt` whose port accepts and never speaks; \
            (3) `dolt server` -> Starting: the same, with a record younger than 30s; \
            (4) `dolt binary` -> ok with a real version string; \
            (5) `dolt storage format` against a manifest dolt actually wrote; \
            (6) `dolt remote vs git origin` against a repo_state.json dolt actually wrote; \
            (7) `bd init --backend=dolt` succeeding, and the record bd-dolt then writes"]
fn uncovered_without_a_dolt_binary() {
    panic!(
        "not written: these need a real dolt binary and a real sql-server. \
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
|    - `dolt server` -> Running  (needs a live process named `dolt`)         |
|    - `dolt server` -> Wedged   (needs a live `dolt` that will not speak)   |
|    - `dolt server` -> Starting (needs a live `dolt`, record < 30s old)     |
|    - `dolt binary` reporting a real version string                         |
|    - the manifest / repo_state.json parsers against files dolt wrote       |
|    - `bd init --backend=dolt` succeeding                                   |
|                                                                            |
|  Everything else in this file IS covered against the real bd binary,       |
|  including the port probe (a fake MySQL server stands in for dolt) and     |
|  every repair.                                                             |
|                                                                            |
|  Install dolt and run `cargo test -p bd-cli -- --ignored` to close the gap.|
+---------------------------------------------------------------------------+";
    eprintln!("{banner}");
    println!("{banner}");

    if std::env::var_os("BD_REQUIRE_DOLT").is_some() {
        panic!("BD_REQUIRE_DOLT is set, but there is no dolt binary on PATH");
    }
}
