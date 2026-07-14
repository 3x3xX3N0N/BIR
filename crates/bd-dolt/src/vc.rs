//! Version control, via Dolt's SQL stored procedures.
//!
//! `CALL DOLT_COMMIT()`, `CALL DOLT_MERGE()`, `CALL DOLT_PUSH()`. Dolt exposes
//! its whole commit graph as SQL, over the same connection as everything else,
//! which is the only reason a Rust port can have one at all. Nothing here is a
//! reimplementation of git; every method is a procedure call or a read of a
//! `dolt_*` system table.
//!
//! # Three things this module must get right
//!
//! 1. **Merges and pulls recompute `is_blocked`.** See [`DoltStore::settle_readiness`].
//!    This is the reason the module is written the way it is; everything else is
//!    plumbing.
//! 2. **Dolt procedures must not run inside an explicit transaction.** They
//!    manipulate the working set and the commit graph, which a `BEGIN` has no
//!    way to roll back. Every method here therefore takes a plain pooled
//!    connection and issues statements in autocommit — no `sqlx::Transaction`
//!    appears in this file, and none should be added.
//! 3. **Conflicts are data, not errors.** A merge that conflicts is a merge that
//!    *worked* and left work to do; it returns [`MergeOutcome::Conflicted`], not
//!    `Err`. Reporting a conflicted merge as [`MergeOutcome::Merged`] would be a
//!    silent data-integrity failure, so [`classify_merge`] treats any evidence
//!    of conflict as decisive.

use async_trait::async_trait;
use bd_core::{Issue, IssueType, Priority, Status};
use bd_storage::capability::{
    ChangeKind, CommitInfo, Conflict, FieldChange, HistoryViewer, IssueDiff, MergeOutcome,
    RemoteStore, ResolveStrategy, Revision, VersionControl,
};
use bd_storage::error::anyhow_lite;
use bd_storage::{Error, Identity, Result, Storage};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::de::DeserializeOwned;
use sqlx::mysql::{MySql, MySqlRow};
use sqlx::pool::PoolConnection;
use sqlx::{MySqlConnection, Row};

use crate::DoltStore;

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

impl DoltStore {
    /// A plain pooled connection, in autocommit.
    ///
    /// Deliberately not a transaction. `CALL DOLT_MERGE()`, `CALL DOLT_COMMIT()`
    /// and friends move the working set and the commit graph; Dolt rejects them
    /// inside an open transaction, and even where it did not, a rollback could
    /// not undo a commit. Upstream hit this and documented it — the fix is
    /// structural, so it lives here rather than in a comment on each call site.
    async fn vc_conn(&self) -> Result<PoolConnection<MySql>> {
        self.pool.acquire().await.map_err(db)
    }

    /// Recompute the denormalized `is_blocked` cache over the whole graph.
    ///
    /// # Do not remove this. Do not make it conditional.
    ///
    /// `bd ready` does not walk the dependency graph. It filters on the
    /// `issues.is_blocked` column, which local write paths maintain
    /// *incrementally* — each write knows which edges and closures it touched
    /// and fixes up exactly those.
    ///
    /// A merge or a pull lands closed blockers and brand-new edges that **no
    /// local write path ever saw**. The cache is therefore stale *by definition*
    /// the moment rows arrive from elsewhere, and nothing about the arriving
    /// rows announces itself. Skip the recompute and `bd ready` is quietly,
    /// confidently wrong after every single sync: no error, no crash, just the
    /// wrong work handed to the next agent.
    ///
    /// It is not clever about when to skip, on purpose. A full pass over a local
    /// issue database, on an explicitly user-initiated sync, is cheap; being
    /// wrong is not. The one concession is the conflicted case — see below.
    async fn settle_readiness(&self, outcome: &MergeOutcome) -> Result<()> {
        let recomputed = Storage::recompute_blocked(self).await;
        match (outcome, recomputed) {
            (_, Ok(_)) => Ok(()),

            // A conflicted merge still lands every *non*-conflicting row in the
            // working set, so the cache is stale here too and we still try. But
            // a table holding unresolved conflicts may refuse writes, and if it
            // does, that must not turn a conflicted merge — which is data, not
            // an error — into an `Err`. Warn, return the conflicts, and let
            // `resolve_conflicts` recompute once the tables are writable again.
            (MergeOutcome::Conflicted { .. }, Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "could not recompute the is_blocked cache while conflicts are unresolved; \
                     `bd ready` may be wrong until the merge is resolved (`bd vc resolve`)"
                );
                Ok(())
            }

            (_, Err(e)) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// VersionControl
// ---------------------------------------------------------------------------

#[async_trait]
impl VersionControl for DoltStore {
    async fn current_branch(&self) -> Result<String> {
        let mut c = self.vc_conn().await?;
        sqlx::query_scalar::<_, String>("SELECT active_branch()")
            .fetch_one(&mut *c)
            .await
            .map_err(db)
    }

    async fn list_branches(&self) -> Result<Vec<String>> {
        let mut c = self.vc_conn().await?;
        sqlx::query_scalar::<_, String>("SELECT name FROM dolt_branches ORDER BY name")
            .fetch_all(&mut *c)
            .await
            .map_err(db)
    }

    async fn create_branch(&self, name: &str) -> Result<()> {
        let mut c = self.vc_conn().await?;
        call(&mut c, sqlx::query("CALL DOLT_BRANCH(?)").bind(name))
            .await
            .map_err(|e| {
                // Dolt says "fatal: A branch named 'x' already exists." Naming it
                // as `AlreadyExists` lets the CLI say so without regex-matching a
                // database error message.
                if says(&e, &["already exists"]) {
                    Error::AlreadyExists(name.to_string())
                } else {
                    e
                }
            })?;
        Ok(())
    }

    async fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        // `-d` refuses to drop a branch that is not merged; `-D` does it anyway.
        // Collapsing the two would make `force` a lie in one direction or the
        // other, and the un-forced refusal is the whole safety net.
        let flag = if force { "-D" } else { "-d" };
        let mut c = self.vc_conn().await?;
        call(
            &mut c,
            sqlx::query("CALL DOLT_BRANCH(?, ?)").bind(flag).bind(name),
        )
        .await
        .map_err(|e| {
            if says(&e, &["not found", "does not exist"]) {
                Error::NotFound(format!("branch {name}"))
            } else {
                e
            }
        })?;
        Ok(())
    }

    /// Switch branches.
    ///
    /// # The pooled-connection hazard
    ///
    /// In `dolt sql-server` the checked-out branch is **session state**.
    /// `CALL DOLT_CHECKOUT()` moves the session it runs in and nothing else — so
    /// on a connection pool it moves *one* connection, and the very next query
    /// may land on a sibling connection still sitting on the old branch. That is
    /// not a slow path or a stale read; it is writing issues to the wrong branch
    /// with no error anywhere.
    ///
    /// So the checkout is followed by moving the database's *default* branch,
    /// which is what a newly-opened session starts on. If that second step fails
    /// we fail loudly rather than leave the pool split-brained.
    ///
    /// Connections already idle in the pool are still on the old branch, and
    /// this module cannot reach them — the pool is built in `store.rs`. The
    /// complete fix is for the pool to carry the branch (a single connection, or
    /// an `after_connect` hook that checks out the recorded branch); see the
    /// hand-off note in the port status.
    async fn checkout(&self, name: &str) -> Result<()> {
        let branch = rev_literal(name)?;
        let mut c = self.vc_conn().await?;

        call(&mut c, sqlx::query("CALL DOLT_CHECKOUT(?)").bind(name))
            .await
            .map_err(|e| {
                if says(&e, &["not found", "did not match", "does not exist"]) {
                    Error::NotFound(format!("branch {name}"))
                } else {
                    e
                }
            })?;

        let database = sqlx::query_scalar::<_, Option<String>>("SELECT DATABASE()")
            .fetch_one(&mut *c)
            .await
            .map_err(db)?
            .ok_or_else(|| bad("the dolt connection has no database selected".to_string()))?;

        // A system-variable *name* cannot be a bind parameter, and the value is
        // spliced for the same reason `rev_literal` exists. Both halves are
        // validated, never escaped.
        let var = default_branch_var(&database)?;
        sqlx::query(&format!("SET {var} = {branch}"))
            .execute(&mut *c)
            .await
            .map_err(|e| {
                Error::Db(format!(
                    "checked out {name}, but could not make it the default branch for new \
                     connections ({var}): {e}. Other pooled connections are still on the old \
                     branch, so this workspace is now inconsistent — reopen it before writing."
                ))
            })?;

        Ok(())
    }

    async fn commit(&self, message: &str) -> Result<String> {
        let author = author_arg(self.identity());
        let mut c = self.vc_conn().await?;

        // `DOLT_COMMIT` commits what is *staged*, and nothing in this program
        // ever stages anything, so without this every commit would be empty.
        call(&mut c, sqlx::query("CALL DOLT_ADD('-A')")).await?;

        // `DOLT_COMMIT` errors when there is nothing to commit. That is not a
        // failure — the user asked for the working tree to be committed and it
        // already is — so answer with the commit they are already on, exactly as
        // git does. Checking first keeps the benign case off the error path.
        if dirty_tables(&mut c).await? == 0 {
            return head_hash(&mut c).await;
        }

        let rows = call(
            &mut c,
            sqlx::query("CALL DOLT_COMMIT('-m', ?, '--author', ?)")
                .bind(message)
                .bind(&author),
        )
        .await;

        match rows {
            // The same benign case, arrived at by a race (something else
            // committed our working set between the check and the call).
            Err(e) if says_nothing_to_commit(&e) => head_hash(&mut c).await,
            Err(e) => Err(e),
            Ok(rows) => match rows.first() {
                // DOLT_COMMIT's one result column is the new commit hash. Read
                // it positionally: the column name has changed across Dolt
                // versions, the position has not.
                Some(r) => r.try_get::<String, _>(0).map_err(db),
                None => head_hash(&mut c).await,
            },
        }
    }

    async fn current_commit(&self) -> Result<String> {
        let mut c = self.vc_conn().await?;
        head_hash(&mut c).await
    }

    async fn status(&self) -> Result<Vec<String>> {
        let mut c = self.vc_conn().await?;
        // A table appears twice in dolt_status when it has both staged and
        // unstaged changes; the caller asked which tables are dirty, not how.
        sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT table_name FROM dolt_status ORDER BY table_name",
        )
        .fetch_all(&mut *c)
        .await
        .map_err(db)
    }

