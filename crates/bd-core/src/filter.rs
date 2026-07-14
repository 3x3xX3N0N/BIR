use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{IssueType, Priority, Status};

/// How a listing orders its results.
///
/// Every variant is pushed into SQL's `ORDER BY`. That is not an optimization:
/// a sort applied in memory *after* the database applied a `LIMIT` returns the
/// wrong page, silently, and nothing about the output says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SortPolicy {
    /// Recent work sorts by priority; older work sorts by age. Keeps urgent new
    /// items visible without letting old ones starve.
    #[default]
    Hybrid,
    Priority,
    Oldest,

    // The next two point in *opposite* directions, and each one points the way
    // its question does. They are not a symmetric pair and must not be made one.
    /// Least-recently-updated first. The order `bd stale` asks for: "what has
    /// nobody touched?" — so the answer starts with what nobody touched longest.
    Updated,
    /// Most-recently-closed first. The order "what did we just finish?" asks for.
    /// Issues that are still open have no close time and sort last.
    Closed,
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
            "updated" => Ok(SortPolicy::Updated),
            "closed" => Ok(SortPolicy::Closed),
            other => Err(crate::Error::Invalid(format!("unknown sort policy: {other}"))),
        }
    }
}

/// A structured query, pushed down into SQL by the storage layer.
///
/// Anything expressible here is answered by the database. The query DSL falls
/// back to an in-memory predicate only for what this cannot express — and even
/// then it uses a filter to pre-shrink the candidate set first.
/// # Semantics
///
/// Every field is a conjunct: a filter is the AND of its non-empty parts. The
/// details below are stated here, not in the backend, because `bd-query` decides
/// whether a query is *fully* answerable in SQL by reasoning about them. A
/// backend that quietly disagrees turns a pushdown into a wrong answer.
///
/// - **String equality is exact**: byte-for-byte, not case-folded.
/// - **`text` is a case-insensitive substring search** over title/description —
///   ASCII-insensitive, i.e. whatever SQL `LIKE` does.
/// - **Date bounds are strict**: `*_after` is `col > t`, `*_before` is
///   `col < t`. A NULL timestamp satisfies neither.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssueFilter {
    pub status: Option<Status>,
    /// Match any of these statuses (an OR).
    pub statuses: Vec<Status>,
    /// Match none of these. `NOT status=closed` and `status!=closed` both land
    /// here; without it, negation could not be pushed down at all.
    pub exclude_statuses: Vec<Status>,
    pub priority: Option<Priority>,
    /// Urgency bounds, inclusive — and note that they are numeric bounds *in
    /// reverse*, because P0 is the most urgent priority, not the least:
    ///
    /// - `min_priority` = "at least this important" = `priority <= n`
    /// - `max_priority` = "at most this important"  = `priority >= n`
    ///
    /// Reading either as a plain numeric min/max silently inverts the filter and
    /// hands back exactly the issues the user was trying to exclude.
    ///
    /// A bound may legitimately fall outside P0-P4: `priority>4` is a query with
    /// no answers, and encoding it as `priority >= 5` lets the database say so
    /// without a scan.
    pub min_priority: Option<Priority>,
    pub max_priority: Option<Priority>,
    pub issue_type: Option<IssueType>,
    pub exclude_types: Vec<IssueType>,
    pub assignee: Option<String>,
    pub owner: Option<String>,

    /// Must carry *every* one of these labels.
    pub labels_all: Vec<String>,
    /// Must carry *at least one* of these.
    pub labels_any: Vec<String>,

    pub parent: Option<String>,
    pub spec_id: Option<String>,
    pub has_metadata_key: Option<String>,

    /// The two halves of the tracker join key.
    ///
    /// Together they are how a sync tells "the issue I already have" from "a new
    /// one". Without them every tracker had to scan the whole workspace and index
    /// it in memory to answer a question the database can answer directly:
    /// `source_system = 'jira' AND external_ref = 'PROJ-12'`.
    ///
    /// `source_system` matches `''` like any other value — that is a real query
    /// ("beads no tracker owns"), not a wildcard.
    pub source_system: Option<String>,
    pub external_ref: Option<String>,

    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub updated_after: Option<DateTime<Utc>>,
    pub updated_before: Option<DateTime<Utc>>,
    pub closed_after: Option<DateTime<Utc>>,
    pub closed_before: Option<DateTime<Utc>>,

    /// Case-insensitive substring match across title/description.
    pub text: Option<String>,

    pub pinned: Option<bool>,
    pub ephemeral: Option<bool>,
    pub is_template: Option<bool>,

    /// `None` means "don't care" — the default. `bd ready` sets this to
    /// `Some(false)`.
    pub is_blocked: Option<bool>,

    /// Whether the issue is under an **unexpired** lease.
    ///
    /// A lease that has lapsed is not a claim: the agent holding it is presumed
    /// dead, and the work returns to whoever asks for it next. So this is a
    /// predicate about the *clock*, not about the `lease_expires_at` column
    /// being set — an issue with a lease that expired an hour ago answers
    /// `false` here, which is what makes leases self-healing.
    ///
    /// `Some(false)` is what `bd ready` needs: an issue somebody currently holds
    /// is not claimable, so offering it to a second agent is how two agents end
    /// up doing the same work.
    pub lease_active: Option<bool>,

    pub sort: SortPolicy,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

impl IssueFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The filter behind `bd ready`: workable, unblocked, unheld, not deferred,
    /// not pinned, and not an infrastructure bead.
    ///
    /// `lease_active: Some(false)` is the one that is easy to leave out and
    /// expensive to leave out. `bd ready` means *claimable*, and an issue whose
    /// lease is still running is claimed — `claim_issue` will refuse it. Showing
    /// it anyway hands two agents the same bead and lets one of them find out by
    /// failing. Note the complement is *not* "my own work": an agent looking for
    /// what it already holds is asking `bd prime`, not `bd ready`.
    pub fn ready() -> Self {
        IssueFilter {
            statuses: vec![Status::Open, Status::InProgress],
            is_blocked: Some(false),
            pinned: Some(false),
            ephemeral: Some(false),
            lease_active: Some(false),
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
