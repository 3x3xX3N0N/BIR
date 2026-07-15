//! [`Storage`] over the MySQL wire protocol.
//!
//! This is `bd-sqlite`'s store, `blocked`, `sqlfilter` and `rows` modules folded
//! into one file and translated into MySQL. The *logic* is deliberately a
//! faithful copy — same transactions, same event trail, same fixpoint — because
//! the two backends must answer identically or `bd ready` means two things.
//! What changes is the dialect, and every difference below was a silent bug
//! waiting to happen rather than a matter of style:
//!
//! | SQLite | MySQL / Dolt | what the naive port would have done |
//! |---|---|---|
//! | `INSERT OR IGNORE` | `INSERT … ON DUPLICATE KEY UPDATE` | `INSERT IGNORE` also downgrades *foreign-key* violations to warnings |
//! | `INSERT OR REPLACE` | `INSERT … ON DUPLICATE KEY UPDATE` | `REPLACE` is DELETE+INSERT, so a Dolt diff shows an edge as removed-and-added |
//! | `ON CONFLICT (k) DO UPDATE SET x = excluded.x` | `… ON DUPLICATE KEY UPDATE x = VALUES(x)` | syntax error |
//! | `RETURNING id` | no such thing | — (ids are minted client-side, as bd-sqlite now also does) |
//! | `json_extract(m, '$.k')` | `JSON_UNQUOTE(JSON_EXTRACT(m, '$.k'))` | MySQL returns the JSON value `"any-children"`, *with quotes*, so `= 'any-children'` is silently false and every `waits-for` gate stays shut |
//! | `json_type(m, '$."k"')` | `JSON_EXTRACT(m, CONCAT('$."', ?, '"'))` | MySQL's `JSON_TYPE` takes one argument, not a doc and a path |
//! | `LIMIT -1 OFFSET n` | `LIMIT 18446744073709551615 OFFSET n` | MySQL rejects a negative limit |
//! | `WHERE key = ?` | ``WHERE `key` = ?`` | `KEY` is a reserved word |
//! | `col = 'Alice'` matches exactly | matches `'alice'` too, by default | see the collation note in `schema.sql` |
//! | `LIKE` is ASCII-case-insensitive | case-*sensitive* under a `_bin` collation | `bd list --text foo` stops finding `Foo` |
//!
//! And one that is not a dialect difference but an engine limitation: MySQL
//! forbids an `UPDATE t …` whose `WHERE` selects from `t` (error 1093), which is
//! exactly the shape of bd-sqlite's mark/unmark statements. The fixpoint here
//! therefore *selects* the ids that must flip and *then* updates them by id, in
//! the same transaction. Same semantics, two round trips.

use async_trait::async_trait;
use bd_core::{
    Comment, Dependency, DependencyType, Event, EventType, Issue, IssueFilter, SortPolicy, Status,
    idgen,
};
use bd_storage::{
    Backend, Claim, Error, Field, HistoryViewer, Identity, IssuePatch, RemoteStore, Result, Stats,
    Storage, VersionControl,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlRow};
use sqlx::{MySql, MySqlConnection, MySqlPool, QueryBuilder, Row};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;

use crate::DoltStore;
use crate::server::DoltServer;

/// Edge types that define an ordering between beads, and therefore the ones a
/// cycle in would be a real contradiction ("A must finish before B, which must
/// finish before A"). `conditional-blocks` and `waits-for` are excluded on
/// purpose: neither can destabilize the `is_blocked` fixpoint, because neither
/// reads its target's `is_blocked` — only its status.
const ORDERING_EDGES: [&str; 2] = ["blocks", "parent-child"];

/// Types that are never claimable work. Infrastructure beads (molecules, gates,
/// events, messages) are bookkeeping, not tasks.
const READY_EXCLUDED_TYPES: [&str; 4] = ["molecule", "gate", "event", "message"];

/// Every issue column, in a fixed order. Shared by every SELECT so that a new
/// column cannot be added to one query and forgotten in another.
const ISSUE_COLUMNS: &str = "\
    id, title, description, design, acceptance_criteria, notes, \
    status, priority, issue_type, \
    assignee, owner, created_by, estimated_minutes, \
    created_at, updated_at, started_at, closed_at, close_reason, closed_by_session, \
    lease_expires_at, heartbeat_at, due_at, defer_until, \
    external_ref, source_system, spec_id, metadata, \
    ephemeral, no_history, pinned, is_template, \
    wisp_type, mol_type, work_type, content_hash";

const DEPENDENCY_COLUMNS: &str =
    "issue_id, depends_on_id, `type`, created_at, created_by, metadata, thread_id";

/// `COUNT(*)`, cast to a type sqlx will actually hand back.
///
/// MySQL types `COUNT(*)` as `BIGINT UNSIGNED`, and sqlx's `Type<MySql> for i64`
/// explicitly refuses an UNSIGNED column — so `row.get::<i64, _>(0)` on a bare
/// `COUNT(*)` fails at runtime with a type mismatch, taking `bd list`, `bd status`
/// and `next_id` with it. Reading it as `u64` instead would work on MySQL and
/// break on Dolt, whose engine types the same aggregate as *signed*. The cast is
/// the only spelling both servers agree on.
const COUNT: &str = "CAST(COUNT(*) AS SIGNED)";

/// MySQL's protocol caps a prepared statement at 65535 parameters, but Dolt is
/// not MySQL and there is no reason to find its ceiling experimentally. This
/// matches bd-sqlite's chunk size, so both backends page identically.
const CHUNK: usize = 400;

