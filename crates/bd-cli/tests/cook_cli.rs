//! `bd cook`, end to end through the real binary and a real database.
//!
//! The compiler is tested exhaustively in `bd-formula`; what these tests pin is
//! the *wiring* — that a cooked plan becomes real issues with real dependencies,
//! that the blocked cache is recomputed so `bd ready` is correct immediately, and
//! that `--dry-run` writes nothing.

use std::path::PathBuf;
use std::process::Command;

struct Ws(PathBuf);

impl Ws {
    fn new(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-cook-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        let ws = Ws(std::fs::canonicalize(&p).unwrap());
        assert_eq!(ws.run(&["init", "--prefix", "t"]).1, 0, "init");
        ws
    }

    fn run(&self, args: &[&str]) -> (String, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", self.0.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "cook-test")
            .env("NO_COLOR", "1")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn write_formula(&self, name: &str, body: &str) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, body).unwrap();
        p.to_str().unwrap().to_string()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

const CHAIN: &str = r#"
formula = "chain"
version = 1
[vars.name]
required = true
[[steps]]
id = "design"
title = "Design {{name}}"
[[steps]]
id = "build"
title = "Build {{name}}"
needs = ["design"]
"#;

#[test]
fn cook_creates_the_graph_and_ready_is_correct_immediately() {
    let ws = Ws::new("chain");
    let f = ws.write_formula("chain.formula.toml", CHAIN);

    let (_out, code) = ws.run(&["cook", &f, "--var", "name=auth"]);
    assert_eq!(code, 0);

    // Both issues exist.
    let (list, _) = ws.run(&["list"]);
    assert!(list.contains("Design auth"), "{list}");
    assert!(list.contains("Build auth"), "{list}");

    // And — the part that matters — the blocked cache was recomputed as part of
    // cooking, so `bd ready` is right on the very next call. `design` is free,
    // `build` blocks on it. A cook that skipped the recompute would show both, or
    // neither, with no error.
    let (ready, _) = ws.run(&["ready"]);
    assert!(ready.contains("Design auth"), "design should be ready: {ready}");
    assert!(
        !ready.contains("Build auth"),
        "build blocks on design and must not be ready: {ready}"
    );

    let (blocked, _) = ws.run(&["blocked"]);
    assert!(blocked.contains("Build auth"), "build should be blocked: {blocked}");
}

#[test]
fn dry_run_writes_nothing() {
    let ws = Ws::new("dry");
    let f = ws.write_formula("chain.formula.toml", CHAIN);

    let (out, code) = ws.run(&["cook", &f, "--var", "name=auth", "--dry-run"]);
    assert_eq!(code, 0);
    assert!(out.contains("would create"), "{out}");

    // Nothing landed.
    let (list, _) = ws.run(&["list"]);
    assert!(list.contains("No matching"), "dry-run created issues: {list}");
}

#[test]
fn a_missing_required_var_fails_before_writing_anything() {
    let ws = Ws::new("missingvar");
    let f = ws.write_formula("chain.formula.toml", CHAIN);

    // No --var name.
    let (_out, code) = ws.run(&["cook", &f]);
    assert_ne!(code, 0, "a missing required var must fail the cook");

    let (list, _) = ws.run(&["list"]);
    assert!(list.contains("No matching"), "a failed cook left issues behind: {list}");
}

/// An unsupported construct is a capability gap (exit 2), not a broken formula
/// (exit 1) and not an unbuilt command (exit 64). `extends` parses fine; this
/// port just does not weave it yet.
#[test]
fn an_unsupported_formula_exits_two_not_one() {
    let ws = Ws::new("extends");
    let f = ws.write_formula(
        "child.formula.toml",
        r#"
        formula = "child"
        version = 1
        extends = ["parent"]
        [[steps]]
        id = "a"
        title = "A"
        "#,
    );
    let (_out, code) = ws.run(&["cook", &f]);
    assert_eq!(code, 2, "extends is a capability gap, not a failure");
}
