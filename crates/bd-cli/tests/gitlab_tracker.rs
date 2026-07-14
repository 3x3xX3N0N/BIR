//! The GitLab tracker, against recorded fixtures and a real database.
//!
//! No network is touched here and none can be: the tracker is handed a
//! [`FakeHttp`], which errors loudly on any URL it was not told about. So a
//! tracker that asks for the wrong URL fails the test rather than quietly
//! 404ing in someone's repo.
//!
//! The two properties worth the most here are the ones that fail *silently* in
//! production:
//!
//! 1. **A second pull must update, not duplicate.** Get the join key wrong and
//!    every sync appends the entire backlog again, reporting success each time.
//! 2. **Paging must actually page.** A tracker that fetches page one and stops
//!    looks identical from the outside to one that fetched everything. The only
//!    place the difference shows up is in the requests it made — so that is what
//!    is asserted.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use bd_cli::cli::Cli;
use bd_cli::context::{Ctx, Need};
use bd_cli::integrations::Tracker;
use bd_cli::integrations::gitlab::GitLab;
use bd_cli::integrations::http::{FakeHttp, Method};
use bd_core::IssueFilter;
use clap::Parser;
use serde_json::{Value, json};

const HOST: &str = "https://gitlab.example.com";

/// The token lives in the environment and nowhere else, so a test that needs a
/// configured tracker has to put it there. Written exactly once, before any test
/// reads it, because the test binary runs its tests on several threads.
fn with_token() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe { std::env::set_var("GITLAB_TOKEN", "glpat-testing") });
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-gitlab-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// A workspace with a real sqlite database behind it.
fn workspace(tag: &str) -> PathBuf {
    let dir = tempdir(tag);
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap(), "init", "--prefix", "gl"])
        .env("BEADS_ACTOR", "tester")
        .output()
        .expect("run bd init");
    assert!(out.status.success(), "bd init failed");
    dir
}

async fn ctx_for(dir: &Path) -> Ctx {
    // Any workspace command will do — `Ctx` is built from the global flags, and
    // the subcommand only decides whether a workspace is required.
    let cli = Cli::parse_from(["bd", "-C", dir.to_str().unwrap(), "list"]);
    Ctx::build(&cli, Need::Workspace).await.expect("build ctx")
}

/// Point the tracker at a project. The token never comes from here — see
/// [`with_token`].
async fn configure(ctx: &Ctx, project: &str) {
    let store = ctx.store().await.unwrap();
    store.set_config("gitlab.url", HOST).await.unwrap();
    store.set_config("gitlab.project", project).await.unwrap();
}

fn issues_url(project_encoded: &str, page: u32) -> String {
    format!("{HOST}/api/v4/projects/{project_encoded}/issues?per_page=100&page={page}")
}

/// One issue as GitLab's REST v4 renders it — note that `id` and `iid` differ,
/// which is the whole point: a tracker that joins on `id` passes a fixture where
/// they are equal and fails against the real thing.
fn gl_issue(id: i64, iid: i64, title: &str, state: &str, labels: &[&str]) -> Value {
    json!({
        "id": id,
        "iid": iid,
        "project_id": 77,
        "title": title,
        "description": format!("body of {title}"),
        "state": state,
        "labels": labels,
        "web_url": format!("{HOST}/group/project/-/issues/{iid}"),
        "created_at": "2026-01-02T03:04:05.000Z",
        "updated_at": "2026-01-03T03:04:05.000Z",
        // GitLab really does send `weight` and `assignees`; both are ignored on
        // purpose, and an unknown field must not break deserialization.
        "weight": 5,
        "assignees": [{ "username": "someone" }],
    })
}

/// One bead's labels, sorted. `list_issues` does not hydrate relations, so they
/// have to be asked for separately — which is also how the tracker reads them.
async fn labels_of(ctx: &Ctx, id: &str) -> Vec<String> {
    let ids = [id.to_string()];
    let mut ls = ctx
        .store()
        .await
        .unwrap()
        .labels_of(&ids)
        .await
        .unwrap()
        .pop()
        .map(|(_, l)| l)
        .unwrap_or_default();
    ls.sort();
    ls
}

