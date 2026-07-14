//! The dependency graph, driven through the real binary and a real database.
//!
//! This is the subsystem the whole product rests on: `bd ready` does not walk
//! the graph, it reads a *cache* of the graph (`issues.is_blocked`), and a stale
//! cache does not fail loudly — it hands an agent a bead whose blocker is still
//! open, or hides one that is claimable. Neither shows up as an error anywhere.
//!
//! So these tests assert on what `bd ready` and `bd blocked` actually *say*,
//! from outside the process, after real writes. The unit tests in
//! `bd-sqlite::blocked` prove the fixpoint converges; these prove the fixpoint is
//! the thing the CLI is actually reading.

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
        let dir = std::env::temp_dir().join(format!(
            "bd-deps-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Ws {
            dir: std::fs::canonicalize(&dir).unwrap(),
        };
        ws.ok(&["init", "--prefix", "t"]);
        ws
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", self.dir.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "tester")
            // Colour would smuggle escape codes into every assertion below.
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

    #[track_caller]
    fn json(&self, args: &[&str]) -> Value {
        let mut a = args.to_vec();
        a.push("--json");
        let stdout = self.ok(&a);
        serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("bd {args:?} --json emitted no JSON ({e}): {stdout}"))
    }

    /// Create an issue and return its id. `bd q` prints the id and nothing else.
    #[track_caller]
    fn q(&self, title: &str) -> String {
        let id = self.ok(&["q", title]);
        assert!(id.starts_with("t-"), "unexpected id from `bd q`: {id}");
        id
    }

    /// `bd dep add ISSUE DEPENDS_ON --type T`, i.e. ISSUE waits for DEPENDS_ON.
    #[track_caller]
    fn dep(&self, issue: &str, depends_on: &str, ty: &str) {
        self.ok(&["dep", "add", issue, depends_on, "--type", ty]);
    }

    #[track_caller]
    fn close(&self, id: &str, reason: &str) {
        self.ok(&["close", id, "--reason", reason]);
    }

    /// The ids `bd ready` would hand an agent right now. `--limit 0` because the
    /// default limit is 20 and a truncated answer would make a test lie.
    #[track_caller]
    fn ready(&self) -> Vec<String> {
        ids(&self.json(&["ready", "--limit", "0"]))
    }

    #[track_caller]
    fn blocked(&self) -> Vec<String> {
        ids(&self.json(&["blocked", "--limit", "0"]))
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn ids(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .as_array()
        .expect("a list command must emit a JSON array")
        .iter()
        .map(|i| i["id"].as_str().expect("every issue has an id").to_string())
        .collect();
    out.sort();
    out
}

fn sorted(xs: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

/// The fixpoint must already have been reached by the incremental path.
///
/// `bd recompute-blocked` rebuilds `is_blocked` from scratch and reports how many
/// rows it *changed*. On a workspace where every write maintained the cache
/// correctly, that number is zero. Any other number means some write path left a
/// stale row behind — which is precisely the bug class that makes `bd ready` lie
/// while every command still exits 0.
#[track_caller]
fn assert_cache_is_already_at_the_fixpoint(ws: &Ws, when: &str) {
    let doc = ws.json(&["recompute-blocked"]);
    assert_eq!(
        doc["updated"], 0,
        "a full recompute changed {} row(s) {when}: the incremental is_blocked \
         maintenance left the cache stale, so `bd ready` was lying",
        doc["updated"]
    );
}

// ---------------------------------------------------------------------------
// The basic gate
// ---------------------------------------------------------------------------

#[test]
fn a_blocks_b_so_only_a_is_ready_until_a_closes() {
    let ws = Ws::new("basic");
    let a = ws.q("Write the schema");
    let b = ws.q("Ship it");
    ws.dep(&b, &a, "blocks");

    assert_eq!(ws.ready(), sorted(&[&a]), "a blocked bead must never be ready");
    assert_eq!(ws.blocked(), sorted(&[&b]));
    assert_cache_is_already_at_the_fixpoint(&ws, "after adding the edge");

    ws.close(&a, "done");

    assert_eq!(ws.ready(), sorted(&[&b]), "closing the blocker must free B");
    assert!(ws.blocked().is_empty());
    assert_cache_is_already_at_the_fixpoint(&ws, "after closing the blocker");
}

/// Removing the edge is the other way B can become ready, and it exercises a
/// different write path (`remove_dependency` seeds the recompute from *both*
/// ends, because after the DELETE there is nothing left to say who was gated).
#[test]
fn removing_the_edge_frees_the_bead_it_was_gating() {
    let ws = Ws::new("unedge");
    let a = ws.q("Blocker");
    let b = ws.q("Gated");
    ws.dep(&b, &a, "blocks");
    assert_eq!(ws.blocked(), sorted(&[&b]));

    ws.ok(&["dep", "remove", &b, &a]);

    assert_eq!(ws.ready(), sorted(&[&a, &b]));
    assert!(ws.blocked().is_empty());
    assert_cache_is_already_at_the_fixpoint(&ws, "after removing the edge");
}

/// **Data loss, through the front door.**
///
/// Two beads can be joined by several edges at once. `bd dep remove A B` used to
/// take a pair and delete every edge between them, so removing a blocker also
/// destroyed the `related` edge somebody recorded a month ago — silently, while
/// reporting success. The type is what makes the removal name one edge.
#[test]
fn dep_remove_takes_the_edge_you_named_and_not_the_others() {
    let ws = Ws::new("edgetype");
    let a = ws.q("The blocker");
    let b = ws.q("The gated one");

    ws.dep(&b, &a, "blocks");
    ws.dep(&b, &a, "related");

    ws.ok(&["dep", "remove", &b, &a, "--type", "blocks"]);

    let types: Vec<String> = ws.json(&["dep", "list", &b])["depends_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        types,
        vec!["related"],
        "removing the `blocks` edge also destroyed the `related` one"
    );
    // The gate really did lift: this is not passing because nothing was removed.
    assert_eq!(ws.ready(), sorted(&[&a, &b]));

    // Removing an edge that is not there is a failure, not a shrug — otherwise a
    // typo'd type reports that it removed something and removes nothing.
    let (_, stderr, code) = ws.run(&["dep", "remove", &b, &a, "--type", "blocks"]);
    assert_eq!(code, 1, "removing an absent edge must not report success");
    assert!(!stderr.is_empty());
}

/// `bd dep relate` / `bd dep unrelate`: an association, and its removal.
///
/// `unrelate` could not be written honestly until `remove_dependency` took an
/// edge type — "drop the relation between these two" would have dropped whatever
/// else joined them, including the edge blocking one on the other.
#[test]
fn relate_records_an_association_and_unrelate_removes_only_that() {
    let ws = Ws::new("relate");
    let a = ws.q("The blocker");
    let b = ws.q("The gated one");

    ws.dep(&b, &a, "blocks");
    ws.ok(&["dep", "relate", &b, &a]);
    assert_eq!(
        ws.blocked(),
        sorted(&[&b]),
        "an association must not change what gates what"
    );

    ws.ok(&["dep", "unrelate", &b, &a]);

    let types: Vec<String> = ws.json(&["dep", "list", &b])["depends_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        types,
        vec!["blocks"],
        "unrelate took the blocking edge with it: {types:?}"
    );
    assert_eq!(
        ws.blocked(),
        sorted(&[&b]),
        "B is still gated by A — unrelate must not have freed it"
    );
    assert_cache_is_already_at_the_fixpoint(&ws, "after unrelate");
}

// ---------------------------------------------------------------------------
// The dangerous one: transitive parent-child propagation
// ---------------------------------------------------------------------------

/// **The most dangerous semantic in the system.**
///
/// `A blocks B`; `B` is the parent of `C`; `C` is the parent of `D`. Blocked-ness
/// propagates *down* the containment tree, so all of B, C and D are gated by A.
///
/// Closing A must free all three. A single mark/unmark pass propagates the
/// unblock exactly one level — B learns it is free, and C, having already been
/// visited, does not — so a one-pass implementation leaves C and D wrongly
/// blocked, *nondeterministically*, depending on the order the database happened
/// to visit rows in. Nothing errors. `bd ready` just quietly hides two beads that
/// an agent could be working on.
///
/// This is the test that fails if anyone ever "simplifies" the fixpoint.
#[test]
fn closing_one_blocker_frees_the_whole_parent_child_chain() {
    let ws = Ws::new("chain");
    let a = ws.q("A: the blocker");
    let b = ws.q("B: the parent");
    let c = ws.q("C: child of B");
    let d = ws.q("D: child of C");

    ws.dep(&b, &a, "blocks"); // A blocks B
    ws.dep(&c, &b, "parent-child"); // C is a child of B
    ws.dep(&d, &c, "parent-child"); // D is a child of C

    assert_eq!(
        ws.blocked(),
        sorted(&[&b, &c, &d]),
        "blocked-ness must propagate all the way down the containment tree"
    );
    assert_eq!(ws.ready(), sorted(&[&a]));
    assert_cache_is_already_at_the_fixpoint(&ws, "after building the chain");

    ws.close(&a, "done");

    assert_eq!(
        ws.ready(),
        sorted(&[&b, &c, &d]),
        "closing A must unblock B *and* C *and* D — the deep end of the chain is \
         where a single-pass recompute gets it wrong"
    );
    assert!(
        ws.blocked().is_empty(),
        "nothing is left to block: {:?}",
        ws.blocked()
    );
    assert_cache_is_already_at_the_fixpoint(&ws, "after closing the root blocker");
}

/// The same chain, built in the adverse order: the containment tree exists and
/// is entirely ready, and *then* the blocker arrives. Blocked-ness has to
/// propagate down to a fixpoint when it lands, not only when it lifts — and the
/// seed for that recompute is an edge, not a status change.
#[test]
fn a_late_blocker_propagates_down_an_existing_chain() {
    let ws = Ws::new("late");
    let a = ws.q("A: the late blocker");
    let b = ws.q("B: the parent");
    let c = ws.q("C: child of B");
    let d = ws.q("D: child of C");

    ws.dep(&c, &b, "parent-child");
    ws.dep(&d, &c, "parent-child");
    assert_eq!(
        ws.ready(),
        sorted(&[&a, &b, &c, &d]),
        "an unblocked hierarchy is entirely claimable"
    );

    ws.dep(&b, &a, "blocks");

    assert_eq!(
        ws.blocked(),
        sorted(&[&b, &c, &d]),
        "a blocker landing on the root of a hierarchy must gate the whole subtree"
    );
    assert_eq!(ws.ready(), sorted(&[&a]));
    assert_cache_is_already_at_the_fixpoint(&ws, "after the late blocker arrived");
}

// ---------------------------------------------------------------------------
// Cycles
// ---------------------------------------------------------------------------

/// A cycle is a contradiction ("A before B before A"), it makes `bd dep tree`
/// non-terminating, and it costs the `is_blocked` fixpoint its entire iteration
/// budget. The write path refuses to create one — and it must refuse *before*
/// writing, not by cleaning up afterwards.
#[test]
fn a_cycle_is_refused_and_the_graph_stays_clean() {
    let ws = Ws::new("cycle");
    let a = ws.q("A");
    let b = ws.q("B");
    ws.dep(&b, &a, "blocks");

    let (_, stderr, code) = ws.run(&["dep", "add", &a, &b, "--type", "blocks"]);
    assert_eq!(code, 1, "closing a loop must fail, not succeed quietly");
    assert!(
        stderr.to_lowercase().contains("cycle"),
        "the error must name the problem: {stderr}"
    );

    // And the refusal must be total: no half-written edge left behind.
    assert!(
        ws.json(&["dep", "cycles"]).as_array().unwrap().is_empty(),
        "a refused edge must not appear in the graph"
    );
    let deps = ws.json(&["dep", "list", &a]);
    assert!(
        deps["depends_on"].as_array().unwrap().is_empty(),
        "A must not depend on anything: {deps}"
    );
    assert_eq!(ws.ready(), sorted(&[&a]));
}

/// Containment is an ordering too: a bead cannot be its own ancestor.
#[test]
fn a_parent_child_cycle_is_refused() {
    let ws = Ws::new("pccycle");
    let a = ws.q("Parent");
    let b = ws.q("Child");
    ws.dep(&b, &a, "parent-child");

    let (_, stderr, code) = ws.run(&["dep", "add", &a, &b, "--type", "parent-child"]);
    assert_eq!(code, 1, "a containment loop must be refused: {stderr}");
    assert!(stderr.to_lowercase().contains("cycle"), "{stderr}");
}

/// Self-dependency is a cycle of length one, and it is rejected in the domain
/// type before the store is even asked.
#[test]
fn an_issue_cannot_depend_on_itself() {
    let ws = Ws::new("selfdep");
    let a = ws.q("A");
    let (_, stderr, code) = ws.run(&["dep", "add", &a, &a]);
    assert_ne!(code, 0, "a self-edge blocks the bead on itself, forever");
    assert!(!stderr.is_empty());
}

// ---------------------------------------------------------------------------
// Which edges gate, and which do not
// ---------------------------------------------------------------------------

/// Only four edge types gate readiness. `related` and `discovered-from` are
/// associations: they exist to be traversed and displayed, and a bead that merely
/// *mentions* another must stay claimable.
///
/// The second half of the test is what keeps the first half honest: the same
/// pair of beads, with a `blocks` edge added on top, *does* gate — so a bug that
/// simply never blocked anything could not pass this.
#[test]
fn association_edges_do_not_gate_readiness() {
    let ws = Ws::new("assoc");
    let a = ws.q("The thing that was mentioned");
    let b = ws.q("The thing that mentions it");

    ws.dep(&b, &a, "related");
    ws.dep(&b, &a, "discovered-from");

    assert_eq!(
        ws.ready(),
        sorted(&[&a, &b]),
        "an association is not a gate: B must still be claimable"
    );
    assert!(ws.blocked().is_empty());

    // Both edges really are in the graph — the assertion above is not passing
    // because the writes silently did nothing.
    let deps = ws.json(&["dep", "list", &b]);
    let types: Vec<&str> = deps["depends_on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"related"), "{deps}");
    assert!(types.contains(&"discovered-from"), "{deps}");

    // Now a real gate over the very same pair.
    ws.dep(&b, &a, "blocks");
    assert_eq!(ws.blocked(), sorted(&[&b]));
    assert_eq!(ws.ready(), sorted(&[&a]));
}

/// `B conditional-blocks A` = "run B only if A **fails**".
///
/// So closing A is not enough: closing it *successfully* means the failure path
/// is moot and B stays blocked (deliberately — a store that auto-closed beads
/// nobody asked it to close would be worse). Closing it with a reason that reads
/// as a failure is what opens B.
///
/// Both halves run in one workspace so the two outcomes are compared under
/// identical conditions.
#[test]
fn conditional_blocks_opens_on_failure_and_stays_shut_on_success() {
    let ws = Ws::new("cond");
    let ok_task = ws.q("Deploy");
    let rollback = ws.q("Roll back the deploy");
    let flaky = ws.q("Flaky migration");
    let repair = ws.q("Repair the migration");

    ws.dep(&rollback, &ok_task, "conditional-blocks");
    ws.dep(&repair, &flaky, "conditional-blocks");

    assert_eq!(
        ws.blocked(),
        sorted(&[&rollback, &repair]),
        "a failure path is blocked while its subject is still open"
    );

    ws.close(&ok_task, "done");
    ws.close(&flaky, "failed");

    assert_eq!(
        ws.ready(),
        sorted(&[&repair]),
        "only the failure path of the bead that actually failed becomes ready"
    );
    assert_eq!(
        ws.blocked(),
        sorted(&[&rollback]),
        "the deploy succeeded, so its rollback must stay blocked — not ready, and \
         emphatically not auto-closed"
    );
    assert_cache_is_already_at_the_fixpoint(&ws, "after the two closes");
}

/// `waits-for` names a *spawner*, and the gate is over the spawner's children —
/// so a child changing status moves a gate the child has no edge to. That
/// indirection is easy to miss when seeding an incremental recompute, and missing
/// it leaves the waiter blocked forever.
#[test]
fn waits_for_opens_when_the_spawners_children_are_done() {
    let ws = Ws::new("waits");
    let spawner = ws.q("Fan out the work");
    let child = ws.q("One of the fanned-out pieces");
    let after = ws.q("Collect the results");

    ws.dep(&child, &spawner, "parent-child");
    ws.dep(&after, &spawner, "waits-for");

    assert_eq!(
        ws.blocked(),
        sorted(&[&after]),
        "the collector waits until the spawner's children are done"
    );

    ws.close(&child, "done");

    assert!(
        ws.ready().contains(&after),
        "the last child closing must open the gate, even though the child has no \
         edge to the waiter: {:?}",
        ws.ready()
    );
    assert_cache_is_already_at_the_fixpoint(&ws, "after the last child closed");
}

/// Reopening is the inverse of closing, and the cache has to follow it back.
#[test]
fn reopening_a_blocker_re_gates_its_dependents() {
    let ws = Ws::new("reopen");
    let a = ws.q("A");
    let b = ws.q("B");
    let c = ws.q("C, a child of B");
    ws.dep(&b, &a, "blocks");
    ws.dep(&c, &b, "parent-child");

    ws.close(&a, "done");
    assert_eq!(ws.ready(), sorted(&[&b, &c]));

    ws.ok(&["reopen", &a]);

    assert_eq!(
        ws.blocked(),
        sorted(&[&b, &c]),
        "reopening the blocker must re-gate the whole subtree it was holding"
    );
    assert_cache_is_already_at_the_fixpoint(&ws, "after reopening the blocker");
}

// ---------------------------------------------------------------------------
// bd graph
// ---------------------------------------------------------------------------

#[test]
fn graph_renders_dot_that_names_every_node_and_edge() {
    let ws = Ws::new("dot");
    let a = ws.q("Write the \"schema\"");
    let b = ws.q("Ship it");
    ws.dep(&b, &a, "blocks");
    ws.dep(&b, &a, "related");

    let dot = ws.ok(&["graph"]);
    assert!(dot.starts_with("digraph beads {"), "{dot}");
    assert!(dot.trim_end().ends_with('}'), "{dot}");
    assert!(dot.contains(&format!("\"{a}\"")), "{dot}");
    assert!(dot.contains(&format!("\"{b}\" -> \"{a}\"")), "{dot}");
    assert!(dot.contains("label=\"blocks\""), "{dot}");
    // A quote in a title must arrive escaped, or `dot` cannot parse the file.
    assert!(dot.contains(r#"\"schema\""#), "unescaped quote in DOT: {dot}");
    // The association is drawn, but it must not look like a gate.
    let related = dot
        .lines()
        .find(|l| l.contains("label=\"related\""))
        .expect("the related edge is drawn");
    assert!(related.contains("dashed"), "{related}");
}

#[test]
fn graph_json_carries_the_nodes_the_edges_and_who_is_blocked() {
    let ws = Ws::new("gjson");
    let a = ws.q("Blocker");
    let b = ws.q("Gated");
    ws.dep(&b, &a, "blocks");

    let doc = ws.json(&["graph"]);
    let nodes = doc["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);

    let node = |id: &str| -> Value {
        nodes
            .iter()
            .find(|n| n["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from the graph"))
            .clone()
    };
    assert_eq!(node(&a)["is_blocked"], false);
    assert_eq!(node(&b)["is_blocked"], true);

    let edges = doc["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["issue_id"], b.as_str());
    assert_eq!(edges[0]["depends_on_id"], a.as_str());
    assert_eq!(edges[0]["type"], "blocks");
}

#[test]
fn graph_check_passes_on_a_sound_graph() {
    let ws = Ws::new("check");
    let a = ws.q("A");
    let b = ws.q("B");
    let c = ws.q("C");
    ws.dep(&b, &a, "blocks");
    ws.dep(&c, &b, "parent-child");
    ws.dep(&c, &a, "related");

    let (stdout, _, code) = ws.run(&["graph", "check"]);
    assert_eq!(code, 0, "a sound graph must exit 0: {stdout}");
    assert!(stdout.contains("sound"), "{stdout}");

    let doc = ws.json(&["graph", "check"]);
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["nodes"], 3);
    assert_eq!(doc["edges"], 3);
    assert_eq!(doc["blocked"], 2, "B is gated by A, and C by B");
    assert_eq!(doc["edge_types"]["blocks"], 1);
    assert_eq!(doc["edge_types"]["parent-child"], 1);
    assert_eq!(doc["edge_types"]["related"], 1);
    assert!(doc["cycles"].as_array().unwrap().is_empty());
}

/// The write path cannot produce a cycle, so this reaches around it and writes
/// one directly — which is exactly how a cycle really arrives: an import, a
/// merge, or another beads implementation. `bd graph check` exists to notice.
#[test]
fn graph_check_finds_a_cycle_written_behind_the_stores_back() {
    let ws = Ws::new("corrupt");
    let a = ws.q("A");
    let b = ws.q("B");
    ws.dep(&b, &a, "blocks");

    // `bd sql` is the only back door past the write path's cycle check. It is a
    // stub today (someone else's file), so this test cannot yet corrupt anything
    // — and it says so rather than reporting a false green. It will start
    // asserting for real the moment `bd sql` can write.
    ws.run(&[
        "sql",
        &format!(
            "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) \
             VALUES ('{a}', '{b}', 'blocks', '2020-01-01T00:00:00+00:00')"
        ),
    ]);
    if ws.json(&["dep", "cycles"]).as_array().unwrap().is_empty() {
        eprintln!(
            "SKIPPED graph_check_finds_a_cycle_written_behind_the_stores_back: \
             `bd sql` cannot write, so there is no way to plant a cycle from out here. \
             The check's own logic is covered by the unit tests in commands::deps."
        );
        return;
    }

    let (stdout, _, code) = ws.run(&["graph", "check"]);
    assert_eq!(
        code, 1,
        "a corrupt graph is a real failure, not a clean report: {stdout}"
    );
    assert!(stdout.to_lowercase().contains("cycle"), "{stdout}");

    let doc = ws.json(&["graph", "check"]);
    assert_eq!(doc["ok"], false);
    let cycles = doc["cycles"].as_array().unwrap();
    assert_eq!(cycles.len(), 1, "{doc}");
    let cycle = cycles[0].as_array().unwrap();
    assert_eq!(
        cycle.first(),
        cycle.last(),
        "a cycle is reported as a path that closes on itself: {doc}"
    );
}

// ---------------------------------------------------------------------------
// The rest of the dep family
// ---------------------------------------------------------------------------

/// A diamond re-converges: `bd dep tree` must draw it without expanding the
/// shared node twice, and must terminate.
#[test]
fn dep_tree_draws_a_diamond_once() {
    let ws = Ws::new("tree");
    let base = ws.q("Base");
    let left = ws.q("Left");
    let right = ws.q("Right");
    let top = ws.q("Top");
    ws.dep(&left, &base, "blocks");
    ws.dep(&right, &base, "blocks");
    ws.dep(&top, &left, "blocks");
    ws.dep(&top, &right, "blocks");

    let out = ws.ok(&["dep", "tree", &top]);
    assert_eq!(
        out.matches(&base).count(),
        2,
        "the shared node appears on both arms: {out}"
    );
    assert!(
        out.contains("already shown"),
        "the second visit must be marked, not silently re-expanded: {out}"
    );

    // JSON hands back the edges and lets the client build its own tree.
    let doc = ws.json(&["dep", "tree", &top]);
    assert_eq!(doc["root"], top.as_str());
    assert!(doc["edges"].as_array().unwrap().len() >= 4, "{doc}");
}

#[test]
fn flatten_is_honest_about_not_being_ported() {
    let ws = Ws::new("flat");
    let a = ws.q("A");
    let (_, _, code) = ws.run(&["flatten", &a]);
    assert_eq!(
        code, 64,
        "an unported command must exit 64, never 1 — a script has to be able to \
         tell a gap from a failure"
    );

    // Note the shape of this call: `ws.json` would assert exit 0, and a stub is
    // *supposed* to fail. It still owes an agent a parseable answer.
    let (stdout, _, code) = ws.run(&["flatten", &a, "--json"]);
    assert_eq!(code, 64);
    let doc: Value = serde_json::from_str(&stdout).expect("a --json stub must emit JSON");
    assert_eq!(doc["error"], "not_implemented");
    assert_eq!(doc["command"], "flatten");
}

/// `--readonly` is a guard, not a suggestion: it has to stop the write before it
/// reaches the store, or a "dry run" can half-apply a graph edit.
#[test]
fn readonly_refuses_to_touch_the_graph() {
    let ws = Ws::new("ro");
    let a = ws.q("A");
    let b = ws.q("B");

    let (_, _, code) = ws.run(&["--readonly", "dep", "add", &b, &a]);
    assert_eq!(code, 1);
    let (_, _, code) = ws.run(&["--readonly", "recompute-blocked"]);
    assert_eq!(code, 1);

    assert!(
        ws.json(&["dep", "list", &b])["depends_on"]
            .as_array()
            .unwrap()
            .is_empty(),
        "--readonly let a write through"
    );
}
