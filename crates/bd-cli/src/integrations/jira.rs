//! Jira Cloud sync, REST v3.
//!
//! # Three things Jira will get you if you let it
//!
//! 1. **Auth is HTTP Basic, not Bearer.** The credential is
//!    `base64(email:api_token)`. A `Bearer` token gets a 401 whose body talks
//!    about authentication headers and tells you nothing about *why*, and every
//!    hour spent debugging it is spent looking at the token instead of the
//!    scheme. The email is config; the token is `$JIRA_TOKEN`.
//! 2. **`fields.description` is an ADF document, not a string.** Atlassian
//!    Document Format is a nested JSON tree. `description.as_str()` returns
//!    `None`, and a mapper that shrugs at `None` imports every issue with a
//!    blank body — successfully, silently, for the whole backlog. See
//!    [`adf_to_text`].
//! 3. **Pagination is `startAt`/`maxResults`/`total`, and the server may return
//!    fewer than you asked for.** Advance the cursor by the number of issues
//!    that actually came back; a loop that adds its own page size skips
//!    everything Jira trimmed. See [`Jira::search`].
//!
//! # Field mapping
//!
//! | Jira | beads | note |
//! |---|---|---|
//! | `key` (`PROJ-12`) | `external_ref` (+ `source_system = "jira"`) | the join key |
//! | `fields.summary` | `title` | |
//! | `fields.description` (ADF) | `description` | flattened to text |
//! | `fields.status.statusCategory.key` | `status` | `new`→open, `indeterminate`→in_progress, `done`→closed |
//! | `fields.priority.name` | `priority` | Highest…Lowest → P0…P4 |
//! | `fields.issuetype.name` | `issue_type` | Bug/Story/Task/Epic; unknown → custom |
//! | `fields.labels` | `labels` | |
//! | `fields.assignee.emailAddress` | `assignee` | falls back to `displayName` |
//!
//! Status comes from the **category**, never the name. Status *names* are
//! per-project configuration — one project's "In Review" is another's
//! "Reviewing" is another's "QA" — so a name-based match works on the project
//! you tested against and quietly mislabels every other one. The category is a
//! fixed three-value enum and is the only stable thing here.
//!
//! # What push deliberately does not send
//!
//! - **Status.** Jira changes status through the transitions API
//!   (`POST /issue/{key}/transitions` with an id from `GET .../transitions`),
//!   not through a field update. `PUT` with `fields.status` is a 400, not a
//!   silent no-op — so status stays where the remote has it, and an issue closed
//!   locally is reported in [`SyncReport::skipped`] rather than pretended.
//! - **Assignee.** Jira Cloud wants an `accountId`, not an email or a username
//!   (`{"name": ...}` is the Server API and is gone). Resolving one means a user
//!   search per assignee, and guessing wrong silently unassigns people.

use std::collections::HashMap;

