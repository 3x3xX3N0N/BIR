use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{IssueType, Priority, Status};

/// How `bd ready` orders claimable work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SortPolicy {
    /// Recent work sorts by priority; older work sorts by age. Keeps urgent new
    /// items visible without letting old ones starve.
    #[default]
    Hybrid,
    Priority,
    Oldest,
}

impl SortPolicy {
    /// Issues newer than this are ranked by priority; older ones by age.
    pub const HYBRID_RECENCY_WINDOW: chrono::Duration = chrono::Duration::hours(48);
}

impl std::str::FromStr for SortPolicy {
    type Err = crate::Error;
    fn from_str(s: &str) -> crate::Result<Self> {
        match s {
            "hybrid" => Ok(SortPolicy::Hybrid),
            "priority" => Ok(SortPolicy::Priority),
            "oldest" => Ok(SortPolicy::Oldest),
            other => Err(crate::Error::Invalid(format!("unknown sort policy: {other}"))),
        }
    }
}

/// A structured query, pushed down into SQL by the storage layer.
///
/// Anything expressible here is answered by the database. The query DSL falls
/// back to an in-memory predicate only for what this cannot express — and even
/// then it uses a filter to pre-shrink the candidate set first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssueFilter {
    pub status: Option<Status>,
    /// Match any of these statuses (an OR).
    pub statuses: Vec<Status>,
    pub priority: Option<Priority>,
    pub min_priority: Option<Priority>,
    pub issue_type: Option<IssueType>,
    pub assignee: Option<String>,
    pub owner: Option<String>,

    /// Must carry *every* one of these labels.
    pub labels_all: Vec<String>,
    /// Must carry *at least one* of these.
    pub labels_any: Vec<String>,

    pub parent: Option<String>,
    pub spec_id: Option<String>,
    pub has_metadata_key: Option<String>,

    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub updated_after: Option<DateTime<Utc>>,
    pub updated_before: Option<DateTime<Utc>>,
    pub closed_after: Option<DateTime<Utc>>,
    pub closed_before: Option<DateTime<Utc>>,

    /// Substring match across title/description.
    pub text: Option<String>,

    pub pinned: Option<bool>,
    pub ephemeral: Option<bool>,
    pub is_template: Option<bool>,

    /// `None` means "don't care" — the default. `bd ready` sets this to
    /// `Some(false)`.
    pub is_blocked: Option<bool>,

    pub sort: SortPolicy,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

impl IssueFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The filter behind `bd ready`: workable, unblocked, not deferred, not
    /// pinned, and not an infrastructure bead.
    pub fn ready() -> Self {
        IssueFilter {
            statuses: vec![Status::Open, Status::InProgress],
            is_blocked: Some(false),
            pinned: Some(false),
            ephemeral: Some(false),
            ..Default::default()
        }
    }

    /// The complement: workable but gated by the graph.
    pub fn blocked() -> Self {
        IssueFilter {
            statuses: vec![Status::Open, Status::InProgress],
            is_blocked: Some(true),
            ..Default::default()
        }
    }

    pub fn with_limit(mut self, n: u32) -> Self {
        self.limit = Some(n);
        self
    }

    pub fn with_status(mut self, s: Status) -> Self {
        self.status = Some(s);
        self
    }

    pub fn with_assignee(mut self, a: impl Into<String>) -> Self {
        self.assignee = Some(a.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        *self == IssueFilter::default()
    }
}