/// A `parent-child` cycle would make the fixpoint oscillate forever. Cycles are
/// rejected at `add_dependency`, so hitting this means the graph was corrupted
/// behind our back — an import, or a **merge**, which on this backend is a thing
/// that actually happens — and spinning is the wrong answer.
const MAX_ITERATIONS: usize = 100;

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl DoltStore {
    /// Wrap an open pool. `server` is the `dolt sql-server` this process
    /// started, if it started one; dropping the store must take it down with it.
    pub fn new(
        pool: MySqlPool,
        identity: Identity,
        server: Option<DoltServer>,
        dir: impl Into<PathBuf>,
    ) -> Self {
        DoltStore {
            pool,
            identity,
            server,
            dir: dir.into(),
            event_clock: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// The next event timestamp: at least `at`, always strictly greater than the
    /// last one this store handed out. See [`crate::DoltStore::event_clock`].
    fn next_event_time(&self, at: DateTime<Utc>) -> DateTime<Utc> {
        use std::sync::atomic::Ordering::Relaxed;
        let want = at.timestamp_nanos_opt().unwrap_or(0);
        let mut cur = self.event_clock.load(Relaxed);
        let stamp = loop {
            let next = want.max(cur + 1);
            match self
                .event_clock
                .compare_exchange_weak(cur, next, Relaxed, Relaxed)
            {
                Ok(_) => break next,
                Err(actual) => cur = actual,
            }
        };
        DateTime::from_timestamp_nanos(stamp)
    }

    /// Connect to an already-running server and make sure the schema is there.
    ///
    /// Used by tests and by anything pointing beads at a Dolt server it does not
    /// own (a shared `dolt sql-server`, DoltLab, Hosted Dolt). The store has no
    /// `server` handle in that case and must not try to stop it on drop.
    pub async fn connect(url: &str, identity: Identity, dir: impl Into<PathBuf>) -> Result<Self> {
        let pool = connect_pool(url).await?;
        apply_schema(&pool).await?;
        Ok(DoltStore::new(pool, identity, None, dir))
    }
}

/// Open a pool with the session settings this store's SQL assumes.
///
/// # Why the pool holds exactly one connection
///
/// **In `dolt sql-server`, the checked-out branch is *session* state.** It is not
/// a property of the database, it is a property of the connection. `vc.rs` runs
/// `CALL DOLT_CHECKOUT()`, which moves the one session it executes in — so on a
/// pool of eight, it moves one connection and leaves seven sitting on the old
/// branch. The next `bd create` then lands on whichever connection the pool
/// happened to hand back, which is to say: on the wrong branch, roughly
/// seven-eighths of the time, with no error anywhere.
///
/// That is the worst class of bug this backend can have — a silent write to the
/// wrong branch — and it is not fixable from `vc.rs`, because the pool is built
/// here. A pool of one has no sessions to disagree with each other.
///
/// The cost is that store calls serialize. For a CLI that runs one command and
/// exits, that is not a cost. Nothing in this file or `vc.rs` acquires a second
/// connection while holding one — `vc.rs` explicitly drops its connection before
/// calling back through `Storage` for exactly this reason — so a pool of one
/// queues; it does not deadlock.
///
/// `vc.rs` additionally moves the database's *default* branch on checkout, which
/// covers the remaining case: if this single connection is ever recycled (idle
/// timeout, a dropped socket), the replacement opens on the branch we checked
/// out rather than on `main`.
///
/// # Session settings
///
/// `time_zone` is pinned to UTC because sqlx encodes a `DateTime<Utc>` by
/// dropping the offset and sending the naive UTC value; a session in any other
/// zone would read those bytes back shifted. sqlx already defaults to `+00:00` —
/// this states it rather than inheriting it, because a silent one-hour skew in
/// `lease_expires_at` is not the kind of thing to leave to a default.
///
/// `pipes_as_concat` and engine substitution are switched *off* to keep the
/// connection-time `SET` statements Dolt has to swallow down to a minimum:
/// nothing here uses `||` for concatenation (it is `CONCAT()`), and Dolt has no
/// storage engines to substitute.
pub async fn connect_pool(url: &str) -> Result<MySqlPool> {
    let opts = MySqlConnectOptions::from_str(url)
        .map_err(db)?
        .charset("utf8mb4")
        .timezone(Some("+00:00".to_string()))
        .pipes_as_concat(false)
        .no_engine_substitution(false);

    MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(db)
}

/// Create the tables if they are not there.
///
/// Two deliberate choices, both about failure modes rather than speed:
///
/// **`raw_sql`, not `query`.** `sqlx::query` goes down the *prepared-statement*
/// path, and preparing DDL is a corner of the MySQL protocol that servers
/// implement unevenly — Dolt is a Go reimplementation, not MySQL. There are no
/// parameters here to prepare, so the text protocol is both safer and the honest
/// thing to use.
///
/// **Statement by statement, not one blob.** The protocol can carry a
/// multi-statement query, but a failure inside one reports a character offset
/// rather than a statement, and a schema that half-applied is exactly the kind of
/// thing that gets misdiagnosed as "Dolt is broken".
pub async fn apply_schema(pool: &MySqlPool) -> Result<()> {
    for stmt in schema_statements(crate::SCHEMA) {
        sqlx::raw_sql(&stmt).execute(pool).await.map_err(|e| {
            let head: String = stmt.chars().take(60).collect();
            Error::Db(format!("schema statement failed ({head}…): {e}"))
        })?;
    }
    Ok(())
}

/// Split a DDL script into statements.
///
/// Aware of `'…'` literals and `` `…` `` quoted identifiers so a semicolon inside
/// one cannot split a statement, and it strips `--` line comments so a `;` in
/// prose cannot either. There is exactly one place a schema can go wrong
/// silently and this is it.
pub fn schema_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = sql.chars().peekable();
    let (mut in_str, mut in_ident) = (false, false);

    while let Some(c) = chars.next() {
        if !in_str && !in_ident && c == '-' && chars.peek() == Some(&'-') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            cur.push('\n');
            continue;
        }
        match c {
            '\'' if !in_ident => in_str = !in_str,
            '`' if !in_str => in_ident = !in_ident,
            ';' if !in_str && !in_ident => {
                push_statement(&mut out, &mut cur);
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    push_statement(&mut out, &mut cur);
    out
}

fn push_statement(out: &mut Vec<String>, cur: &mut String) {
    let stmt = cur.trim().to_string();
    cur.clear();
    if !stmt.is_empty() {
        out.push(stmt);
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[async_trait]
impl Storage for DoltStore {
    fn backend(&self) -> Backend {
        Backend::Dolt
    }

    fn identity(&self) -> &Identity {
        &self.identity
    }

    // -- issues --------------------------------------------------------------

    /// Persists the issue row and its labels.
    ///
    /// `issue.dependencies` and `issue.comments` are *not* persisted here, even
    /// when the caller populated them. Edges have to be cycle-checked against
    /// the live graph and comments have to be authored by somebody; both are
    /// operations in their own right.
    async fn create_issue(&self, issue: &Issue) -> Result<Issue> {
        issue.validate()?;

        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        let mut row = issue.clone();
        if row.created_by.is_empty() {
            row.created_by = self.identity.actor.clone();
        }
        if row.content_hash.is_empty() {
            row.content_hash = row.compute_content_hash();
        }

        insert_issue(&mut tx, &row).await?;

        for label in &row.labels {
            insert_label(&mut tx, &row.id, label).await?;
        }

        self.event(&mut tx, &row.id, EventType::Created, None, Some(&row.title), now)
            .await?;

        // A brand-new bead has no edges yet, so this can only ever flip the bead
        // itself -- but running it keeps "every write path recomputes" true
        // without exception, and exceptions are how this cache goes stale.
        recompute_affected(&mut tx, &[row.id.clone()]).await?;

        tx.commit().await.map_err(db)?;
        self.get_issue(&row.id)
            .await?
            .ok_or_else(|| Error::NotFound(row.id.clone()))
    }

    async fn get_issue(&self, id: &str) -> Result<Option<Issue>> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        let Some(mut issue) = fetch_issue(&mut conn, id).await? else {
            return Ok(None);
        };
        issue.labels = fetch_labels(&mut conn, id).await?;
        issue.dependencies = fetch_dependencies_of(&mut conn, id).await?;
        issue.comments = fetch_comments(&mut conn, id).await?;
        Ok(Some(issue))
    }

    /// Relations are left empty, as in `list_issues`: this is the batched form
    /// of a listing, not of `get_issue`.
    async fn get_issues(&self, ids: &[String]) -> Result<Vec<Issue>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut by_id: HashMap<String, Issue> = HashMap::new();
        for chunk in ids.chunks(CHUNK) {
            let mut qb: QueryBuilder<MySql> = QueryBuilder::new(format!(
                "SELECT {ISSUE_COLUMNS} FROM issues WHERE id IN ("
            ));
            let mut sep = qb.separated(", ");
            for id in chunk {
                sep.push_bind(id.clone());
            }
            qb.push(")");

            for row in qb.build().fetch_all(&self.pool).await.map_err(db)? {
                let issue = issue_from_row(&row)?;
                by_id.insert(issue.id.clone(), issue);
            }
        }

        // The caller's order, so a listing can zip this against the ids it asked
        // about. A missing id is a gap, not an error — see the seam's doc.
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        let old = fetch_issue(&mut tx, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;

        if !patch.is_empty() {
            let mut qb: QueryBuilder<MySql> = QueryBuilder::new("UPDATE issues SET updated_at = ");
            qb.push_bind(now);

            if let Some(v) = &patch.title {
                qb.push(", title = ").push_bind(v.clone());
            }
            push_text(&mut qb, "description", &patch.description);
            push_text(&mut qb, "design", &patch.design);
            push_text(&mut qb, "acceptance_criteria", &patch.acceptance_criteria);
            push_text(&mut qb, "notes", &patch.notes);
            if let Some(v) = &patch.status {
                qb.push(", status = ").push_bind(v.as_str().to_string());
                if v.is_closed() && !old.status.is_closed() {
                    qb.push(", closed_at = ").push_bind(now);
                    qb.push(", closed_by_session = ")
                        .push_bind(self.identity.session.clone().unwrap_or_default());
                } else if !v.is_closed() && old.status.is_closed() {
                    qb.push(", closed_at = NULL, close_reason = '', close_is_failure = 0");
                }
            }
            if let Some(v) = &patch.priority {
                qb.push(", priority = ").push_bind(v.0);
            }
            if let Some(v) = &patch.issue_type {
                qb.push(", issue_type = ").push_bind(v.as_str().to_string());
            }
            // Clearing the assignee is what `bd unclaim` does, and it must drop
            // the lease with it -- an unassigned issue still holding a lease is
            // invisible to `bd ready` and claimable by nobody.
            match &patch.assignee {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", assignee = ").push_bind(v.clone());
                }
                Field::Clear => {
                    qb.push(", assignee = '', lease_expires_at = NULL, heartbeat_at = NULL");
                }
            }
            push_text(&mut qb, "spec_id", &patch.spec_id);

            match patch.estimated_minutes {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", estimated_minutes = ").push_bind(v);
                }
                Field::Clear => {
                    qb.push(", estimated_minutes = NULL");
                }
            }
            match patch.due_at {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", due_at = ").push_bind(v);
                }
                Field::Clear => {
                    qb.push(", due_at = NULL");
                }
            }
            // `bd undefer`: a NULL defer_until is what puts the issue back in
            // `bd ready`, so this clear is load-bearing, not cosmetic.
            match patch.defer_until {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", defer_until = ").push_bind(v);
                }
                Field::Clear => {
                    qb.push(", defer_until = NULL");
                }
            }
            // The derived column and its source move together, always: a
            // close_reason whose close_is_failure disagrees with it silently
            // changes which conditional-blocks edges release.
            match &patch.close_reason {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", close_reason = ").push_bind(v.clone());
                    qb.push(", close_is_failure = ")
                        .push_bind(bd_core::types::is_failure_close(v));
                }
                Field::Clear => {
                    qb.push(", close_reason = '', close_is_failure = 0");
                }
            }
            match &patch.metadata {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", metadata = ").push_bind(v.to_string());
                }
                Field::Clear => {
                    qb.push(", metadata = NULL");
                }
            }
            match &patch.external_ref {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", external_ref = ").push_bind(v.clone());
                }
                Field::Clear => {
                    qb.push(", external_ref = NULL");
                }
            }
            // The other half of the tracker join key. Clearing it writes `''`,
            // not NULL: the column is NOT NULL, and `Issue::source_system` is a
            // `String` whose empty value already means "no remote".
            match &patch.source_system {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", source_system = ").push_bind(v.clone());
                }
                Field::Clear => {
                    qb.push(", source_system = ''");
                }
            }
            if let Some(v) = patch.pinned {
                qb.push(", pinned = ").push_bind(v);
            }
            // `bd promote` moves these two together. They are separate fields
            // because only one of them is a flag: an ephemeral bead with no
            // wisp type has no TTL at all, so `bd gc` leaves it alone forever.
            if let Some(v) = patch.ephemeral {
                qb.push(", ephemeral = ").push_bind(v);
            }
            match &patch.wisp_type {
                Field::Keep => {}
                Field::Set(v) => {
                    qb.push(", wisp_type = ").push_bind(enum_to_str(v));
                }
                Field::Clear => {
                    qb.push(", wisp_type = NULL");
                }
            }

            qb.push(" WHERE id = ").push_bind(id.to_string());
            qb.build().execute(&mut *tx).await.map_err(db)?;

            // Almost every column above is hashed content. Recomputing here
            // rather than field-by-field is not laziness: the hash exists for
            // cross-clone identity and import dedup, so a hash that describes
            // the issue's *previous* text does not merely go stale — it makes
            // two different beads look like the same one.
            refresh_content_hash(&mut tx, id).await?;
        }

        if let Some(new_status) = &patch.status
            && *new_status != old.status
        {
            self.status_events(&mut tx, id, &old.status, new_status, now)
                .await?;
        }

        // Status, pinned-ness and close_reason all feed the fixpoint, and a
        // caller can move any of them here -- so this path recomputes
        // unconditionally rather than trying to be clever about which fields
        // matter. Guessing wrong leaves `bd ready` lying.
        recompute_affected(&mut tx, &[id.to_string()]).await?;

        tx.commit().await.map_err(db)?;
        self.get_issue(id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn delete_issue(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        let existing = fetch_issue(&mut tx, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;

        // Seed *before* the delete: the ON DELETE CASCADE is about to remove the
        // very edges that tell us who cared about this bead. (And the cascade is
        // only there because the foreign keys in `schema.sql` are declared at
        // table level — MySQL ignores the inline column-level spelling.)
        let affected = affected_set(&mut tx, &[id.to_string()]).await?;

        self.event(
            &mut tx,
            id,
            EventType::Deleted,
            Some(&existing.title),
            None,
            now,
        )
        .await?;

        sqlx::query("DELETE FROM issues WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;

        fixpoint(&mut tx, &affected).await?;

        tx.commit().await.map_err(db)
    }

    /// Relations are left empty. Hydrating labels, edges and comments per row
    /// would be one query per issue; `get_issue` is where you go for the whole
    /// bead.
    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>> {
        let mut qb: QueryBuilder<MySql> =
            QueryBuilder::new(format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE 1 = 1"));
        push_filter(&mut qb, filter);
        push_order_and_limit(&mut qb, filter);

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(issue_from_row).collect()
    }

    async fn count_issues(&self, filter: &IssueFilter) -> Result<u64> {
        let mut qb: QueryBuilder<MySql> =
            QueryBuilder::new(format!("SELECT {COUNT} FROM issues WHERE 1 = 1"));
        push_filter(&mut qb, filter);

        let row = qb.build().fetch_one(&self.pool).await.map_err(db)?;
        Ok(row.get::<i64, _>(0) as u64)
    }

    async fn close_issue(&self, id: &str, reason: &str) -> Result<Issue> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        let old = fetch_issue(&mut tx, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;

        sqlx::query(
            "UPDATE issues
             SET status = 'closed', closed_at = ?, close_reason = ?, close_is_failure = ?,
                 closed_by_session = ?, lease_expires_at = NULL, updated_at = ?
             WHERE id = ?",
        )
        .bind(now)
        .bind(reason)
        // bd-core owns the definition of a failing close; this column is only a
        // cache of its answer so the SQL recompute can read it.
        .bind(bd_core::types::is_failure_close(reason))
        .bind(self.identity.session.clone().unwrap_or_default())
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        // Not `status_events`: that would emit its own bare `Closed`, and this
        // path wants the one carrying the *reason* — the reason is what a
        // `conditional-blocks` edge reads to decide whether the failure branch
        // becomes ready.
        if !old.status.is_closed() {
            self.event(
                &mut tx,
                id,
                EventType::StatusChanged,
                Some(old.status.as_str()),
                Some(Status::Closed.as_str()),
                now,
            )
            .await?;
        }
        self.event(&mut tx, id, EventType::Closed, None, Some(reason), now)
            .await?;

        // `status` and `close_reason` are both hashed.
        refresh_content_hash(&mut tx, id).await?;
        recompute_affected(&mut tx, &[id.to_string()]).await?;
        tx.commit().await.map_err(db)?;

        self.get_issue(id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn reopen_issue(&self, id: &str) -> Result<Issue> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        let old = fetch_issue(&mut tx, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;

        sqlx::query(
            "UPDATE issues
             SET status = 'open', closed_at = NULL, close_reason = '', close_is_failure = 0,
                 closed_by_session = '', updated_at = ?
             WHERE id = ?",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        if old.status != Status::Open {
            self.status_events(&mut tx, id, &old.status, &Status::Open, now)
                .await?;
        }

        refresh_content_hash(&mut tx, id).await?;
        recompute_affected(&mut tx, &[id.to_string()]).await?;
        tx.commit().await.map_err(db)?;

        self.get_issue(id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn next_id(&self, prefix: &str, title: &str, description: &str) -> Result<String> {
        let mut conn = self.pool.acquire().await.map_err(db)?;

        let count: i64 = sqlx::query(&format!("SELECT {COUNT} FROM issues"))
            .fetch_one(&mut *conn)
            .await
            .map_err(db)?
            .get(0);
        let now_nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0);

        for (length, nonce) in idgen::candidate_sequence(count as u64) {
            let candidate = idgen::generate_hash_id(
                prefix,
                title,
                description,
                &self.identity.actor,
                now_nanos,
                length,
                nonce,
            );
            let taken: Option<String> = sqlx::query_scalar("SELECT id FROM issues WHERE id = ?")
                .bind(&candidate)
                .fetch_optional(&mut *conn)
                .await
                .map_err(db)?;
            if taken.is_none() {
                return Ok(candidate);
            }
        }
        Err(Error::IdExhausted)
    }

    // -- claims --------------------------------------------------------------

    /// One conditional UPDATE, not a read followed by a write.
    ///
    /// Two agents racing for the last ready bead is the *expected* case, not an
    /// edge case. A read-then-write would let both observe "unclaimed" before
    /// either writes, and both would believe they own it.
    ///
    /// Open-competition beads are exempt: several agents may hold one at once.
    ///
    /// `rows_affected` is the fence, and it means "matched", not "changed" —
    /// sqlx negotiates `CLIENT_FOUND_ROWS` on every MySQL connection. That is
    /// the SQLite semantics this code was written against, so it carries over;
    /// but it is a negotiated flag, not a law, and a server that ignored it
    /// would still be safe here because every SET in this statement writes a
    /// fresh timestamp and therefore genuinely changes the row.
    async fn claim_issue(&self, id: &str, lease: Duration) -> Result<Claim> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();
        let expires = now + lease;

        let affected = sqlx::query(
            "UPDATE issues
             SET assignee = ?, lease_expires_at = ?, status = 'in_progress',
                 started_at = COALESCE(started_at, ?), updated_at = ?
             WHERE id = ?
               AND status <> 'closed'
               AND (
                     work_type = 'open_competition'
                     OR assignee = ''
                     OR assignee = ?
                     OR lease_expires_at IS NULL
                     OR lease_expires_at < ?
               )",
        )
        .bind(&self.identity.actor)
        .bind(expires)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(&self.identity.actor)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        if affected == 0 {
            let existing = fetch_issue(&mut tx, id)
                .await?
                .ok_or_else(|| Error::NotFound(id.to_string()))?;
            return Err(if existing.status.is_closed() {
                Error::Db(format!("issue {id} is closed; reopen it before claiming"))
            } else {
                Error::AlreadyClaimed {
                    id: id.to_string(),
                    holder: existing.assignee,
                }
            });
        }

        self.event(
            &mut tx,
            id,
            EventType::AssigneeChanged,
            None,
            Some(&self.identity.actor),
            now,
        )
        .await?;
        // A claim moves `assignee` and `status`, and both are hashed.
        refresh_content_hash(&mut tx, id).await?;
        recompute_affected(&mut tx, &[id.to_string()]).await?;
        tx.commit().await.map_err(db)?;

        Ok(Claim {
            issue_id: id.to_string(),
            holder: self.identity.actor.clone(),
            expires_at: expires,
        })
    }

    /// Renewing is not claiming: it extends a lease this actor already holds and
    /// never takes one it does not.
    ///
    /// The three ways it can fail are three *different* facts, and reporting them
    /// as one is how `bd heartbeat` on an unclaimed bead came to say "already
    /// claimed by ''" — a message describing a race with a nameless agent when
    /// what actually happened is that you never claimed the issue.
    async fn renew_claim(&self, id: &str, lease: Duration) -> Result<Claim> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        let now = Utc::now();
        let expires = now + lease;

        let affected = sqlx::query(
            "UPDATE issues SET lease_expires_at = ?, heartbeat_at = ?
             WHERE id = ? AND assignee = ? AND status <> 'closed'",
        )
        .bind(expires)
        .bind(now)
        .bind(id)
        .bind(&self.identity.actor)
        .execute(&mut *conn)
        .await
        .map_err(db)?
        .rows_affected();

        if affected == 0 {
            let existing = fetch_issue(&mut conn, id)
                .await?
                .ok_or_else(|| Error::NotFound(id.to_string()))?;
            return Err(if existing.assignee.is_empty() {
                Error::NotClaimed(id.to_string())
            } else if existing.assignee != self.identity.actor {
                Error::AlreadyClaimed {
                    id: id.to_string(),
                    holder: existing.assignee,
                }
            } else {
                // Ours, and the UPDATE still matched nothing: the only remaining
                // predicate is the status, so the issue is closed.
                Error::Db(format!(
                    "issue {id} is closed; reopen it before renewing the claim"
                ))
            });
        }

        Ok(Claim {
            issue_id: id.to_string(),
            holder: self.identity.actor.clone(),
            expires_at: expires,
        })
    }

    async fn release_claim(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        let existing = fetch_issue(&mut tx, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;

        if existing.assignee.is_empty() {
            return Ok(());
        }
        let held_by_other = existing.assignee != self.identity.actor
            && existing.lease_expires_at.is_some_and(|e| e > now);
        if held_by_other {
            return Err(Error::AlreadyClaimed {
                id: id.to_string(),
                holder: existing.assignee,
            });
        }

        sqlx::query(
            "UPDATE issues
             SET assignee = '', lease_expires_at = NULL, updated_at = ?,
                 status = CASE WHEN status = 'in_progress' THEN 'open' ELSE status END
             WHERE id = ?",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        if existing.status == Status::InProgress {
            self.status_events(&mut tx, id, &Status::InProgress, &Status::Open, now)
                .await?;
        }
        refresh_content_hash(&mut tx, id).await?;
        recompute_affected(&mut tx, &[id.to_string()]).await?;
        tx.commit().await.map_err(db)
    }

    /// An agent that died mid-task must not hold its work hostage.
    async fn expire_claims(&self) -> Result<Vec<String>> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        let freed: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM issues
             WHERE status = 'in_progress'
               AND lease_expires_at IS NOT NULL
               AND lease_expires_at < ?",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await
        .map_err(db)?;

        if freed.is_empty() {
            return Ok(freed);
        }

        for id in &freed {
            sqlx::query(
                "UPDATE issues
                 SET status = 'open', assignee = '', lease_expires_at = NULL, updated_at = ?
                 WHERE id = ?",
            )
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;

            self.status_events(&mut tx, id, &Status::InProgress, &Status::Open, now)
                .await?;
            refresh_content_hash(&mut tx, id).await?;
        }

        recompute_affected(&mut tx, &freed).await?;
        tx.commit().await.map_err(db)?;
        Ok(freed)
    }

    // -- dependencies --------------------------------------------------------

    async fn add_dependency(&self, dep: &Dependency) -> Result<()> {
        dep.dep_type.validate()?;
        if dep.issue_id == dep.depends_on_id {
            return Err(Error::Domain(bd_core::Error::SelfDependency(
                dep.issue_id.clone(),
            )));
        }

        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        for id in [&dep.issue_id, &dep.depends_on_id] {
            if fetch_issue(&mut tx, id).await?.is_none() {
                return Err(Error::NotFound(id.clone()));
            }
        }

        if ORDERING_EDGES.contains(&dep.dep_type.as_str())
            && let Some(path) = path_between(&mut tx, &dep.depends_on_id, &dep.issue_id).await?
        {
            let mut cycle = vec![dep.issue_id.clone()];
            cycle.extend(path);
            return Err(Error::Cycle(cycle));
        }

        // Upsert, not `REPLACE INTO`. `REPLACE` is a DELETE followed by an
        // INSERT, so re-adding an edge that already exists would show up in
        // `dolt diff` as the edge being removed and a different one added — and
        // on a merge, a delete and an insert of the same key is a conflict where
        // an update would have been a no-op.
        sqlx::query(
            "INSERT INTO dependencies
                 (issue_id, depends_on_id, `type`, created_at, created_by, metadata, thread_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
                 created_at = VALUES(created_at),
                 created_by = VALUES(created_by),
                 metadata   = VALUES(metadata),
                 thread_id  = VALUES(thread_id)",
        )
        .bind(&dep.issue_id)
        .bind(&dep.depends_on_id)
        .bind(dep.dep_type.as_str())
        .bind(dep.created_at)
        .bind(if dep.created_by.is_empty() {
            self.identity.actor.clone()
        } else {
            dep.created_by.clone()
        })
        .bind(none_if_empty(&dep.metadata))
        .bind(none_if_empty(&dep.thread_id))
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        self.event(
            &mut tx,
            &dep.issue_id,
            EventType::DependencyAdded,
            None,
            Some(&format!("{} ({})", dep.depends_on_id, dep.dep_type)),
            now,
        )
        .await?;

        recompute_affected(&mut tx, std::slice::from_ref(&dep.issue_id)).await?;
        tx.commit().await.map_err(db)
    }

    /// Removes exactly the one edge named by the triple.
    ///
    /// The type is part of the primary key precisely because a pair of beads can
    /// hold several edges at once, so a DELETE without it removes all of them —
    /// `bd dep remove A B` silently destroying an unrelated `related` edge while
    /// reporting success.
    async fn remove_dependency(
        &self,
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &DependencyType,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        // Seeded before the delete, and seeded from *both* ends: dropping a
        // parent-child edge moves the gate of anyone waiting on the parent, and
        // after the DELETE there is nothing left to tell us who that was.
        let affected =
            affected_set(&mut tx, &[issue_id.to_string(), depends_on_id.to_string()]).await?;

        let removed = sqlx::query(
            "DELETE FROM dependencies
             WHERE issue_id = ? AND depends_on_id = ? AND `type` = ?",
        )
        .bind(issue_id)
        .bind(depends_on_id)
        .bind(dep_type.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();

        if removed == 0 {
            return Err(Error::NotFound(format!(
                "{issue_id} -> {depends_on_id} [{dep_type}]"
            )));
        }

        self.event(
            &mut tx,
            issue_id,
            EventType::DependencyRemoved,
            Some(&format!("{depends_on_id} ({dep_type})")),
            None,
            now,
        )
        .await?;

        fixpoint(&mut tx, &affected).await?;
        tx.commit().await.map_err(db)
    }

    async fn dependencies_of(&self, id: &str) -> Result<Vec<Dependency>> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        fetch_dependencies_of(&mut conn, id).await
    }

    async fn dependents_of(&self, id: &str) -> Result<Vec<Dependency>> {
        let rows = sqlx::query(&format!(
            "SELECT {DEPENDENCY_COLUMNS} FROM dependencies
             WHERE depends_on_id = ? ORDER BY issue_id, `type`"
        ))
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(dependency_from_row).collect()
    }

    async fn list_dependencies(&self, filter: &IssueFilter) -> Result<Vec<Dependency>> {
        let mut qb: QueryBuilder<MySql> = QueryBuilder::new(format!(
            "SELECT {DEPENDENCY_COLUMNS} FROM dependencies"
        ));

        // An empty filter means every edge *in the table* — deliberately not
        // "every edge whose source is an issue that exists". The two differ only
        // on a corrupt graph, and a corrupt graph is the entire reason
        // `bd lint` and `bd graph check` ask this question. Foreign keys make
        // that state harder to reach here than on SQLite, but not impossible:
        // a Dolt merge can land constraint violations.
        if !filter.is_empty() {
            qb.push(" WHERE issue_id IN (SELECT id FROM issues WHERE 1 = 1");
            push_filter(&mut qb, filter);
            qb.push(")");
        }
        qb.push(" ORDER BY issue_id, depends_on_id, `type`");

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(dependency_from_row).collect()
    }

    async fn dependencies_of_many(&self, ids: &[String]) -> Result<Vec<(String, Vec<Dependency>)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut grouped: HashMap<String, Vec<Dependency>> = HashMap::new();
        for chunk in ids.chunks(CHUNK) {
            let mut qb: QueryBuilder<MySql> = QueryBuilder::new(format!(
                "SELECT {DEPENDENCY_COLUMNS} FROM dependencies WHERE issue_id IN ("
            ));
            let mut sep = qb.separated(", ");
            for id in chunk {
                sep.push_bind(id.clone());
            }
            qb.push(") ORDER BY issue_id, depends_on_id, `type`");

            for row in qb.build().fetch_all(&self.pool).await.map_err(db)? {
                let dep = dependency_from_row(&row)?;
                grouped.entry(dep.issue_id.clone()).or_default().push(dep);
            }
        }

        Ok(ids
            .iter()
            .filter_map(|id| grouped.remove(id).map(|deps| (id.clone(), deps)))
            .collect())
    }

    async fn find_cycles(&self) -> Result<Vec<Vec<String>>> {
        let edges = load_ordering_edges(&self.pool).await?;
        Ok(detect_cycles(&edges))
    }

    // -- labels --------------------------------------------------------------

    async fn add_label(&self, issue_id: &str, label: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        if fetch_issue(&mut tx, issue_id).await?.is_none() {
            return Err(Error::NotFound(issue_id.to_string()));
        }
        insert_label(&mut tx, issue_id, label).await?;
        self.event(
            &mut tx,
            issue_id,
            EventType::LabelAdded,
            None,
            Some(label),
            Utc::now(),
        )
        .await?;
        // Labels are inside the content hash — `Issue::compute_content_hash`
        // sorts and folds them in — so a label write moves the hash.
        refresh_content_hash(&mut tx, issue_id).await?;
        tx.commit().await.map_err(db)
    }

    async fn remove_label(&self, issue_id: &str, label: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let removed = sqlx::query("DELETE FROM labels WHERE issue_id = ? AND label = ?")
            .bind(issue_id)
            .bind(label)
            .execute(&mut *tx)
            .await
            .map_err(db)?
            .rows_affected();
        if removed == 0 {
            return Err(Error::NotFound(format!("{issue_id}: label {label}")));
        }
        self.event(
            &mut tx,
            issue_id,
            EventType::LabelRemoved,
            Some(label),
            None,
            Utc::now(),
        )
        .await?;
        refresh_content_hash(&mut tx, issue_id).await?;
        tx.commit().await.map_err(db)
    }

    async fn list_labels(&self) -> Result<Vec<String>> {
        sqlx::query_scalar("SELECT DISTINCT label FROM labels ORDER BY label")
            .fetch_all(&self.pool)
            .await
            .map_err(db)
    }

    /// Labels for many issues in one round trip.
    ///
    /// An empty `ids` returns empty rather than building `IN ()`, which is not
    /// legal SQL. Issues with no labels are simply absent from the result, so a
    /// caller should treat "missing" as "none" rather than expecting a row back
    /// for every id it asked about.
    async fn labels_of(&self, ids: &[String]) -> Result<Vec<(String, Vec<String>)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
        for chunk in ids.chunks(CHUNK) {
            let mut qb: QueryBuilder<MySql> =
                QueryBuilder::new("SELECT issue_id, label FROM labels WHERE issue_id IN (");
            let mut sep = qb.separated(", ");
            for id in chunk {
                sep.push_bind(id.clone());
            }
            qb.push(") ORDER BY issue_id, label");

            for row in qb.build().fetch_all(&self.pool).await.map_err(db)? {
                let issue_id: String = row.try_get("issue_id").map_err(db)?;
                let label: String = row.try_get("label").map_err(db)?;
                grouped.entry(issue_id).or_default().push(label);
            }
        }

        // Return in the caller's order so a listing can zip this against its
        // rows without re-sorting.
        Ok(ids
            .iter()
            .filter_map(|id| grouped.remove(id).map(|labels| (id.clone(), labels)))
            .collect())
    }

    // -- comments and audit trail --------------------------------------------

    /// Insert or update a comment, preserving its id and author.
    ///
    /// This is the idempotent half of the pair. `add_comment` mints a new id and
    /// stamps the caller as author; re-running an import through it would
    /// duplicate every comment and reattribute all of them to the importer.
    ///
    /// The incoming id is the identity, and it is honored *whatever it says* —
    /// including an integer id from an older export, which is why nothing here
    /// parses or validates its shape.
    async fn upsert_comment(&self, comment: &Comment) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        if fetch_issue(&mut tx, &comment.issue_id).await?.is_none() {
            return Err(Error::NotFound(comment.issue_id.clone()));
        }

        sqlx::query(
            "INSERT INTO comments (id, issue_id, author, `text`, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
                 issue_id   = VALUES(issue_id),
                 author     = VALUES(author),
                 `text`     = VALUES(`text`),
                 created_at = VALUES(created_at)",
        )
        .bind(&comment.id)
        .bind(&comment.issue_id)
        .bind(&comment.author)
        .bind(&comment.text)
        .bind(comment.created_at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn add_comment(&self, issue_id: &str, text: &str) -> Result<Comment> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let now = Utc::now();

        if fetch_issue(&mut tx, issue_id).await?.is_none() {
            return Err(Error::NotFound(issue_id.to_string()));
        }

        // Minted here, not by the database. Two reasons, and the second one is
        // the one that bites: MySQL has no `RETURNING`, so an AUTO_INCREMENT id
        // would have to be read back with `LAST_INSERT_ID()`; and a
        // workspace-local integer id is not an identity at all once workspaces
        // *merge*, which on this backend they do. `upsert_comment` keys on the
        // id, so a comment 1 in two clones is a comment that overwrites another.
        let id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO comments (id, issue_id, author, `text`, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(issue_id)
        .bind(&self.identity.actor)
        .bind(text)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        self.event(&mut tx, issue_id, EventType::Commented, None, Some(text), now)
            .await?;
        tx.commit().await.map_err(db)?;

        Ok(Comment {
            id,
            issue_id: issue_id.to_string(),
            author: self.identity.actor.clone(),
            text: text.to_string(),
            created_at: now,
        })
    }

    async fn list_comments(&self, issue_id: &str) -> Result<Vec<Comment>> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        fetch_comments(&mut conn, issue_id).await
    }

    async fn comments_of_many(&self, ids: &[String]) -> Result<Vec<(String, Vec<Comment>)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut grouped: HashMap<String, Vec<Comment>> = HashMap::new();
        for chunk in ids.chunks(CHUNK) {
            let mut qb: QueryBuilder<MySql> = QueryBuilder::new(
                "SELECT id, issue_id, author, `text`, created_at FROM comments WHERE issue_id IN (",
            );
            let mut sep = qb.separated(", ");
            for id in chunk {
                sep.push_bind(id.clone());
            }
            // By time, never by id: comment ids are UUIDs and sort meaninglessly,
            // so ordering on them would shuffle a conversation.
            qb.push(") ORDER BY issue_id, created_at, id");

            for row in qb.build().fetch_all(&self.pool).await.map_err(db)? {
                let c = comment_from_row(&row)?;
                grouped.entry(c.issue_id.clone()).or_default().push(c);
            }
        }

        Ok(ids
            .iter()
            .filter_map(|id| grouped.remove(id).map(|cs| (id.clone(), cs)))
            .collect())
    }

    async fn list_events(&self, issue_id: &str) -> Result<Vec<Event>> {
        let rows = sqlx::query(
            "SELECT id, issue_id, event_type, actor, old_value, new_value, created_at
             FROM events WHERE issue_id = ? ORDER BY created_at, id",
        )
        .bind(issue_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(event_from_row).collect()
    }

    // -- work queries --------------------------------------------------------

    async fn ready_work(&self, filter: &IssueFilter) -> Result<Vec<Issue>> {
        self.work(filter, false).await
    }

    async fn blocked_work(&self, filter: &IssueFilter) -> Result<Vec<Issue>> {
        self.work(filter, true).await
    }

    /// The full fixpoint over the whole graph.
    ///
    /// `vc.rs` calls this after every merge and every pull, and that call is not
    /// belt-and-braces: rows arriving from a sync were never seen by a local
    /// write path, so the incremental recompute has no seed to work from and the
    /// cache is stale *by definition* the moment the merge lands.
    async fn recompute_blocked(&self) -> Result<u64> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let changed = recompute_all(&mut tx).await?;
        tx.commit().await.map_err(db)?;
        Ok(changed)
    }

    // -- config --------------------------------------------------------------

    async fn get_config(&self, key: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT value FROM config WHERE `key` = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO config (`key`, value) VALUES (?, ?)
             ON DUPLICATE KEY UPDATE value = VALUES(value)",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn list_config(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT `key`, value FROM config ORDER BY `key`")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>("key"), r.get::<String, _>("value")))
            .collect())
    }

    // -- aggregate -----------------------------------------------------------

    async fn stats(&self) -> Result<Stats> {
        let by_status = format!("SELECT {COUNT} FROM issues WHERE status = ?");
        let mut s = Stats {
            total: count(&self.pool, &format!("SELECT {COUNT} FROM issues"), &[]).await?,
            open: count(&self.pool, &by_status, &["open"]).await?,
            in_progress: count(&self.pool, &by_status, &["in_progress"]).await?,
            closed: count(&self.pool, &by_status, &["closed"]).await?,
            ..Default::default()
        };

        s.blocked = self.count_work(true).await?;
        s.ready = self.count_work(false).await?;

        for r in sqlx::query(&format!(
            "SELECT priority, {COUNT} AS n FROM issues GROUP BY priority"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        {
            s.by_priority
                .insert(r.get::<i32, _>("priority"), r.get::<i64, _>("n") as u64);
        }
        for r in sqlx::query(&format!(
            "SELECT issue_type, {COUNT} AS n FROM issues GROUP BY issue_type"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        {
            s.by_type
                .insert(r.get::<String, _>("issue_type"), r.get::<i64, _>("n") as u64);
        }
        Ok(s)
    }

    // -- schema version --------------------------------------------------------

    /// The stamp in `schema_meta`, raw: an empty table reads `0`, meaning the
    /// database predates version stamping (`bd init` seeds the row, so fresh
    /// workspaces never read 0). The table is versioned data, so the stamp
    /// rides along on clone/push/pull — a database migrated on one machine
    /// announces it to every clone at the next sync.
    async fn schema_version(&self) -> Result<u32> {
        let v: Option<u32> = sqlx::query_scalar("SELECT version FROM schema_meta WHERE id = 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        Ok(v.unwrap_or(0))
    }

    /// Like every other write on this backend, the stamp lands in the working
    /// set and is captured by the next `bd dolt commit` — migrate does not
    /// mint a commit of its own, because `DOLT_COMMIT -A` would sweep whatever
    /// else is sitting uncommitted into a commit labeled "migrate".
    async fn migrate(&self) -> Result<bd_storage::MigrateOutcome> {
        let from = Storage::schema_version(self).await?;
        let effective = bd_storage::effective_schema_version(from);
        if effective > bd_storage::SCHEMA_VERSION {
            return Err(Error::Db(format!(
                "this database records schema v{effective}, newer than this build of bd \
                 (v{}); migrating would be a downgrade. Upgrade bd instead.",
                bd_storage::SCHEMA_VERSION
            )));
        }

        // The migration ladder runs here when a v2 schema ever ships. Today
        // the only possible work is stamping a pre-versioning database.
        if from != bd_storage::SCHEMA_VERSION {
            sqlx::query(
                "INSERT INTO schema_meta (id, version) VALUES (1, ?)
                 ON DUPLICATE KEY UPDATE version = ?",
            )
            .bind(bd_storage::SCHEMA_VERSION)
            .bind(bd_storage::SCHEMA_VERSION)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        }

        Ok(bd_storage::MigrateOutcome {
            from,
            to: bd_storage::SCHEMA_VERSION,
        })
    }

    async fn close(&self) -> Result<()> {
        self.pool.close().await;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Capabilities
    //
    // The three lines this crate exists for. `bd branch`, `bd dolt push`,
    // `bd vc` and `bd diff` are already-finished commands that on SQLite exit 2
    // ("this backend has no commit graph"); saying `Some` here is the entire
    // change that turns them on. The traits themselves are implemented for
    // `DoltStore` in `vc.rs`.
    // -----------------------------------------------------------------------

    fn version_control(&self) -> Option<&dyn VersionControl> {
        Some(self)
    }
    fn remote(&self) -> Option<&dyn RemoteStore> {
        Some(self)
    }
    fn history(&self) -> Option<&dyn HistoryViewer> {
        Some(self)
    }
}

// ---------------------------------------------------------------------------
// Helpers on the store
// ---------------------------------------------------------------------------

impl DoltStore {
    async fn work(&self, filter: &IssueFilter, blocked_side: bool) -> Result<Vec<Issue>> {
        // The ready predicates are ANDed with the caller's filter, so the caller
        // can only ever narrow the result — `bd ready --assignee me` works,
        // `bd ready --include-blocked` cannot exist. The one field dropped is
        // `is_blocked`: `IssueFilter::ready()` pins it to `false`, and handing
        // that same filter to `blocked_work` would otherwise AND `is_blocked = 0`
        // with `is_blocked = 1` and silently return nothing.
        let mut f = filter.clone();
        f.is_blocked = None;

        let mut qb: QueryBuilder<MySql> =
            QueryBuilder::new(format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE 1 = 1"));
        push_ready_predicates(&mut qb, blocked_side);
        push_filter(&mut qb, &f);
        push_order_and_limit(&mut qb, filter);

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(issue_from_row).collect()
    }

    async fn count_work(&self, blocked_side: bool) -> Result<u64> {
        let mut qb: QueryBuilder<MySql> =
            QueryBuilder::new(format!("SELECT {COUNT} FROM issues WHERE 1 = 1"));
        push_ready_predicates(&mut qb, blocked_side);
        let row = qb.build().fetch_one(&self.pool).await.map_err(db)?;
        Ok(row.get::<i64, _>(0) as u64)
    }

    /// Events go in the *same transaction* as the mutation that earned them. An
    /// event written after the commit is an event that a crash can lose, leaving
    /// an audit trail that disagrees with the data it audits.
    async fn event(
        &self,
        conn: &mut MySqlConnection,
        issue_id: &str,
        kind: EventType,
        old: Option<&str>,
        new: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let kind = enum_to_str(&kind)
            .ok_or_else(|| Error::Db("event type is not a string".to_string()))?;
        // Client-minted UUID, so a merge between clones is a clean union rather
        // than a primary-key collision between two different events.
        let id = uuid::Uuid::new_v4().to_string();
        // Strictly-increasing timestamp so the audit trail totally orders.
        let at = self.next_event_time(at);
        sqlx::query(
            "INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(issue_id)
        .bind(kind)
        .bind(&self.identity.actor)
        .bind(old)
        .bind(new)
        .bind(at)
        .execute(&mut *conn)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// A status move is always a `status_changed`, and additionally a `closed`
    /// or a `reopened` when it crosses the terminal boundary.
    async fn status_events(
        &self,
        conn: &mut MySqlConnection,
        id: &str,
        old: &Status,
        new: &Status,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.event(
            conn,
            id,
            EventType::StatusChanged,
            Some(old.as_str()),
            Some(new.as_str()),
            at,
        )
        .await?;
        // The store's monotonic event clock orders the terminal event after the
        // StatusChanged on its own; the caller need not stagger `at`.
        if new.is_closed() && !old.is_closed() {
            self.event(conn, id, EventType::Closed, None, Some(new.as_str()), at)
                .await?;
        } else if old.is_closed() && !new.is_closed() {
            self.event(conn, id, EventType::Reopened, Some(old.as_str()), None, at)
                .await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Row I/O
// ---------------------------------------------------------------------------

async fn insert_issue(conn: &mut MySqlConnection, i: &Issue) -> Result<()> {
    let res = sqlx::query(
        "INSERT INTO issues (
            id, title, description, design, acceptance_criteria, notes,
            status, priority, issue_type,
            assignee, owner, created_by, estimated_minutes,
            created_at, updated_at, started_at, closed_at, close_reason, closed_by_session,
            lease_expires_at, heartbeat_at, due_at, defer_until,
            external_ref, source_system, spec_id, metadata,
            ephemeral, no_history, pinned, is_template,
            wisp_type, mol_type, work_type, content_hash, close_is_failure
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                  ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&i.id)
    .bind(&i.title)
    .bind(&i.description)
    .bind(&i.design)
    .bind(&i.acceptance_criteria)
    .bind(&i.notes)
    .bind(i.status.as_str())
    .bind(i.priority.0)
    .bind(i.issue_type.as_str())
    .bind(&i.assignee)
    .bind(&i.owner)
    .bind(&i.created_by)
    .bind(i.estimated_minutes)
    .bind(i.created_at)
    .bind(i.updated_at)
    .bind(i.started_at)
    .bind(i.closed_at)
    .bind(&i.close_reason)
    .bind(&i.closed_by_session)
    .bind(i.lease_expires_at)
    .bind(i.heartbeat_at)
    .bind(i.due_at)
    .bind(i.defer_until)
    .bind(i.external_ref.clone())
    .bind(&i.source_system)
    .bind(&i.spec_id)
    .bind(metadata_to_text(&i.metadata))
    .bind(i.ephemeral)
    .bind(i.no_history)
    .bind(i.pinned)
    .bind(i.is_template)
    .bind(i.wisp_type.as_ref().and_then(enum_to_str))
    .bind(i.mol_type.as_ref().and_then(enum_to_str))
    .bind(i.work_type.as_ref().and_then(enum_to_str))
    .bind(&i.content_hash)
    .bind(bd_core::types::is_failure_close(&i.close_reason))
    .execute(&mut *conn)
    .await;

    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Err(Error::AlreadyExists(i.id.clone()))
        }
        Err(e) => Err(db(e)),
    }
}

/// `INSERT OR IGNORE`, MySQL-style — and deliberately *not* `INSERT IGNORE`.
///
/// `INSERT IGNORE` downgrades every error in the statement to a warning, not
/// just the duplicate-key one: a label naming an issue that does not exist would
/// violate the foreign key and be silently dropped on the floor. `ON DUPLICATE
/// KEY UPDATE` ignores exactly the collision it is asked to and lets everything
/// else raise.
async fn insert_label(conn: &mut MySqlConnection, issue_id: &str, label: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO labels (issue_id, label) VALUES (?, ?)
         ON DUPLICATE KEY UPDATE label = VALUES(label)",
    )
    .bind(issue_id)
    .bind(label)
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    Ok(())
}

/// Push a `SET col = …` clause for a text column whose domain type is `String`.
///
/// Clearing one of these means the empty string, not NULL: `Issue.notes` is a
/// `String`, so a NULL would only be read back as `""` anyway, and storing one
/// would just give the same value two spellings for a reader to trip over.
///
/// `col` is always a literal from this module, never user input.
fn push_text(qb: &mut QueryBuilder<'_, MySql>, col: &str, field: &Field<String>) {
    match field {
        Field::Keep => {}
        Field::Set(v) => {
            qb.push(format!(", {col} = ")).push_bind(v.clone());
        }
        Field::Clear => {
            qb.push(format!(", {col} = ''"));
        }
    }
}

async fn fetch_issue(conn: &mut MySqlConnection, id: &str) -> Result<Option<Issue>> {
    let row = sqlx::query(&format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE id = ?"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?;
    row.as_ref().map(issue_from_row).transpose()
}

async fn fetch_labels(conn: &mut MySqlConnection, id: &str) -> Result<Vec<String>> {
    sqlx::query_scalar("SELECT label FROM labels WHERE issue_id = ? ORDER BY label")
        .bind(id)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)
}

async fn fetch_dependencies_of(conn: &mut MySqlConnection, id: &str) -> Result<Vec<Dependency>> {
    let rows = sqlx::query(&format!(
        "SELECT {DEPENDENCY_COLUMNS} FROM dependencies
         WHERE issue_id = ? ORDER BY depends_on_id, `type`"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    rows.iter().map(dependency_from_row).collect()
}

/// A comment thread is ordered by *time*. Ordering by id worked only while ids
/// were a monotonic integer; a UUID sorts at random, and a shuffled thread reads
/// as a different conversation.
async fn fetch_comments(conn: &mut MySqlConnection, id: &str) -> Result<Vec<Comment>> {
    let rows = sqlx::query(
        "SELECT id, issue_id, author, `text`, created_at FROM comments
         WHERE issue_id = ? ORDER BY created_at, id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    rows.iter().map(comment_from_row).collect()
}

/// Rewrite `content_hash` from what the issue now says.
///
/// Called from inside every transaction that touches hashed content — which is
/// most of them, because the hash covers the title, the body, the status, the
/// assignee, the close reason *and* the labels. It is deliberately a re-read
/// rather than an incremental adjustment: `Issue::compute_content_hash` in
/// bd-core is the single definition of what the hash is over, and a second copy
/// of that field list here would be the copy that goes stale.
///
/// Note it writes `content_hash` and nothing else — in particular not
/// `updated_at`, which is a record of somebody editing the issue and must not be
/// bumped by derived state.
async fn refresh_content_hash(conn: &mut MySqlConnection, id: &str) -> Result<()> {
    let Some(mut issue) = fetch_issue(conn, id).await? else {
        // Deleted inside this same transaction. Nothing to hash.
        return Ok(());
    };
    issue.labels = fetch_labels(conn, id).await?;

    sqlx::query("UPDATE issues SET content_hash = ? WHERE id = ?")
        .bind(issue.compute_content_hash())
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    Ok(())
}

async fn count(pool: &MySqlPool, sql: &str, binds: &[&str]) -> Result<u64> {
    let mut q = sqlx::query(sql);
    for b in binds {
        q = q.bind(*b);
    }
    let row = q.fetch_one(pool).await.map_err(db)?;
    Ok(row.get::<i64, _>(0) as u64)
}

/// The unit-variant enums (`WispType`, `MolType`, `WorkType`, `EventType`) have
/// no `as_str`, only a serde renaming. Going through serde keeps the stored
/// spelling and the JSON spelling from drifting apart — and it keeps this
/// backend's spelling identical to SQLite's, which is what makes an export from
/// one importable into the other.
fn enum_to_str<T: Serialize>(v: &T) -> Option<String> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Some(s),
        _ => None,
    }
}

fn enum_from_str<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

fn issue_from_row(r: &MySqlRow) -> Result<Issue> {
    let metadata: Option<String> = r.try_get("metadata").map_err(dec)?;
    let metadata = match metadata.as_deref() {
        None | Some("") => None,
        Some(s) => Some(serde_json::from_str(s).map_err(|e| {
            Error::Db(format!("issue {}: corrupt metadata JSON: {e}", row_id(r)))
        })?),
    };

    let opt_enum = |col: &str| -> Result<Option<String>> { r.try_get(col).map_err(dec) };
    let wisp_type = opt_enum("wisp_type")?
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(enum_from_str);
    let mol_type = opt_enum("mol_type")?
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(enum_from_str);
    let work_type = opt_enum("work_type")?
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(enum_from_str);

    Ok(Issue {
        id: r.try_get("id").map_err(dec)?,
        title: r.try_get("title").map_err(dec)?,
        description: r.try_get("description").map_err(dec)?,
        design: r.try_get("design").map_err(dec)?,
        acceptance_criteria: r.try_get("acceptance_criteria").map_err(dec)?,
        notes: r.try_get("notes").map_err(dec)?,

        status: Status::from(r.try_get::<String, _>("status").map_err(dec)?),
        priority: bd_core::Priority(r.try_get("priority").map_err(dec)?),
        issue_type: bd_core::IssueType::from(r.try_get::<String, _>("issue_type").map_err(dec)?),

        assignee: r.try_get("assignee").map_err(dec)?,
        owner: r.try_get("owner").map_err(dec)?,
        created_by: r.try_get("created_by").map_err(dec)?,
        estimated_minutes: r.try_get("estimated_minutes").map_err(dec)?,

        created_at: r.try_get("created_at").map_err(dec)?,
        updated_at: r.try_get("updated_at").map_err(dec)?,
        started_at: r.try_get("started_at").map_err(dec)?,
        closed_at: r.try_get("closed_at").map_err(dec)?,
        close_reason: r.try_get("close_reason").map_err(dec)?,
        closed_by_session: r.try_get("closed_by_session").map_err(dec)?,

        lease_expires_at: r.try_get("lease_expires_at").map_err(dec)?,
        heartbeat_at: r.try_get("heartbeat_at").map_err(dec)?,

        due_at: r.try_get("due_at").map_err(dec)?,
        defer_until: r.try_get("defer_until").map_err(dec)?,

        external_ref: r.try_get("external_ref").map_err(dec)?,
        source_system: r.try_get("source_system").map_err(dec)?,
        spec_id: r.try_get("spec_id").map_err(dec)?,
        metadata,

        ephemeral: r.try_get("ephemeral").map_err(dec)?,
        no_history: r.try_get("no_history").map_err(dec)?,
        pinned: r.try_get("pinned").map_err(dec)?,
        is_template: r.try_get("is_template").map_err(dec)?,

        wisp_type,
        mol_type,
        work_type,

        // Hydrated separately, and only by `get_issue`.
        labels: Vec::new(),
        dependencies: Vec::new(),
        comments: Vec::new(),

        content_hash: r.try_get("content_hash").map_err(dec)?,
    })
}

fn dependency_from_row(r: &MySqlRow) -> Result<Dependency> {
    Ok(Dependency {
        issue_id: r.try_get("issue_id").map_err(dec)?,
        depends_on_id: r.try_get("depends_on_id").map_err(dec)?,
        dep_type: DependencyType::from(r.try_get::<String, _>("type").map_err(dec)?),
        created_at: r.try_get("created_at").map_err(dec)?,
        created_by: r.try_get("created_by").map_err(dec)?,
        metadata: r
            .try_get::<Option<String>, _>("metadata")
            .map_err(dec)?
            .unwrap_or_default(),
        thread_id: r
            .try_get::<Option<String>, _>("thread_id")
            .map_err(dec)?
            .unwrap_or_default(),
    })
}

fn comment_from_row(r: &MySqlRow) -> Result<Comment> {
    Ok(Comment {
        id: r.try_get("id").map_err(dec)?,
        issue_id: r.try_get("issue_id").map_err(dec)?,
        author: r.try_get("author").map_err(dec)?,
        text: r.try_get("text").map_err(dec)?,
        created_at: r.try_get("created_at").map_err(dec)?,
    })
}

fn event_from_row(r: &MySqlRow) -> Result<Event> {
    let raw: String = r.try_get("event_type").map_err(dec)?;
    let event_type: EventType = enum_from_str(&raw)
        .ok_or_else(|| Error::Db(format!("unknown event type in database: {raw}")))?;
    Ok(Event {
        id: r.try_get("id").map_err(dec)?,
        issue_id: r.try_get("issue_id").map_err(dec)?,
        event_type,
        actor: r.try_get("actor").map_err(dec)?,
        old_value: r.try_get("old_value").map_err(dec)?,
        new_value: r.try_get("new_value").map_err(dec)?,
        created_at: r.try_get("created_at").map_err(dec)?,
    })
}

/// Empty strings become NULL on the way in, so that `JSON_VALID` guards and
/// `IS NULL` checks in SQL see one representation of "absent", not two.
fn none_if_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn metadata_to_text(v: &Option<serde_json::Value>) -> Option<String> {
    v.as_ref().map(|m| m.to_string())
}

fn row_id(r: &MySqlRow) -> String {
    r.try_get::<String, _>("id").unwrap_or_default()
}

// ---------------------------------------------------------------------------
// IssueFilter -> SQL
//
// Everything an `IssueFilter` can express is pushed down into the database.
// Nothing is filtered in Rust after the fact: a `LIMIT` applied to a set that was
// then filtered in memory returns the wrong page, and that bug is invisible until
// someone notices `bd list --limit 10` showing four rows.
// ---------------------------------------------------------------------------

/// Append the filter's clauses. The caller has already emitted a `WHERE` and at
/// least one predicate, so every clause here starts with `AND`.
fn push_filter(qb: &mut QueryBuilder<'_, MySql>, f: &IssueFilter) {
    if let Some(s) = &f.status {
        qb.push(" AND status = ").push_bind(s.as_str().to_string());
    }
    if !f.statuses.is_empty() {
        qb.push(" AND status IN (");
        let mut sep = qb.separated(", ");
        for s in &f.statuses {
            sep.push_bind(s.as_str().to_string());
        }
        qb.push(")");
    }
    // `NOT status=closed` in the query DSL. This clause is what lets that query
    // be answered in SQL at all — without it the DSL has to fall back to an
    // in-memory pass over every issue in the database.
    if !f.exclude_statuses.is_empty() {
        qb.push(" AND status NOT IN (");
        let mut sep = qb.separated(", ");
        for s in &f.exclude_statuses {
            sep.push_bind(s.as_str().to_string());
        }
        qb.push(")");
    }
    if let Some(p) = f.priority {
        qb.push(" AND priority = ").push_bind(p.0);
    }
    // P0 is the *most* important, so "minimum priority P2" means "P2 or better",
    // which is a numeric <=. Reading this as >= would silently invert the filter.
    // `max_priority` is the mirror image: "P2 or worse", a numeric >=.
    if let Some(p) = f.min_priority {
        qb.push(" AND priority <= ").push_bind(p.0);
    }
    if let Some(p) = f.max_priority {
        qb.push(" AND priority >= ").push_bind(p.0);
    }
    if let Some(t) = &f.issue_type {
        qb.push(" AND issue_type = ").push_bind(t.as_str().to_string());
    }
    if !f.exclude_types.is_empty() {
        qb.push(" AND issue_type NOT IN (");
        let mut sep = qb.separated(", ");
        for t in &f.exclude_types {
            sep.push_bind(t.as_str().to_string());
        }
        qb.push(")");
    }
    // Exact, byte-for-byte — which on MySQL is a property of the *column's*
    // collation, not of the comparison. `schema.sql` declares every table
    // `utf8mb4_0900_bin` for exactly this line: under MySQL's default
    // `utf8mb4_0900_ai_ci` this would also match `'ALICE'` and `'álice'`, and
    // bd-query's property tests say string equality is exact.
    if let Some(a) = &f.assignee {
        qb.push(" AND assignee = ").push_bind(a.clone());
    }
    if let Some(o) = &f.owner {
        qb.push(" AND owner = ").push_bind(o.clone());
    }

    for label in &f.labels_all {
        qb.push(" AND id IN (SELECT issue_id FROM labels WHERE label = ")
            .push_bind(label.clone())
            .push(")");
    }
    if !f.labels_any.is_empty() {
        qb.push(" AND id IN (SELECT issue_id FROM labels WHERE label IN (");
        let mut sep = qb.separated(", ");
        for label in &f.labels_any {
            sep.push_bind(label.clone());
        }
        qb.push("))");
    }

    // Transitive, not one hop. `--parent` promising "everything under this epic"
    // and delivering only its direct children is the kind of quiet undercount
    // that makes an agent think an epic is finished.
    if let Some(parent) = &f.parent {
        qb.push(
            " AND id IN (
                WITH RECURSIVE descendants(id) AS (
                    SELECT issue_id FROM dependencies
                    WHERE `type` = 'parent-child' AND depends_on_id = ",
        )
        .push_bind(parent.clone())
        .push(
            "
                    UNION
                    SELECT d.issue_id FROM dependencies d
                    JOIN descendants ON d.depends_on_id = descendants.id
                    WHERE d.`type` = 'parent-child'
                )
                SELECT id FROM descendants
            )",
        );
    }

    if let Some(spec) = &f.spec_id {
        qb.push(" AND spec_id = ").push_bind(spec.clone());
    }
    // The tracker join key. `external_ref` is nullable, and `col = ?` is never
    // true of NULL — so an issue bound to no remote is correctly excluded rather
    // than matching every ref.
    if let Some(sys) = &f.source_system {
        qb.push(" AND source_system = ").push_bind(sys.clone());
    }
    if let Some(r) = &f.external_ref {
        qb.push(" AND external_ref = ").push_bind(r.clone());
    }
    // SQLite's two-argument `json_type(doc, path)` does not exist in MySQL —
    // `JSON_TYPE` there takes a single JSON value. `JSON_EXTRACT` returns SQL
    // NULL for a path that is absent and a JSON `null` (which is *not* SQL NULL)
    // for a key explicitly set to null, so `IS NOT NULL` answers "has this key"
    // exactly as `json_type` did.
    //
    // The `JSON_VALID` guard is not optional: `JSON_EXTRACT` raises a hard error
    // on malformed input, and a single corrupt metadata blob would fail the
    // whole listing rather than just failing to match.
    if let Some(key) = &f.has_metadata_key {
        qb.push(" AND (CASE WHEN JSON_VALID(metadata) THEN JSON_EXTRACT(metadata, CONCAT('$.\"', ")
            .push_bind(key.clone())
            .push(", '\"')) END) IS NOT NULL");
    }

    if let Some(t) = f.created_after {
        qb.push(" AND created_at > ").push_bind(t);
    }
    if let Some(t) = f.created_before {
        qb.push(" AND created_at < ").push_bind(t);
    }
    if let Some(t) = f.updated_after {
        qb.push(" AND updated_at > ").push_bind(t);
    }
    if let Some(t) = f.updated_before {
        qb.push(" AND updated_at < ").push_bind(t);
    }
    if let Some(t) = f.closed_after {
        qb.push(" AND closed_at IS NOT NULL AND closed_at > ")
            .push_bind(t);
    }
    if let Some(t) = f.closed_before {
        qb.push(" AND closed_at IS NOT NULL AND closed_at < ")
            .push_bind(t);
    }

    // Two dialect traps in three lines.
    //
    // **LOWER()**, because the `_bin` collation that makes `=` byte-exact also
    // makes `LIKE` case-*sensitive*, where SQLite's is ASCII-case-insensitive and
    // `IssueFilter::text` is documented as case-insensitive. Without it,
    // `bd list --text foo` would stop finding "Foo" the moment a workspace moved
    // to this backend — a wrong answer, not an error.
    //
    // **The raw string**, because backslash is an escape character inside a MySQL
    // string literal and is not one inside a SQLite string literal. bd-sqlite
    // writes `" ESCAPE '\\'"`, which is the four characters `'\'` — a perfectly
    // good one-backslash string to SQLite, and to MySQL an *unterminated* literal
    // whose backslash escapes the closing quote. The SQL has to carry two
    // backslashes, so the Rust has to be raw (or carry four).
    if let Some(text) = &f.text {
        let pat = format!("%{}%", escape_like(&text.to_lowercase()));
        qb.push(" AND (LOWER(title) LIKE ")
            .push_bind(pat.clone())
            .push(r" ESCAPE '\\' OR LOWER(description) LIKE ")
            .push_bind(pat)
            .push(r" ESCAPE '\\')");
    }

    if let Some(b) = f.pinned {
        qb.push(" AND pinned = ").push_bind(b);
    }
    if let Some(b) = f.ephemeral {
        qb.push(" AND ephemeral = ").push_bind(b);
    }
    if let Some(b) = f.is_template {
        qb.push(" AND is_template = ").push_bind(b);
    }
    if let Some(b) = f.is_blocked {
        qb.push(" AND is_blocked = ").push_bind(b);
    }
    if let Some(b) = f.lease_active {
        push_lease(qb, b);
    }
}

/// Whether the issue is under a lease that has **not** yet expired.
///
/// The clock is bound from Rust, never `NOW()`: the server's clock is not this
/// process's clock, and on this backend it may not even be on this machine.
fn push_lease(qb: &mut QueryBuilder<'_, MySql>, active: bool) {
    if active {
        qb.push(" AND lease_expires_at IS NOT NULL AND lease_expires_at > ")
            .push_bind(Utc::now());
    } else {
        // A lapsed lease is not a claim. This is the term that makes leases
        // self-healing: the moment it expires, the work is back on offer.
        qb.push(" AND (lease_expires_at IS NULL OR lease_expires_at <= ")
            .push_bind(Utc::now())
            .push(")");
    }
}

/// The ready-work predicates, which no caller-supplied filter may relax.
///
/// `bd ready` means "claimable *right now*". A filter that could switch off the
/// `is_blocked = 0` term would turn `bd ready` into `bd list` with extra steps.
fn push_ready_predicates(qb: &mut QueryBuilder<'_, MySql>, blocked: bool) {
    qb.push(" AND is_blocked = ").push_bind(blocked);
    qb.push(" AND pinned = 0 AND ephemeral = 0");
    qb.push(" AND status IN ('open', 'in_progress')");

    qb.push(" AND issue_type NOT IN (");
    let mut sep = qb.separated(", ");
    for t in READY_EXCLUDED_TYPES {
        sep.push_bind(t.to_string());
    }
    qb.push(")");

    if !blocked {
        // A deferred bead is not blocked, it is *early*. Blocked work stays
        // visible in `bd blocked` regardless of its defer time, so this term is
        // only applied to the ready side.
        qb.push(" AND (defer_until IS NULL OR defer_until <= ")
            .push_bind(Utc::now())
            .push(")");

        // Nor is a *held* bead. An issue somebody claimed five minutes ago is
        // not claimable — `claim_issue` will fence a second agent out of it — so
        // offering it in `bd ready` only means one of the two agents finds out by
        // failing, after it has read the issue and started thinking.
        //
        // Applied here rather than left to `IssueFilter::ready()` alone because
        // `count_work` (and therefore `bd status`, `bd info`, `bd prime`) counts
        // ready work without any filter at all. A count that disagreed with the
        // list would be its own bug.
        push_lease(qb, false);
    }
}

/// ORDER BY for a sort policy, plus LIMIT/OFFSET.
///
/// The hybrid policy is the default and the interesting one: work created inside
/// the last 48h is ranked by priority, and work older than that is ranked by age.
/// A pure priority sort starves old P3s forever; a pure age sort buries a P0
/// filed this morning behind a year of backlog.
///
/// Two things are written as literals rather than bound, and both for the same
/// reason — a placeholder outside a `WHERE` clause is the part of the protocol
/// servers disagree about, and none of these values is user text:
///
///   * the hybrid cutoff, which appears inside an `ORDER BY` expression;
///   * `LIMIT`/`OFFSET`, which are `u32` from the CLI.
///
/// Note also that SQLite's `LIMIT -1` idiom for "no limit" is a syntax error in
/// MySQL. The equivalent is the largest `BIGINT UNSIGNED`, which is what the
/// MySQL manual itself recommends for offset-without-limit.
fn push_order_and_limit(qb: &mut QueryBuilder<'_, MySql>, f: &IssueFilter) {
    match f.sort {
        SortPolicy::Hybrid => {
            let cutoff = sql_datetime(Utc::now() - SortPolicy::HYBRID_RECENCY_WINDOW);
            qb.push(format!(
                " ORDER BY CASE WHEN created_at >= '{cutoff}' THEN 0 ELSE 1 END ASC, \
                 CASE WHEN created_at >= '{cutoff}' THEN priority ELSE 999 END ASC, \
                 created_at ASC, id ASC"
            ));
        }
        SortPolicy::Priority => {
            qb.push(" ORDER BY priority ASC, created_at ASC, id ASC");
        }
        SortPolicy::Oldest => {
            qb.push(" ORDER BY created_at ASC, id ASC");
        }
        // Ascending: `bd stale` asks "what has nobody touched", and the answer
        // leads with what nobody has touched for longest.
        SortPolicy::Updated => {
            qb.push(" ORDER BY updated_at ASC, id ASC");
        }
        // Descending, and pointing the other way on purpose — the question is
        // "what did we just finish". MySQL, like SQLite, sorts NULLs last under
        // DESC, so open issues (which have no close time) fall to the end rather
        // than the top. That the two agree here is luck, not standardization;
        // it is checked by a test.
        SortPolicy::Closed => {
            qb.push(" ORDER BY closed_at DESC, id ASC");
        }
    }

    match (f.limit, f.offset) {
        (Some(n), Some(o)) => {
            qb.push(format!(" LIMIT {n} OFFSET {o}"));
        }
        (Some(n), None) => {
            qb.push(format!(" LIMIT {n}"));
        }
        (None, Some(o)) => {
            qb.push(format!(" LIMIT {NO_LIMIT} OFFSET {o}"));
        }
        (None, None) => {}
    }
}

/// MySQL's "all rows" limit, for an OFFSET with no LIMIT. `LIMIT -1` is SQLite's.
const NO_LIMIT: u64 = u64::MAX;

/// A `DATETIME(6)` literal. Only ever fed values this process minted.
fn sql_datetime(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

// ---------------------------------------------------------------------------
// The `is_blocked` fixpoint
//
// `bd ready` does not traverse the dependency graph. It filters on the
// `issues.is_blocked` column, which is a *cache* of the graph maintained here.
// A stale `is_blocked` does not make `bd ready` slow — it makes `bd ready` lie,
// and an agent that is lied to about what is claimable will happily work on a
// bead whose blocker is still open.
//
// # The rule
//
// An issue is blocked iff it is itself neither closed nor pinned, and any of:
//
//   1. it has a `blocks` or `conditional-blocks` edge to a target still live;
//   2. it has a `parent-child` edge to a parent that is itself blocked — that is,
//      blocked-ness propagates *down* the containment tree;
//   3. it has a `waits-for` edge to a spawner whose gate is unsatisfied.
//
// # Why a fixpoint, and not one pass
//
// Rule 2 is transitive. Given `A blocks B`, `C` a child of `B`, `D` a child of
// `C`: closing `A` unblocks `B`; only *then* does `C` see an unblocked parent;
// only then does `D`. A single mark/unmark pass propagates exactly one level per
// statement, and which level it catches depends on the order the server happens
// to visit rows in — so one pass leaves `C` and `D` wrongly blocked, and does so
// *nondeterministically*. Iterating to a fixpoint is not an optimization; it is
// the algorithm. bd-sqlite has a test that asserts one pass gets it wrong.
//
// # `conditional-blocks`
//
// `B conditional-blocks A` means "run B only if A **fails**". B is blocked while
// A is open, and when A closes B becomes ready only if A's close reason reads as
// a failure. If A closed successfully the failure path is moot and B stays
// blocked — deliberately, because a store that silently closes beads nobody asked
// it to close is worse than one that leaves a visibly-stuck bead for a human.
//
// # Nothing here writes `updated_at`, and that omission is load-bearing
//
// `is_blocked` is DERIVED state. Bumping `updated_at` when it flips stamps the
// local machine's wall clock onto a row in a version-controlled table, for a
// change the user never made. Two clones that recompute the same flip a second
// apart then disagree on `updated_at` and hand the merge a conflict on a column
// neither of them edited. On SQLite that was a nicety. **Here the table actually
// gets merged, so it is a real bug.** The schema also declines to give any column
// `ON UPDATE CURRENT_TIMESTAMP`, which would have re-introduced it behind the SQL.
// ---------------------------------------------------------------------------

/// "Alive": the state in which an issue still gates whatever depends on it.
/// Pinned-ness is expressible two ways — as a status and as a flag — and both
/// must count, or a bead pinned one way keeps blocking while a bead pinned the
/// other way does not.
const LIVE: &str = "(t.status <> 'closed' AND t.status <> 'pinned' AND t.pinned = 0)";

/// The blocking predicate, for a row aliased `i`. Rules 1-3 above, in order.
fn blocking_predicate() -> String {
    format!(
        r#"
        EXISTS (
            SELECT 1 FROM dependencies d JOIN issues t ON t.id = d.depends_on_id
            WHERE d.issue_id = i.id AND d.`type` = 'blocks'
              AND {LIVE}
        )
        OR EXISTS (
            SELECT 1 FROM dependencies d JOIN issues t ON t.id = d.depends_on_id
            WHERE d.issue_id = i.id AND d.`type` = 'conditional-blocks'
              AND (
                    {LIVE}
                    OR (t.status = 'closed' AND t.close_is_failure = 0)
              )
        )
        OR EXISTS (
            SELECT 1 FROM dependencies d JOIN issues t ON t.id = d.depends_on_id
            WHERE d.issue_id = i.id AND d.`type` = 'parent-child'
              AND t.is_blocked = 1
        )
        OR EXISTS (
            SELECT 1 FROM dependencies d
            WHERE d.issue_id = i.id AND d.`type` = 'waits-for'
              AND {gate}
        )
        "#,
        gate = waits_for_gate_blocked()
    )
}

/// A `waits-for` edge names a *spawner*; the gate is over the spawner's
/// children. By default every child must be done. An edge whose metadata says
/// `{"gate":"any-children"}` opens as soon as one child closes.
///
/// **`JSON_UNQUOTE` is the whole ballgame.** MySQL's `JSON_EXTRACT` returns a
/// *JSON value*, so `$.gate` on `{"gate":"any-children"}` comes back as the
/// six-and-a-bit bytes `"any-children"` — quotes included — and `= 'any-children'`
/// is then silently false. SQLite's `json_extract` unquotes for you. Ported
/// verbatim, every `any-children` gate on this backend would have stayed shut
/// forever, with no error anywhere.
///
/// `JSON_VALID` guards because `JSON_EXTRACT` raises on malformed input, which
/// inside a recompute would abort the whole pass; and the `COALESCE` guards
/// because a NULL would poison the surrounding `NOT (...)` into NULL and silently
/// unblock the waiter.
fn waits_for_gate_blocked() -> String {
    format!(
        r#"(
            EXISTS (
                SELECT 1 FROM dependencies cd JOIN issues t ON t.id = cd.issue_id
                WHERE cd.`type` = 'parent-child' AND cd.depends_on_id = d.depends_on_id
                  AND {LIVE}
            )
            AND NOT (
                COALESCE(
                    CASE WHEN JSON_VALID(d.metadata)
                         THEN JSON_UNQUOTE(JSON_EXTRACT(d.metadata, '$.gate')) END,
                    ''
                ) = 'any-children'
                AND EXISTS (
                    SELECT 1 FROM dependencies cd JOIN issues t ON t.id = cd.issue_id
                    WHERE cd.`type` = 'parent-child' AND cd.depends_on_id = d.depends_on_id
                      AND t.status = 'closed'
                )
            )
        )"#
    )
}

// The mark/unmark statements are SELECTs, where bd-sqlite's are UPDATEs.
//
// Not a style choice: MySQL raises error 1093 — "You can't specify target table
// 'issues' for update in FROM clause" — for an `UPDATE issues ... WHERE EXISTS
// (SELECT ... FROM issues ...)`, which is precisely the shape of the blocking
// predicate. So the ids that must flip are selected first and written second, in
// the same transaction, which is semantically identical and costs one extra round
// trip per pass.

fn mark_select_sql(where_ids: &str) -> String {
    format!(
        r#"SELECT i.id FROM issues i
           WHERE {where_ids}
             AND i.is_blocked = 0
             AND i.status <> 'closed' AND i.status <> 'pinned' AND i.pinned = 0
             AND ({pred})"#,
        pred = blocking_predicate()
    )
}

fn unmark_select_sql(where_ids: &str) -> String {
    format!(
        r#"SELECT i.id FROM issues i
           WHERE {where_ids}
             AND i.is_blocked = 1
             AND (
                   i.status = 'closed' OR i.status = 'pinned' OR i.pinned = 1
                   OR NOT ({pred})
             )"#,
        pred = blocking_predicate()
    )
}

/// Write `is_blocked` for exactly these ids — and nothing else. In particular
/// not `updated_at`; see the module note above.
async fn set_blocked(conn: &mut MySqlConnection, ids: &[String], value: bool) -> Result<u64> {
    let mut changed = 0u64;
    for chunk in ids.chunks(CHUNK) {
        let mut qb: QueryBuilder<MySql> = QueryBuilder::new("UPDATE issues SET is_blocked = ");
        qb.push_bind(value);
        qb.push(" WHERE id IN (");
        let mut sep = qb.separated(", ");
        for id in chunk {
            sep.push_bind(id.clone());
        }
        qb.push(")");
        changed += qb
            .build()
            .execute(&mut *conn)
            .await
            .map_err(db)?
            .rows_affected();
    }
    Ok(changed)
}

/// Recompute `is_blocked` for every issue in the table, to a fixpoint.
///
/// Required after anything that changes rows behind the store's back — an import,
/// a merge, a pull. The incremental path cannot help there: it seeds from the ids
/// a write path touched, and a merge touched ids no write path saw.
///
/// Also refreshes the derived `close_is_failure` column first, for the same
/// reason: rows that arrived without going through `close_issue` never had it
/// computed.
///
/// Returns the number of rows whose `is_blocked` changed.
async fn recompute_all(conn: &mut MySqlConnection) -> Result<u64> {
    refresh_close_is_failure(conn).await?;

    let mut total = 0u64;
    for _ in 0..MAX_ITERATIONS {
        let mut changed = 0u64;

        let to_mark: Vec<String> = sqlx::query_scalar(&mark_select_sql("1 = 1"))
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
        changed += set_blocked(conn, &to_mark, true).await?;

        let to_unmark: Vec<String> = sqlx::query_scalar(&unmark_select_sql("1 = 1"))
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
        changed += set_blocked(conn, &to_unmark, false).await?;

        total += changed;
        if changed == 0 {
            return Ok(total);
        }
    }
    Err(not_converged())
}

/// Recompute `is_blocked` for everything a change to `seed_ids` could possibly
/// affect, to a fixpoint. This is what write paths call, inside their own
/// transaction.
async fn recompute_affected(conn: &mut MySqlConnection, seed_ids: &[String]) -> Result<u64> {
    if seed_ids.is_empty() {
        return Ok(0);
    }
    let affected = affected_set(conn, seed_ids).await?;
    fixpoint(conn, &affected).await
}

/// The closure of `seed_ids` under "could this row's `is_blocked` change?".
///
/// Three sources, and missing any one of them leaves a stale row:
///
/// * **blocking dependers** — whoever has a `blocks` / `conditional-blocks` /
///   `waits-for` edge *into* a seed reads the seed's status;
/// * **waiters on the seed's parents** — a `waits-for` gate is over a spawner's
///   *children*, so a child changing status moves a gate the child has no edge to;
/// * **parent-child descendants**, transitively. Expanded by BFS from the whole
///   seed set (dependers included), because a depender that flips must in turn
///   push the flip down its own subtree.
///
/// Callers that are about to *delete* rows must call this first: the edges it
/// walks are the very edges the delete will cascade away.
async fn affected_set(conn: &mut MySqlConnection, seed_ids: &[String]) -> Result<Vec<String>> {
    let mut seen: HashSet<String> = seed_ids.iter().cloned().collect();
    let mut queue: Vec<String> = seed_ids.to_vec();

    for chunk in seed_ids.chunks(CHUNK) {
        for id in select_ids(
            conn,
            "SELECT issue_id FROM dependencies
             WHERE `type` IN ('blocks', 'conditional-blocks', 'waits-for')
               AND depends_on_id IN ",
            chunk,
            "",
        )
        .await?
        {
            if seen.insert(id.clone()) {
                queue.push(id);
            }
        }

        for id in select_ids(
            conn,
            "SELECT w.issue_id FROM dependencies w
             WHERE w.`type` = 'waits-for'
               AND w.depends_on_id IN (
                   SELECT pc.depends_on_id FROM dependencies pc
                   WHERE pc.`type` = 'parent-child' AND pc.issue_id IN ",
            chunk,
            ")",
        )
        .await?
        {
            if seen.insert(id.clone()) {
                queue.push(id);
            }
        }
    }

    let mut head = 0;
    while head < queue.len() {
        let end = (head + CHUNK).min(queue.len());
        let frontier: Vec<String> = queue[head..end].to_vec();
        head = end;

        for id in select_ids(
            conn,
            "SELECT issue_id FROM dependencies
             WHERE `type` = 'parent-child' AND depends_on_id IN ",
            &frontier,
            "",
        )
        .await?
        {
            if seen.insert(id.clone()) {
                queue.push(id);
            }
        }
    }

    Ok(queue)
}

/// Iterate mark/unmark over exactly `ids` until nothing changes.
async fn fixpoint(conn: &mut MySqlConnection, ids: &[String]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }

    let mut total = 0u64;
    for _ in 0..MAX_ITERATIONS {
        let mut changed = 0u64;

        for chunk in ids.chunks(CHUNK) {
            // Mark first, then unmark, and each reads the state the other left:
            // that ordering is what lets a single pass make progress at all.
            let to_mark = select_flips(conn, &mark_select_sql(&id_predicate(chunk.len())), chunk)
                .await?;
            changed += set_blocked(conn, &to_mark, true).await?;

            let to_unmark =
                select_flips(conn, &unmark_select_sql(&id_predicate(chunk.len())), chunk).await?;
            changed += set_blocked(conn, &to_unmark, false).await?;
        }

        total += changed;
        if changed == 0 {
            return Ok(total);
        }
    }
    Err(not_converged())
}

/// Exactly one mark/unmark pass. Exists so a test can demonstrate that one pass
/// is *not enough* — the same demonstration bd-sqlite's
/// `one_pass_leaves_the_deep_end_of_the_chain_wrong` makes.
pub async fn one_pass(conn: &mut MySqlConnection, ids: &[String]) -> Result<u64> {
    let mut changed = 0u64;
    for chunk in ids.chunks(CHUNK) {
        let to_mark =
            select_flips(conn, &mark_select_sql(&id_predicate(chunk.len())), chunk).await?;
        changed += set_blocked(conn, &to_mark, true).await?;

        let to_unmark =
            select_flips(conn, &unmark_select_sql(&id_predicate(chunk.len())), chunk).await?;
        changed += set_blocked(conn, &to_unmark, false).await?;
    }
    Ok(changed)
}

fn id_predicate(n: usize) -> String {
    let ph = std::iter::repeat_n("?", n).collect::<Vec<_>>().join(", ");
    format!("i.id IN ({ph})")
}

async fn select_flips(
    conn: &mut MySqlConnection,
    sql: &str,
    ids: &[String],
) -> Result<Vec<String>> {
    let mut q = sqlx::query_scalar::<_, String>(sql);
    for id in ids {
        q = q.bind(id.clone());
    }
    q.fetch_all(&mut *conn).await.map_err(db)
}

/// Recompute the derived `close_is_failure` column from `close_reason`, using
/// bd-core as the sole authority on what "failure" reads like.
///
/// Writes `close_is_failure` and nothing else — the same rule as `is_blocked`.
async fn refresh_close_is_failure(conn: &mut MySqlConnection) -> Result<()> {
    let rows = sqlx::query("SELECT id, close_reason, close_is_failure FROM issues")
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;

    let stale: Vec<(String, bool)> = rows
        .into_iter()
        .filter_map(|r| {
            let id: String = r.get("id");
            let reason: String = r.get("close_reason");
            let stored: bool = r.get("close_is_failure");
            let want = bd_core::types::is_failure_close(&reason);
            (want != stored).then_some((id, want))
        })
        .collect();

    for (id, want) in stale {
        sqlx::query("UPDATE issues SET close_is_failure = ? WHERE id = ?")
            .bind(want)
            .bind(&id)
            .execute(&mut *conn)
            .await
            .map_err(db)?;
    }
    Ok(())
}

async fn select_ids(
    conn: &mut MySqlConnection,
    prefix: &str,
    ids: &[String],
    suffix: &str,
) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb: QueryBuilder<MySql> = QueryBuilder::new(prefix);
    qb.push("(");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(id.clone());
    }
    qb.push(")");
    qb.push(suffix);

    let rows = qb.build().fetch_all(&mut *conn).await.map_err(db)?;
    Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
}

fn not_converged() -> Error {
    Error::Db(format!(
        "is_blocked did not converge after {MAX_ITERATIONS} passes; \
         the dependency graph almost certainly contains a cycle (try `bd dep cycles`). \
         On this backend a cycle can arrive through a merge, not only through a write."
    ))
}

// ---------------------------------------------------------------------------
// Cycles
// ---------------------------------------------------------------------------

async fn load_ordering_edges(pool: &MySqlPool) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT issue_id, depends_on_id FROM dependencies
         WHERE `type` IN ('blocks', 'parent-child')",
    )
    .fetch_all(pool)
    .await
    .map_err(db)?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("issue_id"),
                r.get::<String, _>("depends_on_id"),
            )
        })
        .collect())
}

