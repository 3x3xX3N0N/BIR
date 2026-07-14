//! GitHub Issues sync.
//!
//! # The three traps this file exists to avoid
//!
//! 1. **The issues endpoint returns pull requests.** `GET /repos/{o}/{r}/issues`
//!    hands back PRs alongside issues — a PR *is* an issue to GitHub, with an
//!    extra `pull_request` key. Sync them and the backlog quietly fills with
//!    every PR the repo ever had. They are skipped here, and the skip is
//!    reported rather than silent.
//!
//! 2. **Pagination.** `per_page=100` is the ceiling, and a repo with 101 issues
//!    is not rare. A tracker that reads only the first page looks *exactly* like
//!    one that worked. So we walk pages until one comes back short.
//!
//!    GitHub's own answer is the `Link` header's `rel="next"` — but
//!    [`HttpResponse`](super::HttpResponse) carries only a status and a body, so
//!    no header is reachable from behind the seam. Page-walking is the honest
//!    fallback: same result, one extra request when the issue count happens to be
//!    an exact multiple of the page size. Give the seam
//!    `headers: Vec<(String, String)>` and this becomes a `Link` walk.
//!
//! 3. **Identity.** Every pulled bead carries `external_ref = "<number>"` and
//!    `source_system = "github"`. That pair is the join key, and the next pull
//!    finds an existing bead by it and *updates* rather than inserting a second
//!    copy. Get this wrong and every sync duplicates the backlog.

use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use bd_core::{Issue, IssueFilter, IssueType, Status};
use bd_storage::{Field, IssuePatch};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use super::{Http, HttpRequest, Method, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

pub struct GitHub;

/// `Issue.source_system` for everything this tracker pulls, and the name on the
/// command line. The two must be the same string.
pub const NAME: &str = "github";

/// The workspace config key holding `owner/name`. Set with `bd config set`.
pub const REPO_KEY: &str = "github.repo";

/// A local bead carrying this label is destined for GitHub even though it did
/// not come from there.
///
/// It exists because there is no other way to say it: `IssuePatch` has no
/// `source_system` field, so nothing on the CLI side can stamp a locally-created
/// bead as GitHub's. A label is the only durable, user-settable marker
/// available. `push` strips it from what it sends (it is beads' bookkeeping, not
/// the repo's), and `pull` preserves it when reconciling labels — otherwise the
/// first pull after a push would strip the bead's only mark of ownership.
pub const MARKER_LABEL: &str = "github";

const API: &str = "https://api.github.com";
const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";

/// GitHub's maximum. Public so a test can build a full page without guessing.
pub const PER_PAGE: usize = 100;

/// A page-walk needs a stop, and "the server keeps saying there is more" must
/// not be an infinite loop. 100 full pages is 10k issues; hitting the cap is
/// reported, never silent.
const MAX_PAGES: usize = 100;

// ---------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    /// GitHub sends `null`, not `""`, for an empty body.
    #[serde(default)]
    body: Option<String>,
    /// `open` | `closed`.
    state: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    /// **The PR tell.** Present (as an object) on pull requests and absent on
    /// issues. Its contents are irrelevant; only its presence matters.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    closed_at: Option<DateTime<Utc>>,
}

/// Labels come back as objects from the REST API, but bare strings from a few
/// older payloads and from anything hand-rolled. Accept both rather than fail
/// the whole page on one unexpected shape.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GhLabel {
    Name(String),
    Object { name: String },
}

impl GhLabel {
    fn name(&self) -> &str {
        match self {
            GhLabel::Name(s) => s,
            GhLabel::Object { name } => name,
        }
    }
}

/// Just enough of the response to a create.
#[derive(Debug, Deserialize)]
struct GhRef {
    number: u64,
}

