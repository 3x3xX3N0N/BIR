use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const MAX_TITLE_LEN: usize = 500;
pub const MAX_DEPENDENCY_TYPE_LEN: usize = 50;

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Where an issue sits in its lifecycle.
///
/// Beads also supports user-defined statuses. Rather than model those as a
/// separate parallel concept the way upstream does, an unrecognized status is
/// carried as `Custom` and resolved against the workspace's status config when
/// its category is needed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(from = "String", into = "String")]
pub enum Status {
    #[default]
    Open,
    InProgress,
    Blocked,
    Deferred,
    Closed,
    Pinned,
    Hooked,
    Custom(String),
}

impl Status {
    pub fn as_str(&self) -> &str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in_progress",
            Status::Blocked => "blocked",
            Status::Deferred => "deferred",
            Status::Closed => "closed",
            Status::Pinned => "pinned",
            Status::Hooked => "hooked",
            Status::Custom(s) => s,
        }
    }

    /// The category drives visibility: `bd ready` shows `Active`/`Wip`, and
    /// `bd list` hides `Done` by default. Custom statuses get their category
    /// from workspace config, so this returns `Unspecified` for them.
    pub fn category(&self) -> StatusCategory {
        match self {
            Status::Open => StatusCategory::Active,
            Status::InProgress => StatusCategory::Wip,
            Status::Blocked | Status::Deferred | Status::Pinned | Status::Hooked => {
                StatusCategory::Frozen
            }
            Status::Closed => StatusCategory::Done,
            Status::Custom(_) => StatusCategory::Unspecified,
        }
    }

    pub fn is_closed(&self) -> bool {
        matches!(self, Status::Closed)
    }

    /// Statuses an issue can hold and still be claimable work.
    pub fn is_workable(&self) -> bool {
        matches!(self, Status::Open | Status::InProgress)
    }
}

impl From<String> for Status {
    fn from(s: String) -> Self {
        match s.as_str() {
            "open" => Status::Open,
            "in_progress" => Status::InProgress,
            "blocked" => Status::Blocked,
            "deferred" => Status::Deferred,
            "closed" => Status::Closed,
            "pinned" => Status::Pinned,
            "hooked" => Status::Hooked,
            _ => Status::Custom(s),
        }
    }
}

impl From<Status> for String {
    fn from(s: Status) -> String {
        s.as_str().to_string()
    }
}

impl std::str::FromStr for Status {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(Status::from(s.to_string()))
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusCategory {
    /// Not started, available to claim.
    Active,
    /// Work in progress.
    Wip,
    /// Terminal.
    Done,
    /// Alive but deliberately not claimable (deferred, pinned, blocked).
    Frozen,
    Unspecified,
}

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

/// P0 (critical) through P4 (trivial). Note that 0 is a *valid, meaningful*
/// value, so this must never be skipped during serialization the way an
/// `Option`-like "omitempty" field would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Priority(pub i32);

impl Priority {
    pub const CRITICAL: Priority = Priority(0);
    pub const HIGH: Priority = Priority(1);
    pub const NORMAL: Priority = Priority(2);
    pub const LOW: Priority = Priority(3);
    pub const TRIVIAL: Priority = Priority(4);

    pub const MIN: i32 = 0;
    pub const MAX: i32 = 4;

    pub fn new(v: i32) -> Result<Self> {
        if !(Self::MIN..=Self::MAX).contains(&v) {
            return Err(Error::PriorityOutOfRange(v));
        }
        Ok(Priority(v))
    }

    pub fn value(&self) -> i32 {
        self.0
    }
}

impl Default for Priority {
    fn default() -> Self {
        Priority::NORMAL
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "P{}", self.0)
    }
}

impl std::str::FromStr for Priority {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let t = s.trim();
        let digits = t.strip_prefix(['p', 'P']).unwrap_or(t);
        let v: i32 = digits
            .parse()
            .map_err(|_| Error::Invalid(format!("invalid priority: {s}")))?;
        Priority::new(v)
    }
}

// ---------------------------------------------------------------------------
// IssueType
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(from = "String", into = "String")]
pub enum IssueType {
    Bug,
    Feature,
    #[default]
    Task,
    Epic,
    Chore,
    Decision,
    Message,
    Molecule,
    Gate,
    Spike,
    Story,
    Milestone,
    /// Internal audit-trail record.
    Event,
    Custom(String),
}