/// Is `to` reachable from `from` along ordering edges? Returns the path if so.
///
/// Used to reject a new edge that would close a loop, *before* it is written —
/// once a cycle exists, the `is_blocked` fixpoint has to defend itself against
/// it, and `bd dep tree` never terminates.
async fn path_between(
    conn: &mut MySqlConnection,
    from: &str,
    to: &str,
) -> Result<Option<Vec<String>>> {
    let mut prev: HashMap<String, String> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::from([from.to_string()]);
    let mut queue: Vec<String> = vec![from.to_string()];
    let mut head = 0;

    while head < queue.len() {
        let node = queue[head].clone();
        head += 1;

        if node == to {
            let mut path = vec![node.clone()];
            let mut cur = node;
            while let Some(p) = prev.get(&cur) {
                path.push(p.clone());
                cur = p.clone();
            }
            path.reverse();
            return Ok(Some(path));
        }

        let next: Vec<String> = sqlx::query_scalar(
            "SELECT depends_on_id FROM dependencies
             WHERE issue_id = ? AND `type` IN ('blocks', 'parent-child')",
        )
        .bind(&node)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;

        for n in next {
            if seen.insert(n.clone()) {
                prev.insert(n.clone(), node.clone());
                queue.push(n);
            }
        }
    }
    Ok(None)
}

