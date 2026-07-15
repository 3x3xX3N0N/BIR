//! The `is_blocked` fixpoint engine.
//!
//! `bd ready` does not traverse the dependency graph. It filters on the
//! `issues.is_blocked` column, which is a *cache* of the graph maintained by
//! this module. Everything here exists to keep that cache honest, because a
//! stale `is_blocked` does not make `bd ready` slow — it makes `bd ready` lie,
//! and an agent that is lied to about what is claimable will happily work on a
//! bead whose blocker is still open.
//!
//! # The rule
//!
//! An issue is blocked iff it is itself neither closed nor pinned, **and** any
//! of:
//!
//! 1. it has a `blocks` or `conditional-blocks` edge to a target that is still
//!    live (see [`conditional-blocks`](#conditional-blocks) for the wrinkle);
//! 2. it has a `parent-child` edge to a parent that is itself blocked — that
//!    is, blocked-ness propagates *down* the containment tree;
//! 3. it has a `waits-for` edge to a spawner whose gate is unsatisfied.
//!
//! # Why a fixpoint, and not one pass
//!
//! Rule 2 is transitive. Consider `A blocks B`, `C` a child of `B`, `D` a child
//! of `C`. Closing `A` unblocks `B`; only *then* does `C` see an unblocked
//! parent; only then does `D`. A single mark/unmark pass propagates exactly one
//! level per statement, and which level it happens to catch depends on the order
//! SQLite visits rows in — so one pass leaves `C` and `D` wrongly blocked, and
//! does so *nondeterministically*. Iterating to a fixpoint is not an
//! optimization; it is the algorithm.
//!
//! # <a name="conditional-blocks"></a>`conditional-blocks`
//!
//! `B conditional-blocks A` means "run B only if A **fails**". So B is blocked
//! while A is open, and when A closes B becomes ready only if A's close reason
//! reads as a failure ([`bd_core::is_failure_close`]). If A closed successfully,
//! the failure path is moot and B stays blocked forever.
//!
//! Leaving it blocked (rather than auto-closing it) is a deliberate choice: a
//! store that silently closes beads the user did not ask it to close is worse
//! than one that leaves a visibly-stuck bead for a human to reap. `bd blocked`
//! will show it.

use bd_storage::{Error, Result};
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};
use std::collections::HashSet;

/// SQLite's default host-parameter ceiling is 999. Stay well under it.
const CHUNK: usize = 400;

/// A parent-child cycle would make the fixpoint oscillate forever. Cycles are
/// rejected at `add_dependency`, so hitting this means the graph was corrupted
/// behind our back (an import, a merge) and spinning is the wrong answer.
const MAX_ITERATIONS: usize = 100;

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
            WHERE d.issue_id = i.id AND d.type = 'blocks'
              AND {LIVE}
        )
        OR EXISTS (
            SELECT 1 FROM dependencies d JOIN issues t ON t.id = d.depends_on_id
            WHERE d.issue_id = i.id AND d.type = 'conditional-blocks'
              AND (
                    {LIVE}
                    OR (t.status = 'closed' AND t.close_is_failure = 0)
              )
        )
        OR EXISTS (
            SELECT 1 FROM dependencies d JOIN issues t ON t.id = d.depends_on_id
            WHERE d.issue_id = i.id AND d.type = 'parent-child'
              AND t.is_blocked = 1
        )
        OR EXISTS (
            SELECT 1 FROM dependencies d
            WHERE d.issue_id = i.id AND d.type = 'waits-for'
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
/// `json_extract` raises a hard error on malformed input, which inside an UPDATE
/// would abort the whole recompute — hence the `json_valid` guard, and the
/// `COALESCE`, without which a NULL would poison the surrounding `NOT (...)`
/// into NULL and silently unblock the waiter.
fn waits_for_gate_blocked() -> String {
    format!(
        r#"(
            EXISTS (
                SELECT 1 FROM dependencies cd JOIN issues t ON t.id = cd.issue_id
                WHERE cd.type = 'parent-child' AND cd.depends_on_id = d.depends_on_id
                  AND {LIVE}
            )
            AND NOT (
                COALESCE(
                    CASE WHEN json_valid(d.metadata)
                         THEN json_extract(d.metadata, '$.gate') END,
                    ''
                ) = 'any-children'
                AND EXISTS (
                    SELECT 1 FROM dependencies cd JOIN issues t ON t.id = cd.issue_id
                    WHERE cd.type = 'parent-child' AND cd.depends_on_id = d.depends_on_id
                      AND t.status = 'closed'
                )
            )
        )"#
    )
}

