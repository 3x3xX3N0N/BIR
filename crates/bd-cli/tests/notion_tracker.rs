//! The Notion tracker, against recorded fixtures. No network, no credentials.
//!
//! Notion is the awkward one, and every test here is aimed at a way it can look
//! successful while being wrong:
//!
//! * the property names are the *user's*, so a mapping that only works on the
//!   defaults works on nobody's real database;
//! * a title is an array of rich-text runs, so reading `[0]` truncates every
//!   title anyone formatted, and never says so;
//! * `Notion-Version` is mandatory and its absence produces an error that does
//!   not mention it;
//! * a pull that cannot re-find what it already imported duplicates the backlog
//!   on every run;
//! * a pull that ignores the cursor syncs page one and reports success.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use bd_cli::cli::Cli;
use bd_cli::context::{Ctx, Need};
use bd_cli::integrations::http::{FakeHttp, Http, HttpRequest, HttpResponse, Method};
use bd_cli::integrations::notion::Notion;
use bd_cli::integrations::Tracker;
use bd_core::{IssueFilter, Issue, Priority, Status};
use clap::Parser;

const DB: &str = "db-1234";
const QUERY_URL: &str = "https://api.notion.com/v1/databases/db-1234/query";
const SCHEMA_URL: &str = "https://api.notion.com/v1/databases/db-1234";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Every test sets the same token, and none ever unsets it: cargo runs the tests
/// of one binary as threads of one process, so a test that cleared the
/// environment would be clearing it out from under whichever test happened to be
/// mid-`pull`. Identical writes race harmlessly; a remove would not.
fn set_token() {
    // SAFETY: see above — all tests write the same value and none remove it.
    unsafe { std::env::set_var("NOTION_TOKEN", "secret-token") };
}

