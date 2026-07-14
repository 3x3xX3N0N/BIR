# Port status — `bd` CLI (Rust)

The command surface is **complete**: every command upstream has is registered,
with its flags, aliases, and help. What varies is what happens when you run one.

## How to resume

1. `cargo build` — the binary must always compile. `bd --help` prints the whole
   map, grouped by family.
2. Pick a `stub` row below. Its handler is already wired into
   `crates/bd-cli/src/commands/` and named in the dispatch table
   (`commands/mod.rs`); it currently calls `stub("<name>", ctx)`. Replace that
   call with the real implementation, against `Box<dyn Storage>` — never against
   a concrete backend.
3. Move the row to `done` here, in the same commit. This table is the only
   durable record of where the port is.

Rules that are not negotiable (they come from the storage seam, and every one of
them was a real bug upstream):

- Code against `bd_storage::Storage`. The only two places allowed to name a
  concrete backend are `context.rs::open_store` and `commands/setup.rs::init`.
- The backend comes from the locator on disk, never from a flag or an env var.
  `--backend` exists only on `bd init`.
- A capability (`version_control`, `remote`, `history`) may make a core command
  *better*. It may never make one *possible*.

## Exit codes

This is the contract with any script or agent driving `bd`, and the reason the
port is legible from the outside:

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | Real failure: not found, bad input, I/O, no workspace, `--readonly` refusal, bad usage. |
| `2` | **Capability gap.** The command is built, but this workspace's backend cannot serve it (`bd branch` on SQLite). Not a bug — SQLite genuinely has no commit graph. |
| `64` | **Not ported yet.** The command exists upstream and is registered here, but this port has not built it. |

`2` and `64` must never be conflated: one is permanent and honest, the other is a
to-do. Under `--json` they are distinguishable without parsing prose:

```json
{"error":"not_implemented","command":"gc","see":"PORT_STATUS.md"}
{"error":"unsupported_backend","command":"branch","backend":"sqlite","requires":"dolt","capability":"version_control"}
```

Usage errors exit `1`, not clap's default `2` — `2` is reserved for the case
above.

## Legend

- **done** — implemented and exercised by tests.
- **stub** — registered, parses, helps; exits 64.
- **needs-dolt** — implemented as a capability probe; exits 2 on a SQLite
  workspace with an honest message. Will work when the Dolt backend lands.

---

## Issues

| Command | Status | Notes |
| --- | --- | --- |
| `create` (alias `new`) | done | `-d -p -t -a -l --design --acceptance --notes --defer-until --due --deps <id:type> --estimate`. Mints the id via `store.next_id`. |
| `q` | done | Quick capture: prints only the id, so `ID=$(bd q "...")` works. |
| `show` (alias `view`) | done | Hydrates edges (both directions) and comments. One id → one JSON object, not a 1-element array. |
| `update` | done | `--claim` (+`--lease`, default `claim.lease` = 1h) and every patchable field. |
| `close` (alias `done`) | done | `--reason` is data: `conditional-blocks` edges read it. |
| `reopen` | done | |
| `delete` | done | |
| `assign` | done | |
| `unclaim` | done | `store.release_claim` |
| `priority` | done | |
| `label add` | done | |
| `label remove` | done | |
| `label list` | done | Reads `issue.labels` — the seam has no per-issue label getter. |
| `label list-all` | done | |
| `label propagate` | stub | |
| `comment` | done | |
| `comments list` | done | |
| `comments add` | done | |
| `edit` | stub | Needs `$EDITOR` round-tripping. |
| `restore` | stub | |
| `rename` | stub | |
| `tag` | stub | |
| `note` | stub | Append-to-notes is a read-modify-write; needs a story for the race. |
| `defer` / `undefer` | stub | `IssuePatch` cannot express *clearing* `defer_until` (`None` means "leave alone"), so `undefer` needs a seam change. Deliberately not half-built. |
| `duplicate` | stub | |
| `supersede` | stub | |
| `link` | stub | `dep add --type` covers the same ground today. |
| `heartbeat` (alias `hb`) | stub | `store.renew_claim` is right there; the open question is what it does to `heartbeat_at`. |
| `state` / `set-state` | stub | Custom-status workflow. |
| `statuses` / `types` | stub | |
| `promote` | stub | |
| `batch` | stub | |

