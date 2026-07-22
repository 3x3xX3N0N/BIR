# Port status — `bd` (Rust)

The command surface is **complete**: every command upstream has is registered,
with its flags, aliases and help. What varies is what happens when you run one.

**746 tests. Two backends: SQLite, complete and green everywhere; Dolt,
complete — and verified wherever a `dolt` binary is present, but on a host
WITHOUT one its 31 integration tests skip LOUDLY (they cover nothing and say
so). Dolt's binary is a runtime dependency, not vendored; install it and those
31 run. See "Dolt: what is untested, and why" at the bottom for exactly which,
and why the skip is loud rather than silent.**

## How to resume

1. `cargo test --workspace` and `cargo clippy --workspace --all-targets`. Both
   must be clean before you start and clean when you stop.
2. Pick a `stub` row. Its handler already exists in `crates/bd-cli/src/commands/`
   and is already wired into the dispatch table; it calls `stub("<name>", ctx)`.
   Replace that call. Code against `Box<dyn Storage>`, never a concrete backend.
3. Move the row here, in the same commit. **This file is the only durable record
   of where the port is**, and it has been wrong before.

Read `OWNERSHIP.md` before running agents in parallel. It is the thing that makes
fan-out safe, and it was written after finding the two shared files that every
agent would otherwise have had to edit.

## Exit codes — the contract with any script or agent driving `bd`

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | Real failure: not found, bad input, I/O, no workspace, `--readonly` refusal, a `doctor` that found something. |
| `2` | **Capability gap.** The command is built; this workspace's backend cannot serve it. `bd branch` on SQLite. Not a bug and not a to-do — SQLite genuinely has no commit graph. |
| `64` | **Not ported yet.** Registered here, exists upstream, unbuilt. |

`2` and `64` must never be conflated: one is permanent and honest, the other is a
to-do. Under `--json` they are distinguishable without parsing prose:

```json
{"error":"not_implemented","command":"cook","see":"PORT_STATUS.md"}
{"error":"unsupported_backend","command":"branch","backend":"sqlite","requires":"dolt","capability":"version_control"}
```

A **missing `dolt` binary is exit 1, not 2** — the backend is perfectly capable;
the machine is one download short. Exit 2 would tell the user to give up on
something an install fixes.

---

## Done

**Issues** — `create` (`new`), `q`, `show`, `update`, `close`, `reopen`, `delete`,
`edit`, `assign`, `unclaim`, `priority`, `defer`/`undefer`, `promote`, `rename`,
`tag`, `note`, `duplicate`, `supersede`, `link`, `heartbeat`, `batch`,
`label add|remove|list|list-all`, `comment`, `comments list|add`, `statuses`,
`types`, `state`, `set-state`, `label propagate`.

**Views** — `list`, `ready`, `blocked`, `search`, `query`, `count`, `status`,
`history`, `where`, `children`, `epic status|close-eligible`, `info`, `stale`,
`orphans`, `duplicates`, `find-duplicates`, `lint`, `context`,
`ping`, **`diff`** (needs a commit graph → exit 2 on SQLite). (`kv` and `audit`
are NOT here — they are stubs; see the exit-64 table.)

**Deps** — `dep add|remove|list|tree|cycles|relate|unrelate`, `graph`,
`graph check`, `recompute-blocked`.

**Sync** — `export`, `import`, `ship`, `mail`, **`branch`** (exit 2 on
SQLite), and six trackers (`github`, `gitlab`, `jira`, `linear`,
`notion`, `ado`) each with `sync|status|push|pull`. Every tracker is tested
offline against a fake HTTP seam — **zero network calls, zero credentials.**
(`vc`, `dolt *`, `federation *` and `repo` are NOT here — they are stubs; see
the exit-64 table. `branch` is the only version-control command with a real
implementation.)

**Setup** — `init` (`--backend=sqlite|dolt`), `version`, `completion`,
`config set|get|list`, `bootstrap`, `setup`, `onboard`, `quickstart`, `prime`,
`hooks`, `upgrade`, `metrics`.

**Formulas** — **`cook`** (compile a `.formula.toml` into a live issue graph:
vars, `needs`, `condition`, `loop`, `gate`; `--var`, `--dry-run`), `formula
list|show|schema`. The compiler is the `bd-formula` crate — pure, TOML in, a plan
of proto-issues out, tested against the formula files upstream ships.

