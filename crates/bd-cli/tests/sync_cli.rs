//! `bd export`, `bd import`, and the rest of the sync family, through the real
//! binary and a real database.
//!
//! The property that matters here is **idempotency**. An import that duplicates
//! comments, or restamps them with the name of whoever ran it, is worse than one
//! that drops them outright: it corrupts the record silently, and it corrupts it
//! a little more on every run. So the test imports the same file twice and looks
//! very carefully at what changed.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

/// Run `bd` inside `dir` as `actor`. Returns stdout and the exit code.
fn run_as(dir: &Path, actor: &str, args: &[&str]) -> (String, i32) {
    let out = bd()
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .env("BEADS_ACTOR", actor)
        .output()
        .expect("run bd");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// This issue's record, read back out of a fresh export.
///
/// Export is the only reader that hydrates every relation, which makes it the
/// honest way to ask what actually landed in the database.
fn exported(dir: &Path, id: &str) -> serde_json::Value {
    let (jsonl, code) = run_as(dir, "reader", &["export"]);
    assert_eq!(code, 0, "export failed: {jsonl}");
    jsonl
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("export emits valid JSONL"))
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("{id} is missing from the export"))
}

fn comment_authors(record: &serde_json::Value) -> Vec<String> {
    record["comments"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .map(|c| c["author"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Export a workspace with comments, import it into a fresh one, and then do it
/// again. The second import is the whole test: nothing may move.
#[test]
fn importing_twice_restores_comments_once_and_keeps_their_authors() {
    let src = tempdir("sync-src");
    let dst = tempdir("sync-dst");

    assert_eq!(run_as(&src, "alice", &["init", "--prefix", "s"]).1, 0, "init");

    let (out, code) = run_as(&src, "alice", &["create", "Write the exporter", "-l", "urgent"]);
    assert_eq!(code, 0, "{out}");
    let id = out.rsplit(' ').next().expect("create prints the id").to_string();

    // Two *different* authors on purpose. With only one, "the author survived
    // the round trip" and "the author was overwritten with the importer's name"
    // are indistinguishable whenever the importer happens to be that author.
    assert_eq!(run_as(&src, "alice", &["comment", &id, "alice was here"]).1, 0);
    assert_eq!(run_as(&src, "carol", &["comment", &id, "so was carol"]).1, 0);

    let dump = src.join("dump.jsonl");
    let (out, code) = run_as(&src, "alice", &["export", "-o", dump.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");

    // The export has to carry the comments, or the import has nothing to restore.
    assert_eq!(
        comment_authors(&exported(&src, &id)),
        ["alice", "carol"],
        "export dropped the comments"
    );

    // --- first import, into a workspace that has never seen this issue ---

    assert_eq!(run_as(&dst, "bob", &["init", "--prefix", "s"]).1, 0, "init");

    let (out, code) = run_as(&dst, "bob", &["--json", "import", dump.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let doc: serde_json::Value = serde_json::from_str(&out).expect("--json import emits JSON");
    assert_eq!(doc["created"], 1);
    assert_eq!(doc["comments"], 2, "import must restore the comments");
    // The field that used to announce the data loss. Its absence is the fix.
    assert!(
        doc.get("comments_not_imported").is_none(),
        "import is still reporting dropped comments"
    );

    let record = exported(&dst, &id);
    assert_eq!(
        comment_authors(&record),
        ["alice", "carol"],
        "import must preserve each comment's original author, not stamp the importer on all of them"
    );
    assert_eq!(record["comments"][0]["text"], "alice was here");
    assert_eq!(record["comments"][1]["text"], "so was carol");
    assert_eq!(record["labels"][0], "urgent", "import dropped the labels");

    // --- second import, byte-identical, into the workspace that now has it ---

    let (out, code) = run_as(&dst, "bob", &["--json", "import", dump.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let doc: serde_json::Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(doc["created"], 0, "the issue already existed");
    assert_eq!(doc["updated"], 1);

    let record = exported(&dst, &id);
    let authors = comment_authors(&record);
    assert_eq!(
        authors.len(),
        2,
        "a second import duplicated the comments (got {authors:?}) — \
         re-importing a file must be a no-op, not a way to grow it"
    );
    assert_eq!(
        authors, ["alice", "carol"],
        "a second import reattributed the comments to whoever ran it"
    );
    assert!(
        !authors.iter().any(|a| a == "bob"),
        "the importer must never become the author: {authors:?}"
    );

    cleanup(&[src, dst]);
}

/// `--dry-run` reports what it would do and touches nothing.
#[test]
fn a_dry_run_import_reports_comments_but_writes_none() {
    let src = tempdir("sync-dry-src");
    let dst = tempdir("sync-dry-dst");

    assert_eq!(run_as(&src, "alice", &["init", "--prefix", "d"]).1, 0);
    let (id, code) = run_as(&src, "alice", &["q", "Something worth saying"]);
    assert_eq!(code, 0);
    assert_eq!(run_as(&src, "alice", &["comment", &id, "a note"]).1, 0);

    let dump = src.join("dump.jsonl");
    assert_eq!(
        run_as(&src, "alice", &["export", "-o", dump.to_str().unwrap()]).1,
        0
    );

    assert_eq!(run_as(&dst, "bob", &["init", "--prefix", "d"]).1, 0);
    let (out, code) = run_as(
        &dst,
        "bob",
        &["--json", "import", "--dry-run", dump.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{out}");
    let doc: serde_json::Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["created"], 1);
    assert_eq!(doc["comments"], 1);

    // Nothing was written, so nothing comes back out.
    let (jsonl, code) = run_as(&dst, "bob", &["export"]);
    assert_eq!(code, 0);
    assert!(jsonl.trim().is_empty(), "a dry run wrote to the database: {jsonl}");

    cleanup(&[src, dst]);
}

/// Federation needs peers, remotes, and a commit graph to exchange. SQLite has
/// none of those, and that is a property of the backend rather than a gap in the
/// port — so it must exit 2, not 64. Collapsing the two would make `bd
/// federation` look like unfinished work forever.
#[test]
fn federation_is_a_capability_gap_not_a_stub() {
    let tmp = tempdir("sync-fed");
    assert_eq!(run_as(&tmp, "alice", &["init"]).1, 0);

    for args in [
        vec!["--json", "federation", "status"],
        vec!["--json", "federation", "list-peers"],
        vec!["--json", "federation", "add-peer", "north", "http://example"],
    ] {
        let (out, code) = run_as(&tmp, "alice", &args);
        assert_eq!(code, 2, "{args:?} must be exit 2, not 64: {out}");
        let doc: serde_json::Value = serde_json::from_str(&out).expect("JSON");
        assert_eq!(doc["error"], "unsupported_backend");
        assert_eq!(doc["backend"], "sqlite");
    }

    cleanup(&[tmp]);
}

/// `bd mail` owns no mailbox: it shells out to whatever does. So the two things
/// worth testing are that it says how to configure itself when it cannot, and
/// that it hands the provider's verdict back to the shell unflattened.
#[test]
fn mail_delegates_and_carries_the_providers_exit_code_out() {
    let tmp = tempdir("sync-mail");
    assert_eq!(run_as(&tmp, "alice", &["init"]).1, 0);

    let unconfigured = bd()
        .args(["-C", tmp.to_str().unwrap(), "mail"])
        .env_remove("BEADS_MAIL_DELEGATE")
        .env_remove("BD_MAIL_DELEGATE")
        .output()
        .expect("run bd");
    assert_eq!(
        unconfigured.status.code(),
        Some(1),
        "an unconfigured provider is a misconfiguration (1), not a missing feature (64)"
    );
    let err = String::from_utf8_lossy(&unconfigured.stderr);
    assert!(
        err.contains("mail.delegate"),
        "the error has to name the setting that fixes it: {err}"
    );

    // The shim splits the configured command on whitespace — it is a command
    // line, not a shell line — so a build path with a space in it cannot be used
    // as a delegate. That is a real limitation of the design, and it is exactly
    // why this is a skip rather than an escape.
    let exe = env!("CARGO_BIN_EXE_bd");
    if exe.contains(char::is_whitespace) || tmp.to_str().unwrap().contains(char::is_whitespace) {
        cleanup(&[tmp]);
        return;
    }

    // Delegate to bd itself: `bd migrate` is a stub, so it exits 64. Nothing
    // else in the pipeline produces a 64, which makes it proof that the code we
    // exit with is the *provider's* and not one we invented.
    //
    // This said `gc` until `gc` was implemented -- the second test in this repo
    // to be pinned to "gc is a stub" and broken by that ceasing to be true.
    // `migrate` is the safer choice: it needs a schema version to migrate
    // between, and this port does not have one.
    let delegate = format!("{exe} -C {} migrate", tmp.to_str().unwrap());
    let out = bd()
        .args(["-C", tmp.to_str().unwrap(), "mail"])
        .env("BEADS_MAIL_DELEGATE", &delegate)
        .output()
        .expect("run bd");
    assert_eq!(
        out.status.code(),
        Some(64),
        "bd mail must exit with the provider's code, not translate it"
    );

    cleanup(&[tmp]);
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-cli-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

fn cleanup(dirs: &[PathBuf]) {
    for d in dirs {
        std::fs::remove_dir_all(d).ok();
    }
}
