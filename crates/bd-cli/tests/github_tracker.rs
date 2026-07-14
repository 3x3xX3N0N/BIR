//! The GitHub tracker, against recorded fixtures and a real database.
//!
//! No network, no credentials: [`FakeHttp`] replays canned bodies keyed by
//! `"METHOD url"` and records what was asked. That last part is what these tests
//! turn on — a tracker that pages correctly and one that silently reads only the
//! first hundred issues return the same *shape* of answer. They differ only in
//! the requests they make, so that is what gets asserted.
//!
//! The test that matters most is [`pull_twice_creates_nothing_the_second_time`].
//! An integration that loses its join key does not fail: it succeeds, twice, and
//! the backlog doubles.

use std::path::PathBuf;
use std::process::Command;

use bd_cli::cli::Cli;
use bd_cli::context::{Ctx, Need};
use bd_cli::integrations::github::{self, GitHub, MARKER_LABEL, PER_PAGE};
use bd_cli::integrations::http::{FakeHttp, Method};
use bd_cli::integrations::Tracker;
use bd_core::{Issue, IssueFilter, IssueType, Priority, Status};
use clap::Parser;
use serde_json::{json, Value};

const REPO: &str = "octo/demo";
const PAGE1: &str = "https://api.github.com/repos/octo/demo/issues?state=all&per_page=100&page=1";
const PAGE2: &str = "https://api.github.com/repos/octo/demo/issues?state=all&per_page=100&page=2";
const CREATE: &str = "https://api.github.com/repos/octo/demo/issues";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A real workspace, made by the real `bd init`, with `github.repo` set.
///
/// Config lands in the store's config table (where `bd config set` puts it), not
/// in `.beads/config.yaml` — and the token is never written anywhere, which is
/// why nothing here sets one.
async fn workspace(tag: &str) -> Ctx {
    let dir = tempdir(tag);
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap(), "init", "--prefix", "t"])
        .output()
        .expect("run bd init");
    assert!(
        out.status.success(),
        "bd init: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cli = Cli::try_parse_from(["bd", "-C", dir.to_str().unwrap(), "github", "status"])
        .expect("parse");
    let ctx = Ctx::build(&cli, Need::Workspace).await.expect("build ctx");
    ctx.store()
        .await
        .unwrap()
        .set_config(github::REPO_KEY, REPO)
        .await
        .unwrap();
    ctx
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("bd-gh-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// One issue as GitHub renders it, with the fields we read.
fn gh_issue(number: u64, title: &str, state: &str, labels: &[&str]) -> Value {
    json!({
        "number": number,
        "title": title,
        "body": format!("body of #{number}"),
        "state": state,
        "labels": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
        "created_at": "2026-01-02T03:04:05Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": if state == "closed" { json!("2026-02-02T03:04:05Z") } else { Value::Null },
    })
}

/// The whole point of this fixture is the `pull_request` key: to GitHub a PR
/// *is* an issue, and it comes back from the issues endpoint like any other.
fn gh_pull_request(number: u64, title: &str) -> Value {
    let mut v = gh_issue(number, title, "open", &[]);
    v["pull_request"] = json!({"url": format!("https://api.github.com/repos/octo/demo/pulls/{number}")});
    v
}

/// One stubbed page of results.
fn page(http: FakeHttp, url: &str, issues: Vec<Value>) -> FakeHttp {
    http.on(Method::Get, url, 200, &Value::Array(issues).to_string())
}

async fn all_issues(ctx: &Ctx) -> Vec<Issue> {
    ctx.store()
        .await
        .unwrap()
        .list_issues(&IssueFilter::default())
        .await
        .unwrap()
}

/// The bead GitHub issue `number` maps to, found the way the tracker finds it:
/// by the (source_system, external_ref) pair.
async fn bead_for(ctx: &Ctx, number: u64) -> Issue {
    let want = number.to_string();
    let found = all_issues(ctx)
        .await
        .into_iter()
        .find(|i| i.external_ref.as_deref() == Some(&want) && i.source_system == "github")
        .unwrap_or_else(|| panic!("no bead is joined to github #{number}"));
    // Re-read so the labels are hydrated; `list_issues` does not hydrate.
    ctx.store()
        .await
        .unwrap()
        .get_issue(&found.id)
        .await
        .unwrap()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pull_maps_a_github_issue_onto_a_bead() {
    let ctx = workspace("map").await;
    let http = page(
        FakeHttp::new(),
        PAGE1,
        vec![
            gh_issue(7, "the parser eats the last line", "open", &["bug", "parser"]),
            gh_issue(9, "shipped", "closed", &[]),
        ],
    );

    let report = GitHub.pull(&ctx, &http).await.unwrap();
    assert_eq!((report.pulled, report.created, report.updated), (2, 2, 0));

    let bug = bead_for(&ctx, 7).await;
    assert_eq!(bug.title, "the parser eats the last line");
    assert_eq!(bug.description, "body of #7");
    assert_eq!(bug.status, Status::Open);
    assert_eq!(bug.external_ref.as_deref(), Some("7"));
    assert_eq!(bug.source_system, "github");
    // A `bug` label is a type signal; the rest are just labels, and all of them
    // are kept.
    assert_eq!(bug.issue_type, IssueType::Bug);
    let mut labels = bug.labels.clone();
    labels.sort();
    assert_eq!(labels, vec!["bug".to_string(), "parser".to_string()]);
    // GitHub has no priority, so nothing is invented: the default stands.
    assert_eq!(bug.priority, Priority::NORMAL);

    let closed = bead_for(&ctx, 9).await;
    assert_eq!(closed.status, Status::Closed);
    // An unlabelled issue says nothing about its type, so it gets the default.
    assert_eq!(closed.issue_type, IssueType::Task);

    // The request went where it was supposed to, with the paging GitHub needs.
    let urls: Vec<String> = http.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(urls, vec![PAGE1.to_string()]);
}

/// The one that matters. A sync that cannot recognize what it already has does
/// not fail — it succeeds and doubles the backlog, every single run.
#[tokio::test]
async fn pull_twice_creates_nothing_the_second_time() {
    let ctx = workspace("idem").await;
    let fixture = || {
        vec![
            gh_issue(1, "one", "open", &["bug"]),
            gh_issue(2, "two", "closed", &[]),
            gh_issue(3, "three", "open", &["docs"]),
        ]
    };

    let first = GitHub
        .pull(&ctx, &page(FakeHttp::new(), PAGE1, fixture()))
        .await
        .unwrap();
    assert_eq!((first.created, first.updated), (3, 0));
    assert_eq!(all_issues(&ctx).await.len(), 3);

    let second = GitHub
        .pull(&ctx, &page(FakeHttp::new(), PAGE1, fixture()))
        .await
        .unwrap();
    assert_eq!(
        (second.pulled, second.created, second.updated),
        (3, 0, 3),
        "the second pull must join on (source_system, external_ref), not insert"
    );
    assert_eq!(all_issues(&ctx).await.len(), 3, "the backlog was duplicated");
}

#[tokio::test]
async fn pull_carries_remote_edits_onto_the_existing_bead() {
    let ctx = workspace("edit").await;
    GitHub
        .pull(
            &ctx,
            &page(
                FakeHttp::new(),
                PAGE1,
                vec![gh_issue(4, "before", "open", &["bug", "stale-label"])],
            ),
        )
        .await
        .unwrap();

    // Retitled, closed, relabelled upstream.
    let report = GitHub
        .pull(
            &ctx,
            &page(
                FakeHttp::new(),
                PAGE1,
                vec![gh_issue(4, "after", "closed", &["bug", "fresh-label"])],
            ),
        )
        .await
        .unwrap();
    assert_eq!((report.created, report.updated), (0, 1));

    let bead = bead_for(&ctx, 4).await;
    assert_eq!(bead.title, "after");
    assert_eq!(bead.status, Status::Closed);
    assert!(bead.closed_at.is_some(), "closing must stamp closed_at");
    let mut labels = bead.labels.clone();
    labels.sort();
    assert_eq!(
        labels,
        vec!["bug".to_string(), "fresh-label".to_string()],
        "labels are a set that GitHub owns: the dropped one must go"
    );
}

/// The single most common bug in a GitHub integration. The issues endpoint
/// returns pull requests, and a tracker that does not notice fills the backlog
/// with them — silently, and forever.
#[tokio::test]
async fn pull_requests_are_skipped_and_reported() {
    let ctx = workspace("prs").await;
    let http = page(
        FakeHttp::new(),
        PAGE1,
        vec![
            gh_issue(10, "a real issue", "open", &[]),
            gh_pull_request(11, "fix: the thing"),
            gh_pull_request(12, "chore: bump deps"),
        ],
    );

    let report = GitHub.pull(&ctx, &http).await.unwrap();
    assert_eq!(
        (report.pulled, report.created),
        (1, 1),
        "a PR is not an issue and must not be counted as one"
    );
    assert_eq!(report.skipped.len(), 2);
    assert!(
        report.skipped.iter().all(|s| s.contains("pull request")),
        "the skip must say why: {:?}",
        report.skipped
    );
    assert!(report.skipped.iter().any(|s| s.contains("#11")));

    let issues = all_issues(&ctx).await;
    assert_eq!(issues.len(), 1, "a PR landed in the tracker");
    assert_eq!(issues[0].external_ref.as_deref(), Some("10"));
}

/// A repo with 101 issues is not exotic, and `per_page=100` is GitHub's ceiling.
/// A tracker that reads page one and stops looks *exactly* like one that worked.
#[tokio::test]
async fn pagination_is_followed_to_the_last_page() {
    let ctx = workspace("pages").await;

    // A full page is the signal that another may exist, so page one must be
    // exactly PER_PAGE long — a short page ends the walk.
    let full: Vec<Value> = (1..=PER_PAGE as u64)
        .map(|n| gh_issue(n, &format!("issue {n}"), "open", &[]))
        .collect();
    let rest = vec![
        gh_issue(101, "the hundred and first", "open", &[]),
        gh_issue(102, "and the next", "closed", &[]),
    ];

    let http = page(page(FakeHttp::new(), PAGE1, full), PAGE2, rest);
    let report = GitHub.pull(&ctx, &http).await.unwrap();

    assert_eq!(report.pulled, PER_PAGE as u64 + 2);
    assert_eq!(report.created, PER_PAGE as u64 + 2);
    assert!(report.skipped.is_empty());

    // The proof is in what was *asked*, not in what came back.
    let urls: Vec<String> = http.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(
        urls,
        vec![PAGE1.to_string(), PAGE2.to_string()],
        "page 2 was never requested, so 100 issues would have been silently lost"
    );

    // The issue that only exists on page two.
    assert_eq!(bead_for(&ctx, 101).await.title, "the hundred and first");
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// A local bead marked for GitHub is created there, and the remote number is
/// written back *immediately* — a bead that forgets the issue it just made will
/// make it again on the next push.
#[tokio::test]
async fn push_creates_a_marked_bead_and_records_the_remote_number() {
    let ctx = workspace("push-new").await;
    let store = ctx.store().await.unwrap();
    let id = store.next_id("t", "ship the thing", "").await.unwrap();
    store
        .create_issue(&Issue {
            description: "please".into(),
            labels: vec![MARKER_LABEL.to_string(), "urgent".to_string()],
            ..Issue::new(&id, "ship the thing")
        })
        .await
        .unwrap();

    let http = FakeHttp::new().on(Method::Post, CREATE, 201, &json!({"number": 42}).to_string());
    let report = GitHub.push(&ctx, &http).await.unwrap();
    assert_eq!(report.pushed, 1);

    let sent = http.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method.as_str(), "POST");
    let body: Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["title"], "ship the thing");
    assert_eq!(body["body"], "please");
    assert_eq!(body["state"], "open");
    assert_eq!(
        body["labels"],
        json!(["urgent"]),
        "the `github` marker is beads' bookkeeping and must not be pushed into someone's repo"
    );

    let bead = store.get_issue(&id).await.unwrap().unwrap();
    assert_eq!(bead.external_ref.as_deref(), Some("42"));
}

/// The round trip that duplicates the backlog if the marker is not honored: push
/// creates issue 42, then a pull sees 42 come back. The bead we just created has
/// no `source_system` (there is no `IssuePatch` field for one), so it is found by
/// its marker label — or it is found not at all and inserted a second time.
#[tokio::test]
async fn a_pushed_bead_is_not_duplicated_by_the_next_pull() {
    let ctx = workspace("roundtrip").await;
    let store = ctx.store().await.unwrap();
    let id = store.next_id("t", "local work", "").await.unwrap();
    store
        .create_issue(&Issue {
            labels: vec![MARKER_LABEL.to_string()],
            ..Issue::new(&id, "local work")
        })
        .await
        .unwrap();

    let http = FakeHttp::new().on(Method::Post, CREATE, 201, &json!({"number": 42}).to_string());
    GitHub.push(&ctx, &http).await.unwrap();

    let report = GitHub
        .pull(
            &ctx,
            &page(
                FakeHttp::new(),
                PAGE1,
                vec![gh_issue(42, "local work", "open", &[])],
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        (report.created, report.updated),
        (0, 1),
        "the bead this push created was pulled back in as a second copy"
    );
    assert_eq!(all_issues(&ctx).await.len(), 1);

    // And the marker survives the pull, or the *next* round trip duplicates it.
    let bead = store.get_issue(&id).await.unwrap().unwrap();
    assert!(bead.labels.iter().any(|l| l == MARKER_LABEL));
}

#[tokio::test]
async fn push_updates_a_bead_that_came_from_github() {
    let ctx = workspace("push-existing").await;
    GitHub
        .pull(
            &ctx,
            &page(
                FakeHttp::new(),
                PAGE1,
                vec![gh_issue(5, "from github", "open", &["bug"])],
            ),
        )
        .await
        .unwrap();

    // Close it locally; push must carry that upstream.
    let store = ctx.store().await.unwrap();
    let bead = bead_for(&ctx, 5).await;
    store.close_issue(&bead.id, "done").await.unwrap();

    let url = "https://api.github.com/repos/octo/demo/issues/5";
    let http = FakeHttp::new().on(Method::Patch, url, 200, &json!({"number": 5}).to_string());
    let report = GitHub.push(&ctx, &http).await.unwrap();
    assert_eq!(report.pushed, 1);

    let sent = http.requests();
    assert_eq!(sent.len(), 1, "an existing issue must be patched, not recreated");
    assert_eq!(sent[0].method.as_str(), "PATCH");
    assert_eq!(sent[0].url, url);
    let body: Value = serde_json::from_str(sent[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["state"], "closed");
    assert_eq!(body["labels"], json!(["bug"]));
}

/// A bead that belongs to another tracker is not GitHub's to push, whatever it
/// happens to be labelled — pushing it would fork the same work across two
/// systems. It is declined out loud.
#[tokio::test]
async fn push_declines_a_bead_owned_by_another_tracker() {
    let ctx = workspace("push-foreign").await;
    let store = ctx.store().await.unwrap();
    let id = store.next_id("t", "jira's issue", "").await.unwrap();
    store
        .create_issue(&Issue {
            source_system: "jira".into(),
            external_ref: Some("PROJ-9".into()),
            labels: vec![MARKER_LABEL.to_string()],
            ..Issue::new(&id, "jira's issue")
        })
        .await
        .unwrap();

    // FakeHttp errors on any unstubbed request, so a push that touched the
    // network here would fail rather than pass quietly.
    let http = FakeHttp::new();
    let report = GitHub.push(&ctx, &http).await.unwrap();

    assert_eq!(report.pushed, 0);
    assert!(http.requests().is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].contains("jira"), "{:?}", report.skipped);
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// `status` exists to be run *before* anything is set up. If it needs the setup
/// to work, it is useless in the only situation anyone runs it in.
#[tokio::test]
async fn status_names_the_missing_config_instead_of_exploding() {
    let dir = tempdir("status-bare");
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap(), "init", "--prefix", "t"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let cli =
        Cli::try_parse_from(["bd", "-C", dir.to_str().unwrap(), "github", "status"]).unwrap();
    let ctx = Ctx::build(&cli, Need::Workspace).await.unwrap();

    // Nothing configured: no repo key, and no token written anywhere (it never
    // is — it comes from the environment).
    let st = GitHub.status(&ctx).await.unwrap();
    assert!(!st.configured);
    assert!(
        st.missing.iter().any(|m| m == github::REPO_KEY),
        "status must name the key that is missing: {:?}",
        st.missing
    );
    // It must not claim to be unported, which is how `bd github pull` decides to
    // exit 64 rather than run.
    assert_ne!(st.detail.as_deref(), Some("not implemented yet"));
    assert_eq!(GitHub.required_config(), &[github::REPO_KEY]);
    assert_eq!(GitHub.secret_env(), "GITHUB_TOKEN");
    assert_eq!(GitHub.name(), "github");
}

/// The whole (repo, token) matrix, without touching the process environment —
/// which parallel tests cannot safely mutate.
#[test]
fn status_needs_both_the_repo_and_the_token() {
    let st = github::evaluate(Some(REPO), Some("ghp_x"));
    assert!(st.configured);
    assert!(st.missing.is_empty());

    let st = github::evaluate(None, Some("ghp_x"));
    assert!(!st.configured);
    assert_eq!(st.missing, vec![github::REPO_KEY.to_string()]);

    let st = github::evaluate(Some(REPO), None);
    assert!(!st.configured);
    assert_eq!(st.missing, vec!["$GITHUB_TOKEN".to_string()]);

    let st = github::evaluate(None, None);
    assert!(!st.configured);
    assert_eq!(st.missing.len(), 2);

    // Set, but unusable. "missing" alone would send someone hunting for a key
    // that is sitting right there.
    let st = github::evaluate(Some("https://github.com/octo/demo"), Some("ghp_x"));
    assert!(!st.configured);
    assert!(st.detail.unwrap().contains("owner/name"));
}