/// Iterative three-colour DFS. Recursion would be the obvious way to write this
/// and would blow the stack on a deep import.
fn detect_cycles(edges: &[(String, String)]) -> Vec<Vec<String>> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut nodes: Vec<&str> = Vec::new();
    for (from, to) in edges {
        if !adj.contains_key(from.as_str()) {
            nodes.push(from.as_str());
        }
        adj.entry(from.as_str()).or_default().push(to.as_str());
        if !adj.contains_key(to.as_str()) {
            nodes.push(to.as_str());
            adj.entry(to.as_str()).or_default();
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        White,
        Grey,
        Black,
    }

    let mut colour: HashMap<&str, Colour> = nodes.iter().map(|n| (*n, Colour::White)).collect();
    let mut cycles: Vec<Vec<String>> = Vec::new();

    for root in &nodes {
        if colour[root] != Colour::White {
            continue;
        }

        // (node, index of the next neighbour to visit)
        let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
        let mut path: Vec<&str> = vec![root];
        colour.insert(root, Colour::Grey);

        while let Some((node, idx)) = stack.pop() {
            let neighbours = adj.get(node).map(Vec::as_slice).unwrap_or(&[]);
            if idx < neighbours.len() {
                stack.push((node, idx + 1));
                let next = neighbours[idx];
                match colour.get(next).copied().unwrap_or(Colour::White) {
                    // A grey neighbour is an ancestor on the current path: the
                    // path from it to here, plus the edge back, is the cycle.
                    Colour::Grey => {
                        if let Some(at) = path.iter().position(|p| *p == next) {
                            let mut c: Vec<String> =
                                path[at..].iter().map(|s| s.to_string()).collect();
                            c.push(next.to_string());
                            cycles.push(c);
                        }
                    }
                    Colour::White => {
                        colour.insert(next, Colour::Grey);
                        path.push(next);
                        stack.push((next, 0));
                    }
                    Colour::Black => {}
                }
            } else {
                colour.insert(node, Colour::Black);
                path.pop();
            }
        }
    }
    cycles
}

