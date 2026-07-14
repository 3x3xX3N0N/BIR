//! The storage seam.
//!
//! # Design
//!
//! A small **core** that every backend must implement, plus **capabilities**
//! that backends may or may not offer. SQLite is a complete, first-class store
//! that simply has no commit graph; Dolt is the same store plus branching,
//! merging, and remotes. Neither is a degraded version of the other.
//!
//! Four rules govern this boundary. Upstream's Rust spike learned each of them
//! the hard way — every one was a real bug — so they are stated as rules, not
//! suggestions:
//!
//! 1. **Construction is on the seam.** [`open`] and [`init`] are seam
//!    operations. The moment a caller has to name a concrete backend to get a
//!    store, every entry point in the program starts naming backends.
//! 2. **Identity is on the seam.** Who is acting is construction-time config
//!    ([`Identity`]), not a parameter threaded through every write.
//! 3. **The locator is backend-neutral and self-describing.** A workspace
//!    records which backend created it; opening *reads* that. `--backend` and
//!    the environment apply only at `init`. Otherwise a stray env var silently
//!    reinterprets an existing database as the wrong engine.
//! 4. **Capabilities optimize core behavior; they never gate it.** For any
//!    capability used inside a core command there must be a core-only fallback,
//!    or the whole command is a capability command. A core command that quietly
//!    does less on SQLite is the bug this rule exists to prevent.

pub mod capability;
pub mod error;
pub mod locator;
pub mod stats;

use async_trait::async_trait;
use bd_core::{Dependency, DependencyType, Event, Issue, IssueFilter};
use chrono::{DateTime, Utc};

pub use capability::{Conflict, HistoryViewer, RemoteStore, VersionControl};
pub use error::{Error, Result};
pub use locator::{Backend, Locator};
pub use stats::Stats;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_can_say_leave_it_set_it_and_empty_it() {
        // The distinction Option<T> could not make, and the reason `bd undefer`
        // and `bd unassign` were unwritable.
        let current = Some("alice".to_string());
        assert_eq!(Field::Keep.resolve(current.clone()), Some("alice".into()));
        assert_eq!(
            Field::Set("bob".to_string()).resolve(current.clone()),
            Some("bob".into())
        );
        assert_eq!(Field::<String>::Clear.resolve(current), None);
    }

    #[test]
    fn a_missing_flag_keeps_rather_than_clears() {
        // The whole point: an absent CLI flag must never silently empty a field.
        let from_absent_flag: Field<String> = None.into();
        assert_eq!(from_absent_flag, Field::Keep);
        let from_present_flag: Field<String> = Some("x".to_string()).into();
        assert_eq!(from_present_flag, Field::Set("x".into()));
    }

    #[test]
    fn undefer_clears_only_defer_until() {
        let p = IssuePatch::undefer();
        assert_eq!(p.defer_until, Field::Clear);
        assert!(p.assignee.is_keep());
        assert!(p.title.is_none());
        assert!(!p.is_empty());
    }
}

/// Who is performing an operation. Set once, when the store is opened.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    /// Actor id recorded on events and claims — typically an agent name or a
    /// git email.
    pub actor: String,
    /// Session id, used to attribute a close to a specific agent run.
    pub session: Option<String>,
}

impl Identity {
    pub fn new(actor: impl Into<String>) -> Self {
        Identity {
            actor: actor.into(),
            session: None,
        }
    }
}

/// A claim on an issue, held for a bounded time.
///
/// Leases expire. An agent that dies mid-task does not hold its work hostage —
/// the lease lapses and the issue returns to `bd ready`. This is why claiming
/// is a first-class store operation rather than "set assignee and hope".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub issue_id: String,
    pub holder: String,
    pub expires_at: DateTime<Utc>,
}

/// One field of a patch: leave it, set it, or empty it.
///
/// `Option<T>` cannot express this. In a patch, `None` already means "leave this
/// alone" — which uses up the only empty case and leaves no way to say "set this
/// back to nothing". That gap is not academic: it is exactly why `bd undefer`
/// and `bd unassign` could not be written. Clearing a field is a real operation
/// and it needs a real representation.
///
/// `Option<Option<T>>` would technically work and would be unreadable at every
/// call site.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Field<T> {
    /// Not mentioned in this patch.
    #[default]
    Keep,
    Set(T),
    /// Explicitly emptied — to SQL NULL, or to the type's empty value.
    Clear,
}

impl<T> Field<T> {
    pub fn is_keep(&self) -> bool {
        matches!(self, Field::Keep)
    }

    pub fn as_set(&self) -> Option<&T> {
        match self {
            Field::Set(v) => Some(v),
            _ => None,
        }
    }

