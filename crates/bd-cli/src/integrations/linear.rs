//! Linear sync.
//!
//! Linear is GraphQL: one endpoint, one verb. Everything below — paging,
//! creating, updating — is a POST to `https://api.linear.app/graphql` with a
//! `{query, variables}` body. There is no REST surface to fall back on.
//!
//! Three things here will bite anyone who skims:
//!
//! 1. **Auth is the raw key.** `Authorization: lin_api_…`, *not*
//!    `Bearer lin_api_…`. Linear answers a `Bearer`-prefixed API key with a 401
//!    that says "authentication required", which reads exactly like a bad token.
//! 2. **GraphQL errors arrive with HTTP 200.** A failed query is
//!    `{"data": null, "errors": [...]}` under a 200, so checking the status code
//!    alone turns an expired token into "synced 0 issues, all good".
//! 3. **The priority scales collide.** See [`priority_from_linear`]. Copying the
//!    number across inverts urgency.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use bd_core::{Issue, IssueFilter, IssueType, Priority, Status};
use bd_storage::{Field, IssuePatch, Storage};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Http, HttpRequest, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

/// The only URL this module knows.
const ENDPOINT: &str = "https://api.linear.app/graphql";

/// Workspace config key holding the team *key* — the short prefix Linear puts in
/// front of every identifier (`ENG` in `ENG-123`), not the team's UUID. It is
/// the one identifier a human can be expected to type.
const TEAM_KEY: &str = "linear.team";

/// Linear caps `first` at 250; 50 keeps a page small enough to be cheap and
/// large enough that paging is rare.
const PAGE_SIZE: u32 = 50;

/// A backstop on the cursor loop. A server that keeps saying `hasNextPage: true`
/// forever would otherwise hang the CLI with no output at all.
const MAX_PAGES: usize = 500;

pub struct Linear;

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