use anyhow::{Result, bail};
use async_trait::async_trait;
use bd_core::{Issue, IssueFilter, IssueType, Priority, Status};
use bd_storage::{Field, IssuePatch, Storage};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use super::{Http, HttpRequest, Method, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

/// Jira caps this server-side (typically at 100 for search); asking for more is
/// not an error, it just gets you fewer.
const PAGE: u64 = 100;

pub struct Jira;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Everything a call needs, once we know we have all of it.
struct Conf {
    /// No trailing slash, so `{base}/rest/...` never doubles it.
    base: String,
    project: String,
    email: String,
    token: String,
}

impl Conf {
    /// `Basic base64(email:token)`. Not `Bearer` — see the module docs.
    fn auth(&self) -> String {
        format!("Basic {}", base64(format!("{}:{}", self.email, self.token).as_bytes()))
    }

    fn get(&self, url: &str) -> HttpRequest {
        HttpRequest::get(url)
            .header("Authorization", self.auth())
            .header("Accept", "application/json")
    }

    fn post(&self, url: &str, body: String) -> HttpRequest {
        HttpRequest::post(url, body)
            .header("Authorization", self.auth())
            .header("Accept", "application/json")
            .json()
    }

    fn put(&self, url: &str, body: String) -> HttpRequest {
        HttpRequest {
            method: Method::Put,
            url: url.to_string(),
            headers: Vec::new(),
            body: Some(body),
        }
        .header("Authorization", self.auth())
        .header("Accept", "application/json")
        .json()
    }
}

/// The config this tracker reads, and where each piece comes from.
///
/// The token is read from the environment and *only* from the environment:
/// `.beads/config.yaml` is committed in most workspaces, so a token written
/// there is a token on GitHub.
async fn resolve(ctx: &Ctx) -> (Option<Conf>, Vec<String>) {
    let mut missing = Vec::new();

    // A workspace that cannot even be opened must not make `status` explode: it
    // is the command you run *because* nothing works. No store, nothing set.
    let store = ctx.store().await.ok();

    let url = read(store, "jira.url").await;
    let project = read(store, "jira.project").await;
    let email = read(store, "jira.email").await;
    let token = std::env::var("JIRA_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    if url.is_none() {
        missing.push("jira.url".to_string());
    }
    if project.is_none() {
        missing.push("jira.project".to_string());
    }
    if email.is_none() {
        missing.push("jira.email".to_string());
    }
    if token.is_none() {
        missing.push("$JIRA_TOKEN".to_string());
    }

    let conf = match (url, project, email, token) {
        (Some(url), Some(project), Some(email), Some(token)) => Some(Conf {
            base: url.trim_end_matches('/').to_string(),
            project,
            email,
            token,
        }),
        _ => None,
    };
    (conf, missing)
}

async fn read(store: Option<&dyn Storage>, key: &str) -> Option<String> {
    let v = store?.get_config(key).await.ok().flatten()?;
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// The configuration, or a message naming exactly what is absent.
async fn require(ctx: &Ctx) -> Result<Conf> {
    let (conf, missing) = resolve(ctx).await;
    let Some(conf) = conf else {
        bail!(
            "jira is not configured (missing: {}). Set the keys with `bd config set`, \
             and export the API token as $JIRA_TOKEN — never write it to .beads/config.yaml.",
            missing.join(", ")
        );
    };

    // The project key is interpolated into a JQL clause and into a URL. Jira's
    // own keys are `[A-Z][A-Z0-9_]*`, so anything else is either a typo or an
    // attempt to smuggle a second JQL clause in through the config.
    if conf.project.is_empty()
        || !conf
            .project
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!(
            "jira.project must be a project key like `PROJ` (got `{}`)",
            conf.project
        );
    }
    if !conf.base.starts_with("http://") && !conf.base.starts_with("https://") {
        bail!(
            "jira.url must be the full site URL, e.g. https://acme.atlassian.net (got `{}`)",
            conf.base
        );
    }
    Ok(conf)
}

// ---------------------------------------------------------------------------
// The tracker
// ---------------------------------------------------------------------------

#[async_trait]
impl Tracker for Jira {
    fn name(&self) -> &'static str {
        "jira"
    }

    fn required_config(&self) -> &'static [&'static str] {
        &["jira.url", "jira.project", "jira.email"]
    }

    fn secret_env(&self) -> &'static str {
        "JIRA_TOKEN"
    }

    async fn status(&self, ctx: &Ctx) -> Result<TrackerStatus> {
        let (conf, missing) = resolve(ctx).await;
        Ok(TrackerStatus {
            name: "jira".into(),
            configured: missing.is_empty(),
            missing,
            // Never the string "not implemented yet": `commands::sync::tracker`
            // reads that as "this tracker is a stub" and exits 64.
            detail: conf.map(|c| format!("project {} at {} as {}", c.project, c.base, c.email)),
        })
    }

    async fn pull(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let conf = require(ctx).await?;
        let store = ctx.store().await?;
        let prefix = ctx.prefix().await;

        let remote = self.search(&conf, http).await?;

        // The join key, built once: (source_system, external_ref) -> local id.
        // Without this every pull re-creates the whole backlog.
        let local = store.list_issues(&IssueFilter::default()).await?;
        let by_key = index_by_key(&local, &conf.project);
        let ids: Vec<String> = local.iter().map(|i| i.id.clone()).collect();
        let labels: HashMap<String, Vec<String>> = store.labels_of(&ids).await?.into_iter().collect();

        let mut report = SyncReport {
            pulled: remote.len() as u64,
            ..Default::default()
        };

        for raw in &remote {
            let Some(m) = Mapped::from(raw) else {
                report
                    .skipped
                    .push(format!("a Jira record with no key or no summary: {}", brief(raw)));
                report.pulled -= 1;
                continue;
            };

            match by_key.get(&m.key) {
                Some(id) => {
                    store.update_issue(id, &m.patch()).await?;
                    let have = labels.get(id).cloned().unwrap_or_default();
                    for l in m.labels.iter().filter(|l| !have.contains(l)) {
                        store.add_label(id, l).await?;
                    }
                    for l in have.iter().filter(|l| !m.labels.contains(l)) {
                        store.remove_label(id, l).await?;
                    }
                    report.updated += 1;
                }
                None => {
                    let id = store.next_id(&prefix, &m.title, &m.description).await?;
                    store.create_issue(&m.issue(id)).await?;
                    report.created += 1;
                }
            }
        }

        Ok(report)
    }

    async fn push(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let conf = require(ctx).await?;
        let store = ctx.store().await?;

        let local = store.list_issues(&IssueFilter::default()).await?;
        let ids: Vec<String> = local.iter().map(|i| i.id.clone()).collect();
        let labels: HashMap<String, Vec<String>> = store.labels_of(&ids).await?.into_iter().collect();

        let mut report = SyncReport::default();

        for issue in &local {
            // Wisps, molecules and audit events are bookkeeping. Filing them as
            // Jira tickets would flood the project with beads' own plumbing.
            if issue.ephemeral || issue.is_template || issue.issue_type.excluded_from_ready() {
                continue;
            }
            // An issue that came from another tracker is that tracker's to sync.
            if !issue.source_system.is_empty() && issue.source_system != "jira" {
                report.skipped.push(format!(
                    "{}: belongs to {}, not jira",
                    issue.id, issue.source_system
                ));
                continue;
            }

            let mut mine = issue.labels.clone();
            if mine.is_empty() {
                mine = labels.get(&issue.id).cloned().unwrap_or_default();
            }
            // Jira rejects a label containing whitespace outright (400 on the
            // whole request), so one bad label would otherwise fail the issue.
            let (pushable, spaced): (Vec<String>, Vec<String>) = mine
                .into_iter()
                .partition(|l| !l.chars().any(char::is_whitespace));
            for l in &spaced {
                report.skipped.push(format!(
                    "{}: label `{l}` not pushed — Jira labels cannot contain whitespace",
                    issue.id
                ));
            }

            match linked_key(issue, &conf.project) {
                Some(key) => {
                    let url = format!("{}/rest/api/3/issue/{key}", conf.base);
                    let body = json!({ "fields": push_fields(issue, &pushable, None) });
                    let resp = http.send(conf.put(&url, body.to_string())).await?;
                    if !resp.ok() {
                        bail!("cannot update {key}: HTTP {} {}", resp.status, resp.body);
                    }
                    if issue.status.is_closed() {
                        report.skipped.push(format!(
                            "{key}: closed locally — Jira status moves through the transitions API, \
                             so it was not pushed"
                        ));
                    }
                    report.pushed += 1;
                }
                None => {
                    let url = format!("{}/rest/api/3/issue", conf.base);
                    let body =
                        json!({ "fields": push_fields(issue, &pushable, Some(&conf.project)) });
                    let resp = http.send(conf.post(&url, body.to_string())).await?;
                    let created: Value = resp.json()?;
                    let Some(key) = created["key"].as_str() else {
                        bail!("Jira accepted {} but returned no key: {}", issue.id, brief(&created));
                    };

                    // Write the remote's id back, or the next pull sees a bead it
                    // has never heard of and files a duplicate.
                    //
                    // NOTE: `IssuePatch` has no `source_system`, so the other half
                    // of the join key cannot be written here. See `linked_key`.
                    store
                        .update_issue(
                            &issue.id,
                            &IssuePatch {
                                external_ref: Field::Set(key.to_string()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    report.pushed += 1;
                }
            }
        }

        Ok(report)
    }
}

impl Jira {
    /// Every issue in the project, following `startAt` to the end.
    ///
    /// A tracker that fetches page one and stops looks *exactly* like one that
    /// works — same shape of report, same absence of errors, just a backlog that
    /// stops at 100. Hence the `requests()` assertion in the test.
    async fn search(&self, conf: &Conf, http: &dyn Http) -> Result<Vec<Value>> {
        let mut all: Vec<Value> = Vec::new();
        let mut start: u64 = 0;

        loop {
            // The JQL is deliberately one clause with no spaces, so it needs no
            // percent-encoding; `require` has already checked the key is
            // alphanumeric.
            let url = format!(
                "{}/rest/api/3/search?jql=project={}&maxResults={PAGE}&startAt={start}",
                conf.base, conf.project
            );
            let page: Value = http.send(conf.get(&url)).await?.json()?;

            let issues = page["issues"].as_array().cloned().unwrap_or_default();
            let got = issues.len() as u64;
            all.extend(issues);

            // Advance by what came back. Jira may return fewer than `maxResults`
            // (it caps the page size itself), and a cursor that advances by the
            // *requested* size skips every issue the server trimmed.
            start += got;
            let total = page["total"].as_u64().unwrap_or(start);
            if got == 0 || start >= total {
                break;
            }
        }
        Ok(all)
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The Jira key this bead is already linked to, if any.
///
/// The contract's join key is the pair (`source_system`, `external_ref`), and a
/// bead we *pulled* carries both. A bead we *pushed* carries only
/// `external_ref`: `IssuePatch` cannot set `source_system`, so after `push`
/// creates a Jira issue there is no way to stamp the system half of the pair.
///
/// So an unstamped bead counts as ours when its ref is shaped like a key in
/// *our* project (`PROJ-123`) — narrow enough that another tracker's ref cannot
/// collide with it, and the only alternative is a pull that duplicates every
/// issue this workspace has ever pushed. The proper fix is a `source_system`
/// field on `IssuePatch`; it is in the report.
fn linked_key(issue: &Issue, project: &str) -> Option<String> {
    let key = issue.external_ref.as_deref()?;
    match issue.source_system.as_str() {
        "jira" => Some(key.to_string()),
        "" if is_key_of(key, project) => Some(key.to_string()),
        _ => None,
    }
}

fn is_key_of(key: &str, project: &str) -> bool {
    key.strip_prefix(project)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

fn index_by_key(local: &[Issue], project: &str) -> HashMap<String, String> {
    local
        .iter()
        .filter_map(|i| linked_key(i, project).map(|k| (k, i.id.clone())))
        .collect()
}

// ---------------------------------------------------------------------------
// Remote -> beads
// ---------------------------------------------------------------------------

struct Mapped {
    key: String,
    title: String,
    description: String,
    status: Status,
    priority: Priority,
    issue_type: IssueType,
    assignee: String,
    labels: Vec<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    closed_at: Option<DateTime<Utc>>,
}

impl Mapped {
    fn from(raw: &Value) -> Option<Mapped> {
        let key = raw["key"].as_str()?.to_string();
        let f = &raw["fields"];
        let title = f["summary"].as_str().unwrap_or("").trim().to_string();
        if title.is_empty() {
            // `Issue::validate` refuses an empty title, and rightly: a bead with
            // no title is unfindable. Skipping it loudly beats failing the sync.
            return None;
        }

        let status = status_of(f);
        Some(Mapped {
            key,
            title,
            description: adf_to_text(&f["description"]),
            priority: priority_of(f["priority"]["name"].as_str()),
            issue_type: type_of(f["issuetype"]["name"].as_str()),
            assignee: f["assignee"]["emailAddress"]
                .as_str()
                .or_else(|| f["assignee"]["displayName"].as_str())
                .unwrap_or("")
                .to_string(),
            labels: f["labels"]
                .as_array()
                .map(|ls| ls.iter().filter_map(|l| l.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            created_at: timestamp(f["created"].as_str()),
            updated_at: timestamp(f["updated"].as_str()),
            closed_at: status
                .is_closed()
                .then(|| timestamp(f["resolutiondate"].as_str()))
                .flatten(),
            status,
        })
    }

    /// A brand-new bead. Both halves of the join key are set here — this is the
    /// only place `source_system` can be written at all.
    fn issue(&self, id: String) -> Issue {
        let mut i = Issue::new(id, self.title.clone());
        i.description = self.description.clone();
        i.status = self.status.clone();
        i.priority = self.priority;
        i.issue_type = self.issue_type.clone();
        i.assignee = self.assignee.clone();
        i.labels = self.labels.clone();
        i.external_ref = Some(self.key.clone());
        i.source_system = "jira".to_string();
        if let Some(t) = self.created_at {
            i.created_at = t;
        }
        if let Some(t) = self.updated_at {
            i.updated_at = t;
        }
        i.closed_at = self.closed_at;
        i
    }

    /// Only the fields Jira actually owns.
    ///
    /// Unlike `bd import`, a tracker is **not** authoritative over the whole
    /// bead: `design`, `notes`, `acceptance_criteria`, the dependency graph and
    /// the defer date are beads' own, and Jira has never heard of them. Clearing
    /// them because the remote did not mention them would delete local work on
    /// every sync.
    fn patch(&self) -> IssuePatch {
        IssuePatch {
            title: Some(self.title.clone()),
            description: Field::Set(self.description.clone()),
            status: Some(self.status.clone()),
            priority: Some(self.priority),
            issue_type: Some(self.issue_type.clone()),
            assignee: Field::Set(self.assignee.clone()),
            external_ref: Field::Set(self.key.clone()),
            ..Default::default()
        }
    }
}

/// The **category**, not the name. See the module docs.
fn status_of(fields: &Value) -> Status {
    match fields["status"]["statusCategory"]["key"].as_str() {
        Some("done") => Status::Closed,
        Some("indeterminate") => Status::InProgress,
        // `new`, and anything a future Jira invents: an unknown category is
        // work that has not started, which is the safe reading — it keeps the
        // issue visible in `bd ready` rather than silently closing it.
        _ => Status::Open,
    }
}

/// `Highest`…`Lowest` → P0…P4, plus the classic five (`Blocker`…`Trivial`) that
/// Jira Server projects still ship with. An unrecognized or absent priority is
/// P2, the same default `bd create` uses.
fn priority_of(name: Option<&str>) -> Priority {
    match name.unwrap_or("").to_ascii_lowercase().as_str() {
        "highest" | "blocker" | "critical" => Priority::CRITICAL,
        "high" | "major" => Priority::HIGH,
        "low" | "minor" => Priority::LOW,
        "lowest" | "trivial" => Priority::TRIVIAL,
        _ => Priority::NORMAL,
    }
}

fn priority_name(p: Priority) -> &'static str {
    match p.value() {
        0 => "Highest",
        1 => "High",
        3 => "Low",
        4 => "Lowest",
        _ => "Medium",
    }
}

fn type_of(name: Option<&str>) -> IssueType {
    match name.unwrap_or("").to_ascii_lowercase().as_str() {
        "bug" => IssueType::Bug,
        "story" => IssueType::Story,
        "epic" => IssueType::Epic,
        // A sub-task is a task with a parent. The parent link lives in the
        // dependency graph, not in the type.
        "task" | "sub-task" | "subtask" => IssueType::Task,
        "improvement" | "new feature" => IssueType::Feature,
        "spike" => IssueType::Spike,
        // A project with custom issue types keeps them, lowercased, rather than
        // having them flattened into `task`.
        other if !other.is_empty() => IssueType::from(other.to_string()),
        _ => IssueType::Task,
    }
}

/// beads types Jira has no idea about (`chore`, `decision`, `spike`, …) become
/// `Task`: filing them under a type the project has not defined is a 400.
fn type_name(t: &IssueType) -> &'static str {
    match t {
        IssueType::Bug => "Bug",
        IssueType::Epic => "Epic",
        IssueType::Story | IssueType::Feature => "Story",
        _ => "Task",
    }
}

/// Jira stamps `2024-01-15T10:30:00.000+0000` — RFC 3339 except for the missing
/// colon in the offset, which `parse_from_rfc3339` rejects. Both spellings are
/// in the wild, so try both.
fn timestamp(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?;
    DateTime::parse_from_rfc3339(s)
        .or_else(|_| DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z"))
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// ADF
// ---------------------------------------------------------------------------

/// Flatten an Atlassian Document Format tree to plain text.
///
/// **This is the bug most Jira integrations ship with.** `fields.description` in
/// REST v3 is a document, not a string:
///
/// ```json
/// {"type":"doc","version":1,"content":[
///   {"type":"paragraph","content":[{"type":"text","text":"Steps"}]}]}
/// ```
///
/// `.as_str()` on that is `None`, and the natural `.unwrap_or_default()` turns
/// the whole backlog's descriptions into empty strings without a single error.
///
/// The projection is lossy on purpose — beads stores markdown-ish text, not a
/// document tree — but it must never lose the *words*. Anything it does not
/// recognize recurses into `content`, so a node type Atlassian adds next year
/// still yields its text instead of vanishing.
fn adf_to_text(node: &Value) -> String {
    let mut raw = String::new();
    walk(node, &mut raw);

    // Nested blocks each end with a newline, so a list inside a panel inside a
    // doc can stack up a dozen of them. Two is a paragraph break; more is noise.
    let mut out = String::with_capacity(raw.len());
    let mut runs = 0;
    for c in raw.chars() {
        if c == '\n' {
            runs += 1;
            if runs > 2 {
                continue;
            }
        } else {
            runs = 0;
        }
        out.push(c);
    }
    out.trim().to_string()
}

fn walk(node: &Value, out: &mut String) {
    match node {
        // A v2 description, or a workspace that predates ADF: already text.
        Value::String(s) => out.push_str(s),
        Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
        Value::Object(o) => {
            match o.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => out.push_str(o.get("text").and_then(Value::as_str).unwrap_or("")),
                "hardBreak" => out.push('\n'),
                // The text of a mention already carries its `@`.
                "mention" => out.push_str(attr(o, "text").unwrap_or("@unknown")),
                "emoji" => out.push_str(attr(o, "text").or_else(|| attr(o, "shortName")).unwrap_or("")),
                "inlineCard" | "blockCard" | "embedCard" => out.push_str(attr(o, "url").unwrap_or("")),
                "media" => out.push_str(attr(o, "id").unwrap_or("")),
                "listItem" | "taskItem" => {
                    out.push_str("- ");
                    children(o, out);
                    out.push('\n');
                }
                "rule" => out.push_str("\n---\n"),
                "paragraph" | "heading" | "codeBlock" | "blockquote" | "panel" => {
                    children(o, out);
                    out.push_str("\n\n");
                }
                // doc, bulletList, orderedList, table rows, and whatever comes
                // next: pass through to the content rather than drop it.
                _ => children(o, out),
            }
        }
        _ => {}
    }
}

fn children(o: &Map<String, Value>, out: &mut String) {
    if let Some(c) = o.get("content") {
        walk(c, out);
    }
}

fn attr<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    o.get("attrs")?.get(key)?.as_str()
}

/// The inverse, for push: v3 rejects a plain string in `description`.
fn text_to_adf(text: &str) -> Value {
    let content: Vec<Value> = text
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                // A paragraph with no content is valid ADF; one with an empty
                // text node is not.
                json!({ "type": "paragraph" })
            } else {
                json!({ "type": "paragraph", "content": [{ "type": "text", "text": line }] })
            }
        })
        .collect();
    json!({ "type": "doc", "version": 1, "content": content })
}

// ---------------------------------------------------------------------------
// beads -> remote
// ---------------------------------------------------------------------------

/// `project` is `Some` on create and `None` on update: Jira refuses to have the
/// project set on an edit.
fn push_fields(issue: &Issue, labels: &[String], project: Option<&str>) -> Value {
    let mut f = Map::new();
    if let Some(key) = project {
        f.insert("project".into(), json!({ "key": key }));
        f.insert("issuetype".into(), json!({ "name": type_name(&issue.issue_type) }));
    }
    f.insert("summary".into(), json!(issue.title));
    f.insert("priority".into(), json!({ "name": priority_name(issue.priority) }));
    if !issue.description.is_empty() {
        f.insert("description".into(), text_to_adf(&issue.description));
    }
    if !labels.is_empty() {
        f.insert("labels".into(), json!(labels));
    }
    Value::Object(f)
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------

/// Standard base64, padded. Hand-rolled because adding a dependency to the
/// workspace manifest is not this file's to make, and Basic auth is the one
/// thing here that cannot be skipped.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Enough of a record to identify it in an error, without pasting a page of JSON.
fn brief(v: &Value) -> String {
    let s = v.to_string();
    if s.len() <= 200 {
        s
    } else {
        format!("{}…", &s[..200])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_is_email_colon_token_base64() {
        // The whole point of Basic: it is the *pair*, not the token.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"user@acme.com:tok"), "dXNlckBhY21lLmNvbTp0b2s=");
    }

    #[test]
    fn adf_survives_nesting() {
        let doc = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "first"},
                    {"type": "hardBreak"},
                    {"type": "text", "text": "second"}
                ]},
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "one"}]}
                    ]}
                ]}
            ]
        });
        let text = adf_to_text(&doc);
        assert!(text.contains("first\nsecond"), "{text:?}");
        assert!(text.contains("- one"), "{text:?}");

        // The trap: a null or a bare string must not blow up or vanish.
        assert_eq!(adf_to_text(&Value::Null), "");
        assert_eq!(adf_to_text(&json!("plain v2 text")), "plain v2 text");
    }

    #[test]
    fn a_key_belongs_to_its_project_only() {
        assert!(is_key_of("PROJ-12", "PROJ"));
        assert!(!is_key_of("PROJECT-12", "PROJ"));
        assert!(!is_key_of("PROJ-", "PROJ"));
        assert!(!is_key_of("OTHER-1", "PROJ"));
        assert!(!is_key_of("12", "PROJ"));
    }
}
