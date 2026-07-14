//! The Linear tracker, against recorded GraphQL fixtures and a real database.
//!
//! No network, no credentials, no Linear account. Every request goes through the
//! `Http` seam, and every assertion that matters here is about what the tracker
//! *asked for* as much as what it stored — a tracker that pages correctly and one
//! that silently syncs page one look identical from the outside.

use std::path::PathBuf;
use std::process::Command;
use std::sync::LazyLock;

use bd_cli::cli::Cli;
use bd_cli::context::{Ctx, Need};
use bd_cli::integrations::http::{FakeHttp, Method};
use bd_cli::integrations::{Tracker, linear::Linear};
use bd_core::{IssueFilter, Issue, Priority, Status};
use bd_storage::Identity;
use clap::Parser;

const ENDPOINT: &str = "https://api.linear.app/graphql";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Set once, before anything in this binary reads it.
///
/// `LazyLock` blocks every other thread until the initializer returns, so the
/// write cannot race a read made through `Tracker`. The "no token" case is
/// deliberately *not* tested by unsetting this — a process-wide unset would race
/// every other test in the binary. It runs `bd` as a subprocess instead, with its
/// own environment.
static TOKEN: LazyLock<()> = LazyLock::new(|| {
    // SAFETY: single-shot, and no test touches the tracker before going through
    // this lock.
    unsafe { std::env::set_var("LINEAR_API_KEY", "lin_api_fixture") };
});

fn with_token() {
    LazyLock::force(&TOKEN);
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-linear-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// A real sqlite workspace and a `Ctx` pointed at it.
async fn workspace(tag: &str) -> (PathBuf, Ctx) {
    with_token();
    let dir = tempdir(tag);
    bd_sqlite::init(&dir, "bd", Identity::new("tester"))
        .await
        .expect("init")
        .close()
        .await
        .expect("close");

    let cli = Cli::parse_from(["bd", "-C", dir.to_str().unwrap(), "list"]);
    let ctx = Ctx::build(&cli, Need::Workspace).await.expect("ctx");
    (dir, ctx)
}

/// A configured workspace: the team key is where `bd config set` puts it.
async fn configured(tag: &str) -> (PathBuf, Ctx) {
    let (dir, ctx) = workspace(tag).await;
    ctx.store()
        .await
        .unwrap()
        .set_config("linear.team", "ENG")
        .await
        .unwrap();
    (dir, ctx)
}

async fn all_issues(ctx: &Ctx) -> Vec<Issue> {
    ctx.store()
        .await
        .unwrap()
        .list_issues(&IssueFilter::default())
        .await
        .unwrap()
}

/// The bead carrying this Linear id, hydrated (labels included).
async fn by_ref(ctx: &Ctx, external_ref: &str) -> Issue {
    let listed = all_issues(ctx).await;
    let stub = listed
        .iter()
        .find(|i| i.external_ref.as_deref() == Some(external_ref))
        .unwrap_or_else(|| panic!("no bead references {external_ref}"));
    ctx.store()
        .await
        .unwrap()
        .get_issue(&stub.id)
        .await
        .unwrap()
        .expect("hydrated")
}

/// Responses in order, one per request.
///
/// Every Linear request is a POST to the same endpoint, so `FakeHttp::on()` —
/// one canned response per `"METHOD url"` — cannot express a fixture whose second
/// response differs from its first, which is exactly what paging and mutation
/// sequences are. `on_seq` queues them against that one key.
///
/// A request to any *other* URL finds no stub and fails loudly, which is the
/// assertion that Linear speaks GraphQL at one URL only. Running the queue dry
/// fails just as loudly: a tracker that made one request too many must never be
/// handed a blank 200 and pass.
fn seq(bodies: &[&str]) -> FakeHttp {
    FakeHttp::new().on_seq(
        Method::Post,
        ENDPOINT,
        bodies.iter().map(|b| (200u16, b.to_string())).collect(),
    )
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Four issues covering every axis that can be got wrong: an urgent one, an
/// untriaged one (Linear priority 0), a completed one, and a canceled one.
fn page_one() -> &'static str {
    r#"{"data":{"issues":{"nodes":[
      {"id":"11111111-1111-1111-1111-111111111111","identifier":"ENG-1",
       "title":"Rewrite the tokenizer","description":"It eats emoji.",
       "priority":1,"url":"https://linear.app/acme/issue/ENG-1",
       "createdAt":"2026-07-01T10:00:00.000Z","updatedAt":"2026-07-02T10:00:00.000Z",
       "completedAt":null,"canceledAt":null,
       "state":{"name":"In Progress","type":"started"},
       "assignee":{"email":"dev@acme.test","name":"Dev"},
       "labels":{"nodes":[{"name":"bug"}]}},

      {"id":"22222222-2222-2222-2222-222222222222","identifier":"ENG-2",
       "title":"Somebody should look at the logs","description":"",
       "priority":0,"url":"https://linear.app/acme/issue/ENG-2",
       "createdAt":"2026-07-01T11:00:00.000Z","updatedAt":"2026-07-01T11:00:00.000Z",
       "completedAt":null,"canceledAt":null,
       "state":{"name":"Backlog","type":"backlog"},
       "assignee":null,"labels":{"nodes":[]}},

      {"id":"33333333-3333-3333-3333-333333333333","identifier":"ENG-3",
       "title":"Ship the beta","description":"",
       "priority":4,"url":"https://linear.app/acme/issue/ENG-3",
       "createdAt":"2026-06-01T09:00:00.000Z","updatedAt":"2026-06-20T09:00:00.000Z",
       "completedAt":"2026-06-20T09:00:00.000Z","canceledAt":null,
       "state":{"name":"Done","type":"completed"},
       "assignee":null,"labels":{"nodes":[{"name":"feature"}]}},

      {"id":"44444444-4444-4444-4444-444444444444","identifier":"ENG-4",
       "title":"Port to CORBA","description":"",
       "priority":2,"url":"https://linear.app/acme/issue/ENG-4",
       "createdAt":"2026-05-01T09:00:00.000Z","updatedAt":"2026-05-09T09:00:00.000Z",
       "completedAt":null,"canceledAt":"2026-05-09T09:00:00.000Z",
       "state":{"name":"Canceled","type":"canceled"},
       "assignee":null,"labels":{"nodes":[]}}
    ],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}"#
}

