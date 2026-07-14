//! `IssueFilter` -> SQL.
//!
//! Everything an [`IssueFilter`] can express is pushed down into the database.
//! Nothing is filtered in Rust after the fact: a `LIMIT` applied to a set that
//! was then filtered in memory returns the wrong page, and that bug is invisible
//! until someone notices `bd list --limit 10` showing four rows.

use bd_core::{IssueFilter, SortPolicy};
use chrono::Utc;
use sqlx::{QueryBuilder, Sqlite};

/// Types that are never claimable work. Infrastructure beads (molecules, gates,
/// events, messages) are bookkeeping, not tasks.
pub const READY_EXCLUDED_TYPES: [&str; 4] = ["molecule", "gate", "event", "message"];

/// Append the filter's clauses. The caller has already emitted a `WHERE` and at
/// least one predicate, so every clause here starts with `AND`.
pub fn push_filter(qb: &mut QueryBuilder<'_, Sqlite>, f: &IssueFilter) {
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
                    WHERE type = 'parent-child' AND depends_on_id = ",
        )
        .push_bind(parent.clone())
        .push(
            "
                    UNION
                    SELECT d.issue_id FROM dependencies d
                    JOIN descendants ON d.depends_on_id = descendants.id
                    WHERE d.type = 'parent-child'
                )
                SELECT id FROM descendants
            )",
        );
    }

    if let Some(spec) = &f.spec_id {
        qb.push(" AND spec_id = ").push_bind(spec.clone());
    }
    if let Some(key) = &f.has_metadata_key {
        qb.push(
            " AND (CASE WHEN json_valid(metadata) \
             THEN json_type(metadata, '$.\"' || ",
        )
        .push_bind(key.clone())
        .push(" || '\"') END) IS NOT NULL");
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
        qb.push(" AND closed_at IS NOT NULL AND closed_at > ").push_bind(t);
    }
    if let Some(t) = f.closed_before {
        qb.push(" AND closed_at IS NOT NULL AND closed_at < ").push_bind(t);
    }

    if let Some(text) = &f.text {
        let pat = format!("%{}%", escape_like(text));
        qb.push(" AND (title LIKE ")
            .push_bind(pat.clone())
            .push(" ESCAPE '\\' OR description LIKE ")
            .push_bind(pat)
            .push(" ESCAPE '\\')");
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
}

/// The ready-work predicates, which no caller-supplied filter may relax.
///
/// `bd ready` means "claimable *right now*". A filter that could switch off the
/// `is_blocked = 0` term would turn `bd ready` into `bd list` with extra steps.
pub fn push_ready_predicates(qb: &mut QueryBuilder<'_, Sqlite>, blocked: bool) {
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
    }
}

/// ORDER BY for a sort policy, plus LIMIT/OFFSET.
///
/// The hybrid policy is the default and the interesting one: work created inside
/// the last 48h is ranked by priority, and work older than that is ranked by age.
/// A pure priority sort starves old P3s forever; a pure age sort buries a P0
/// filed this morning behind a year of backlog.
pub fn push_order_and_limit(qb: &mut QueryBuilder<'_, Sqlite>, f: &IssueFilter) {
    match f.sort {
        SortPolicy::Hybrid => {
            let cutoff = Utc::now() - SortPolicy::HYBRID_RECENCY_WINDOW;
            qb.push(" ORDER BY CASE WHEN created_at >= ")
                .push_bind(cutoff)
                .push(" THEN 0 ELSE 1 END ASC, CASE WHEN created_at >= ")
                .push_bind(cutoff)
                .push(" THEN priority ELSE 999 END ASC, created_at ASC, id ASC");
        }
        SortPolicy::Priority => {
            qb.push(" ORDER BY priority ASC, created_at ASC, id ASC");
        }
        SortPolicy::Oldest => {
            qb.push(" ORDER BY created_at ASC, id ASC");
        }
    }

    if let Some(n) = f.limit {
        qb.push(" LIMIT ").push_bind(n as i64);
        if let Some(o) = f.offset {
            qb.push(" OFFSET ").push_bind(o as i64);
        }
    } else if let Some(o) = f.offset {
        // SQLite refuses OFFSET without LIMIT; -1 is its idiom for "no limit".
        qb.push(" LIMIT -1 OFFSET ").push_bind(o as i64);
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
