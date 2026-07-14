//! End-to-end tests for the Dolt [`Storage`] implementation.
//!
//! # Read this before you trust a green run
//!
//! **There was no `dolt` binary on the machine where this file was written.**
//! Not one line below has ever executed against a real server. Every test here
//! begins by checking for `dolt` and, when it is missing, prints a skip notice
//! and returns — so a green `cargo test` on a machine without Dolt means
//! *nothing here was verified*, and stderr says so rather than the run quietly
//! reporting as coverage.
//!
//! What *is* verified without Dolt lives in the unit tests inside `store.rs`:
//! that the SQL this backend emits is MySQL rather than SQLite, and that the
//! schema is too. Those are real tests. They cannot tell you whether Dolt's
//! engine accepts the SQL, only that MySQL's dialect would.
//!
//! # Getting these to run
//!
//! Install Dolt (<https://github.com/dolthub/dolt>) and either:
//!
//! * point `BD_DOLT_TEST_URL` at a running server
//!   (`mysql://root@127.0.0.1:3306/beads`), or
//! * do nothing else — the harness below starts a throwaway `dolt sql-server`
//!   in a temp directory and tears it down afterwards.
//!
//! The spawn path deliberately *skips loudly* rather than failing when it cannot
//! get a server up. A red test would claim "the store is broken" when what
//! actually happened is "the harness could not start a server". A skip says
//! which.
//!
//! # Once `server.rs` lands
//!
//! `harness()` should collapse to `server::DoltServer::start(…)` plus
//! `DoltStore::new(…)`, deleting the private spawn code below. It exists only
//! because `store.rs` had to be testable before `server.rs` did.

use bd_core::{Dependency, DependencyType, Issue, IssueFilter, Priority, Status};
use bd_dolt::{DoltStore, require_dolt};
use bd_storage::{Identity, IssuePatch, Storage};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A store plus whatever has to stay alive for it to keep working.
struct Fixture {
    store: DoltStore,
    _server: Option<ServerGuard>,
    _dir: Option<PathBuf>,
}

impl std::ops::Deref for Fixture {
    type Target = DoltStore;
    fn deref(&self) -> &DoltStore {
        &self.store
    }
}

/// Kills the `dolt sql-server` on drop. An orphan holds the database lock, and
/// the *next* run then fails for a reason that looks like corruption rather than
/// like a stray process.
struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Cargo runs the tests in this binary on threads, in parallel, so each one
/// takes its own port.
static NEXT_PORT: AtomicU16 = AtomicU16::new(3399);

/// Open a store, or bail out of the test.
///
/// Both exits are silent-free: `require_dolt!` explains that there is no `dolt`,
/// and `harness` explains anything else. Neither ever lets a test pass by
/// covering nothing without saying so.
macro_rules! fixture {
    ($db:expr) => {{
        require_dolt!();
        match harness($db).await {
            Some(f) => f,
            None => return,
        }
    }};
}