#[async_trait]
impl Tracker for Linear {
    fn name(&self) -> &'static str {
        "linear"
    }

    fn required_config(&self) -> &'static [&'static str] {
        // The endpoint is fixed (Linear is not self-hosted), so the team is the
        // only thing the workspace has to say. The token is deliberately absent:
        // it comes from the environment, never from `.beads/config.yaml`, which
        // is committed to git in most projects.
        &[TEAM_KEY]
    }

    fn secret_env(&self) -> &'static str {
        "LINEAR_API_KEY"
    }

    /// Never fails on missing configuration — reporting missing configuration is
    /// the entire job. It is also the only verb anyone runs *before* setting the
    /// thing up, so it must survive a workspace whose store will not even open.
    async fn status(&self, ctx: &Ctx) -> Result<TrackerStatus> {
        let team = soft_config(ctx, TEAM_KEY).await;
        let token = secret(self.secret_env()).ok();

        let mut missing = Vec::new();
        if team.is_none() {
            missing.push(TEAM_KEY.to_string());
        }
        if token.is_none() {
            // Named with a `$` so it cannot be mistaken for something to put in
            // the config file.
            missing.push(format!("${}", self.secret_env()));
        }

        let detail = match (&team, token.is_some()) {
            (Some(t), true) => Some(format!("team {t}, token from ${}", self.secret_env())),
            (Some(t), false) => Some(format!(
                "team {t}; export {}=<personal api key> (Settings → API in Linear)",
                self.secret_env()
            )),
            (None, _) => Some(format!(
                "set the team key with `bd config set {TEAM_KEY} ENG` (the prefix in ENG-123)"
            )),
        };

        Ok(TrackerStatus {
            name: self.name().to_string(),
            configured: missing.is_empty(),
            missing,
            detail,
        })
    }

    async fn pull(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let team = require_config(ctx, TEAM_KEY).await?;
        let token = secret(self.secret_env())?;
        let store = ctx.store().await?;
        let prefix = ctx.prefix().await;

        // The join key. `IssueFilter` cannot express "where external_ref = ?", so
        // the index is built once in memory rather than issuing one query per
        // remote issue. See the note on `linked_index` for why an empty
        // source_system is also accepted.
        let mut index = linked_index(store).await?;
        let mut labels = labels_index(store, &index).await?;

        let mut report = SyncReport::default();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let page: PullData = gql(
                http,
                &token,
                PULL_QUERY,
                json!({ "team": team, "after": cursor }),
            )
            .await?;

            for node in page.issues.nodes {
                let local = index.get(&node.id).cloned();
                match local {
                    Some(existing) => {
                        store
                            .update_issue(&existing.id, &patch_from(&node))
                            .await
                            .with_context(|| format!("updating {} from {}", existing.id, node.identifier))?;
                        let want = node.label_names();
                        let have = labels.remove(&existing.id).unwrap_or_default();
                        sync_labels(store, &existing.id, &have, &want).await?;
                        labels.insert(existing.id.clone(), want);
                        report.updated += 1;
                    }
                    None => {
                        let id = store
                            .next_id(&prefix, &node.title, node.description())
                            .await?;
                        let issue = issue_from(&id, &node);
                        store
                            .create_issue(&issue)
                            .await
                            .with_context(|| format!("creating a bead for {}", node.identifier))?;
                        // Index it immediately: a page that repeats an id (or a
                        // fixture that does) must not create the bead twice.
                        index.insert(node.id.clone(), issue.clone());
                        labels.insert(id, issue.labels.clone());
                        report.created += 1;
                    }
                }
                report.pulled += 1;
            }

            let info = page.issues.page_info;
            if !info.has_next_page {
                break;
            }
            // `hasNextPage: true` with no cursor is a server that cannot tell us
            // where to resume. Stopping is the only honest move — asking again
            // with the same cursor would loop forever.
            let Some(next) = info.end_cursor.filter(|c| !c.is_empty()) else {
                report
                    .skipped
                    .push("linear reported another page but returned no cursor".into());
                break;
            };
            if cursor.as_deref() == Some(next.as_str()) {
                report
                    .skipped
                    .push(format!("linear returned the same cursor twice ({next}); stopping"));
                break;
            }
            cursor = Some(next);
        }

        Ok(report)
    }

    async fn push(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let team_key = require_config(ctx, TEAM_KEY).await?;
        let token = secret(self.secret_env())?;
        let store = ctx.store().await?;

        // One round trip buys both halves of what a mutation needs: the team's
        // UUID (`issueCreate` takes `teamId`, never the key) and the workflow
        // state ids (`issueUpdate` takes a `stateId`, never a state *type*).
        let team = resolve_team(http, &token, &team_key).await?;

        let mut report = SyncReport::default();

        for local in store.list_issues(&IssueFilter::default()).await? {
            if let Some(why) = declined(&local) {
                report.skipped.push(format!("{}: {why}", local.id));
                continue;
            }

            let state = state_id_for(&team, &local);
            if state.is_none() && !local.status.is_closed() {
                // Blocked/deferred/pinned/custom have no Linear equivalent.
                // Content still goes up; the remote's state is left alone rather
                // than being flattened into a lie.
                report.skipped.push(format!(
                    "{}: status `{}` has no Linear workflow state; pushed everything else",
                    local.id, local.status
                ));
            }

            let mut input = json!({
                "title": local.title,
                "description": local.description,
                "priority": priority_to_linear(local.priority),
            });
            if let Some(sid) = &state {
                input["stateId"] = json!(sid);
            }

            // Already linked → update in place. Otherwise it is a local bead
            // Linear has never seen, and we create it.
            let linked = local
                .external_ref
                .as_deref()
                .filter(|_| ours(&local))
                .map(str::to_string);

            let payload: MutationResult = match &linked {
                Some(remote_id) => {
                    let d: UpdateData = gql(
                        http,
                        &token,
                        UPDATE_MUTATION,
                        json!({ "id": remote_id, "input": input }),
                    )
                    .await?;
                    d.issue_update
                }
                None => {
                    input["teamId"] = json!(team.id);
                    let d: CreateData =
                        gql(http, &token, CREATE_MUTATION, json!({ "input": input })).await?;
                    d.issue_create
                }
            };

            // Linear can refuse a single mutation without raising a GraphQL
            // error. That is a declined record, not a transport failure: say so
            // and keep going, but never count it as pushed.
            if !payload.success {
                report
                    .skipped
                    .push(format!("{}: linear refused the mutation", local.id));
                continue;
            }
            report.pushed += 1;

            if linked.is_none()
                && let Some(remote) = payload.issue
            {
                // Record the link, or the next pull sees a brand-new remote issue
                // and creates a *second* local bead for the one we just pushed.
                //
                // Only half of the join key can be written here: `IssuePatch` has
                // an `external_ref` field and no `source_system` field, so a bead
                // created by push keeps an empty source_system forever. `pull`
                // compensates by treating an empty source_system as a match — see
                // `linked_index`. The real fix is a `source_system: Field<String>`
                // on `IssuePatch`, which lives in a frozen file.
                store
                    .update_issue(
                        &local.id,
                        &IssuePatch {
                            external_ref: Field::Set(remote.id.clone()),
                            metadata: Field::Set(remote.metadata()),
                            ..Default::default()
                        },
                    )
                    .await
                    .with_context(|| format!("recording the linear ref on {}", local.id))?;
            }
        }

        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// Config and credentials
// ---------------------------------------------------------------------------

/// Config, or `None` — including when there is no workspace at all. Only
/// [`Tracker::status`] may use this: everywhere else a missing store is a real
/// error and must not be swallowed.
async fn soft_config(ctx: &Ctx, key: &str) -> Option<String> {
    let store = ctx.store().await.ok()?;
    store
        .get_config(key)
        .await
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty())
}