    async fn log(&self, limit: u32) -> Result<Vec<CommitInfo>> {
        let mut c = self.vc_conn().await?;
        let rows = sqlx::query(
            "SELECT commit_hash, committer, message, date \
             FROM dolt_log ORDER BY date DESC LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut *c)
        .await
        .map_err(db)?;

        rows.iter()
            .map(|r| {
                Ok(CommitInfo {
                    hash: r.try_get("commit_hash").map_err(db)?,
                    author: r.try_get("committer").map_err(db)?,
                    message: r.try_get("message").map_err(db)?,
                    committed_at: sql_datetime(r, "date")?,
                })
            })
            .collect()
    }

    async fn merge(&self, branch: &str) -> Result<MergeOutcome> {
        let author = author_arg(self.identity());
        let mut c = self.vc_conn().await?;

        let before = head_hash(&mut c).await?;
        let rows = call(
            &mut c,
            sqlx::query("CALL DOLT_MERGE(?, '--author', ?)")
                .bind(branch)
                .bind(&author),
        )
        .await
        .map_err(|e| {
            if says(&e, &["not found", "did not match", "does not exist"]) {
                Error::NotFound(format!("branch {branch}"))
            } else {
                e
            }
        })?;

        let outcome = self.read_merge_result(&mut c, &before, &rows).await?;

        // Release the connection *before* recomputing: `recompute_blocked` goes
        // through `Storage`, which acquires its own connection from the same
        // pool. Holding this one across that call deadlocks a pool of one.
        drop(c);
        self.settle_readiness(&outcome).await?;
        Ok(outcome)
    }

    async fn conflicts(&self) -> Result<Vec<Conflict>> {
        let mut c = self.vc_conn().await?;

        // `dolt_conflicts` names the tables; the per-table `dolt_conflicts_<t>`
        // views hold the rows, with the three sides side by side.
        let tables = sqlx::query_scalar::<_, String>("SELECT `table` FROM dolt_conflicts")
            .fetch_all(&mut *c)
            .await
            .map_err(db)?;

        let mut out = Vec::new();
        for table in tables {
            let Some(shape) = conflict_shape(&table) else {
                // Not a table this schema knows about. Say so rather than drop
                // it: an unreported conflict is an invisible one.
                out.push(Conflict {
                    table,
                    issue_id: String::new(),
                    ours: None,
                    theirs: None,
                    base: None,
                });
                continue;
            };
            let rows = sqlx::query(&conflict_sql(shape))
                .fetch_all(&mut *c)
                .await
                .map_err(db)?;
            for r in &rows {
                out.push(Conflict {
                    table: shape.table.to_string(),
                    issue_id: r
                        .try_get::<Option<String>, _>("issue_id")
                        .map_err(db)?
                        .unwrap_or_default(),
                    ours: r.try_get("ours").map_err(db)?,
                    theirs: r.try_get("theirs").map_err(db)?,
                    base: r.try_get("base").map_err(db)?,
                });
            }
        }
        Ok(out)
    }

    async fn resolve_conflicts(&self, strategy: ResolveStrategy) -> Result<u64> {
        let flag = match strategy {
            ResolveStrategy::Ours => "--ours",
            ResolveStrategy::Theirs => "--theirs",
        };

        let mut c = self.vc_conn().await?;
        // Count first: once resolved, the rows are gone and there is nothing
        // left to count.
        let resolved = data_conflicts(&mut c).await?;
        if resolved == 0 {
            return Ok(0);
        }

        // `.` is Dolt's "every table with conflicts".
        call(
            &mut c,
            sqlx::query("CALL DOLT_CONFLICTS_RESOLVE(?, '.')").bind(flag),
        )
        .await?;
        drop(c);

        // Resolving is the moment the merged graph finally settles — whichever
        // side won, the winning rows are ones no local write path ever saw. This
        // is the second half of the recompute the merge could not finish while
        // the tables were still conflicted.
        Storage::recompute_blocked(self).await?;
        Ok(resolved)
    }
}

impl DoltStore {
    /// Turn `DOLT_MERGE`/`DOLT_PULL`'s result row into an honest [`MergeOutcome`].
    async fn read_merge_result(
        &self,
        c: &mut MySqlConnection,
        before: &str,
        rows: &[MySqlRow],
    ) -> Result<MergeOutcome> {
        let first = rows.first();
        // Both columns are absent in some Dolt versions and present in others,
        // so a missing column is a shrug, not an error.
        let fast_forward = first.and_then(|r| r.try_get::<i64, _>("fast_forward").ok()) == Some(1);
        let claimed = first
            .and_then(|r| r.try_get::<i64, _>("conflicts").ok())
            .unwrap_or(0)
            .max(0) as u64;

        // The procedure's `conflicts` number has meant different things across
        // versions (tables, then rows). The `dolt_*` tables are the authority on
        // what is actually unsettled; the procedure's number is only ever used
        // to *raise* the count, never to lower it to zero.
        let unsettled = unsettled_rows(c).await?.max(claimed);
        let after = head_hash(c).await?;
        Ok(classify_merge(before, &after, fast_forward, unsettled))
    }
}

// ---------------------------------------------------------------------------
// RemoteStore
// ---------------------------------------------------------------------------

#[async_trait]
impl RemoteStore for DoltStore {
    async fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        let mut c = self.vc_conn().await?;
        call(
            &mut c,
            sqlx::query("CALL DOLT_REMOTE('add', ?, ?)").bind(name).bind(url),
        )
        .await?;
        Ok(())
    }