impl IssueType {
    pub fn as_str(&self) -> &str {
        match self {
            IssueType::Bug => "bug",
            IssueType::Feature => "feature",
            IssueType::Task => "task",
            IssueType::Epic => "epic",
            IssueType::Chore => "chore",
            IssueType::Decision => "decision",
            IssueType::Message => "message",
            IssueType::Molecule => "molecule",
            IssueType::Gate => "gate",
            IssueType::Spike => "spike",
            IssueType::Story => "story",
            IssueType::Milestone => "milestone",
            IssueType::Event => "event",
            IssueType::Custom(s) => s,
        }
    }

    /// Types that never surface as claimable work. Infrastructure beads
    /// (molecules, gates, events) are bookkeeping, not tasks.
    pub fn excluded_from_ready(&self) -> bool {
        matches!(
            self,
            IssueType::Molecule | IssueType::Gate | IssueType::Event | IssueType::Message
        )
    }

    pub fn is_builtin(&self) -> bool {
        !matches!(self, IssueType::Custom(_))
    }
}

impl From<String> for IssueType {
    fn from(s: String) -> Self {
        // Normalize the aliases upstream accepts, so `enhancement` and
        // `feature` don't become two distinct types in the database.
        match s.to_lowercase().as_str() {
            "bug" | "defect" => IssueType::Bug,
            "feature" | "enhancement" => IssueType::Feature,
            "task" => IssueType::Task,
            "epic" => IssueType::Epic,
            "chore" => IssueType::Chore,
            "decision" | "adr" => IssueType::Decision,
            "message" => IssueType::Message,
            "molecule" => IssueType::Molecule,
            "gate" => IssueType::Gate,
            "spike" => IssueType::Spike,
            "story" => IssueType::Story,
            "milestone" => IssueType::Milestone,
            "event" => IssueType::Event,
            _ => IssueType::Custom(s),
        }
    }
}

impl From<IssueType> for String {
    fn from(t: IssueType) -> String {
        t.as_str().to_string()
    }
}

impl std::str::FromStr for IssueType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(IssueType::from(s.to_string()))
    }
}

impl std::fmt::Display for IssueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Dependency graph
// ---------------------------------------------------------------------------

/// The kind of edge between two beads.
///
/// Only four of these affect whether work is claimable — see
/// [`DependencyType::affects_ready_work`]. The rest are associations, links,
/// or provenance, and exist to be traversed and displayed, not to gate work.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(from = "String", into = "String")]
pub enum DependencyType {
    // --- workflow: these gate readiness ---
    /// A blocks B: B cannot start until A closes.
    #[default]
    Blocks,
    /// Structural containment. Propagates blocked-ness down to children.
    ParentChild,
    /// B runs only if A *fails*. See [`is_failure_close`].
    ConditionalBlocks,
    /// Fan-out gate: wait for a spawner's children to finish.
    WaitsFor,

    // --- association ---
    Related,
    DiscoveredFrom,

    // --- graph links ---
    RepliesTo,
    RelatesTo,
    Duplicates,
    Supersedes,

    // --- entity ---
    AuthoredBy,
    AssignedTo,
    ApprovedBy,
    Attests,

    // --- convoy / reference / delegation ---
    Tracks,
    Until,
    CausedBy,
    Validates,
    DelegatedFrom,

    /// Custom edge types are legal; anything non-empty and <= 50 chars.
    Custom(String),
}

impl DependencyType {
    pub fn as_str(&self) -> &str {
        match self {
            DependencyType::Blocks => "blocks",
            DependencyType::ParentChild => "parent-child",
            DependencyType::ConditionalBlocks => "conditional-blocks",
            DependencyType::WaitsFor => "waits-for",
            DependencyType::Related => "related",
            DependencyType::DiscoveredFrom => "discovered-from",
            DependencyType::RepliesTo => "replies-to",
            DependencyType::RelatesTo => "relates-to",
            DependencyType::Duplicates => "duplicates",
            DependencyType::Supersedes => "supersedes",
            DependencyType::AuthoredBy => "authored-by",
            DependencyType::AssignedTo => "assigned-to",
            DependencyType::ApprovedBy => "approved-by",
            DependencyType::Attests => "attests",
            DependencyType::Tracks => "tracks",
            DependencyType::Until => "until",
            DependencyType::CausedBy => "caused-by",
            DependencyType::Validates => "validates",
            DependencyType::DelegatedFrom => "delegated-from",
            DependencyType::Custom(s) => s,
        }
    }

    /// Whether this edge participates in `is_blocked` computation.
    ///
    /// This is the single most load-bearing predicate in the system: it decides
    /// what `bd ready` will and will not show you.
    pub fn affects_ready_work(&self) -> bool {
        matches!(
            self,
            DependencyType::Blocks
                | DependencyType::ParentChild
                | DependencyType::ConditionalBlocks
                | DependencyType::WaitsFor
        )
    }