async fn require_config(ctx: &Ctx, key: &str) -> Result<String> {
    soft_config(ctx, key)
        .await
        .ok_or_else(|| anyhow!("linear is not configured: set it with `bd config set {key} <value>`"))
}

/// The token, from the environment and nowhere else.
fn secret(var: &str) -> Result<String> {
    let v = std::env::var(var).unwrap_or_default();
    if v.trim().is_empty() {
        bail!("${var} is not set (Linear: Settings → Security & access → Personal API keys)");
    }
    Ok(v)
}

/// Linear takes a **personal API key raw**, with no scheme. OAuth access tokens
/// are the one exception and do want `Bearer`. Sending `Bearer lin_api_…` gets a
/// 401 that blames the token rather than the prefix, which is a long afternoon.
fn auth_header(token: &str) -> String {
    if token.starts_with("lin_oauth_") {
        format!("Bearer {token}")
    } else {
        token.to_string()
    }
}

// ---------------------------------------------------------------------------
// GraphQL
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GqlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Deserialize)]
struct GqlError {
    message: String,
}

/// One POST, one envelope. Every request in this module goes through here.
async fn gql<T: serde::de::DeserializeOwned>(
    http: &dyn Http,
    token: &str,
    query: &str,
    variables: Value,
) -> Result<T> {
    let body = serde_json::to_string(&json!({ "query": query, "variables": variables }))?;
    let resp = http
        .send(
            HttpRequest::post(ENDPOINT, body)
                .json()
                .header("Authorization", auth_header(token)),
        )
        .await?;

    // `HttpResponse::json` already refuses a non-2xx with the body attached. The
    // check that matters is the next one: Linear reports query and auth failures
    // *inside* a 200. Deserializing straight into `PullData` would see a null
    // `issues` field, fail with "unexpected response shape", and bury the real
    // message ("Authentication required") that was sitting right there.
    let env: GqlEnvelope<T> = resp.json()?;
    if !env.errors.is_empty() {
        let msgs: Vec<_> = env.errors.iter().map(|e| e.message.as_str()).collect();
        bail!("linear: {}", msgs.join("; "));
    }
    env.data
        .ok_or_else(|| anyhow!("linear returned neither data nor errors"))
}