fn tempdir(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("bd-notion-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A real workspace with a real sqlite store behind it — the tracker writes
/// through the same seam every command does, so an idempotency claim here is a
/// claim about the database, not about a mock.
async fn workspace(tag: &str) -> (PathBuf, Ctx) {
    set_token();
    let dir = tempdir(tag);
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap(), "init", "--prefix", "nt"])
        .output()
        .expect("run bd init");
    assert!(
        out.status.success(),
        "bd init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ctx = ctx_for(&dir).await;
    set_cfg(&ctx, "notion.database_id", DB).await;
    (dir, ctx)
}

async fn ctx_for(dir: &Path) -> Ctx {
    let cli = Cli::parse_from(["bd", "-C", dir.to_str().unwrap(), "--actor", "tester", "list"]);
    Ctx::build(&cli, Need::Workspace).await.expect("build ctx")
}

async fn set_cfg(ctx: &Ctx, key: &str, value: &str) {
    ctx.store()
        .await
        .unwrap()
        .set_config(key, value)
        .await
        .unwrap();
}

async fn all_issues(ctx: &Ctx) -> Vec<Issue> {
    ctx.store()
        .await
        .unwrap()
        .list_issues(&IssueFilter::default())
        .await
        .unwrap()
}

/// Hydrated (labels included), which `list_issues` deliberately is not.
async fn issue(ctx: &Ctx, id: &str) -> Issue {
    ctx.store()
        .await
        .unwrap()
        .get_issue(id)
        .await
        .unwrap()
        .expect("issue exists")
}

fn only(issues: &[Issue]) -> &Issue {
    assert_eq!(issues.len(), 1, "expected exactly one issue, got {issues:?}");
    &issues[0]
}

/// Responses queued *per key*, popped in order.
///
/// `FakeHttp` keys one response per `"METHOD url"`, which cannot express Notion's
/// pagination: the cursor travels in the POST **body**, so page one and page two
/// are the same method and the same URL. Replaying the first response forever
/// would either hang the tracker or hide the bug the test is hunting. So the
/// pagination test uses this; everything else uses `FakeHttp`.
#[derive(Default)]
struct SeqHttp {
    queued: Mutex<HashMap<String, VecDeque<HttpResponse>>>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl SeqHttp {
    fn on(self, method: Method, url: &str, status: u16, body: &str) -> Self {
        self.queued
            .lock()
            .unwrap()
            .entry(format!("{} {url}", method.as_str()))
            .or_default()
            .push_back(HttpResponse {
                status,
                body: body.to_string(),
            });
        self
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Http for SeqHttp {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        let key = format!("{} {}", req.method.as_str(), req.url);
        self.seen.lock().unwrap().push(req);
        self.queued
            .lock()
            .unwrap()
            .get_mut(&key)
            .and_then(|q| q.pop_front())
            .ok_or_else(|| anyhow::anyhow!("SeqHttp: no queued response left for `{key}`"))
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A page using the columns Notion itself creates: `Name` and `Tags`.
///
/// The title is deliberately split into three runs, which is what Notion sends
/// for "Fix **the** parser".
const DEFAULT_PROPS_PAGE: &str = r#"{
  "results": [
    {
      "id": "page-aaa",
      "created_time": "2026-01-02T03:04:05.000Z",
      "last_edited_time": "2026-02-03T04:05:06.000Z",
      "properties": {
        "Name": {
          "type": "title",
          "title": [
            { "plain_text": "Fix " },
            { "plain_text": "the" },
            { "plain_text": " parser" }
          ]
        },
        "Status": { "type": "status", "status": { "name": "In progress" } },
        "Priority": { "type": "select", "select": { "name": "P1" } },
        "Tags": {
          "type": "multi_select",
          "multi_select": [{ "name": "infra" }, { "name": "urgent" }]
        },
        "Description": {
          "type": "rich_text",
          "rich_text": [{ "plain_text": "the tokenizer eats the last byte" }]
        },
        "Assignee": {
          "type": "people",
          "people": [{ "name": "alice", "person": { "email": "alice@example.com" } }]
        }
      }
    }
  ],
  "has_more": false,
  "next_cursor": null
}"#;

/// The same page in a database somebody actually designed: different column
/// names, and a `select` where the default database has a `status`.
const CUSTOM_PROPS_PAGE: &str = r#"{
  "results": [
    {
      "id": "page-bbb",
      "properties": {
        "Task": { "type": "title", "title": [{ "plain_text": "Ship the release" }] },
        "State": { "type": "select", "select": { "name": "Done" } },
        "Urgency": { "type": "number", "number": 0 },
        "Topics": { "type": "multi_select", "multi_select": [{ "name": "release" }] }
      }
    }
  ],
  "has_more": false,
  "next_cursor": null
}"#;

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pull_maps_a_page_using_the_default_property_names() {
    let (_d, ctx) = workspace("defaults").await;
    let http = FakeHttp::new().on(Method::Post, QUERY_URL, 200, DEFAULT_PROPS_PAGE);

    let report = Notion.pull(&ctx, &http).await.unwrap();
    assert_eq!((report.pulled, report.created, report.updated), (1, 1, 0));

    let issues = all_issues(&ctx).await;
    let i = issue(&ctx, &only(&issues).id).await;

    // The three runs, concatenated. `[0]` would give "Fix ".
    assert_eq!(i.title, "Fix the parser");
    assert_eq!(i.status, Status::InProgress);
    assert_eq!(i.priority, Priority::HIGH);
    assert_eq!(i.description, "the tokenizer eats the last byte");
    assert_eq!(i.assignee, "alice");
    assert_eq!(i.labels, vec!["infra", "urgent"]);

    // The join key, both halves. Without them the next pull duplicates this.
    assert_eq!(i.external_ref.as_deref(), Some("page-aaa"));
    assert_eq!(i.source_system, "notion");
}

#[tokio::test]
async fn pull_maps_a_page_using_property_names_from_config() {
    let (_d, ctx) = workspace("custom").await;
    set_cfg(&ctx, "notion.prop.title", "Task").await;
    set_cfg(&ctx, "notion.prop.status", "State").await;
    set_cfg(&ctx, "notion.prop.priority", "Urgency").await;
    set_cfg(&ctx, "notion.prop.labels", "Topics").await;

    let http = FakeHttp::new().on(Method::Post, QUERY_URL, 200, CUSTOM_PROPS_PAGE);
    let report = Notion.pull(&ctx, &http).await.unwrap();
    assert_eq!((report.pulled, report.created), (1, 1));

    let issues = all_issues(&ctx).await;
    let i = issue(&ctx, &only(&issues).id).await;
    assert_eq!(i.title, "Ship the release");
    // A `select` named "Done" is a status just as much as a `status` is.
    assert_eq!(i.status, Status::Closed);
    // Urgency is a *number* column here, not a select.
    assert_eq!(i.priority, Priority::CRITICAL);
    assert_eq!(i.labels, vec!["release"]);
    assert_eq!(i.external_ref.as_deref(), Some("page-bbb"));

    // The columns this database does not have are reported once, not guessed at
    // and not silently dropped — a typo'd `notion.prop.*` key looks exactly like
    // a missing column, and this is the only thing that tells the two apart.
    let notes = report.skipped.join("\n");
    assert!(
        notes.contains("notion.prop.description"),
        "the absent Description column should be reported: {notes}"
    );
}

/// The header that, when forgotten, produces an error message that never
/// mentions it.
#[tokio::test]
async fn every_request_carries_the_token_and_the_notion_version() {
    let (_d, ctx) = workspace("headers").await;
    let http = FakeHttp::new().on(Method::Post, QUERY_URL, 200, DEFAULT_PROPS_PAGE);
    Notion.pull(&ctx, &http).await.unwrap();

    let reqs = http.requests();
    assert_eq!(reqs.len(), 1);
    let headers: HashMap<&str, &str> = reqs[0]
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    assert_eq!(headers.get("Notion-Version"), Some(&"2022-06-28"));
    assert_eq!(headers.get("Authorization"), Some(&"Bearer secret-token"));
    assert_eq!(headers.get("Content-Type"), Some(&"application/json"));
}

/// The property that decides whether every sync is a sync or a duplication.
#[tokio::test]
async fn pulling_the_same_fixture_twice_updates_and_never_duplicates() {
    let (_d, ctx) = workspace("idempotent").await;

    let first = Notion
        .pull(
            &ctx,
            &FakeHttp::new().on(Method::Post, QUERY_URL, 200, DEFAULT_PROPS_PAGE),
        )
        .await
        .unwrap();
    assert_eq!((first.created, first.updated), (1, 0));

    let second = Notion
        .pull(
            &ctx,
            &FakeHttp::new().on(Method::Post, QUERY_URL, 200, DEFAULT_PROPS_PAGE),
        )
        .await
        .unwrap();
    assert_eq!(
        (second.created, second.updated),
        (0, 1),
        "the second pull must find the page by (external_ref, source_system) and update it"
    );

    let issues = all_issues(&ctx).await;
    assert_eq!(issues.len(), 1, "the backlog was duplicated: {issues:?}");
    // And the update path did not re-add the labels it already had.
    assert_eq!(issue(&ctx, &issues[0].id).await.labels, vec!["infra", "urgent"]);
}

/// A page with no title column must not become a bead with no title. An empty
/// title is unfindable and nothing downstream will ever explain where it came
/// from.
#[tokio::test]
async fn a_page_missing_the_title_property_is_skipped_with_a_reason() {
    let (_d, ctx) = workspace("no-title").await;
    let body = r#"{
      "results": [
        {
          "id": "page-good",
          "properties": {
            "Name": { "type": "title", "title": [{ "plain_text": "Real work" }] }
          }
        },
        {
          "id": "page-bad",
          "properties": {
            "Summary": { "type": "title", "title": [{ "plain_text": "Wrong column" }] }
          }
        },
        {
          "id": "page-empty",
          "properties": { "Name": { "type": "title", "title": [] } }
        }
      ],
      "has_more": false,
      "next_cursor": null
    }"#;

    let http = FakeHttp::new().on(Method::Post, QUERY_URL, 200, body);
    let report = Notion.pull(&ctx, &http).await.unwrap();

    assert_eq!((report.pulled, report.created), (1, 1));
    let issues = all_issues(&ctx).await;
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].title, "Real work");

    let reasons = report.skipped.join("\n");
    assert!(
        reasons.contains("page-bad") && reasons.contains("notion.prop.title"),
        "the skip must name the page and the config key that would fix it: {reasons}"
    );
    assert!(
        reasons.contains("page-empty") && reasons.contains("empty"),
        "an empty title is a skip, not an empty bead: {reasons}"
    );
}