## Views

| Command | Status | Notes |
| --- | --- | --- |
| `list` | done | Defaults to every status except closed; `--all` includes closed. |
| `ready` | done | `--limit --priority --assignee --type --label --sort` |
| `blocked` | done | |
| `search` | done | Substring over title/description, pushed into SQL. |
| `query` | done | `bd_query::parse`; uses `as_filter()` when the DB can answer alone, else `filter_hint()` + `matches()` in memory. |
| `count` | done | |
| `status` (alias `stats`) | done | |
| `history` | done | The audit trail (`store.list_events`) — core, works on every backend. |
| `where` | done | Workspace paths, backend, actor. Needs no database, which is what makes it useful when the database is the problem. |
| `diff` | needs-dolt | Diffing refs is `HistoryViewer`; a backend with no commit graph has no refs. |
| `children` | stub | |
| `epic status` / `epic close-eligible` | stub | |
| `info` | stub | |
| `stale` | stub | |
| `orphans` | stub | |
| `duplicates` | stub | |
| `find-duplicates` (alias `find-dups`) | stub | |
| `lint` | stub | |
| `sql` | stub | |
| `kv set/get/clear/list` | stub | |
| `audit record/label` | stub | |
| `context` | stub | |
| `ping` | stub | |

## Deps

| Command | Status | Notes |
| --- | --- | --- |
| `dep add` | done | `--type` (default `blocks`). |
| `dep remove` (alias `rm`) | done | |
| `dep list` | done | Both directions. |
| `dep tree` | done | ASCII tree, `--depth`. Iterative, not recursive: the graph is not guaranteed acyclic and a cycle must not blow the stack. Repeated nodes are marked, not re-expanded. |
| `dep cycles` | done | |
| `dep relate` / `dep unrelate` | stub | |
| `graph` / `graph check` | stub | |
| `flatten` | stub | |
| `recompute-blocked` | done | |

## Sync

| Command | Status | Notes |
| --- | --- | --- |
| `export` | done | JSONL, one object per line, `_type: "issue"` discriminator, `bd_core::Issue` field names. Re-reads each issue with `get_issue` so labels/edges/comments survive (see gaps). `-o <file>`. |
| `import` | done | JSONL upsert, two passes so forward edge references work. Re-import is a no-op. Runs `recompute_blocked` at the end — a bulk upsert lands rows no single write path saw in order. `--dry-run`. |
| `branch` | needs-dolt | |
| `vc merge/commit/status` | needs-dolt | |
| `dolt show/set/test/commit/push/pull/start/stop/status/killall/clean-databases` | needs-dolt | |
| `dolt remote add/list/remove` | needs-dolt | |
| `federation sync/status/add-peer/remove-peer/list-peers` | stub | |
| `repo add/remove/list/sync` | stub | |
| `ado`, `jira`, `linear`, `github`, `gitlab`, `notion` (each `sync/status/push/pull`) | stub | 24 leaves, one shared `TrackerCmd`. |
| `mail` | stub | |
| `ship` | stub | |

## Setup

| Command | Status | Notes |
| --- | --- | --- |
| `init` | done | `--prefix` (default: derived from the directory name), `--backend` (init-only, the one place a flag may pick an engine), `--force`. Writes `.beads/config.yaml`. `--backend=dolt` exits 64. |
| `version` | done | No workspace needed. |
| `completion <shell>` | done | `clap_complete`; no workspace needed. |
| `config set/get/list` | done | Store-backed. |
| `config unset/validate/show` | stub | The seam has no config delete. |
| `bootstrap`, `setup`, `onboard`, `quickstart`, `prime` | stub | All run without a workspace. |
| `hooks install/uninstall/list/run` | stub | |
| `upgrade status/review/ack` | stub | |
| `metrics on/off/example` | stub | |