const PULL_QUERY: &str = r#"
query BdPull($team: String!, $after: String) {
  issues(first: 50, after: $after, filter: { team: { key: { eq: $team } } }) {
    nodes {
      id
      identifier
      title
      description
      priority
      url
      createdAt
      updatedAt
      completedAt
      canceledAt
      state { name type }
      assignee { email name }
      labels { nodes { name } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

const TEAM_QUERY: &str = r#"
query BdTeam($team: String!) {
  teams(first: 1, filter: { key: { eq: $team } }) {
    nodes {
      id
      key
      states { nodes { id name type position } }
    }
  }
}"#;

const CREATE_MUTATION: &str = r#"
mutation BdCreate($input: IssueCreateInput!) {
  issueCreate(input: $input) {
    success
    issue { id identifier url }
  }
}"#;

const UPDATE_MUTATION: &str = r#"
mutation BdUpdate($id: String!, $input: IssueUpdateInput!) {
  issueUpdate(id: $id, input: $input) {
    success
    issue { id identifier url }
  }
}"#;

// `first: 50` in PULL_QUERY is PAGE_SIZE spelled out — GraphQL has no way to
// interpolate a constant into a literal query, and a variable for it would be
// one more thing to get out of step. This keeps them honest.
const _: () = assert!(PAGE_SIZE == 50);

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PullData {
    issues: IssueConnection,
}

#[derive(Deserialize)]
struct IssueConnection {
    #[serde(default)]
    nodes: Vec<LinearIssue>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize, Default)]
struct PageInfo {
    #[serde(default, rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(default, rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Deserialize, Clone)]
struct LinearIssue {
    /// The stable UUID. This is the join key and the handle every mutation
    /// takes; `identifier` is display sugar and *moves* if the issue changes
    /// team, so it must not be what we key on.
    id: String,
    #[serde(default)]
    identifier: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    /// Linear declares this `Float!`, not `Int`. Deserializing into an integer
    /// works right up until the API sends `2.0`.
    #[serde(default)]
    priority: Option<f64>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "createdAt")]
    created_at: Option<DateTime<Utc>>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default, rename = "completedAt")]
    completed_at: Option<DateTime<Utc>>,
    #[serde(default, rename = "canceledAt")]
    canceled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    state: Option<LinearState>,
    #[serde(default)]
    assignee: Option<LinearUser>,
    #[serde(default)]
    labels: Option<LabelConnection>,
}

#[derive(Deserialize, Clone)]
struct LinearState {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "type")]
    kind: String,
}

#[derive(Deserialize, Clone, Default)]
struct LinearUser {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize, Clone, Default)]
struct LabelConnection {
    #[serde(default)]
    nodes: Vec<LinearLabel>,
}

#[derive(Deserialize, Clone)]
struct LinearLabel {
    name: String,
}

#[derive(Deserialize)]
struct TeamData {
    teams: TeamConnection,
}

#[derive(Deserialize)]
struct TeamConnection {
    #[serde(default)]
    nodes: Vec<LinearTeam>,
}

#[derive(Deserialize)]
struct LinearTeam {
    id: String,
    #[serde(default)]
    states: StateConnection,
}

#[derive(Deserialize, Default)]
struct StateConnection {
    #[serde(default)]
    nodes: Vec<WorkflowState>,
}

#[derive(Deserialize)]
struct WorkflowState {
    id: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    position: Option<f64>,
}

#[derive(Deserialize)]
struct CreateData {
    #[serde(rename = "issueCreate")]
    issue_create: MutationResult,
}

#[derive(Deserialize)]
struct UpdateData {
    #[serde(rename = "issueUpdate")]
    issue_update: MutationResult,
}

#[derive(Deserialize)]
struct MutationResult {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    issue: Option<MutatedIssue>,
}