// ---------------------------------------------------------------------------
// The two UPDATE statements
//
// Neither touches `updated_at`, and that omission is load-bearing.
//
// `is_blocked` is DERIVED state. Bumping `updated_at` when it flips would stamp
// the local machine's wall clock onto a row in a version-controlled table, for a
// change the user never made. Two clones that recompute the same flip a second
// apart would then disagree on `updated_at` and hand the merge a conflict on a
// column neither of them edited. Worse, stale-guard and conflict-guard consumers
// read `updated_at` to mean "a human touched this".
//
// Upstream has to write `SET ..., updated_at = updated_at` because MySQL columns
// carry ON UPDATE CURRENT_TIMESTAMP. SQLite has no such clause, so the
// protection here is simply never adding `updated_at` to these SET lists. It
// looks like an oversight. It is not. Do not "fix" it.
// ---------------------------------------------------------------------------

fn mark_sql(where_ids: &str) -> String {
    format!(
        r#"UPDATE issues AS i SET is_blocked = 1
           WHERE {where_ids}
             AND i.is_blocked = 0
             AND i.status <> 'closed' AND i.status <> 'pinned' AND i.pinned = 0
             AND ({pred})"#,
        pred = blocking_predicate()
    )
}

fn unmark_sql(where_ids: &str) -> String {
    format!(
        r#"UPDATE issues AS i SET is_blocked = 0
           WHERE {where_ids}
             AND i.is_blocked = 1
             AND (
                   i.status = 'closed' OR i.status = 'pinned' OR i.pinned = 1
                   OR NOT ({pred})
             )"#,
        pred = blocking_predicate()
    )
}

