//! `bd mol …`, end to end through the real binary and a real sqlite database.
//!
//! The formula compiler is tested in `bd-formula`, and `bd cook`'s wiring in
//! `cook_cli`. What these tests pin is the *molecule* lifecycle: that `seed`
//! plants a dormant container recording its formula, that `pour` grows it into
//! real `parent-child` children with the blocked cache recomputed (so `bd ready`
//! is right on the next call), that a second pour is refused, and that the
//! destroy/collapse paths (`burn`, `squash`) and the honest exit codes hold.

use std::path::PathBuf;
use std::process::Command;

struct Ws(PathBuf);

impl Ws {
    fn new(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-mol-{tag}-{}-{:?}",
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
            .env("BEADS_ACTOR", "mol-test")
            .env("NO_COLOR", "1")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    /// Write a formula into the workspace formula dir so `bd mol seed <name>`
    /// resolves it by name — the path callers actually use.
    fn write_formula(&self, name: &str, body: &str) {
        let dir = self.0.join(".beads").join("formulas");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.formula.toml")), body).unwrap();
    }

    /// Seed a molecule and return its id, parsed from `--json`.
    fn seed(&self, template: &str) -> String {
        let (out, code) = self.run(&["--json", "mol", "seed", template]);
        assert_eq!(code, 0, "seed {template}: {out}");
        first_id(&out)
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// The first `"id": "…"` in a `--json` blob.
fn first_id(json: &str) -> String {
    let key = "\"id\"";
    let i = json.find(key).unwrap_or_else(|| panic!("no id in: {json}"));
    let rest = &json[i + key.len()..];
    let start = rest.find('"').unwrap() + 1;
    let end = rest[start..].find('"').unwrap();
    rest[start..start + end].to_string()
}

const CHAIN: &str = r#"
formula = "chain"
version = 1
[[steps]]
id = "design"
title = "Design the thing"
[[steps]]
id = "build"
title = "Build the thing"
needs = ["design"]
"#;

const SINGLE: &str = r#"
formula = "single"
version = 1
[[steps]]
id = "only"
title = "The only step"
"#;

#[test]
fn seed_plants_a_dormant_molecule_then_pour_grows_it() {
    let ws = Ws::new("seedpour");
    ws.write_formula("chain", CHAIN);
    let mol = ws.seed("chain");

    // Seeded but not poured: a Molecule-typed container, no steps yet.
    let (show, _) = ws.run(&["mol", "show", &mol]);
    assert!(show.contains("poured: false"), "{show}");
    assert!(!show.contains("Design the thing"), "no steps before pour: {show}");
    // The container itself is not claimable work.
    let (ready, _) = ws.run(&["ready"]);
    assert!(!ready.contains(&mol), "a molecule is not claimable work: {ready}");

    // Pour materializes the steps.
    let (out, code) = ws.run(&["mol", "pour", &mol]);
    assert_eq!(code, 0, "{out}");
    let (show, _) = ws.run(&["mol", "show", &mol]);
    assert!(show.contains("poured: true"), "{show}");
    assert!(show.contains("Design the thing"), "{show}");
    assert!(show.contains("Build the thing"), "{show}");

    // The part that matters: the blocked cache was recomputed as part of pour,
    // so `bd ready` is right immediately. `design` is free, `build` blocks on it.
    let (ready, _) = ws.run(&["ready"]);
    assert!(ready.contains("Design the thing"), "design should be ready: {ready}");
    assert!(
        !ready.contains("Build the thing"),
        "build blocks on design and must not be ready: {ready}"
    );
    let (blocked, _) = ws.run(&["blocked"]);
    assert!(blocked.contains("Build the thing"), "build should be blocked: {blocked}");
}

#[test]
fn pouring_twice_is_refused() {
    let ws = Ws::new("twice");
    ws.write_formula("single", SINGLE);
    let mol = ws.seed("single");

    assert_eq!(ws.run(&["mol", "pour", &mol]).1, 0);
    let (out, code) = ws.run(&["mol", "pour", &mol]);
    assert_eq!(code, 1, "a second pour must fail, not duplicate steps: {out}");

    // Exactly one copy of the step exists.
    let (list, _) = ws.run(&["list"]);
    assert_eq!(
        list.matches("The only step").count(),
        1,
        "the second pour duplicated the step: {list}"
    );
}

#[test]
fn wisp_is_ephemeral_and_promotable() {
    let ws = Ws::new("wisp");
    let (out, code) = ws.run(&["--json", "mol", "wisp", "scratch", "note"]);
    assert_eq!(code, 0, "{out}");
    let wisp = first_id(&out);
    assert!(out.contains("\"ephemeral\": true"), "a wisp must be ephemeral: {out}");

    // Ephemeral beads are excluded from claimable work.
    let (ready, _) = ws.run(&["ready"]);
    assert!(!ready.contains(&wisp), "a wisp must not be claimable: {ready}");

    // `bd promote` turns it into a real, claimable bead.
    assert_eq!(ws.run(&["promote", &wisp]).1, 0);
    let (ready, _) = ws.run(&["ready"]);
    assert!(ready.contains(&wisp), "a promoted wisp should be ready: {ready}");
}

#[test]
fn an_empty_wisp_title_is_refused() {
    let ws = Ws::new("wispempty");
    assert_eq!(ws.run(&["mol", "wisp"]).1, 1, "a wisp needs a title");
}

#[test]
fn burn_removes_the_molecule_and_its_steps() {
    let ws = Ws::new("burn");
    ws.write_formula("chain", CHAIN);
    let mol = ws.seed("chain");
    ws.run(&["mol", "pour", &mol]);

    // Two steps plus the container exist.
    assert_eq!(ws.run(&["list", "--all"]).0.matches("the thing").count(), 2);

    let (out, code) = ws.run(&["mol", "burn", &mol]);
    assert_eq!(code, 0, "{out}");

    // The container is gone, and so are its children.
    assert_eq!(ws.run(&["mol", "show", &mol]).1, 1, "molecule should be gone");
    let (list, _) = ws.run(&["list", "--all"]);
    assert!(!list.contains("Design the thing"), "children should be gone: {list}");
    assert!(!list.contains("Build the thing"), "children should be gone: {list}");
}

#[test]
fn squash_collapses_the_molecule_into_a_digest() {
    let ws = Ws::new("squash");
    ws.write_formula("chain", CHAIN);
    let mol = ws.seed("chain");
    ws.run(&["mol", "pour", &mol]);

    let (out, code) = ws.run(&["--json", "mol", "squash", &mol]);
    assert_eq!(code, 0, "{out}");

    // The molecule and its steps are gone; a closed digest replaces them.
    assert_eq!(ws.run(&["mol", "show", &mol]).1, 1, "molecule should be gone");
    let (list, _) = ws.run(&["list", "--all"]);
    assert!(list.contains("Digest: chain"), "a digest should remain: {list}");
    assert!(!list.contains("Design the thing"), "steps should be gone: {list}");
}

#[test]
fn bond_relates_molecules_and_guards_its_input() {
    let ws = Ws::new("bond");
    ws.write_formula("single", SINGLE);
    let a = ws.seed("single");
    let b = ws.seed("single");

    assert_eq!(ws.run(&["mol", "bond", &a, &b]).1, 0);

    // Fewer than two is a usage error.
    assert_eq!(ws.run(&["mol", "bond", &a]).1, 1, "bond needs two");

    // A non-molecule is rejected by name.
    let (out, _) = ws.run(&["--json", "q", "just a bead"]);
    let bead = first_id(&out);
    let (msg, code) = ws.run(&["mol", "bond", &a, &bead]);
    assert_eq!(code, 1, "bonding a non-molecule must fail: {msg}");
}

#[test]
fn mol_ready_shows_molecules_with_claimable_work() {
    let ws = Ws::new("molready");
    ws.write_formula("chain", CHAIN);
    let mol = ws.seed("chain");

    // Dormant: no claimable work yet.
    assert!(!ws.run(&["mol", "ready"]).0.contains(&mol));

    ws.run(&["mol", "pour", &mol]);
    // Poured: `design` is claimable, so the molecule is ready.
    let (out, _) = ws.run(&["mol", "ready"]);
    assert!(out.contains(&mol), "molecule with a ready step should show: {out}");
}

#[test]
fn distill_is_registered_but_not_ported() {
    let ws = Ws::new("distill");
    ws.write_formula("single", SINGLE);
    let mol = ws.seed("single");
    assert_eq!(
        ws.run(&["mol", "distill", &mol]).1,
        64,
        "distill needs --var/--output the args do not carry yet; it stays exit 64"
    );
}

#[test]
fn seed_error_paths_carry_distinct_exit_codes() {
    let ws = Ws::new("seederr");

    // A formula that does not exist is a plain failure.
    assert_eq!(ws.run(&["mol", "seed", "nope"]).1, 1, "missing formula → exit 1");

    // A required, default-less variable cannot be bound with no `--var`; that is
    // a failure (exit 1), naming the variable — not a half-made molecule.
    ws.write_formula(
        "needsvar",
        "formula = \"needsvar\"\nversion = 1\n[vars.name]\nrequired = true\n\
         [[steps]]\nid = \"a\"\ntitle = \"Do {{name}}\"\n",
    );
    assert_eq!(ws.run(&["mol", "seed", "needsvar"]).1, 1, "required var → exit 1");
    // And nothing was planted.
    assert!(ws.run(&["list", "--all"]).0.contains("No matching"), "a failed seed left a molecule");

    // An unsupported construct (`extends`) is a capability gap, not a broken
    // formula: exit 2, the same as `bd cook`.
    ws.write_formula(
        "ext",
        "formula = \"ext\"\nversion = 1\nextends = [\"parent\"]\n\
         [[steps]]\nid = \"a\"\ntitle = \"A\"\n",
    );
    assert_eq!(ws.run(&["mol", "seed", "ext"]).1, 2, "unsupported → exit 2");
}
