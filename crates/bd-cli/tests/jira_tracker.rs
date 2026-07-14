//! The Jira tracker, against recorded fixtures and a real sqlite workspace.
//!
//! No network, no credentials, no Atlassian account. `FakeHttp` replays canned
//! responses and records every request, which is what lets these tests assert on
//! the two failures that look exactly like success from the outside:
//!
//! - a pull that reads page one and stops (the report says "synced 100 issues",
//!   and it is even true — it just isn't all of them);
//! - a pull that maps the ADF description to `None` and imports the whole
//!   backlog with an empty body.
//!
//! Neither raises an error, so neither can be caught by "does it fail?". They
//! are caught by looking at the requests made and at the text that landed.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use bd_cli::cli::Cli;
use bd_cli::context::{Ctx, Need};
use bd_cli::integrations::Tracker;
use bd_cli::integrations::http::{FakeHttp, Method};
use bd_cli::integrations::jira::Jira;
use bd_core::{Issue, IssueFilter, IssueType, Priority, Status};
use clap::Parser;
use serde_json::{Value, json};

const SITE: &str = "https://acme.atlassian.net";
const EMAIL: &str = "user@acme.com";
const TOKEN: &str = "tok";
/// `base64("user@acme.com:tok")` — Basic auth is over the **pair**.
const BASIC: &str = "Basic dXNlckBhY21lLmNvbTp0b2s=";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// `$JIRA_TOKEN` is process-global and cargo runs tests as threads in one
/// process, so a test that sets it races every test that reads it. One lock,
/// held across the whole test body — the runtime is built inside it, so nothing
/// here is ever holding it across an `.await`.
static ENV: Mutex<()> = Mutex::new(());

fn with_token<F: Future<Output = ()>>(token: Option<&str>, body: impl FnOnce() -> F) {
    // A panic in one test poisons the lock; that must fail *that* test, not
    // every test after it.
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the lock above is the only writer, and every reader in this binary
    // holds it too.
    unsafe {
        match token {
            Some(t) => std::env::set_var("JIRA_TOKEN", t),
            None => std::env::remove_var("JIRA_TOKEN"),
        }
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(body());
}

fn tempdir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "bd-jira-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// A real workspace, through the real binary — the tracker writes to a real
/// store, so a mapping that the schema rejects fails here rather than in prod.
fn workspace(tag: &str) -> PathBuf {
    let dir = tempdir(tag);
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap(), "init", "--prefix", "t"])
        .output()
        .expect("run bd init");
    assert!(
        out.status.success(),
        "bd init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

async fn ctx_for(dir: &Path) -> Ctx {
    let cli = Cli::parse_from(["bd", "-C", dir.to_str().unwrap(), "jira", "status"]);
    Ctx::build(&cli, Need::Workspace).await.expect("ctx")
}

/// The three config keys, in the store — where `bd config set` puts them, and
/// pointedly *not* the token.
async fn configured(dir: &Path) -> Ctx {
    let ctx = ctx_for(dir).await;
    let store = ctx.store().await.unwrap();
    store.set_config("jira.url", SITE).await.unwrap();
    store.set_config("jira.project", "PROJ").await.unwrap();
    store.set_config("jira.email", EMAIL).await.unwrap();
    ctx
}

fn search_url(start: u64) -> String {
    format!("{SITE}/rest/api/3/search?jql=project=PROJ&maxResults=100&startAt={start}")
}

fn page(start: u64, total: u64, issues: Vec<Value>) -> String {
    json!({
        "startAt": start,
        "maxResults": 100,
        "total": total,
        "issues": issues,
    })
    .to_string()
}

/// A Jira issue as v3 actually returns one: ADF description, a status whose
/// *name* is project-local, a priority by name.
fn fixture(key: &str, summary: &str, category: &str, status_name: &str) -> Value {
    json!({
        "id": "10001",
        "key": key,
        "fields": {
            "summary": summary,
            "description": adf(&format!("{summary} body")),
            "status": { "name": status_name, "statusCategory": { "key": category } },
            "priority": { "name": "Medium" },
            "issuetype": { "name": "Task" },
            "labels": [],
            "created": "2024-01-15T10:30:00.000+0000",
            "updated": "2024-01-16T09:00:00.000+0000",
        }
    })
}

fn adf(text: &str) -> Value {
    json!({
        "type": "doc",
        "version": 1,
        "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": text }] }]
    })
}