    async fn remove_remote(&self, name: &str) -> Result<()> {
        let mut c = self.vc_conn().await?;
        call(
            &mut c,
            sqlx::query("CALL DOLT_REMOTE('remove', ?)").bind(name),
        )
        .await
        .map_err(|e| {
            if says(&e, &["unknown remote", "not found", "does not exist"]) {
                Error::NotFound(format!("remote {name}"))
            } else {
                e
            }
        })?;
        Ok(())
    }

    async fn list_remotes(&self) -> Result<Vec<(String, String)>> {
        let mut c = self.vc_conn().await?;
        let rows = sqlx::query("SELECT name, url FROM dolt_remotes ORDER BY name")
            .fetch_all(&mut *c)
            .await
            .map_err(db)?;
        rows.iter()
            .map(|r| Ok((r.try_get("name").map_err(db)?, r.try_get("url").map_err(db)?)))
            .collect()
    }

    async fn push(&self, remote: &str, branch: &str) -> Result<()> {
        let mut c = self.vc_conn().await?;
        call(
            &mut c,
            sqlx::query("CALL DOLT_PUSH(?, ?)").bind(remote).bind(branch),
        )
        .await
        .map_err(|e| remote_error("push", remote, e))?;
        Ok(())
    }

    async fn pull(&self, remote: &str, branch: &str) -> Result<MergeOutcome> {
        let mut c = self.vc_conn().await?;
        let before = head_hash(&mut c).await?;

        let rows = call(
            &mut c,
            sqlx::query("CALL DOLT_PULL(?, ?)").bind(remote).bind(branch),
        )
        .await
        .map_err(|e| remote_error("pull", remote, e))?;

        let outcome = self.read_merge_result(&mut c, &before, &rows).await?;

        // Release before `settle_readiness`, which acquires its own connection
        // from this pool (see `merge`).
        drop(c);

        // THE POINT OF THIS WHOLE FUNCTION, and the one line whose absence is
        // invisible: rows just arrived from another clone, so `is_blocked` is
        // stale by definition. `RemoteStore::pull`'s doc comment says this in
        // as many words. Do not "optimize" it away.
        self.settle_readiness(&outcome).await?;
        Ok(outcome)
    }

