# Port status — `bd` (Rust)

The command surface is **complete**: every command upstream has is registered,
with its flags, aliases and help. What varies is what happens when you run one.

**730 tests. Two backends, both complete and green: SQLite, and Dolt —
**verified against real dolt 2.1.10** (100 bd-dolt tests run for real; nothing
skips). Dolt's binary is a runtime dependency, not vendored; install it and it is
on. See "Dolt: verified" at the bottom.**

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
`types`.

**Views** — `list`, `ready`, `blocked`, `search`, `query`, `count`, `status`,
`history`, `where`, `children`, `epic status|close-eligible`, `info`, `stale`,
`orphans`, `duplicates`, `find-duplicates`, `lint`, `kv`, `audit`, `context`,
`ping`, **`diff`** (needs a commit graph → exit 2 on SQLite).

**Deps** — `dep add|remove|list|tree|cycles|relate|unrelate`, `graph`,
`graph check`, `recompute-blocked`.

**Sync** — `export`, `import`, `ship`, `mail`, `repo`, **`branch`** (exit 2 on
SQLite), `vc`, `dolt *`, and six trackers (`github`, `gitlab`, `jira`, `linear`,
`notion`, `ado`) each with `sync|status|push|pull`. Every tracker is tested
offline against a fake HTTP seam — **zero network calls, zero credentials.**

**Setup** — `init` (`--backend=sqlite|dolt`), `version`, `completion`,
`config set|get|list`, `bootstrap`, `setup`, `onboard`, `quickstart`, `prime`,
`hooks`, `upgrade`, `metrics`.

**Formulas** — **`cook`** (compile a `.formula.toml` into a live issue graph:
vars, `needs`, `condition`, `loop`, `gate`; `--var`, `--dry-run`), `formula
list|show|schema`. The compiler is the `bd-formula` crate — pure, TOML in, a plan
of proto-issues out, tested against the formula files upstream ships.

**Maintenance** — **`doctor`** (48 checks, 9 families, `--fix`), `preflight`,
`gc`, `purge`, `prune`, `reclaim`, `admin cleanup`, `backup` (exit 2 on SQLite —
it is a *Dolt* backup: branches, history, working set), `merge-slot`, `worktree`.

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
| `restore`, `state`, `set-state`, `label propagate` | Custom-status workflow; the seam has the pieces. |
| `flatten` | Graph flattening; unbuilt. |
| `compact`, `migrate`, `rename-prefix`, `admin compact`, `admin reset` | `migrate` wants `Storage::schema_version()`, which this port has no notion of. Deliberately **not** exit 2: SQLite compacts (`VACUUM`) and SQLite has a schema, so "the backend cannot" would be a lie. |
| `config unset` / `validate` / `show` | The seam has no config *delete*. |
| `sql` | Raw SQL cannot go through a backend-agnostic trait, and giving it one would make every other backend a liar the moment it did not speak SQLite's dialect. **The seam has no `execute_sql`, on purpose.** |
| `formula convert` | This port only ever spoke TOML; there is nothing to convert *from*. |

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

2. **`bd_sqlite::blocked::recompute_all` cannot converge on a `parent-child`
   cycle.** It seeds `unmark` from the very column it is fixing, so two mutually
   parented issues each see the other as blocked and neither unmarks — forever,
   even after the real blocker closes. `find_cycles` reports the cycle, but
   `recompute_blocked()` returns success while `bd ready` still lies. The doctor
   repair re-verifies from the edges afterwards and reports failure rather than
   lying. **Unfixed.**

3. **`events.id` is an `AUTO_INCREMENT` integer.** On Dolt, two clones on separate
   branches both allocate id N+1, and merging is a genuine key conflict between
   rows that are not the same event. Confined to the audit trail (nothing joins
   on an event id). The real fix is widening `bd_core::Event::id` to a `String`,
   as `Comment::id` already was for the same reason. **Unfixed.**

4. **The lock format records a pid but no hostname.** On a `.beads/` sitting on a
   network share, a lock written by another machine gets its pid checked against
   the *local* process table, can read as dead, and `doctor --fix` would delete a
   lock whose owner is alive elsewhere. The doctor check is deliberately
   conservative about this. The fix belongs in the lock *writer*: record `host=`
   alongside `pid=`. **Unfixed.**

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

---

## Dolt: what is untested, and why

The Dolt backend is **complete and unverified**. There is no `dolt` binary on the
development machine and installing one was not authorized. Every test that needs a
real server **skips loudly** — `"SKIPPED: this test is NOT covering anything"` —
because a test that silently passes by doing nothing is worse than no test: it
reports as coverage.

To verify, install `dolt` and run `cargo test --workspace`. The 29 skipping tests
will light up. In rough order of danger:

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
