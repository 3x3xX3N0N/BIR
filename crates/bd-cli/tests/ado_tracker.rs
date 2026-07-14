//! Azure DevOps, against recorded fixtures. No network, no credentials.
//!
//! The tests that earn their keep here are the ones that assert on the
//! *requests*, not the responses. A tracker that fetches only the first 200 ids
//! and one that fetches all 250 return the same shape to their caller; they
//! differ only in what they asked for, and the difference shows up in
//! production, on somebody's real backlog, and nowhere else.

use std::path::{Path, PathBuf};
use std::process::Command;

use bd_cli::cli::Cli;
use bd_cli::context::{Ctx, Need};
use bd_cli::integrations::Tracker;
use bd_cli::integrations::ado::Ado;
use bd_cli::integrations::http::{FakeHttp, HttpRequest, Method};
use bd_core::{IssueFilter, IssueType, Priority, Status};
use clap::Parser;
use serde_json::{Value, json};

const ORG: &str = "contoso";
const PROJECT: &str = "Widgets";
const PAT: &str = "s3cr3t";

/// `Basic base64(":s3cr3t")`. Computed independently of the implementation —
/// the empty username before the colon is the whole point.
const EXPECTED_AUTH: &str = "Basic OnMzY3IzdA==";

fn wiql_url() -> String {
    format!("https://dev.azure.com/{ORG}/{PROJECT}/_apis/wit/wiql?api-version=7.0")
}

fn batch_url(ids: &[i64]) -> String {
    let csv = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("https://dev.azure.com/{ORG}/{PROJECT}/_apis/wit/workitems?ids={csv}&api-version=7.0")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn wiql_response(ids: &[i64]) -> String {
    // WIQL returns references — ids and urls. No fields. That is the trap the
    // two-step flow exists for, so the fixture must not be more generous than
    // the real API is.
    let items: Vec<Value> = ids
        .iter()
        .map(|id| json!({ "id": id, "url": format!("https://dev.azure.com/_apis/wit/workItems/{id}") }))
        .collect();
    json!({ "queryType": "flat", "workItems": items }).to_string()
}

fn work_item(id: i64, title: &str, state: &str, wit: &str, priority: i64) -> Value {
    json!({
        "id": id,
        "rev": 3,
        "fields": {
            "System.Title": title,
            "System.State": state,
            "System.WorkItemType": wit,
            "System.Description": format!("<div>body of {id}</div>"),
            "Microsoft.VSTS.Common.Priority": priority,
            "System.AssignedTo": { "displayName": "Ada L", "uniqueName": "ada@contoso.com" },
        }
    })
}

fn batch_response(items: Vec<Value>) -> String {
    json!({ "count": items.len(), "value": items }).to_string()
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The PAT lives in the environment, and the environment is process-wide.
///
/// Written exactly once, under a `Once`: every thread that goes on to read it
/// has already been through here, so the write happens-before every read and
/// there is no race for `set_var` to lose.
fn set_pat() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { std::env::set_var("AZURE_DEVOPS_PAT", PAT) });
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("bd-ado-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

fn bd(dir: &Path, args: &[&str]) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .env("BEADS_ACTOR", "tester")
        .env_remove("AZURE_DEVOPS_PAT")
        .output()
        .expect("run bd");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// A workspace, and a `Ctx` pointed at it, with ado configured unless told not to.
async fn workspace(tag: &str, configured: bool) -> (PathBuf, Ctx) {
    set_pat();
    let dir = tempdir(tag);
    assert_eq!(bd(&dir, &["init", "--prefix", "w"]).1, 0, "init failed");

    let cli = Cli::try_parse_from([
        "bd",
        "-C",
        dir.to_str().unwrap(),
        "--actor",
        "tester",
        "list",
    ])
    .expect("the harness's own command line must parse");
    let ctx = Ctx::build(&cli, Need::Workspace).await.expect("build ctx");

    if configured {
        let store = ctx.store().await.expect("open store");
        store.set_config("ado.org", ORG).await.unwrap();
        store.set_config("ado.project", PROJECT).await.unwrap();
    }
    (dir, ctx)
}

fn header<'a>(r: &'a HttpRequest, name: &str) -> &'a str {
    r.headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_default()
}

