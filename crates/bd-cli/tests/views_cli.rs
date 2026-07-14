//! The view commands, through the real binary and a real database.
//!
//! These are read commands over a graph, and a read command that is subtly wrong
//! does not crash — it just answers a different question than the one asked, and
//! keeps doing it. So each assertion here names the question: children must not
//! return the parent, `epic close-eligible` must not offer an epic whose child is
//! still open, `orphans` must not call a linked issue lonely.

use std::process::Command;

use serde_json::Value;

fn bd(dir: &str, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir])
        .args(args)
        .env("BEADS_ACTOR", "agent-7")
        .output()
        .expect("run bd");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

fn json(dir: &str, args: &[&str]) -> Value {
    let mut argv = args.to_vec();
    argv.push("--json");
    let (stdout, stderr, code) = bd(dir, &argv);
    assert_eq!(code, 0, "bd {args:?} exited {code}: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("bd {args:?} emitted no JSON: {e}\n{stdout}"))
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("expected a JSON array")
        .iter()
        .map(|i| i["id"].as_str().expect("issue id").to_string())
        .collect()
}

/// One workspace, one graph, every view command asked about it. Sharing the
/// fixture is deliberate: these commands only disagree in interesting ways when
/// they are all looking at the *same* graph.
#[test]
fn the_view_commands_agree_about_one_graph() {
    let dir = tempdir("views");
    let d = dir.to_str().unwrap();
    assert_eq!(bd(d, &["init", "--prefix", "v"]).2, 0, "init");

    let q = |args: &[&str]| -> String {
        let (id, err, code) = bd(d, args);
        assert_eq!(code, 0, "bd {args:?}: {err}");
        id
    };

    // An epic with two children, one of them closed; a lone unlinked issue; and
    // a pair of issues with the same title.
    let epic = q(&["q", "Ship v2", "-t", "epic"]);
    let done = q(&["q", "Write the schema", "-p", "1"]);
    let todo = q(&["q", "Cut the release", "-p", "2"]);
    let lonely = q(&["q", "Nobody linked me"]);
    let dup_a = q(&["q", "Fix the parser"]);
    let dup_b = q(&["q", "fix   the parser!"]);

    for child in [&done, &todo] {
        assert_eq!(
            bd(d, &["dep", "add", child, &epic, "--type", "parent-child"]).2,
            0
        );
    }
    assert_eq!(bd(d, &["close", &done, "--reason", "done"]).2, 0);

    // --- children: the parent's children, and not the parent -----------------
    let kids = ids(&json(d, &["children", &epic]));
    assert_eq!(kids.len(), 2, "both children, closed or not: {kids:?}");
    assert!(kids.contains(&done) && kids.contains(&todo));
    assert!(!kids.contains(&epic), "an epic is not its own child");

    let (_, err, code) = bd(d, &["children", "v-nope"]);
    assert_eq!(code, 1, "a missing issue is a real failure, not a stub: {err}");

    // --- epic status: progress, counted from the children --------------------
    let epics = json(d, &["epic", "status"]);
    let e = &epics.as_array().expect("array")[0];
    assert_eq!(e["id"].as_str(), Some(epic.as_str()));
    assert_eq!(e["children_total"], 2);
    assert_eq!(e["children_closed"], 1);
    assert_eq!(e["percent_complete"], 50);
    // The issue's own serde field names survive the extra fields beside them.
    assert_eq!(e["issue_type"], "epic");
    assert_eq!(e["priority"], 2);

    // --- epic close-eligible: not while a child is open ----------------------
    let eligible = ids(&json(d, &["epic", "close-eligible"]));
    assert!(
        eligible.is_empty(),
        "an epic with an open child must never be offered for closing: {eligible:?}"
    );
    assert_eq!(bd(d, &["close", &todo, "--reason", "done"]).2, 0);
    let eligible = ids(&json(d, &["epic", "close-eligible"]));
    assert_eq!(eligible, vec![epic.clone()], "now every child is closed");

    // --- orphans: linked issues are not orphans ------------------------------
    let orphans = ids(&json(d, &["orphans"]));
    assert!(orphans.contains(&lonely), "an unlinked issue is an orphan");
    assert!(!orphans.contains(&epic), "the epic has edges");
    assert!(
        !orphans.contains(&done),
        "closed work is finished, not orphaned"
    );

    // --- duplicates: the same title, however it is spelled -------------------
    let groups = json(d, &["duplicates"]);
    let groups = groups["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1, "exactly one duplicate group: {groups:?}");
    let g = &groups[0];
    assert_eq!(g["title"], "fix the parser");
    assert_eq!(
        g["identical_content"], false,
        "same title, different text: a candidate, not a certainty"
    );
    let dup_ids = ids(&g["issues"]);
    assert!(dup_ids.contains(&dup_a) && dup_ids.contains(&dup_b));

    let found = json(d, &["find-duplicates", &dup_a]);
    let candidates = ids(&found["candidates"]);
    assert_eq!(candidates, vec![dup_b.clone()]);
    assert_eq!(found["candidates"][0]["match"], "title");
    assert!(
        found["linked"].as_array().expect("linked").is_empty(),
        "nobody has declared these duplicates yet"
    );

    // --- lint: a childless epic is a finding, and nothing else is ------------
    let bare = q(&["q", "An epic with no plan", "-t", "epic"]);
    let report = json(d, &["lint"]);
    assert_eq!(report["ok"], false);
    let kinds: Vec<&str> = report["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .map(|p| p["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(kinds, vec!["childless_epic"], "one finding, and the right one");
    assert_eq!(report["problems"][0]["id"].as_str(), Some(bare.as_str()));
    // Findings are not failures: a script must be able to tell "lint ran and
    // found things" from "lint broke".
    assert_eq!(bd(d, &["lint"]).2, 0, "lint exits 0 with findings");

    // --- info / ping / context: the workspace answers for itself -------------
    let info = json(d, &["info"]);
    assert_eq!(info["backend"], "sqlite");
    assert_eq!(info["prefix"], "v");
    assert_eq!(info["capabilities"]["commit_graph"], false);
    assert_eq!(info["stats"]["total"], 7);

    let ping = json(d, &["ping"]);
    assert_eq!(ping["ok"], true);
    assert_eq!(ping["issues"], 7);
    assert!(ping["open_ms"].as_f64().is_some());

    let cx = json(d, &["context"]);
    let ready = ids(&cx["ready"]);
    assert!(ready.contains(&lonely), "unblocked work is claimable");
    assert!(
        !ready.contains(&done),
        "closed work is not on anyone's plate"
    );
    let recent = ids(&cx["recently_closed"]);
    assert!(recent.contains(&done) && recent.contains(&todo));

    // --- stale: nothing is stale in a workspace created a second ago ---------
    let fresh = json(d, &["stale", "--older-than", "14d"]);
    assert!(
        fresh.as_array().expect("array").is_empty(),
        "brand-new issues cannot be stale: {fresh}"
    );
    // Everything is stale relative to a zero-length window, and the sort is by
    // last touch, not by creation.
    let all_stale = ids(&json(d, &["stale", "--older-than", "0s"]));
    assert!(all_stale.contains(&lonely) && !all_stale.contains(&done));

    std::fs::remove_dir_all(&dir).ok();
}

/// **`bd ready` means claimable.**
///
/// beads exists to coordinate several agents over one board. An issue another
/// agent claimed five minutes ago is not claimable — `bd update --claim` will
/// fence a second agent out of it — so listing it in `bd ready` hands two agents
/// the same bead and lets one of them discover the collision after it has read
/// the issue and started thinking.
///
/// The other half is what makes a lease a lease and not a lock: once it lapses,
/// the work comes back on its own, without anyone having to notice that the agent
/// holding it died.
#[test]
fn ready_does_not_offer_work_another_agent_is_already_holding() {
    let dir = tempdir("held");
    let d = dir.to_str().unwrap();
    assert_eq!(bd(d, &["init", "--prefix", "h"]).2, 0, "init");

    let as_agent = |actor: &str, args: &[&str]| -> (String, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_bd"))
            .args(["-C", d])
            .args(args)
            .env("BEADS_ACTOR", actor)
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    };

    let free = as_agent("alice", &["q", "Nobody has this"]).0;
    let held = as_agent("alice", &["q", "Alice is on this"]).0;
    let lapsed = as_agent("alice", &["q", "Alice died holding this"]).0;

    assert_eq!(as_agent("alice", &["update", &held, "--claim"]).1, 0);
    // A zero-length lease is one that has already lapsed by the time anyone reads
    // it: a dead agent, without the wait.
    assert_eq!(
        as_agent("alice", &["update", &lapsed, "--claim", "--lease", "0s"]).1,
        0
    );

    // Bob asks what he can work on.
    let ready = as_agent("bob", &["--json", "ready", "--limit", "0"]).0;
    let ready: Vec<String> = serde_json::from_str::<Value>(&ready)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect();

    assert!(
        !ready.contains(&held),
        "`bd ready` offered bob a bead alice is holding — the two of them are now \
         about to do the same work: {ready:?}"
    );
    assert!(
        ready.contains(&lapsed),
        "a lapsed lease is not a claim: the work must come back on its own, or a \
         dead agent holds it forever: {ready:?}"
    );
    assert!(ready.contains(&free));

    // Alice does not get a private view of it either. Finding your own in-flight
    // work is `bd prime`'s job — it has a `yours` section for exactly this.
    let hers = as_agent("alice", &["--json", "ready", "--limit", "0"]).0;
    assert!(
        !hers.contains(&held),
        "`bd ready` is not where an agent finds what it already claimed"
    );
    let prime = as_agent("alice", &["--json", "prime"]).0;
    let prime: Value = serde_json::from_str(&prime).unwrap();
    assert_eq!(
        prime["yours"][0]["id"], held,
        "…and `bd prime` is: {prime}"
    );

    // The count has to agree with the list, or `bd status` says 3 over a board of 2.
    let status = as_agent("bob", &["--json", "status"]).0;
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["ready"], 2, "{status}");

    // And releasing it puts it straight back on offer.
    assert_eq!(as_agent("alice", &["unclaim", &held]).1, 0);
    let ready = as_agent("bob", &["--json", "ready", "--limit", "0"]).0;
    assert!(ready.contains(&held), "unclaiming must return the work: {ready}");

    std::fs::remove_dir_all(&dir).ok();
}

/// The blocked-cache staleness check has to survive the one thing that makes it
/// interesting: a `conditional-blocks` edge whose target closed *successfully*,
/// which the store leaves blocked forever on purpose.
#[test]
fn lint_names_the_bead_that_can_never_become_ready() {
    let dir = tempdir("lint");
    let d = dir.to_str().unwrap();
    assert_eq!(bd(d, &["init", "--prefix", "l"]).2, 0);

    let (attempt, _, _) = bd(d, &["q", "Try the thing"]);
    let (fallback, _, _) = bd(d, &["q", "Clean up after the thing failed"]);
    assert_eq!(
        bd(
            d,
            &["dep", "add", &fallback, &attempt, "--type", "conditional-blocks"]
        )
        .2,
        0
    );

    // Closed with a success reason: the failure path is now moot, and the store
    // deliberately leaves the fallback blocked rather than closing it for you.
    assert_eq!(bd(d, &["close", &attempt, "--reason", "done"]).2, 0);

    let report = json(d, &["lint"]);
    let stuck: Vec<&Value> = report["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .filter(|p| p["kind"] == "stuck_conditional")
        .collect();
    assert_eq!(stuck.len(), 1, "the stuck bead must be named: {report}");
    assert_eq!(stuck[0]["id"].as_str(), Some(fallback.as_str()));

    // And it must not *also* be reported as a stale cache entry — it is blocked
    // for a reason, and a lint that cries wolf about it is a lint people mute.
    let stale: Vec<&Value> = report["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .filter(|p| p["kind"] == "stale_blocked")
        .collect();
    assert!(stale.is_empty(), "blocked on purpose is not blocked in error");

    std::fs::remove_dir_all(&dir).ok();
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-views-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}
