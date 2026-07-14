-- Beads SQLite schema.
--
-- Deliberately simpler than upstream, which duplicates every table into a
-- `wisp_*` shadow twin so that ephemeral beads can live outside the commit
-- graph. SQLite has no commit graph, so the twin buys nothing and costs a
-- doubled write path in every query, every recompute, and every migration.
-- Here an ephemeral bead is an ordinary row with `ephemeral = 1`.
--
-- Timestamps are TEXT holding RFC-3339 with an explicit `+00:00` offset --
-- exactly what sqlx's `DateTime<Utc>` encoder emits. That encoding is
-- lexicographically ordered, so `<`, `>` and ORDER BY on these columns are
-- correct without a conversion. Never compare one against a SQL-side
-- `datetime('now')`, whose format differs; bind a Rust `Utc::now()` instead.

CREATE TABLE IF NOT EXISTS issues (
    id                  TEXT PRIMARY KEY,

    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    design              TEXT NOT NULL DEFAULT '',
    acceptance_criteria TEXT NOT NULL DEFAULT '',
    notes               TEXT NOT NULL DEFAULT '',

    status              TEXT NOT NULL DEFAULT 'open',
    priority            INTEGER NOT NULL DEFAULT 2,
    issue_type          TEXT NOT NULL DEFAULT 'task',

    assignee            TEXT NOT NULL DEFAULT '',
    owner               TEXT NOT NULL DEFAULT '',
    created_by          TEXT NOT NULL DEFAULT '',
    estimated_minutes   INTEGER,

    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    started_at          TEXT,
    closed_at           TEXT,
    close_reason        TEXT NOT NULL DEFAULT '',
    closed_by_session   TEXT NOT NULL DEFAULT '',

    lease_expires_at    TEXT,
    heartbeat_at        TEXT,

    due_at              TEXT,
    defer_until         TEXT,

    external_ref        TEXT,
    source_system       TEXT NOT NULL DEFAULT '',
    spec_id             TEXT NOT NULL DEFAULT '',
    metadata            TEXT,

    ephemeral           INTEGER NOT NULL DEFAULT 0,
    no_history          INTEGER NOT NULL DEFAULT 0,
    pinned              INTEGER NOT NULL DEFAULT 0,
    is_template         INTEGER NOT NULL DEFAULT 0,

    wisp_type           TEXT,
    mol_type            TEXT,
    work_type           TEXT,

    content_hash        TEXT NOT NULL DEFAULT '',

    -- Derived cache of the dependency graph, maintained to a fixpoint by
    -- `blocked.rs` inside every mutating transaction. `bd ready` reads this
    -- column instead of walking the graph, which is the only reason it is fast
    -- -- and the reason a stale value is a silent correctness bug rather than a
    -- performance one.
    is_blocked          INTEGER NOT NULL DEFAULT 0,

    -- Derived from `close_reason` via `bd_core::is_failure_close`, written by
    -- whatever Rust code writes `close_reason`. It exists because a
    -- `conditional-blocks` edge must ask "did the target fail?" from inside the
    -- `is_blocked` UPDATE, and the answer is a fuzzy word-list match that lives
    -- in bd-core. Re-encoding that word list in SQL would fork the definition
    -- of failure between two languages; caching the answer keeps bd-core the
    -- only authority.
    close_is_failure    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_issues_ready      ON issues(is_blocked, status);
CREATE INDEX IF NOT EXISTS idx_issues_lease      ON issues(status, lease_expires_at);
CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at);
CREATE INDEX IF NOT EXISTS idx_issues_assignee   ON issues(assignee);
CREATE INDEX IF NOT EXISTS idx_issues_type       ON issues(issue_type);

CREATE TABLE IF NOT EXISTS dependencies (
    issue_id      TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    depends_on_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    type          TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    created_by    TEXT NOT NULL DEFAULT '',
    metadata      TEXT,
    thread_id     TEXT,

    -- The type is part of the key: two beads may legitimately be joined by more
    -- than one kind of edge (a child that also blocks its parent, say).
    PRIMARY KEY (issue_id, depends_on_id, type)
);

-- Both directions are hot. The `is_blocked` recompute walks edges out of an
-- issue; seeding the incremental recompute walks edges into one.
CREATE INDEX IF NOT EXISTS idx_dependencies_issue  ON dependencies(issue_id, type);
CREATE INDEX IF NOT EXISTS idx_dependencies_target ON dependencies(depends_on_id, type);

CREATE TABLE IF NOT EXISTS labels (
    issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    label    TEXT NOT NULL,
    PRIMARY KEY (issue_id, label)
);

CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);

-- Comment ids are UUIDs, and the reason is `bd import`.
--
-- `upsert_comment` keys on the incoming id, which is what makes re-importing a
-- file a no-op instead of a way to duplicate every comment. That only works if
-- an id means the same comment *everywhere*. A workspace-local AUTOINCREMENT
-- does not: two workspaces that have each ever written a comment both hold a
-- comment 1, so importing A's export into B overwrites B's comment with A's
-- text and re-parents it onto A's issue. No error, no conflict — B's comment is
-- simply gone.
--
-- Ids are therefore minted globally unique on insert. Not `bd_core::idgen`:
-- that mints *short readable* ids to be typed at a terminal, and nobody ever
-- types a comment id.
CREATE TABLE IF NOT EXISTS comments (
    id         TEXT PRIMARY KEY,
    issue_id   TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author     TEXT NOT NULL DEFAULT '',
    text       TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Ordering is by time, not by id: a UUID sorts randomly, so an index (and an
-- ORDER BY) on the id would hand back a thread of comments shuffled.
CREATE INDEX IF NOT EXISTS idx_comments_issue ON comments(issue_id, created_at);

-- No foreign key to `issues`, on purpose. The audit trail must outlive the row
-- it describes: `delete_issue` records a `deleted` event, and an ON DELETE
-- CASCADE would erase that event in the same statement that earned it.
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id   TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor      TEXT NOT NULL DEFAULT '',
    old_value  TEXT,
    new_value  TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id, id);

CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