async fn all_issues(ctx: &Ctx) -> Vec<Issue> {
    ctx.store()
        .await
        .unwrap()
        .list_issues(&IssueFilter::default())
        .await
        .unwrap()
}

async fn by_ref(ctx: &Ctx, key: &str) -> Issue {
    let id = all_issues(ctx)
        .await
        .into_iter()
        .find(|i| i.external_ref.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no local issue carries external_ref {key}"))
        .id;
    // `get_issue` is the only reader that hydrates labels.
    ctx.store().await.unwrap().get_issue(&id).await.unwrap().unwrap()
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

#[test]
fn pull_maps_a_jira_issue_onto_a_bead() {
    with_token(Some(TOKEN), || async {
        let dir = workspace("map");
        let ctx = configured(&dir).await;

        let remote = json!({
            "id": "10001",
            "key": "PROJ-12",
            "fields": {
                "summary": "Fix the widget",
                "description": adf("the widget is broken"),
                // The *name* is project-local nonsense; the *category* is the
                // only stable signal. A tracker that matched on "In Review"
                // would map this to open on every project that spells its
                // in-flight status differently.
                "status": { "name": "In Review", "statusCategory": { "key": "indeterminate" } },
                "priority": { "name": "Highest" },
                "issuetype": { "name": "Bug" },
                "labels": ["urgent", "widget"],
                "assignee": { "displayName": "Ada", "emailAddress": "ada@acme.com" },
                "created": "2024-01-15T10:30:00.000+0000",
                "updated": "2024-01-16T09:00:00.000+0000",
            }
        });
        let http = FakeHttp::new().on(
            Method::Get,
            &search_url(0),
            200,
            &page(0, 1, vec![remote]),
        );

        let report = Jira.pull(&ctx, &http).await.expect("pull");
        assert_eq!((report.pulled, report.created, report.updated), (1, 1, 0));

        let issue = by_ref(&ctx, "PROJ-12").await;
        assert_eq!(issue.title, "Fix the widget");
        assert_eq!(issue.description, "the widget is broken");
        assert_eq!(issue.status, Status::InProgress, "statusCategory drives status");
        assert_eq!(issue.priority, Priority::CRITICAL, "Highest is P0");
        assert_eq!(issue.issue_type, IssueType::Bug);
        assert_eq!(issue.assignee, "ada@acme.com");
        assert_eq!(issue.labels, ["urgent", "widget"]);
        // The join key. Both halves, or the next pull duplicates the backlog.
        assert_eq!(issue.external_ref.as_deref(), Some("PROJ-12"));
        assert_eq!(issue.source_system, "jira");
        assert_eq!(issue.created_at.to_rfc3339(), "2024-01-15T10:30:00+00:00");

        ctx.close().await;
    });
}

/// **The bug every Jira integration ships with.**
///
/// `fields.description` is an ADF *document*. `as_str()` on it is `None`, and
/// the natural `.unwrap_or_default()` imports the entire backlog with an empty
/// body — no error, no warning, and nobody notices until someone opens a bead
/// and finds it blank.
#[test]
fn adf_description_is_extracted_as_text() {
    with_token(Some(TOKEN), || async {
        let dir = workspace("adf");
        let ctx = configured(&dir).await;

        let description = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "The parser drops the last row."},
                    {"type": "hardBreak"},
                    {"type": "text", "text": "Reproduced on main."}
                ]},
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "open the file"}]}
                    ]},
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "count the rows"}]}
                    ]}
                ]},
                {"type": "codeBlock", "attrs": {"language": "rust"},
                 "content": [{"type": "text", "text": "assert_eq!(rows.len(), 3);"}]}
            ]
        });
        let remote = json!({
            "key": "PROJ-1",
            "fields": {
                "summary": "Parser drops a row",
                "description": description,
                "status": { "name": "To Do", "statusCategory": { "key": "new" } },
                "priority": { "name": "Medium" },
                "issuetype": { "name": "Bug" },
            }
        });
        let http = FakeHttp::new().on(Method::Get, &search_url(0), 200, &page(0, 1, vec![remote]));

        Jira.pull(&ctx, &http).await.expect("pull");

        let body = by_ref(&ctx, "PROJ-1").await.description;
        assert!(
            !body.is_empty(),
            "the ADF description was thrown away — `description.as_str()` is None on a document"
        );
        assert!(body.contains("The parser drops the last row."), "{body:?}");
        assert!(body.contains("Reproduced on main."), "{body:?}");
        assert!(body.contains("- open the file"), "list items lost: {body:?}");
        assert!(body.contains("- count the rows"), "list items lost: {body:?}");
        assert!(body.contains("assert_eq!(rows.len(), 3);"), "code block lost: {body:?}");
        // The hard break is a line break, not a lost space.
        assert!(
            body.contains("The parser drops the last row.\nReproduced on main."),
            "{body:?}"
        );

        ctx.close().await;
    });
}

