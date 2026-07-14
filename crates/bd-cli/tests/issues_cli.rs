//! The issue-mutation commands, through the real binary and a real database.
//!
//! These commands are small, and that is exactly why they are worth driving end
//! to end: each one is a couple of seam calls, so the only place they can go
//! wrong is in the wiring — a note that clobbers instead of appending, a tag
//! that clap swallowed, an association edge that quietly gates work.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

/// A workspace with one issue in it, and a closure to run `bd` against it.
struct Ws {
    dir: PathBuf,
}

impl Ws {
    fn new(tag: &str) -> Ws {
        let dir = tempdir(tag);
        let ws = Ws { dir };
        assert_eq!(ws.run(&["init", "--prefix", "t"]).1, 0, "init");
        ws
    }

    fn run(&self, args: &[&str]) -> (String, i32) {
        let out = bd()
            .args(["-C", self.dir.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    /// The same, but handing back stderr — where a failure says *why*.
    fn run_err(&self, args: &[&str]) -> (String, i32) {
        let out = bd()
            .args(["-C", self.dir.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let (out, code) = self.run(&a);
        assert_eq!(code, 0, "bd {args:?} failed: {out}");
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("bd {args:?}: not JSON ({e}): {out}"))
    }

    /// A fresh issue, by id.
    fn issue(&self, title: &str) -> String {
        let (id, code) = self.run(&["q", title]);
        assert_eq!(code, 0, "q: {id}");
        id
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

#[test]
fn rename_retitles_and_refuses_an_empty_title() {
    let ws = Ws::new("rename");
    let id = ws.issue("Before");

    let doc = ws.json(&["rename", &id, "After"]);
    assert_eq!(doc["title"], "After");

    // An empty title is a real failure, not a no-op success.
    assert_eq!(ws.run(&["rename", &id, "   "]).1, 1);
    assert_eq!(ws.json(&["show", &id])["title"], "After");
}

#[test]
fn tag_adds_and_a_leading_hyphen_removes() {
    let ws = Ws::new("tag");
    let id = ws.issue("Tagged");

    // The `-stale` argument is the whole reason `tags` allows hyphen values: if
    // clap ever takes it for a flag again, this stops parsing entirely.
    assert_eq!(ws.run(&["tag", &id, "urgent", "stale"]).1, 0);
    let doc = ws.json(&["tag", &id, "shipped", "-stale"]);

    let labels: Vec<&str> = doc["labels"]
        .as_array()
        .expect("labels")
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(labels.contains(&"urgent"), "{labels:?}");
    assert!(labels.contains(&"shipped"), "{labels:?}");
    assert!(!labels.contains(&"stale"), "`-stale` did not remove: {labels:?}");

    // A typo'd id must not report success.
    assert_eq!(ws.run(&["tag", "t-nope", "x"]).1, 1);
}

#[test]
fn note_appends_rather_than_clobbering() {
    let ws = Ws::new("note");
    let id = ws.issue("Noted");

    assert_eq!(ws.run(&["note", &id, "first", "thought"]).1, 0);
    let doc = ws.json(&["note", &id, "second thought"]);

    let notes = doc["notes"].as_str().expect("notes");
    assert!(notes.contains("first thought"), "the first note was lost: {notes:?}");
    assert!(notes.contains("second thought"), "{notes:?}");
    assert_eq!(notes.lines().count(), 2, "notes should stack: {notes:?}");
}

#[test]
fn association_edges_do_not_gate_ready_work() {
    let ws = Ws::new("assoc");
    let original = ws.issue("The original");
    let dupe = ws.issue("The duplicate");
    let successor = ws.issue("The successor");

    assert_eq!(ws.run(&["duplicate", &dupe, "--of", &original]).1, 0);
    assert_eq!(ws.run(&["supersede", &original, "--with", &successor]).1, 0);
    assert_eq!(ws.run(&["link", &dupe, &successor]).1, 0);

    // The point of the whole exercise: none of those three edges may take work
    // out of `bd ready`. If one does, marking a duplicate silently blocks the
    // issue it marks and nothing tells you.
    let ready = ws.json(&["ready"]);
    let ids: Vec<&str> = ready
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    for id in [&original, &dupe, &successor] {
        assert!(ids.contains(&id.as_str()), "{id} vanished from ready: {ids:?}");
    }

    // And a blocking edge is not `bd link`'s business: it has a command. This is
    // now refused by clap, before the store is opened — a `--type` that parses,
    // tab-completes, appears in `--help`, and is then rejected by the handler
    // reads as a bug in beads rather than as a mistake by the caller.
    let (err, code) = ws.run_err(&["link", &dupe, &original, "--type", "blocks"]);
    assert_eq!(code, 1, "`bd link --type blocks` must refuse, not create a blocker");
    assert!(
        err.contains("bd dep add"),
        "refusing is half the job — say where to go instead: {err}"
    );
    // The edge really was not written.
    let doc = ws.json(&["show", &dupe]);
    assert!(
        !doc["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["type"] == "blocks"),
        "a refused link left a blocking edge behind: {doc}"
    );

    // The edge is real, though, and `show` can see it.
    let doc = ws.json(&["show", &dupe]);
    let deps = doc["dependencies"].as_array().expect("dependencies");
    assert!(deps.iter().any(|d| d["type"] == "duplicates" && d["depends_on_id"] == original.as_str()));
}

#[test]
fn heartbeat_renews_a_claim_it_holds() {
    let ws = Ws::new("hb");
    let id = ws.issue("Claimed work");

    // Nobody holds it yet, so there is nothing to renew. That is a failure, not
    // a silent claim.
    let (out, code) = ws.run_err(&["heartbeat", &id]);
    assert_eq!(code, 1);
    // And it has to say *that*. It used to report "already claimed by ''", which
    // describes a race with a nameless agent — so the reader goes looking for an
    // agent that does not exist, instead of simply claiming the issue.
    assert!(
        out.contains("not claimed"),
        "an unclaimed bead must not be reported as claimed by nobody: {out}"
    );
    assert!(
        !out.contains("already claimed"),
        "still reporting a phantom holder: {out}"
    );

    assert_eq!(ws.run(&["update", &id, "--claim"]).1, 0);
    let doc = ws.json(&["hb", &id]);
    assert_eq!(doc["issue_id"], id.as_str());
    assert_eq!(doc["holder"], "agent-7");
    assert!(doc["expires_at"].is_string(), "a renewal has an expiry: {doc}");
}

/// `bd promote`: a wisp becomes a real bead.
///
/// Both halves have to move. An ephemeral bead never appears in `bd ready`, and a
/// bead that still declares a `wisp_type` still carries that type's TTL — so a
/// half-promotion leaves work that is either invisible or that `bd gc` deletes
/// out from under whoever claimed it.
#[test]
fn promote_turns_a_wisp_into_claimable_work_that_gc_will_not_eat() {
    let ws = Ws::new("promote");

    // Nothing in the CLI mints a wisp yet (`bd mol wisp` is a stub), so it
    // arrives the only way it can: through import, which preserves the flags.
    let born = chrono::Utc::now() - chrono::Duration::hours(12);
    let record = serde_json::json!({
        "_type": "issue",
        "id": "t-wisp",
        "title": "A ping that turned out to matter",
        "status": "open",
        "priority": 2,
        "issue_type": "task",
        "ephemeral": true,
        "wisp_type": "ping",          // a 6h TTL, and it is 12h old
        "created_at": born.to_rfc3339(),
        "updated_at": born.to_rfc3339(),
    });
    let path = ws.dir.join("wisp.jsonl");
    std::fs::write(&path, format!("{record}\n")).unwrap();
    assert_eq!(ws.run(&["import", path.to_str().unwrap()]).1, 0);

    // As a wisp: not claimable, and gc is about to reap it.
    let ready = ws.json(&["ready"]);
    assert!(
        !ready.as_array().unwrap().iter().any(|i| i["id"] == "t-wisp"),
        "an ephemeral bead is not claimable work"
    );
    let doc = ws.json(&["gc", "--dry-run"]);
    assert_eq!(doc["reaped"], serde_json::json!(["t-wisp"]));

    let doc = ws.json(&["promote", "t-wisp"]);
    assert!(doc.get("ephemeral").is_none(), "still ephemeral: {doc}");
    assert!(
        doc.get("wisp_type").is_none(),
        "a promoted bead that kept its wisp type still has that type's TTL: {doc}"
    );

    // Now it is real: claimable, and gc will not touch it.
    let ready = ws.json(&["ready"]);
    assert!(
        ready.as_array().unwrap().iter().any(|i| i["id"] == "t-wisp"),
        "a promoted bead must be claimable: {ready}"
    );
    let doc = ws.json(&["gc", "--dry-run"]);
    assert_eq!(
        doc["reaped_count"], 0,
        "gc is still going to reap the bead somebody just promoted: {doc}"
    );

    // Promoting a bead that was never a wisp is a mistake worth hearing about.
    let real = ws.issue("An ordinary bead");
    assert_eq!(ws.run(&["promote", &real]).1, 1);
}

#[test]
fn statuses_and_types_report_the_builtins() {
    let ws = Ws::new("enums");

    let statuses = ws.json(&["statuses"]);
    let open = statuses
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "open")
        .expect("open is a status");
    assert_eq!(open["category"], "active");

    let types = ws.json(&["types"]);
    let types = types.as_array().unwrap();
    assert!(types.iter().any(|t| t["name"] == "bug" && t["excluded_from_ready"] == false));
    // The distinction that matters: infrastructure beads are never claimable.
    assert!(types.iter().any(|t| t["name"] == "gate" && t["excluded_from_ready"] == true));
}

#[test]
fn batch_applies_a_document_and_a_bad_one_applies_nothing() {
    let ws = Ws::new("batch");
    let target = ws.issue("Batch target");

    let doc = format!(
        r#"{{"op":"create","title":"From a batch","type":"bug","priority":1,"labels":["batched"]}}
{{"op":"note","id":"{target}","text":"from the batch"}}
{{"op":"label","id":"{target}","add":["done"]}}
{{"op":"close","id":"{target}","reason":"shipped"}}
"#
    );
    let path = ws.dir.join("ops.jsonl");
    std::fs::write(&path, doc).unwrap();

    let out = ws.json(&["batch", path.to_str().unwrap()]);
    assert_eq!(out["applied"], 4);
    let made = out["created"][0].as_str().expect("a created id");

    let made = ws.json(&["show", made]);
    assert_eq!(made["title"], "From a batch");
    assert_eq!(made["issue_type"], "bug");
    assert_eq!(made["priority"], 1);
    assert_eq!(made["labels"][0], "batched");

    let closed = ws.json(&["show", &target]);
    assert_eq!(closed["status"], "closed");
    assert_eq!(closed["close_reason"], "shipped");
    assert_eq!(closed["notes"], "from the batch");

    // A misspelled field is caught in the parse, before anything is applied --
    // `resaon` would otherwise close the issue with the wrong reason, and a
    // conditional-blocks dependent reads that reason to decide what runs next.
    let reopened = ws.issue("Still open");
    let bad = format!(
        r#"{{"op":"note","id":"{reopened}","text":"applied?"}}
{{"op":"close","id":"{reopened}","resaon":"typo"}}
"#
    );
    std::fs::write(&path, bad).unwrap();
    assert_eq!(ws.run(&["batch", path.to_str().unwrap()]).1, 1);

    let untouched = ws.json(&["show", &reopened]);
    assert_eq!(untouched["status"], "open", "a malformed batch must change nothing");
    assert!(
        untouched["notes"].is_null(),
        "the parse must fail before the first op runs: {untouched}"
    );
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-issues-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    canonical(&p)
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}
