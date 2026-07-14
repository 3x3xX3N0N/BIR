//! The SQLite [`Storage`] implementation.

use async_trait::async_trait;
use bd_core::{
    Comment, Dependency, DependencyType, Event, EventType, Issue, IssueFilter, Status, idgen,
};
use bd_storage::{Backend, Claim, Error, Field, Identity, IssuePatch, Result, Stats, Storage};
use chrono::{DateTime, Duration, Utc};
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool};
use std::collections::{HashMap, HashSet};

use crate::blocked;
use crate::rows::{
    ISSUE_COLUMNS, comment_from_row, dependency_from_row, enum_to_str, event_from_row,
    issue_from_row, metadata_to_text, none_if_empty,
};
use crate::sqlfilter::{push_filter, push_order_and_limit, push_ready_predicates};

/// Edge types that define an ordering between beads, and therefore the ones a
/// cycle in would be a real contradiction ("A must finish before B, which must
/// finish before A"). `conditional-blocks` and `waits-for` are excluded on
/// purpose: they cannot destabilize the `is_blocked` fixpoint, because neither
/// reads its target's `is_blocked` — only its status.
const ORDERING_EDGES: [&str; 2] = ["blocks", "parent-child"];

pub struct SqliteStore {
    pool: SqlitePool,
    identity: Identity,
}

impl SqliteStore {
    pub(crate) fn new(pool: SqlitePool, identity: Identity) -> Self {
        SqliteStore { pool, identity }
    }
}

#[async_trait]
impl Storage for SqliteStore {
    fn backend(&self) -> Backend {
        Backend::Sqlite
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
    /// operations in their own right. Use `add_dependency` / `add_comment`.
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
            sqlx::query("INSERT OR IGNORE INTO labels (issue_id, label) VALUES (?, ?)")
                .bind(&row.id)
                .bind(label)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }

        self.event(&mut tx, &row.id, EventType::Created, None, Some(&row.title), now)
            .await?;