**Maintenance** — **`doctor`** (48 checks, 9 families, `--fix`), `preflight`,
`gc`, `purge`, `prune`, `reclaim`, `admin cleanup`, **`migrate`** (stamps the
schema version; refuses a downgrade). (`backup`, `merge-slot` and `worktree`
are NOT here — they are stubs; see the exit-64 table.)

**Advanced** — **`mol`** (`wisp`, `seed --var`, `pour`, `show`, `ready`,
`current`, `stale`, `burn`, `squash`, `bond`), **`gate`** (`create`, `show`,
`list`, `check`, `resolve`), **`swarm validate`**, **`swarm list`**, **`rules
audit`**, `remember`/`recall`/`memories`/`forget`, `todo`, `human`. Molecules and
gates are just issues (type `Molecule`/`Gate`); wisps set `ephemeral`. `mol
seed`/`pour` cook a formula into a tracked container.

## Stubs — exit 64

| Command | Blocked on |
| --- | --- |
| `mol distill` | Needs `--var` and an `--output` path (flagless `Distill { id }` cannot supply them) to emit a *parameterized* `.formula.toml`. A literal, un-reusable formula is the one thing distill exists not to produce. |
| `swarm create` / `swarm status` | Ride on a `mol_type = swarm` molecule linked to an epic, and a `convoy`-type formula that `bd-formula` does not cook yet. No honest substrate. |
| `rules compact` | It merges rule files and *deletes the sources*. The flagless `Compact` variant offers no `--dry-run`/`--group`/`--auto`, so it cannot be driven safely — refusing beats a reckless default. |
| `restore` | Needs a *soft* delete; this port's `delete` is a hard cascade, so there is nothing to restore. |
| `flatten` | Graph flattening; unbuilt. |
| `compact`, `rename-prefix`, `admin compact`, `admin reset` | `compact` wants `Storage::compact()`. Deliberately **not** exit 2: SQLite compacts (`VACUUM`), so "the backend cannot" would be a lie. (`migrate` graduated: `Storage::schema_version()`/`migrate()` exist now, every database carries a version stamp, and a mismatched stamp is refused at open with the fix named — see "Known gaps and decisions".) |
| `config unset` / `validate` / `show` | The seam has no config *delete*. |
| `sql` | Raw SQL cannot go through a backend-agnostic trait, and giving it one would make every other backend a liar the moment it did not speak SQLite's dialect. **The seam has no `execute_sql`, on purpose.** |
| `formula convert` | This port only ever spoke TOML; there is nothing to convert *from*. |
| `vc merge` / `vc commit` / `vc status` | Needs a write path through `VersionControl`. Exit 2 on SQLite (no commit graph), exit 64 on Dolt. |
| `dolt *` (11 subcommands + `dolt remote add`/`list`/`remove`) | The `dolt sql-server`/branch/remote plumbing lives in `bd-dolt`; the CLI dispatch is not wired. Exit 2 on SQLite, exit 64 on Dolt. |
| `federation sync` / `status` / `add-peer` / `remove-peer` / `list-peers` | Peer registry has no home in `Config`. Exit 2 on SQLite, exit 64 on Dolt. |
| `repo add` / `remove` / `list` / `sync` | No `repos:` field in `Config`, and the seam hands out exactly one store. Exit 64 unconditionally (no cap gate). |
| `kv set` / `get` / `clear` / `list` | The seam has no key/value store; `get_config`/`set_config` are the config table, not a KV. |
| `audit record` / `label` | Nothing on the seam writes an `Event`, and `EventType` has no free-text variant. |
| `backup status` / `init` / `sync` / `remove` / `restore` | Exit 2 on SQLite (no commit graph — it is a *Dolt* backup: branches, history, working set). On a Dolt workspace the work is real and simply not wired, so exit 64. |
| `merge-slot create` / `check` / `acquire` / `release` | Needs an atomic read-modify-write on metadata (`Storage::swap_metadata`), which the seam does not expose. |
| `worktree create` / `list` / `remove` / `info` | A *git* worktree subsystem; this port has no git module. |

---

## Known gaps and decisions

Things a future maintainer would otherwise rediscover the hard way.

