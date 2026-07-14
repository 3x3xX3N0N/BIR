//! Azure DevOps Boards — work items.
//!
//! Four things about this API bite, and every one of them fails quietly:
//!
//! 1. **Auth is HTTP Basic with an *empty username*.** The PAT is the password:
//!    the header is `Basic base64(":{pat}")`. The same PAT sent as a bearer
//!    token gets a 401, which reads as "bad credential" and sends you off to
//!    regenerate a PAT that was never the problem.
//! 2. **Reading is two calls.** A WIQL query returns *ids and nothing else*; the
//!    fields come from a second, batched GET. A one-step implementation
//!    compiles, runs, and reports an empty backlog.
//! 3. **The batch GET caps at 200 ids.** Past that it fails — so it fails for
//!    exactly the people with a backlog worth syncing, and never in a test with
//!    a five-item fixture. Hence [`MAX_BATCH_IDS`] and the chunk loop.
//! 4. **Writing is a JSON Patch document**, `application/json-patch+json`, a
//!    *list* of `{op, path, value}`. A plain JSON object is rejected.
//!
//! # Identity
//!
//! Pulled beads carry `external_ref = <work item id>` and `source_system =
//! "ado"`. That pair is the join key on the next pull: found → update, not found
//! → create. Get it wrong and every sync clones the whole backlog.

use std::collections::HashMap;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use bd_core::{Issue, IssueFilter, IssueType, Priority, Status};
use bd_storage::{Field, IssuePatch};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Http, HttpRequest, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

pub struct Ado;

const NAME: &str = "ado";
const SECRET_ENV: &str = "AZURE_DEVOPS_PAT";
const ORG_KEY: &str = "ado.org";
const PROJECT_KEY: &str = "ado.project";

/// 7.0 is the current GA version. Pinned rather than left off: an unversioned
/// request is rejected outright by every `_apis/` route.
const API_VERSION: &str = "7.0";

/// The hard cap on `?ids=` for the work item batch endpoint. Trap 3.
const MAX_BATCH_IDS: usize = 200;

/// Where push records the id it was given, in the bead's `metadata`.
///
/// This exists only because [`IssuePatch`] has no `source_system` field — see
/// [`is_ours`]. It is a marker, not a mapping anyone should read.
const MARKER: &str = "ado";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

struct Cfg {
    org: String,
    project: String,
    pat: String,
}

impl Cfg {
    /// Everything needed to make a call, or the list of what is not there.
    async fn load(ctx: &Ctx) -> Result<Cfg> {
        let org = config_value(ctx, ORG_KEY).await;
        let project = config_value(ctx, PROJECT_KEY).await;
        let pat = secret();
        match (org, project, pat) {
            (Some(org), Some(project), Some(pat)) => Ok(Cfg { org, project, pat }),
            (org, project, pat) => {
                let mut missing = Vec::new();
                if org.is_none() {
                    missing.push(ORG_KEY);
                }
                if project.is_none() {
                    missing.push(PROJECT_KEY);
                }
                if pat.is_none() {
                    missing.push(SECRET_ENV);
                }
                bail!("ado is not configured (missing: {})", missing.join(", "))
            }
        }
    }

    fn base(&self) -> String {
        format!(
            "https://dev.azure.com/{}/{}/_apis/wit",
            self.org, self.project
        )
    }
}