impl GhIssue {
    fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }

    fn status(&self) -> Status {
        // GitHub has exactly two states. Anything else is a shape we do not
        // know, and "open" is the safe reading: it keeps the work visible.
        if self.state == "closed" {
            Status::Closed
        } else {
            Status::Open
        }
    }

    fn label_names(&self) -> Vec<String> {
        self.labels.iter().map(|l| l.name().to_string()).collect()
    }

    /// The type GitHub's labels *positively* imply, or `None` if they imply
    /// nothing.
    ///
    /// `None` is not "task" — it means "GitHub has no opinion", and on update we
    /// leave the local type alone rather than resetting a hand-set `epic` to
    /// `task` on every pull. Only `bug` and `feature`/`enhancement` are read;
    /// inventing a mapping for arbitrary labels would be guessing.
    fn implied_type(&self) -> Option<IssueType> {
        self.labels.iter().find_map(|l| {
            match l.name().to_ascii_lowercase().as_str() {
                "bug" => Some(IssueType::Bug),
                // `IssueType::from` already folds `enhancement` into `Feature`.
                "enhancement" | "feature" => Some(IssueType::Feature),
                _ => None,
            }
        })
    }

    fn description(&self) -> String {
        self.body.clone().unwrap_or_default()
    }

    /// beads validates a title at <= 500 chars and *rejects* the issue
    /// otherwise. GitHub does not, so an over-long remote title would abort the
    /// whole pull rather than land one imperfect bead. Clamp it.
    fn clamped_title(&self) -> String {
        let max = bd_core::types::MAX_TITLE_LEN;
        if self.title.chars().count() <= max {
            return self.title.clone();
        }
        self.title.chars().take(max - 1).collect::<String>() + "…"
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    /// `owner/name`, and nothing else. `https://github.com/o/n` and `o/n/extra`
    /// are both rejected rather than half-understood, because a URL built from a
    /// misparsed repo 404s in a way that reads like a permissions problem.
    pub fn parse(s: &str) -> Option<Repo> {
        let s = s.trim().trim_end_matches('/');
        let (owner, name) = s.split_once('/')?;
        let ok = |p: &str| !p.is_empty() && !p.contains('/') && !p.contains(char::is_whitespace);
        (ok(owner) && ok(name)).then(|| Repo {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

/// The token, or `None`.
///
/// Read from the environment, never from `.beads/config.yaml` — that file is
/// committed in most projects, and a token in it is a token on GitHub.
fn token() -> Option<String> {
    std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty())
}

/// The configured repo string, or `None`. Never an error: [`Tracker::status`]
/// has to work in a workspace that is not set up, which is the only situation
/// anyone runs it in.
async fn configured_repo(ctx: &Ctx) -> Option<String> {
    let store = ctx.store().await.ok()?;
    store
        .get_config(REPO_KEY)
        .await
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
}

/// The whole of `status`'s logic, as a pure function — so it is testable across
/// all four combinations of (repo, token) without touching the process
/// environment, which tests cannot safely mutate in parallel.
pub fn evaluate(repo: Option<&str>, token: Option<&str>) -> TrackerStatus {
    let mut missing = Vec::new();
    let mut detail = None;

    let parsed = repo.and_then(Repo::parse);
    match (repo, &parsed) {
        (None, _) => missing.push(REPO_KEY.to_string()),
        // Set but unusable. Saying "missing" alone would send someone hunting
        // for a key that is right there in front of them.
        (Some(raw), None) => {
            missing.push(REPO_KEY.to_string());
            detail = Some(format!("{REPO_KEY} must be `owner/name` (got `{raw}`)"));
        }
        (Some(_), Some(_)) => {}
    }

    // The token is an env var, not a config key, and the `$` says so.
    if token.is_none_or(str::is_empty) {
        missing.push("$GITHUB_TOKEN".to_string());
    }

    let configured = missing.is_empty();
    if configured && let Some(r) = &parsed {
        detail = Some(format!("{}/{} — token from $GITHUB_TOKEN", r.owner, r.name));
    }

    TrackerStatus {
        name: NAME.to_string(),
        configured,
        missing,
        detail,
    }
}

// ---------------------------------------------------------------------------
// Tracker
// ---------------------------------------------------------------------------

#[async_trait]
impl Tracker for GitHub {
    fn name(&self) -> &'static str {
        NAME
    }

    fn required_config(&self) -> &'static [&'static str] {
        &[REPO_KEY]
    }

    fn secret_env(&self) -> &'static str {
        "GITHUB_TOKEN"
    }

    async fn status(&self, ctx: &Ctx) -> Result<TrackerStatus> {
        let repo = configured_repo(ctx).await;
        Ok(evaluate(repo.as_deref(), token().as_deref()))
    }

    async fn pull(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let repo = repo(ctx).await?;
        let store = ctx.store().await?;
        let mut report = SyncReport::default();

        let remote = fetch_all(&repo, http, &mut report).await?;

        // One scan, one label query — not one lookup per remote issue. A repo
        // with 500 issues would otherwise be 500 round trips into SQLite.
        let locals = store.list_issues(&IssueFilter::default()).await?;
        let ids: Vec<String> = locals.iter().map(|i| i.id.clone()).collect();
        let mut labels: HashMap<String, Vec<String>> =
            store.labels_of(&ids).await?.into_iter().collect();

        // THE JOIN KEY: (source_system, external_ref). Anything that misses here
        // gets inserted a second time, and the duplicate is permanent.
        let mut by_ref: HashMap<&str, &Issue> = HashMap::new();
        for i in &locals {
            let Some(ext) = i.external_ref.as_deref() else {
                continue;
            };
            if owns(i, labels.get(&i.id).map(Vec::as_slice).unwrap_or(&[])) {
                by_ref.insert(ext, i);
            }
        }

        let prefix = ctx.prefix().await;
        for gh in &remote {
            if gh.is_pull_request() {
                // The bug this whole tracker is most likely to have.
                report
                    .skipped
                    .push(format!("#{}: pull request, not an issue", gh.number));
                continue;
            }
            report.pulled += 1;
            let ext = gh.number.to_string();

            match by_ref.get(ext.as_str()) {
                // `updated` counts beads *matched and reconciled*, not rows
                // written: `reconcile` skips the write when nothing differs, so a
                // pull that changes nothing does not churn `updated_at` and skew
                // `bd stale`.
                Some(local) => {
                    let current = labels.remove(&local.id).unwrap_or_default();
                    reconcile(store, local, &current, gh).await?;
                    report.updated += 1;
                }
                None => {
                    let id = store
                        .next_id(&prefix, &gh.clamped_title(), &gh.description())
                        .await?;
                    let now = Utc::now();
                    let issue = Issue {
                        id,
                        title: gh.clamped_title(),
                        description: gh.description(),
                        status: gh.status(),
                        // No priority. GitHub has none, and defaulting every
                        // pulled bead to P2 is honest; inventing one from labels
                        // would not be.
                        issue_type: gh.implied_type().unwrap_or_default(),
                        created_at: gh.created_at.unwrap_or(now),
                        updated_at: gh.updated_at.unwrap_or(now),
                        closed_at: gh.closed_at,
                        external_ref: Some(ext),
                        source_system: NAME.to_string(),
                        labels: gh.label_names(),
                        ..Default::default()
                    };
                    store.create_issue(&issue).await?;
                    report.created += 1;
                }
            }
        }

        Ok(report)
    }

    async fn push(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let repo = repo(ctx).await?;
        let store = ctx.store().await?;
        let tok = token();
        let mut report = SyncReport::default();

        // Wisps are TTL bookkeeping — heartbeats, gc reports. They are nobody's
        // GitHub issue, and a reaped one would leave a dangling remote.
        let filter = IssueFilter {
            ephemeral: Some(false),
            ..Default::default()
        };
        let locals = store.list_issues(&filter).await?;
        let ids: Vec<String> = locals.iter().map(|i| i.id.clone()).collect();
        let labels: HashMap<String, Vec<String>> =
            store.labels_of(&ids).await?.into_iter().collect();

        for local in &locals {
            let mine = labels.get(&local.id).map(Vec::as_slice).unwrap_or(&[]);
            // A bead that came from Jira but happens to carry a `github` label is
            // not ours to push — doing so would fork it across two trackers.
            if !local.source_system.is_empty() && local.source_system != NAME {
                if mine.iter().any(|l| l == MARKER_LABEL) {
                    report.skipped.push(format!(
                        "{}: labelled `{MARKER_LABEL}` but owned by `{}`",
                        local.id, local.source_system
                    ));
                }
                continue;
            }
            if !owns(local, mine) {
                continue;
            }

            // The marker is beads' own bookkeeping. Creating a `github` label in
            // someone's repo because of it would be rude and confusing.
            let out: Vec<&str> = mine
                .iter()
                .map(String::as_str)
                .filter(|l| *l != MARKER_LABEL)
                .collect();
            let state = if local.status.is_closed() {
                "closed"
            } else {
                "open"
            };
            let body = json!({
                "title": local.title,
                "body": local.description,
                "state": state,
                "labels": out,
            })
            .to_string();

            match &local.external_ref {
                Some(ext) => {
                    let url = format!("{API}/repos/{}/{}/issues/{ext}", repo.owner, repo.name);
                    let req = authed(
                        HttpRequest {
                            method: Method::Patch,
                            url,
                            headers: Vec::new(),
                            body: Some(body),
                        }
                        .json(),
                        tok.as_deref(),
                    );
                    let resp = http.send(req).await?;
                    if !resp.ok() {
                        bail!(
                            "github: updating #{ext} ({}) failed: HTTP {} — {}",
                            local.id,
                            resp.status,
                            resp.body
                        );
                    }
                }
                None => {
                    let url = format!("{API}/repos/{}/{}/issues", repo.owner, repo.name);
                    let req = authed(HttpRequest::post(url, body).json(), tok.as_deref());
                    let resp = http.send(req).await?;
                    let created: GhRef = resp
                        .json()
                        .with_context(|| format!("github: creating an issue for {}", local.id))?;

                    // Record the remote id immediately. If this write is lost the
                    // next push creates the issue *again* — the local bead has no
                    // memory of the one we just made.
                    //
                    // Note what we cannot do: stamp `source_system = "github"`.
                    // `IssuePatch` has no such field. That is why the marker label
                    // exists and why `owns` accepts it as a second key.
                    let patch = IssuePatch {
                        external_ref: Field::Set(created.number.to_string()),
                        ..Default::default()
                    };
                    store.update_issue(&local.id, &patch).await?;
                }
            }
            report.pushed += 1;
        }

        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The repo, or a message that says what to do about it.
async fn repo(ctx: &Ctx) -> Result<Repo> {
    let raw = configured_repo(ctx)
        .await
        .ok_or_else(|| anyhow::anyhow!("{REPO_KEY} is not set (`bd config set {REPO_KEY} owner/name`)"))?;
    Repo::parse(&raw).ok_or_else(|| anyhow::anyhow!("{REPO_KEY} must be `owner/name` (got `{raw}`)"))
}

/// Whether this bead is GitHub's.
///
/// Two keys, not one, and the second is not redundant: a bead this tracker
/// *created* on GitHub cannot be stamped with `source_system` (see
/// [`MARKER_LABEL`]), so it is recognized by its marker instead. Without that,
/// the first pull after a push would not find the bead it had just created and
/// would duplicate it.
fn owns(i: &Issue, labels: &[String]) -> bool {
    i.source_system == NAME
        || (i.source_system.is_empty() && labels.iter().any(|l| l == MARKER_LABEL))
}

fn authed(req: HttpRequest, token: Option<&str>) -> HttpRequest {
    let req = req
        .header("Accept", ACCEPT)
        .header("X-GitHub-Api-Version", API_VERSION);
    // Anonymous requests are legal against a public repo, and the CLI already
    // refuses to run pull/push when `status` says the token is missing. So this
    // does not second-guess the token here: a 401 from GitHub says far more than
    // a pre-emptive guess would.
    match token {
        Some(t) => req.bearer(t),
        None => req,
    }
}

/// Every issue in the repo, following pagination.
///
/// The stop condition is a *short page*: fewer than `per_page` records means
/// there is no next page. It costs one extra request when the count is an exact
/// multiple of 100, and it needs no response headers — which is the point, since
/// the `Link` header is not visible through the [`Http`] seam.
async fn fetch_all(repo: &Repo, http: &dyn Http, report: &mut SyncReport) -> Result<Vec<GhIssue>> {
    let tok = token();
    let mut all: Vec<GhIssue> = Vec::new();

    for page in 1..=MAX_PAGES {
        let url = format!(
            "{API}/repos/{}/{}/issues?state=all&per_page={PER_PAGE}&page={page}",
            repo.owner, repo.name
        );
        let resp = http.send(authed(HttpRequest::get(&*url), tok.as_deref())).await?;
        let batch: Vec<GhIssue> = resp
            .json()
            .with_context(|| format!("github: listing issues for {}/{}", repo.owner, repo.name))?;

        let n = batch.len();
        all.extend(batch);

        if n < PER_PAGE {
            return Ok(all);
        }
        if page == MAX_PAGES {
            // Loud, not silent: a truncated sync that reports success is exactly
            // the failure this whole module is written to avoid.
            report.skipped.push(format!(
                "stopped after {MAX_PAGES} pages ({} issues); the rest of the repo was not read",
                all.len()
            ));
        }
    }
    Ok(all)
}

/// Bring an existing bead in line with GitHub.
///
/// GitHub is authoritative over what GitHub owns — title, body, state, labels —
/// and over *nothing else*. Priority is untouched (GitHub has none, so a pull
/// would otherwise reset a hand-set P0 to the default on every run), and so is
/// the assignee (a GitHub login is not a beads actor, and overwriting it would
/// desynchronize the local claim from its live lease).
async fn reconcile(
    store: &dyn bd_storage::Storage,
    local: &Issue,
    current_labels: &[String],
    gh: &GhIssue,
) -> Result<()> {
    let title = gh.clamped_title();
    let description = gh.description();
    let status = gh.status();

    let mut patch = IssuePatch::default();
    if local.title != title {
        patch.title = Some(title);
    }
    if local.description != description {
        patch.description = Field::Set(description);
    }
    if local.status != status {
        patch.status = Some(status);
    }
    // Only on a positive signal — see `implied_type`.
    if let Some(t) = gh.implied_type()
        && local.issue_type != t
    {
        patch.issue_type = Some(t);
    }
    if !patch.is_empty() {
        store.update_issue(&local.id, &patch).await?;
    }

    // Labels are a set, so they are reconciled rather than patched: GitHub's set
    // wins, except that the marker survives (it is ours, not theirs, and pull
    // must not strip the mark that push relies on).
    let want: HashSet<&str> = gh
        .labels
        .iter()
        .map(GhLabel::name)
        .chain(
            current_labels
                .iter()
                .map(String::as_str)
                .filter(|l| *l == MARKER_LABEL),
        )
        .collect();
    let have: HashSet<&str> = current_labels.iter().map(String::as_str).collect();

    for add in want.difference(&have) {
        store.add_label(&local.id, add).await?;
    }
    for gone in have.difference(&want) {
        store.remove_label(&local.id, gone).await?;
    }
    Ok(())
}