fn team_fixture() -> &'static str {
    r#"{"data":{"teams":{"nodes":[{"id":"team-uuid","key":"ENG","states":{"nodes":[
      {"id":"state-triage","name":"Triage","type":"triage","position":0},
      {"id":"state-todo","name":"Todo","type":"unstarted","position":1},
      {"id":"state-doing","name":"In Progress","type":"started","position":2},
      {"id":"state-done","name":"Done","type":"completed","position":3},
      {"id":"state-canceled","name":"Canceled","type":"canceled","position":4}
    ]}}]}}}"#
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pull_maps_a_linear_issue_onto_a_bead() {
    let (dir, ctx) = configured("map").await;
    let http = FakeHttp::new().on(Method::Post, ENDPOINT, 200, page_one());

    let report = Linear.pull(&ctx, &http).await.expect("pull");
    assert_eq!(report.pulled, 4);
    assert_eq!(report.created, 4);
    assert_eq!(report.updated, 0);

    let bead = by_ref(&ctx, "11111111-1111-1111-1111-111111111111").await;
    assert_eq!(bead.title, "Rewrite the tokenizer");
    assert_eq!(bead.description, "It eats emoji.");
    assert_eq!(bead.status, Status::InProgress, "state type `started`");
    assert_eq!(bead.assignee, "dev@acme.test");
    assert_eq!(bead.labels, vec!["bug".to_string()]);
    assert_eq!(bead.issue_type.as_str(), "bug", "inferred from the label");

    // The join key. Both halves, or the next pull duplicates the backlog.
    assert_eq!(
        bead.external_ref.as_deref(),
        Some("11111111-1111-1111-1111-111111111111"),
        "external_ref must be Linear's stable id"
    );
    assert_eq!(bead.source_system, "linear");

    // The human-facing identifier is not the join key, but losing it entirely
    // would make a synced bead impossible to find in Linear.
    let md = bead.metadata.expect("metadata");
    assert_eq!(md["linear"]["identifier"], "ENG-1");

    // The auth header is the raw key. `Bearer <api key>` is a 401 from Linear,
    // and the message it returns blames the token.
    let sent = &http.requests()[0];
    let auth = sent
        .headers
        .iter()
        .find(|(k, _)| k == "Authorization")
        .expect("Authorization header");
    assert_eq!(auth.1, "lin_api_fixture", "no Bearer prefix on an API key");

    // The team key must actually reach the server, or the query returns whatever
    // the token can see across every team.
    assert!(sent.body.as_deref().unwrap().contains("\"team\":\"ENG\""));

    std::fs::remove_dir_all(dir).ok();
}

