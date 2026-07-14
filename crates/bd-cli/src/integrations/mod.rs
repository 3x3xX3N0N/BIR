//! Syncing with external issue trackers.
//!
//! # The shape
//!
//! Each tracker implements [`Tracker`]. None of them own an HTTP client — they
//! are handed a [`Http`], which is a trait. That is the whole design, and it is
//! load-bearing for a reason that has nothing to do with elegance:
//!
//! **You cannot test six external API integrations if each one hard-codes
//! `reqwest`.** Testing would mean six sets of live credentials, a network, and
//! six third-party services being up — so in practice it would mean not testing
//! them at all, and shipping six modules whose first real execution is in
//! someone's repo. Behind a trait, every one of them is testable against
//! recorded fixtures, offline, in CI, forever.
//!
//! # Identity across the boundary
//!
//! `Issue.external_ref` holds the remote's id (`"PROJ-123"`), and
//! `Issue.source_system` holds which remote it came from (`"jira"`). Together
//! they are the join key. A tracker MUST set both on anything it pulls, or the
//! next pull cannot tell "this is the issue I already have" from "this is new",
//! and will duplicate the entire backlog on every run.

pub mod ado;
pub mod github;
pub mod gitlab;
pub mod http;
pub mod jira;
pub mod linear;
pub mod notion;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::context::Ctx;
pub use http::{Http, HttpRequest, HttpResponse, Method};

/// What a sync did. Reported to the user, and asserted on in tests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub pulled: u64,
    pub created: u64,
    pub updated: u64,
    pub pushed: u64,
    /// Records the tracker deliberately declined, and why. Never silent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// Whether a tracker is usable right now, and if not, exactly what is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerStatus {
    pub name: String,
    pub configured: bool,
    /// Config keys that are required and absent. The point of this field is that
    /// `bd jira status` can tell you *which* key you forgot rather than just
    /// failing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[async_trait]
pub trait Tracker: Send + Sync {
    /// The name used on the command line and stored in `Issue.source_system`.
    /// These must be the same string, or a pull cannot find what it pushed.
    fn name(&self) -> &'static str;

    /// Config keys this tracker needs, e.g. `["jira.url", "jira.token"]`.
    /// Read from the workspace config; secrets may also come from the
    /// environment (see [`Tracker::secret`]).
    fn required_config(&self) -> &'static [&'static str];

    /// The environment variable holding this tracker's credential, e.g.
    /// `JIRA_TOKEN`. **A token must never be written to the workspace config**:
    /// `.beads/` is committed to git in most projects, and a token in there is a
    /// token on GitHub.
    fn secret_env(&self) -> &'static str;

    async fn status(&self, ctx: &Ctx) -> Result<TrackerStatus>;

    /// Remote → beads.
    async fn pull(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport>;

    /// beads → remote.
    async fn push(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport>;

    /// Both directions. The default is pull-then-push, which is almost always
    /// what you want: pushing first would send a local view that is already
    /// stale, and then immediately pull over the top of it.
    async fn sync(&self, ctx: &Ctx, http: &dyn Http) -> Result<SyncReport> {
        let a = self.pull(ctx, http).await?;
        let b = self.push(ctx, http).await?;
        Ok(SyncReport {
            pulled: a.pulled,
            created: a.created,
            updated: a.updated,
            pushed: b.pushed,
            skipped: [a.skipped, b.skipped].concat(),
        })
    }
}

/// Every tracker bd knows.
pub fn registry() -> Vec<Box<dyn Tracker>> {
    vec![
        Box::new(linear::Linear),
        Box::new(jira::Jira),
        Box::new(github::GitHub),
        Box::new(gitlab::GitLab),
        Box::new(notion::Notion),
        Box::new(ado::Ado),
    ]
}

pub fn get(name: &str) -> Option<Box<dyn Tracker>> {
    registry().into_iter().find(|t| t.name() == name)
}