#[derive(Deserialize)]
struct MutatedIssue {
    id: String,
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    url: Option<String>,
}

impl MutatedIssue {
    fn metadata(&self) -> Value {
        json!({ "linear": { "identifier": self.identifier, "url": self.url } })
    }
}

impl LinearIssue {
    fn description(&self) -> &str {
        self.description.as_deref().unwrap_or_default()
    }

    fn label_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .labels
            .as_ref()
            .map(|l| l.nodes.iter().map(|n| n.name.clone()).collect())
            .unwrap_or_default();
        v.sort();
        v.dedup();
        v
    }

    /// Who holds it. Email first: it is what `bd assign` and git agree on, so an
    /// assignee that round-trips is one whose name is an email.
    fn assignee(&self) -> Option<String> {
        let u = self.assignee.as_ref()?;
        u.email
            .clone()
            .or_else(|| u.name.clone())
            .filter(|s| !s.is_empty())
    }

    fn state_kind(&self) -> &str {
        self.state.as_ref().map(|s| s.kind.as_str()).unwrap_or("")
    }

    /// The human-readable state name, kept only in metadata. Beads has no place
    /// for "In Review" and inventing a `Status::Custom` for every workflow state
    /// a team has drawn would make `bd ready` depend on Linear's board layout.
    fn metadata(&self) -> Value {
        json!({
            "linear": {
                "identifier": self.identifier,
                "url": self.url,
                "state": self.state.as_ref().map(|s| s.name.clone()),
            }
        })
    }

    fn closed_at(&self) -> Option<DateTime<Utc>> {
        // Linear stamps exactly one of these, matching the state type.
        self.completed_at.or(self.canceled_at).or(self.updated_at)
    }
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

/// Linear's workflow state *type* → a beads status, plus the reason to close with.
///
/// **`canceled` is not `completed`.** Both are terminal, and beads has one
/// terminal status, so the distinction survives only in `close_reason` — which
/// is not decoration: `bd_core::is_failure_close` reads it, and a
/// `conditional-blocks` edge ("run B only if A failed") releases or does not
/// release based on the answer. Closing a canceled issue as "done" tells the
/// graph the work succeeded, and the failure-path work never becomes ready.
///
/// `backlog` and `unstarted` both land on `Open`. Beads' `Deferred` was the
/// tempting alternative for `backlog`, but it means "deliberately not claimable",
/// and a backlog item is claimable — it is just untriaged.
fn status_from_state(kind: &str) -> (Status, Option<&'static str>) {
    match kind {
        "started" => (Status::InProgress, None),
        "completed" => (Status::Closed, Some("completed")),
        // `is_failure_close("canceled") == true`, which is the whole point.
        "canceled" | "cancelled" => (Status::Closed, Some("canceled")),
        // "triage" and "backlog" are both "nobody has started this".
        _ => (Status::Open, None),
    }
}

/// Linear priority → beads priority.
///
/// The two scales use the same digits and mean different things:
///
/// | Linear        | means         | beads |
/// |---------------|---------------|-------|
/// | 0             | *No priority* | P2    |
/// | 1             | Urgent        | P0    |
/// | 2             | High          | P1    |
/// | 3             | Medium        | P2    |
/// | 4             | Low           | P3    |
///
/// Copying the number across is the bug this function exists to prevent: it
/// would make Linear's 0 — *unset* — into beads' P0, the most critical thing in
/// the workspace, for every untriaged issue in the team. And it would demote
/// Urgent (1) to P1 while promoting Low (4) to P4/trivial.
///
/// Linear has no rung below Low, so beads' P4 is never produced by a pull.
fn priority_from_linear(p: Option<f64>) -> Priority {
    match p.unwrap_or(0.0).round() as i64 {
        1 => Priority::CRITICAL,
        2 => Priority::HIGH,
        3 => Priority::NORMAL,
        4 => Priority::LOW,
        // 0 = "No priority". It is the *absence* of a priority, so it maps to the
        // beads default, never to P0.
        _ => Priority::NORMAL,
    }
}