/// The test this whole module exists for.
///
/// A tracker that cannot recognize what it already pulled duplicates the entire
/// backlog on every sync — and it does so *successfully*, reporting a bigger and
/// bigger number each time.
#[tokio::test]
async fn pull_twice_creates_once_and_updates_thereafter() {
    let (dir, ctx) = configured("idempotent").await;
    let http = FakeHttp::new().on(Method::Post, ENDPOINT, 200, page_one());

    let first = Linear.pull(&ctx, &http).await.expect("first pull");
    assert_eq!((first.created, first.updated), (4, 0));

    let second = Linear.pull(&ctx, &http).await.expect("second pull");
    assert_eq!(
        (second.created, second.updated),
        (0, 4),
        "the second pull must recognize all four and create nothing"
    );

    assert_eq!(all_issues(&ctx).await.len(), 4, "no duplicates in the store");

    // And a third, because "idempotent after two" has been a real bug.
    Linear.pull(&ctx, &http).await.expect("third pull");
    assert_eq!(all_issues(&ctx).await.len(), 4);

    std::fs::remove_dir_all(dir).ok();
}

/// Linear's 0-4 and beads' P0-P4 use the same digits for opposite meanings.
#[tokio::test]
async fn the_priority_scale_is_not_copied_across() {
    let (dir, ctx) = configured("priority").await;
    let http = FakeHttp::new().on(Method::Post, ENDPOINT, 200, page_one());
    Linear.pull(&ctx, &http).await.expect("pull");

    // Linear 1 = Urgent. Beads' most urgent is P0.
    let urgent = by_ref(&ctx, "11111111-1111-1111-1111-111111111111").await;
    assert_eq!(urgent.priority, Priority::CRITICAL, "Urgent must land as P0");

    // Linear 0 = "No priority" — the *absence* of one. A naive copy makes it P0,
    // and every untriaged issue in the team becomes the most critical bead in the
    // workspace.
    let untriaged = by_ref(&ctx, "22222222-2222-2222-2222-222222222222").await;
    assert_ne!(
        untriaged.priority,
        Priority::CRITICAL,
        "Linear's `no priority` must never become beads' P0"
    );
    assert_eq!(untriaged.priority, Priority::NORMAL);

    // Linear 4 = Low. A copy would make it P4 (trivial) — plausible-looking, and
    // still wrong: it is one rung below where it belongs.
    let low = by_ref(&ctx, "33333333-3333-3333-3333-333333333333").await;
    assert_eq!(low.priority, Priority::LOW);

    // Linear 2 = High.
    let high = by_ref(&ctx, "44444444-4444-4444-4444-444444444444").await;
    assert_eq!(high.priority, Priority::HIGH);

    std::fs::remove_dir_all(dir).ok();
}