    /// A hard blocker. `parent-child` affects readiness by *propagation* but is
    /// not itself a blocking edge, which is why this is narrower than
    /// [`affects_ready_work`](Self::affects_ready_work).
    pub fn is_blocking_edge(&self) -> bool {
        matches!(
            self,
            DependencyType::Blocks | DependencyType::ConditionalBlocks | DependencyType::WaitsFor
        )
    }

    pub fn is_well_known(&self) -> bool {
        !matches!(self, DependencyType::Custom(_))
    }

    pub fn validate(&self) -> Result<()> {
        let s = self.as_str();
        if s.is_empty() || s.len() > MAX_DEPENDENCY_TYPE_LEN {
            return Err(Error::InvalidDependencyType(s.to_string()));
        }
        Ok(())
    }

    /// Every well-known edge type, for `--help` and shell completion.
    pub fn all_well_known() -> &'static [DependencyType] {
        use DependencyType::*;
        &[
            Blocks,
            ParentChild,
            ConditionalBlocks,
            WaitsFor,
            Related,
            DiscoveredFrom,
            RepliesTo,
            RelatesTo,
            Duplicates,
            Supersedes,
            AuthoredBy,
            AssignedTo,
            ApprovedBy,
            Attests,
            Tracks,
            Until,
            CausedBy,
            Validates,
            DelegatedFrom,
        ]
    }
}

impl From<String> for DependencyType {
    fn from(s: String) -> Self {
        match s.as_str() {
            "blocks" => DependencyType::Blocks,
            "parent-child" => DependencyType::ParentChild,
            "conditional-blocks" => DependencyType::ConditionalBlocks,
            "waits-for" => DependencyType::WaitsFor,
            "related" => DependencyType::Related,
            "discovered-from" => DependencyType::DiscoveredFrom,
            "replies-to" => DependencyType::RepliesTo,
            "relates-to" => DependencyType::RelatesTo,
            "duplicates" => DependencyType::Duplicates,
            "supersedes" => DependencyType::Supersedes,
            "authored-by" => DependencyType::AuthoredBy,
            "assigned-to" => DependencyType::AssignedTo,
            "approved-by" => DependencyType::ApprovedBy,
            "attests" => DependencyType::Attests,
            "tracks" => DependencyType::Tracks,
            "until" => DependencyType::Until,
            "caused-by" => DependencyType::CausedBy,
            "validates" => DependencyType::Validates,
            "delegated-from" => DependencyType::DelegatedFrom,
            _ => DependencyType::Custom(s),
        }
    }
}

impl From<DependencyType> for String {
    fn from(t: DependencyType) -> String {
        t.as_str().to_string()
    }
}

impl std::str::FromStr for DependencyType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let t = DependencyType::from(s.to_string());
        t.validate()?;
        Ok(t)
    }
}

impl std::fmt::Display for DependencyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A directed edge: `issue_id` --[type]--> `depends_on_id`.
///
/// Upstream splits the target across two nullable columns
/// (`depends_on_issue_id` / `depends_on_wisp_id`) because ephemeral beads live
/// in shadow tables. We keep a single target here and let the storage backend
/// decide where the row physically lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub issue_id: String,
    pub depends_on_id: String,
    #[serde(rename = "type")]
    pub dep_type: DependencyType,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_by: String,
    /// Edge-type-specific JSON payload (e.g. the gate policy for `waits-for`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metadata: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thread_id: String,
}

