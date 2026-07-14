//! `bd doctor`, Data & Config family — driven through the real binary.
//!
//! The checks' own logic is covered next to the code, in
//! `doctor::checks::graph`, where a real SQLite store is built through the real
//! write paths and the derived `is_blocked` is diffed against the stored column.
//! What *these* tests establish is the other half: that the checks are wired into
//! `bd doctor`, that they say the right thing about a workspace an agent actually
//! built with the CLI, and that they name the beads they are complaining about
//! rather than counting them.
//!
//! # Two things these tests deliberately do not assert
//!
//! **The exit code**, and `doctor.ok`. Nine families are landing checks into one
//! registry, and this workspace is a bare temp directory with no git repo, no
//! remote and no hooks — so other families have every right to fail here. A test
//! that asserted on the run's *overall* verdict would be asserting on eight other
//! people's checks, and would break every time one of them landed. Everything
//! below addresses a check by name.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Ws {
    dir: PathBuf,
}

impl Ws {
    /// A fresh workspace on disk, initialized by the real `bd init`.
    fn new(tag: &str) -> Ws {
        Ws {
            dir: fresh(tag, true),
        }
    }

    /// A directory that is *not* a workspace. Doctor's real job.
    fn bare(tag: &str) -> Ws {
        Ws {
            dir: fresh(tag, false),
        }
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", self.dir.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "tester")
            .env("NO_COLOR", "1")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    #[track_caller]
    fn ok(&self, args: &[&str]) -> String {
        let (stdout, stderr, code) = self.run(args);
        assert_eq!(code, 0, "bd {args:?} failed ({code}): {stderr}\n{stdout}");
        stdout
    }

    /// `bd doctor --json`, **whatever it exits with**.
    ///
    /// A doctor that found a real problem exits 1, and that is the case half of
    /// these tests are about. Asserting exit 0 here would make the interesting
    /// half unwritable.
    #[track_caller]
    fn doctor(&self, extra: &[&str]) -> Value {
        let mut args = vec!["doctor"];
        args.extend_from_slice(extra);
        args.push("--json");
        let (stdout, stderr, _) = self.run(&args);
        serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!("`bd doctor --json` emitted no JSON ({e}):\nstdout: {stdout}\nstderr: {stderr}")
        })
    }

    #[track_caller]
    fn q(&self, title: &str) -> String {
        let id = self.ok(&["q", title]);
        assert!(id.starts_with("t-"), "unexpected id from `bd q`: {id}");
        id
    }

    /// `bd dep add ISSUE DEPENDS_ON --type T` — ISSUE waits for DEPENDS_ON.
    #[track_caller]
    fn dep(&self, issue: &str, depends_on: &str, ty: &str) {
        self.ok(&["dep", "add", issue, depends_on, "--type", ty]);
    }

    #[track_caller]
    fn close(&self, id: &str, reason: &str) {
        self.ok(&["close", id, "--reason", reason]);
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn fresh(tag: &str, init: bool) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bd-doctor-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    if init {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", dir.to_str().unwrap(), "init", "--prefix", "t"])
            .output()
            .expect("bd init");
        assert!(out.status.success(), "bd init failed");
    }
    dir
}

/// One check, by name. The name is the key agents grep for, so addressing checks
/// by it is also a test that the names have not drifted.
#[track_caller]
fn check<'a>(doc: &'a Value, name: &str) -> &'a Value {
    doc["checks"]
        .as_array()
        .expect("`bd doctor --json` must emit a `checks` array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("`{name}` is not registered in `bd doctor`:\n{doc:#}"))
}

#[track_caller]
fn status(doc: &Value, name: &str) -> String {
    check(doc, name)["status"].as_str().unwrap().to_string()
}

#[track_caller]
fn detail(doc: &Value, name: &str) -> String {
    let c = check(doc, name);
    c["detail"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "`{name}` said `{}` and named nothing — a finding you cannot act on",
                c["message"]
            )
        })
        .to_string()
}

/// Every check this family owns. If one is dropped, the registry test below
/// notices; if one is renamed, every other test in this file notices.
const FAMILY: [&str; 6] = [
    "blocked-cache",
    "dependency-cycles",
    "orphaned-dependencies",
    "parent-child-coherence",
    "duplicate-issues",
    "stuck-conditional-paths",
];