/// Pull the same fixture twice. The second run must find what the first one
/// wrote — by (`source_system`, `external_ref`) — and update it. If it creates,
/// every scheduled sync doubles the backlog.
#[test]
fn pulling_twice_creates_once_and_then_only_updates() {
    with_token(Some(TOKEN), || async {
        let dir = workspace("idem");
        let ctx = configured(&dir).await;

        let issues = vec![
            fixture("PROJ-1", "First", "new", "To Do"),
            fixture("PROJ-2", "Second", "done", "Shipped"),
        ];
        let http =
            FakeHttp::new().on(Method::Get, &search_url(0), 200, &page(0, 2, issues.clone()));

        let first = Jira.pull(&ctx, &http).await.expect("first pull");
        assert_eq!((first.created, first.updated), (2, 0));
        assert_eq!(all_issues(&ctx).await.len(), 2);

        let second = Jira.pull(&ctx, &http).await.expect("second pull");
        assert_eq!(
            (second.pulled, second.created, second.updated),
            (2, 0, 2),
            "a second pull re-created the issues instead of matching them on \
             (source_system, external_ref)"
        );
        assert_eq!(
            all_issues(&ctx).await.len(),
            2,
            "the backlog doubled: every sync would double it again"
        );

        // And the mapping still holds after an update, not just after a create.
        assert_eq!(by_ref(&ctx, "PROJ-2").await.status, Status::Closed);

        ctx.close().await;
    });
}

/// Two pages. A tracker that ignores `total` fetches the first, reports success,
/// and silently syncs a prefix of the project — so the assertion that matters is
/// on the *requests*, not on the report.
#[test]
fn pull_follows_pagination() {
    with_token(Some(TOKEN), || async {
        let dir = workspace("page");
        let ctx = configured(&dir).await;

        // Page one returns two of three: Jira caps the page size server-side, so
        // the cursor must advance by what came back, not by the 100 we asked for.
        let http = FakeHttp::new()
            .on(
                Method::Get,
                &search_url(0),
                200,
                &page(
                    0,
                    3,
                    vec![
                        fixture("PROJ-1", "One", "new", "To Do"),
                        fixture("PROJ-2", "Two", "new", "To Do"),
                    ],
                ),
            )
            .on(
                Method::Get,
                &search_url(2),
                200,
                &page(2, 3, vec![fixture("PROJ-3", "Three", "new", "To Do")]),
            );

        let report = Jira.pull(&ctx, &http).await.expect("pull");
        assert_eq!((report.pulled, report.created), (3, 3));

        let urls: Vec<String> = http.requests().iter().map(|r| r.url.clone()).collect();
        assert_eq!(
            urls,
            vec![search_url(0), search_url(2)],
            "the second page was never requested: the sync silently stopped at page one"
        );
        assert!(by_ref(&ctx, "PROJ-3").await.title == "Three");

        ctx.close().await;
    });
}

