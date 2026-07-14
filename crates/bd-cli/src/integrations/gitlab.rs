//! GitLab sync, over the REST v4 API.
//!
//! # `id` vs `iid` — pick one and never mix them
//!
//! A GitLab issue carries two numbers. `id` is unique across the whole instance;
//! `iid` ("internal id") is the per-project one — `#42` in the UI, `/-/issues/42`
//! in the URL, and **the one every project-scoped endpoint takes**. `PUT
//! /projects/:p/issues/:id` does not exist; it is `PUT /projects/:p/issues/:iid`.
//!
//! So the join key here is the **iid**: `external_ref = "42"`, `source_system =
//! "gitlab"`. The global `id` is kept in `metadata.gitlab.id` so it is not lost,
//! but nothing joins on it. Mixing the two is the classic GitLab bug: the update
//! lands on a *different issue that happens to have that iid*, or 404s, and both
//! failures look like a permissions problem.
//!
//! # What is mapped, and what deliberately is not
//!
//! - `state: opened|closed` → [`Status`]. Two states upstream, seven here, so the
//!   mapping is *asymmetric on purpose* — see [`GitLab::pull`].
//! - `labels` → beads labels. The remote is authoritative: a label removed on
//!   GitLab is removed here.
//! - **Priority is not mapped, because GitLab has no priority field.** What looks
//!   like one is either issue *weight* (a paid feature, and a size estimate, not
//!   an urgency) or a `priority::high` scoped-label convention that every team
//!   spells differently. Guessing would silently rewrite the priority of every
//!   bead on every pull, so pulled issues keep the beads default (P2) and local
//!   priority survives a sync untouched.
//! - **Assignees are not mapped.** `Issue.assignee` is the *claim holder* here —
//!   it comes with a lease and gates `bd ready`. Importing a GitLab assignee
//!   would manufacture a claim that no agent holds and no lease expires.
//! - `issue_type` (issue/incident/test_case/task) is a different taxonomy from
//!   ours; it is left alone rather than guessed at.
//!
//! # The seam gap this file works around
//!
//! [`IssuePatch`] can set `external_ref` but has **no `source_system` field**, so
//! after `push` creates an issue on GitLab there is no way to stamp the local
//! bead as ours. The next `pull` would then not recognize it and would duplicate
//! it. Until the patch grows `source_system: Field<String>`, `push` writes a
//! `metadata.gitlab` marker instead and [`is_ours`] reads it. That is the only
//! reason this file looks in metadata at all; delete it the day the field lands.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail};
use async_trait::async_trait;
use bd_core::{Issue, IssueFilter, Status};
use bd_storage::{Field, IssuePatch};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use super::{Http, HttpRequest, Method, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

/// Must equal [`Tracker::name`]: it is what a pull writes into
/// `Issue.source_system`, and what the next pull looks it up by.
const SOURCE: &str = "gitlab";
const KEY_URL: &str = "gitlab.url";
const KEY_PROJECT: &str = "gitlab.project";
const ENV_TOKEN: &str = "GITLAB_TOKEN";
/// Self-hosted is the common case, so the host is config — but SaaS is the
/// common *default*, so it has one.
const DEFAULT_URL: &str = "https://gitlab.com";
const PER_PAGE: usize = 100;
/// A stop on the paging loop. A well-behaved server ends it by returning a short
/// page; this only trips if one never does, and an infinite loop against an API
/// is worse than an error that says why it stopped.
const MAX_PAGES: usize = 1000;

pub struct GitLab;

#[async_trait]
impl Tracker for GitLab {
    fn name(&self) -> &'static str {
        SOURCE
    }

    /// `gitlab.url` is listed because self-hosted installs must set it, but it is
    /// the one key with a default ([`DEFAULT_URL`]); only `gitlab.project` is
    /// unguessable, so only it can make a workspace unconfigured.
    fn required_config(&self) -> &'static [&'static str] {
        &[KEY_URL, KEY_PROJECT]
    }

    fn secret_env(&self) -> &'static str {
        ENV_TOKEN
    }

    async fn status(&self, ctx: &Ctx) -> Result<TrackerStatus> {
        let project = config(ctx, KEY_PROJECT).await;
        let url = config(ctx, KEY_URL).await;
        let token = token();

        let mut missing = Vec::new();
        if project.is_none() {
            missing.push(KEY_PROJECT.to_string());
        }
        // Named in `missing` even though it is not a config key: the caller
        // renders `missing` as the list of what to go fix, and "not configured
        // (missing: )" is not an answer.
        if token.is_none() {
            missing.push(format!("${ENV_TOKEN}"));
        }

        let base = url.clone().unwrap_or_else(|| DEFAULT_URL.to_string());
        let detail = match (&project, missing.is_empty()) {
            (Some(p), true) => format!(
                "{} project {} (issues joined on iid → external_ref)",
                trim_base(&base),
                p
            ),
            _ => format!(
                "set {KEY_PROJECT} to the numeric project id or its full path \
                 (`group/project`); {KEY_URL} defaults to {DEFAULT_URL}; \
                 the token comes from ${ENV_TOKEN}, never from .beads/config.yaml"
            ),
        };

        Ok(TrackerStatus {
            name: SOURCE.to_string(),
            configured: missing.is_empty(),
            missing,
            detail: Some(detail),
        })
    }

    /// Remote → beads: page through the project's issues, then create or update.
    ///
    /// The status mapping is deliberately **not** symmetric. GitLab has two live
    /// states and beads has several, so `opened` is taken to mean "not closed"
    /// rather than "Open": it reopens a locally closed bead, but it does not
    /// knock an `in_progress`, `blocked` or `deferred` bead back to `open`. The
    /// symmetric mapping would silently un-claim every bead an agent is working
    /// on, on every pull, and report success while doing it.
    async fn pull(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let cfg = self.resolve(ctx).await?;
        // The dispatcher exempts `pull` from this check; it still writes the
        // local database, so it is a write and --readonly must refuse it.
        ctx.ensure_writable("pull gitlab issues into the workspace")?;

        let remote = fetch_issues(http, &cfg).await?;
        let store = ctx.store().await?;
        let prefix = ctx.prefix().await;

        // The join: every bead we already own, keyed by the iid it came from.
        let locals = store.list_issues(&IssueFilter::default()).await?;
        let mine: HashMap<String, Issue> = locals
            .into_iter()
            .filter(is_ours)
            .filter_map(|i| i.external_ref.clone().map(|r| (r, i)))
            .collect();
        // `list_issues` does not hydrate relations, so labels come separately —
        // one query, not one per issue.
        let ids: Vec<String> = mine.values().map(|i| i.id.clone()).collect();
        let mut have_labels: HashMap<String, Vec<String>> =
            store.labels_of(&ids).await?.into_iter().collect();

        let mut report = SyncReport::default();
        for gl in &remote {
            if gl.title.trim().is_empty() {
                report
                    .skipped
                    .push(format!("gitlab #{}: empty title", gl.iid));
                continue;
            }
            let key = gl.iid.to_string();
            let closed = match gl.state.as_str() {
                "closed" => true,
                "opened" | "reopened" => false,
                other => {
                    // Unknown states are treated as live rather than dropped: a
                    // new GitLab state we have not heard of is still an issue.
                    report.skipped.push(format!(
                        "gitlab #{}: unknown state `{other}`, treated as open",
                        gl.iid
                    ));
                    false
                }
            };
            let want: BTreeSet<String> = gl.labels.iter().cloned().collect();

            match mine.get(&key) {
                Some(local) => {
                    let patch = IssuePatch {
                        title: Some(gl.title.clone()),
                        description: Field::authoritative(
                            gl.description.clone().filter(|d| !d.is_empty()),
                        ),
                        // Only ever a state *transition* — see the doc comment.
                        status: match (closed, local.status.is_closed()) {
                            (true, false) => Some(Status::Closed),
                            (false, true) => Some(Status::Open),
                            _ => None,
                        },
                        // Re-stamped rather than assumed: a bead whose ref was
                        // cleared by hand would otherwise be duplicated forever.
                        external_ref: Field::Set(key.clone()),
                        ..Default::default()
                    };
                    store.update_issue(&local.id, &patch).await?;

                    let have: BTreeSet<String> =
                        have_labels.remove(&local.id).unwrap_or_default().into_iter().collect();
                    for l in want.difference(&have) {
                        store.add_label(&local.id, l).await?;
                    }
                    for l in have.difference(&want) {
                        store.remove_label(&local.id, l).await?;
                    }
                    report.updated += 1;
                }
                None => {
                    let desc = gl.description.clone().unwrap_or_default();
                    let id = store.next_id(&prefix, &gl.title, &desc).await?;
                    let mut issue = Issue::new(id, gl.title.clone());
                    issue.description = desc;
                    issue.status = if closed { Status::Closed } else { Status::Open };
                    issue.labels = want.into_iter().collect();
                    // The pair that makes the next pull an update instead of a
                    // second copy of the whole backlog.
                    issue.external_ref = Some(key);
                    issue.source_system = SOURCE.to_string();
                    issue.metadata = Some(gl.marker());
                    if let Some(t) = gl.created_at {
                        issue.created_at = t;
                    }
                    if let Some(t) = gl.updated_at {
                        issue.updated_at = t;
                    }
                    issue.closed_at = gl.closed_at;
                    store.create_issue(&issue).await?;
                    report.created += 1;
                }
            }
            report.pulled += 1;
        }

        Ok(report)
    }

    /// beads → remote. Issues we already own are `PUT` back; unlinked local beads
    /// are `POST`ed and then stamped with the iid GitLab minted for them.
    async fn push(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let cfg = self.resolve(ctx).await?;
        ctx.ensure_writable("push issues to gitlab")?;

        let store = ctx.store().await?;
        let locals = store.list_issues(&IssueFilter::default()).await?;
        let ids: Vec<String> = locals.iter().map(|i| i.id.clone()).collect();
        let labels: HashMap<String, Vec<String>> =
            store.labels_of(&ids).await?.into_iter().collect();

        let mut report = SyncReport::default();
        let mut local_only = 0u64;

        for issue in &locals {
            // Wisps, gates, molecules and audit events are bookkeeping — they are
            // beads-internal and mean nothing on GitLab. Counted, not listed:
            // one line per heartbeat would bury the skips that matter.
            if issue.ephemeral || issue.issue_type.excluded_from_ready() {
                local_only += 1;
                continue;
            }
            // Someone else's issue. Pushing it would fork it into a second
            // tracker and there would be no way back.
            if !issue.source_system.is_empty() && issue.source_system != SOURCE {
                report.skipped.push(format!(
                    "{}: belongs to {}",
                    issue.id, issue.source_system
                ));
                continue;
            }

            let ls = labels.get(&issue.id).cloned().unwrap_or_default();

            match (is_ours(issue), issue.external_ref.as_deref()) {
                (true, Some(iid)) => {
                    let url = format!("{}/issues/{iid}", cfg.project_url());
                    let body = json!({
                        "title": issue.title,
                        "description": issue.description,
                        "labels": ls.join(","),
                        // GitLab has no "set the state to X" — only an event. It
                        // is a no-op when the state already matches, so sending
                        // it unconditionally is safe and is the only way a bead
                        // reopened locally ever reopens upstream.
                        "state_event": if issue.status.is_closed() { "close" } else { "reopen" },
                    });
                    let req = HttpRequest {
                        method: Method::Put,
                        url,
                        headers: Vec::new(),
                        body: Some(body.to_string()),
                    }
                    .json();
                    let resp = cfg.send(http, req).await?;
                    let _: GlIssue = resp.json()?;
                    report.pushed += 1;
                }
                (false, None) => {
                    let url = format!("{}/issues", cfg.project_url());
                    let body = json!({
                        "title": issue.title,
                        "description": issue.description,
                        "labels": ls.join(","),
                    });
                    let req = HttpRequest::post(url, body.to_string()).json();
                    let resp = cfg.send(http, req).await?;
                    let created: GlIssue = resp.json()?;

                    // Stamp the bead with the iid GitLab just minted, or the next
                    // pull creates a second copy of the issue we just created.
                    // `source_system` cannot be patched (see the module docs), so
                    // the marker goes in metadata, which can.
                    let mut meta = issue
                        .metadata
                        .clone()
                        .filter(|m| m.is_object())
                        .unwrap_or_else(|| json!({}));
                    meta[SOURCE] = created.marker()[SOURCE].clone();
                    let patch = IssuePatch {
                        external_ref: Field::Set(created.iid.to_string()),
                        metadata: Field::Set(meta),
                        ..Default::default()
                    };
                    store.update_issue(&issue.id, &patch).await?;
                    report.pushed += 1;
                }
                // An external_ref we did not put there. It names something in a
                // system we cannot identify, and overwriting it would destroy the
                // only link back to whatever that is.
                (false, Some(r)) => report.skipped.push(format!(
                    "{}: external_ref `{r}` has no source_system — not assumed to be gitlab",
                    issue.id
                )),
                // Ours, but with no iid: nothing to PUT to. Push it as new next
                // run once its ref is repaired; guessing an iid would overwrite a
                // stranger's issue.
                (true, None) => report
                    .skipped
                    .push(format!("{}: marked gitlab but has no iid", issue.id)),
            }
        }

        if local_only > 0 {
            report.skipped.push(format!(
                "{local_only} ephemeral/infrastructure bead(s) are local-only and were not pushed"
            ));
        }
        Ok(report)
    }
}