fn db(e: impl std::fmt::Display) -> Error {
    Error::Db(e.to_string())
}

fn dec(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

// ---------------------------------------------------------------------------
// Tests
//
// Everything here runs without a `dolt` binary, because there is none on the
// machine this was written on. What that buys is real but bounded: it proves the
// SQL this module *emits* is MySQL and not SQLite, and it proves the schema is
// MySQL and not SQLite. It proves nothing about whether Dolt accepts either.
// The end-to-end tests that would are in `tests/store.rs`, and they skip loudly.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bd_core::{IssueType, Priority};

    fn filter_sql(f: &IssueFilter) -> String {
        let mut qb: QueryBuilder<MySql> = QueryBuilder::new("SELECT id FROM issues WHERE 1 = 1");
        push_filter(&mut qb, f);
        push_order_and_limit(&mut qb, f);
        qb.sql().to_string()
    }

    /// The DDL the server actually sees: comments stripped, statements rejoined.
    /// Asserting against the raw file would only prove things about its prose —
    /// and the prose necessarily *names* every construct these tests forbid.
    fn ddl() -> String {
        schema_statements(crate::SCHEMA).join(";\n")
    }

    // -- the schema ----------------------------------------------------------

    #[test]
    fn the_schema_splits_into_one_statement_per_table() {
        let stmts = schema_statements(crate::SCHEMA);
        // issues, dependencies, labels, comments, events, config, schema_meta.
        assert_eq!(
            stmts.len(),
            7,
            "expected one CREATE TABLE per table, got:\n{stmts:#?}"
        );
        for s in &stmts {
            assert!(s.starts_with("CREATE TABLE"), "not a CREATE TABLE: {s}");
            assert!(!s.contains("--"), "line comments should be stripped: {s}");
        }
    }

    /// MySQL has no `CREATE INDEX IF NOT EXISTS`. The SQLite schema's standalone
    /// index statements would therefore fail on the *second* open of a workspace
    /// — the first would create them and the second would blow up on a duplicate
    /// key name, which reads as corruption. Indexes live inside `CREATE TABLE`.
    #[test]
    fn there_are_no_standalone_create_index_statements() {
        assert!(!ddl().contains("CREATE INDEX"));
        assert!(ddl().contains("KEY idx_issues_ready"));
    }

    /// Dolt is a versioned store: a table with no primary key cannot be diffed or
    /// three-way merged row-wise.
    #[test]
    fn every_table_has_a_primary_key() {
        for s in schema_statements(crate::SCHEMA) {
            assert!(
                s.contains("PRIMARY KEY"),
                "a Dolt table without a PRIMARY KEY cannot be merged:\n{s}"
            );
        }
    }

    /// The single most expensive collation decision in the crate. MySQL's default
    /// is case- and accent-insensitive, so `assignee = 'Alice'` would match
    /// `'alice'` — which SQLite does not, and which bd-query's property tests
    /// assert does not.
    #[test]
    fn every_table_pins_a_byte_exact_collation() {
        for s in schema_statements(crate::SCHEMA) {
            assert!(
                s.contains("COLLATE=utf8mb4_0900_bin"),
                "string equality would stop being exact:\n{s}"
            );
        }
        let ddl = ddl();
        assert!(
            !ddl.contains("_ai_ci") && !ddl.contains("_as_cs"),
            "a collation-aware comparison is not a byte-exact one"
        );
    }

    /// The bug this schema exists to avoid. A `TIMESTAMP` column silently
    /// acquires `ON UPDATE CURRENT_TIMESTAMP`, which would re-stamp `updated_at`
    /// every time the derived `is_blocked` cache flipped — planting per-clone wall
    /// clock into a version-controlled table and conflicting two clones on a
    /// column neither of them edited.
    #[test]
    fn no_column_auto_updates_a_timestamp() {
        let ddl = ddl();
        assert!(!ddl.contains("ON UPDATE"));
        assert!(
            !ddl.contains("TIMESTAMP"),
            "use DATETIME: TIMESTAMP is session-timezone-converted and auto-updating"
        );
        assert!(
            ddl.contains("DATETIME(6)") && !ddl.contains("DATETIME "),
            "a bare DATETIME truncates to whole seconds, collapsing lease expiry"
        );
    }

    /// MySQL parses an inline column-level `REFERENCES ... ON DELETE CASCADE` and
    /// then ignores it. Written the SQLite way, `delete_issue` would leave orphan
    /// edges, labels and comments behind — with no error at all.
    #[test]
    fn foreign_keys_are_declared_at_table_level() {
        let ddl = ddl();
        assert_eq!(
            ddl.matches("FOREIGN KEY").count(),
            4,
            "dependencies x2, labels, comments"
        );
        for line in ddl.lines() {
            let l = line.trim_start();
            if l.starts_with("CONSTRAINT") || l.starts_with("FOREIGN KEY") {
                continue;
            }
            assert!(
                !l.contains("REFERENCES"),
                "MySQL silently ignores an inline column REFERENCES: {line}"
            );
        }
    }

    /// MySQL error 1101: a TEXT/BLOB column cannot have a DEFAULT.
    #[test]
    fn no_text_column_carries_a_default() {
        for line in ddl().lines() {
            assert!(
                !(line.contains("TEXT") && line.contains("DEFAULT")),
                "MySQL rejects a DEFAULT on a TEXT column: {line}"
            );
        }
    }

    /// `KEY` is reserved. Unquoted, `SELECT value FROM config WHERE key = ?` — the
    /// SQLite spelling, which works there — is a syntax error against Dolt.
    #[test]
    fn the_config_key_column_is_quoted() {
        let ddl = ddl();
        assert!(ddl.contains("`key` VARCHAR(255)"));
        assert!(ddl.contains("PRIMARY KEY (`key`)"));
    }

    /// SQLite-isms that would parse as nothing on MySQL.
    #[test]
    fn the_schema_is_free_of_sqlite_spellings() {
        let ddl = ddl();
        assert!(!ddl.contains("AUTOINCREMENT"), "AUTOINCREMENT is the SQLite spelling");
        assert!(!ddl.contains("INTEGER"), "INTEGER is the SQLite integer type; MySQL is INT/BIGINT");
        // No `AUTO_INCREMENT` assertion any more: the events table was the only
        // one that used it, and its id is now a client-minted UUID (so a merge
        // between clones cannot collide two events on one key). The schema having
        // *no* autoincrement at all is correct, not a regression.
    }

    // -- filter pushdown -----------------------------------------------------

    /// SQLite spells "no limit" `LIMIT -1`. MySQL rejects a negative limit
    /// outright, so `bd list --offset 20` with no `--limit` would have been a hard
    /// error on this backend and nowhere else.
    #[test]
    fn an_offset_without_a_limit_uses_the_mysql_idiom() {
        let f = IssueFilter {
            offset: Some(20),
            ..Default::default()
        };
        let sql = filter_sql(&f);
        assert!(sql.contains("LIMIT 18446744073709551615 OFFSET 20"), "{sql}");
        assert!(!sql.contains("LIMIT -1"), "{sql}");
    }

    #[test]
    fn limit_and_offset_are_literals_not_placeholders() {
        let f = IssueFilter {
            limit: Some(10),
            offset: Some(5),
            ..Default::default()
        };
        assert!(filter_sql(&f).contains("LIMIT 10 OFFSET 5"));
    }

    /// The `_bin` collation that makes `=` byte-exact also makes `LIKE`
    /// case-*sensitive*, where SQLite's is ASCII-case-insensitive. `IssueFilter`
    /// documents `text` as case-insensitive, so both sides get lowercased.
    #[test]
    fn the_text_search_stays_case_insensitive() {
        let f = IssueFilter {
            text: Some("Foo".to_string()),
            ..Default::default()
        };
        let sql = filter_sql(&f);
        assert!(sql.contains("LOWER(title) LIKE"), "{sql}");
        assert!(sql.contains("LOWER(description) LIKE"), "{sql}");
    }

    #[test]
    fn like_wildcards_in_the_search_text_are_escaped() {
        assert_eq!(escape_like("100%_x"), "100\\%\\_x");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
    }

    /// Backslash is an escape character inside a MySQL string literal and is not
    /// one inside a SQLite string literal. bd-sqlite's `ESCAPE '\\'` (Rust) emits
    /// the SQL `ESCAPE '\'` — fine for SQLite, and for MySQL an unterminated
    /// string whose backslash eats the closing quote. The SQL must carry two.
    #[test]
    fn the_like_escape_clause_survives_mysqls_string_literals() {
        let f = IssueFilter {
            text: Some("x".to_string()),
            ..Default::default()
        };
        let sql = filter_sql(&f);
        assert!(sql.contains(r"ESCAPE '\\'"), "{sql}");
        assert!(
            !sql.contains(r"ESCAPE '\'"),
            "MySQL would read this as an unterminated literal: {sql}"
        );
    }

    /// MySQL types `COUNT(*)` as BIGINT **UNSIGNED**, and sqlx's `i64` refuses an
    /// unsigned column outright — so a bare `COUNT(*)` fails at runtime, not at
    /// compile time, and takes `bd list`, `bd status` and `next_id` with it.
    /// Reading it as `u64` would fix MySQL and break Dolt, whose engine types the
    /// same aggregate as signed. Only the cast satisfies both.
    #[test]
    fn counts_are_cast_to_a_type_sqlx_will_decode() {
        let mut qb: QueryBuilder<MySql> =
            QueryBuilder::new(format!("SELECT {COUNT} FROM issues WHERE 1 = 1"));
        push_ready_predicates(&mut qb, false);
        let sql = qb.sql().to_string();
        assert!(sql.starts_with("SELECT CAST(COUNT(*) AS SIGNED)"), "{sql}");
    }

    /// MySQL's `JSON_TYPE` takes one argument. SQLite's takes a document *and* a
    /// path, and the ported two-argument call would simply not parse.
    #[test]
    fn the_metadata_key_probe_uses_json_extract_not_json_type() {
        let f = IssueFilter {
            has_metadata_key: Some("sprint".to_string()),
            ..Default::default()
        };
        let sql = filter_sql(&f);
        assert!(sql.contains("JSON_VALID(metadata)"), "{sql}");
        assert!(sql.contains("JSON_EXTRACT(metadata, CONCAT("), "{sql}");
        assert!(!sql.contains("JSON_TYPE"), "{sql}");
    }

    /// A dangling `?` would be bound to nothing and the query would fail at the
    /// server. Counting placeholders is the cheapest proof the builder is sane.
    #[test]
    fn every_clause_binds_exactly_the_placeholders_it_emits() {
        let f = IssueFilter {
            status: Some(Status::Open),
            statuses: vec![Status::Open, Status::InProgress],
            exclude_statuses: vec![Status::Closed],
            priority: Some(Priority(1)),
            min_priority: Some(Priority(0)),
            max_priority: Some(Priority(3)),
            issue_type: Some(IssueType::Bug),
            exclude_types: vec![IssueType::Epic],
            assignee: Some("alice".into()),
            owner: Some("bob".into()),
            labels_all: vec!["a".into(), "b".into()],
            labels_any: vec!["c".into()],
            parent: Some("bd-1".into()),
            spec_id: Some("s".into()),
            has_metadata_key: Some("k".into()),
            source_system: Some("jira".into()),
            external_ref: Some("PROJ-1".into()),
            created_after: Some(Utc::now()),
            created_before: Some(Utc::now()),
            updated_after: Some(Utc::now()),
            updated_before: Some(Utc::now()),
            closed_after: Some(Utc::now()),
            closed_before: Some(Utc::now()),
            text: Some("x".into()),
            pinned: Some(false),
            ephemeral: Some(false),
            is_template: Some(false),
            is_blocked: Some(false),
            lease_active: Some(false),
            ..Default::default()
        };
        // 1 status + 2 statuses + 1 exclude + 3 priorities + 1 type + 1 exclude
        // + assignee + owner + 2 labels_all + 1 labels_any + parent + spec
        // + source_system + external_ref + metadata key + 6 dates + 2 text
        // + 4 bools + 1 lease clock = 32.
        let sql = filter_sql(&f);
        assert_eq!(sql.matches('?').count(), 32, "{sql}");
    }

    /// `bd ready` is the reason this project exists, and `is_blocked = 0` is the
    /// term that makes it mean "claimable". No caller-supplied filter may relax it.
    #[test]
    fn ready_work_cannot_be_talked_out_of_its_own_predicates() {
        let mut qb: QueryBuilder<MySql> = QueryBuilder::new("SELECT id FROM issues WHERE 1 = 1");
        push_ready_predicates(&mut qb, false);
        let sql = qb.sql().to_string();
        assert!(sql.contains("is_blocked = ?"));
        assert!(sql.contains("pinned = 0 AND ephemeral = 0"));
        assert!(sql.contains("status IN ('open', 'in_progress')"));
        assert!(sql.contains("defer_until IS NULL"));
        assert!(sql.contains("lease_expires_at IS NULL"));
    }

    /// The blocked side must *not* hide deferred or leased work: a claimed bead
    /// that a new edge has since gated is exactly what `bd blocked` is for.
    #[test]
    fn the_blocked_side_does_not_inherit_the_ready_sides_clock_terms() {
        let mut qb: QueryBuilder<MySql> = QueryBuilder::new("SELECT id FROM issues WHERE 1 = 1");
        push_ready_predicates(&mut qb, true);
        let sql = qb.sql().to_string();
        assert!(!sql.contains("defer_until"));
        assert!(!sql.contains("lease_expires_at"));
    }

    // -- the fixpoint --------------------------------------------------------

    /// The rule that makes this backend's merges survivable. `is_blocked` is
    /// derived state; writing `updated_at` alongside it would plant local wall
    /// clock into a version-controlled table and conflict two clones on a column
    /// neither of them edited.
    #[test]
    fn nothing_in_the_recompute_touches_updated_at() {
        let ids = id_predicate(3);
        for sql in [mark_select_sql(&ids), unmark_select_sql(&ids)] {
            assert!(!sql.contains("updated_at"), "{sql}");
        }
        // And the write half, which is where it would actually have happened.
        let mut qb: QueryBuilder<MySql> = QueryBuilder::new("UPDATE issues SET is_blocked = ");
        qb.push_bind(true);
        qb.push(" WHERE id IN (?)");
        assert!(!qb.sql().contains("updated_at"));
    }

    /// MySQL raises error 1093 for an UPDATE whose WHERE selects from the table
    /// being updated — which is exactly bd-sqlite's mark/unmark shape. Here the
    /// ids are selected first and written second.
    #[test]
    fn the_recompute_never_updates_a_table_it_is_selecting_from() {
        for sql in [mark_select_sql("1 = 1"), unmark_select_sql("1 = 1")] {
            assert!(sql.trim_start().starts_with("SELECT i.id"), "{sql}");
            assert!(!sql.contains("UPDATE"), "{sql}");
        }
    }

    /// The one that would have been silent. MySQL's `JSON_EXTRACT` hands back a
    /// *JSON* string — `"any-children"`, quotes and all — so the ported
    /// comparison would be false for every gate, forever, with no error.
    #[test]
    fn the_waits_for_gate_unquotes_the_json_it_compares() {
        let sql = waits_for_gate_blocked();
        assert!(sql.contains("JSON_UNQUOTE(JSON_EXTRACT(d.metadata, '$.gate'))"), "{sql}");
        assert!(sql.contains("JSON_VALID(d.metadata)"), "{sql}");
        assert!(sql.contains("COALESCE("), "a NULL gate must not unblock the waiter");
    }

    /// All four rules, and both spellings of "pinned". A bead pinned as a status
    /// and a bead pinned as a flag must both stop gating their dependents.
    #[test]
    fn the_blocking_predicate_covers_every_edge_type_and_both_pins() {
        let p = blocking_predicate();
        for edge in ["'blocks'", "'conditional-blocks'", "'parent-child'", "'waits-for'"] {
            assert!(p.contains(edge), "missing {edge}");
        }
        assert!(p.contains("t.status <> 'pinned'") && p.contains("t.pinned = 0"));
        assert!(
            p.contains("t.close_is_failure = 0"),
            "a conditional-blocks edge releases only on a *failing* close"
        );
    }

    #[test]
    fn id_predicates_emit_one_placeholder_per_id() {
        assert_eq!(id_predicate(1), "i.id IN (?)");
        assert_eq!(id_predicate(3), "i.id IN (?, ?, ?)");
    }

    // -- dialect sweep -------------------------------------------------------

    /// A sweep over every statement this module can build, looking for the SQLite
    /// spellings that MySQL would reject or — worse — silently reinterpret.
    #[test]
    fn no_generated_statement_contains_a_sqlite_ism() {
        let mut sqls = vec![
            mark_select_sql("1 = 1"),
            unmark_select_sql("1 = 1"),
            blocking_predicate(),
            waits_for_gate_blocked(),
        ];
        for f in [
            IssueFilter::default(),
            IssueFilter::ready(),
            IssueFilter::blocked(),
            IssueFilter {
                parent: Some("bd-1".into()),
                has_metadata_key: Some("k".into()),
                text: Some("t".into()),
                sort: SortPolicy::Closed,
                offset: Some(1),
                ..Default::default()
            },
        ] {
            sqls.push(filter_sql(&f));
        }

        for sql in sqls {
            for bad in [
                "json_extract", // MySQL's returns a *quoted* JSON value
                "json_valid",   // lowercase spelling belongs to SQLite
                "json_type",
                "LIMIT -1",
                "AUTOINCREMENT",
                "excluded.",  // SQLite's ON CONFLICT alias
                "ON CONFLICT",
                "INSERT OR",
                "RETURNING",
                "datetime('now')",
            ] {
                assert!(!sql.contains(bad), "SQLite-ism `{bad}` in:\n{sql}");
            }
        }
    }

    /// `d.type` is fine in SQLite and fine in MySQL, but `key` is reserved in
    /// MySQL and `type` is not — so this is really a check that the *config*
    /// statements quote what has to be quoted. They are string literals in the
    /// `Storage` impl, so this reaches them through the schema they must agree with.
    #[test]
    fn the_dependency_column_list_quotes_the_type_column() {
        assert!(DEPENDENCY_COLUMNS.contains("`type`"));
    }

    // -- cycles --------------------------------------------------------------

    fn e(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn a_dag_has_no_cycles() {
        let edges = vec![e("a", "b"), e("b", "c"), e("a", "c")];
        assert!(detect_cycles(&edges).is_empty());
    }

    #[test]
    fn a_loop_is_found_and_reported_as_a_path() {
        let edges = vec![e("a", "b"), e("b", "c"), e("c", "a")];
        let cycles = detect_cycles(&edges);
        assert_eq!(cycles.len(), 1);
        let c = &cycles[0];
        assert_eq!(c.first(), c.last(), "a cycle must close on itself: {c:?}");
        assert_eq!(c.len(), 4);
    }

    #[test]
    fn self_loops_are_cycles_too() {
        let cycles = detect_cycles(&[e("a", "a")]);
        assert_eq!(cycles, vec![vec!["a".to_string(), "a".to_string()]]);
    }

    // -- misc ----------------------------------------------------------------

    #[test]
    fn a_datetime_literal_keeps_microseconds() {
        let t = DateTime::parse_from_rfc3339("2026-07-14T12:34:56.123456Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(sql_datetime(t), "2026-07-14 12:34:56.123456");
    }

    #[test]
    fn a_statement_splitter_ignores_semicolons_inside_quotes() {
        let sql = "CREATE TABLE t (a VARCHAR(1) DEFAULT 'x;y'); -- a; comment\nSELECT 1;";
        assert_eq!(
            schema_statements(sql),
            vec![
                "CREATE TABLE t (a VARCHAR(1) DEFAULT 'x;y')".to_string(),
                "SELECT 1".to_string()
            ]
        );
    }
}
