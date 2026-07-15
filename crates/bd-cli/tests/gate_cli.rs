//! `bd gate …`, end to end through the real binary and a real sqlite database.
//!
//! The load-bearing test is `resolving_a_gate_makes_its_dependent_ready`: it is
//! the whole reason the family exists. Everything else pins the edges — the
//! metadata shape a cooked gate and a manual gate share, that `check` never
//! writes, that pointing a gate command at a non-gate is a named error.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

struct Ws(PathBuf);

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Ws {
    fn new(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-gate-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        let ws = Ws(std::fs::canonicalize(&p).unwrap());
        assert_eq!(ws.run(&["init", "--prefix", "t"]).code, 0, "init");
        ws
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", self.0.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "gate-test")
            .env("NO_COLOR", "1")
            .output()
            .expect("run bd");
        Run {
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// Create a gate and return the id the store minted.
    fn create_gate(&self, name: &str) -> String {
        let r = self.run(&["gate", "create", name, "--json"]);
        assert_eq!(r.code, 0, "gate create failed: {}", r.stderr);
        let v: Value = serde_json::from_str(&r.stdout).expect("gate create --json is JSON");
        v["id"].as_str().expect("gate has an id").to_string()
    }

    /// Create a plain workable issue via `bd q`, which prints the bare id.
    fn quick(&self, title: &str) -> String {
        let r = self.run(&["q", title]);
        assert_eq!(r.code, 0, "q failed: {}", r.stderr);
        r.stdout.trim().to_string()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// The point of the whole family: a gate blocks real work, and resolving the
/// gate frees it — correctly, on the very next `bd ready`, with no manual
/// recompute. That last part is what proves `close_issue` re-fixpoints the
/// blocked cache for the gate's dependents on its own.
#[test]
fn resolving_a_gate_makes_its_dependent_ready() {
    let ws = Ws::new("resolve");

    let gate = ws.create_gate("await approval");
    let work = ws.quick("ship it");

    // `work` blocks on `gate`: work depends-on gate (default edge is `blocks`).
    assert_eq!(
        ws.run(&["dep", "add", &work, &gate]).code,
        0,
        "dep add should succeed"
    );

    // Before resolving: the work is blocked, not ready.
    let ready = ws.run(&["ready"]).stdout;
    assert!(
        !ready.contains("ship it"),
        "work must not be ready while the gate is open: {ready}"
    );
    let blocked = ws.run(&["blocked"]).stdout;
    assert!(
        blocked.contains("ship it"),
        "work should be blocked by the gate: {blocked}"
    );

    // Resolve the gate.
    let resolved = ws.run(&["gate", "resolve", &gate]);
    assert_eq!(resolved.code, 0, "resolve failed: {}", resolved.stderr);

    // After resolving: the work is ready on the very next call — no `bd
    // recompute-blocked` in between. If `close_issue` did not recompute the
    // dependents' `is_blocked`, this is the assertion that would fail.
    let ready = ws.run(&["ready"]).stdout;
    assert!(
        ready.contains("ship it"),
        "work must be ready once the gate is resolved: {ready}"
    );
    let blocked = ws.run(&["blocked"]).stdout;
    assert!(
        !blocked.contains("ship it"),
        "work must no longer be blocked: {blocked}"
    );
}

/// `resolve --json` reports which dependents it freed, beside the closed gate.
#[test]
fn resolve_reports_the_issues_it_unblocked() {
    let ws = Ws::new("unblocked");
    let gate = ws.create_gate("ci green");
    let work = ws.quick("deploy");
    assert_eq!(ws.run(&["dep", "add", &work, &gate]).code, 0);

    let r = ws.run(&["gate", "resolve", &gate, "--json"]);
    assert_eq!(r.code, 0, "resolve --json: {}", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).expect("resolve --json is JSON");
    assert_eq!(v["status"], "closed", "a resolved gate is closed");
    let unblocked = v["unblocked"].as_array().expect("unblocked is an array");
    assert!(
        unblocked.iter().any(|x| x == &Value::String(work.clone())),
        "the freed work should be listed: {v}"
    );
}

/// A manually-created gate carries the same `{"gate":{…}}` metadata shape that
/// `bd cook` writes, and is `gate`-typed.
#[test]
fn create_writes_the_cook_compatible_gate_shape() {
    let ws = Ws::new("shape");
    let r = ws.run(&["gate", "create", "human sign-off", "--json"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).expect("JSON");

    assert_eq!(v["issue_type"], "gate", "a gate is Gate-typed");
    assert_eq!(v["title"], "human sign-off");
    // The policy blob matches formula.rs: {"gate": {"await_type", "await_id",
    // "timeout"}}. A manual gate records `manual` with the two halves null.
    assert_eq!(v["metadata"]["gate"]["await_type"], "manual");
    assert!(v["metadata"]["gate"].get("await_id").is_some(), "await_id key present");
    assert!(v["metadata"]["gate"].get("timeout").is_some(), "timeout key present");
}

/// `list` shows open gates and hides resolved ones — "what am I still waiting
/// on". A plain (non-gate) issue never appears.
#[test]
fn list_shows_open_gates_only() {
    let ws = Ws::new("list");
    let g1 = ws.create_gate("gate one");
    let _g2 = ws.create_gate("gate two");
    let _work = ws.quick("ordinary task");

    let listed = ws.run(&["gate", "list"]).stdout;
    assert!(listed.contains("gate one"), "open gate should list: {listed}");
    assert!(listed.contains("gate two"), "open gate should list: {listed}");
    assert!(
        !listed.contains("ordinary task"),
        "a non-gate must not appear in gate list: {listed}"
    );

    // Resolve one; it drops out of the open list.
    assert_eq!(ws.run(&["gate", "resolve", &g1]).code, 0);
    let listed = ws.run(&["gate", "list"]).stdout;
    assert!(
        !listed.contains("gate one"),
        "a resolved gate must leave the open list: {listed}"
    );
    assert!(listed.contains("gate two"), "the unresolved gate stays: {listed}");
}

/// `check` is read-only: it reports state without mutating, and works under
/// `--readonly`.
#[test]
fn check_reports_state_and_never_writes() {
    let ws = Ws::new("check");
    let gate = ws.create_gate("timer");

    // Waiting, and readable under --readonly.
    let r = ws.run(&["gate", "check", &gate, "--readonly", "--json"]);
    assert_eq!(r.code, 0, "check must work read-only: {}", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).expect("JSON");
    assert_eq!(v["satisfied"], false);
    assert_eq!(v["state"], "waiting");

    // The gate is still open afterwards: check changed nothing.
    let after = ws.run(&["gate", "check", &gate]).stdout;
    assert!(after.contains("still waiting"), "check must not resolve: {after}");

    // Resolve, then check flips to satisfied.
    assert_eq!(ws.run(&["gate", "resolve", &gate]).code, 0);
    let r = ws.run(&["gate", "check", &gate, "--json"]);
    let v: Value = serde_json::from_str(&r.stdout).expect("JSON");
    assert_eq!(v["satisfied"], true);
    assert_eq!(v["state"], "satisfied");
}

/// A gate under `--readonly` cannot be resolved: the write is refused before it
/// reaches the store.
#[test]
fn resolve_is_refused_under_readonly() {
    let ws = Ws::new("ro");
    let gate = ws.create_gate("approval");
    let r = ws.run(&["gate", "resolve", &gate, "--readonly"]);
    assert_ne!(r.code, 0, "readonly resolve must fail");
    // And the gate is still open.
    let check = ws.run(&["gate", "check", &gate]).stdout;
    assert!(check.contains("still waiting"), "the gate must be untouched: {check}");
}

/// `show` lists the gate's dependents — what is waiting on it.
#[test]
fn show_lists_what_waits_on_the_gate() {
    let ws = Ws::new("show");
    let gate = ws.create_gate("review");
    let work = ws.quick("merge the branch");
    assert_eq!(ws.run(&["dep", "add", &work, &gate]).code, 0);

    let r = ws.run(&["gate", "show", &gate, "--json"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).expect("JSON");
    let dependents = v["dependents"].as_array().expect("dependents present");
    assert!(
        dependents.iter().any(|d| d["issue_id"] == Value::String(work.clone())),
        "the waiting work should be a dependent: {v}"
    );
}

/// Pointing a gate command at an ordinary issue is a named error, not a
/// back-door `bd close`. Exit 1, and the message says "gate".
#[test]
fn a_gate_command_refuses_a_non_gate() {
    let ws = Ws::new("nongate");
    let task = ws.quick("a plain task");

    let r = ws.run(&["gate", "resolve", &task]);
    assert_eq!(r.code, 1, "resolving a non-gate is a failure, not a stub or cap gap");
    assert!(r.stderr.contains("not a gate"), "message should say so: {}", r.stderr);

    // The task was not closed by the refused resolve.
    let show = ws.run(&["show", &task]).stdout;
    assert!(!show.contains("closed"), "the task must be untouched: {show}");

    // And `check`/`show` refuse it too.
    assert_eq!(ws.run(&["gate", "check", &task]).code, 1);
    assert_eq!(ws.run(&["gate", "show", &task]).code, 1);
}

/// A missing id is a not-found failure (exit 1), not a stub (64) or a capability
/// gap (2).
#[test]
fn a_missing_gate_is_exit_one() {
    let ws = Ws::new("missing");
    let r = ws.run(&["gate", "check", "t-nope"]);
    assert_eq!(r.code, 1, "not-found is a real failure");
    assert!(r.stderr.contains("not found"), "{}", r.stderr);
}

/// Resolving a gate twice is idempotent: the second call reports it is already
/// resolved and succeeds, rather than re-closing and bumping timestamps.
#[test]
fn resolving_twice_is_idempotent() {
    let ws = Ws::new("twice");
    let gate = ws.create_gate("once");

    assert_eq!(ws.run(&["gate", "resolve", &gate]).code, 0);
    let second = ws.run(&["gate", "resolve", &gate]);
    assert_eq!(second.code, 0, "a second resolve is not an error");
    assert!(
        second.stdout.contains("already resolved"),
        "the second resolve should say so: {}",
        second.stdout
    );

    let r = ws.run(&["gate", "resolve", &gate, "--json"]);
    let v: Value = serde_json::from_str(&r.stdout).expect("JSON");
    assert_eq!(v["already_resolved"], true);
}