/// `canceled` and `completed` are both terminal and are not the same thing.
/// Beads keeps the difference in `close_reason`, which `is_failure_close` reads
/// to decide whether a `conditional-blocks` dependent becomes ready.
#[tokio::test]
async fn a_canceled_issue_closes_as_a_failure_not_a_success() {
    let (dir, ctx) = configured("canceled").await;
    let http = FakeHttp::new().on(Method::Post, ENDPOINT, 200, page_one());
    Linear.pull(&ctx, &http).await.expect("pull");

    let canceled = by_ref(&ctx, "44444444-4444-4444-4444-444444444444").await;
    assert_eq!(canceled.status, Status::Closed);
    assert!(canceled.closed_at.is_some(), "a closed bead has a close time");
    assert!(
        bd_core::types::is_failure_close(&canceled.close_reason),
        "`{}` does not read as a failure, so the failure path of every \
         conditional-blocks edge stays gated",
        canceled.close_reason
    );

    let completed = by_ref(&ctx, "33333333-3333-3333-3333-333333333333").await;
    assert_eq!(completed.status, Status::Closed);
    assert!(
        !bd_core::types::is_failure_close(&completed.close_reason),
        "a completed issue must not close as a failure"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// One page fetched, one page synced, exit 0 — the most convincing bug in any
/// paginated integration, because it looks exactly like success.
#[tokio::test]
async fn cursor_pagination_is_followed() {
    let (dir, ctx) = configured("paging").await;

    let page1 = r#"{"data":{"issues":{"nodes":[
      {"id":"aaaa-1","identifier":"ENG-10","title":"First page","description":"",
       "priority":3,"url":null,"createdAt":"2026-07-01T10:00:00.000Z",
       "updatedAt":"2026-07-01T10:00:00.000Z","completedAt":null,"canceledAt":null,
       "state":{"name":"Todo","type":"unstarted"},"assignee":null,"labels":{"nodes":[]}}
    ],"pageInfo":{"hasNextPage":true,"endCursor":"CURSOR-PAGE-2"}}}}"#;

    let page2 = r#"{"data":{"issues":{"nodes":[
      {"id":"bbbb-2","identifier":"ENG-11","title":"Second page","description":"",
       "priority":3,"url":null,"createdAt":"2026-07-01T10:00:00.000Z",
       "updatedAt":"2026-07-01T10:00:00.000Z","completedAt":null,"canceledAt":null,
       "state":{"name":"Todo","type":"unstarted"},"assignee":null,"labels":{"nodes":[]}}
    ],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}"#;

    let http = seq(&[page1, page2]);
    let report = Linear.pull(&ctx, &http).await.expect("pull");

    assert_eq!(report.pulled, 2, "both pages must be synced");
    assert_eq!(report.created, 2);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let bodies = http.bodies();
    assert_eq!(bodies.len(), 2, "the second page must actually be fetched");

    // The first request opens the connection with no cursor; the second must
    // carry the one the server handed back. Asserting only on the issue count
    // would pass for a tracker that fetched page one twice.
    let first: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    assert!(
        first["variables"]["after"].is_null(),
        "the first page is fetched without a cursor"
    );
    let second: serde_json::Value = serde_json::from_str(&bodies[1]).unwrap();
    assert_eq!(
        second["variables"]["after"], "CURSOR-PAGE-2",
        "the cursor from pageInfo.endCursor must be sent on the next request"
    );

    let titles: Vec<String> = all_issues(&ctx).await.into_iter().map(|i| i.title).collect();
    assert!(titles.contains(&"First page".to_string()));
    assert!(titles.contains(&"Second page".to_string()));

    std::fs::remove_dir_all(dir).ok();
}

/// Linear answers a bad token with **HTTP 200** and an `errors` array. A tracker
/// that trusts the status code turns that into "0 issues, no problem".
#[tokio::test]
async fn a_graphql_error_under_a_200_is_not_a_successful_sync() {
    let (dir, ctx) = configured("gql-error").await;
    let http = FakeHttp::new().on(
        Method::Post,
        ENDPOINT,
        200,
        r#"{"data":null,"errors":[{"message":"Authentication required, not authenticated"}]}"#,
    );

    let err = Linear.pull(&ctx, &http).await.expect_err("must fail loudly");
    assert!(
        err.to_string().contains("Authentication required"),
        "the real message must survive: {err}"
    );
    assert_eq!(all_issues(&ctx).await.len(), 0);

    std::fs::remove_dir_all(dir).ok();
}

