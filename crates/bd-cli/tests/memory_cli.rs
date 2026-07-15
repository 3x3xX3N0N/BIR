//! `bd remember/recall/memories/forget`, `bd todo …`, and `bd human …` through
//! the real binary and a real sqlite database.
//!
//! The load-bearing assertion is negative and lives in every one of these:
//! **an agent's memory (and its todo) is not work.** A note remembered across
//! sessions, or a personal checklist item, must never surface in `bd ready` or
//! the default `bd list` — doing so would hand it to another agent as claimable
//! work. So each family is created, then the work views are checked to be sure it
//! stayed out of them.

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

/// Run with `--json` appended and require success.
fn json(dir: &str, args: &[&str]) -> Value {
    let mut argv = args.to_vec();
    argv.push("--json");
    let (stdout, stderr, code) = bd(dir, &argv);
    assert_eq!(code, 0, "bd {args:?} exited {code}: {stderr}");
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("bd {args:?} emitted no JSON: {e}\n{stdout}"))
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("expected a JSON array")
        .iter()
        .map(|i| i["id"].as_str().expect("issue id").to_string())
        .collect()
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-memory-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// A memory round-trips, is searchable, is deletable — and above all is invisible
/// to the work views.
#[test]
fn a_memory_is_recalled_but_never_offered_as_work() {
    let dir = tempdir("mem");
    let d = dir.to_str().unwrap();
    assert_eq!(bd(d, &["init", "--prefix", "t"]).2, 0, "init");

    // A real piece of work, so the work views are non-empty and the negative
    // assertions below are meaningful rather than vacuous.
    let work = json(d, &["q", "Ship the release"]);
    let work = work["id"].as_str().unwrap().to_string();

    // Two memories.
    let m1 = json(d, &["remember", "always", "run", "tests", "with", "the", "race", "flag"]);
    let m1 = m1["id"].as_str().expect("remember returns an id").to_string();
    let m2 = json(d, &["remember", "dolt", "phantom", "DBs", "hide", "in", "three", "places"]);
    let m2 = m2["id"].as_str().unwrap().to_string();

    // A memory is a stored bead the type marks as such.
    let one = json(d, &["show", &m1]);
    assert_eq!(one["issue_type"], "memory", "a memory is typed `memory`");
    assert_eq!(one["title"], "always run tests with the race flag");

    // memories lists both; recall narrows by substring over the content.
    let all = ids(&json(d, &["memories"]));
    assert!(all.contains(&m1) && all.contains(&m2), "both memories listed: {all:?}");

    let hits = ids(&json(d, &["recall", "dolt"]));
    assert_eq!(hits, vec![m2.clone()], "recall matches only the dolt memory");
    let miss = json(d, &["recall", "kubernetes"]);
    assert!(
        miss.as_array().unwrap().is_empty(),
        "recall of an unknown term matches nothing: {miss}"
    );

    // THE PROPERTY: neither memory is claimable work, and neither shows in the
    // default board — but the real issue does both.
    let ready = ids(&json(d, &["ready", "--limit", "0"]));
    assert!(ready.contains(&work), "real work is ready");
    assert!(
        !ready.contains(&m1) && !ready.contains(&m2),
        "a memory must never be offered as ready work: {ready:?}"
    );
    let listed = ids(&json(d, &["list"]));
    assert!(listed.contains(&work), "real work is on the default board");
    assert!(
        !listed.contains(&m1) && !listed.contains(&m2),
        "a memory must never appear in the default `bd list`: {listed:?}"
    );
    // And it is not counted as open work.
    let status = json(d, &["status"]);
    assert_eq!(status["ready"], 1, "only the one real issue is ready: {status}");

    // forget removes it; the other survives.
    assert_eq!(bd(d, &["forget", &m1]).2, 0, "forget succeeds");
    let after = ids(&json(d, &["memories"]));
    assert_eq!(after, vec![m2.clone()], "only the forgotten memory is gone: {after:?}");

    // forget refuses a real bead (it would be silent data loss) and an unknown id.
    let (_, err, code) = bd(d, &["forget", &work]);
    assert_eq!(code, 1, "forgetting a real issue is refused: {err}");
    assert_eq!(
        json(d, &["show", &work])["id"].as_str(),
        Some(work.as_str()),
        "the work bead survived"
    );
    assert_eq!(bd(d, &["forget", "t-nope"]).2, 1, "forgetting a missing memory fails");

    // Read paths work under --readonly; the write path is refused before it can
    // touch the store.
    assert_eq!(bd(d, &["--readonly", "memories"]).2, 0, "memories is read-only");
    assert_eq!(bd(d, &["--readonly", "recall", "dolt"]).2, 0, "recall is read-only");
    assert_eq!(
        bd(d, &["--readonly", "remember", "nope"]).2,
        1,
        "remember is a write and --readonly refuses it"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A todo is a checklist item: added, listed while open, closed by `done` — and,
/// like a memory, kept out of the work views.
#[test]
fn a_todo_is_a_private_checklist_not_claimable_work() {
    let dir = tempdir("todo");
    let d = dir.to_str().unwrap();
    assert_eq!(bd(d, &["init", "--prefix", "t"]).2, 0, "init");

    let work = json(d, &["q", "Real work here"]);
    let work = work["id"].as_str().unwrap().to_string();

    let t1 = json(d, &["todo", "add", "write", "the", "changelog"]);
    let t1 = t1["id"].as_str().expect("todo add returns an id").to_string();
    let t2 = json(d, &["todo", "add", "tag", "the", "commit"]);
    let t2 = t2["id"].as_str().unwrap().to_string();

    let open = ids(&json(d, &["todo", "list"]));
    assert!(open.contains(&t1) && open.contains(&t2), "both open todos listed: {open:?}");

    // THE PROPERTY again: a todo is not work anyone else can pick up.
    let ready = ids(&json(d, &["ready", "--limit", "0"]));
    assert!(ready.contains(&work));
    assert!(
        !ready.contains(&t1) && !ready.contains(&t2),
        "a todo must never be offered as ready work: {ready:?}"
    );
    let listed = ids(&json(d, &["list"]));
    assert!(
        !listed.contains(&t1) && !listed.contains(&t2),
        "a todo must never appear in the default `bd list`: {listed:?}"
    );

    // done closes it; the open list drops it.
    assert_eq!(bd(d, &["todo", "done", &t1]).2, 0, "todo done succeeds");
    let open = ids(&json(d, &["todo", "list"]));
    assert_eq!(open, vec![t2.clone()], "a completed todo leaves the open list: {open:?}");

    // done refuses a real bead, and a second `done` on the same todo.
    assert_eq!(bd(d, &["todo", "done", &work]).2, 1, "todo done refuses a real bead");
    assert_eq!(bd(d, &["todo", "done", &t1]).2, 1, "a todo cannot be completed twice");

    assert_eq!(bd(d, &["--readonly", "todo", "list"]).2, 0, "todo list is read-only");

    std::fs::remove_dir_all(&dir).ok();
}

/// The human queue: a bead tagged `human` is listed as pending, answered by
/// `respond` (a comment plus a close) or discarded by `dismiss`, and `stats`
/// partitions the queue by outcome.
#[test]
fn the_human_queue_lists_answers_and_dismisses() {
    let dir = tempdir("human");
    let d = dir.to_str().unwrap();
    assert_eq!(bd(d, &["init", "--prefix", "t"]).2, 0, "init");

    // Empty to begin with, and that is a real answer, not a stub.
    assert!(
        json(d, &["human", "list"]).as_array().unwrap().is_empty(),
        "a fresh workspace has an empty human queue"
    );
    let s0 = json(d, &["human", "stats"]);
    assert_eq!(s0["total"], 0, "no human beads yet: {s0}");

    // Two beads escalated to a person.
    let a = json(d, &["q", "Which auth scheme?", "-l", "human"]);
    let a = a["id"].as_str().unwrap().to_string();
    let b = json(d, &["q", "Approve the migration?", "-l", "human"]);
    let b = b["id"].as_str().unwrap().to_string();

    let pending = ids(&json(d, &["human", "list"]));
    assert!(pending.contains(&a) && pending.contains(&b), "both are pending: {pending:?}");

    // respond: records the answer as a comment and closes the bead.
    assert_eq!(bd(d, &["human", "respond", &a, "use", "OAuth2"]).2, 0, "respond succeeds");
    let shown = json(d, &["show", &a]);
    assert_eq!(shown["status"], "closed", "an answered bead is closed");
    let comments = shown["comments"].as_array().expect("comments hydrated");
    assert!(
        comments.iter().any(|c| c["text"].as_str() == Some("Response: use OAuth2")),
        "the answer is recorded as a comment: {comments:?}"
    );

    // dismiss: closes without answering.
    assert_eq!(bd(d, &["human", "dismiss", &b]).2, 0, "dismiss succeeds");
    assert_eq!(json(d, &["show", &b])["status"], "closed");

    // The pending list is empty again; stats tell the two outcomes apart.
    assert!(
        json(d, &["human", "list"]).as_array().unwrap().is_empty(),
        "both beads have left the pending queue"
    );
    let s = json(d, &["human", "stats"]);
    assert_eq!(s["total"], 2, "{s}");
    assert_eq!(s["pending"], 0, "{s}");
    assert_eq!(s["responded"], 1, "{s}");
    assert_eq!(s["dismissed"], 1, "{s}");

    // A closed bead cannot be answered again, and an unknown id is a real failure.
    assert_eq!(bd(d, &["human", "respond", &a, "again"]).2, 1, "no responding to a closed bead");
    assert_eq!(bd(d, &["human", "dismiss", "t-nope"]).2, 1, "an unknown id fails");
    assert!(
        bd(d, &["human", "respond", &b]).2 != 0,
        "an empty response is refused"
    );

    // Read paths work under --readonly.
    assert_eq!(bd(d, &["--readonly", "human", "list"]).2, 0, "human list is read-only");
    assert_eq!(bd(d, &["--readonly", "human", "stats"]).2, 0, "human stats is read-only");

    std::fs::remove_dir_all(&dir).ok();
}