    /// Apply this patch field to whatever is currently stored.
    pub fn resolve(self, current: Option<T>) -> Option<T> {
        match self {
            Field::Keep => current,
            Field::Set(v) => Some(v),
            Field::Clear => None,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Field<U> {
        match self {
            Field::Keep => Field::Keep,
            Field::Set(v) => Field::Set(f(v)),
            Field::Clear => Field::Clear,
        }
    }

    /// Build from a source that is *authoritative* about the whole issue — an
    /// import record, a sync from an upstream tracker.
    ///
    /// Here `None` means "this issue has no estimate", so the field is
    /// **cleared**. That is the exact opposite of [`Field::from`]`(Option<T>)`,
    /// where `None` came from an unmentioned CLI flag and must be *kept*.
    ///
    /// The two look identical at the call site and mean opposite things, which
    /// is precisely why this one has a name. Import an issue whose estimate was
    /// removed upstream through the wrong one and the stale estimate survives
    /// forever, because nothing will ever say "clear it" again.
    pub fn authoritative(o: Option<T>) -> Self {
        match o {
            Some(v) => Field::Set(v),
            None => Field::Clear,
        }
    }
}

/// `Some(v)` sets, `None` keeps — so a plain CLI flag converts without ceremony.
/// Clearing is never accidental; it has to be asked for by name.
impl<T> From<Option<T>> for Field<T> {
    fn from(o: Option<T>) -> Self {
        match o {
            Some(v) => Field::Set(v),
            None => Field::Keep,
        }
    }
}

/// Fields to change on an issue.
///
/// Nullable fields use [`Field`] so they can be cleared. The rest stay
/// `Option<T>` on purpose: an issue always has a title, a status, a priority and
/// a type, so "clear the status" is not a thing anyone should be able to say.
/// The type refuses to represent it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssuePatch {
    // Always present; `None` means "leave alone" and clearing is meaningless.
    pub title: Option<String>,
    pub status: Option<bd_core::Status>,
    pub priority: Option<bd_core::Priority>,
    pub issue_type: Option<bd_core::IssueType>,
    pub pinned: Option<bool>,
    /// Whether the bead lives outside the commit graph and is reaped by TTL.
    /// Clearing this is what `bd promote` does to a wisp — and it is why
    /// `promote` was unwritable while this field did not exist.
    pub ephemeral: Option<bool>,

    // Free text: empty is a legitimate value, so these are clearable.
    pub description: Field<String>,
    pub design: Field<String>,
    pub acceptance_criteria: Field<String>,
    pub notes: Field<String>,
    pub close_reason: Field<String>,

    // Nullable columns.
    pub assignee: Field<String>,
    pub estimated_minutes: Field<i32>,
    pub due_at: Field<DateTime<Utc>>,
    pub defer_until: Field<DateTime<Utc>>,
    pub metadata: Field<serde_json::Value>,
    pub spec_id: Field<String>,
    /// The remote's id for this issue (`"PROJ-123"`, `"42"`).
    pub external_ref: Field<String>,
    /// Which remote that id belongs to (`"jira"`, `"github"`).
    ///
    /// Identity across a tracker boundary is the **pair** (`source_system`,
    /// `external_ref`), and both halves have to be writable on an *existing*
    /// issue — that is precisely what `push` does when it creates a remote issue
    /// for a locally-authored bead. With only `external_ref` here, a pushed bead
    /// could record which id it was given but not which system gave it, so the
    /// next `pull` did not recognize the issue it had just created and filed a
    /// duplicate. Every tracker independently invented a marker (a label, a
    /// metadata blob, a guess at the shape of the ref) to work around its
    /// absence; all of them are gone.
    pub source_system: Field<String>,
    /// The TTL class of an ephemeral bead. Clearable, because a promoted wisp
    /// has no TTL — leaving one behind would let `bd gc` reap the real bead it
    /// just became.
    pub wisp_type: Field<bd_core::WispType>,
}

impl IssuePatch {
    pub fn is_empty(&self) -> bool {
        *self == IssuePatch::default()
    }

    /// `bd unclaim`: drop the assignee and let the lease go.
    pub fn unassign() -> Self {
        IssuePatch {
            assignee: Field::Clear,
            ..Default::default()
        }
    }

    /// `bd undefer`: bring an issue back into `bd ready` now.
    pub fn undefer() -> Self {
        IssuePatch {
            defer_until: Field::Clear,
            ..Default::default()
        }
    }