/// The inverse. P4 (trivial) clamps to Low: Linear's 0 would *erase* the
/// priority rather than express a lower one, so pushing it would quietly
/// untriage the issue.
fn priority_to_linear(p: Priority) -> i64 {
    match p.value() {
        0 => 1, // critical → Urgent
        1 => 2, // high     → High
        2 => 3, // normal   → Medium
        _ => 4, // low, trivial → Low
    }
}

/// Linear has no issue type, so it is inferred from labels. Anything unlabelled
/// is a task, which is beads' default anyway.
fn issue_type_from_labels(labels: &[String]) -> IssueType {
    for l in labels {
        match l.to_lowercase().as_str() {
            "bug" | "defect" => return IssueType::Bug,
            "feature" | "enhancement" => return IssueType::Feature,
            "chore" => return IssueType::Chore,
            "epic" => return IssueType::Epic,
            "spike" => return IssueType::Spike,
            _ => {}
        }
    }
    IssueType::Task
}

fn issue_from(id: &str, node: &LinearIssue) -> Issue {
    let (status, reason) = status_from_state(node.state_kind());
    let labels = node.label_names();
    let now = Utc::now();

    Issue {
        id: id.to_string(),
        title: node.title.clone(),
        description: node.description().to_string(),
        status: status.clone(),
        priority: priority_from_linear(node.priority),
        issue_type: issue_type_from_labels(&labels),
        assignee: node.assignee().unwrap_or_default(),
        created_at: node.created_at.unwrap_or(now),
        updated_at: node.updated_at.unwrap_or(now),
        closed_at: if status.is_closed() {
            node.closed_at()
        } else {
            None
        },
        close_reason: reason.unwrap_or_default().to_string(),
        // Half of the join key each. Without both, the next pull cannot tell an
        // issue it already has from a new one, and duplicates the backlog.
        external_ref: Some(node.id.clone()),
        source_system: "linear".to_string(),
        metadata: Some(node.metadata()),
        labels,
        ..Default::default()
    }
}

