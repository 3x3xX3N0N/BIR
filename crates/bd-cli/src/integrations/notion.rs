//! Notion sync.
//!
//! # Why this one is shaped differently from the other five
//!
//! Jira has a `summary`. GitHub has a `title`. A **Notion database has whatever
//! columns the user drew**, keyed by their display name, each with its own type
//! (`title`, `status`, `select`, `multi_select`, `rich_text`, `number`,
//! `people`, `date`, `checkbox`). There is no `properties.Status` to hard-code —
//! a database whose status column is called "State" is not malformed, it is
//! Tuesday.
//!
//! So the property→field mapping is **configuration**, with defaults that match
//! what Notion itself creates:
//!
//! | key | default | expected Notion type |
//! |---|---|---|
//! | `notion.database_id` | — (required) | — |
//! | `notion.prop.title` | `Name` | `title` |
//! | `notion.prop.status` | `Status` | `status` or `select` |
//! | `notion.prop.priority` | `Priority` | `select` or `number` |
//! | `notion.prop.labels` | `Tags` | `multi_select` |
//! | `notion.prop.description` | `Description` | `rich_text` |
//! | `notion.prop.assignee` | `Assignee` | `people` or `rich_text` |
//!
//! `Name` and `Tags` are Notion's own defaults for a new database, so the
//! zero-config case works on an untouched table. Only `notion.database_id` is
//! required; everything else has a default and is therefore not in
//! [`Tracker::required_config`], which exists to tell you what you *must* set.
//!
//! `bd notion status` prints the resolved mapping, because "which column did it
//! think was the title" is the first question every failure raises.
//!
//! # What is a skip and what is a default
//!
//! - The **title** property is load-bearing: missing, wrong-typed, or empty and
//!   the page is skipped with a reason. A bead with an empty title is worse than
//!   no bead — it is unfindable, and nothing downstream will ever tell you why.
//! - A property that is present but the **wrong type** skips the page. It means
//!   the config points at the wrong column, and guessing would mislabel the
//!   entire import.
//! - A property that is **absent from every page** is not an error per page (a
//!   database is allowed to have no Priority column) but it *is* reported once,
//!   at the end, in `skipped` — because the far likelier cause is a typo in the
//!   config key, and silently importing a backlog that is uniformly P2/open is
//!   exactly the failure this file exists to prevent.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use async_trait::async_trait;
use bd_core::{Issue, IssueFilter, IssueType, Priority, Status};
use bd_storage::{Field, IssuePatch, Storage};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Http, HttpRequest, Method, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

const API: &str = "https://api.notion.com/v1";

/// The header everyone forgets. Notion rejects a request without it, and the
/// error it returns does not mention the version — so the debugging session that
/// follows is spent looking anywhere but here. Every request in this file is
/// built by [`request`], which is the only place either header is set.
const VERSION_HEADER: &str = "Notion-Version";
const NOTION_VERSION: &str = "2022-06-28";

const DB_KEY: &str = "notion.database_id";
const PAGE_SIZE: u32 = 100;

/// Notion rejects a rich-text run longer than this, so long text is split into
/// runs rather than truncated.
const MAX_RUN_CHARS: usize = 2000;

pub struct Notion;

// ---------------------------------------------------------------------------
// Configurable property mapping
// ---------------------------------------------------------------------------

struct Props {
    title: String,
    status: String,
    priority: String,
    labels: String,
    description: String,
    assignee: String,
}

impl Props {
    async fn load(store: &dyn Storage) -> Result<Props> {
        Ok(Props {
            title: cfg(store, "notion.prop.title", "Name").await?,
            status: cfg(store, "notion.prop.status", "Status").await?,
            priority: cfg(store, "notion.prop.priority", "Priority").await?,
            labels: cfg(store, "notion.prop.labels", "Tags").await?,
            description: cfg(store, "notion.prop.description", "Description").await?,
            assignee: cfg(store, "notion.prop.assignee", "Assignee").await?,
        })
    }

    /// The optional ones, paired with the config key that names them — so a
    /// "nothing has this column" report can tell you which key to fix.
    fn optional(&self) -> [(&str, &str); 5] {
        [
            ("notion.prop.status", &self.status),
            ("notion.prop.priority", &self.priority),
            ("notion.prop.labels", &self.labels),
            ("notion.prop.description", &self.description),
            ("notion.prop.assignee", &self.assignee),
        ]
    }
}

