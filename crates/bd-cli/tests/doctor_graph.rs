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
//!
//! # Why one of these tests writes SQL
//!
//! The check that justifies this whole family — `blocked-cache` — cannot be
//! tripped through the CLI at all, and that is a *compliment* to the store: every
//! write path recomputes `is_blocked` to a fixpoint, `bd import` recomputes,
//! `add_dependency` refuses a cycle before writing, and foreign keys are on. The
//! stale cache it guards against therefore never arrives through `bd`. It arrives
//! from a second writer — a merge, a pull, another beads implementation — landing
//! rows in the table without going through any of that.
//!
//! So the one test that matters most reaches *behind* the store and corrupts the
//! database the way a merge corrupts it: real bytes, on real disk, under a real
//! `bd doctor`. It could have been written by injecting the staleness as data and
//! comparing two sets in-process, but that would have tested the comparison and
//! not the command — and this is precisely the check where a test that reports as
//! coverage without exercising the thing is worse than no test.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

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

    /// The ids the *stored cache* currently says are blocked.
    ///
    /// `bd blocked` filters on `is_blocked = 1` and does not walk the graph, so
    /// this is the column itself, read back through the front door — the same
    /// answer `bd ready` is inverting to decide what to hand an agent.
    #[track_caller]
    fn blocked(&self) -> Vec<String> {
        ids(&self.ok(&["blocked", "--limit", "0", "--json"]))
    }

    #[track_caller]
    fn ready(&self) -> Vec<String> {
        ids(&self.ok(&["ready", "--limit", "0", "--json"]))
    }

    /// Write straight into `.beads/beads.db`, **behind the store's back**.
    ///
    /// This is the only way to produce the input `blocked-cache` exists for. Every
    /// write path in this port recomputes `is_blocked` to a fixpoint, so the
    /// front door cannot leave a stale cache behind however hard it is pushed. A
    /// merge can, an import from another implementation can, a hand-edited
    /// database can — none of which go through `Storage` at all. This call is that
    /// second writer.
    ///
    /// The pool is opened, used and **closed** inside this call. The store caps
    /// itself at one connection because a second concurrent write transaction in
    /// WAL mode fails immediately with `SQLITE_BUSY_SNAPSHOT`, which
    /// `busy_timeout` does not retry — so a test still holding a write connection
    /// when it shells out to `bd` would be measuring that, not the check.
    ///
    /// Returns the rows each statement actually touched. A corruption that
    /// silently matched nothing would make everything below a green test that
    /// proves nothing, which is the exact failure this file is here to stop.
    #[track_caller]
    fn behind_the_stores_back(&self, statements: &[String]) -> Vec<u64> {
        let db = self.dir.join(".beads").join("beads.db");
        assert!(db.exists(), "no database at {}", db.display());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime to reach the database with");

        rt.block_on(async {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(SqliteConnectOptions::new().filename(&db).foreign_keys(true))
                .await
                .expect("open the workspace database directly");

            let mut affected = Vec::new();
            for sql in statements {
                let n = sqlx::query(sql)
                    .execute(&pool)
                    .await
                    .unwrap_or_else(|e| panic!("{sql}\n  -> {e}"))
                    .rows_affected();
                affected.push(n);
            }

            // Before `bd` runs again, and not a line later.
            pool.close().await;
            affected
        })
    }
}