/// Bearer is the reflex, and it is wrong: Jira Cloud wants Basic over
/// `email:api_token`. The 401 it answers a bearer token with says nothing about
/// the scheme, which is why this is asserted rather than assumed.
#[test]
fn auth_is_basic_over_email_and_token_not_bearer() {
    with_token(Some(TOKEN), || async {
        let dir = workspace("auth");
        let ctx = configured(&dir).await;

        let http = FakeHttp::new().on(Method::Get, &search_url(0), 200, &page(0, 0, vec![]));
        Jira.pull(&ctx, &http).await.expect("pull");

        let reqs = http.requests();
        let auth = reqs[0]
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.clone())
            .expect("no Authorization header at all");
        assert_eq!(auth, BASIC, "must be base64(email:token), not the raw token");
        assert!(!auth.starts_with("Bearer"), "a bearer token gets a confusing 401");

        ctx.close().await;
    });
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

#[test]
fn push_creates_unlinked_beads_and_updates_linked_ones() {
    with_token(Some(TOKEN), || async {
        let dir = workspace("push");
        let ctx = configured(&dir).await;
        let store = ctx.store().await.unwrap();

        // One bead beads owns, one bead Jira already knows about.
        let mut fresh = Issue::new("t-new", "Ship the thing");
        fresh.description = "line one\nline two".to_string();
        fresh.issue_type = IssueType::Bug;
        fresh.priority = Priority::HIGH;
        store.create_issue(&fresh).await.unwrap();

        let mut linked = Issue::new("t-old", "Already filed");
        linked.external_ref = Some("PROJ-7".into());
        linked.source_system = "jira".into();
        linked.status = Status::Closed;
        store.create_issue(&linked).await.unwrap();

        let http = FakeHttp::new()
            .on(
                Method::Post,
                &format!("{SITE}/rest/api/3/issue"),
                201,
                r#"{"id":"10009","key":"PROJ-9","self":"https://acme.atlassian.net/rest/api/3/issue/10009"}"#,
            )
            // A Jira PUT answers 204 with an empty body: parsing it as JSON is a
            // failure the tracker must not have.
            .on(Method::Put, &format!("{SITE}/rest/api/3/issue/PROJ-7"), 204, "");

        let report = Jira.push(&ctx, &http).await.expect("push");
        assert_eq!(report.pushed, 2);

        let reqs = http.requests();
        let post = reqs.iter().find(|r| r.method == Method::Post).expect("no POST");
        let body: Value = serde_json::from_str(post.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["fields"]["project"]["key"], "PROJ");
        assert_eq!(body["fields"]["summary"], "Ship the thing");
        assert_eq!(body["fields"]["issuetype"]["name"], "Bug");
        assert_eq!(body["fields"]["priority"]["name"], "High");
        assert!(
            body["fields"]["description"].is_object(),
            "v3 rejects a plain string description: it must be an ADF document"
        );
        assert_eq!(body["fields"]["description"]["type"], "doc");
        assert_eq!(
            body["fields"]["description"]["content"][1]["content"][0]["text"],
            "line two",
            "the second line was dropped on the way into ADF"
        );

        let put = reqs.iter().find(|r| r.method == Method::Put).expect("no PUT");
        let body: Value = serde_json::from_str(put.body.as_deref().unwrap()).unwrap();
        assert!(body["fields"]["project"].is_null(), "an edit must not set the project");
        assert!(
            body["fields"]["status"].is_null(),
            "Jira moves status through the transitions API; PUTting it is a 400"
        );
        // And it says so instead of pretending the close was synced.
        assert!(
            report.skipped.iter().any(|s| s.contains("PROJ-7") && s.contains("transitions")),
            "the local close was silently dropped: {:?}",
            report.skipped
        );

        // The key comes back, or the next pull files PROJ-9 as a brand-new bead.
        let created = store.get_issue("t-new").await.unwrap().unwrap();
        assert_eq!(created.external_ref.as_deref(), Some("PROJ-9"));

        ctx.close().await;
    });
}

