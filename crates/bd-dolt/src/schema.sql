-- Beads schema, MySQL/Dolt dialect.
--
-- The same tables and the same column names as `bd-sqlite/src/schema.sql` --
-- every query and every domain type is shared, so a renamed column here is a
-- bug there. What differs is only what the dialect forces to differ, and each
-- of those is a real trap rather than a matter of taste:
--
--   * **Every table has an explicit PRIMARY KEY.** Dolt is a versioned store.
--     A table without a key cannot be diffed or three-way merged row-wise; Dolt
--     will accept one and then behave as though every row were rewritten on
--     every commit. `events` therefore carries a surrogate key rather than
--     leaning on a rowid.
--
--   * **Timestamps are `DATETIME(6)`, never `TIMESTAMP` and never text.**
--     Three reasons, in ascending order of nastiness:
--       - sqlx's `DateTime<Utc>` is only `compatible()` with `DATETIME` and
--         `TIMESTAMP` columns, so the SQLite trick of storing RFC-3339 text
--         would make every `try_get::<DateTime<Utc>>` fail at runtime;
--       - a bare `DATETIME` truncates to whole seconds, which would collapse
--         lease expiry and the `(created_at, id)` sort tiebreak onto a
--         one-second grid — hence the explicit `(6)`;
--       - `TIMESTAMP` is converted to/from UTC using the *session* time zone and
--         (with `explicit_defaults_for_timestamp` off) the first such column in
--         a table silently acquires `DEFAULT CURRENT_TIMESTAMP ON UPDATE
--         CURRENT_TIMESTAMP`. That auto-update clause is precisely the thing
--         `blocked.rs` must not have: it would stamp local wall-clock time onto
--         `updated_at` every time the derived `is_blocked` cache flipped, and on
--         a *version-controlled* table that means two clones conflict on a
--         column neither of them edited. `DATETIME` has no such clause, and
--         nothing below adds one. Its absence is load-bearing. Do not "fix" it.
--
--   * **Collation is `utf8mb4_0900_bin`, declared, on every table.** MySQL's
--     default (`utf8mb4_0900_ai_ci`) is case- *and* accent-insensitive, so
--     `assignee = 'Alice'` would quietly match a row holding `'alice'` --
--     while the same query on SQLite would not. `bd-query` has property tests
--     asserting string equality is exact, byte-for-byte; `_bin` is the only
--     collation that is actually byte-for-byte. `_as_cs` still applies Unicode
--     collation weights and is not the same predicate.
--     The consequence is that `LIKE` also becomes case-*sensitive* here, where
--     SQLite's is ASCII-case-insensitive -- so the text search in `store.rs`
--     lowercases both sides explicitly rather than relying on the collation.
--
--   * **Foreign keys are declared at table level.** MySQL *parses and then
--     silently ignores* an inline column-level `REFERENCES ... ON DELETE
--     CASCADE`. Written the SQLite way, the cascade would simply not exist, and
--     `delete_issue` would leave orphaned edges, labels and comments behind with
--     no error at all. This also forces every FK column to be `VARCHAR` rather
--     than `TEXT`: a foreign key needs a full-column index, and a `TEXT` column
--     can only be indexed by prefix.
--
--   * **Indexes are declared inside `CREATE TABLE`.** MySQL has no
--     `CREATE INDEX IF NOT EXISTS`, so the SQLite spelling would fail on the
--     second open of an existing workspace. Inline `KEY` clauses inherit the
--     idempotence of `CREATE TABLE IF NOT EXISTS`.
--
--   * **`TEXT`/`LONGTEXT` columns carry no `DEFAULT`.** MySQL rejects it
--     (error 1101). Every insert in `store.rs` names every column, so the
--     defaults were decorative anyway -- and a strict-mode error on an omitted
--     NOT NULL column is a loud failure, which is the one we want.
--
-- Kept from SQLite, deliberately: there is no `wisp_*` shadow twin. Upstream
-- duplicates every table so ephemeral beads can live outside the commit graph.
-- Here an ephemeral bead is an ordinary row with `ephemeral = 1`, and `bd gc`
-- reaps it. The twin would double every write path, every recompute and every
-- migration to save writing a `WHERE` clause.