1. **`is_blocked` is a denormalized cache, maintained to a *fixpoint*.** `bd ready`
   reads the column; it does not traverse the graph. Blocked-ness propagates
   transitively down `parent-child` edges, so a single UPDATE pass is *wrong*
   (`bd-sqlite/src/blocked.rs` has a test proving it). **Anything that lands rows
   without going through a local write path — a merge, a pull, an import — leaves
   the cache stale by definition, and `bd ready` then hands out the wrong work
   with no error, no exit code and no log line.** It is the worst failure this
   system has and it is invisible. `bd doctor`'s `blocked-cache` check exists for
   exactly this; it re-derives the value from the edges and diffs it against the
   stored column, and it is mutation-tested.

2. **`recompute_all` is a *least* fixpoint** (reset all to unblocked, then
   mark-only). This is what makes it correct on a `parent-child` **cycle**, which
   a merge or import can land even though write paths reject them: from an
   all-unblocked base a cycle with no external blocker never marks itself, and
   monotonic marking always converges. The incremental path (`recompute_affected`,
   on an already-correct acyclic graph) keeps its mark+unmark repair. *(Was a bug:
   the old full recompute used mark+unmark and a cycle trapped it in a
   stable-but-wrong "both blocked forever" state. Fixed; test:
   `a_parent_child_cycle_is_recomputed_correctly_not_just_without_spinning`.)*

3. **`events.id` is a client-minted UUID**, not an autoincrement integer — so a
   Dolt merge between two clones is a clean union rather than a primary-key
   collision between two different events. Event listing orders by `created_at`;
   a multi-event mutation stamps its terminal event one microsecond later so the
   order is total and deterministic. *(Was a bug; fixed.)*

4. **The lock sweeper refuses to delete a lock from another machine.** A lock may
   carry `host=`; on a shared/network `.beads/` a foreign host's pid names a
   process in a table this machine cannot see, so such a lock is `Undetermined`
   (reported, never deleted) regardless of the local pid probe. A lock with no
   `host=` keeps pid-only behaviour. *(Was a bug: a foreign lock whose pid was
   dead locally read as orphaned and `doctor --fix` would delete it. Fixed in the
   reader; a future beads lock-writer should record `host=`.)*

5. **`list_issues` does not hydrate relations.** Only `get_issue` returns labels
   and edges. So `bd list --json` carries no `labels` array while `bd show --json`
   does. `export` pays for the hydration (a backup that loses labels is not a
   backup); listings deliberately do not, because it would be N+1 on every
   `bd ready`.

6. **`import` restores comments by upsert, keyed on the incoming id.**
   `add_comment` would mint a fresh id and stamp *the importer* as the author, so
   re-importing a file would duplicate every comment and misattribute all of them.

7. **The default `bd list` view enumerates statuses.** `IssueFilter` has no "not
   closed" predicate, only a status set, so a workspace's *custom* statuses are
   not in the default view. Ask for them by name, or pass `--all`.

8. **The binary runs on a 16 MiB worker thread.** clap's derive builds the
   ~120-subcommand tree in one enormous stack frame, which overflows Windows' 1 MiB
   main-thread stack in a debug build. This was a real `STATUS_STACK_OVERFLOW`
   before `main` ran, not a theoretical one.

9. **Never put `install`, `setup`, `update` or `patch` in a `tests/` filename.**
   Cargo names the test binary after the file, and Windows' installer-detection
   heuristic auto-elevates any executable whose name contains those words. This is
   why `wiring_cli.rs` is not called `setup_cli.rs`, and why the doctor family is
   `runtime.rs` and not `install.rs`.

10. **Capability probes work without an open store.** `bd branch` on SQLite never
    opens the database: the locator is the authority on which engine owns a
    workspace, and `Backend::has_commit_graph()` answers the question. Opening a
    database for a command that will refuse anyway turns a clean exit 2 into a
    spurious exit 1.

11. **Every database carries a schema version stamp, checked at open.**
    `bd_storage::SCHEMA_VERSION` (currently 1) is stamped by `bd init` — SQLite's
    `PRAGMA user_version`, Dolt's `schema_meta` table (a *versioned* table, so
    the stamp rides along on clone/push/pull and a migration on one machine
    announces itself to every clone). `Ctx::store()` refuses a mismatch with the
    fix named: behind → "run `bd migrate`", ahead → "upgrade bd". A raw stamp of
    0 is a pre-versioning database and reads as v1 (exactly one schema ever
    shipped unversioned), so 0.1.0 workspaces keep working with no ceremony —
    the doctor nags once to stamp. `migrate` and `doctor` open **unchecked**
    (`Ctx::store_unchecked`): one exists to fix what the gate refuses, the other
    to examine it. This was built *before* the first schema change on purpose —
    upstream ships schema changes with no recorded version, and every upgrade
    there is a manual, coordinated event (pick a master, migrate, everyone else
    re-bootstraps; version skew between machines corrupts sync).

