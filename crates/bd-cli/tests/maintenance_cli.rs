//! Maintenance, end to end through the real binary and a real database.
//!
//! Two things are being pinned here, and the second matters as much as the first:
//!
//! * The sweeps do what they say — and, more importantly, *stop* where they say.
//!   A garbage collector is only trustworthy in terms of what it leaves alone.
//! * The exit-code classification. `backup` is exit 2 forever (sqlite has no
//!   commit graph to back up); `compact` and `migrate` are exit 64 until somebody
//!   builds them. If those two ever swap, the port's status becomes unreadable —
//!   so they are asserted, not documented.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

struct Ws(PathBuf);

impl Ws {
    fn new(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-maint-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        let p = std::fs::canonicalize(&p).unwrap();
        let ws = Ws(p);
        assert_eq!(ws.run(&["init", "--prefix", "t"]).1, 0, "init");
        ws
    }

    /// (stdout, exit code)
    fn run(&self, args: &[&str]) -> (String, i32) {
        let out = bd()
            .args(["-C", self.0.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn json(&self, args: &[&str]) -> (Value, i32) {
        let (out, code) = self.run(args);
        let v = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("bd {args:?} did not emit JSON ({e}): {out}"));
        (v, code)
    }

    /// Nothing in the CLI creates an ephemeral bead yet (`bd mol wisp` is a
    /// stub), so wisps arrive the only way they can: through `bd import`, which
    /// preserves `ephemeral`, `wisp_type` and `created_at` verbatim.
    fn import(&self, records: &[Value]) {
        let path = self.0.join("in.jsonl");
        let body: String = records
            .iter()
            .map(|r| format!("{r}\n"))
            .collect::<Vec<_>>()
            .concat();
        std::fs::write(&path, body).unwrap();
        let (out, code) = self.run(&["import", path.to_str().unwrap()]);
        assert_eq!(code, 0, "import failed: {out}");
    }

    fn exists(&self, id: &str) -> bool {
        self.run(&["show", id]).1 == 0
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn wisp(id: &str, kind: &str, age_hours: i64) -> Value {
    let born = chrono::Utc::now() - chrono::Duration::hours(age_hours);
    serde_json::json!({
        "_type": "issue",
        "id": id,
        "title": format!("{kind} wisp"),
        "status": "open",
        "priority": 2,
        "issue_type": "task",
        "ephemeral": true,
        "wisp_type": kind,
        "created_at": born.to_rfc3339(),
        "updated_at": born.to_rfc3339(),
    })
}

// ---------------------------------------------------------------------------
// gc / prune
// ---------------------------------------------------------------------------

#[test]
fn gc_reaps_expired_wisps_and_touches_nothing_else() {
    let ws = Ws::new("gc");

    // A ping keeps 6h, an error keeps 7d. One of each, aged past the ping's TTL
    // and well inside the error's: a collector that reaps on a single global
    // clock would take both, and the error wisp is the one you go looking for
    // after something has gone wrong.
    ws.import(&[wisp("t-ping", "ping", 12), wisp("t-err", "error", 12)]);
    let (real, code) = ws.run(&["q", "Actual work"]);
    assert_eq!(code, 0);

    let (doc, code) = ws.json(&["--json", "gc"]);
    assert_eq!(code, 0);
    assert_eq!(doc["reaped"], serde_json::json!(["t-ping"]));
    assert_eq!(doc["reaped_count"], 1);
    assert_eq!(doc["dry_run"], false);

    assert!(!ws.exists("t-ping"), "an expired ping must be reaped");
    assert!(ws.exists("t-err"), "an error wisp keeps a week: it is forensics");
    assert!(ws.exists(&real), "gc must never touch real work");
}

/// Two spellings of the same request, and both must exit 0: the caller asked for
/// no writes and got none, which is a success. A preview that exits 1 is not a
/// preview.
#[test]
fn dry_run_and_readonly_both_preview_the_sweeps_without_performing_them() {
    let ws = Ws::new("gcdry");
    ws.import(&[wisp("t-ping", "ping", 12)]);

    for args in [
        vec!["--json", "--readonly", "gc"],
        vec!["--json", "gc", "--dry-run"],
        vec!["--json", "prune", "--dry-run"],
    ] {
        let (doc, code) = ws.json(&args);
        assert_eq!(code, 0, "a preview is a success, not a refusal: {args:?}");
        assert_eq!(doc["dry_run"], true, "{args:?}");
        assert_eq!(doc["reaped"], serde_json::json!(["t-ping"]), "{args:?}");
        assert!(
            ws.exists("t-ping"),
            "{args:?} reported the sweep and then performed it anyway"
        );
    }

    // And without the flag it really does reap — the previews above are not
    // passing because the sweep was broken.
    let (doc, code) = ws.json(&["--json", "gc"]);
    assert_eq!(code, 0);
    assert_eq!(doc["dry_run"], false);
    assert!(!ws.exists("t-ping"));
}

#[test]
fn prune_sweeps_wisps_but_leaves_leases_alone() {
    let ws = Ws::new("prune");
    ws.import(&[wisp("t-ping", "ping", 12)]);
    let (id, _) = ws.run(&["q", "Claimed and abandoned"]);
    // A zero-length lease is one that has already lapsed by the time the next
    // command reads it — a dead agent, without the wait.
    assert_eq!(ws.run(&["update", &id, "--claim", "--lease", "0s"]).1, 0);

    let (doc, code) = ws.json(&["--json", "prune"]);
    assert_eq!(code, 0);
    assert_eq!(doc["reaped_count"], 1);
    // `bd prune` is the wisp half of gc and nothing else.
    assert_eq!(doc["leases_freed_count"], 0);

    let (doc, _) = ws.json(&["--json", "reclaim"]);
    assert_eq!(doc["count"], 1, "prune should have left the lapsed lease for reclaim");
}

// ---------------------------------------------------------------------------
// reclaim
// ---------------------------------------------------------------------------

/// Find an issue in a `bd list --json` array.
fn find<'a>(doc: &'a Value, id: &str) -> Option<&'a Value> {
    doc.as_array()?.iter().find(|i| i["id"] == id)
}

#[test]
fn reclaim_frees_work_a_dead_agent_was_holding_hostage() {
    let ws = Ws::new("reclaim");
    let (id, _) = ws.run(&["q", "Held by a corpse"]);
    assert_eq!(ws.run(&["update", &id, "--claim", "--lease", "0s"]).1, 0);

    // The agent took it, stamped its name on it, and died.
    let (wip, _) = ws.json(&["--json", "list", "--status", "in_progress"]);
    assert_eq!(find(&wip, &id).expect("claimed")["assignee"], "agent-7");

    let (doc, code) = ws.json(&["--json", "reclaim"]);
    assert_eq!(code, 0);
    assert_eq!(doc["count"], 1);
    assert_eq!(doc["reclaimed"][0], id.as_str());

    // The lease lapsed, so the work comes back: open again, and held by nobody.
    // Without this it stays in_progress forever, with a dead agent's name on it,
    // and no second agent will ever pick it up.
    // (An empty assignee is *absent* from the JSON — `Issue.assignee` is skipped
    // when empty — so "held by nobody" is a missing key, not an empty string.)
    let (open, _) = ws.json(&["--json", "list", "--status", "open"]);
    let back = find(&open, &id).expect("back to open");
    assert!(
        back.get("assignee").is_none(),
        "reclaim left a dead agent's name on the work: {back}"
    );
    let (wip, _) = ws.json(&["--json", "list", "--status", "in_progress"]);
    assert!(find(&wip, &id).is_none(), "still held after reclaim");

    // Idempotent: there is nothing left to reclaim.
    let (doc, _) = ws.json(&["--json", "reclaim"]);
    assert_eq!(doc["count"], 0);
}

// ---------------------------------------------------------------------------
// purge
// ---------------------------------------------------------------------------

#[test]
fn purge_will_not_delete_anything_it_cannot_get_consent_for() {
    let ws = Ws::new("purge");
    let (id, _) = ws.run(&["q", "Long since done"]);
    assert_eq!(ws.run(&["close", &id]).1, 0);

    // stdout is a pipe here, so there is nobody to answer the prompt — exactly
    // the situation an agent or a CI job is in. It must refuse, loudly, and
    // delete nothing.
    let (doc, code) = ws.json(&["--json", "purge", "--older-than", "0s"]);
    assert_eq!(code, 1, "an unconfirmed purge must fail, not silently no-op");
    assert_eq!(doc["error"], "needs_confirmation");
    assert_eq!(doc["would_delete"][0], id.as_str());
    assert!(ws.exists(&id), "purge deleted an issue nobody confirmed");

    // --readonly asks for a preview and gets one: exit 0, still nothing deleted.
    let (doc, code) = ws.json(&["--json", "--readonly", "purge", "--older-than", "0s"]);
    assert_eq!(code, 0);
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["count"], 1);
    assert_eq!(doc["deleted"], serde_json::json!([]));
    assert!(ws.exists(&id));

    // …and so does --dry-run, which is the flag a script reaches for.
    let (doc, code) = ws.json(&["--json", "purge", "--older-than", "0s", "--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["would_delete"][0], id.as_str());
    assert!(ws.exists(&id));
}

/// **A scripted purge has to be able to succeed.**
///
/// `bd purge` refuses to read silence on a pipe as consent, and it is right to —
/// but with no way to *give* consent in writing there was no path on which a
/// scripted purge succeeded at all. That does not make the command safe, it makes
/// it useless: a destructive command nobody can run is one people work around,
/// usually by deleting rows by hand.
#[test]
fn yes_is_consent_a_script_can_actually_give() {
    let ws = Ws::new("purgeyes");
    let (doomed, _) = ws.run(&["q", "Long since done"]);
    let (kept, _) = ws.run(&["q", "Still going"]);
    assert_eq!(ws.run(&["close", &doomed]).1, 0);

    // stdout is a pipe: nobody can answer the prompt. `--yes` is the answer.
    let (doc, code) = ws.json(&["--json", "purge", "--older-than", "0s", "--yes"]);
    assert_eq!(code, 0, "a purge with written consent must succeed: {doc}");
    assert_eq!(doc["dry_run"], false);
    assert_eq!(doc["deleted"][0], doomed.as_str());
    assert_eq!(doc["count"], 1);

    assert!(!ws.exists(&doomed), "purge did not delete what it said it did");
    assert!(kept.is_empty() || ws.exists(&kept), "purge ate open work");

    // --dry-run wins over --yes. Asking to preview and to consent at once is a
    // contradiction, and the safe reading of a contradiction is the one that
    // writes nothing.
    let (other, _) = ws.run(&["q", "Also done"]);
    assert_eq!(ws.run(&["close", &other]).1, 0);
    let (doc, code) = ws.json(&[
        "--json", "purge", "--older-than", "0s", "--yes", "--dry-run",
    ]);
    assert_eq!(code, 0);
    assert_eq!(doc["dry_run"], true);
    assert!(ws.exists(&other), "--dry-run --yes deleted an issue");
}

#[test]
fn purge_leaves_open_work_and_young_closed_work_alone() {
    let ws = Ws::new("purgeage");
    let (open, _) = ws.run(&["q", "Still going"]);
    let (closed, _) = ws.run(&["q", "Just finished"]);
    assert_eq!(ws.run(&["close", &closed]).1, 0);

    // Nothing has been closed for 90 days; the default threshold must find
    // nothing, and an empty purge is a success.
    let (doc, code) = ws.json(&["--json", "purge"]);
    assert_eq!(code, 0, "an empty purge is not a failure");
    assert_eq!(doc["count"], 0);
    assert!(ws.exists(&open));
    assert!(ws.exists(&closed));

    // Even at zero threshold, open work is not a candidate.
    let (doc, _) = ws.json(&["--json", "--readonly", "purge", "--older-than", "0s"]);
    let would: Vec<&str> = doc["would_delete"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(would, vec![closed.as_str()]);
}

// ---------------------------------------------------------------------------
// preflight
// ---------------------------------------------------------------------------

#[test]
fn preflight_without_a_workspace_reports_instead_of_crashing() {
    let tmp = std::env::temp_dir().join(format!("bd-maint-pf-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let tmp = std::fs::canonicalize(&tmp).unwrap();

    let out = bd()
        .args(["-C", tmp.to_str().unwrap(), "--json", "preflight"])
        .output()
        .expect("run bd");
    // `bd preflight` is in the Need::Nothing list precisely so it can answer this
    // question. "There is no workspace" is the *finding*, not a crash on the way
    // in — but it is still a failing preflight, so `bd preflight && bd ready`
    // stops here.
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).expect("preflight --json must emit JSON");
    assert_eq!(doc["ok"], false);
    let ws = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "workspace")
        .expect("a workspace check");
    assert_eq!(ws["status"], "fail");

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn preflight_passes_in_a_healthy_workspace_and_notices_a_lapsed_lease() {
    let ws = Ws::new("pf");
    let (id, _) = ws.run(&["q", "Something to do"]);

    let (doc, code) = ws.json(&["--json", "preflight"]);
    assert_eq!(code, 0, "a healthy workspace must pass: {doc}");
    assert_eq!(doc["ok"], true);

    assert_eq!(ws.run(&["update", &id, "--claim", "--lease", "0s"]).1, 0);
    let (doc, code) = ws.json(&["--json", "preflight"]);
    // A lapsed lease is a warning, not a failure: the work is recoverable and
    // `bd reclaim` recovers it. Failing here would block an agent that could
    // simply have run the fix.
    assert_eq!(code, 0);
    let claims = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "claims")
        .expect("a claims check");
    assert_eq!(claims["status"], "warn");
}

// ---------------------------------------------------------------------------
// The classification itself
// ---------------------------------------------------------------------------

/// Exit 2 means "this backend will never do this". Exit 64 means "we have not
/// built it". Conflating them lies to whoever reads the exit code, so the
/// difference is a test.
#[test]
fn backup_is_a_real_no_and_compact_is_an_unfinished_yes() {
    let ws = Ws::new("codes");

    // Upstream's backup is a Dolt backup remote: it preserves branches and commit
    // history, and `backup sync` is a push. SQLite has no commit graph, so this
    // is a contract, not a gap — and it will still be exit 2 when the port is
    // finished.
    let (doc, code) = ws.json(&["--json", "backup", "status"]);
    assert_eq!(code, 2, "backup on sqlite is a capability answer, not a stub");
    assert_eq!(doc["error"], "unsupported_backend");
    assert_eq!(doc["requires"], "dolt");

    // Compaction and schema migration are things SQLite can absolutely do
    // (`VACUUM`; a schema version). The seam just does not expose them yet. Exit
    // 2 here would tell a user to stop waiting for something that is coming.
    for cmd in ["compact", "migrate"] {
        let (doc, code) = ws.json(&["--json", cmd]);
        assert_eq!(code, 64, "`bd {cmd}` is unbuilt, not impossible");
        assert_eq!(doc["error"], "not_implemented");
    }
}