/// The full graph, built through the CLI: every construct that gates readiness,
/// exactly once. Returns (blocker, deep child) — the two ends of the chain.
fn build_everything(ws: &Ws) -> (String, String) {
    let e = ws.q("E: the blocker");
    let d = ws.q("D: gated by E");
    let c = ws.q("C: child of D");
    let b = ws.q("B: child of C");
    let a = ws.q("A: child of B");
    ws.dep(&d, &e, "blocks");
    ws.dep(&c, &d, "parent-child");
    ws.dep(&b, &c, "parent-child");
    ws.dep(&a, &b, "parent-child");

    let deploy = ws.q("Deploy");
    let rollback = ws.q("Roll back the deploy");
    ws.dep(&rollback, &deploy, "conditional-blocks");

    let flaky = ws.q("Flaky migration");
    let repair = ws.q("Repair the migration");
    ws.dep(&repair, &flaky, "conditional-blocks");

    let spawn = ws.q("Fan out the work");
    let kid = ws.q("One of the fanned-out pieces");
    let collect = ws.q("Collect the results");
    ws.dep(&kid, &spawn, "parent-child");
    ws.dep(&collect, &spawn, "waits-for");

    ws.close(&flaky, "failed");

    (e, a)
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/// The family is registered, every check lands in Data & Config, and every check
/// says something. A check that is not in the registry protects nothing.
#[test]
fn every_check_in_the_family_is_registered_under_data_and_config() {
    let ws = Ws::new("registry");
    ws.q("Something to look at");

    let doc = ws.doctor(&[]);
    for name in FAMILY {
        let c = check(&doc, name);
        assert_eq!(c["category"], "data", "{name} is filed in the wrong family");
        assert!(
            c["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{name} reported nothing at all"
        );
    }
}

// ---------------------------------------------------------------------------
// The blocked cache
// ---------------------------------------------------------------------------

/// The check must be **right** about a real graph before it can be trusted to be
/// right about a broken one.
///
/// `blocked-cache` re-derives `is_blocked` from the edges and compares it to the
/// column. Here the column was written by the real fixpoint, over a graph
/// containing every construct that gates readiness — a three-deep containment
/// chain, both branches of `conditional-blocks`, and a `waits-for` gate. The two
/// implementations share no code, so if either is wrong about what `bd ready`
/// means, this reports an inconsistency that is not there.
///
/// `bd recompute-blocked` is the independent second opinion: it rebuilds the cache
/// in SQL and reports how many rows *changed*. Zero, and doctor saying `ok`, are
/// two different programs agreeing that the cache is at the fixpoint.
#[test]
fn the_blocked_cache_agrees_with_the_graph_across_every_gating_construct() {
    let ws = Ws::new("blocked-clean");
    let (blocker, deep_child) = build_everything(&ws);

    // The cache is not trivially empty: the deep end of the containment chain is
    // gated by a blocker three levels above it. If it were, "consistent" would
    // mean nothing.
    let blocked: Vec<String> = ws
        .doctor(&[])["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert!(blocked.contains(&"blocked-cache".to_string()));
    let listed = ws.ok(&["blocked", "--limit", "0", "--json"]);
    assert!(
        listed.contains(&deep_child),
        "the graph under test does not actually block anything: {listed}"
    );

    let doc = ws.doctor(&[]);
    assert_eq!(
        status(&doc, "blocked-cache"),
        "ok",
        "the derivation disagrees with the real fixpoint: {:#}",
        check(&doc, "blocked-cache")
    );

    let recompute: Value = serde_json::from_str(&ws.ok(&["recompute-blocked", "--json"])).unwrap();
    assert_eq!(
        recompute["updated"], 0,
        "the store itself says the cache was stale, and doctor said it was fine"
    );

    // Now move the graph and ask again. Closing the root blocker frees the entire
    // chain, which is the transitive unblock a single-pass recompute gets wrong —
    // so this is where a derivation that quietly stopped propagating would show up.
    ws.close(&blocker, "done");
    let doc = ws.doctor(&[]);
    assert_eq!(
        status(&doc, "blocked-cache"),
        "ok",
        "after the chain unblocked: {:#}",
        check(&doc, "blocked-cache")
    );
    let recompute: Value = serde_json::from_str(&ws.ok(&["recompute-blocked", "--json"])).unwrap();
    assert_eq!(recompute["updated"], 0);
}

/// A healthy cache is not a thing to repair.
///
/// `--fix` only calls `repair()` for findings that are not `Ok`, so a clean
/// `blocked-cache` must not appear in the repairs list at all. A doctor that
/// "repaired" a workspace with nothing wrong with it teaches people that its
/// repairs mean nothing.
#[test]
fn fix_does_not_claim_to_have_repaired_a_healthy_cache() {
    let ws = Ws::new("fix-noop");
    build_everything(&ws);

    let doc = ws.doctor(&["--fix"]);
    assert_eq!(status(&doc, "blocked-cache"), "ok");

    let repairs = doc["repairs"].as_array().cloned().unwrap_or_default();
    assert!(
        !repairs.iter().any(|r| r["check"] == "blocked-cache"),
        "doctor --fix repaired a cache that was already correct: {repairs:#?}"
    );

    // And nothing it did while other families were repairing broke the cache.
    let recompute: Value = serde_json::from_str(&ws.ok(&["recompute-blocked", "--json"])).unwrap();
    assert_eq!(recompute["updated"], 0);
}

// ---------------------------------------------------------------------------
// The graph checks that a CLI-built workspace *can* trip
// ---------------------------------------------------------------------------

/// **The bead that will never move again.**
///
/// `B conditional-blocks A` means "run B only if A fails". When A closes
/// *successfully* the failure path is moot — and the store deliberately leaves B
/// blocked rather than closing a bead nobody asked it to close. So B sits in
/// `bd blocked` forever, looking exactly like ordinary work waiting its turn.
/// Nothing errors. Nothing logs. This check is the only thing in the program that
/// says "that one is not waiting, it is stranded".
#[test]
fn a_rollback_stranded_by_a_successful_deploy_is_named() {
    let ws = Ws::new("stranded");
    let deploy = ws.q("Deploy");
    let rollback = ws.q("Roll back the deploy");
    let flaky = ws.q("Flaky migration");
    let repair = ws.q("Repair the migration");
    ws.dep(&rollback, &deploy, "conditional-blocks");
    ws.dep(&repair, &flaky, "conditional-blocks");

    // Nothing has closed yet: both failure paths are still live possibilities.
    assert_eq!(status(&ws.doctor(&[]), "stuck-conditional-paths"), "ok");

    ws.close(&deploy, "done"); // succeeded — the rollback is now moot
    ws.close(&flaky, "failed"); // failed — the repair is now *ready*

    let doc = ws.doctor(&[]);
    assert_eq!(status(&doc, "stuck-conditional-paths"), "warning");

    let d = detail(&doc, "stuck-conditional-paths");
    assert!(d.contains(&rollback), "the stranded bead must be named: {d}");
    assert!(
        !d.contains(&repair),
        "the repair's subject really did fail, so the repair is ready, not stranded: {d}"
    );

    // …and the cache is not what is wrong here. The rollback is *correctly*
    // marked blocked; the point is that it will be marked blocked forever.
    assert_eq!(status(&doc, "blocked-cache"), "ok");
    assert!(ws.ok(&["ready", "--limit", "0", "--json"]).contains(&repair));
}

/// Containment is a tree. Two parents makes a recursive `--parent` descent count
/// the subtree twice, and lets blocked-ness arrive from an epic the author never
/// put the bead in.
#[test]
fn a_bead_with_two_parents_is_reported() {
    let ws = Ws::new("twoparents");
    let epic_a = ws.q("Epic A");
    let epic_b = ws.q("Epic B");
    let child = ws.q("The contested child");

    ws.dep(&child, &epic_a, "parent-child");
    assert_eq!(status(&ws.doctor(&[]), "parent-child-coherence"), "ok");

    ws.dep(&child, &epic_b, "parent-child");

    let doc = ws.doctor(&[]);
    assert_eq!(status(&doc, "parent-child-coherence"), "warning");
    let d = detail(&doc, "parent-child-coherence");
    assert!(d.contains(&child), "{d}");
    assert!(d.contains(&epic_a) && d.contains(&epic_b), "{d}");
}

/// An epic reported done while its children are still open. Nothing gates,
/// nothing errors, and every summary in the program says the epic is finished.
#[test]
fn an_open_child_under_a_closed_parent_is_reported() {
    let ws = Ws::new("abandoned");
    let epic = ws.q("Ship the thing");
    let child = ws.q("The bit nobody did");
    ws.dep(&child, &epic, "parent-child");

    assert_eq!(status(&ws.doctor(&[]), "parent-child-coherence"), "ok");

    ws.close(&epic, "done");

    let doc = ws.doctor(&[]);
    assert_eq!(status(&doc, "parent-child-coherence"), "warning");
    let d = detail(&doc, "parent-child-coherence");
    assert!(d.contains(&child) && d.contains(&epic), "{d}");

    // Closing the parent must not have gated the child — the child is *ready*,
    // which is exactly why nobody notices.
    assert!(ws.ok(&["ready", "--limit", "0", "--json"]).contains(&child));
}

/// Two beads that describe the same work. Two agents will do it twice.
#[test]
fn duplicate_issues_are_named_not_merely_counted() {
    let ws = Ws::new("dupes");
    let one = ws.q("Rewrite the parser");
    assert_eq!(status(&ws.doctor(&[]), "duplicate-issues"), "ok");

    let two = ws.q("Rewrite the parser");
    assert_ne!(one, two, "two `bd q`s must mint two beads");

    let doc = ws.doctor(&[]);
    assert_eq!(status(&doc, "duplicate-issues"), "warning");
    let d = detail(&doc, "duplicate-issues");
    assert!(d.contains(&one) && d.contains(&two), "{d}");

    // Closing one of them settles it: duplicated history is not a problem to act
    // on, only duplicated *work* is.
    ws.close(&two, "duplicate");
    assert_eq!(status(&ws.doctor(&[]), "duplicate-issues"), "ok");
}

/// The write path refuses to create a cycle or a dangling edge, so on a workspace
/// built entirely through the CLI these two must be clean — and must *say* they
/// looked, rather than staying silent.
///
/// They exist for the rows the CLI did not write: a merge, a pull, an import from
/// another beads implementation, a hand-edited database.
#[test]
fn cycles_and_orphaned_edges_are_clean_on_a_graph_the_cli_built() {
    let ws = Ws::new("sound");
    build_everything(&ws);

    let doc = ws.doctor(&[]);
    assert_eq!(status(&doc, "dependency-cycles"), "ok");
    assert_eq!(status(&doc, "orphaned-dependencies"), "ok");

    // The write path really is what is holding the line here.
    let a = ws.q("A");
    let b = ws.q("B");
    ws.dep(&b, &a, "blocks");
    let (_, stderr, code) = ws.run(&["dep", "add", &a, &b, "--type", "blocks"]);
    assert_eq!(code, 1, "a cycle must be refused at the door");
    assert!(stderr.to_lowercase().contains("cycle"), "{stderr}");
    assert_eq!(status(&ws.doctor(&[]), "dependency-cycles"), "ok");
}

// ---------------------------------------------------------------------------
// The rule that shapes doctor
// ---------------------------------------------------------------------------

/// **Doctor runs on workspaces too broken to open — that is the job.**
///
/// Every check in this family needs a store. Not getting one is a warning *about
/// the check*, never an `Ok`: the check did not establish that the graph is fine,
/// and saying `ok` here is how a diagnostic quietly stops diagnosing while still
/// reporting as coverage.
///
/// Upstream's version of this check returns `StatusOK` with the message
/// "No database yet". That is the bug this assertion exists to prevent.
#[test]
fn outside_a_workspace_every_check_says_it_could_not_look() {
    let ws = Ws::bare("nowhere");
    let doc = ws.doctor(&[]);

    for name in FAMILY {
        let c = check(&doc, name);
        assert_eq!(
            c["status"], "unknown",
            "`{name}` reported `{}` with no database to look at — an undeterminable \
             check that reports ok is worse than no check, because it reports as coverage",
            c["status"]
        );
        assert_eq!(c["message"], "could not check");
        assert!(
            c["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "`{name}` must say *why* it could not look"
        );
    }

    // And `--fix` must not pretend it repaired a workspace it could not read.
    let doc = ws.doctor(&["--fix"]);
    for r in doc["repairs"].as_array().cloned().unwrap_or_default() {
        if FAMILY.contains(&r["check"].as_str().unwrap_or("")) {
            assert_ne!(
                r["outcome"], "fixed",
                "`{}` claims to have fixed a workspace that does not exist: {r:#}",
                r["check"]
            );
        }
    }
}