12. **Every SQLite write transaction is `BEGIN IMMEDIATE`** (`write_tx()` in
    `bd-sqlite/src/store.rs`). sqlx's default deferred `BEGIN` takes a read
    snapshot at the opening SELECT; the later lock upgrade fails **instantly**
    with `SQLITE_BUSY_SNAPSHOT` when any other process committed in between —
    `busy_timeout` does not apply to it, and six concurrent agents produced
    "database is locked" in under a second. Found by the cross-process
    contention test (`contention_cli.rs`, which runs the README's claim loop at
    maximal collision), not by reasoning. Do not "simplify" a write path back
    to `pool.begin()`.

13. **Dolt remote operations run under a deadline.** `BEADS_REMOTE_TIMEOUT`
    seconds (default 600, `0` disables); on expiry, an honest error that names
    the setting. Upstream users report multi-hour `dolt fetch` calls with zero
    feedback — a bounded wait that says *why* beats unbounded silence. A
    malformed value is an error, not a silent fallback to the default.

---

## Dolt: what is untested, and why

The Dolt backend is **complete, and verified wherever a `dolt` binary is
present** — install `dolt` and its integration tests run for real. On a host
WITHOUT one, every test that needs a real server **skips loudly** —
`"SKIPPED: this test is NOT covering anything"` — because a test that silently
passes by doing nothing is worse than no test: it reports as coverage.

**There are exactly 31 such tests** (`require_dolt!` in `bd-dolt/src/lib.rs`: 5
in `tests/server.rs`, 5 in `tests/vc.rs`, and 21 via the `fixture!` macro in
`tests/store.rs` — every integration test in `bd-dolt/tests/`). Symmetrically,
one test in `bd-cli/tests/doctor_dolt.rs` skips only when dolt IS present (it
asserts the *absent*-binary refusal). So "nothing skips" is never quite true:
without dolt, 31 skip; with dolt, that one does. Install `dolt` and run
`cargo test --workspace`; the 31 light up, in rough order of danger:

1. **The merge test.** Two clones diverge, both mutate the graph, merge — and
   `bd ready` must still be correct. Note that the *obvious* version of this test
   cannot fail: `is_blocked` is itself a versioned column, so a branch that closes
   a blocker has already set it correctly and the merge just carries that across.
   The real test needs a state **neither side ever computed**: base has `A`
   blocking `B`; one branch closes `A` (correctly setting `B` free); the other adds
   a new open blocker `C` (correctly leaving `B` blocked). Dolt merges the cell to
   *free*. Every local step was right and the answer is wrong. Only a full
   `recompute_blocked()` catches it — which `merge`, `pull` and `resolve_conflicts`
   all do, unconditionally.
2. **`checkout` on a connection pool.** The checked-out branch is *session* state
   in `dolt sql-server`. The pool is pinned to **one** connection for this reason;
   with more, a write could land on a sibling connection still on the old branch —
   silently writing issues to the wrong branch. Verify the pin actually holds.
3. **Does Dolt accept the DDL at all** — `DATETIME(6)`, `COLLATE=utf8mb4_0900_bin`,
   table-level FKs with `ON DELETE CASCADE`, inline `KEY`, `AUTO_INCREMENT`.
4. **`WITH RECURSIVE` inside an `IN (…)` subquery** (the `--parent` transitive
   filter) — the highest-risk untested query.
5. **The fixpoint, end to end.** MySQL rejects `UPDATE t … WHERE EXISTS (SELECT …
   FROM t)` (error 1093), which is exactly the shape of the SQLite fixpoint, so it
   was rewritten as select-then-update-by-id. Verified as *generated SQL*, never as
   behaviour.
6. **`a_recompute_never_bumps_updated_at`** — the merge-conflict-prevention
   invariant. Asserted statically today; the runtime assertion has never run.