/// A page that repeats an issue must not create it twice — the in-memory index
/// has to learn about a bead the moment it is created, not on the next pull.
#[tokio::test]
async fn a_repeated_node_within_one_pull_is_created_once() {
    let (dir, ctx) = configured("repeat").await;

    let dupe = r#"{"data":{"issues":{"nodes":[
      {"id":"same-1","identifier":"ENG-20","title":"Seen twice","description":"",
       "priority":3,"url":null,"createdAt":"2026-07-01T10:00:00.000Z",
       "updatedAt":"2026-07-01T10:00:00.000Z","completedAt":null,"canceledAt":null,
       "state":{"name":"Todo","type":"unstarted"},"assignee":null,"labels":{"nodes":[]}},
      {"id":"same-1","identifier":"ENG-20","title":"Seen twice","description":"",
       "priority":3,"url":null,"createdAt":"2026-07-01T10:00:00.000Z",
       "updatedAt":"2026-07-01T10:00:00.000Z","completedAt":null,"canceledAt":null,
       "state":{"name":"Todo","type":"unstarted"},"assignee":null,"labels":{"nodes":[]}}
    ],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}"#;

    let http = FakeHttp::new().on(Method::Post, ENDPOINT, 200, dupe);
    let report = Linear.pull(&ctx, &http).await.expect("pull");
    assert_eq!((report.created, report.updated), (1, 1));
    assert_eq!(all_issues(&ctx).await.len(), 1);

    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// A local bead Linear has never seen is created there, and the link is written
/// back — otherwise the next pull sees a new remote issue and makes a second
/// local copy of the one we just pushed.
#[tokio::test]
async fn push_creates_an_unlinked_bead_and_records_the_link() {
    let (dir, ctx) = configured("push-create").await;

    let store = ctx.store().await.unwrap();
    let mut issue = Issue::new("bd-1", "Teach the parser to count");
    issue.description = "Recursion, mostly.".into();
    issue.priority = Priority::CRITICAL;
    issue.status = Status::InProgress;
    store.create_issue(&issue).await.unwrap();

    let created = r#"{"data":{"issueCreate":{"success":true,
      "issue":{"id":"new-remote-uuid","identifier":"ENG-99",
               "url":"https://linear.app/acme/issue/ENG-99"}}}}"#;
    let http = seq(&[team_fixture(), created]);

    let report = Linear.push(&ctx, &http).await.expect("push");
    assert_eq!(report.pushed, 1);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let bodies = http.bodies();
    assert_eq!(bodies.len(), 2, "one team lookup, one mutation");
    assert!(bodies[0].contains("teams"), "the team is resolved first");

    let mutation: serde_json::Value = serde_json::from_str(&bodies[1]).unwrap();
    assert!(mutation["query"].as_str().unwrap().contains("issueCreate"));
    let input = &mutation["variables"]["input"];
    assert_eq!(input["title"], "Teach the parser to count");
    // `issueCreate` takes the team's UUID, never the key.
    assert_eq!(input["teamId"], "team-uuid");
    // P0 (critical) is Linear's 1 (Urgent) — not its 0, which means "no priority"
    // and would silently untriage the most important bead in the workspace.
    assert_eq!(input["priority"], 1);
    // A state *id*, not a state type: `issueUpdate`/`issueCreate` take no types.
    assert_eq!(input["stateId"], "state-doing");

    let bead = ctx.store().await.unwrap().get_issue("bd-1").await.unwrap().unwrap();
    assert_eq!(
        bead.external_ref.as_deref(),
        Some("new-remote-uuid"),
        "the link must be written back or the next pull duplicates this bead"
    );

    std::fs::remove_dir_all(dir).ok();
}