async fn harness(db: &str) -> Option<Fixture> {
    let identity = Identity {
        actor: "tester".to_string(),
        session: Some("s1".to_string()),
    };

    // A server somebody else is running. Each test still gets its own database
    // inside it — they run concurrently and share nothing else.
    if let Ok(base) = std::env::var("BD_DOLT_TEST_URL") {
        let url = match provision(&base, db).await {
            Ok(url) => url,
            Err(e) => return skip(&format!("BD_DOLT_TEST_URL is set but unusable: {e}")),
        };
        return match DoltStore::connect(&url, identity, ".").await {
            Ok(store) => Some(Fixture {
                store,
                _server: None,
                _dir: None,
            }),
            Err(e) => skip(&format!("could not open {url}: {e}")),
        };
    }

    // Otherwise, a throwaway server of our own.
    let dir = std::env::temp_dir().join(format!("bd-dolt-{db}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return skip(&format!("could not create {}: {e}", dir.display()));
    }
    if let Err(e) = dolt(&dir, &["init", "--name", "tester", "--email", "t@example.com"]) {
        return skip(&format!("`dolt init` failed: {e}"));
    }

    let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
    let spawned = Command::new("dolt")
        .current_dir(&dir)
        .args([
            "sql-server",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--user",
            "root",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let server = match spawned {
        Ok(c) => ServerGuard(c),
        Err(e) => return skip(&format!("could not spawn `dolt sql-server`: {e}")),
    };

    // `dolt init` names the database after its directory, with everything that
    // is not alphanumeric folded to an underscore.
    let dbname: String = dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let url = format!("mysql://root@127.0.0.1:{port}/{dbname}");

    // Binding the port takes a moment. Poll for it rather than sleeping a
    // guessed interval and hoping.
    let mut last = String::new();
    for _ in 0..40 {
        match DoltStore::connect(&url, identity.clone(), &dir).await {
            Ok(store) => {
                return Some(Fixture {
                    store,
                    _server: Some(server),
                    _dir: Some(dir),
                });
            }
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
    skip(&format!("`dolt sql-server` never came up on {url}: {last}"))
}

/// Give each test its own database on a shared server.
async fn provision(base_url: &str, db: &str) -> Result<String, String> {
    let pool = bd_dolt::store::connect_pool(base_url)
        .await
        .map_err(|e| e.to_string())?;
    // `db` is a literal from this file, never user input. `raw_sql` because DDL
    // over the prepared-statement path is the corner of the protocol Dolt is
    // least likely to implement the way MySQL does.
    sqlx::raw_sql(&format!("CREATE DATABASE IF NOT EXISTS `{db}`"))
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;
    pool.close().await;

    let (head, _) = base_url.rsplit_once('/').ok_or("no path in the url")?;
    Ok(format!("{head}/{db}"))
}

fn dolt(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("dolt")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn skip(why: &str) -> Option<Fixture> {
    eprintln!(
        "SKIPPED: {why}\n\
         This test COVERED NOTHING. The Dolt store is UNVERIFIED on this machine."
    );
    None
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn issue(s: &DoltStore, id: &str) -> Issue {
    s.create_issue(&Issue::new(id, format!("title of {id}")))
        .await
        .unwrap()
}

async fn edge(s: &DoltStore, from: &str, to: &str, ty: &str) {
    s.add_dependency(&dep(from, to, ty, "")).await.unwrap();
}

fn dep(from: &str, to: &str, ty: &str, metadata: &str) -> Dependency {
    Dependency {
        issue_id: from.to_string(),
        depends_on_id: to.to_string(),
        dep_type: DependencyType::from(ty.to_string()),
        created_at: chrono::Utc::now(),
        created_by: String::new(),
        metadata: metadata.to_string(),
        thread_id: String::new(),
    }
}

/// Read the denormalized cache the way `bd blocked` does — through the store,
/// not by peeking at the column. A test that read the column directly would pass
/// even if the query that consumes it were wrong.
async fn is_blocked(s: &DoltStore, id: &str) -> bool {
    s.blocked_work(&IssueFilter::default())
        .await
        .unwrap()
        .iter()
        .any(|i| i.id == id)
}

// ---------------------------------------------------------------------------
// The core seam
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_issue_round_trips_through_every_column() {
    let s = fixture!("bd_roundtrip");

    let mut i = Issue::new("bd-1", "a title");
    i.description = "desc".into();
    i.design = "design".into();
    i.acceptance_criteria = "ac".into();
    i.notes = "notes".into();
    i.priority = Priority(1);
    i.assignee = "alice".into();
    i.owner = "bob".into();
    i.estimated_minutes = Some(90);
    i.due_at = Some(chrono::Utc::now());
    i.metadata = Some(serde_json::json!({"sprint": 4}));
    i.external_ref = Some("PROJ-9".into());
    i.source_system = "jira".into();
    i.labels = vec!["a".into(), "b".into()];

    s.create_issue(&i).await.unwrap();
    let got = s.get_issue("bd-1").await.unwrap().unwrap();

    assert_eq!(got.title, "a title");
    assert_eq!(got.priority, Priority(1));
    assert_eq!(got.assignee, "alice");
    assert_eq!(got.estimated_minutes, Some(90));
    assert_eq!(got.metadata, Some(serde_json::json!({"sprint": 4})));
    assert_eq!(got.external_ref.as_deref(), Some("PROJ-9"));
    assert_eq!(got.labels, vec!["a".to_string(), "b".to_string()]);
    assert!(!got.content_hash.is_empty());

    // DATETIME(6) keeps microseconds. A bare DATETIME truncates to the second,
    // which would collapse lease expiry and the (created_at, id) sort tiebreak
    // onto a one-second grid — so this assertion is load-bearing, not pedantic.
    let drift = (got.created_at - i.created_at).num_microseconds().unwrap();
    assert!(drift.abs() <= 1, "timestamp precision lost: {drift}µs");
}

/// The reason `schema.sql` pins `utf8mb4_0900_bin`. Under MySQL's *default*
/// collation this test fails: `assignee = 'alice'` would match `'Alice'` too,
/// and bd-query's property tests say string equality is byte-exact.
#[tokio::test]
async fn string_equality_is_byte_exact_not_case_folded() {
    let s = fixture!("bd_collation");

    let mut a = Issue::new("bd-lower", "x");
    a.assignee = "alice".into();
    s.create_issue(&a).await.unwrap();

    let mut b = Issue::new("bd-upper", "x");
    b.assignee = "Alice".into();
    s.create_issue(&b).await.unwrap();

    let found = s
        .list_issues(&IssueFilter::new().with_assignee("alice"))
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "the collation is case-insensitive");
    assert_eq!(found[0].id, "bd-lower");
}

/// The other half of that decision. `=` is byte-exact, but `--text` is documented
/// as a case-*insensitive* substring search — which under a `_bin` collation only
/// works because `push_filter` lowercases both sides by hand.
#[tokio::test]
async fn the_text_search_stays_case_insensitive() {
    let s = fixture!("bd_textsearch");
    s.create_issue(&Issue::new("bd-1", "Fix The Parser"))
        .await
        .unwrap();

    let found = s
        .list_issues(&IssueFilter {
            text: Some("fix the parser".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "LIKE went case-sensitive under the _bin collation");
}

#[tokio::test]
async fn a_patch_can_clear_a_field_as_well_as_set_one() {
    let s = fixture!("bd_patch");
    let mut i = Issue::new("bd-1", "x");
    i.defer_until = Some(chrono::Utc::now() + chrono::Duration::days(1));
    s.create_issue(&i).await.unwrap();

    let got = s.update_issue("bd-1", &IssuePatch::undefer()).await.unwrap();
    assert!(got.defer_until.is_none());
}

/// MySQL *parses and then ignores* an inline column-level `REFERENCES … ON
/// DELETE CASCADE`. Written the SQLite way the cascade would simply not exist,
/// and every one of these assertions would find the orphans it left behind.
#[tokio::test]
async fn deleting_an_issue_cascades_to_its_edges_labels_and_comments() {
    let s = fixture!("bd_cascade");
    issue(&s, "bd-a").await;
    issue(&s, "bd-b").await;
    edge(&s, "bd-a", "bd-b", "blocks").await;
    s.add_label("bd-a", "urgent").await.unwrap();
    s.add_comment("bd-a", "a note").await.unwrap();

    s.delete_issue("bd-a").await.unwrap();

    assert!(s.dependents_of("bd-b").await.unwrap().is_empty());
    assert!(s.list_labels().await.unwrap().is_empty());
    assert!(s.list_comments("bd-a").await.unwrap().is_empty());
    // The audit trail, by contrast, must survive the row it describes — which is
    // why `events` deliberately has no foreign key.
    assert!(!s.list_events("bd-a").await.unwrap().is_empty());
}

/// A bare `COUNT(*)` is `BIGINT UNSIGNED` on MySQL and sqlx refuses to decode
/// one into an `i64`. Without the `CAST(… AS SIGNED)` every line here fails at
/// runtime — and it is `bd list`, `bd status` and `next_id` that fail with it.
#[tokio::test]
async fn counts_and_stats_decode_as_numbers() {
    let s = fixture!("bd_counts");
    issue(&s, "bd-1").await;
    issue(&s, "bd-2").await;
    s.close_issue("bd-2", "done").await.unwrap();

    assert_eq!(s.count_issues(&IssueFilter::default()).await.unwrap(), 2);

    let st = s.stats().await.unwrap();
    assert_eq!(st.total, 2);
    assert_eq!(st.open, 1);
    assert_eq!(st.closed, 1);
    assert_eq!(st.ready, 1);
    assert_eq!(st.blocked, 0);

    // And `next_id` counts rows to size the id it mints.
    let id = s.next_id("bd", "a new bead", "").await.unwrap();
    assert!(id.starts_with("bd-"), "{id}");
}

/// `KEY` is a reserved word in MySQL. The SQLite spelling of these statements is
/// a syntax error against Dolt.
#[tokio::test]
async fn config_survives_the_reserved_word_key() {
    let s = fixture!("bd_config");
    s.set_config("issue.prefix", "bd").await.unwrap();
    // An upsert: a second write of the same key must not raise a duplicate key.
    s.set_config("issue.prefix", "xy").await.unwrap();

    assert_eq!(
        s.get_config("issue.prefix").await.unwrap().as_deref(),
        Some("xy")
    );
    assert_eq!(
        s.list_config().await.unwrap(),
        vec![("issue.prefix".to_string(), "xy".to_string())]
    );
}

#[tokio::test]
async fn a_comment_is_idempotent_under_reimport() {
    let s = fixture!("bd_comments");
    issue(&s, "bd-1").await;

    let c = s.add_comment("bd-1", "first").await.unwrap();
    assert!(
        c.id.contains('-') && c.id.len() >= 32,
        "the id is a client-minted UUID: MySQL has no RETURNING to ask for one"
    );

    let mut again = c.clone();
    again.text = "edited".into();
    s.upsert_comment(&again).await.unwrap();

    let all = s.list_comments("bd-1").await.unwrap();
    assert_eq!(all.len(), 1, "the upsert duplicated the comment");
    assert_eq!(all[0].text, "edited");
    assert_eq!(
        all[0].author, c.author,
        "an import must not reattribute a comment to the importer"
    );
}

#[tokio::test]
async fn claiming_is_a_fence_not_a_suggestion() {
    let s = fixture!("bd_claim");
    issue(&s, "bd-1").await;

    s.claim_issue("bd-1", chrono::Duration::hours(1))
        .await
        .unwrap();
    let got = s.get_issue("bd-1").await.unwrap().unwrap();
    assert_eq!(got.assignee, "tester");
    assert_eq!(got.status, Status::InProgress);

    // A held bead is not claimable, so `bd ready` must not offer it: the second
    // agent would otherwise find out by failing, after it had started thinking.
    assert!(s.ready_work(&IssueFilter::ready()).await.unwrap().is_empty());

    s.release_claim("bd-1").await.unwrap();
    assert_eq!(s.ready_work(&IssueFilter::ready()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_edge_that_would_close_a_loop_is_refused() {
    let s = fixture!("bd_cycle");
    issue(&s, "bd-a").await;
    issue(&s, "bd-b").await;
    edge(&s, "bd-a", "bd-b", "blocks").await;

    let err = s.add_dependency(&dep("bd-b", "bd-a", "blocks", "")).await;
    assert!(matches!(err, Err(bd_storage::Error::Cycle(_))), "{err:?}");
    assert!(s.find_cycles().await.unwrap().is_empty());
}

/// Two beads may legitimately be joined by several edges at once, so a delete
/// keyed on the pair alone would destroy all of them — silently, while reporting
/// success. The type is part of the primary key for exactly this reason.
#[tokio::test]
async fn removing_an_edge_removes_exactly_one_edge() {
    let s = fixture!("bd_edgetype");
    issue(&s, "bd-a").await;
    issue(&s, "bd-b").await;
    edge(&s, "bd-a", "bd-b", "blocks").await;
    edge(&s, "bd-a", "bd-b", "related").await;

    let blocks = DependencyType::from("blocks".to_string());
    s.remove_dependency("bd-a", "bd-b", &blocks).await.unwrap();

    let left = s.dependencies_of("bd-a").await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].dep_type.as_str(), "related");

    // Removing an edge that is not there is an error, not a no-op: a typo'd edge
    // type must not report success.
    assert!(s.remove_dependency("bd-a", "bd-b", &blocks).await.is_err());
}

/// The upsert path. `ON DUPLICATE KEY UPDATE`, not `REPLACE INTO`: a REPLACE is
/// a DELETE plus an INSERT, so re-adding an existing edge would show up in a
/// Dolt diff as one edge removed and another added.
#[tokio::test]
async fn re_adding_an_edge_updates_it_rather_than_duplicating_it() {
    let s = fixture!("bd_reedge");
    issue(&s, "bd-a").await;
    issue(&s, "bd-b").await;

    edge(&s, "bd-a", "bd-b", "blocks").await;
    s.add_dependency(&dep("bd-a", "bd-b", "blocks", r#"{"note":"again"}"#))
        .await
        .unwrap();

    let edges = s.dependencies_of("bd-a").await.unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].metadata, r#"{"note":"again"}"#);
}

// ---------------------------------------------------------------------------
// The fixpoint — the thing that matters most
// ---------------------------------------------------------------------------

/// `bd-e` blocks `bd-d`; `bd-c` is a child of `bd-d`, `bd-b` of `bd-c`, `bd-a` of
/// `bd-b`. Closing `bd-e` frees the whole chain — but only a *fixpoint* discovers
/// that. One mark/unmark pass propagates the unblock exactly one level.
async fn chain(s: &DoltStore) {
    for id in ["bd-a", "bd-b", "bd-c", "bd-d", "bd-e"] {
        issue(s, id).await;
    }
    edge(s, "bd-d", "bd-e", "blocks").await;
    edge(s, "bd-c", "bd-d", "parent-child").await;
    edge(s, "bd-b", "bd-c", "parent-child").await;
    edge(s, "bd-a", "bd-b", "parent-child").await;
}

#[tokio::test]
async fn blocking_propagates_down_the_whole_subtree() {
    let s = fixture!("bd_blockdown");
    chain(&s).await;

    for id in ["bd-a", "bd-b", "bd-c", "bd-d"] {
        assert!(is_blocked(&s, id).await, "{id} should be blocked");
    }
    assert!(
        !is_blocked(&s, "bd-e").await,
        "the blocker itself is not blocked"
    );
}

/// The test that justifies the fixpoint. A single-pass implementation frees
/// `bd-d` and leaves `bd-a`, `bd-b` and `bd-c` wrongly blocked — so `bd ready`
/// would silently hide three claimable beads and nothing would say a word.
#[tokio::test]
async fn unblocking_reaches_the_deep_end_of_the_chain() {
    let s = fixture!("bd_unblock");
    chain(&s).await;

    s.close_issue("bd-e", "done").await.unwrap();

    for id in ["bd-a", "bd-b", "bd-c", "bd-d"] {
        assert!(
            !is_blocked(&s, id).await,
            "{id} is still blocked after its blocker closed: the recompute is not \
             running to a fixpoint, and `bd ready` is now lying"
        );
    }
    // Four, not five: `bd-e` is closed, and closed work is not ready work.
    assert_eq!(s.ready_work(&IssueFilter::ready()).await.unwrap().len(), 4);
}

/// What `vc.rs` must call after every merge and pull. Rows arriving from a sync
/// were never seen by a local write path, so the incremental recompute has no
/// seed and the cache is stale *by definition*. Here that staleness is
/// manufactured by writing the column behind the store's back — which is exactly
/// what a merge does.
#[tokio::test]
async fn a_full_recompute_repairs_a_cache_no_write_path_maintained() {
    let s = fixture!("bd_recompute");
    chain(&s).await;

    sqlx::query("UPDATE issues SET is_blocked = 0")
        .execute(s.pool())
        .await
        .unwrap();
    assert!(
        !is_blocked(&s, "bd-d").await,
        "precondition: the cache is now wrong"
    );

    let changed = s.recompute_blocked().await.unwrap();
    assert!(changed > 0);
    for id in ["bd-a", "bd-b", "bd-c", "bd-d"] {
        assert!(
            is_blocked(&s, id).await,
            "{id} was not repaired by recompute_blocked"
        );
    }
}

/// `is_blocked` is derived state. Writing `updated_at` alongside it stamps local
/// wall-clock time onto a row in a *version-controlled* table for a change the
/// user never made — and hands the next merge a conflict on a column neither
/// clone edited. On SQLite that was a nicety; here the table actually merges.
#[tokio::test]
async fn a_recompute_never_bumps_updated_at() {
    let s = fixture!("bd_updatedat");
    chain(&s).await;

    let stamps = |issues: Vec<Issue>| -> Vec<(String, chrono::DateTime<chrono::Utc>)> {
        issues.into_iter().map(|i| (i.id, i.updated_at)).collect()
    };

    let before = stamps(s.list_issues(&IssueFilter::default()).await.unwrap());

    sqlx::query("UPDATE issues SET is_blocked = 0")
        .execute(s.pool())
        .await
        .unwrap();
    s.recompute_blocked().await.unwrap();

    let after = stamps(s.list_issues(&IssueFilter::default()).await.unwrap());
    assert_eq!(
        before, after,
        "the recompute moved updated_at; two clones will now conflict on a column \
         neither of them edited"
    );
}

/// `B conditional-blocks A` means "run B only if A **fails**". A successful close
/// of A leaves B blocked forever, on purpose: a store that silently closed beads
/// nobody asked it to close would be worse than one that leaves a visibly stuck
/// bead for a human to reap.
#[tokio::test]
async fn a_conditional_block_releases_only_on_a_failing_close() {
    let s = fixture!("bd_conditional");
    for id in ["bd-ok", "bd-fallback", "bd-bad", "bd-recover"] {
        issue(&s, id).await;
    }
    edge(&s, "bd-fallback", "bd-ok", "conditional-blocks").await;
    edge(&s, "bd-recover", "bd-bad", "conditional-blocks").await;

    s.close_issue("bd-ok", "done").await.unwrap();
    s.close_issue("bd-bad", "failed: the build broke")
        .await
        .unwrap();

    assert!(
        is_blocked(&s, "bd-fallback").await,
        "the failure path must stay shut when its target succeeded"
    );
    assert!(
        !is_blocked(&s, "bd-recover").await,
        "the failure path must open when its target failed"
    );
}

/// The `any-children` gate is read out of the edge's JSON metadata — and MySQL's
/// `JSON_EXTRACT` hands back a *quoted* JSON string, `"any-children"`. Without
/// `JSON_UNQUOTE` the comparison is false for every gate, forever, with no error
/// anywhere: the waiter simply never becomes ready.
#[tokio::test]
async fn a_waits_for_gate_reads_its_json_metadata() {
    let s = fixture!("bd_gate");
    for id in ["bd-spawn", "bd-c1", "bd-c2", "bd-waiter"] {
        issue(&s, id).await;
    }
    edge(&s, "bd-c1", "bd-spawn", "parent-child").await;
    edge(&s, "bd-c2", "bd-spawn", "parent-child").await;
    s.add_dependency(&dep(
        "bd-waiter",
        "bd-spawn",
        "waits-for",
        r#"{"gate":"any-children"}"#,
    ))
    .await
    .unwrap();

    assert!(
        is_blocked(&s, "bd-waiter").await,
        "no child has closed yet, so even an any-children gate is shut"
    );

    s.close_issue("bd-c1", "done").await.unwrap();
    assert!(
        !is_blocked(&s, "bd-waiter").await,
        "an any-children gate opens on the first close — unless JSON_EXTRACT's \
         quotes were never stripped, in which case it never opens at all"
    );
}

// ---------------------------------------------------------------------------
// Capabilities — the whole point of the crate
// ---------------------------------------------------------------------------

/// `bd branch`, `bd dolt push`, `bd vc` and `bd diff` are finished commands that
/// exit 2 on SQLite ("this backend has no commit graph"). These three accessors
/// are the entire difference between that and a working command.
#[tokio::test]
async fn the_dolt_store_advertises_a_commit_graph() {
    let s = fixture!("bd_caps");
    let store: &dyn Storage = &s.store;

    assert!(store.version_control().is_some());
    assert!(store.remote().is_some());
    assert!(store.history().is_some());
    assert!(store.has_commit_graph());
    assert_eq!(store.backend(), bd_storage::Backend::Dolt);
}