/// The other half of the identity rule: what push wrote back must be what the
/// next pull matches on. (`source_system` cannot be patched — see the note on
/// `linked_key` — so this is the case that would otherwise duplicate.)
#[test]
fn a_bead_this_workspace_pushed_is_updated_by_the_next_pull_not_duplicated() {
    with_token(Some(TOKEN), || async {
        let dir = workspace("roundtrip");
        let ctx = configured(&dir).await;
        let store = ctx.store().await.unwrap();
        store
            .create_issue(&Issue::new("t-1", "Ship the thing"))
            .await
            .unwrap();

        let http = FakeHttp::new()
            .on(
                Method::Post,
                &format!("{SITE}/rest/api/3/issue"),
                201,
                r#"{"id":"10009","key":"PROJ-9"}"#,
            )
            .on(
                Method::Get,
                &search_url(0),
                200,
                &page(0, 1, vec![fixture("PROJ-9", "Ship the thing", "new", "To Do")]),
            );

        assert_eq!(Jira.push(&ctx, &http).await.expect("push").pushed, 1);
        let report = Jira.pull(&ctx, &http).await.expect("pull");
        assert_eq!(
            (report.created, report.updated),
            (0, 1),
            "the issue this workspace just pushed came back as a duplicate"
        );
        assert_eq!(all_issues(&ctx).await.len(), 1);

        ctx.close().await;
    });
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// `bd jira status` exists to be run when nothing works. It must answer, not
/// throw — and it must name what is missing rather than say "not configured".
#[test]
fn status_names_every_missing_key_and_never_explodes() {
    with_token(None, || async {
        let dir = workspace("status");
        let ctx = ctx_for(&dir).await;

        let st = Jira.status(&ctx).await.expect("status must not fail unconfigured");
        assert_eq!(st.name, "jira");
        assert!(!st.configured);
        assert_eq!(
            st.missing,
            ["jira.url", "jira.project", "jira.email", "$JIRA_TOKEN"],
            "status has to say which key is missing, including the token"
        );
        // The sentinel `commands::sync::tracker` reads as "this is still a stub".
        assert_ne!(st.detail.as_deref(), Some("not implemented yet"));

        // Half-configured is still not configured, and still says which half.
        let store = ctx.store().await.unwrap();
        store.set_config("jira.url", SITE).await.unwrap();
        let st = Jira.status(&ctx).await.unwrap();
        assert!(!st.configured);
        assert_eq!(st.missing, ["jira.project", "jira.email", "$JIRA_TOKEN"]);

        // Pull refuses, and the refusal names the keys rather than a 401.
        let err = Jira
            .pull(&ctx, &FakeHttp::new())
            .await
            .expect_err("an unconfigured pull must not reach the network")
            .to_string();
        assert!(err.contains("jira.project"), "{err}");
        assert!(err.contains("JIRA_TOKEN"), "{err}");

        ctx.close().await;
    });
}

#[test]
fn status_is_configured_once_the_keys_and_the_token_are_there() {
    with_token(Some(TOKEN), || async {
        let dir = workspace("status-ok");
        let ctx = configured(&dir).await;

        let st = Jira.status(&ctx).await.unwrap();
        assert!(st.configured, "missing: {:?}", st.missing);
        assert!(st.missing.is_empty());
        let detail = st.detail.unwrap_or_default();
        assert!(detail.contains("PROJ") && detail.contains(SITE), "{detail}");
        // The token is a secret. It has no business in a status line.
        assert!(!detail.contains(TOKEN), "status leaked the token: {detail}");

        ctx.close().await;
    });
}

/// The token comes from the environment, never from `.beads/config.yaml` — that
/// file is committed, and a token in it is a token on GitHub.
#[test]
fn a_token_in_the_workspace_config_is_not_a_token() {
    with_token(None, || async {
        let dir = workspace("no-config-token");
        let ctx = configured(&dir).await;
        ctx.store()
            .await
            .unwrap()
            .set_config("jira.token", "hunter2")
            .await
            .unwrap();

        let st = Jira.status(&ctx).await.unwrap();
        assert!(
            !st.configured && st.missing == ["$JIRA_TOKEN"],
            "the tracker read a token out of the workspace config: {st:?}"
        );

        ctx.close().await;
    });
}