    async fn fetch(&self, remote: &str) -> Result<()> {
        // Fetch moves no rows into the working set — it only brings refs down —
        // so, alone among the remote operations, it does not disturb the
        // readiness cache and does not recompute it.
        let mut c = self.vc_conn().await?;
        call(&mut c, sqlx::query("CALL DOLT_FETCH(?)").bind(remote))
            .await
            .map_err(|e| remote_error("fetch", remote, e))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HistoryViewer
// ---------------------------------------------------------------------------

#[async_trait]
impl HistoryViewer for DoltStore {
    async fn history(&self, issue_id: &str) -> Result<Vec<Revision>> {
        // `dolt_history_issues` is the issue table crossed with every commit
        // that touched it. It carries the committer and the date but not the
        // message, so it joins `dolt_log` for that.
        let sql = format!(
            "SELECT l.commit_hash AS commit_hash, l.committer AS committer, \
                    l.message AS message, l.date AS date, {cols} \
             FROM dolt_history_issues AS h \
             JOIN dolt_log AS l ON l.commit_hash = h.commit_hash \
             WHERE h.id = ? \
             ORDER BY l.date DESC",
            cols = issue_columns("h")
        );

        let mut c = self.vc_conn().await?;
        let rows = sqlx::query(&sql)
            .bind(issue_id)
            .fetch_all(&mut *c)
            .await
            .map_err(db)?;

        rows.iter()
            .map(|r| {
                Ok(Revision {
                    commit: r.try_get("commit_hash").map_err(db)?,
                    author: r.try_get("committer").map_err(db)?,
                    message: r.try_get("message").map_err(db)?,
                    committed_at: sql_datetime(r, "date")?,
                    issue: issue_from_row(r)?,
                })
            })
            .collect()
    }

    async fn as_of(&self, issue_id: &str, commit: &str) -> Result<Option<Issue>> {
        // `AS OF` is Dolt's time-travel clause. Its argument is a literal, not a
        // bind parameter — hence `rev_literal`, which validates rather than
        // escapes.
        let sql = format!(
            "SELECT {cols} FROM issues AS OF {rev} WHERE id = ?",
            cols = issue_columns(""),
            rev = rev_literal(commit)?
        );

        let mut c = self.vc_conn().await?;
        let row = sqlx::query(&sql)
            .bind(issue_id)
            .fetch_optional(&mut *c)
            .await
            .map_err(db)?;

        row.as_ref().map(issue_from_row).transpose()
    }

    async fn diff(&self, from: &str, to: &str) -> Result<Vec<IssueDiff>> {
        // `DOLT_DIFF(from, to, table)` is a table function; like `AS OF`, its
        // arguments are literals.
        let sql = diff_sql(&rev_literal(from)?, &rev_literal(to)?);

        let mut c = self.vc_conn().await?;
        let rows = sqlx::query(&sql).fetch_all(&mut *c).await.map_err(db)?;

        rows.iter()
            .map(|r| {
                let change = change_kind(&r.try_get::<String, _>("diff_type").map_err(db)?)?;
                let issue_id = r
                    .try_get::<Option<String>, _>("issue_id")
                    .map_err(db)?
                    .unwrap_or_default();

                let mut sides = Vec::with_capacity(DIFF_FIELDS.len());
                for f in DIFF_FIELDS {
                    sides.push((
                        *f,
                        r.try_get::<Option<String>, _>(&*format!("from_{f}"))
                            .map_err(db)?,
                        r.try_get::<Option<String>, _>(&*format!("to_{f}"))
                            .map_err(db)?,
                    ));
                }

                Ok(IssueDiff {
                    issue_id,
                    change,
                    fields: field_changes(change, sides),
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Merge classification — the part that must not lie
// ---------------------------------------------------------------------------

/// Decide what a merge or pull actually did.
///
/// Pure, and separated from the SQL for exactly that reason: this is the
/// judgment that a wrong answer makes into a data-integrity bug, so it is the
/// part that gets tested without a database.
///
/// The ordering is not arbitrary. `unsettled` wins over everything, because a
/// merge that left conflicts and *also* fast-forwarded is a conflicted merge —
/// reporting it as [`MergeOutcome::Merged`] would tell the user their work
/// landed cleanly when half of it is sitting in `dolt_conflicts`.
///
/// `unsettled` counts data conflicts, schema conflicts and constraint
/// violations alike. All three are "the merge landed but left something to
/// settle", which is precisely what `Conflicted` means, and a constraint
/// violation in particular is how a merge announces a dependency edge whose
/// target does not exist — the dangling edge `bd lint` was written to find.
fn classify_merge(before: &str, after: &str, fast_forward: bool, unsettled: u64) -> MergeOutcome {
    if unsettled > 0 {
        return MergeOutcome::Conflicted { count: unsettled };
    }
    if after == before {
        // Nothing moved and nothing conflicted: the branch was already in.
        return MergeOutcome::UpToDate;
    }
    if fast_forward {
        MergeOutcome::FastForward {
            to: after.to_string(),
        }
    } else {
        MergeOutcome::Merged {
            commit: after.to_string(),
        }
    }
}

/// Rows in `dolt_conflicts` — data conflicts only, and the number
/// `resolve_conflicts` can actually resolve.
async fn data_conflicts(c: &mut MySqlConnection) -> Result<u64> {
    // SUM() over an integer column comes back DECIMAL in MySQL, which does not
    // decode as an integer. Cast it.
    let n: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(num_conflicts), 0) AS SIGNED) FROM dolt_conflicts",
    )
    .fetch_one(&mut *c)
    .await
    .map_err(db)?;
    Ok(n.max(0) as u64)
}

/// Everything a merge can leave behind that a human has to settle.
async fn unsettled_rows(c: &mut MySqlConnection) -> Result<u64> {
    let mut n = data_conflicts(c).await?;

    // These two system tables are newer than `dolt_conflicts` and absent on
    // older servers. A missing table means "this Dolt cannot leave that kind of
    // mess", not "the query failed" — so their absence is tolerated, while
    // `dolt_conflicts` above is mandatory.
    n += optional_count(c, "SELECT COUNT(*) FROM dolt_schema_conflicts").await;
    n += optional_count(
        c,
        "SELECT CAST(COALESCE(SUM(num_violations), 0) AS SIGNED) FROM dolt_constraint_violations",
    )
    .await;
    Ok(n)
}

async fn optional_count(c: &mut MySqlConnection, sql: &str) -> u64 {
    match sqlx::query_scalar::<_, i64>(sql).fetch_one(c).await {
        Ok(n) => n.max(0) as u64,
        Err(e) => {
            tracing::debug!(error = %e, sql, "optional dolt system table unavailable");
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Small SQL helpers
// ---------------------------------------------------------------------------

/// Run a `CALL`, collecting every result set.
///
/// `fetch_all` rather than `fetch_one`: a stored procedure returns its result
/// set *plus* a trailing OK packet, and the single-row fetchers trip over the
/// second one. Some procedures also return no rows at all, which is not an
/// error and which `fetch_one` would report as one.
async fn call<'q>(
    c: &mut PoolConnection<MySql>,
    q: sqlx::query::Query<'q, MySql, sqlx::mysql::MySqlArguments>,
) -> Result<Vec<MySqlRow>> {
    q.fetch_all(&mut **c).await.map_err(db)
}

async fn head_hash(c: &mut MySqlConnection) -> Result<String> {
    match sqlx::query_scalar::<_, String>("SELECT HASHOF('HEAD')")
        .fetch_one(&mut *c)
        .await
    {
        Ok(h) => Ok(h),
        // Older servers spell it differently; `dolt_log` has been stable
        // forever and its first row is HEAD.
        Err(_) => sqlx::query_scalar::<_, String>("SELECT commit_hash FROM dolt_log LIMIT 1")
            .fetch_one(c)
            .await
            .map_err(db),
    }
}

/// `CAST(… AS SIGNED)` is not noise. MySQL types `COUNT(*)` as `BIGINT
/// UNSIGNED`, and sqlx's `Type<MySql> for i64` explicitly rejects unsigned — so
/// the bare count fails to decode. Reading it as `u64` instead fixes MySQL and
/// breaks Dolt, whose go-mysql-server types the same expression *signed*. The
/// cast is the only spelling both servers agree on.
async fn dirty_tables(c: &mut PoolConnection<MySql>) -> Result<i64> {
    sqlx::query_scalar::<_, i64>("SELECT CAST(COUNT(*) AS SIGNED) FROM dolt_status")
        .fetch_one(&mut **c)
        .await
        .map_err(db)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

fn bad(msg: String) -> Error {
    Error::Other(anyhow_lite::Error(msg))
}

fn says(e: &Error, needles: &[&str]) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    needles.iter().any(|n| msg.contains(n))
}

fn says_nothing_to_commit(e: &Error) -> bool {
    is_nothing_to_commit(&e.to_string())
}

/// Dolt's several ways of saying the working tree is clean.
///
/// Not a failure: the caller asked for the working tree to be committed and it
/// already is. Treating it as an error is how a no-op sync becomes a red build.
fn is_nothing_to_commit(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("nothing to commit")
        || m.contains("no changes added to commit")
        || m.contains("cannot commit an empty commit")
}

/// Credentials are Dolt's business, not ours.
///
/// There is no beads credential store and there must not be one: `dolt` already
/// resolves remotes through its own config and environment, and a second source
/// of truth would silently shadow it. All this does is make the failure legible
/// when Dolt's auth is the thing that failed — and only then, so that a plain
/// "unknown remote" is not buried under advice about logging in.
fn remote_error(op: &str, remote: &str, e: Error) -> Error {
    let msg = e.to_string();
    if looks_like_auth(&msg) {
        Error::Db(format!(
            "dolt {op} to '{remote}' was rejected: {msg} — credentials come from dolt's own \
             config, not from beads (`dolt login`, or DOLT_REMOTE_PASSWORD in the environment)"
        ))
    } else {
        Error::Db(format!("dolt {op} to '{remote}' failed: {msg}"))
    }
}

fn looks_like_auth(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "unauthorized",
        "authentication",
        "permission denied",
        "access denied",
        "credential",
        "forbidden",
        "not logged in",
        "401",
        "403",
    ]
    .iter()
    .any(|n| m.contains(n))
}

// ---------------------------------------------------------------------------
// Identifiers spliced into SQL
// ---------------------------------------------------------------------------

/// Quote a revision for a context that cannot take a bind parameter.
///
/// Dolt's `AS OF` clause and its table functions (`DOLT_DIFF(...)`) take literal
/// expressions; prepared-statement placeholders do not work there. So the
/// revision has to be spliced into the SQL text, and this is the only thing
/// standing between a branch name and an injection.
///
/// It **rejects rather than escapes**. A revision is a git-shaped word — a hash,
/// a branch, `HEAD~2` — and anything that is not one is a bug or an attack, so
/// there is nothing to escape *to*. Since the accepted alphabet contains no
/// quote and no backslash, wrapping the result in quotes is then trivially safe.
fn rev_literal(rev: &str) -> Result<String> {
    const MAX: usize = 256;
    let shaped = !rev.is_empty()
        && rev.len() <= MAX
        && rev.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '^' | '~' | '+')
        });

    // `.` is Dolt's all-tables wildcard and `..` is git range syntax; neither is
    // a revision. A leading `-` would be read as a flag by the procedures.
    if !shaped || rev == "." || rev.contains("..") || rev.starts_with('-') {
        return Err(bad(format!("not a usable dolt revision: {rev:?}")));
    }
    Ok(format!("'{rev}'"))
}

/// The `SET @@GLOBAL.<db>_default_branch` variable for a database.
///
/// The variable *name* embeds the database name, and a name cannot be a bind
/// parameter, so it is validated to an identifier alphabet on the way in.
fn default_branch_var(database: &str) -> Result<String> {
    let ok = !database.is_empty()
        && database
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(bad(format!(
            "dolt database name {database:?} is not a plain identifier; cannot set its default \
             branch, which means a checkout could not be made to stick"
        )));
    }
    Ok(format!("@@GLOBAL.{database}_default_branch"))
}

/// Dolt wants `Name <email>` and refuses a commit without one.
///
/// [`Identity::actor`] is "an agent name or a git email" — both shapes are
/// ordinary here, so both have to produce a valid author line. A bare agent name
/// gets a synthetic local address rather than being rejected: refusing to commit
/// because an agent is called `worker-3` would be an absurd way to lose work.
fn author_arg(id: &Identity) -> String {
    let actor = id.actor.trim();
    if actor.is_empty() {
        return "beads <beads@localhost>".to_string();
    }
    // Already `Name <email>`; pass it through untouched.
    if actor.contains('<') && actor.ends_with('>') {
        return actor.to_string();
    }
    if actor.contains('@') && !actor.contains(' ') {
        return format!("{actor} <{actor}>");
    }
    // Angle brackets would terminate the address early; strip them out of a name.
    let name: String = actor.chars().filter(|c| !matches!(c, '<' | '>')).collect();
    let local: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{name} <{local}@beads.local>")
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/// The issue columns a `bd diff` reports on.
///
/// Derived columns are deliberately absent. `is_blocked`, `close_is_failure` and
/// `content_hash` are caches of things already shown — a diff that announced
/// "is_blocked: 0 -> 1" would be reporting the *cache* as a change the user made,
/// which is exactly the confusion this crate exists to prevent.
const DIFF_FIELDS: &[&str] = &[
    "title",
    "description",
    "design",
    "acceptance_criteria",
    "notes",
    "status",
    "priority",
    "issue_type",
    "assignee",
    "owner",
    "estimated_minutes",
    "created_at",
    "updated_at",
    "started_at",
    "closed_at",
    "close_reason",
    "due_at",
    "defer_until",
    "external_ref",
    "source_system",
    "spec_id",
    "metadata",
    "ephemeral",
    "no_history",
    "pinned",
    "is_template",
    "wisp_type",
    "mol_type",
    "work_type",
];

/// `SELECT` over Dolt's diff table function.
///
/// Every value is `CAST(... AS CHAR)` so that one decode path handles the lot:
/// [`FieldChange`] is text on both sides anyway, and casting in SQL is cheaper
/// than thirty match arms that each have to know whether `pinned` came back as
/// an `INT` or a `TINYINT`.
fn diff_sql(from: &str, to: &str) -> String {
    let mut cols = String::from(
        "diff_type, COALESCE(CAST(to_id AS CHAR), CAST(from_id AS CHAR)) AS issue_id",
    );
    for f in DIFF_FIELDS {
        cols.push_str(&format!(
            ", CAST(from_{f} AS CHAR) AS from_{f}, CAST(to_{f} AS CHAR) AS to_{f}"
        ));
    }
    format!("SELECT {cols} FROM DOLT_DIFF({from}, {to}, 'issues')")
}

fn change_kind(diff_type: &str) -> Result<ChangeKind> {
    match diff_type {
        "added" => Ok(ChangeKind::Added),
        "modified" => Ok(ChangeKind::Modified),
        "removed" => Ok(ChangeKind::Removed),
        other => Err(bad(format!("unknown dolt diff_type: {other:?}"))),
    }
}

/// Which fields a diff row actually changed.
///
/// For a modification, only the fields that differ — a diff listing thirty
/// unchanged columns is a diff nobody reads. For an add or a remove the whole
/// row is the change, so every field it has is reported.
fn field_changes(
    change: ChangeKind,
    sides: Vec<(&'static str, Option<String>, Option<String>)>,
) -> Vec<FieldChange> {
    sides
        .into_iter()
        .filter(|(_, from, to)| match change {
            ChangeKind::Modified => from != to,
            ChangeKind::Added => to.is_some(),
            ChangeKind::Removed => from.is_some(),
        })
        .map(|(field, from, to)| FieldChange {
            field: field.to_string(),
            from,
            to,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------

/// How to read one `dolt_conflicts_<table>` view.
///
/// Those views put the three sides of a conflict side by side, one column each:
/// `base_x`, `our_x`, `their_x`. A side whose primary key is NULL is a side on
/// which the row does not exist — which is how an add/delete conflict looks —
/// and that is what `exists` probes.
struct ConflictShape {
    table: &'static str,
    /// The column that says which issue this conflict is *about*.
    issue_key: &'static str,
    /// The column that proves the row exists on a given side.
    exists: &'static str,
    /// Columns worth showing the user.
    summary: &'static [&'static str],
}

fn conflict_shape(table: &str) -> Option<&'static ConflictShape> {
    // The whole schema, so this match is exhaustive rather than a best guess.
    const SHAPES: &[ConflictShape] = &[
        ConflictShape {
            table: "issues",
            issue_key: "id",
            exists: "id",
            summary: &[
                "title",
                "status",
                "priority",
                "assignee",
                "close_reason",
                "updated_at",
            ],
        },
        ConflictShape {
            table: "dependencies",
            issue_key: "issue_id",
            exists: "issue_id",
            summary: &["issue_id", "depends_on_id", "type"],
        },
        ConflictShape {
            table: "labels",
            issue_key: "issue_id",
            exists: "issue_id",
            summary: &["issue_id", "label"],
        },
        ConflictShape {
            table: "comments",
            issue_key: "issue_id",
            exists: "id",
            summary: &["id", "author", "text", "created_at"],
        },
        ConflictShape {
            table: "events",
            issue_key: "issue_id",
            exists: "id",
            summary: &["event_type", "actor", "new_value", "created_at"],
        },
        ConflictShape {
            // Not issue-shaped. The config key goes in `issue_id` because that
            // is the only identifier `Conflict` has, and a conflict reported
            // with no identifier at all is one nobody can act on.
            table: "config",
            issue_key: "key",
            exists: "key",
            summary: &["key", "value"],
        },
    ];
    SHAPES.iter().find(|s| s.table == table)
}

fn conflict_sql(s: &ConflictShape) -> String {
    let side = |prefix: &str| -> String {
        let pairs: Vec<String> = s
            .summary
            .iter()
            .map(|c| format!("'{c}', CAST({prefix}_{c} AS CHAR)"))
            .collect();
        // JSON_OBJECT over an all-NULL row yields `{"a": null}`, not NULL, so
        // "this row does not exist on this side" has to be asked separately —
        // otherwise a delete/modify conflict reads as a modify/modify one.
        format!(
            "CASE WHEN {prefix}_{exists} IS NULL THEN NULL \
             ELSE CAST(JSON_OBJECT({pairs}) AS CHAR) END",
            exists = s.exists,
            pairs = pairs.join(", ")
        )
    };

    format!(
        "SELECT COALESCE(CAST(our_{k} AS CHAR), CAST(their_{k} AS CHAR), CAST(base_{k} AS CHAR)) \
                AS issue_id, \
                {ours} AS ours, {theirs} AS theirs, {base} AS base \
         FROM dolt_conflicts_{table}",
        k = s.issue_key,
        ours = side("our"),
        theirs = side("their"),
        base = side("base"),
        table = s.table
    )
}

// ---------------------------------------------------------------------------
// Rows -> domain
// ---------------------------------------------------------------------------

/// Every issue column, in a fixed order, optionally table-qualified.
///
/// Hand-written rather than `SELECT *` because `dolt_history_issues` adds
/// columns of its own (`commit_hash`, `committer`, `commit_date`) that collide
/// with the joined `dolt_log`, and a star would make which one wins depend on
/// join order.
fn issue_columns(table: &str) -> String {
    const COLS: &[&str] = &[
        "id",
        "title",
        "description",
        "design",
        "acceptance_criteria",
        "notes",
        "status",
        "priority",
        "issue_type",
        "assignee",
        "owner",
        "created_by",
        "estimated_minutes",
        "created_at",
        "updated_at",
        "started_at",
        "closed_at",
        "close_reason",
        "closed_by_session",
        "lease_expires_at",
        "heartbeat_at",
        "due_at",
        "defer_until",
        "external_ref",
        "source_system",
        "spec_id",
        "metadata",
        "ephemeral",
        "no_history",
        "pinned",
        "is_template",
        "wisp_type",
        "mol_type",
        "work_type",
        "content_hash",
    ];
    let q = if table.is_empty() {
        String::new()
    } else {
        format!("{table}.")
    };
    COLS.iter()
        .map(|c| format!("{q}{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn issue_from_row(r: &MySqlRow) -> Result<Issue> {
    let id: String = r.try_get("id").map_err(db)?;

    let metadata = match r.try_get::<Option<String>, _>("metadata").map_err(db)?.as_deref() {
        None | Some("") => None,
        Some(s) => Some(
            serde_json::from_str(s)
                .map_err(|e| Error::Db(format!("issue {id}: corrupt metadata JSON: {e}")))?,
        ),
    };

    let unit_enum = |col: &str| -> Result<Option<String>> {
        Ok(r.try_get::<Option<String>, _>(col)
            .map_err(db)?
            .filter(|s| !s.is_empty()))
    };

    Ok(Issue {
        id,
        title: r.try_get("title").map_err(db)?,
        description: r.try_get("description").map_err(db)?,
        design: r.try_get("design").map_err(db)?,
        acceptance_criteria: r.try_get("acceptance_criteria").map_err(db)?,
        notes: r.try_get("notes").map_err(db)?,

        status: Status::from(r.try_get::<String, _>("status").map_err(db)?),
        priority: Priority(r.try_get::<i64, _>("priority").map_err(db)? as i32),
        issue_type: IssueType::from(r.try_get::<String, _>("issue_type").map_err(db)?),

        assignee: r.try_get("assignee").map_err(db)?,
        owner: r.try_get("owner").map_err(db)?,
        created_by: r.try_get("created_by").map_err(db)?,
        estimated_minutes: r.try_get("estimated_minutes").map_err(db)?,

        created_at: text_ts_req(r, "created_at")?,
        updated_at: text_ts_req(r, "updated_at")?,
        started_at: text_ts(r, "started_at")?,
        closed_at: text_ts(r, "closed_at")?,
        close_reason: r.try_get("close_reason").map_err(db)?,
        closed_by_session: r.try_get("closed_by_session").map_err(db)?,

        lease_expires_at: text_ts(r, "lease_expires_at")?,
        heartbeat_at: text_ts(r, "heartbeat_at")?,

        due_at: text_ts(r, "due_at")?,
        defer_until: text_ts(r, "defer_until")?,

        external_ref: r.try_get("external_ref").map_err(db)?,
        source_system: r.try_get("source_system").map_err(db)?,
        spec_id: r.try_get("spec_id").map_err(db)?,
        metadata,

        ephemeral: int_bool(r, "ephemeral")?,
        no_history: int_bool(r, "no_history")?,
        pinned: int_bool(r, "pinned")?,
        is_template: int_bool(r, "is_template")?,

        wisp_type: unit_enum("wisp_type")?.as_deref().and_then(enum_from_str),
        mol_type: unit_enum("mol_type")?.as_deref().and_then(enum_from_str),
        work_type: unit_enum("work_type")?.as_deref().and_then(enum_from_str),

        // A historical revision of an issue is the issue's own row at a commit.
        // Its labels, edges and comments live in other tables and are *not*
        // hydrated here — same contract as `Storage::list_issues`.
        labels: Vec::new(),
        dependencies: Vec::new(),
        comments: Vec::new(),

        content_hash: r.try_get("content_hash").map_err(db)?,
    })
}

/// The unit-variant enums have no `as_str`, only a serde renaming; going through
/// serde keeps the stored spelling and the JSON spelling from drifting apart.
fn enum_from_str<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// Booleans are `TINYINT(1)`, which sqlx decodes as an integer.
fn int_bool(r: &MySqlRow, col: &str) -> Result<bool> {
    Ok(r.try_get::<i64, _>(col).map_err(db)? != 0)
}

/// Timestamps are `DATETIME(6)`, decoded by sqlx directly.
///
/// They were `LONGTEXT` holding RFC-3339 when this file was written, and these
/// two functions parsed the string. `schema.sql` then moved to real datetimes —
/// sqlx's `DateTime<Utc>` is only `compatible()` with `DATETIME`/`TIMESTAMP`, so
/// text columns would have failed every decode. The microsecond precision is not
/// decoration either: a bare `DATETIME` truncates to whole seconds and would
/// collapse lease expiry and the `(created_at, id)` sort tiebreak onto a
/// one-second grid.
fn text_ts(r: &MySqlRow, col: &str) -> Result<Option<DateTime<Utc>>> {
    r.try_get::<Option<DateTime<Utc>>, _>(col).map_err(db)
}

fn text_ts_req(r: &MySqlRow, col: &str) -> Result<DateTime<Utc>> {
    r.try_get::<DateTime<Utc>, _>(col).map_err(db)
}

/// `dolt_log.date`, by contrast, *is* a real SQL `DATETIME` — Dolt's own system
/// tables are not stored with our schema's conventions.
fn sql_datetime(r: &MySqlRow, col: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = r.try_get::<DateTime<Utc>, _>(col) {
        return Ok(dt);
    }
    let naive: NaiveDateTime = r.try_get(col).map_err(db)?;
    Ok(naive.and_utc())
}

// ---------------------------------------------------------------------------
// Tests
//
// Everything below runs without a `dolt` binary, because everything below is
// the part that can be wrong on a machine that has one: the merge
// classification, the strings spliced into SQL, and the author line Dolt
// refuses commits without. The behavior that genuinely needs a server lives in
// `tests/vc.rs` and skips loudly.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- the classification that must not lie ---

    #[test]
    fn a_clean_merge_that_moved_head_is_merged() {
        assert_eq!(
            classify_merge("aaa", "bbb", false, 0),
            MergeOutcome::Merged {
                commit: "bbb".into()
            }
        );
    }

    #[test]
    fn a_fast_forward_is_not_a_merge_commit() {
        assert_eq!(
            classify_merge("aaa", "bbb", true, 0),
            MergeOutcome::FastForward { to: "bbb".into() }
        );
    }

    #[test]
    fn head_that_did_not_move_is_up_to_date() {
        assert_eq!(classify_merge("aaa", "aaa", false, 0), MergeOutcome::UpToDate);
    }

    #[test]
    fn conflicts_outrank_everything_else() {
        // The failure this ordering exists to prevent: Dolt reports a
        // fast-forward *and* leaves conflicts, and we tell the user their work
        // landed cleanly. Any evidence of conflict is decisive.
        assert_eq!(
            classify_merge("aaa", "bbb", true, 3),
            MergeOutcome::Conflicted { count: 3 }
        );
        assert_eq!(
            classify_merge("aaa", "bbb", false, 1),
            MergeOutcome::Conflicted { count: 1 }
        );
        // Even a merge that moved nothing: conflicts sit in the working set, so
        // an unmoved HEAD is not "up to date".
        assert_eq!(
            classify_merge("aaa", "aaa", false, 2),
            MergeOutcome::Conflicted { count: 2 }
        );
    }

    // --- strings that get spliced into SQL ---

    #[test]
    fn rev_literal_accepts_the_shapes_dolt_uses() {
        for r in [
            "main",
            "feature/a-b",
            "HEAD",
            "HEAD~1",
            "main^",
            "v1.2.3",
            "qtcnv0fbrivqvhrgn0ubvjm5f4t4kgkt",
            "WORKING",
        ] {
            assert_eq!(rev_literal(r).unwrap(), format!("'{r}'"), "rejected {r:?}");
        }
    }

    #[test]
    fn rev_literal_rejects_anything_that_could_close_the_quote() {
        // The whole reason this function exists. `AS OF ?` is not a thing in
        // Dolt, so a revision is spliced into SQL text; if this test ever goes
        // green on one of these, the diff and time-travel paths are injectable.
        for r in [
            "main'; DROP TABLE issues; --",
            "main' OR '1'='1",
            "main\\",
            "main branch",
            "",
            ".",
            "-f",
            "a..b",
            "main\n",
            "\u{5b8b}",
        ] {
            assert!(rev_literal(r).is_err(), "accepted {r:?}");
        }
    }

    #[test]
    fn a_rev_that_survives_validation_cannot_contain_a_quote() {
        // Belt and braces: the property the splice relies on.
        for r in ["main", "HEAD~3", "feature/x"] {
            let lit = rev_literal(r).unwrap();
            assert_eq!(lit.matches('\'').count(), 2);
            assert!(!lit.contains('\\'));
        }
    }

    #[test]
    fn default_branch_var_refuses_a_database_name_it_cannot_quote() {
        assert_eq!(
            default_branch_var("beads").unwrap(),
            "@@GLOBAL.beads_default_branch"
        );
        assert_eq!(
            default_branch_var("my_db2").unwrap(),
            "@@GLOBAL.my_db2_default_branch"
        );
        // A checkout that cannot be made to stick has to fail loudly: a pool
        // half on one branch and half on another writes to the wrong branch
        // with no error at all.
        for bad in ["my-db", "db;drop", "", "db name", "db`x`"] {
            assert!(default_branch_var(bad).is_err(), "accepted {bad:?}");
        }
    }

    // --- the author line Dolt refuses commits without ---

    #[test]
    fn an_agent_name_still_produces_a_valid_author() {
        // Dolt rejects a commit whose author is not `Name <email>`. Refusing to
        // commit because an agent is called `worker-3` would be an absurd way to
        // lose work.
        assert_eq!(
            author_arg(&Identity::new("worker-3")),
            "worker-3 <worker-3@beads.local>"
        );
        assert_eq!(
            author_arg(&Identity::new("Ada Lovelace")),
            "Ada Lovelace <Ada-Lovelace@beads.local>"
        );
    }

    #[test]
    fn an_email_actor_becomes_its_own_address() {
        assert_eq!(
            author_arg(&Identity::new("ada@example.com")),
            "ada@example.com <ada@example.com>"
        );
    }

    #[test]
    fn an_actor_that_is_already_an_author_line_is_left_alone() {
        assert_eq!(
            author_arg(&Identity::new("Ada <ada@example.com>")),
            "Ada <ada@example.com>"
        );
    }

    #[test]
    fn an_empty_actor_does_not_produce_an_empty_author() {
        assert_eq!(author_arg(&Identity::default()), "beads <beads@localhost>");
        // A stray angle bracket must not be able to terminate the address early.
        assert_eq!(author_arg(&Identity::new("a<b")), "ab <ab@beads.local>");
    }

    // --- benign outcomes that must not be errors ---

    #[test]
    fn dolts_ways_of_saying_the_tree_is_clean_are_all_benign() {
        assert!(is_nothing_to_commit("nothing to commit"));
        assert!(is_nothing_to_commit(
            "Error: nothing to commit, working tree clean"
        ));
        assert!(is_nothing_to_commit("no changes added to commit"));
        assert!(!is_nothing_to_commit("permission denied"));
        assert!(!is_nothing_to_commit("connection refused"));
    }

    #[test]
    fn only_auth_failures_get_the_credentials_hint() {
        assert!(looks_like_auth("permission denied (publickey)"));
        assert!(looks_like_auth("HTTP 403 Forbidden"));
        assert!(looks_like_auth("Unauthorized"));
        // An unknown remote is not a login problem, and burying it under advice
        // about `dolt login` would send the user in the wrong direction.
        assert!(!looks_like_auth("unknown remote: origin"));
        assert!(!looks_like_auth("connection refused"));
    }

    // --- diff ---

    #[test]
    fn a_modification_reports_only_what_changed() {
        let sides = vec![
            ("title", Some("a".to_string()), Some("b".to_string())),
            ("status", Some("open".to_string()), Some("open".to_string())),
            ("assignee", None, Some("ada".to_string())),
        ];
        let fields = field_changes(ChangeKind::Modified, sides);
        assert_eq!(
            fields.iter().map(|f| f.field.as_str()).collect::<Vec<_>>(),
            ["title", "assignee"]
        );
    }

    #[test]
    fn an_added_issue_reports_every_field_it_has() {
        let sides = vec![
            ("title", None, Some("b".to_string())),
            ("assignee", None, None),
        ];
        let fields = field_changes(ChangeKind::Added, sides);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].from, None);
        assert_eq!(fields[0].to.as_deref(), Some("b"));
    }

    #[test]
    fn a_removed_issue_reports_what_it_had() {
        let sides = vec![
            ("title", Some("b".to_string()), None),
            ("assignee", None, None),
        ];
        let fields = field_changes(ChangeKind::Removed, sides);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].from.as_deref(), Some("b"));
        assert_eq!(fields[0].to, None);
    }

    #[test]
    fn diff_never_reports_the_derived_caches_as_user_changes() {
        for derived in ["is_blocked", "close_is_failure", "content_hash"] {
            assert!(
                !DIFF_FIELDS.contains(&derived),
                "{derived} is a cache, not a change the user made"
            );
        }
    }

    #[test]
    fn diff_sql_asks_for_both_sides_of_every_field() {
        let sql = diff_sql("'main'", "'feature'");
        assert!(sql.contains("FROM DOLT_DIFF('main', 'feature', 'issues')"));
        assert!(sql.starts_with("SELECT diff_type,"));
        for f in DIFF_FIELDS {
            assert!(sql.contains(&format!("AS from_{f}")), "missing from_{f}");
            assert!(sql.contains(&format!("AS to_{f}")), "missing to_{f}");
        }
    }

    #[test]
    fn unknown_diff_types_are_loud() {
        assert_eq!(change_kind("added").unwrap(), ChangeKind::Added);
        assert_eq!(change_kind("modified").unwrap(), ChangeKind::Modified);
        assert_eq!(change_kind("removed").unwrap(), ChangeKind::Removed);
        // Guessing here would silently mislabel a deletion as an edit.
        assert!(change_kind("renamed").is_err());
    }

    // --- conflicts ---

    #[test]
    fn every_table_in_the_schema_has_a_conflict_shape() {
        // A conflict on a table nobody described is a conflict reported with no
        // identifier — actionable by nobody. These are all the tables there are.
        for t in [
            "issues",
            "dependencies",
            "labels",
            "comments",
            "events",
            "config",
        ] {
            assert!(conflict_shape(t).is_some(), "no shape for {t}");
        }
        assert!(conflict_shape("dolt_docs").is_none());
    }

    #[test]
    fn conflict_sql_reads_all_three_sides_and_can_tell_a_side_is_absent() {
        let sql = conflict_sql(conflict_shape("issues").unwrap());
        assert!(sql.contains("FROM dolt_conflicts_issues"));
        for side in ["our", "their", "base"] {
            assert!(sql.contains(&format!("CAST({side}_title AS CHAR)")));
            // The NULL probe: without it, a delete/modify conflict would read as
            // a modify/modify one, with the deleted side shown as a row of nulls.
            assert!(sql.contains(&format!("CASE WHEN {side}_id IS NULL THEN NULL")));
        }
        assert!(sql.contains("AS issue_id"));
    }

    #[test]
    fn a_conflict_on_a_non_issue_table_still_names_something() {
        let sql = conflict_sql(conflict_shape("config").unwrap());
        assert!(sql.contains("COALESCE(CAST(our_key AS CHAR)"));
        assert!(sql.contains("FROM dolt_conflicts_config"));
    }

    // --- row mapping ---

    #[test]
    fn issue_columns_can_be_table_qualified_for_the_history_join() {
        let plain = issue_columns("");
        assert!(plain.starts_with("id, title,"));
        assert!(plain.contains(", content_hash"));

        let qualified = issue_columns("h");
        assert!(qualified.starts_with("h.id, h.title,"));
        // The join drags in dolt_log's `committer`/`date`; an unqualified star
        // would make which side wins depend on join order.
        assert!(!qualified.contains(" committer"));
    }
}