CREATE TABLE IF NOT EXISTS issues (
    id                  VARCHAR(255) NOT NULL,

    -- Free-form and unbounded. `validate()` caps a locally-authored title at
    -- MAX_TITLE_LEN, but `validate_for_import()` deliberately does not -- a peer's
    -- long title is their business, and a VARCHAR here would turn importing it
    -- into a strict-mode error.
    title               LONGTEXT     NOT NULL,
    description         LONGTEXT     NOT NULL,
    design              LONGTEXT     NOT NULL,
    acceptance_criteria LONGTEXT     NOT NULL,
    notes               LONGTEXT     NOT NULL,

    status              VARCHAR(32)  NOT NULL DEFAULT 'open',
    priority            INT          NOT NULL DEFAULT 2,
    issue_type          VARCHAR(64)  NOT NULL DEFAULT 'task',

    assignee            VARCHAR(255) NOT NULL DEFAULT '',
    owner               VARCHAR(255) NOT NULL DEFAULT '',
    created_by          VARCHAR(255) NOT NULL DEFAULT '',
    estimated_minutes   INT              NULL,

    created_at          DATETIME(6)  NOT NULL,
    updated_at          DATETIME(6)  NOT NULL,
    started_at          DATETIME(6)      NULL,
    closed_at           DATETIME(6)      NULL,
    close_reason        LONGTEXT     NOT NULL,
    closed_by_session   VARCHAR(255) NOT NULL DEFAULT '',

    lease_expires_at    DATETIME(6)      NULL,
    heartbeat_at        DATETIME(6)      NULL,

    due_at              DATETIME(6)      NULL,
    defer_until         DATETIME(6)      NULL,

    external_ref        VARCHAR(255)     NULL,
    source_system       VARCHAR(64)  NOT NULL DEFAULT '',
    spec_id             VARCHAR(255) NOT NULL DEFAULT '',
    metadata            LONGTEXT         NULL,

    -- TINYINT(1), not INT: this is what sqlx encodes a Rust `bool` as, and what
    -- it expects to decode one from.
    ephemeral           TINYINT(1)   NOT NULL DEFAULT 0,
    no_history          TINYINT(1)   NOT NULL DEFAULT 0,
    pinned              TINYINT(1)   NOT NULL DEFAULT 0,
    is_template         TINYINT(1)   NOT NULL DEFAULT 0,

    wisp_type           VARCHAR(32)      NULL,
    mol_type            VARCHAR(32)      NULL,
    work_type           VARCHAR(32)      NULL,

    content_hash        VARCHAR(64)  NOT NULL DEFAULT '',

    -- Derived cache of the dependency graph, maintained to a fixpoint by the
    -- `blocked` section of `store.rs` inside every mutating transaction.
    -- `bd ready` reads this column instead of walking the graph, which is the
    -- only reason it is fast -- and the reason a stale value is a silent
    -- correctness bug rather than a performance one.
    is_blocked          TINYINT(1)   NOT NULL DEFAULT 0,

    -- Derived from `close_reason` via `bd_core::is_failure_close`, written by
    -- whatever Rust code writes `close_reason`. It exists because a
    -- `conditional-blocks` edge must ask "did the target fail?" from inside the
    -- `is_blocked` recompute, and the answer is a fuzzy word-list match that
    -- lives in bd-core. Re-encoding that word list in SQL would fork the
    -- definition of failure between two languages; caching the answer keeps
    -- bd-core the only authority.
    close_is_failure    TINYINT(1)   NOT NULL DEFAULT 0,

    PRIMARY KEY (id),
    KEY idx_issues_ready      (is_blocked, status),
    KEY idx_issues_lease      (status, lease_expires_at),
    KEY idx_issues_created_at (created_at),
    KEY idx_issues_assignee   (assignee),
    KEY idx_issues_type       (issue_type)
) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_bin;

CREATE TABLE IF NOT EXISTS dependencies (
    issue_id      VARCHAR(255) NOT NULL,
    depends_on_id VARCHAR(255) NOT NULL,
    -- MAX_DEPENDENCY_TYPE_LEN is 50. Sized to fit it and no wider: this column
    -- is a third of the primary key, and MySQL caps an index key at 3072 bytes
    -- (utf8mb4 charges 4 per character).
    `type`        VARCHAR(64)  NOT NULL,
    created_at    DATETIME(6)  NOT NULL,
    created_by    VARCHAR(255) NOT NULL DEFAULT '',
    metadata      LONGTEXT         NULL,
    thread_id     VARCHAR(255)     NULL,

    -- The type is part of the key: two beads may legitimately be joined by more
    -- than one kind of edge (a child that also blocks its parent, say).
    PRIMARY KEY (issue_id, depends_on_id, `type`),

    -- Both directions are hot. The `is_blocked` recompute walks edges out of an
    -- issue; seeding the incremental recompute walks edges into one.
    KEY idx_dependencies_issue  (issue_id, `type`),
    KEY idx_dependencies_target (depends_on_id, `type`),

    CONSTRAINT fk_dependencies_issue
        FOREIGN KEY (issue_id)      REFERENCES issues (id) ON DELETE CASCADE,
    CONSTRAINT fk_dependencies_target
        FOREIGN KEY (depends_on_id) REFERENCES issues (id) ON DELETE CASCADE
) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_bin;