/// Config pointing at the wrong column is not a page we can guess our way
/// through: importing it would mislabel every issue in the database.
#[tokio::test]
async fn a_property_of_the_wrong_type_skips_the_page() {
    let (_d, ctx) = workspace("wrong-type").await;
    let body = r#"{
      "results": [
        {
          "id": "page-ccc",
          "properties": {
            "Name": { "type": "title", "title": [{ "plain_text": "Mislabeled" }] },
            "Status": { "type": "rich_text", "rich_text": [{ "plain_text": "done" }] }
          }
        }
      ],
      "has_more": false,
      "next_cursor": null
    }"#;

    let report = Notion
        .pull(&ctx, &FakeHttp::new().on(Method::Post, QUERY_URL, 200, body))
        .await
        .unwrap();

    assert_eq!(report.pulled, 0);
    assert!(all_issues(&ctx).await.is_empty());
    let reasons = report.skipped.join("\n");
    assert!(
        reasons.contains("rich_text") && reasons.contains("notion.prop.status"),
        "the skip must say what the column actually is: {reasons}"
    );
}

/// A pull that ignores `has_more` syncs page one and reports success. The only
/// evidence either way is the request it made second.
#[tokio::test]
async fn cursor_pagination_is_followed_to_the_last_page() {
    let (_d, ctx) = workspace("paging").await;

    let page1 = r#"{
      "results": [
        { "id": "page-1", "properties": {
            "Name": { "type": "title", "title": [{ "plain_text": "First" }] } } }
      ],
      "has_more": true,
      "next_cursor": "cursor-1"
    }"#;
    let page2 = r#"{
      "results": [
        { "id": "page-2", "properties": {
            "Name": { "type": "title", "title": [{ "plain_text": "Second" }] } } }
      ],
      "has_more": false,
      "next_cursor": null
    }"#;

    let http = SeqHttp::default()
        .on(Method::Post, QUERY_URL, 200, page1)
        .on(Method::Post, QUERY_URL, 200, page2);

    let report = Notion.pull(&ctx, &http).await.unwrap();
    assert_eq!((report.pulled, report.created), (2, 2));

    let mut titles: Vec<String> = all_issues(&ctx).await.into_iter().map(|i| i.title).collect();
    titles.sort();
    assert_eq!(titles, vec!["First", "Second"]);

    let reqs = http.requests();
    assert_eq!(reqs.len(), 2, "the second page was never asked for");
    let first = reqs[0].body.clone().unwrap_or_default();
    let second = reqs[1].body.clone().unwrap_or_default();
    assert!(
        !first.contains("start_cursor"),
        "the first request must not send a cursor: {first}"
    );
    assert!(
        second.contains("\"start_cursor\":\"cursor-1\""),
        "the cursor from page one must be sent back in the body: {second}"
    );
    // The version header is not just on the first request.
    assert!(
        reqs.iter().all(|r| r
            .headers
            .iter()
            .any(|(k, v)| k == "Notion-Version" && v == "2022-06-28")),
        "every request must carry Notion-Version"
    );
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// Push reads the database schema first, because the envelope a value must be
/// wrapped in is the column's type — and only the schema knows it.
#[tokio::test]
async fn push_patches_the_linked_page_using_the_columns_the_database_actually_has() {
    let (_d, ctx) = workspace("push").await;

    // Get an issue linked to a page the honest way: pull one.
    Notion
        .pull(
            &ctx,
            &FakeHttp::new().on(Method::Post, QUERY_URL, 200, DEFAULT_PROPS_PAGE),
        )
        .await
        .unwrap();

    let schema = r#"{
      "properties": {
        "Name": { "type": "title", "title": {} },
        "Status": { "type": "status", "status": { "options": [
          { "name": "Not started" }, { "name": "In progress" }, { "name": "Done" }
        ] } },
        "Priority": { "type": "select", "select": { "options": [
          { "name": "P0" }, { "name": "P1" }, { "name": "P2" }
        ] } },
        "Tags": { "type": "multi_select", "multi_select": { "options": [] } }
      }
    }"#;

    let http = FakeHttp::new()
        .on(Method::Get, SCHEMA_URL, 200, schema)
        .on(
            Method::Patch,
            "https://api.notion.com/v1/pages/page-aaa",
            200,
            r#"{ "id": "page-aaa" }"#,
        );

    let report = Notion.push(&ctx, &http).await.unwrap();
    assert_eq!(report.pushed, 1);

    let reqs = http.requests();
    assert_eq!(reqs.len(), 2, "one schema read, one page update");
    assert_eq!(reqs[0].method, Method::Get);
    assert_eq!(reqs[1].method, Method::Patch);
    assert!(
        reqs[1]
            .headers
            .iter()
            .any(|(k, v)| k == "Notion-Version" && v == "2022-06-28"),
        "the PATCH must carry the version header too"
    );

    let body: serde_json::Value = serde_json::from_str(reqs[1].body.as_deref().unwrap()).unwrap();
    let props = &body["properties"];
    assert_eq!(props["Name"]["title"][0]["text"]["content"], "Fix the parser");
    // The option name is the user's spelling ("In progress"), not ours
    // ("in_progress") — and it is wrapped in `status`, not `select`, because that
    // is what the column is.
    assert_eq!(props["Status"]["status"]["name"], "In progress");
    assert_eq!(props["Priority"]["select"]["name"], "P1");
    assert_eq!(props["Tags"]["multi_select"][0]["name"], "infra");
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_the_resolved_property_mapping() {
    let (_d, ctx) = workspace("status").await;
    set_cfg(&ctx, "notion.prop.title", "Task").await;

    let st = Notion.status(&ctx).await.unwrap();
    assert!(st.configured, "database id and token are both set: {st:?}");
    assert!(st.missing.is_empty());

    // With a user-defined schema, "which column did it take for the title" is
    // the first question any failure raises. `bd notion status` answers it.
    let detail = st.detail.unwrap_or_default();
    assert!(detail.contains("title=`Task`"), "{detail}");
    assert!(detail.contains("status=`Status`"), "{detail}");
    assert!(detail.contains(DB), "{detail}");
}

#[tokio::test]
async fn status_names_the_database_key_when_it_is_unset() {
    set_token();
    let dir = tempdir("status-unset");
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap(), "init", "--prefix", "nt"])
        .output()
        .expect("run bd init");
    assert!(out.status.success());

    let ctx = ctx_for(&dir).await;
    let st = Notion.status(&ctx).await.unwrap();
    assert!(!st.configured);
    assert_eq!(st.missing, vec!["notion.database_id"]);
    // Not "not implemented yet" — that string is what `bd notion pull` checks to
    // decide whether to exit 64 instead of running.
    assert_ne!(st.detail.as_deref(), Some("not implemented yet"));
}