        // A brand-new bead has no edges yet, so this can only ever flip the bead
        // itself -- but running it keeps "every write path recomputes" true
        // without exception, and exceptions are how this cache goes stale.
        blocked::recompute_affected(&mut tx, &[row.id.clone()]).await?;

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

        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
            "SELECT {ISSUE_COLUMNS} FROM issues WHERE id IN ("
        ));
        let mut sep = qb.separated(", ");
        for id in ids {
            sep.push_bind(id.clone());
        }
        qb.push(")");

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;
        let mut by_id: HashMap<String, Issue> = rows
            .iter()
            .map(|r| issue_from_row(r).map(|i| (i.id.clone(), i)))
            .collect::<Result<_>>()?;

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
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("UPDATE issues SET updated_at = ");
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
        blocked::recompute_affected(&mut tx, &[id.to_string()]).await?;

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
        // very edges that tell us who cared about this bead.
        let affected = blocked::affected_set(&mut tx, &[id.to_string()]).await?;

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

        blocked::fixpoint(&mut tx, &affected).await?;

        tx.commit().await.map_err(db)
    }

    /// Relations are left empty. Hydrating labels, edges and comments per row
    /// would be one query per issue; `get_issue` is where you go for the whole
    /// bead.
    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>> {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new(format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE 1 = 1"));
        push_filter(&mut qb, filter);
        push_order_and_limit(&mut qb, filter);

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(issue_from_row).collect()
    }

    async fn count_issues(&self, filter: &IssueFilter) -> Result<u64> {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT COUNT(*) FROM issues WHERE 1 = 1");
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
        // cache of its answer so the SQL fixpoint can read it.
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
        blocked::recompute_affected(&mut tx, &[id.to_string()]).await?;
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
        blocked::recompute_affected(&mut tx, &[id.to_string()]).await?;
        tx.commit().await.map_err(db)?;

        self.get_issue(id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn next_id(&self, prefix: &str, title: &str, description: &str) -> Result<String> {
        let mut conn = self.pool.acquire().await.map_err(db)?;

        let count: i64 = sqlx::query("SELECT COUNT(*) FROM issues")
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
    /// With a single `assignee` column the claim is then advisory — the column
    /// records the most recent claimant, and nothing is fenced.
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
        blocked::recompute_affected(&mut tx, &[id.to_string()]).await?;
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
    /// claimed by ''" — a message that describes a race with a nameless agent
    /// when what actually happened is that you never claimed the issue.
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
        blocked::recompute_affected(&mut tx, &[id.to_string()]).await?;
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

        blocked::recompute_affected(&mut tx, &freed).await?;
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

        sqlx::query(
            "INSERT OR REPLACE INTO dependencies
                 (issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
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

        blocked::recompute_affected(&mut tx, std::slice::from_ref(&dep.issue_id)).await?;
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
            blocked::affected_set(&mut tx, &[issue_id.to_string(), depends_on_id.to_string()])
                .await?;

        let removed = sqlx::query(
            "DELETE FROM dependencies
             WHERE issue_id = ? AND depends_on_id = ? AND type = ?",
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

        blocked::fixpoint(&mut tx, &affected).await?;
        tx.commit().await.map_err(db)
    }

    async fn dependencies_of(&self, id: &str) -> Result<Vec<Dependency>> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        fetch_dependencies_of(&mut conn, id).await
    }

    async fn dependents_of(&self, id: &str) -> Result<Vec<Dependency>> {
        let rows = sqlx::query(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies WHERE depends_on_id = ? ORDER BY issue_id, type",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(dependency_from_row).collect()
    }

    async fn list_dependencies(&self, filter: &IssueFilter) -> Result<Vec<Dependency>> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies",
        );

        // An empty filter means every edge *in the table* — deliberately not
        // "every edge whose source is an issue that exists". The two differ only
        // on a corrupt graph, and a corrupt graph is the entire reason
        // `bd lint` and `bd graph check` ask this question.
        if !filter.is_empty() {
            qb.push(" WHERE issue_id IN (SELECT id FROM issues WHERE 1 = 1");
            push_filter(&mut qb, filter);
            qb.push(")");
        }
        qb.push(" ORDER BY issue_id, depends_on_id, type");

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(dependency_from_row).collect()
    }

    async fn dependencies_of_many(&self, ids: &[String]) -> Result<Vec<(String, Vec<Dependency>)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies WHERE issue_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in ids {
            sep.push_bind(id.clone());
        }
        qb.push(") ORDER BY issue_id, depends_on_id, type");

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;

        let mut grouped: HashMap<String, Vec<Dependency>> = HashMap::new();
        for row in rows.iter() {
            let dep = dependency_from_row(row)?;
            grouped.entry(dep.issue_id.clone()).or_default().push(dep);
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
        sqlx::query("INSERT OR IGNORE INTO labels (issue_id, label) VALUES (?, ?)")
            .bind(issue_id)
            .bind(label)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
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

    // -- comments and audit trail --------------------------------------------

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

        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT issue_id, label FROM labels WHERE issue_id IN (");
        let mut sep = qb.separated(", ");
        for id in ids {
            sep.push_bind(id.clone());
        }
        qb.push(") ORDER BY issue_id, label");

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;

        let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let issue_id: String = row.try_get("issue_id").map_err(db)?;
            let label: String = row.try_get("label").map_err(db)?;
            grouped.entry(issue_id).or_default().push(label);
        }

        // Return in the caller's order so a listing can zip this against its
        // rows without re-sorting.
        Ok(ids
            .iter()
            .filter_map(|id| grouped.remove(id).map(|labels| (id.clone(), labels)))
            .collect())
    }

    /// Insert or update a comment, preserving its id and author.
    ///
    /// This is the idempotent half of the pair. `add_comment` mints a new id and
    /// stamps the caller as author; re-running an import through it would
    /// duplicate every comment and reattribute all of them to the importer.
    ///
    /// The incoming id is the identity, and it is honored *whatever it says* —
    /// including an integer id from an older export, which is why nothing here
    /// parses or validates its shape. Ids minted from now on are UUIDs (see
    /// `schema.sql`), so a collision between two workspaces is not a thing that
    /// happens rather than a thing that is caught.
    async fn upsert_comment(&self, comment: &Comment) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;

        if fetch_issue(&mut tx, &comment.issue_id).await?.is_none() {
            return Err(Error::NotFound(comment.issue_id.clone()));
        }

        sqlx::query(
            "INSERT INTO comments (id, issue_id, author, text, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 issue_id   = excluded.issue_id,
                 author     = excluded.author,
                 text       = excluded.text,
                 created_at = excluded.created_at",
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

        // Globally unique, not workspace-local. `upsert_comment` treats the id
        // as the comment's identity, so an id that only means something inside
        // one database turns `bd import` into a way to overwrite the importer's
        // own comments.
        let id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO comments (id, issue_id, author, text, created_at)
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

        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT id, issue_id, author, text, created_at FROM comments WHERE issue_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in ids {
            sep.push_bind(id.clone());
        }
        // By time, never by id: comment ids are UUIDs and sort meaninglessly, so
        // ordering on them would shuffle a conversation.
        qb.push(") ORDER BY issue_id, created_at, id");

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;

        let mut grouped: HashMap<String, Vec<Comment>> = HashMap::new();
        for row in rows.iter() {
            let c = comment_from_row(row)?;
            grouped.entry(c.issue_id.clone()).or_default().push(c);
        }
        Ok(ids
            .iter()
            .filter_map(|id| grouped.remove(id).map(|cs| (id.clone(), cs)))
            .collect())
    }

    async fn list_events(&self, issue_id: &str) -> Result<Vec<Event>> {
        let rows = sqlx::query(
            "SELECT id, issue_id, event_type, actor, old_value, new_value, created_at
             FROM events WHERE issue_id = ? ORDER BY id",
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

    async fn recompute_blocked(&self) -> Result<u64> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let changed = blocked::recompute_all(&mut tx).await?;
        tx.commit().await.map_err(db)?;
        Ok(changed)
    }

    // -- config --------------------------------------------------------------

    async fn get_config(&self, key: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT value FROM config WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO config (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn list_config(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT key, value FROM config ORDER BY key")
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
        let mut s = Stats {
            total: count(&self.pool, "SELECT COUNT(*) FROM issues", &[]).await?,
            open: count(
                &self.pool,
                "SELECT COUNT(*) FROM issues WHERE status = ?",
                &["open"],
            )
            .await?,
            in_progress: count(
                &self.pool,
                "SELECT COUNT(*) FROM issues WHERE status = ?",
                &["in_progress"],
            )
            .await?,
            closed: count(
                &self.pool,
                "SELECT COUNT(*) FROM issues WHERE status = ?",
                &["closed"],
            )
            .await?,
            ..Default::default()
        };

        s.blocked = self.count_work(true).await?;
        s.ready = self.count_work(false).await?;

        for r in sqlx::query("SELECT priority, COUNT(*) AS n FROM issues GROUP BY priority")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
        {
            s.by_priority
                .insert(r.get::<i32, _>("priority"), r.get::<i64, _>("n") as u64);
        }
        for r in sqlx::query("SELECT issue_type, COUNT(*) AS n FROM issues GROUP BY issue_type")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
        {
            s.by_type
                .insert(r.get::<String, _>("issue_type"), r.get::<i64, _>("n") as u64);
        }
        Ok(s)
    }

    async fn close(&self) -> Result<()> {
        self.pool.close().await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers on the store
// ---------------------------------------------------------------------------

impl SqliteStore {
    async fn work(&self, filter: &IssueFilter, blocked_side: bool) -> Result<Vec<Issue>> {
        // The ready predicates are ANDed with the caller's filter, so the caller
        // can only ever narrow the result — `bd ready --assignee me` works,
        // `bd ready --include-blocked` cannot exist. The one field dropped is
        // `is_blocked`: `IssueFilter::ready()` pins it to `false`, and handing
        // that same filter to `blocked_work` would otherwise AND `is_blocked = 0`
        // with `is_blocked = 1` and silently return nothing.
        let mut f = filter.clone();
        f.is_blocked = None;

        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new(format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE 1 = 1"));
        push_ready_predicates(&mut qb, blocked_side);
        push_filter(&mut qb, &f);
        push_order_and_limit(&mut qb, filter);

        let rows = qb.build().fetch_all(&self.pool).await.map_err(db)?;
        rows.iter().map(issue_from_row).collect()
    }

    async fn count_work(&self, blocked_side: bool) -> Result<u64> {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT COUNT(*) FROM issues WHERE 1 = 1");
        push_ready_predicates(&mut qb, blocked_side);
        let row = qb.build().fetch_one(&self.pool).await.map_err(db)?;
        Ok(row.get::<i64, _>(0) as u64)
    }

    /// Events go in the *same transaction* as the mutation that earned them. An
    /// event written after the commit is an event that a crash can lose, leaving
    /// an audit trail that disagrees with the data it audits.
    async fn event(
        &self,
        conn: &mut SqliteConnection,
        issue_id: &str,
        kind: EventType,
        old: Option<&str>,
        new: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let kind = enum_to_str(&kind)
            .ok_or_else(|| Error::Db("event type is not a string".to_string()))?;
        sqlx::query(
            "INSERT INTO events (issue_id, event_type, actor, old_value, new_value, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
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
        conn: &mut SqliteConnection,
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
// Free helpers
// ---------------------------------------------------------------------------

async fn insert_issue(conn: &mut SqliteConnection, i: &Issue) -> Result<()> {
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

/// Push a `SET col = …` clause for a text column whose domain type is `String`.
///
/// Clearing one of these means the empty string, not NULL: `Issue.notes` is a
/// `String`, so a NULL would only be read back as `""` anyway, and storing one
/// would just give the same value two spellings for a reader to trip over.
///
/// `col` is always a literal from this module, never user input.
fn push_text(qb: &mut QueryBuilder<'_, Sqlite>, col: &str, field: &Field<String>) {
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

async fn fetch_issue(conn: &mut SqliteConnection, id: &str) -> Result<Option<Issue>> {
    let row = sqlx::query(&format!(
        "SELECT {ISSUE_COLUMNS} FROM issues WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db)?;
    row.as_ref().map(issue_from_row).transpose()
}

async fn fetch_labels(conn: &mut SqliteConnection, id: &str) -> Result<Vec<String>> {
    sqlx::query_scalar("SELECT label FROM labels WHERE issue_id = ? ORDER BY label")
        .bind(id)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)
}

async fn fetch_dependencies_of(conn: &mut SqliteConnection, id: &str) -> Result<Vec<Dependency>> {
    let rows = sqlx::query(
        "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
         FROM dependencies WHERE issue_id = ? ORDER BY depends_on_id, type",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    rows.iter().map(dependency_from_row).collect()
}

/// A comment thread is ordered by *time*. Ordering by id worked only while ids
/// were a monotonic integer; a UUID sorts at random, and a shuffled thread reads
/// as a different conversation.
async fn fetch_comments(conn: &mut SqliteConnection, id: &str) -> Result<Vec<Comment>> {
    let rows = sqlx::query(
        "SELECT id, issue_id, author, text, created_at FROM comments
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
async fn refresh_content_hash(conn: &mut SqliteConnection, id: &str) -> Result<()> {
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

async fn count(pool: &SqlitePool, sql: &str, binds: &[&str]) -> Result<u64> {
    let mut q = sqlx::query(sql);
    for b in binds {
        q = q.bind(*b);
    }
    let row = q.fetch_one(pool).await.map_err(db)?;
    Ok(row.get::<i64, _>(0) as u64)
}

async fn load_ordering_edges(pool: &SqlitePool) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT issue_id, depends_on_id FROM dependencies
         WHERE type IN ('blocks', 'parent-child')",
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
    conn: &mut SqliteConnection,
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
             WHERE issue_id = ? AND type IN ('blocks', 'parent-child')",
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

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

#[cfg(test)]
mod cycle_tests {
    use super::detect_cycles;

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
}