impl GitLab {
    async fn resolve(&self, ctx: &Ctx) -> Result<Conf> {
        let st = self.status(ctx).await?;
        if !st.configured {
            bail!(
                "gitlab is not configured (missing: {}); `bd config set {KEY_PROJECT} group/project` \
                 and export ${ENV_TOKEN}",
                st.missing.join(", ")
            );
        }
        let base = config(ctx, KEY_URL)
            .await
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        let project = config(ctx, KEY_PROJECT)
            .await
            .ok_or_else(|| anyhow::anyhow!("{KEY_PROJECT} is not set"))?;
        let token = token().ok_or_else(|| anyhow::anyhow!("${ENV_TOKEN} is not set"))?;
        Ok(Conf {
            base: trim_base(&base),
            project: encode_path(&project),
            token,
        })
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

struct Conf {
    base: String,
    /// Already URL-encoded — see [`encode_path`].
    project: String,
    token: String,
}

impl Conf {
    fn project_url(&self) -> String {
        format!("{}/api/v4/projects/{}", self.base, self.project)
    }

    /// Every request, with the header GitLab actually accepts.
    ///
    /// **`PRIVATE-TOKEN`, not `Authorization: Bearer`.** A personal access token
    /// sent as a bearer token is not rejected as malformed — it is simply not a
    /// credential GitLab recognizes there, so you get a 401 that reads exactly
    /// like an expired token. (OAuth tokens *are* bearer tokens; PATs are not.)
    async fn send(&self, http: &dyn Http, req: HttpRequest) -> Result<super::HttpResponse> {
        http.send(req.header("PRIVATE-TOKEN", &self.token)).await
    }
}

/// Tracker config lives in the workspace's key/value config (`bd config set`),
/// never the token.
///
/// Returns `None` rather than failing when there is no workspace at all: that is
/// the exact situation `bd gitlab status` exists to report on, and it must
/// answer rather than explode.
async fn config(ctx: &Ctx, key: &str) -> Option<String> {
    let store = ctx.store().await.ok()?;
    store
        .get_config(key)
        .await
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The token, from the environment only. `.beads/config.yaml` is committed to
/// git in most projects, so a token written there is a token on the internet.
fn token() -> Option<String> {
    std::env::var(ENV_TOKEN).ok().filter(|t| !t.trim().is_empty())
}

fn trim_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Percent-encode a project id for use as a *path segment*.
///
/// GitLab addresses a project either by its numeric id or by its full path, and
/// the path has to arrive encoded: `group/project` → `group%2Fproject`. Leave the
/// slash in and the router sees `/projects/group/project/issues`, which does not
/// match any route — so it 404s, and a 404 from GitLab is indistinguishable from
/// "your token cannot see that project". That is an afternoon spent regenerating
/// tokens that were fine.
fn encode_path(id: &str) -> String {
    id.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Whether a local bead is one this tracker owns.
///
/// `source_system == "gitlab"` is the real test. The metadata fallback covers
/// beads that `push` created on GitLab: [`IssuePatch`] has no `source_system`
/// field, so push can stamp the iid but not the origin, and without the marker
/// the next pull would treat every one of them as new.
fn is_ours(i: &Issue) -> bool {
    i.source_system == SOURCE
        || (i.source_system.is_empty()
            && i.metadata
                .as_ref()
                .is_some_and(|m| m.get(SOURCE).is_some_and(|g| !g.is_null())))
}

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GlIssue {
    /// Instance-wide. Recorded, never joined on.
    id: i64,
    /// Per-project. **This is the identity** — what the UI shows and what
    /// `PUT /projects/:p/issues/:iid` takes.
    iid: i64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    /// `opened` | `closed`.
    state: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    web_url: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    closed_at: Option<DateTime<Utc>>,
}

impl GlIssue {
    /// What goes in `Issue.metadata`. Keeps the global `id` (which nothing joins
    /// on but which the GraphQL API needs) and doubles as the origin marker
    /// [`is_ours`] reads.
    fn marker(&self) -> serde_json::Value {
        json!({ SOURCE: { "id": self.id, "iid": self.iid, "web_url": self.web_url } })
    }
}

/// Every issue in the project, one page at a time.
///
/// GitLab reports the page count in `X-Total-Pages`, but the [`Http`] seam
/// carries a status and a body and no headers — so paging stops on a *short
/// page* instead. That is the more robust signal anyway: `X-Total-Pages` is
/// omitted entirely on large or keyset-paginated result sets, and a tracker that
/// trusts it there fetches page one, sees no header, and reports a successful
/// sync of the first hundred issues.
async fn fetch_issues(http: &dyn Http, cfg: &Conf) -> Result<Vec<GlIssue>> {
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!(
            "{}/issues?per_page={PER_PAGE}&page={page}",
            cfg.project_url()
        );
        let resp = cfg.send(http, HttpRequest::get(url)).await?;
        let batch: Vec<GlIssue> = resp.json()?;
        let short = batch.len() < PER_PAGE;
        all.extend(batch);
        if short {
            return Ok(all);
        }
    }
    bail!(
        "gitlab returned {MAX_PAGES} full pages of issues without a short one; \
         refusing to keep paging"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_path_is_encoded_and_a_numeric_id_is_left_alone() {
        assert_eq!(encode_path("group/project"), "group%2Fproject");
        assert_eq!(
            encode_path("group/sub/project"),
            "group%2Fsub%2Fproject"
        );
        // Numeric ids contain nothing to encode, so they pass through unchanged.
        assert_eq!(encode_path("12345"), "12345");
        // Dots are unreserved: `my.group/my.project` must not become %2E soup.
        assert_eq!(encode_path("my.group/proj"), "my.group%2Fproj");
    }

    #[test]
    fn ownership_survives_a_push_that_could_not_set_source_system() {
        let mut pulled = Issue::new("bd-1", "t");
        pulled.source_system = SOURCE.to_string();
        assert!(is_ours(&pulled));

        // What `push` leaves behind: no source_system (unpatchable), but a marker.
        let mut pushed = Issue::new("bd-2", "t");
        pushed.metadata = Some(json!({ "gitlab": { "id": 9, "iid": 2 } }));
        assert!(is_ours(&pushed));

        // Another tracker's bead, and a plain local one, are not ours.
        let mut theirs = Issue::new("bd-3", "t");
        theirs.source_system = "jira".to_string();
        assert!(!is_ours(&theirs));
        assert!(!is_ours(&Issue::new("bd-4", "t")));
    }
}