fn cleanup(dir: PathBuf) {
    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------------------
// The two-step read
// ---------------------------------------------------------------------------

/// WIQL for the ids, then a batch GET for the fields. A single-step
/// implementation would find nothing and report an empty backlog, which looks
/// exactly like a project with no work in it.
#[tokio::test]
async fn pull_queries_wiql_then_fetches_the_work_items() {
    let (dir, ctx) = workspace("two-step", true).await;

    let http = FakeHttp::new()
        .on(Method::Post, &wiql_url(), 200, &wiql_response(&[7, 9]))
        .on(
            Method::Get,
            &batch_url(&[7, 9]),
            200,
            &batch_response(vec![
                work_item(7, "Ship the thing", "Active", "Task", 2),
                work_item(9, "Fix the thing", "New", "Bug", 1),
            ]),
        );

    let report = Ado.pull(&ctx, &http).await.expect("pull");
    assert_eq!((report.pulled, report.created, report.updated), (2, 2, 0));

    let reqs = http.requests();
    assert_eq!(reqs.len(), 2, "expected exactly two calls, got {reqs:?}");

    // Step 1: the WIQL query, as a POST, with a query that selects ids.
    assert_eq!(reqs[0].method, Method::Post);
    assert_eq!(reqs[0].url, wiql_url());
    let q = reqs[0].body.clone().expect("WIQL is a POST with a body");
    assert!(q.contains("SELECT [System.Id] FROM WorkItems"), "{q}");
    assert!(
        q.contains(&format!("[System.TeamProject] = '{PROJECT}'")),
        "{q}"
    );

    // Step 2: the ids come back from step 1 and go out in the batch GET.
    assert_eq!(reqs[1].method, Method::Get);
    assert_eq!(reqs[1].url, batch_url(&[7, 9]));

    // Auth, on every call: Basic with an *empty username*. A bearer token 401s.
    for r in &reqs {
        assert_eq!(
            header(r, "Authorization"),
            EXPECTED_AUTH,
            "ADO wants Basic base64(\":{{pat}}\"), not a bearer token"
        );
    }

    ctx.close().await;
    cleanup(dir);
}

/// **The 200-id cap.** 250 ids must go out as two batch GETs.
///
/// One oversized request would work against every fixture ever written and fail
/// against every backlog worth syncing.
#[tokio::test]
async fn the_batch_get_is_chunked_at_two_hundred_ids() {
    let (dir, ctx) = workspace("chunking", true).await;

    let ids: Vec<i64> = (1..=250).collect();
    let items = |range: &[i64]| {
        batch_response(
            range
                .iter()
                .map(|id| work_item(*id, &format!("Item {id}"), "New", "Task", 2))
                .collect(),
        )
    };

    let http = FakeHttp::new()
        .on(Method::Post, &wiql_url(), 200, &wiql_response(&ids))
        .on(
            Method::Get,
            &batch_url(&ids[..200]),
            200,
            &items(&ids[..200]),
        )
        .on(
            Method::Get,
            &batch_url(&ids[200..]),
            200,
            &items(&ids[200..]),
        );

    // FakeHttp errors loudly on an unstubbed URL, so a single 250-id request
    // fails here rather than passing quietly — but assert the shape anyway, so
    // the failure names the bug instead of just "no stubbed response".
    let report = Ado.pull(&ctx, &http).await.expect("pull");
    assert_eq!(report.created, 250, "every work item must land");

    let reqs = http.requests();
    let gets: Vec<&HttpRequest> = reqs.iter().filter(|r| r.method == Method::Get).collect();
    assert_eq!(
        gets.len(),
        2,
        "250 ids must be two batch GETs (the endpoint caps at 200), got {}",
        gets.len()
    );
    assert_eq!(
        gets[0].url.matches(',').count(),
        199,
        "first batch: 200 ids"
    );
    assert_eq!(gets[1].url.matches(',').count(), 49, "second batch: 50 ids");

    ctx.close().await;
    cleanup(dir);
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pull_maps_the_fields_and_records_the_join_key() {
    let (dir, ctx) = workspace("mapping", true).await;

    let http = FakeHttp::new()
        .on(Method::Post, &wiql_url(), 200, &wiql_response(&[42]))
        .on(
            Method::Get,
            &batch_url(&[42]),
            200,
            &batch_response(vec![work_item(42, "Rename the widget", "Active", "Bug", 1)]),
        );

    Ado.pull(&ctx, &http).await.expect("pull");

    let store = ctx.store().await.unwrap();
    let all = store.list_issues(&IssueFilter::default()).await.unwrap();
    assert_eq!(all.len(), 1);
    let i = &all[0];

    assert_eq!(i.title, "Rename the widget");
    assert_eq!(i.status, Status::InProgress, "Active is in flight");
    assert_eq!(i.issue_type, IssueType::Bug);
    assert_eq!(i.priority, Priority::CRITICAL, "ADO priority 1 is P0");
    assert_eq!(i.assignee, "ada@contoso.com");
    assert!(i.description.contains("body of 42"));

    // The join key. Both halves — one alone duplicates the backlog next run.
    assert_eq!(i.external_ref.as_deref(), Some("42"));
    assert_eq!(i.source_system, "ado");

    ctx.close().await;
    cleanup(dir);
}

/// **An unrecognized `System.State` is open, not closed.**
///
/// `System.State` belongs to the process template, so any state at all is legal.
/// Guessing "closed" for one we do not know takes the work out of `bd ready`,
/// where nobody will ever see it again. Guessing "open" costs a glance.
#[tokio::test]
async fn an_unknown_state_is_open_and_the_known_ones_map() {
    let (dir, ctx) = workspace("states", true).await;

    let ids: Vec<i64> = (1..=8).collect();
    let items = vec![
        work_item(1, "agile new", "New", "Task", 2),
        work_item(2, "agile active", "Active", "Task", 2),
        work_item(3, "agile resolved", "Resolved", "Task", 2),
        work_item(4, "agile closed", "Closed", "Task", 2),
        work_item(5, "scrum committed", "Committed", "Task", 2),
        work_item(6, "scrum done", "Done", "Task", 2),
        work_item(7, "scrum removed", "Removed", "Task", 2),
        // A state from somebody's customized process. Nothing in the API says
        // what it means, so it must not be treated as terminal.
        work_item(8, "custom", "Needs Triage", "Task", 2),
    ];

    let http = FakeHttp::new()
        .on(Method::Post, &wiql_url(), 200, &wiql_response(&ids))
        .on(Method::Get, &batch_url(&ids), 200, &batch_response(items));

    Ado.pull(&ctx, &http).await.expect("pull");

    let store = ctx.store().await.unwrap();
    let all = store.list_issues(&IssueFilter::default()).await.unwrap();
    let status_of = |ext: &str| {
        all.iter()
            .find(|i| i.external_ref.as_deref() == Some(ext))
            .unwrap_or_else(|| panic!("work item {ext} did not land"))
            .status
            .clone()
    };

    assert_eq!(status_of("1"), Status::Open);
    assert_eq!(status_of("2"), Status::InProgress);
    assert_eq!(
        status_of("3"),
        Status::InProgress,
        "Resolved is dev-complete but unverified — closing it here never reopens"
    );
    assert_eq!(status_of("4"), Status::Closed);
    assert_eq!(status_of("5"), Status::InProgress);
    assert_eq!(status_of("6"), Status::Closed);
    assert_eq!(status_of("7"), Status::Closed, "Removed is terminal");
    assert_eq!(
        status_of("8"),
        Status::Open,
        "an unknown state must never be guessed closed"
    );

    ctx.close().await;
    cleanup(dir);
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Pull the same fixture twice. The second run must update, never duplicate.
#[tokio::test]
async fn pulling_twice_updates_and_creates_nothing() {
    let (dir, ctx) = workspace("idempotent", true).await;

    let ids = [11, 12, 13];
    let http = FakeHttp::new()
        .on(Method::Post, &wiql_url(), 200, &wiql_response(&ids))
        .on(
            Method::Get,
            &batch_url(&ids),
            200,
            &batch_response(vec![
                work_item(11, "one", "New", "Task", 2),
                work_item(12, "two", "Active", "Bug", 1),
                work_item(13, "three", "Closed", "Task", 3),
            ]),
        );

    let first = Ado.pull(&ctx, &http).await.expect("first pull");
    assert_eq!((first.created, first.updated), (3, 0));

    let second = Ado.pull(&ctx, &http).await.expect("second pull");
    assert_eq!(
        (second.created, second.updated),
        (0, 3),
        "a second pull of the same work items must update them, not clone them"
    );

    let store = ctx.store().await.unwrap();
    let all = store.list_issues(&IssueFilter::default()).await.unwrap();
    assert_eq!(all.len(), 3, "the backlog was duplicated: {all:?}");

    ctx.close().await;
    cleanup(dir);
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// Push sends a JSON Patch *document* — a list of ops, with the json-patch
/// content type. A plain JSON object is rejected by the API.
#[tokio::test]
async fn push_creates_a_work_item_with_a_json_patch_document() {
    let (dir, ctx) = workspace("push", true).await;

    // One local bead, filed by hand, bound to nothing.
    assert_eq!(bd(&dir, &["create", "Local work", "-p", "0"]).1, 0);

    let create_url =
        format!("https://dev.azure.com/{ORG}/{PROJECT}/_apis/wit/workitems/$Task?api-version=7.0");
    let http = FakeHttp::new().on(
        Method::Post,
        &create_url,
        200,
        &json!({ "id": 501, "rev": 1 }).to_string(),
    );

    let report = Ado.push(&ctx, &http).await.expect("push");
    assert_eq!(report.pushed, 1);

    let reqs = http.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        header(&reqs[0], "Content-Type"),
        "application/json-patch+json",
        "a work item is created with a JSON Patch document, not a JSON object"
    );
    assert_eq!(header(&reqs[0], "Authorization"), EXPECTED_AUTH);

    let body: Value = serde_json::from_str(reqs[0].body.as_deref().unwrap()).expect("JSON body");
    let ops = body.as_array().expect("the body is a *list* of patch ops");
    let title = ops
        .iter()
        .find(|o| o["path"] == "/fields/System.Title")
        .expect("System.Title op");
    assert_eq!(title["op"], "add");
    assert_eq!(title["value"], "Local work");
    let prio = ops
        .iter()
        .find(|o| o["path"] == "/fields/Microsoft.VSTS.Common.Priority")
        .expect("priority op");
    assert_eq!(prio["value"], 1, "beads P0 is ADO priority 1");

    // The created id is bound to the bead, or the next push files it again.
    let store = ctx.store().await.unwrap();
    let all = store.list_issues(&IssueFilter::default()).await.unwrap();
    assert_eq!(all[0].external_ref.as_deref(), Some("501"));

    // And a second push, with nothing new to say, says nothing.
    let again = Ado.push(&ctx, &http).await.expect("second push");
    assert_eq!(
        (again.pushed, http.requests().len()),
        (0, 1),
        "an already-linked bead must not be filed upstream twice"
    );

    ctx.close().await;
    cleanup(dir);
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// `status` is the command you run *because* the tracker does not work. It has
/// to name what is missing rather than explode, and it must not be mistaken for
/// an unported stub.
#[tokio::test]
async fn status_names_the_missing_config() {
    let (dir, ctx) = workspace("status", false).await;

    let st = Ado.status(&ctx).await.expect("status must not fail");
    assert!(!st.configured);
    assert!(st.missing.contains(&"ado.org".to_string()), "{st:?}");
    assert!(st.missing.contains(&"ado.project".to_string()), "{st:?}");
    assert_ne!(
        st.detail.as_deref(),
        Some("not implemented yet"),
        "`commands::sync` reads that exact string as `this tracker is a stub` and exits 64"
    );

    // Configure it and it says so.
    let store = ctx.store().await.unwrap();
    store.set_config("ado.org", ORG).await.unwrap();
    store.set_config("ado.project", PROJECT).await.unwrap();
    let st = Ado.status(&ctx).await.expect("status");
    assert!(st.configured, "{st:?}");
    assert!(st.missing.is_empty());
    assert_eq!(
        st.detail.as_deref(),
        Some("https://dev.azure.com/contoso/Widgets")
    );

    ctx.close().await;
    cleanup(dir);
}

/// The same thing through the real binary, with the token *actually* absent from
/// the environment — which is the only way to test the missing-token path
/// without racing every other test in this process for `set_var`.
///
/// `pull` is here for the exit code: an unconfigured tracker is a configuration
/// error (1), not an unported command (64), and no HTTP client is built on the
/// way to saying so.
#[test]
fn an_unconfigured_ado_reports_its_missing_token_and_does_not_look_like_a_stub() {
    let dir = tempdir("status-cli");
    assert_eq!(bd(&dir, &["init", "--prefix", "w"]).1, 0);

    let (out, code) = bd(&dir, &["--json", "ado", "status"]);
    assert_eq!(
        code, 0,
        "status must work when nothing is configured: {out}"
    );
    let doc: Value = serde_json::from_str(&out).expect("--json status emits JSON");
    assert_eq!(doc["configured"], false);
    let missing: Vec<&str> = doc["missing"]
        .as_array()
        .expect("missing keys")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(missing.contains(&"ado.org"), "{missing:?}");
    assert!(missing.contains(&"ado.project"), "{missing:?}");
    assert!(
        missing.contains(&"AZURE_DEVOPS_PAT"),
        "the token is missing too, and `status` exists to say so: {missing:?}"
    );

    let (out, code) = bd(&dir, &["ado", "pull"]);
    assert_eq!(
        code, 1,
        "unconfigured is a misconfiguration (1), not an unported command (64): {out}"
    );

    cleanup(dir);
}