impl Dependency {
    pub fn new(issue_id: impl Into<String>, depends_on_id: impl Into<String>, dep_type: DependencyType) -> Result<Self> {
        let issue_id = issue_id.into();
        let depends_on_id = depends_on_id.into();
        if issue_id == depends_on_id {
            return Err(Error::SelfDependency(issue_id));
        }
        dep_type.validate()?;
        Ok(Dependency {
            issue_id,
            depends_on_id,
            dep_type,
            created_at: Utc::now(),
            created_by: String::new(),
            metadata: String::new(),
            thread_id: String::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Ancillary types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// Upstream accepts both string and numeric ids here; we normalize to
    /// string on the way in.
    #[serde(deserialize_with = "de_string_or_number")]
    pub id: String,
    pub issue_id: String,
    pub author: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

fn de_string_or_number<'de, D>(d: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or integer id")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
    }
    d.deserialize_any(V)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Created,
    StatusChanged,
    PriorityChanged,
    AssigneeChanged,
    Closed,
    Reopened,
    DependencyAdded,
    DependencyRemoved,
    LabelAdded,
    LabelRemoved,
    Commented,
    Deleted,
}

/// An entry in the audit trail. Events are written **in the same transaction**
/// as the mutation that produced them — never after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub issue_id: String,
    pub event_type: EventType,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_value: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BondType {
    Sequential,
    Parallel,
    Conditional,
    Root,
}

/// Lineage for compound (bonded) molecules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BondRef {
    #[serde(alias = "proto_id")]
    pub source_id: String,
    pub bond_type: BondType,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bond_point: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MolType {
    Swarm,
    Patrol,
    Work,
}

/// TTL classification for ephemeral beads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WispType {
    Heartbeat,
    Ping,
    Patrol,
    GcReport,
    Recovery,
    Error,
    Escalation,
}

impl WispType {
    /// How long before the garbage collector may reap this.
    pub fn ttl(&self) -> chrono::Duration {
        match self {
            WispType::Heartbeat | WispType::Ping => chrono::Duration::hours(6),
            WispType::Patrol | WispType::GcReport => chrono::Duration::hours(24),
            WispType::Recovery | WispType::Error | WispType::Escalation => {
                chrono::Duration::days(7)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkType {
    /// One claimant at a time.
    Mutex,
    /// Many agents may work it simultaneously.
    OpenCompetition,
}

// ---------------------------------------------------------------------------
// Issue
// ---------------------------------------------------------------------------

/// A bead.
///
/// Note what is *absent*: there is no `is_blocked` field. That column exists in
/// the database as a denormalized cache of the dependency graph, maintained by
/// the storage layer to a fixpoint on every write. It is derived state, and
/// putting it on the domain type would invite callers to trust a stale copy.
/// Ask the storage layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Issue {
    pub id: String,

    // --- content ---
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub design: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub acceptance_criteria: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,

    // --- classification ---
    #[serde(default)]
    pub status: Status,
    /// Not skipped when zero: P0 is a real priority.
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub issue_type: IssueType,

    // --- ownership ---
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub assignee: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_minutes: Option<i32>,

    // --- lifecycle timestamps ---
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub close_reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub closed_by_session: String,

    // --- claim / lease ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<DateTime<Utc>>,

    // --- scheduling ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,
    /// Hides the issue from `bd ready` until this time passes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_until: Option<DateTime<Utc>>,

    // --- external linkage ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_system: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spec_id: String,

    /// Arbitrary JSON, validated as well-formed on write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,

    // --- flags ---
    /// Lives outside the commit graph; reaped by TTL.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ephemeral: bool,
    /// Persisted, but not version-controlled.
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_history: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_template: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wisp_type: Option<WispType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mol_type: Option<MolType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_type: Option<WorkType>,

    // --- hydrated relations (not columns) ---
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Dependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Comment>,

    /// SHA-256 over the substantive content. Used for cross-clone identity,
    /// *not* for the id. Internal; never serialized.
    #[serde(skip)]
    pub content_hash: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Issue {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        let now = Utc::now();
        Issue {
            id: id.into(),
            title: title.into(),
            created_at: now,
            updated_at: now,
            ..Default::default()
        }
    }

    /// Validate an issue authored locally. Stricter than [`validate_for_import`].
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() {
            return Err(Error::TitleEmpty);
        }
        if self.title.chars().count() > MAX_TITLE_LEN {
            return Err(Error::TitleTooLong {
                max: MAX_TITLE_LEN,
                got: self.title.chars().count(),
            });
        }
        Priority::new(self.priority.0)?;
        if self.ephemeral && self.no_history {
            return Err(Error::EphemeralAndNoHistory);
        }
        Ok(())
    }

    /// Validation for records arriving from another repo (import, federation).
    ///
    /// Deliberately more permissive: we trust the chain below us. An unknown
    /// issue type from a peer is *their* custom type, not our typo, so it is
    /// accepted rather than rejected.
    pub fn validate_for_import(&self) -> Result<()> {
        if self.title.trim().is_empty() {
            return Err(Error::TitleEmpty);
        }
        if self.ephemeral && self.no_history {
            return Err(Error::EphemeralAndNoHistory);
        }
        Ok(())
    }

    /// Stable hash of the substantive content, excluding id, timestamps, and
    /// derived state — so the same bead authored on two clones hashes alike.
    pub fn compute_content_hash(&self) -> String {
        let mut h = Sha256::new();
        // Null bytes separate fields so that ("ab","c") and ("a","bc") differ.
        let mut field = |s: &str| {
            h.update(s.as_bytes());
            h.update([0u8]);
        };
        field(&self.title);
        field(&self.description);
        field(&self.design);
        field(&self.acceptance_criteria);
        field(&self.notes);
        field(self.status.as_str());
        field(&self.priority.0.to_string());
        field(self.issue_type.as_str());
        field(&self.assignee);
        field(&self.owner);
        field(&self.close_reason);
        field(&self.spec_id);
        field(&self.source_system);
        field(self.external_ref.as_deref().unwrap_or(""));
        field(
            &self
                .metadata
                .as_ref()
                .map(|m| m.to_string())
                .unwrap_or_default(),
        );
        let mut labels = self.labels.clone();
        labels.sort();
        for l in &labels {
            field(l);
        }
        format!("{:x}", h.finalize())
    }

    /// Whether this bead could be claimable, ignoring the dependency graph.
    /// The graph half of the question lives in the `is_blocked` column.
    pub fn is_potentially_ready(&self, now: DateTime<Utc>) -> bool {
        self.status.is_workable()
            && !self.pinned
            && !self.ephemeral
            && !self.issue_type.excluded_from_ready()
            && self.defer_until.is_none_or(|d| d <= now)
    }
}

/// Does this close reason indicate failure?
///
/// `conditional-blocks` edges ("run B only if A fails") have no explicit
/// failure signal, so intent is inferred from the close reason's wording.
/// Imprecise by nature — that is upstream's design, and changing it would
/// silently alter which work becomes ready.
pub fn is_failure_close(reason: &str) -> bool {
    const FAILURE_WORDS: [&str; 9] = [
        "failed", "rejected", "wontfix", "canceled", "abandoned", "blocked", "error", "timeout",
        "aborted",
    ];
    let lower = reason.to_lowercase();
    FAILURE_WORDS.iter().any(|w| lower.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_zero_is_valid_and_serializes() {
        let p = Priority::new(0).unwrap();
        assert_eq!(p, Priority::CRITICAL);
        assert!(Priority::new(5).is_err());
        assert!(Priority::new(-1).is_err());
        // P0 must survive a serialization round-trip; an "omitempty"-style
        // skip here would silently promote every P0 to the default P2.
        let issue = Issue {
            priority: Priority::CRITICAL,
            ..Issue::new("bd-1", "t")
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"priority\":0"), "P0 dropped from JSON: {json}");
    }

    #[test]
    fn only_four_edge_types_gate_readiness() {
        let gating: Vec<_> = DependencyType::all_well_known()
            .iter()
            .filter(|d| d.affects_ready_work())
            .cloned()
            .collect();
        assert_eq!(
            gating,
            vec![
                DependencyType::Blocks,
                DependencyType::ParentChild,
                DependencyType::ConditionalBlocks,
                DependencyType::WaitsFor,
            ]
        );
        // parent-child propagates blocked-ness but is not itself a hard blocker.
        assert!(!DependencyType::ParentChild.is_blocking_edge());
        assert!(DependencyType::Blocks.is_blocking_edge());
        // Association edges must never gate work.
        assert!(!DependencyType::DiscoveredFrom.affects_ready_work());
        assert!(!DependencyType::Related.affects_ready_work());
    }

    #[test]
    fn issue_type_aliases_normalize() {
        assert_eq!(IssueType::from("enhancement".to_string()), IssueType::Feature);
        assert_eq!(IssueType::from("adr".to_string()), IssueType::Decision);
        assert_eq!(
            IssueType::from("widget".to_string()),
            IssueType::Custom("widget".into())
        );
    }

    #[test]
    fn content_hash_is_order_independent_for_labels() {
        let mut a = Issue::new("bd-1", "title");
        a.labels = vec!["x".into(), "y".into()];
        let mut b = Issue::new("bd-2", "title");
        b.labels = vec!["y".into(), "x".into()];
        assert_eq!(a.compute_content_hash(), b.compute_content_hash());
    }

    #[test]
    fn failure_close_detection() {
        assert!(is_failure_close("Failed to reproduce"));
        assert!(is_failure_close("WONTFIX"));
        assert!(!is_failure_close("done"));
        assert!(!is_failure_close("shipped"));
    }

    #[test]
    fn ephemeral_and_no_history_are_mutually_exclusive() {
        let mut i = Issue::new("bd-1", "t");
        i.ephemeral = true;
        i.no_history = true;
        assert_eq!(i.validate(), Err(Error::EphemeralAndNoHistory));
    }

    #[test]
    fn self_dependency_rejected() {
        assert!(Dependency::new("bd-1", "bd-1", DependencyType::Blocks).is_err());
        assert!(Dependency::new("bd-1", "bd-2", DependencyType::Blocks).is_ok());
    }
}