/// The same mapping, as a patch. The remote is authoritative for a linked bead,
/// so an assignee that went away upstream is *cleared* rather than kept —
/// `Field::authoritative` exists for exactly this distinction.
fn patch_from(node: &LinearIssue) -> IssuePatch {
    let (status, reason) = status_from_state(node.state_kind());
    let labels = node.label_names();

    IssuePatch {
        title: Some(node.title.clone()),
        description: Field::Set(node.description().to_string()),
        status: Some(status.clone()),
        priority: Some(priority_from_linear(node.priority)),
        issue_type: Some(issue_type_from_labels(&labels)),
        assignee: Field::authoritative(node.assignee()),
        // Reopening upstream must drop the old reason, or a bead that was
        // canceled and then revived still reads as a failure to the graph.
        close_reason: match reason {
            Some(r) if status.is_closed() => Field::Set(r.to_string()),
            _ => Field::Clear,
        },
        metadata: Field::Set(node.metadata()),
        // Re-stamped every pull so a bead whose ref was lost (a bad import, a
        // hand edit) is repaired rather than duplicated.
        external_ref: Field::Set(node.id.clone()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Local identity
// ---------------------------------------------------------------------------

/// Is this bead ours to update?
///
/// `source_system == "linear"` is the real answer. The empty case is the
/// concession described in `push`: a bead that *we* created in Linear carries the
/// external_ref we wrote but an empty source_system, because `IssuePatch` has no
/// field for it. Excluding it here would make every push-created bead come back
/// as a duplicate on the next pull — the exact failure the join key exists to
/// prevent. A bead belonging to another tracker (`jira`, `github`) has a
/// non-empty source_system and is never touched.
fn ours(issue: &Issue) -> bool {
    issue.source_system == "linear" || issue.source_system.is_empty()
}

/// Every local bead that carries a Linear reference, keyed by that reference.
///
/// One full listing rather than a query per remote issue: `IssueFilter` has no
/// `external_ref` or `source_system` field, so there is nothing to push down.
async fn linked_index(store: &dyn Storage) -> Result<HashMap<String, Issue>> {
    let mut index = HashMap::new();
    for issue in store.list_issues(&IssueFilter::default()).await? {
        if !ours(&issue) {
            continue;
        }
        if let Some(r) = issue.external_ref.clone().filter(|r| !r.is_empty()) {
            index.insert(r, issue);
        }
    }
    Ok(index)
}

/// Labels for everything in the index, in one query rather than N.
async fn labels_index(
    store: &dyn Storage,
    index: &HashMap<String, Issue>,
) -> Result<HashMap<String, Vec<String>>> {
    let ids: Vec<String> = index.values().map(|i| i.id.clone()).collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(store
        .labels_of(&ids)
        .await?
        .into_iter()
        .map(|(id, mut ls)| {
            ls.sort();
            (id, ls)
        })
        .collect())
}

/// Make the local labels match the remote's. `IssuePatch` carries no labels, so
/// this is the only way to keep them in step — and a pulled bead's labels are the
/// remote's to decide.
async fn sync_labels(store: &dyn Storage, id: &str, have: &[String], want: &[String]) -> Result<()> {
    for l in want.iter().filter(|l| !have.contains(l)) {
        store.add_label(id, l).await?;
    }
    for l in have.iter().filter(|l| !want.contains(l)) {
        store.remove_label(id, l).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Push helpers
// ---------------------------------------------------------------------------

/// Why this bead is not going to Linear, if it isn't.
fn declined(issue: &Issue) -> Option<&'static str> {
    if issue.ephemeral {
        return Some("ephemeral, and Linear has no TTL to reap it");
    }
    if issue.is_template {
        return Some("a template, not work");
    }
    if issue.issue_type.excluded_from_ready() {
        return Some("infrastructure bead (molecule/gate/event/message), not an issue");
    }
    // A bead another tracker owns. Pushing it would fork its identity across two
    // systems, and neither would win.
    if !issue.source_system.is_empty() && issue.source_system != "linear" {
        return Some("owned by another tracker");
    }
    None
}

async fn resolve_team(http: &dyn Http, token: &str, key: &str) -> Result<LinearTeam> {
    let data: TeamData = gql(http, token, TEAM_QUERY, json!({ "team": key })).await?;
    data.teams.nodes.into_iter().next().ok_or_else(|| {
        anyhow!("linear has no team with the key `{key}` (check `bd config get {TEAM_KEY}`)")
    })
}

/// The workflow state to move the remote issue to, or `None` when the beads
/// status has no Linear equivalent (blocked, deferred, pinned, custom).
///
/// Among several states of the same type — a team may have "Todo" and "Triage",
/// both `unstarted` — the lowest board position wins, which is the leftmost
/// column and the least surprising place for work to land.
fn state_id_for(team: &LinearTeam, issue: &Issue) -> Option<String> {
    let want: &[&str] = match &issue.status {
        Status::Open => &["unstarted", "backlog", "triage"],
        Status::InProgress => &["started"],
        Status::Closed => {
            // The same asymmetry as on the way in, in reverse: a bead closed with
            // a failure reason is a *canceled* Linear issue, not a completed one.
            if bd_core::types::is_failure_close(&issue.close_reason) {
                &["canceled", "completed"]
            } else {
                &["completed"]
            }
        }
        _ => &[],
    };

    for kind in want {
        let mut candidates: Vec<&WorkflowState> =
            team.states.nodes.iter().filter(|s| s.kind == *kind).collect();
        candidates.sort_by(|a, b| {
            a.position
                .unwrap_or(f64::MAX)
                .total_cmp(&b.position.unwrap_or(f64::MAX))
        });
        if let Some(s) = candidates.first() {
            return Some(s.id.clone());
        }
    }
    None
}