/// A bead that came from Linear is updated in place, by its id — never created
/// again.
#[tokio::test]
async fn push_updates_a_linked_bead_in_place() {
    let (dir, ctx) = configured("push-update").await;

    // Pull it in first, so the link is real rather than hand-forged.
    let pull = FakeHttp::new().on(Method::Post, ENDPOINT, 200, page_one());
    Linear.pull(&ctx, &pull).await.expect("pull");

    let updated = r#"{"data":{"issueUpdate":{"success":true,
      "issue":{"id":"11111111-1111-1111-1111-111111111111","identifier":"ENG-1","url":null}}}}"#;
    // Four beads pulled, so four mutations follow the team lookup.
    let http = seq(&[team_fixture(), updated, updated, updated, updated]);

    let report = Linear.push(&ctx, &http).await.expect("push");
    assert_eq!(report.pushed, 4);

    // Every mutation after the team lookup is an update, keyed on Linear's id.
    let mutations: Vec<serde_json::Value> = http.bodies()[1..]
        .iter()
        .map(|b| serde_json::from_str(b).unwrap())
        .collect();
    for m in &mutations {
        let q = m["query"].as_str().unwrap();
        assert!(q.contains("issueUpdate"), "a linked bead updates, never creates");
        assert!(!q.contains("issueCreate"));
        assert!(m["variables"]["id"].is_string(), "keyed on Linear's id");
    }

    // The canceled one must go back as canceled, not as done.
    let canceled = mutations
        .iter()
        .find(|m| m["variables"]["id"].as_str().unwrap().starts_with("4444"))
        .expect("the canceled bead was pushed");
    assert_eq!(
        canceled["variables"]["input"]["stateId"], "state-canceled",
        "a bead closed with a failure reason is a *canceled* Linear issue"
    );

    // And the in-progress one lands in a `started` state.
    let doing = mutations
        .iter()
        .find(|m| m["variables"]["id"].as_str().unwrap().starts_with("1111"))
        .expect("the started bead was pushed");
    assert_eq!(doing["variables"]["input"]["stateId"], "state-doing");

    std::fs::remove_dir_all(dir).ok();
}

/// A bead another tracker owns is left alone. Pushing it would fork its identity
/// across two systems.
#[tokio::test]
async fn push_declines_beads_owned_by_another_tracker() {
    let (dir, ctx) = configured("push-foreign").await;

    let store = ctx.store().await.unwrap();
    let mut foreign = Issue::new("bd-9", "Filed in Jira");
    foreign.source_system = "jira".into();
    foreign.external_ref = Some("PROJ-7".into());
    store.create_issue(&foreign).await.unwrap();

    // Only the team lookup is stubbed: a mutation would run the fixture dry and
    // fail the test loudly, which is the assertion.
    let http = seq(&[team_fixture()]);
    let report = Linear.push(&ctx, &http).await.expect("push");

    assert_eq!(report.pushed, 0);
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].contains("another tracker"), "{:?}", report.skipped);

    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_names_the_missing_config_instead_of_exploding() {
    // Deliberately *not* configured: this is the only state anyone ever runs
    // `bd linear status` in.
    let (dir, ctx) = workspace("status").await;

    let st = Linear.status(&ctx).await.expect("status must not fail");
    assert_eq!(st.name, "linear");
    assert!(!st.configured);
    assert!(
        st.missing.contains(&"linear.team".to_string()),
        "status must say *which* key is missing: {:?}",
        st.missing
    );
    assert!(
        st.detail.as_deref().is_some_and(|d| d.contains("bd config set")),
        "and how to fix it: {:?}",
        st.detail
    );

    // Configured, once the team is set (the token comes from the environment).
    ctx.store()
        .await
        .unwrap()
        .set_config("linear.team", "ENG")
        .await
        .unwrap();
    let st = Linear.status(&ctx).await.expect("status");
    assert!(st.configured, "missing: {:?}", st.missing);
    assert!(st.missing.is_empty());

    std::fs::remove_dir_all(dir).ok();
}

/// The token half, through the binary — the only way to test an *absent* env var
/// without racing every other test in this process.
///
/// Also proves the dispatcher no longer treats linear as a stub: an unimplemented
/// tracker exits 64, and this must exit 0 with an honest report.
#[tokio::test]
async fn bd_linear_status_reports_a_missing_token_without_a_workspace_token() {
    let (dir, ctx) = configured("status-cli").await;
    ctx.close().await;

    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap(), "linear", "status", "--json"])
        .env_remove("LINEAR_API_KEY")
        .output()
        .expect("run bd");

    assert_eq!(out.status.code(), Some(0), "status must never exit non-zero");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status --json emits JSON");
    assert_eq!(v["name"], "linear");
    assert_eq!(v["configured"], false, "the team is set, but the token is not");
    let missing: Vec<&str> = v["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert_eq!(
        missing,
        vec!["$LINEAR_API_KEY"],
        "the token is named as an env var, never as a config key — .beads/config.yaml is committed"
    );

    std::fs::remove_dir_all(dir).ok();
}