fn ids(json: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(json).expect("a list command must emit JSON");
    let mut out: Vec<String> = v
        .as_array()
        .expect("a list command must emit a JSON array")
        .iter()
        .map(|i| i["id"].as_str().expect("every issue has an id").to_string())
        .collect();
    out.sort();
    out
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

/// **The test the whole family exists for, end to end, on real bytes.**
///
/// A merge, a pull, or another beads implementation lands rows in `issues` and
/// `dependencies` without going through a write path, so `is_blocked` keeps
/// whatever it happened to have. Nothing crashes. Nothing logs. Every command
/// still exits 0. `bd ready` simply starts giving the wrong answer.
///
/// That state is unreachable through the CLI — every write path recomputes the
/// cache to a fixpoint — so this test *is* the second writer: it opens the real
/// `.beads/beads.db` and flips two rows of the cache, in the two opposite
/// directions the bug has.
///
/// * The deep child is genuinely blocked — three levels of `parent-child`
///   propagation up to a blocker that is still open — and the cache is made to
///   say **free**. `bd ready` hands an agent work whose blocker is open.
/// * The blocker itself is genuinely free, and the cache is made to say
///   **blocked**. `bd ready` hides claimable work.
///
/// The first of those two is also what keeps the derivation honest: a check that
/// had quietly stopped propagating blocked-ness down the containment tree would
/// compute the deep child as free, *agree* with the corrupted cache, and report
/// this workspace as healthy.
///
/// Then `--fix` has to actually mend it — and a fresh `bd doctor`, in a fresh
/// process, has to find nothing left to say.
#[test]
fn a_cache_stale_the_way_a_merge_leaves_it_is_caught_in_both_directions_and_repaired() {
    let ws = Ws::new("stale-cache");
    let (blocker, deep_child) = build_everything(&ws);

    // Ground truth first. Without this the test could "pass" against a check that
    // reports staleness on every workspace, or against a `bd ready` that was
    // already wrong before anything was corrupted.
    assert!(
        ws.blocked().contains(&deep_child),
        "the deep child is gated three levels up; if the cache does not say so, \
         the corruption below is not a corruption"
    );
    assert!(ws.ready().contains(&blocker), "the blocker is claimable");
    assert!(!ws.ready().contains(&deep_child));
    assert_eq!(status(&ws.doctor(&[]), "blocked-cache"), "ok");

    // The merge lands. It carried the edges; it did not carry the fixpoint.
    //
    // The `AND is_blocked = <the opposite>` guard is what makes the row counts
    // below mean something: each UPDATE matches only if the stored value really
    // was the truthful one it is about to destroy.
    let touched = ws.behind_the_stores_back(&[
        format!("UPDATE issues SET is_blocked = 0 WHERE id = '{deep_child}' AND is_blocked = 1"),
        format!("UPDATE issues SET is_blocked = 1 WHERE id = '{blocker}' AND is_blocked = 0"),
    ]);
    assert_eq!(
        touched,
        vec![1, 1],
        "the cache was not corrupted — the rows already held what was written to \
         them, so everything below would be asserting about a healthy workspace"
    );

    // The harm, observed from outside the process. This is the failure in the
    // flesh: no error, no exit code, no log line, and `bd ready` is now handing
    // out a bead whose blocker is still open while hiding one that is claimable.
    assert!(
        ws.ready().contains(&deep_child),
        "a stale cache must make `bd ready` hand out blocked work — if it does \
         not, this test is not reproducing the bug it claims to"
    );
    assert!(!ws.ready().contains(&blocker), "…and hide free work");
    assert!(ws.blocked().contains(&blocker));
    assert!(!ws.blocked().contains(&deep_child));

    // Doctor is the only thing in the program that notices.
    let doc = ws.doctor(&[]);
    assert_eq!(
        status(&doc, "blocked-cache"),
        "error",
        "a lying `bd ready` is not untidiness: {:#}",
        check(&doc, "blocked-cache")
    );

    // And it must *name* the beads, on the correct side. "2 issues are corrupt"
    // is a bug report nobody can act on, and naming them on the wrong side sends
    // the reader to look for a blocker that isn't there.
    let d = detail(&doc, "blocked-cache");
    let offered = d
        .lines()
        .find(|l| l.contains("offering"))
        .unwrap_or_else(|| panic!("the finding never says which beads are being handed out:\n{d}"));
    assert!(
        offered.contains(&deep_child) && !offered.contains(&blocker),
        "the bead `bd ready` is wrongly offering is {deep_child}: {offered}"
    );
    let hidden = d
        .lines()
        .find(|l| l.contains("hiding"))
        .unwrap_or_else(|| panic!("the finding never says which beads are being hidden:\n{d}"));
    assert!(
        hidden.contains(&blocker) && !hidden.contains(&deep_child),
        "the bead `bd ready` is wrongly hiding is {blocker}: {hidden}"
    );

    // -- and now mend it ----------------------------------------------------

    let doc = ws.doctor(&["--fix"]);
    let repair = doc["repairs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|r| r["check"] == "blocked-cache")
        .unwrap_or_else(|| panic!("`--fix` did not even try to repair the cache:\n{doc:#}"));
    assert_eq!(
        repair["outcome"], "fixed",
        "the one repair in this program that is always safe did not happen: {repair:#}"
    );

    // Doctor re-runs its checks after a repair, so the report it printed must
    // already describe the mended workspace — otherwise `bd doctor --fix` in a
    // hook fixes the problem and fails the build anyway.
    assert_eq!(
        status(&doc, "blocked-cache"),
        "ok",
        "--fix reported the pre-repair finding: {:#}",
        check(&doc, "blocked-cache")
    );

    // A fresh process, reading the file back off disk: the repair was durable,
    // and the store agrees from the other side that the cache is at the fixpoint.
    assert_eq!(status(&ws.doctor(&[]), "blocked-cache"), "ok");
    let recompute: Value = serde_json::from_str(&ws.ok(&["recompute-blocked", "--json"])).unwrap();
    assert_eq!(
        recompute["updated"], 0,
        "doctor said the cache was mended and the store says it is still stale"
    );

    // The thing that was actually broken is the thing that works again.
    assert!(
        !ws.ready().contains(&deep_child),
        "`bd ready` is still handing out blocked work after --fix"
    );
    assert!(ws.ready().contains(&blocker));
    assert!(ws.blocked().contains(&deep_child));
    assert!(!ws.blocked().contains(&blocker));
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