async fn all_issues(ctx: &Ctx) -> Vec<bd_core::Issue> {
    ctx.store()
        .await
        .unwrap()
        .list_issues(&IssueFilter::default())
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn pull_maps_a_gitlab_issue_onto_a_bead() {
    with_token();
    let dir = workspace("map");
    let ctx = ctx_for(&dir).await;
    configure(&ctx, "77").await;

    let page1 = json!([
        gl_issue(9001, 7, "Fix the paging", "opened", &["bug", "backend"]),
        gl_issue(9002, 8, "Ship the thing", "closed", &[]),
    ]);
    let http = FakeHttp::new().on(Method::Get, &issues_url("77", 1), 200, &page1.to_string());

    let report = GitLab.pull(&ctx, &http).await.expect("pull");
    assert_eq!((report.pulled, report.created, report.updated), (2, 2, 0));

    let issues = all_issues(&ctx).await;
    assert_eq!(issues.len(), 2);

    let seven = issues
        .iter()
        .find(|i| i.external_ref.as_deref() == Some("7"))
        .expect("the iid, not the global id, is the external_ref");
    assert_eq!(seven.title, "Fix the paging");
    assert_eq!(seven.description, "body of Fix the paging");
    assert_eq!(seven.status, bd_core::Status::Open);
    assert_eq!(seven.source_system, "gitlab");
    // GitLab has no priority. The bead keeps the beads default rather than one
    // invented from a weight or a scoped label.
    assert_eq!(seven.priority, bd_core::Priority::NORMAL);

    assert_eq!(
        labels_of(&ctx, &seven.id).await,
        vec!["backend".to_string(), "bug".to_string()]
    );

    let eight = issues
        .iter()
        .find(|i| i.external_ref.as_deref() == Some("8"))
        .expect("iid 8");
    assert_eq!(eight.status, bd_core::Status::Closed);

    // The global id is kept, but nothing joins on it.
    assert_eq!(seven.metadata.as_ref().unwrap()["gitlab"]["id"], json!(9001));

    // The credential goes in PRIVATE-TOKEN. A bearer token is not an error —
    // GitLab simply does not see it, and answers 401 as if the token expired.
    let req = &http.requests()[0];
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "PRIVATE-TOKEN" && v == "glpat-testing"),
        "expected a PRIVATE-TOKEN header, got {:?}",
        req.headers
    );
    assert!(
        !req.headers.iter().any(|(k, _)| k == "Authorization"),
        "a PAT must not be sent as a bearer token"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The one that matters. If the (external_ref, source_system) join is wrong, the
/// second pull creates two more beads and says "2 pulled, 2 created" — a
/// perfectly successful-looking backlog duplication.
#[tokio::test]
async fn pulling_twice_updates_rather_than_duplicating() {
    with_token();
    let dir = workspace("idempotent");
    let ctx = ctx_for(&dir).await;
    configure(&ctx, "77").await;

    let body = json!([
        gl_issue(9001, 7, "Fix the paging", "opened", &["bug"]),
        gl_issue(9002, 8, "Ship the thing", "opened", &[]),
    ])
    .to_string();

    let first = FakeHttp::new().on(Method::Get, &issues_url("77", 1), 200, &body);
    let r1 = GitLab.pull(&ctx, &first).await.expect("first pull");
    assert_eq!((r1.created, r1.updated), (2, 0));

    // Same fixture, and one issue has since been retitled, closed, and relabelled
    // upstream — so the second pull must *change* the bead, not clone it.
    let changed = json!([
        gl_issue(9001, 7, "Fix the paging properly", "closed", &["bug", "done"]),
        gl_issue(9002, 8, "Ship the thing", "opened", &[]),
    ])
    .to_string();
    let second = FakeHttp::new().on(Method::Get, &issues_url("77", 1), 200, &changed);
    let r2 = GitLab.pull(&ctx, &second).await.expect("second pull");
    assert_eq!(
        (r2.pulled, r2.created, r2.updated),
        (2, 0, 2),
        "a second pull of the same issues must update, never create"
    );

    let issues = all_issues(&ctx).await;
    assert_eq!(issues.len(), 2, "the backlog was duplicated");

    let seven = issues
        .iter()
        .find(|i| i.external_ref.as_deref() == Some("7"))
        .unwrap();
    assert_eq!(seven.title, "Fix the paging properly");
    assert_eq!(seven.status, bd_core::Status::Closed);

    assert_eq!(
        labels_of(&ctx, &seven.id).await,
        vec!["bug".to_string(), "done".to_string()],
        "labels must be reconciled with the remote, not appended to"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A local `in_progress` bead must survive a pull whose remote says `opened`.
/// GitLab has no in-progress state, so the naive mapping (`opened` → Open) would
/// un-claim every bead an agent is working on, on every sync.
#[tokio::test]
async fn an_open_remote_does_not_reset_local_progress() {
    with_token();
    let dir = workspace("wip");
    let ctx = ctx_for(&dir).await;
    configure(&ctx, "77").await;

    let body = json!([gl_issue(9001, 7, "Fix the paging", "opened", &[])]).to_string();
    let http = FakeHttp::new().on(Method::Get, &issues_url("77", 1), 200, &body);
    GitLab.pull(&ctx, &http).await.expect("pull");

    let store = ctx.store().await.unwrap();
    let id = all_issues(&ctx).await[0].id.clone();
    store
        .update_issue(
            &id,
            &bd_storage::IssuePatch {
                status: Some(bd_core::Status::InProgress),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let http = FakeHttp::new().on(Method::Get, &issues_url("77", 1), 200, &body);
    GitLab.pull(&ctx, &http).await.expect("second pull");

    let after = store.get_issue(&id).await.unwrap().unwrap();
    assert_eq!(
        after.status,
        bd_core::Status::InProgress,
        "an `opened` remote must not knock a claimed bead back to open"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// `group/project` has to reach GitLab as `group%2Fproject`. Unencoded, the
/// router never matches the route and answers 404 — which reads exactly like
/// "your token cannot see this project".
#[tokio::test]
async fn a_project_path_is_url_encoded() {
    with_token();
    let dir = workspace("encode");
    let ctx = ctx_for(&dir).await;
    configure(&ctx, "group/sub/project").await;

    let http = FakeHttp::new().on(
        Method::Get,
        &issues_url("group%2Fsub%2Fproject", 1),
        200,
        "[]",
    );
    // FakeHttp refuses any URL it was not given, so reaching this line at all is
    // half the assertion.
    GitLab.pull(&ctx, &http).await.expect("pull");

    let urls: Vec<String> = http.requests().iter().map(|r| r.url.clone()).collect();
    assert_eq!(urls.len(), 1);
    assert!(
        urls[0].contains("/projects/group%2Fsub%2Fproject/issues"),
        "project path was not encoded: {}",
        urls[0]
    );
    assert!(
        !urls[0].contains("/projects/group/"),
        "a raw slash in the project path 404s: {}",
        urls[0]
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Page one is a full page, so there must be a request for page two. A tracker
/// that stops after the first page reports a successful sync of a third of the
/// backlog, and nothing anywhere says so.
#[tokio::test]
async fn pagination_is_followed_past_the_first_page() {
    with_token();
    let dir = workspace("paging");
    let ctx = ctx_for(&dir).await;
    configure(&ctx, "77").await;

    let page1: Vec<Value> = (1..=100)
        .map(|i| gl_issue(9000 + i, i, &format!("issue {i}"), "opened", &[]))
        .collect();
    let page2: Vec<Value> = (101..=102)
        .map(|i| gl_issue(9000 + i, i, &format!("issue {i}"), "opened", &[]))
        .collect();

    let http = FakeHttp::new()
        .on(
            Method::Get,
            &issues_url("77", 1),
            200,
            &json!(page1).to_string(),
        )
        .on(
            Method::Get,
            &issues_url("77", 2),
            200,
            &json!(page2).to_string(),
        );

    let report = GitLab.pull(&ctx, &http).await.expect("pull");
    assert_eq!(report.pulled, 102, "the second page was dropped");
    assert_eq!(report.created, 102);

    let urls: Vec<String> = http.requests().iter().map(|r| r.url.clone()).collect();
    assert!(
        urls.iter().any(|u| u.ends_with("page=2")),
        "page 2 was never requested: {urls:?}"
    );
    // The short page ends it: page 3 must not be asked for. (FakeHttp would error
    // on it anyway, which is why the pull would have failed — but assert the
    // intent, not the accident.)
    assert_eq!(urls.len(), 2, "expected exactly two pages: {urls:?}");

    std::fs::remove_dir_all(&dir).ok();
}

/// Push: an unlinked bead is POSTed and then stamped with the iid GitLab minted,
/// and a bead we already own is PUT back to its iid. The stamp is what stops the
/// next pull from creating a second copy of the issue push just created.
#[tokio::test]
async fn push_creates_then_links_and_updates_what_it_owns() {
    with_token();
    let dir = workspace("push");
    let ctx = ctx_for(&dir).await;
    configure(&ctx, "77").await;

    // One bead pulled from GitLab (so: ours, iid 7)…
    let seven = gl_issue(9001, 7, "Fix the paging", "opened", &["bug"]);
    let body = json!([seven]).to_string();
    let pull_http = FakeHttp::new().on(Method::Get, &issues_url("77", 1), 200, &body);
    GitLab.pull(&ctx, &pull_http).await.expect("pull");

    // …and one authored locally, which GitLab has never seen.
    let store = ctx.store().await.unwrap();
    let id = store.next_id("gl", "Write the pusher", "").await.unwrap();
    store
        .create_issue(&bd_core::Issue::new(&id, "Write the pusher"))
        .await
        .unwrap();

    let created = gl_issue(9500, 42, "Write the pusher", "opened", &[]);
    let http = FakeHttp::new()
        .on(
            Method::Post,
            &format!("{HOST}/api/v4/projects/77/issues"),
            201,
            &created.to_string(),
        )
        .on(
            Method::Put,
            &format!("{HOST}/api/v4/projects/77/issues/7"),
            200,
            &seven.to_string(),
        );

    let report = GitLab.push(&ctx, &http).await.expect("push");
    assert_eq!(report.pushed, 2);

    // The local bead now carries both halves of the join key: the iid it was
    // given, and the system that gave it. Without the second, the next pull does
    // not recognize the issue push just created and files a duplicate.
    let pushed = store.get_issue(&id).await.unwrap().unwrap();
    assert_eq!(pushed.external_ref.as_deref(), Some("42"));
    assert_eq!(pushed.source_system, "gitlab");
    // The metadata is not the key — it carries the *global* id and the web_url,
    // which nothing joins on and which would otherwise be lost.
    assert_eq!(pushed.metadata.as_ref().unwrap()["gitlab"]["iid"], json!(42));

    // And that is enough to make the round trip idempotent: pulling the issue we
    // just created updates it rather than cloning it.
    let after = json!([
        gl_issue(9001, 7, "Fix the paging", "opened", &["bug"]),
        gl_issue(9500, 42, "Write the pusher", "opened", &[]),
    ])
    .to_string();
    let http = FakeHttp::new().on(Method::Get, &issues_url("77", 1), 200, &after);
    let report = GitLab.pull(&ctx, &http).await.expect("pull back");
    assert_eq!(
        (report.created, report.updated),
        (0, 2),
        "push-then-pull duplicated the bead push had just created"
    );
    assert_eq!(all_issues(&ctx).await.len(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

/// `status` is the one verb that has to work when nothing is set up — it exists
/// to say what is missing. Both inside an unconfigured workspace and outside any
/// workspace at all, where there is no database to ask.
#[tokio::test]
async fn status_reports_what_is_missing_rather_than_exploding() {
    with_token();
    let dir = workspace("status");
    let ctx = ctx_for(&dir).await;

    let st = GitLab.status(&ctx).await.expect("status must answer");
    assert!(!st.configured);
    assert!(
        st.missing.iter().any(|m| m == "gitlab.project"),
        "should name the key the user has to set: {:?}",
        st.missing
    );
    // Not the string the dispatcher reads as "this tracker is a stub" (exit 64):
    // an unconfigured tracker is a configuration problem, not an unported one.
    assert_ne!(st.detail.as_deref(), Some("not implemented yet"));

    // Configured, once the project is set (the URL defaults to gitlab.com).
    configure(&ctx, "group/project").await;
    let st = GitLab.status(&ctx).await.unwrap();
    assert!(st.configured, "missing: {:?}", st.missing);
    assert!(st.missing.is_empty());

    // And with no workspace at all: `Ctx::store()` fails, and the tracker still
    // has to answer.
    let bare = tempdir("status-nowhere");
    let cli = Cli::parse_from(["bd", "-C", bare.to_str().unwrap(), "version"]);
    let ctx = Ctx::build(&cli, Need::Nothing).await.expect("ctx");
    let st = GitLab
        .status(&ctx)
        .await
        .expect("status outside a workspace must not error");
    assert!(!st.configured);
    assert!(st.missing.iter().any(|m| m == "gitlab.project"));

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&bare).ok();
}

/// Names are contracts here: `source_system` is written from `name()`, and a pull
/// looks issues up by it. If they ever drift apart, every pull duplicates.
#[tokio::test]
async fn the_tracker_names_itself_consistently() {
    assert_eq!(GitLab.name(), "gitlab");
    assert_eq!(GitLab.secret_env(), "GITLAB_TOKEN");
    assert!(GitLab.required_config().contains(&"gitlab.project"));
}