/// Recompute `is_blocked` for every issue in the table, to a fixpoint.
///
/// Required after anything that changes rows behind the store's back — an
/// import, a merge, a pull. The incremental path cannot help there: it seeds
/// from the ids a write path touched, and a merge touched ids no write path saw.
///
/// Also refreshes the derived `close_is_failure` column first, for the same
/// reason: rows that arrived without going through `close_issue` never had it
/// computed.
///
/// Returns the number of rows whose `is_blocked` actually flipped.
///
/// # A least fixpoint, and why that matters for cycles
///
/// This resets every row to *unblocked* and then only ever marks — it never
/// unmarks. That is the whole trick. The incremental path ([`recompute_affected`])
/// repairs toward the truth from the current stored value with both a mark and an
/// unmark pass, which is right when the graph is already mostly correct and
/// acyclic. But a full recompute runs after a **merge or import**, which is
/// exactly when a `parent-child` *cycle* can arrive (write paths reject cycles;
/// a merge does not go through a write path).
///
/// On a cycle the mark+unmark repair does not converge to the truth — it settles
/// into a *stable but wrong* state: two mutually-parented issues each read the
/// other's stored `is_blocked = 1` through the parent-child rule, so neither
/// unmark fires, and the pair stays blocked forever even after the real blocker
/// closes. mark+unmark returns `Ok` with a lying cache.
///
/// Marking from an all-unblocked base cannot do that. A cyclic pair with no open
/// external blocker starts unmarked, has no direct blocker, and sees an unmarked
/// parent — so nothing marks it, correctly. A cyclic pair that *does* have an
/// external blocker gets marked through the direct edge and propagates. And
/// because marking is monotonic (`0 → 1` only, never back), it always converges,
/// in at most the graph's depth in `parent-child` edges.
pub async fn recompute_all(conn: &mut SqliteConnection) -> Result<u64> {
    refresh_close_is_failure(conn).await?;

    // Which rows were blocked before, so we can report only the true flips — the
    // reset-then-remark below touches rows that end up unchanged, and callers
    // (e.g. `bd recompute-blocked`, whose test asserts `updated: 0` on an already
    // correct cache) need the net change, not the churn.
    let before = blocked_ids(conn).await?;

    // Reset to the all-unblocked base. Only currently-blocked rows are touched,
    // and — per the note above these functions — `updated_at` is deliberately
    // never in the SET list, so this does not stamp wall-clock onto a
    // version-controlled column.
    sqlx::query("UPDATE issues SET is_blocked = 0 WHERE is_blocked = 1")
        .execute(&mut *conn)
        .await
        .map_err(db)?;

    let mark = mark_sql("1 = 1");
    let mut converged = false;
    for _ in 0..MAX_ITERATIONS {
        let changed = sqlx::query(&mark)
            .execute(&mut *conn)
            .await
            .map_err(db)?
            .rows_affected();
        if changed == 0 {
            converged = true;
            break;
        }
    }
    // Monotonic marking converges in at most the parent-child depth, so blowing
    // the cap means a graph deeper than MAX_ITERATIONS levels — vanishingly
    // unlikely, but a runaway is worth an honest error rather than a wrong cache.
    if !converged {
        return Err(not_converged());
    }

    let after = blocked_ids(conn).await?;
    Ok(before.symmetric_difference(&after).count() as u64)
}

/// The ids currently marked blocked. Used only to count true flips.
async fn blocked_ids(conn: &mut SqliteConnection) -> Result<HashSet<String>> {
    let rows = sqlx::query("SELECT id FROM issues WHERE is_blocked = 1")
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;
    rows.iter().map(|r| r.try_get::<String, _>("id").map_err(db)).collect()
}