async fn cfg(store: &dyn Storage, key: &str, default: &str) -> Result<String> {
    Ok(store
        .get_config(key)
        .await?
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string()))
}

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct QueryResponse {
    #[serde(default)]
    results: Vec<Page>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Page {
    id: String,
    #[serde(default)]
    archived: bool,
    /// The newer spelling of `archived`; Notion sends both on recent API
    /// versions and only one on older ones.
    #[serde(default)]
    in_trash: bool,
    #[serde(default)]
    properties: Map<String, Value>,
    #[serde(default)]
    created_time: Option<DateTime<Utc>>,
    #[serde(default)]
    last_edited_time: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct CreatedPage {
    id: String,
}

#[derive(Deserialize)]
struct DatabaseSchema {
    #[serde(default)]
    properties: Map<String, Value>,
}

/// One column of the user's database, as the API describes it.
struct Column {
    ty: String,
    /// The legal names for a `select` / `status` / `multi_select` column. Push
    /// must choose from these: Notion will not invent a `status` option, and
    /// inventing `select` options behind the user's back is rude.
    options: Vec<String>,
}

/// The single funnel every Notion request goes through, so that neither
/// mandatory header can be forgotten on one code path and remembered on another.
fn request(method: Method, url: &str, token: &str, body: Option<String>) -> HttpRequest {
    HttpRequest {
        method,
        url: url.to_string(),
        headers: Vec::new(),
        body,
    }
    .bearer(token)
    .header(VERSION_HEADER, NOTION_VERSION)
    .json()
}

// ---------------------------------------------------------------------------
// Reading Notion's property values
// ---------------------------------------------------------------------------

fn prop_type(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("unknown")
}

/// Concatenate a rich-text array into one string.
///
/// This is the trap that makes Notion's payloads different from every other
/// tracker's: a `title` is not a string, it is an **array of runs**, one per
/// change of formatting. "Fix **the** parser" is three runs. Reading `[0]` gives
/// you "Fix " and looks perfectly correct on every unformatted title you test
/// with — then silently truncates the ones people bothered to style.
fn rich_text(v: Option<&Value>) -> Option<String> {
    let runs = v?.as_array()?;
    Some(
        runs.iter()
            .filter_map(|r| {
                r.get("plain_text")
                    .and_then(Value::as_str)
                    .or_else(|| r.pointer("/text/content").and_then(Value::as_str))
            })
            .collect(),
    )
}

/// Notion status/select names are prose ("In progress", "Not started"); beads
/// statuses are identifiers. An unrecognized name becomes a `Custom` status
/// rather than a guessed `Open`: beads models user-defined statuses, so carrying
/// the user's own word through is lossless, and inventing `open` is not.
fn status_from_name(name: &str) -> Status {
    let k = name.trim().to_lowercase().replace([' ', '-'], "_");
    match k.as_str() {
        "not_started" | "todo" | "to_do" | "backlog" | "open" | "new" => Status::Open,
        "in_progress" | "doing" | "started" | "wip" => Status::InProgress,
        "blocked" => Status::Blocked,
        "deferred" | "on_hold" | "paused" | "later" => Status::Deferred,
        "done" | "complete" | "completed" | "closed" | "shipped" | "cancelled" | "canceled" => {
            Status::Closed
        }
        _ => Status::Custom(k),
    }
}

/// `None` means "this name does not name a priority". Unlike a status, a
/// priority is a constrained integer, so there is nothing lossless to carry
/// through — the page is skipped rather than assigned a made-up P2.
fn priority_from_name(name: &str) -> Option<Priority> {
    let v = match name.trim().to_lowercase().as_str() {
        "p0" | "critical" | "urgent" | "blocker" | "highest" => 0,
        "p1" | "high" => 1,
        "p2" | "medium" | "normal" | "default" => 2,
        "p3" | "low" => 3,
        "p4" | "trivial" | "minor" | "lowest" => 4,
        _ => return None,
    };
    Priority::new(v).ok()
}

/// A page, mapped onto the fields beads has.
///
/// Every optional field is `Option`: `None` means **the column does not exist on
/// this page**, which is not the same as "the column is empty". The first must
/// leave the local value alone; the second is authoritative and must clear it.
/// Collapsing the two is how a synced description survives forever after being
/// deleted upstream.
struct Mapped {
    page_id: String,
    title: String,
    description: Option<String>,
    assignee: Option<String>,
    status: Option<Status>,
    priority: Option<Priority>,
    labels: Option<Vec<String>>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

/// Look up a property and record that the database actually has it.
fn prop<'a>(page: &'a Page, name: &str, seen: &mut HashSet<String>) -> Option<&'a Value> {
    let v = page.properties.get(name)?;
    seen.insert(name.to_string());
    Some(v)
}

/// `Err` is the human-readable reason this page was skipped. It always names the
/// page and the property, because the user's next move is to fix a config key
/// and they need to know which one.
fn map_page(page: &Page, props: &Props, seen: &mut HashSet<String>) -> Result<Mapped, String> {
    let id = &page.id;
    if page.archived || page.in_trash {
        return Err(format!("page {id}: archived in notion"));
    }

    // --- title: required, and the whole reason a page can be refused ---
    let Some(t) = prop(page, &props.title, seen) else {
        return Err(format!(
            "page {id}: no property named `{}` (notion.prop.title) — \
             point it at the right column with `bd config set notion.prop.title <column>`",
            props.title
        ));
    };
    if prop_type(t) != "title" {
        return Err(format!(
            "page {id}: property `{}` is a {}, not a title (notion.prop.title)",
            props.title,
            prop_type(t)
        ));
    }
    let title = rich_text(t.get("title")).unwrap_or_default();
    if title.trim().is_empty() {
        return Err(format!(
            "page {id}: title property `{}` is empty",
            props.title
        ));
    }

    // --- status ---
    let status = match prop(page, &props.status, seen) {
        None => None,
        Some(v) => match prop_type(v) {
            ty @ ("status" | "select") => v
                .get(ty)
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                .map(status_from_name),
            other => {
                return Err(format!(
                    "page {id}: property `{}` is a {other}, not a status or select \
                     (notion.prop.status)",
                    props.status
                ));
            }
        },
    };

    // --- priority: select of names, or a plain number ---
    let priority = match prop(page, &props.priority, seen) {
        None => None,
        Some(v) => match prop_type(v) {
            "select" => match v
                .pointer("/select/name")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                None => None,
                Some(name) => match priority_from_name(&name) {
                    Some(p) => Some(p),
                    None => {
                        return Err(format!(
                            "page {id}: priority `{name}` is not one of P0-P4 \
                             (or critical/high/medium/low/trivial)"
                        ));
                    }
                },
            },
            "number" => match v.pointer("/number").and_then(Value::as_i64) {
                None => None,
                Some(n) => match i32::try_from(n).ok().and_then(|n| Priority::new(n).ok()) {
                    Some(p) => Some(p),
                    None => {
                        return Err(format!("page {id}: priority {n} is outside P0-P4"));
                    }
                },
            },
            other => {
                return Err(format!(
                    "page {id}: property `{}` is a {other}, not a select or number \
                     (notion.prop.priority)",
                    props.priority
                ));
            }
        },
    };

    // --- labels ---
    let labels = match prop(page, &props.labels, seen) {
        None => None,
        Some(v) => match prop_type(v) {
            "multi_select" => Some(
                v.get("multi_select")
                    .and_then(Value::as_array)
                    .map(|opts| {
                        opts.iter()
                            .filter_map(|o| o.get("name").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            other => {
                return Err(format!(
                    "page {id}: property `{}` is a {other}, not a multi_select \
                     (notion.prop.labels)",
                    props.labels
                ));
            }
        },
    };

    // --- description ---
    let description = match prop(page, &props.description, seen) {
        None => None,
        Some(v) => match prop_type(v) {
            "rich_text" => Some(rich_text(v.get("rich_text")).unwrap_or_default()),
            other => {
                return Err(format!(
                    "page {id}: property `{}` is a {other}, not a rich_text \
                     (notion.prop.description)",
                    props.description
                ));
            }
        },
    };

    // --- assignee: a `people` column, or a text column for teams that keep
    // names rather than Notion accounts.
    let assignee = match prop(page, &props.assignee, seen) {
        None => None,
        Some(v) => match prop_type(v) {
            "people" => Some(
                v.get("people")
                    .and_then(Value::as_array)
                    .and_then(|ps| ps.first())
                    .and_then(|p| {
                        p.get("name")
                            .and_then(Value::as_str)
                            .or_else(|| p.pointer("/person/email").and_then(Value::as_str))
                    })
                    .unwrap_or_default()
                    .to_string(),
            ),
            "rich_text" => Some(rich_text(v.get("rich_text")).unwrap_or_default()),
            other => {
                return Err(format!(
                    "page {id}: property `{}` is a {other}, not people or rich_text \
                     (notion.prop.assignee)",
                    props.assignee
                ));
            }
        },
    };

    Ok(Mapped {
        page_id: id.clone(),
        title,
        description,
        assignee,
        status,
        priority,
        labels,
        created_at: page.created_time,
        updated_at: page.last_edited_time,
    })
}

/// Absent column → keep what we have. Present but empty → the remote says there
/// is nothing there, so clear it. See [`Mapped`].
fn field(v: Option<String>) -> Field<String> {
    match v {
        None => Field::Keep,
        Some(s) if s.is_empty() => Field::Clear,
        Some(s) => Field::Set(s),
    }
}

// ---------------------------------------------------------------------------
// Writing Notion's property values
// ---------------------------------------------------------------------------

/// Split text into rich-text runs no longer than Notion accepts.
fn text_runs(s: &str) -> Value {
    let mut runs = Vec::new();
    let mut buf = String::new();
    let mut n = 0usize;
    for c in s.chars() {
        if n == MAX_RUN_CHARS {
            runs.push(json!({ "text": { "content": std::mem::take(&mut buf) } }));
            n = 0;
        }
        buf.push(c);
        n += 1;
    }
    if !buf.is_empty() {
        runs.push(json!({ "text": { "content": buf } }));
    }
    json!(runs)
}

/// Build the `properties` object for a create or an update.
///
/// The shape of a value depends on the *column's* type, which only the database
/// schema knows — `{"Status": {"select": ...}}` and `{"Status": {"status": ...}}`
/// are both correct and each is rejected for the other kind of column. So push
/// reads the schema first and writes what each column actually is.
///
/// `notes` collects anything deliberately not written. They are deduplicated by
/// the caller: "assignee is not pushed" is one fact, not one fact per issue.
fn page_properties(
    issue: &Issue,
    labels: &[String],
    props: &Props,
    schema: &HashMap<String, Column>,
    notes: &mut Vec<String>,
) -> Map<String, Value> {
    let mut out = Map::new();

    match schema.get(&props.title) {
        Some(c) if c.ty == "title" => {
            out.insert(props.title.clone(), json!({ "title": text_runs(&issue.title) }));
        }
        _ => notes.push(format!(
            "the notion database has no `{}` title column (notion.prop.title); titles were not pushed",
            props.title
        )),
    }

    if let Some(c) = schema.get(&props.status) {
        match c.ty.as_str() {
            ty @ ("status" | "select") => {
                // A `status` column's options cannot be created through the API,
                // and quietly inventing `select` options in someone's database is
                // not ours to do. So we map onto an option that exists, or we say
                // we did not.
                match c.options.iter().find(|o| status_from_name(o) == issue.status) {
                    Some(name) => {
                        // The wrapper key is the column's own type: `status` and
                        // `select` columns take the same option, in differently
                        // named envelopes, and each rejects the other's.
                        let mut val = Map::new();
                        val.insert(ty.to_string(), json!({ "name": name }));
                        out.insert(props.status.clone(), Value::Object(val));
                    }
                    None => notes.push(format!(
                        "no `{}` option maps to status `{}`; status was not pushed",
                        props.status,
                        issue.status.as_str()
                    )),
                }
            }
            other => notes.push(format!(
                "property `{}` is a {other} in notion, not a status or select; status was not pushed",
                props.status
            )),
        }
    }

    if let Some(c) = schema.get(&props.priority) {
        match c.ty.as_str() {
            "select" => match c
                .options
                .iter()
                .find(|o| priority_from_name(o) == Some(issue.priority))
            {
                Some(name) => {
                    out.insert(props.priority.clone(), json!({ "select": { "name": name } }));
                }
                None => notes.push(format!(
                    "no `{}` option maps to {}; priority was not pushed",
                    props.priority, issue.priority
                )),
            },
            "number" => {
                out.insert(
                    props.priority.clone(),
                    json!({ "number": issue.priority.value() }),
                );
            }
            other => notes.push(format!(
                "property `{}` is a {other} in notion, not a select or number; priority was not pushed",
                props.priority
            )),
        }
    }

    if let Some(c) = schema.get(&props.labels)
        && c.ty == "multi_select"
    {
        let opts: Vec<Value> = labels.iter().map(|l| json!({ "name": l })).collect();
        out.insert(props.labels.clone(), json!({ "multi_select": opts }));
    }

    if let Some(c) = schema.get(&props.description)
        && c.ty == "rich_text"
    {
        out.insert(
            props.description.clone(),
            json!({ "rich_text": text_runs(&issue.description) }),
        );
    }

    // Assignee is deliberately never pushed: a Notion `people` column takes user
    // *ids*, and beads stores a name or an email. Writing a guessed id would
    // assign someone else's work to a stranger.
    if !issue.assignee.is_empty()
        && schema.get(&props.assignee).map(|c| c.ty.as_str()) == Some("people")
    {
        notes.push(format!(
            "assignees are not pushed: `{}` is a notion people column, which takes user ids, \
             and beads stores names",
            props.assignee
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// Tracker
// ---------------------------------------------------------------------------

impl Notion {
    fn token(&self) -> Result<String> {
        std::env::var(self.secret_env())
            .ok()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "${} is not set. The token must come from the environment, never from \
                     .beads/config.yaml — that file is committed.",
                    self.secret_env()
                )
            })
    }

    async fn database_id(&self, store: &dyn Storage) -> Result<String> {
        store
            .get_config(DB_KEY)
            .await?
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("{DB_KEY} is not set (`bd config set {DB_KEY} <id>`)"))
    }

    async fn schema(&self, http: &dyn Http, token: &str, db: &str) -> Result<HashMap<String, Column>> {
        let url = format!("{API}/databases/{db}");
        let resp = http.send(request(Method::Get, &url, token, None)).await?;
        let schema: DatabaseSchema = resp.json()?;
        Ok(schema
            .properties
            .into_iter()
            .map(|(name, v)| {
                let ty = prop_type(&v).to_string();
                let options = v
                    .pointer(&format!("/{ty}/options"))
                    .and_then(Value::as_array)
                    .map(|os| {
                        os.iter()
                            .filter_map(|o| o.get("name").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                (name, Column { ty, options })
            })
            .collect())
    }
}

#[async_trait]
impl Tracker for Notion {
    fn name(&self) -> &'static str {
        "notion"
    }

    /// Only the database. The property mapping has defaults, and a key with a
    /// default is not "missing" — reporting it as missing would tell the user to
    /// go set something that is already working.
    fn required_config(&self) -> &'static [&'static str] {
        &[DB_KEY]
    }

    fn secret_env(&self) -> &'static str {
        "NOTION_TOKEN"
    }

    async fn status(&self, ctx: &Ctx) -> Result<TrackerStatus> {
        let store = ctx.store().await?;
        let db = store.get_config(DB_KEY).await?.filter(|v| !v.trim().is_empty());
        let props = Props::load(store).await?;

        let mut missing = Vec::new();
        if db.is_none() {
            missing.push(DB_KEY.to_string());
        }
        if self.token().is_err() {
            missing.push(format!("${}", self.secret_env()));
        }

        // The resolved mapping, not just "configured". With a user-defined
        // schema, "which column did it take for the title" is the first question
        // any failure raises, and this is the cheapest possible answer.
        let detail = Some(format!(
            "database {} · title=`{}` status=`{}` priority=`{}` labels=`{}` description=`{}` assignee=`{}`",
            db.as_deref().unwrap_or("<unset>"),
            props.title,
            props.status,
            props.priority,
            props.labels,
            props.description,
            props.assignee,
        ));

        Ok(TrackerStatus {
            name: self.name().into(),
            configured: missing.is_empty(),
            missing,
            detail,
        })
    }

    async fn pull(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let token = self.token()?;
        let store = ctx.store().await?;
        let db = self.database_id(store).await?;
        let props = Props::load(store).await?;
        let prefix = ctx.prefix().await;

        // The join key, built once. `IssueFilter` cannot express
        // "external_ref = ?", so this is a scan of the workspace — see the note
        // in the report about the seam this wants.
        let local = store.list_issues(&IssueFilter::default()).await?;
        let mut by_page: HashMap<String, String> = HashMap::new();
        for i in &local {
            if i.source_system == self.name()
                && let Some(r) = &i.external_ref
            {
                by_page.insert(r.clone(), i.id.clone());
            }
        }
        let linked: Vec<String> = by_page.values().cloned().collect();
        let mut labels_of: HashMap<String, Vec<String>> =
            store.labels_of(&linked).await?.into_iter().collect();

        let url = format!("{API}/databases/{db}/query");
        let mut report = SyncReport::default();
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut body = json!({ "page_size": PAGE_SIZE });
            if let Some(c) = &cursor {
                body["start_cursor"] = json!(c);
            }
            let resp = http
                .send(request(Method::Post, &url, &token, Some(body.to_string())))
                .await?;
            let page: QueryResponse = resp.json()?;

            for p in &page.results {
                let m = match map_page(p, &props, &mut seen) {
                    Ok(m) => m,
                    Err(reason) => {
                        report.skipped.push(reason);
                        continue;
                    }
                };

                match by_page.get(&m.page_id).cloned() {
                    // The join key found it: UPDATE. Getting this wrong does not
                    // fail loudly, it duplicates the entire backlog once per sync.
                    Some(local_id) => {
                        let patch = IssuePatch {
                            title: Some(m.title),
                            status: m.status,
                            priority: m.priority,
                            description: field(m.description),
                            assignee: field(m.assignee),
                            ..Default::default()
                        };
                        store.update_issue(&local_id, &patch).await?;

                        if let Some(want) = m.labels {
                            let have = labels_of.get(&local_id).cloned().unwrap_or_default();
                            for l in want.iter().filter(|l| !have.contains(l)) {
                                store.add_label(&local_id, l).await?;
                            }
                            for l in have.iter().filter(|l| !want.contains(l)) {
                                store.remove_label(&local_id, l).await?;
                            }
                            labels_of.insert(local_id.clone(), want);
                        }
                        report.updated += 1;
                    }
                    None => {
                        let desc = m.description.clone().unwrap_or_default();
                        let id = store.next_id(&prefix, &m.title, &desc).await?;
                        let now = Utc::now();
                        let issue = Issue {
                            description: desc,
                            assignee: m.assignee.unwrap_or_default(),
                            status: m.status.unwrap_or_default(),
                            priority: m
                                .priority
                                .unwrap_or_else(|| default_priority(ctx)),
                            issue_type: IssueType::from(ctx.config.defaults.issue_type.clone()),
                            labels: m.labels.unwrap_or_default(),
                            // Both halves of the join key, or the next pull
                            // cannot tell this page from a new one.
                            external_ref: Some(m.page_id.clone()),
                            source_system: self.name().to_string(),
                            created_by: ctx.identity.actor.clone(),
                            created_at: m.created_at.unwrap_or(now),
                            updated_at: m.updated_at.unwrap_or(now),
                            ..Issue::new(id.as_str(), m.title.as_str())
                        };
                        store.create_issue(&issue).await?;
                        // A page edited mid-scan can legitimately appear on two
                        // cursor pages. Without this it would be created twice.
                        by_page.insert(m.page_id, id);
                        report.created += 1;
                    }
                }
                report.pulled += 1;
            }

            if !page.has_more {
                break;
            }
            let Some(next) = page.next_cursor else {
                bail!("notion said has_more but sent no next_cursor; refusing to guess");
            };
            // Not paranoia: without this, a cursor that fails to advance (a bad
            // stub, a proxy, a Notion bug) is an infinite loop that writes the
            // same pages forever.
            if cursor.as_deref() == Some(next.as_str()) {
                bail!("notion returned the same cursor twice ({next}); refusing to loop");
            }
            cursor = Some(next);
        }

        // A property nothing has is far more often a typo'd config key than a
        // column the database genuinely lacks — and the symptom of the typo is a
        // backlog that is uniformly open/P2/unlabeled, which looks like a
        // successful import.
        if report.pulled > 0 {
            for (key, name) in props.optional() {
                if !seen.contains(name) {
                    report.skipped.push(format!(
                        "no page has a property named `{name}` ({key}) — \
                         if that column exists under another name, `bd config set {key} <column>`"
                    ));
                }
            }
        }

        Ok(report)
    }

    async fn push(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let token = self.token()?;
        let store = ctx.store().await?;
        let db = self.database_id(store).await?;
        let props = Props::load(store).await?;
        // A property value's shape is determined by the column's type, which is
        // the user's business and not ours to assume. One GET, then we write what
        // each column actually is.
        let schema = self.schema(http, &token, &db).await?;

        let local = store.list_issues(&IssueFilter::default()).await?;
        let mine: Vec<&Issue> = local
            .iter()
            .filter(|i| i.source_system == self.name())
            .collect();
        let ids: Vec<String> = mine.iter().map(|i| i.id.clone()).collect();
        let labels: HashMap<String, Vec<String>> =
            store.labels_of(&ids).await?.into_iter().collect();

        let mut report = SyncReport::default();
        let mut notes: Vec<String> = Vec::new();

        for issue in mine {
            let ls = labels.get(&issue.id).map(Vec::as_slice).unwrap_or(&[]);
            let properties = page_properties(issue, ls, &props, &schema, &mut notes);

            match &issue.external_ref {
                Some(page_id) => {
                    let url = format!("{API}/pages/{page_id}");
                    let body = json!({ "properties": properties }).to_string();
                    let resp = http
                        .send(request(Method::Patch, &url, &token, Some(body)))
                        .await?;
                    // `json` is what turns a 401 into "401: token expired" rather
                    // than a decode error about the shape of nothing.
                    let _: Value = resp.json()?;
                }
                None => {
                    let url = format!("{API}/pages");
                    let body = json!({
                        "parent": { "database_id": db },
                        "properties": properties,
                    })
                    .to_string();
                    let resp = http
                        .send(request(Method::Post, &url, &token, Some(body)))
                        .await?;
                    let created: CreatedPage = resp.json()?;
                    // Record the link immediately. An issue whose page exists but
                    // whose external_ref does not is a duplicate on the next pull.
                    let patch = IssuePatch {
                        external_ref: Field::Set(created.id),
                        ..Default::default()
                    };
                    store.update_issue(&issue.id, &patch).await?;
                }
            }
            report.pushed += 1;
        }

        // The honest limit of push, stated rather than papered over.
        //
        // A locally-authored bead has `source_system = ""`. Creating a page for
        // it would require stamping `source_system = "notion"` back onto the
        // existing issue — and `IssuePatch` has no `source_system` field, so
        // there is no way to. Push it anyway and the next pull, which joins on
        // (external_ref, source_system), would not recognize the page and would
        // create a *second* bead for it. Every sync thereafter would do it again.
        // So: not pushed, and said out loud. See the report — this wants
        // `IssuePatch { source_system: Field<String> }` on the storage seam.
        let unlinked = local.iter().filter(|i| i.source_system.is_empty()).count();
        if unlinked > 0 {
            notes.push(format!(
                "{unlinked} local issue(s) have no notion page and were not created: the store \
                 cannot record `source_system` on an existing issue (IssuePatch has no such \
                 field), and a page beads cannot claim is a duplicate on the next pull"
            ));
        }

        notes.sort();
        notes.dedup();
        report.skipped = notes;
        Ok(report)
    }
}

fn default_priority(ctx: &Ctx) -> Priority {
    Priority::new(ctx.config.defaults.priority).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_split_across_runs_is_concatenated_not_truncated() {
        // Bolding one word splits a Notion title into three runs. `[0]` is the
        // bug this test exists for.
        let v = json!([
            { "plain_text": "Fix " },
            { "plain_text": "the" },
            { "plain_text": " parser" },
        ]);
        assert_eq!(rich_text(Some(&v)).unwrap(), "Fix the parser");
    }

    #[test]
    fn unknown_status_names_are_carried_not_guessed() {
        assert_eq!(status_from_name("In progress"), Status::InProgress);
        assert_eq!(status_from_name("Not started"), Status::Open);
        assert_eq!(status_from_name("Done"), Status::Closed);
        // Beads has user-defined statuses; inventing `open` here would be a lie.
        assert_eq!(
            status_from_name("Needs review"),
            Status::Custom("needs_review".into())
        );
    }

    #[test]
    fn priority_names_that_mean_nothing_map_to_nothing() {
        assert_eq!(priority_from_name("P0"), Some(Priority::CRITICAL));
        assert_eq!(priority_from_name("high"), Some(Priority::HIGH));
        assert_eq!(priority_from_name("Someday"), None);
    }

    #[test]
    fn long_text_is_split_into_runs_notion_will_accept() {
        let long = "x".repeat(MAX_RUN_CHARS * 2 + 5);
        let runs = text_runs(&long);
        let runs = runs.as_array().unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(
            runs[0]["text"]["content"].as_str().unwrap().chars().count(),
            MAX_RUN_CHARS
        );
        assert_eq!(runs[2]["text"]["content"].as_str().unwrap().chars().count(), 5);
    }
}