## Maintenance

| Command | Status |
| --- | --- |
| `doctor`, `preflight` | stub (run without a workspace) |
| `gc`, `purge`, `prune`, `compact` | stub |
| `backup status/init/sync/remove/restore` | stub |
| `admin cleanup/compact/reset` | stub |
| `migrate`, `rename-prefix`, `reclaim` | stub |
| `worktree create/list/remove/info` | stub |
| `merge-slot create/check/acquire/release` | stub |

## Advanced

| Command | Status |
| --- | --- |
| `mol bond/burn/current/distill/ready/seed/show/squash/stale/pour/wisp` | stub |
| `formula list/show/convert/schema` | stub |
| `cook` | stub |
| `swarm validate/status/create/list` | stub |
| `gate list/create/show/resolve/check` | stub |
| `rules audit/compact` | stub |
| `todo add/list/done` | stub |
| `human list/respond/dismiss/stats` | stub |
| `remember`, `memories`, `forget`, `recall` | stub |

---

## Known gaps and decisions

Things a future maintainer would otherwise have to rediscover:

1. **`list_issues` does not hydrate relations.** Only `get_issue` returns an
   issue's labels — the seam has no per-issue label getter at all. So
   `bd list --json` / `bd ready --json` carry no `labels` array, while
   `bd show --json` does. `export` pays for one `get_issue` per issue to avoid
   shipping a lossy backup; listings deliberately do not (it would be N+1 on
   every `bd ready`). If `bd-sqlite` starts hydrating labels in `list_issues`,
   delete the compensation in `commands/sync.rs::export`.
2. **`import` does not restore comments.** `add_comment` mints a new id and
   attributes the comment to whoever is importing, so re-running an import would
   duplicate every comment and rewrite its author. Idempotency won. The import
   prints a warning naming the count it skipped; it does not drop them silently.
   The real fix is a comment upsert on the seam.
3. **The default `bd list` view enumerates statuses.** `IssueFilter` has no
   "not closed" predicate, only a status set, so "everything except closed" is
   spelled out as the six built-in non-closed statuses. A workspace's *custom*
   statuses are therefore not in the default view — ask for them by name, or
   pass `--all`.
4. **`undefer` is a stub because `IssuePatch` cannot clear a field.** `None`
   means "leave alone", so there is no way to express "set `defer_until` back to
   nothing". Same reason `note` (append) is a stub. Both want a seam change, not
   a CLI hack.
5. **clap cannot group subcommands.** `help_heading` applies to arguments only,
   so the family grouping in `bd --help` is rendered from the `FAMILIES` table in
   `cli.rs`. `tests/cli.rs` asserts that table and the real command tree never
   drift — a command added to the enum but not the table fails the build.
6. **The binary runs on a 16 MiB worker thread.** clap's derive builds the ~120
   subcommand tree in one enormous stack frame, which overflows Windows' 1 MiB
   main-thread stack in a debug build. This was a real crash
   (`STATUS_STACK_OVERFLOW`, before `main` ran), not a theoretical one. See
   `main.rs`.
7. **Capability probes work without an open store.** `bd branch` on a SQLite
   workspace never opens the database: the locator is the authority on which
   engine owns a workspace (rule 3), and `Backend::has_commit_graph()` answers
   the question. When a store *is* open, the store's own capability accessors
   are used instead. Same reason stubs resolve the workspace but do not open it:
   opening a database for a command that will do nothing turns a clean exit 64
   into a spurious exit 1.
8. **Identity resolution order**: `--actor` > `$BEADS_ACTOR` > `.beads/config.yaml`
   `actor` > `git config user.email` > `unknown`. It never fails — not knowing who
   you are must not stop you from filing a bug. `$BEADS_SESSION` sets the session
   id if present.