    /// `bd promote`: turn a wisp into a real bead.
    ///
    /// Both halves are required. Clearing `ephemeral` without clearing
    /// `wisp_type` leaves a bead that `bd ready` will show and `bd gc` will
    /// still delete out from under whoever claimed it, because the TTL is read
    /// from the wisp type.
    pub fn promote() -> Self {
        IssuePatch {
            ephemeral: Some(false),
            wisp_type: Field::Clear,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// The core seam
// ---------------------------------------------------------------------------

/// Everything a beads backend must be able to do.
///
/// Object-safe on purpose: the CLI holds a `Box<dyn Storage>` and never learns
/// which engine it got. That is the whole point of rule 1.
#[async_trait]
pub trait Storage: Send + Sync {
    // --- identity of the store itself ---

    fn backend(&self) -> Backend;
    fn identity(&self) -> &Identity;

    // --- issues ---

    async fn create_issue(&self, issue: &Issue) -> Result<Issue>;
    async fn get_issue(&self, id: &str) -> Result<Option<Issue>>;

    /// Many issues by id, in one query.
    ///
    /// [`IssueFilter`] cannot name a set of ids, so without this a caller that
    /// *has* ids and wants issues had two options, both bad: one `get_issue` per
    /// id (an N+1), or a full scan indexed in memory (a scan of the workspace to
    /// answer a question about four beads). `bd children`, `bd epic`,
    /// `bd orphans` and `bd lint` all take ids from the graph and need the rows.
    ///
    /// Ids that do not exist are simply absent from the result — asking about a
    /// bead that was deleted is ordinary, not an error. Relations are **not**
    /// hydrated, exactly as in [`Storage::list_issues`].
    async fn get_issues(&self, ids: &[String]) -> Result<Vec<Issue>>;

    async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue>;
    async fn delete_issue(&self, id: &str) -> Result<()>;
    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>>;
    async fn count_issues(&self, filter: &IssueFilter) -> Result<u64>;

    /// Close an issue, recording why. The reason is not decoration: a
    /// `conditional-blocks` edge reads it to decide whether the *failure* path
    /// should now become ready.
    async fn close_issue(&self, id: &str, reason: &str) -> Result<Issue>;
    async fn reopen_issue(&self, id: &str) -> Result<Issue>;

    /// Mint an id that does not collide with anything currently stored.
    async fn next_id(&self, prefix: &str, title: &str, description: &str) -> Result<String>;

    // --- claims ---

    /// Take the issue, exclusively, for `lease` long.
    ///
    /// Fails with [`Error::AlreadyClaimed`] if someone else holds an unexpired
    /// lease — *unless* the issue's work type is open-competition.
    async fn claim_issue(&self, id: &str, lease: chrono::Duration) -> Result<Claim>;
    async fn renew_claim(&self, id: &str, lease: chrono::Duration) -> Result<Claim>;
    async fn release_claim(&self, id: &str) -> Result<()>;
    /// Sweep leases that have lapsed, returning the issues freed.
    async fn expire_claims(&self) -> Result<Vec<String>>;

    // --- dependencies ---

    async fn add_dependency(&self, dep: &Dependency) -> Result<()>;

    /// Remove **one** edge: the `(issue_id, depends_on_id, dep_type)` triple.
    ///
    /// The type is not optional and never should have been. Two beads may
    /// legitimately be joined by several edges at once — A `blocks` B *and* is
    /// `related` to B is an ordinary shape — and a delete keyed on the pair
    /// alone destroys all of them. That is silent data loss: the graph is the
    /// product, and no error is raised when the wrong edges go.
    ///
    /// Removing an edge that is not there is [`Error::NotFound`], not a no-op.
    /// A typo'd edge type must not report success.
    async fn remove_dependency(
        &self,
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &DependencyType,
    ) -> Result<()>;

    /// Edges *out of* this issue: what it depends on.
    async fn dependencies_of(&self, id: &str) -> Result<Vec<Dependency>>;
    /// Edges *into* this issue: what depends on it.
    async fn dependents_of(&self, id: &str) -> Result<Vec<Dependency>>;

    /// The whole edge set, or the edges out of the issues a filter selects, in
    /// one query.
    ///
    /// The filter selects the edges' *source* issues; an empty filter means
    /// every edge in the graph, including any whose endpoints have gone missing.
    /// That last part is the point of the empty case: `bd lint` and
    /// `bd graph check` exist to find edges a foreign import or a merge left
    /// dangling, and a loader that discovers edges by walking the issues that
    /// exist can only ever find half of them.
    ///
    /// `sort`, `limit` and `offset` on the filter are ignored: a set of edges is
    /// not a page of issues.
    async fn list_dependencies(&self, filter: &IssueFilter) -> Result<Vec<Dependency>>;

    /// Out-edges for each of `ids`, in one query. Batched for the same reason
    /// [`Storage::labels_of`] is; ids with no edges are absent from the result.
    async fn dependencies_of_many(&self, ids: &[String]) -> Result<Vec<(String, Vec<Dependency>)>>;

    /// Every cycle in the graph. Empty means the graph is a DAG.
    async fn find_cycles(&self) -> Result<Vec<Vec<String>>>;

    // --- labels ---

    async fn add_label(&self, issue_id: &str, label: &str) -> Result<()>;
    async fn remove_label(&self, issue_id: &str, label: &str) -> Result<()>;
    /// Every label in the workspace.
    async fn list_labels(&self) -> Result<Vec<String>>;

    /// Labels on each of `ids`, in one query.
    ///
    /// Batched deliberately. `list_issues` does not hydrate relations, so
    /// without this the only way to label a listing was to re-read every issue
    /// one at a time — which is how `bd export` silently dropped labels before
    /// anyone noticed, and how it would have become an N+1 once it didn't.
    async fn labels_of(&self, ids: &[String]) -> Result<Vec<(String, Vec<String>)>>;

    // --- comments and audit trail ---

    async fn add_comment(&self, issue_id: &str, text: &str) -> Result<bd_core::Comment>;

    /// Insert a comment, or update it if `id` already exists.
    ///
    /// This is what `bd import` needs and `add_comment` cannot give it:
    /// `add_comment` mints a fresh id and stamps the *importer* as the author,
    /// so re-importing the same file duplicates every comment and misattributes
    /// all of them. An upsert keyed on the incoming id makes import idempotent
    /// and keeps the original author.
    async fn upsert_comment(&self, comment: &bd_core::Comment) -> Result<()>;

    async fn list_comments(&self, issue_id: &str) -> Result<Vec<bd_core::Comment>>;

    /// Comments on each of `ids`, in one query. Ids with no comments are absent
    /// from the result. This is what makes `bd export` one query rather than one
    /// per issue.
    async fn comments_of_many(&self, ids: &[String])
    -> Result<Vec<(String, Vec<bd_core::Comment>)>>;

    async fn list_events(&self, issue_id: &str) -> Result<Vec<Event>>;

    // --- work queries ---
    //
    // These are the reason beads exists. Everything above is bookkeeping in
    // service of answering "what can I work on right now".

    /// Claimable work: open or in-progress, not blocked by the graph, not
    /// deferred into the future, not pinned, not an infrastructure bead.
    async fn ready_work(&self, filter: &IssueFilter) -> Result<Vec<Issue>>;
    /// The complement: real work that the graph is currently gating.
    async fn blocked_work(&self, filter: &IssueFilter) -> Result<Vec<Issue>>;

    /// Recompute the denormalized `is_blocked` cache across the whole graph.
    ///
    /// Normally maintained incrementally by writes. A full pass is required
    /// after any operation that changes rows behind the store's back — most
    /// importantly a merge or a pull, which can land edges and closures that no
    /// local write path ever saw.
    async fn recompute_blocked(&self) -> Result<u64>;

    // --- config and workspace metadata ---

    async fn get_config(&self, key: &str) -> Result<Option<String>>;
    async fn set_config(&self, key: &str, value: &str) -> Result<()>;
    async fn list_config(&self) -> Result<Vec<(String, String)>>;

    // --- aggregate ---

    async fn stats(&self) -> Result<Stats>;

    // --- lifecycle ---

    async fn close(&self) -> Result<()>;

    // -----------------------------------------------------------------------
    // Capability accessors
    //
    // Default to "absent". A backend opts in by overriding. Callers must handle
    // `None` — see rule 4: a capability may make a core command *faster* or
    // *richer*, but a core command must never silently do less without one.
    // -----------------------------------------------------------------------

    fn version_control(&self) -> Option<&dyn VersionControl> {
        None
    }
    fn remote(&self) -> Option<&dyn RemoteStore> {
        None
    }
    fn history(&self) -> Option<&dyn HistoryViewer> {
        None
    }

    /// True when this backend keeps a commit graph. The CLI uses this to decide
    /// whether commit/sync maintenance is even meaningful, rather than
    /// type-testing for a concrete backend.
    fn has_commit_graph(&self) -> bool {
        self.version_control().is_some()
    }
}

// Note on rule 1 ("construction is on the seam"): there is deliberately no
// `open()` here.
//
// A dispatcher in this crate would have to depend on every backend crate, and
// each backend already depends on this one — the cycle is not incidental, it is
// structural. So the dispatch lives one layer *above* the seam instead, in
// exactly one function in bd-cli. That still satisfies rule 1, which asks that
// the program have a single place that names concrete backends, not that the
// place be here.
//
// A stub `open()` that matched on `Backend` and returned "not my department"
// for every arm would look like it honored the rule while being a function that
// cannot succeed. Rule 1 is about the CLI holding a `Box<dyn Storage>` and never
// learning what it got, and it does.
