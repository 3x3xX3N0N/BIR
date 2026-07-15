//! `bd swarm …` and `bd rules …`, end to end through the real binary.
//!
//! What these pin is the honest boundary this port draws: the two commands with
//! real substrate (`swarm validate`, `rules audit`) do genuine work and exit 0;
//! the ones without (`swarm status`/`create`, `rules compact`) refuse cleanly
//! with exit 64 rather than pretending. `swarm list` is a real query that is
//! simply empty until something creates a swarm.

use std::path::PathBuf;
use std::process::Command;

struct Ws(PathBuf);

impl Ws {
    fn new(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-swarm-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        let ws = Ws(std::fs::canonicalize(&p).unwrap());
        assert_eq!(ws.run(&["init", "--prefix", "t"]).1, 0, "init");
        ws
    }

    /// Returns (stdout, exit code). stderr is inherited-free (captured), which is
    /// where stubs print — so tests key on the exit code for those.
    fn run(&self, args: &[&str]) -> (String, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", self.0.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "swarm-test")
            .env("NO_COLOR", "1")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn write(&self, rel: &str, body: &str) -> String {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, body).unwrap();
        p.to_str().unwrap().to_string()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

const WORKFLOW: &str = r#"
formula = "release"
version = 1
type = "workflow"
description = "ship it"
[[steps]]
id = "build"
title = "Build"
[[steps]]
id = "ship"
title = "Ship"
needs = ["build"]
"#;

const CONVOY: &str = r#"
formula = "big-swarm"
version = 1
type = "convoy"
[[steps]]
id = "a"
title = "Do A"
"#;

// ---------------------------------------------------------------------------
// swarm validate
// ---------------------------------------------------------------------------

#[test]
fn validate_accepts_a_valid_workflow_spec() {
    let ws = Ws::new("valid");
    let f = ws.write("release.formula.toml", WORKFLOW);
    let (out, code) = ws.run(&["swarm", "validate", &f]);
    assert_eq!(code, 0, "a valid spec must validate: {out}");
    assert!(out.contains("valid"), "{out}");
    assert!(out.contains("release"), "{out}");
}

#[test]
fn validate_reports_a_convoy_as_valid_but_not_cookable() {
    let ws = Ws::new("convoy");
    let f = ws.write("big-swarm.formula.toml", CONVOY);
    // A convoy is a well-formed formula; this port just does not cook it yet.
    let (out, code) = ws.run(&["swarm", "validate", &f]);
    assert_eq!(code, 0, "a convoy spec is still structurally valid: {out}");
    assert!(out.contains("convoy"), "should name the type: {out}");
    assert!(
        out.to_lowercase().contains("workflow"),
        "should note only workflow cooks: {out}"
    );
}

#[test]
fn validate_json_marks_cookability() {
    let ws = Ws::new("vjson");
    let wf = ws.write("release.formula.toml", WORKFLOW);
    let (out, code) = ws.run(&["--json", "swarm", "validate", &wf]);
    assert_eq!(code, 0);
    assert!(out.contains("\"valid\": true"), "{out}");
    assert!(out.contains("\"cookable\": true"), "{out}");

    let cv = ws.write("big-swarm.formula.toml", CONVOY);
    let (out, code) = ws.run(&["--json", "swarm", "validate", &cv]);
    assert_eq!(code, 0);
    assert!(out.contains("\"cookable\": false"), "{out}");
    assert!(out.contains("\"type\": \"convoy\""), "{out}");
}

#[test]
fn validate_rejects_a_structurally_broken_spec() {
    let ws = Ws::new("broken");
    // An edge to a step that does not exist — caught by the formula parser.
    let f = ws.write(
        "broken.formula.toml",
        r#"
formula = "broken"
version = 1
[[steps]]
id = "a"
title = "A"
needs = ["ghost"]
"#,
    );
    let (_out, code) = ws.run(&["swarm", "validate", &f]);
    assert_eq!(code, 1, "a broken spec is bad input (exit 1), not 64/2");
}

#[test]
fn validate_rejects_unparseable_toml() {
    let ws = Ws::new("garbage");
    let f = ws.write("garbage.formula.toml", "this is not = = toml [[[");
    let (_out, code) = ws.run(&["swarm", "validate", &f]);
    assert_eq!(code, 1);
}

// ---------------------------------------------------------------------------
// swarm list / status / create
// ---------------------------------------------------------------------------

#[test]
fn list_is_empty_but_succeeds() {
    let ws = Ws::new("list");
    let (out, code) = ws.run(&["swarm", "list"]);
    assert_eq!(code, 0, "an empty swarm list is not an error: {out}");
    assert!(out.contains("No swarms"), "{out}");
}

#[test]
fn list_json_is_an_empty_array() {
    let ws = Ws::new("listjson");
    let (out, code) = ws.run(&["--json", "swarm", "list"]);
    assert_eq!(code, 0);
    assert_eq!(out, "[]", "{out}");
}

#[test]
fn status_is_an_honest_stub() {
    let ws = Ws::new("status");
    let (_out, code) = ws.run(&["swarm", "status"]);
    assert_eq!(code, 64, "status has no substrate yet; must be an honest stub");
}

#[test]
fn create_is_an_honest_stub() {
    let ws = Ws::new("create");
    let (_out, code) = ws.run(&["swarm", "create", "my-swarm"]);
    assert_eq!(code, 64, "create needs the molecule substrate; honest stub");
}

// ---------------------------------------------------------------------------
// rules audit / compact
// ---------------------------------------------------------------------------

#[test]
fn audit_on_a_workspace_with_no_rules_is_ok() {
    let ws = Ws::new("norules");
    let (out, code) = ws.run(&["rules", "audit"]);
    assert_eq!(code, 0, "a missing rules dir means zero rules, not an error");
    assert!(out.contains("No rule files"), "{out}");
}

#[test]
fn audit_counts_rules_and_flags_a_contradiction() {
    let ws = Ws::new("contradict");
    // Two rules over the same scope with antonym directives (spawn vs reuse).
    ws.write(
        ".claude/rules/spawn.md",
        "# Agents\n**Do:** spawn a new agent for each task\n",
    );
    ws.write(
        ".claude/rules/reuse.md",
        "# Agents\n**Do:** reuse the existing agent for each task\n",
    );

    let (out, code) = ws.run(&["rules", "audit"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Total rules:      2"), "{out}");
    assert!(out.contains("Contradictions:"), "{out}");
    assert!(out.contains("spawn.md"), "{out}");
    assert!(out.contains("reuse.md"), "{out}");
}

#[test]
fn audit_json_is_structured() {
    let ws = Ws::new("auditjson");
    ws.write(
        ".claude/rules/spawn.md",
        "# Agents\n**Do:** spawn a new agent for each task\n",
    );
    ws.write(
        ".claude/rules/reuse.md",
        "# Agents\n**Do:** reuse the existing agent for each task\n",
    );

    let (out, code) = ws.run(&["--json", "rules", "audit"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
    assert_eq!(v["total_rules"], 2, "{out}");
    assert!(
        v["contradictions"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "expected a contradiction: {out}"
    );
}

#[test]
fn compact_is_an_honest_stub() {
    let ws = Ws::new("compact");
    let (_out, code) = ws.run(&["rules", "compact"]);
    assert_eq!(code, 64, "compact has no safe flag surface here; honest stub");
}