/// Recompute `is_blocked` for everything a change to `seed_ids` could possibly
/// affect, to a fixpoint. This is what write paths call, inside their own
/// transaction.
pub async fn recompute_affected(conn: &mut SqliteConnection, seed_ids: &[String]) -> Result<u64> {
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
///   *children*, so a child changing status moves a gate the child has no edge
///   to;
/// * **parent-child descendants**, transitively — rule 2. Expanded by BFS from
///   the whole seed set (dependers included), because a depender that flips must
///   in turn push the flip down its own subtree.
///
/// Callers that are about to *delete* rows must call this first: the edges it
/// walks are the very edges the delete will cascade away.
pub async fn affected_set(conn: &mut SqliteConnection, seed_ids: &[String]) -> Result<Vec<String>> {
    let mut seen: HashSet<String> = seed_ids.iter().cloned().collect();
    let mut queue: Vec<String> = seed_ids.to_vec();

    for chunk in seed_ids.chunks(CHUNK) {
        for id in select_ids(
            conn,
            "SELECT issue_id FROM dependencies
             WHERE type IN ('blocks', 'conditional-blocks', 'waits-for')
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
             WHERE w.type = 'waits-for'
               AND w.depends_on_id IN (
                   SELECT pc.depends_on_id FROM dependencies pc
                   WHERE pc.type = 'parent-child' AND pc.issue_id IN ",
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
             WHERE type = 'parent-child' AND depends_on_id IN ",
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
pub async fn fixpoint(conn: &mut SqliteConnection, ids: &[String]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }

    let mut total = 0u64;
    for _ in 0..MAX_ITERATIONS {
        let mut changed = 0u64;

        for chunk in ids.chunks(CHUNK) {
            let ph = placeholders(chunk.len());
            let where_ids = format!("i.id IN ({ph})");

            for sql in [mark_sql(&where_ids), unmark_sql(&where_ids)] {
                let mut q = sqlx::query(&sql);
                for id in chunk {
                    q = q.bind(id);
                }
                changed += q.execute(&mut *conn).await.map_err(db)?.rows_affected();
            }
        }

        total += changed;
        if changed == 0 {
            return Ok(total);
        }
    }
    Err(not_converged())
}

/// Exactly one mark/unmark pass. Exists so that a test can demonstrate that one
/// pass is *not enough* — see `one_pass_leaves_the_deep_end_of_the_chain_wrong`.
#[cfg(test)]
async fn one_pass(conn: &mut SqliteConnection, ids: &[String]) -> Result<u64> {
    let ph = placeholders(ids.len());
    let where_ids = format!("i.id IN ({ph})");
    let mut changed = 0u64;
    for sql in [mark_sql(&where_ids), unmark_sql(&where_ids)] {
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        changed += q.execute(&mut *conn).await.map_err(db)?.rows_affected();
    }
    Ok(changed)
}

/// Recompute the derived `close_is_failure` column from `close_reason`, using
/// bd-core as the sole authority on what "failure" reads like.
async fn refresh_close_is_failure(conn: &mut SqliteConnection) -> Result<()> {
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
    conn: &mut SqliteConnection,
    prefix: &str,
    ids: &[String],
    suffix: &str,
) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(prefix);
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

fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(", ")
}

fn not_converged() -> Error {
    Error::Db(format!(
        "is_blocked did not converge after {MAX_ITERATIONS} passes; \
         the dependency graph almost certainly contains a cycle (try `bd dep cycles`)"
    ))
}

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(crate::SCHEMA).execute(&pool).await.unwrap();
        pool
    }

    async fn issue(pool: &SqlitePool, id: &str) {
        sqlx::query("INSERT INTO issues (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(id)
            .bind(Utc::now())
            .bind(Utc::now())
            .execute(pool)
            .await
            .unwrap();
    }

    async fn edge(pool: &SqlitePool, from: &str, to: &str, ty: &str) {
        sqlx::query(
            "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(from)
        .bind(to)
        .bind(ty)
        .bind(Utc::now())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn is_blocked(pool: &SqlitePool, id: &str) -> bool {
        sqlx::query_scalar("SELECT is_blocked FROM issues WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Rows are named so that both the id index and the rowid order visit the
    /// *deepest descendant first* — the adverse order. That is not a trick to
    /// make the test fail; it is the order a real workspace lands in whenever a
    /// child is filed before its parent, which is most of the time.
    ///
    /// `bd-e` blocks `bd-d`; `bd-c` is a child of `bd-d`; `bd-b` a child of
    /// `bd-c`; `bd-a` a child of `bd-b`.
    async fn chain() -> SqlitePool {
        let pool = pool().await;
        for id in ["bd-a", "bd-b", "bd-c", "bd-d", "bd-e"] {
            issue(&pool, id).await;
        }
        edge(&pool, "bd-d", "bd-e", "blocks").await;
        edge(&pool, "bd-c", "bd-d", "parent-child").await;
        edge(&pool, "bd-b", "bd-c", "parent-child").await;
        edge(&pool, "bd-a", "bd-b", "parent-child").await;

        let mut conn = pool.acquire().await.unwrap();
        recompute_all(&mut conn).await.unwrap();
        drop(conn);

        for id in ["bd-b", "bd-c", "bd-d"] {
            assert!(is_blocked(&pool, id).await, "{id} should start blocked");
        }
        pool
    }

    /// The test that justifies this whole module.
    ///
    /// Closing `bd-e` frees the entire chain, but only a fixpoint discovers
    /// that. One mark/unmark pass propagates the unblock exactly one level —
    /// `bd-d` learns it is free, and `bd-c` has already been visited by then, so
    /// it and everything under it stay wrongly blocked. A `bd ready` built on a
    /// single pass would silently hide three claimable beads.
    #[tokio::test]
    async fn one_pass_leaves_the_deep_end_of_the_chain_wrong() {
        let pool = chain().await;
        let all: Vec<String> = ["bd-a", "bd-b", "bd-c", "bd-d", "bd-e"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        sqlx::query("UPDATE issues SET status = 'closed' WHERE id = 'bd-e'")
            .execute(&pool)
            .await
            .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        one_pass(&mut conn, &all).await.unwrap();
        drop(conn);

        assert!(
            !is_blocked(&pool, "bd-d").await,
            "one pass should at least free the direct depender"
        );
        assert!(
            is_blocked(&pool, "bd-c").await && is_blocked(&pool, "bd-b").await,
            "if one pass already freed the whole chain this test proves nothing; \
             the propagation order is no longer adverse"
        );

        // Now the real thing.
        let mut conn = pool.acquire().await.unwrap();
        let changed = recompute_affected(&mut conn, &["bd-e".to_string()])
            .await
            .unwrap();
        drop(conn);

        assert!(changed > 0);
        for id in ["bd-a", "bd-b", "bd-c", "bd-d"] {
            assert!(!is_blocked(&pool, id).await, "{id} still blocked after fixpoint");
        }
    }

    /// Blocked-ness must also propagate *down* to a fixpoint when it arrives,
    /// not only when it lifts.
    #[tokio::test]
    async fn blocking_propagates_down_the_whole_subtree() {
        let pool = chain().await;
        for id in ["bd-a", "bd-b", "bd-c", "bd-d"] {
            assert!(is_blocked(&pool, id).await, "{id} should be blocked");
        }
        assert!(!is_blocked(&pool, "bd-e").await);
    }

    /// A parent-child cycle cannot be created through `add_dependency`, but a
    /// merge or an import can land one — and that is exactly when `recompute_all`
    /// runs. It must not spin, and — the part the old mark+unmark repair got
    /// wrong — it must not *lie*.
    ///
    /// `bd-a` and `bd-b` are each other's parent (the cycle); `bd-x` blocks
    /// `bd-a` from outside. While `bd-x` is open the pair is genuinely blocked
    /// (through it). When `bd-x` closes, the correct answer is that **both are
    /// free** — a cycle does not block itself. The old repair settled into a
    /// stable-but-wrong state (each read the other's stored `is_blocked` and
    /// neither unmarked) and returned `Ok` with both still blocked. The least
    /// fixpoint marks from an all-unblocked base, so the cycle has no way to
    /// sustain itself.
    #[tokio::test]
    async fn a_parent_child_cycle_is_recomputed_correctly_not_just_without_spinning() {
        let pool = pool().await;
        for id in ["bd-a", "bd-b", "bd-x"] {
            issue(&pool, id).await;
        }
        edge(&pool, "bd-a", "bd-x", "blocks").await;
        edge(&pool, "bd-a", "bd-b", "parent-child").await;
        edge(&pool, "bd-b", "bd-a", "parent-child").await;

        let mut conn = pool.acquire().await.unwrap();
        recompute_all(&mut conn).await.expect("converges with x open");
        drop(conn);
        // x is open, so the cycle really is blocked — through x, not itself.
        assert!(is_blocked(&pool, "bd-a").await, "a is blocked by x");
        assert!(is_blocked(&pool, "bd-b").await, "b is blocked via its parent a");

        // Close the only real blocker. The cycle must now read as free.
        sqlx::query("UPDATE issues SET status = 'closed' WHERE id = 'bd-x'")
            .execute(&pool)
            .await
            .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let changed = recompute_all(&mut conn)
            .await
            .expect("still converges after the blocker closes");
        drop(conn);

        assert!(
            !is_blocked(&pool, "bd-a").await,
            "a's only real blocker closed; the cycle must not keep it blocked (this is the bug)"
        );
        assert!(!is_blocked(&pool, "bd-b").await, "same for b");
        assert_eq!(changed, 2, "exactly a and b flipped from blocked to free");
    }
}