CREATE TABLE IF NOT EXISTS labels (
    issue_id VARCHAR(255) NOT NULL,
    label    VARCHAR(255) NOT NULL,

    PRIMARY KEY (issue_id, label),
    KEY idx_labels_label (label),

    CONSTRAINT fk_labels_issue
        FOREIGN KEY (issue_id) REFERENCES issues (id) ON DELETE CASCADE
) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_bin;

-- Comment ids are UUIDs, and the reason is `bd import`.
--
-- `upsert_comment` keys on the incoming id, which is what makes re-importing a
-- file a no-op instead of a way to duplicate every comment. That only works if
-- an id means the same comment *everywhere*. A workspace-local AUTO_INCREMENT
-- does not: two workspaces that have each ever written a comment both hold a
-- comment 1, so importing A's export into B overwrites B's comment with A's text
-- and re-parents it onto A's issue. No error, no conflict -- B's comment is
-- simply gone. On a backend that *merges*, that stops being a hypothetical.
--
-- Ids are therefore minted globally unique on insert, client-side. Client-side
-- also because MySQL has no `RETURNING`: the SQLite store could have asked the
-- database for the id it just assigned, and here that option does not exist.
CREATE TABLE IF NOT EXISTS comments (
    id         VARCHAR(255) NOT NULL,
    issue_id   VARCHAR(255) NOT NULL,
    author     VARCHAR(255) NOT NULL DEFAULT '',
    `text`     LONGTEXT     NOT NULL,
    created_at DATETIME(6)  NOT NULL,

    PRIMARY KEY (id),
    -- Ordering is by time, not by id: a UUID sorts randomly, so an index (and an
    -- ORDER BY) on the id would hand back a thread of comments shuffled.
    KEY idx_comments_issue (issue_id, created_at),

    CONSTRAINT fk_comments_issue
        FOREIGN KEY (issue_id) REFERENCES issues (id) ON DELETE CASCADE
) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_bin;

-- No foreign key to `issues`, on purpose. The audit trail must outlive the row
-- it describes: `delete_issue` records a `deleted` event, and an ON DELETE
-- CASCADE would erase that event in the same statement that earned it.
--
-- AUTO_INCREMENT here is the one place this schema is knowingly weaker than the
-- rest of it. `bd_core::Event::id` is an `i64`, so there is no client-minted
-- UUID to reach for, and Dolt needs *some* primary key. The cost is that two
-- clones working on separate branches both hand out event id N+1, and merging
-- them is a genuine key conflict on rows that are not actually the same event.
-- It is confined to the audit trail -- no query joins on an event id and
-- `list_events` only orders by it -- so a conflict here loses history ordering,
-- not work. Fixing it properly means widening `Event::id` to a string in
-- bd-core, which is frozen.
CREATE TABLE IF NOT EXISTS events (
    -- A client-minted UUID, NOT AUTO_INCREMENT. Two clones on separate branches
    -- would each allocate the same next integer for *different* events, and a
    -- dolt merge would collide them on the primary key — corrupting the audit
    -- trail exactly where version control is supposed to help. A UUID is the same
    -- in every clone, so a merge is a clean union.
    id         VARCHAR(36)  NOT NULL,
    issue_id   VARCHAR(255) NOT NULL,
    event_type VARCHAR(64)  NOT NULL,
    actor      VARCHAR(255) NOT NULL DEFAULT '',
    old_value  LONGTEXT         NULL,
    new_value  LONGTEXT         NULL,
    created_at DATETIME(6)  NOT NULL,

    PRIMARY KEY (id),
    -- created_at, not id: a UUID does not sort chronologically.
    KEY idx_events_issue (issue_id, created_at)
) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_bin;

-- `key` is a *reserved word* in MySQL and must be quoted here and in every
-- statement that names it. SQLite accepts it bare, so the SQLite spelling of
-- `SELECT value FROM config WHERE key = ?` is a syntax error against Dolt.
CREATE TABLE IF NOT EXISTS config (
    `key` VARCHAR(255) NOT NULL,
    value LONGTEXT     NOT NULL,

    PRIMARY KEY (`key`)
) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_bin;

-- The schema version stamp (SQLite keeps it in `PRAGMA user_version`; MySQL
-- has no equivalent, so here it is a table). A *versioned* table on purpose:
-- it rides along on clone, push, pull and merge, so a database migrated on one
-- machine tells every other machine — that is the whole handshake. Not a
-- `config` row, because `bd config set` can reach those and the stamp must not
-- be editable by accident. One row, id = 1; an empty table means the database
-- predates version stamping (read as v1 — the only shape that ever shipped
-- unversioned).
CREATE TABLE IF NOT EXISTS schema_meta (
    id      TINYINT      NOT NULL,
    version INT UNSIGNED NOT NULL,

    PRIMARY KEY (id)
) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_bin;
