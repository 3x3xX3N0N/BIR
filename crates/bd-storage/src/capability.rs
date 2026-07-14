//! Optional backend capabilities.
//!
//! A backend that cannot branch is not broken — it is a backend that cannot
//! branch. These traits let a store *say so*, so the CLI can print something
//! honest ("`bd branch` needs the dolt backend; this workspace is sqlite")
//! instead of either crashing or silently doing nothing.
//!
//! Rule 4 governs use: a capability may make a core command better, never
//! possible. If `bd ready` needs one of these to be correct, the design is
//! wrong.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::Result;

/// A branch/commit graph over the issue database.
#[async_trait]
pub trait VersionControl: Send + Sync {
    async fn current_branch(&self) -> Result<String>;
    async fn list_branches(&self) -> Result<Vec<String>>;
    async fn create_branch(&self, name: &str) -> Result<()>;
    async fn delete_branch(&self, name: &str, force: bool) -> Result<()>;
    async fn checkout(&self, name: &str) -> Result<()>;

    /// Commit staged changes. Returns the new commit hash.
    async fn commit(&self, message: &str) -> Result<String>;
    async fn current_commit(&self) -> Result<String>;

    /// Uncommitted changes, as table names.
    async fn status(&self) -> Result<Vec<String>>;
    async fn log(&self, limit: u32) -> Result<Vec<CommitInfo>>;

    /// Merge `branch` into the current branch. May surface conflicts rather
    /// than failing — conflicts are data, not errors.
    async fn merge(&self, branch: &str) -> Result<MergeOutcome>;
    async fn conflicts(&self) -> Result<Vec<Conflict>>;
    async fn resolve_conflicts(&self, strategy: ResolveStrategy) -> Result<u64>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub hash: String,
    pub author: String,
    pub message: String,
    pub committed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Nothing to do.
    UpToDate,
    FastForward { to: String },
    Merged { commit: String },
    /// The merge landed but left conflicts to settle.
    Conflicted { count: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub table: String,
    pub issue_id: String,
    pub ours: Option<String>,
    pub theirs: Option<String>,
    pub base: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveStrategy {
    Ours,
    Theirs,
}

/// Talking to other clones.
#[async_trait]
pub trait RemoteStore: Send + Sync {
    async fn add_remote(&self, name: &str, url: &str) -> Result<()>;
    async fn remove_remote(&self, name: &str) -> Result<()>;
    async fn list_remotes(&self) -> Result<Vec<(String, String)>>;

    async fn push(&self, remote: &str, branch: &str) -> Result<()>;

    /// Pull and merge.
    ///
    /// **Whoever implements this must recompute `is_blocked` afterwards.** A
    /// pull can land a closed blocker or a brand-new edge, and no local write
    /// path ever saw it happen — so the denormalized readiness cache is stale
    /// by definition until a full recompute runs. Forgetting this makes
    /// `bd ready` quietly wrong after every sync, which is about the worst
    /// failure this system can have.
    async fn pull(&self, remote: &str, branch: &str) -> Result<MergeOutcome>;

    async fn fetch(&self, remote: &str) -> Result<()>;
}

/// Time travel: what did this issue look like before?
#[async_trait]
pub trait HistoryViewer: Send + Sync {
    async fn history(&self, issue_id: &str) -> Result<Vec<Revision>>;
    async fn as_of(&self, issue_id: &str, commit: &str) -> Result<Option<bd_core::Issue>>;
    async fn diff(&self, from: &str, to: &str) -> Result<Vec<IssueDiff>>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Revision {
    pub commit: String,
    pub author: String,
    pub message: String,
    pub committed_at: DateTime<Utc>,
    pub issue: bd_core::Issue,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IssueDiff {
    pub issue_id: String,
    pub change: ChangeKind,
    pub fields: Vec<FieldChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FieldChange {
    pub field: String,
    pub from: Option<String>,
    pub to: Option<String>,
}