/// A config key, or `None`. Never an error: `status` exists to report a broken
/// setup, so it must survive a store it cannot even open.
async fn config_value(ctx: &Ctx, key: &str) -> Option<String> {
    let store = ctx.store().await.ok()?;
    store
        .get_config(key)
        .await
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The PAT, from the environment and nowhere else. `.beads/config.yaml` is
/// committed in most repos, so a token that can live there will eventually live
/// on GitHub.
fn secret() -> Option<String> {
    std::env::var(SECRET_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Auth (trap 1)
// ---------------------------------------------------------------------------

/// `Basic base64(":{pat}")` — empty username, PAT as password.
///
/// This is the whole trick, and it is the single most-gotten-wrong thing about
/// the ADO API. Do not "fix" this into a bearer token.
fn basic_auth(pat: &str) -> String {
    format!("Basic {}", b64(format!(":{pat}").as_bytes()))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64, padded. Hand-rolled because adding a dependency would mean
/// editing the workspace manifest, which this wave does not own — and it is
/// twelve lines.
fn b64(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(c.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(c.get(2).copied().unwrap_or(0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn get(cfg: &Cfg, url: String) -> HttpRequest {
    HttpRequest::get(url)
        .header("Authorization", basic_auth(&cfg.pat))
        .header("Accept", "application/json")
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// WIQL gives back references — `id` and a `url`. No fields. Ever. (Trap 2.)
#[derive(Debug, Deserialize)]
struct WiqlResult {
    #[serde(default, rename = "workItems")]
    work_items: Vec<WiqlRef>,
}

#[derive(Debug, Deserialize)]
struct WiqlRef {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct Batch {
    #[serde(default)]
    value: Vec<WorkItem>,
}

#[derive(Debug, Deserialize)]
struct WorkItem {
    id: i64,
    /// Dotted keys: `System.Title`, `Microsoft.VSTS.Common.Priority`, … A
    /// customized process can add any field it likes, so this stays a map.
    #[serde(default)]
    fields: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct Created {
    id: i64,
}

fn text(fields: &Map<String, Value>, key: &str) -> String {
    fields
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

/// `System.State` → beads status.
///
/// The states are **process-template dependent**, not API constants: Agile says
/// New/Active/Resolved/Closed, Scrum says New/Approved/Committed/Done/Removed,
/// CMMI says Proposed/Active/Resolved/Closed — and a customized process can say
/// anything at all. So this matches what it knows and **falls back to open**.
///
/// The fallback direction is the whole point. A closed bead vanishes from
/// `bd ready` and is never looked at again; an open one that should have been
/// closed costs somebody a glance. Guessing "closed" for a state we do not
/// recognize would silently delete work from the queue.
fn map_state(state: &str) -> Status {
    match state.trim().to_ascii_lowercase().as_str() {
        // Terminal in the stock templates. "Removed" is Scrum for "we are not
        // doing this" — terminal, not a backlog state.
        "closed" | "done" | "removed" | "completed" | "cut" => Status::Closed,
        // In flight. "Resolved" (Agile/CMMI) is dev-complete but *unverified*:
        // a bug that fails verification goes straight back to Active, so calling
        // it closed here would close it and then never reopen it.
        "active" | "committed" | "doing" | "in progress" | "inprogress" | "resolved" => {
            Status::InProgress
        }
        "new" | "approved" | "proposed" | "to do" | "open" => Status::Open,
        // Unknown — including every custom state anyone has ever added.
        _ => Status::Open,
    }
}

fn map_type(work_item_type: &str) -> IssueType {
    match work_item_type.trim().to_ascii_lowercase().as_str() {
        "bug" | "defect" => IssueType::Bug,
        "epic" => IssueType::Epic,
        "feature" => IssueType::Feature,
        "user story" | "product backlog item" | "requirement" | "story" => IssueType::Story,
        "task" | "issue" => IssueType::Task,
        // A custom work item type is carried through rather than flattened into
        // Task: flattening loses it on the way in and then pushes it back as the
        // wrong thing.
        other => IssueType::from(other.to_string()),
    }
}

/// `Microsoft.VSTS.Common.Priority` (1 = highest … 4 = lowest) → beads P0–P4.
///
/// Four levels into five, so the mapping has to lose something and the only
/// question is *what*. It is pinned on the **defaults**: ADO's default priority
/// is 2 and beads' default is P2, so 2 → P2. Sliding the scale instead
/// (1→P0, 2→P1, 3→P2, 4→P3) would make every single work item in a stock ADO
/// project outrank every locally-filed bead, and `bd ready` sorts on priority.
///
/// P1 is therefore unreachable *from* ADO, which is honest: ADO has no level
/// that means "high but not critical".
fn map_priority(fields: &Map<String, Value>) -> Priority {
    match fields
        .get("Microsoft.VSTS.Common.Priority")
        .and_then(Value::as_i64)
    {
        Some(1) => Priority::CRITICAL,
        Some(2) => Priority::NORMAL,
        Some(3) => Priority::LOW,
        Some(4) => Priority::TRIVIAL,
        // Absent, or a value outside 1–4 that a customized field allowed.
        _ => Priority::NORMAL,
    }
}

/// beads P0–P4 → ADO 1–4, the inverse of [`map_priority`] where one exists.
///
/// P1 has no ADO level of its own, and it rounds **up** to 1 rather than down to
/// 2: telling the remote something is less urgent than it is, is the more
/// expensive mistake. The consequence, stated so nobody has to discover it: a
/// P1 bead pushed and then pulled back comes home as P0.
fn ado_priority(p: Priority) -> i64 {
    match p.value() {
        0 | 1 => 1,
        2 => 2,
        3 => 3,
        _ => 4,
    }
}

/// beads type → ADO work item type, for the `POST .../workitems/${type}` route.
///
/// Task is the fallback because every stock process template defines it. Note
/// that "User Story" is **Agile-only** — Scrum calls the same thing a "Product
/// Backlog Item" — so pushing a story-typed bead to a Scrum project fails with
/// a 400 naming the type. That error is surfaced rather than swallowed: quietly
/// retyping a story as a Task would put it in the wrong place in their backlog,
/// forever, and nothing would say so.
fn work_item_type(t: &IssueType) -> &'static str {
    match t {
        IssueType::Bug => "Bug",
        IssueType::Epic => "Epic",
        IssueType::Feature => "Feature",
        IssueType::Story => "User Story",
        _ => "Task",
    }
}

// ---------------------------------------------------------------------------
// The join key
// ---------------------------------------------------------------------------

/// The work item id this bead is bound to, if it is bound to one of ours.
///
/// The contract is `(source_system == "ado", external_ref)`. The second arm is
/// **a workaround for a gap in the storage seam**: [`IssuePatch`] has no
/// `source_system` field, so `push` — which updates an existing local bead — can
/// stamp `external_ref` and nothing else. Without a way to recognize those
/// beads, the very next pull would see an id it does not know and clone the
/// backlog it had just pushed. So push also drops a marker in `metadata`, and
/// this reads it.
///
/// Matching on "external_ref set, source_system empty" *without* the marker
/// would have been simpler and wrong: work item 42 and GitHub issue 42 are both
/// bare numbers, and a pull would then walk into the other tracker's bead.
///
/// **Delete this arm** the day `IssuePatch` grows `source_system: Field<String>`
/// — it is dead weight the moment push can stamp the field properly.
fn is_ours(i: &Issue) -> Option<String> {
    if i.source_system == NAME {
        return i.external_ref.clone();
    }
    if i.source_system.is_empty()
        && let Some(m) = i.metadata.as_ref()
        && let Some(id) = m.get(MARKER).and_then(|v| v.get("work_item_id"))
    {
        let id = id.as_str().map(str::to_string)?;
        // The marker and the ref must agree; if a human has since edited one of
        // them, trust neither.
        return i.external_ref.clone().filter(|r| *r == id);
    }
    None
}

/// `metadata` with the push marker merged in, preserving whatever else is there.
fn with_marker(i: &Issue, work_item_id: i64) -> Value {
    let mut m = match i.metadata.clone() {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    m.insert(
        MARKER.to_string(),
        json!({ "work_item_id": work_item_id.to_string() }),
    );
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// Tracker
// ---------------------------------------------------------------------------

#[async_trait]
impl Tracker for Ado {
    fn name(&self) -> &'static str {
        NAME
    }

    fn required_config(&self) -> &'static [&'static str] {
        &[ORG_KEY, PROJECT_KEY]
    }

    fn secret_env(&self) -> &'static str {
        SECRET_ENV
    }

    /// What is missing, if anything. Must work on a workspace that has never
    /// heard of ADO — that is the only situation anyone runs it in.
    async fn status(&self, ctx: &Ctx) -> Result<TrackerStatus> {
        let org = config_value(ctx, ORG_KEY).await;
        let project = config_value(ctx, PROJECT_KEY).await;
        let pat = secret();

        let mut missing = Vec::new();
        if org.is_none() {
            missing.push(ORG_KEY.to_string());
        }
        if project.is_none() {
            missing.push(PROJECT_KEY.to_string());
        }
        // The token is not a config key, but it *is* missing, and the point of
        // this command is to answer "why can't I sync". Leaving it out of the
        // list would report a fully configured tracker that cannot make a call.
        if pat.is_none() {
            missing.push(SECRET_ENV.to_string());
        }

        let detail = match (&org, &project) {
            (Some(o), Some(p)) => Some(format!("https://dev.azure.com/{o}/{p}")),
            _ => None,
        };
        Ok(TrackerStatus {
            name: NAME.to_string(),
            configured: missing.is_empty(),
            missing,
            // NB: never the literal "not implemented yet" — `commands::sync`
            // treats that exact string as "this tracker is a stub" and exits 64.
            detail,
        })
    }

    /// ADO → beads.
    ///
    /// Two steps, because the API has two (trap 2): WIQL for the ids, then a
    /// chunked batch GET for the fields.
    async fn pull(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let cfg = Cfg::load(ctx).await?;
        let store = ctx.store().await?;
        let mut report = SyncReport::default();

        let ids = query_ids(&cfg, http).await?;
        if ids.is_empty() {
            // An empty `?ids=` is a 400, not an empty result. Nothing to ask.
            return Ok(report);
        }
        let items = fetch_work_items(&cfg, http, &ids).await?;

        // The join key, indexed once. `IssueFilter` cannot select on
        // external_ref (see the report), so this is a full listing — the cost is
        // one query, not one per work item.
        let local = store.list_issues(&IssueFilter::default()).await?;
        let index: HashMap<String, String> = local
            .iter()
            .filter_map(|i| is_ours(i).map(|r| (r, i.id.clone())))
            .collect();

        let prefix = ctx.prefix().await;

        for wi in items {
            let external = wi.id.to_string();
            let title = text(&wi.fields, "System.Title");
            if title.is_empty() {
                // `Issue::validate` rejects an empty title, so this would fail
                // the write anyway. Decline it by name instead of dying halfway
                // through the backlog.
                report
                    .skipped
                    .push(format!("work item {external} has no System.Title"));
                continue;
            }

            let description = text(&wi.fields, "System.Description");
            let status = map_state(&text(&wi.fields, "System.State"));
            let issue_type = map_type(&text(&wi.fields, "System.WorkItemType"));
            let priority = map_priority(&wi.fields);
            // `System.AssignedTo` is an identity object, not a string.
            let assignee = wi
                .fields
                .get("System.AssignedTo")
                .and_then(|v| {
                    v.get("uniqueName")
                        .or_else(|| v.get("displayName"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default()
                .to_string();

            match index.get(&external) {
                // Already ours: update in place. The remote is authoritative on
                // a pull, so every mapped field is written, not merged.
                Some(local_id) => {
                    let patch = IssuePatch {
                        title: Some(title),
                        status: Some(status),
                        priority: Some(priority),
                        issue_type: Some(issue_type),
                        description: Field::Set(description),
                        assignee: Field::Set(assignee),
                        external_ref: Field::Set(external),
                        ..Default::default()
                    };
                    store.update_issue(local_id, &patch).await?;
                    report.updated += 1;
                }
                None => {
                    let id = store.next_id(&prefix, &title, &description).await?;
                    let issue = Issue {
                        title,
                        description,
                        status,
                        priority,
                        issue_type,
                        assignee,
                        // The join key. Both halves, always — one half alone is
                        // a duplicate on the next run.
                        external_ref: Some(external),
                        source_system: NAME.to_string(),
                        ..Issue::new(id, "")
                    };
                    store.create_issue(&issue).await?;
                    report.created += 1;
                }
            }
            report.pulled += 1;
        }

        Ok(report)
    }

    /// beads → ADO. Creates work items for beads that are not linked to one yet.
    ///
    /// **Create-only.** A bead already bound to a work item is left alone rather
    /// than pushed back over: local edits do not win over the remote here, and
    /// pretending otherwise would need a conflict policy that nobody has
    /// specified. (`pull` is the direction that resolves: the remote wins.)
    async fn push(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let cfg = Cfg::load(ctx).await?;
        let store = ctx.store().await?;
        let mut report = SyncReport::default();

        let issues = store.list_issues(&IssueFilter::default()).await?;

        let mut closed = 0u64;
        let mut infra = 0u64;
        let mut candidates = Vec::new();
        for i in issues {
            if is_ours(&i).is_some() {
                continue; // already a work item; create-only, so nothing to do
            }
            match (&i.external_ref, i.source_system.as_str()) {
                (Some(r), "") => {
                    // An external_ref with no system and no marker: somebody set
                    // it by hand. Overwriting it would destroy the only link they
                    // have to whatever it points at.
                    report.skipped.push(format!(
                        "{}: already carries external_ref `{r}` with no source_system",
                        i.id
                    ));
                    continue;
                }
                (Some(r), sys) => {
                    report
                        .skipped
                        .push(format!("{}: already linked to {sys} ({r})", i.id));
                    continue;
                }
                (None, _) => {}
            }
            if i.status.is_closed() {
                closed += 1;
                continue;
            }
            if i.ephemeral || i.is_template || i.issue_type.excluded_from_ready() {
                infra += 1;
                continue;
            }
            candidates.push(i);
        }
        // Summarized rather than one line per bead: on a real backlog the
        // per-bead form is thousands of lines of "not pushed, and that is
        // normal", which is how a report stops being read at all.
        if closed > 0 {
            report.skipped.push(format!(
                "{closed} closed beads were not created upstream (a new work item starts in its \
                 initial state, so creating one for finished work would file it as open)"
            ));
        }
        if infra > 0 {
            report.skipped.push(format!(
                "{infra} ephemeral/template/infrastructure beads were not created upstream"
            ));
        }

        // Labels are not hydrated by `list_issues`, and one read per bead would
        // be an N+1 against the store just to fill in a tag list.
        let ids: Vec<String> = candidates.iter().map(|i| i.id.clone()).collect();
        let labels: HashMap<String, Vec<String>> =
            store.labels_of(&ids).await?.into_iter().collect();

        for i in candidates {
            // Trap 4: a JSON Patch *document* — a list of ops, sent as
            // `application/json-patch+json`. A plain `{"fields": {...}}` object
            // is rejected.
            let mut ops = vec![json!({
                "op": "add",
                "path": "/fields/System.Title",
                "value": i.title,
            })];
            if !i.description.is_empty() {
                ops.push(json!({
                    "op": "add",
                    "path": "/fields/System.Description",
                    "value": i.description,
                }));
            }
            ops.push(json!({
                "op": "add",
                "path": "/fields/Microsoft.VSTS.Common.Priority",
                "value": ado_priority(i.priority),
            }));
            if let Some(ls) = labels.get(&i.id).filter(|ls| !ls.is_empty()) {
                ops.push(json!({
                    "op": "add",
                    "path": "/fields/System.Tags",
                    "value": ls.join("; "),
                }));
            }
            // `System.AssignedTo` is deliberately not sent. It takes an ADO
            // identity, and a beads actor is a git email or an agent name that
            // usually is not one — an unresolvable identity fails the whole
            // create, so an unassigned work item beats no work item.

            let url = format!(
                "{}/workitems/${}?api-version={API_VERSION}",
                cfg.base(),
                // "User Story" has a space in it, and this is a path segment.
                work_item_type(&i.issue_type).replace(' ', "%20"),
            );
            let req = HttpRequest::post(url, serde_json::to_string(&ops)?)
                .header("Authorization", basic_auth(&cfg.pat))
                .header("Accept", "application/json")
                .header("Content-Type", "application/json-patch+json");

            let resp = http.send(req).await?;
            let created: Created = resp
                .json()
                .with_context(|| format!("ado: creating a work item for {}", i.id))?;

            // Bind the bead to the work item *now*. A create that is not
            // recorded locally is a create that happens again on the next push.
            let patch = IssuePatch {
                external_ref: Field::Set(created.id.to_string()),
                metadata: Field::Set(with_marker(&i, created.id)),
                ..Default::default()
            };
            store.update_issue(&i.id, &patch).await?;
            report.pushed += 1;
        }

        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// The two-step read (trap 2 and trap 3)
// ---------------------------------------------------------------------------

/// Step 1: WIQL. Returns ids, and only ids.
async fn query_ids(cfg: &Cfg, http: &dyn Http) -> Result<Vec<i64>> {
    // WIQL is SQL-shaped, and the project name is a string literal in it. A
    // literal quote is doubled — an apostrophe in a project name (they are legal,
    // e.g. "Bob's Team") would otherwise end the string early and the query would
    // fail to parse, or worse, parse as something else.
    let project = cfg.project.replace('\'', "''");
    let query = format!(
        "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = '{project}' \
         ORDER BY [System.ChangedDate] DESC"
    );
    let url = format!("{}/wiql?api-version={API_VERSION}", cfg.base());
    let req = HttpRequest::post(url, json!({ "query": query }).to_string())
        .header("Authorization", basic_auth(&cfg.pat))
        .header("Accept", "application/json")
        .json();

    let resp = http.send(req).await?;
    let result: WiqlResult = resp.json().context("ado: WIQL query")?;
    Ok(result.work_items.into_iter().map(|r| r.id).collect())
}

/// Step 2: the fields, in batches of at most [`MAX_BATCH_IDS`].
///
/// The chunking is the point. `?ids=` with 201 ids does not return 200 items and
/// a warning — the request fails, and it only ever fails for someone whose
/// backlog is big enough to matter.
async fn fetch_work_items(cfg: &Cfg, http: &dyn Http, ids: &[i64]) -> Result<Vec<WorkItem>> {
    let mut items = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(MAX_BATCH_IDS) {
        let csv = chunk
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!(
            "{}/workitems?ids={csv}&api-version={API_VERSION}",
            cfg.base()
        );
        let resp = http.send(get(cfg, url)).await?;
        let batch: Batch = resp
            .json()
            .with_context(|| format!("ado: fetching {} work items", chunk.len()))?;
        items.extend(batch.value);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The auth header, checked against an independently computed value. If this
    /// is wrong every call 401s, and the 401 blames the token.
    #[test]
    fn basic_auth_is_base64_of_colon_pat() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        // base64(":s3cr3t") — note the empty username before the colon.
        assert_eq!(basic_auth("s3cr3t"), "Basic OnMzY3IzdA==");
        assert!(!basic_auth("s3cr3t").starts_with("Bearer"));
    }

    #[test]
    fn an_unknown_state_stays_open() {
        // Agile
        assert_eq!(map_state("New"), Status::Open);
        assert_eq!(map_state("Active"), Status::InProgress);
        assert_eq!(map_state("Resolved"), Status::InProgress);
        assert_eq!(map_state("Closed"), Status::Closed);
        // Scrum
        assert_eq!(map_state("Approved"), Status::Open);
        assert_eq!(map_state("Committed"), Status::InProgress);
        assert_eq!(map_state("Done"), Status::Closed);
        assert_eq!(map_state("Removed"), Status::Closed);
        // A customized process, or an empty field. Open — never closed.
        assert_eq!(map_state("Needs Triage"), Status::Open);
        assert_eq!(map_state(""), Status::Open);
    }

    #[test]
    fn priority_keeps_the_defaults_aligned() {
        let p = |n: i64| {
            let mut f = Map::new();
            f.insert("Microsoft.VSTS.Common.Priority".into(), json!(n));
            map_priority(&f)
        };
        assert_eq!(p(1), Priority::CRITICAL);
        // The one that matters: ADO's default lands on beads' default, so a
        // synced backlog does not outrank everything filed locally.
        assert_eq!(p(2), Priority::NORMAL);
        assert_eq!(p(3), Priority::LOW);
        assert_eq!(p(4), Priority::TRIVIAL);
        assert_eq!(map_priority(&Map::new()), Priority::NORMAL);

        assert_eq!(ado_priority(Priority::CRITICAL), 1);
        assert_eq!(ado_priority(Priority::HIGH), 1); // rounds up, see the doc
        assert_eq!(ado_priority(Priority::NORMAL), 2);
        assert_eq!(ado_priority(Priority::TRIVIAL), 4);
    }
}
