# bd — beads, in Rust

A clean-room Rust port of [beads](https://github.com/gastownhall/beads), the
distributed graph issue tracker for AI agents.

> **Status: working `0.1.0`.** Both storage backends are complete and tested, the
> core workflow is solid, and the `--json` surface is compatible with upstream so
> existing agent tooling ports over. A handful of advanced commands are still
> stubs — they exit with code **64** and say so; they never pretend to succeed.
> [PORT_STATUS.md](PORT_STATUS.md) is the exact per-command manifest, and the open
> issues track what is left.

## What beads is

An issue tracker whose primary user is a coding agent, not a human. Issues
("beads") form a dependency graph, and the central question the tool answers is
**"what can I work on right now?"** — `bd ready` returns the issues that are
open, unblocked, unclaimed, and not deferred. An agent claims one, works it,
closes it, and the beads it was blocking become ready in turn.

```bash
bd init --prefix proj
A=$(bd q "design the API")
bd create "implement the API" --deps "$A:blocks" -p 1

bd ready              # design the API   (implement is blocked by it)
bd close "$A" --reason done
bd ready              # implement the API   (now unblocked)
```

Everything speaks `--json` for scripts and agents.

## What works

- **The whole core loop** — create/update/close, dependencies, `ready`/`blocked`,
  `search`/`query`, labels, comments, `export`/`import`, custom states.
- **Two storage backends, both complete and verified.** SQLite (default, zero
  dependencies) and **Dolt** (a versioned, MySQL-compatible store giving real
  `branch` / `merge` / `push` / `pull` over an issue database). The Dolt backend
  is tested against a live `dolt sql-server`.
- **`bd doctor`** — 48 diagnostic checks across 9 families, with `--fix`.
- **`bd cook`** — a formula DSL (`bd-formula`): compile a `.formula.toml`
  (variables, conditions, loops, gates) into a live issue graph.
- **Six tracker integrations** (github, gitlab, jira, linear, notion, ado),
  tested offline against a mock HTTP seam — zero network calls, zero credentials.
- **Molecules, gates, wisps, agent memory** — the advanced tier, built on the
  same graph.

734 tests; the SQLite suite runs anywhere, and the Dolt suite runs for real when
a `dolt` binary is present.

## Design

Six crates, layered so that nothing above the storage seam knows what database it
is talking to.

| Crate | What it is |
|---|---|
| `bd-core` | The domain. `Issue`, the dependency graph, ids. Pure data; no I/O. |
| `bd-query` | The `bd query` filter language: lexer, parser, evaluator. |
| `bd-formula` | The formula compiler: `.formula.toml` → a plan of proto-issues. Pure; no I/O. |
| `bd-storage` | The seam. An object-safe `Storage` trait plus *optional* capability traits. |
| `bd-sqlite` | A complete store, no commit graph. |
| `bd-dolt` | A complete store *with* a commit graph, over the MySQL wire to `dolt sql-server`. |
| `bd-cli` | The `bd` binary. Holds a `Box<dyn Storage>` and never learns which engine it got. |

### Three things worth knowing

**`bd ready` does not traverse the graph.** Readiness is a denormalized
`is_blocked` column, recomputed **to a fixpoint** on every write that could
change it. A single update pass is not enough, because blocked-ness propagates
transitively down parent-child edges. This is the most load-bearing — and most
breakable — semantic in the system, and it is why any `merge`/`pull`/`import`
triggers a full recompute: rows that arrive that way were never seen by a local
write path, so the cache is stale by definition. Get it wrong and `bd ready`
lies with no error — so `bd doctor` has a dedicated, mutation-tested check that
re-derives the value from the edges and diffs it against the stored column.

**Backends differ in capability, not in quality.** SQLite is not a degraded Dolt.
It is a complete issue store that happens to have no commit graph, so `bd branch`
and `bd dolt push` return **exit 2** ("this backend cannot") rather than failing
or, worse, silently doing nothing. On Dolt the same commands light up with no
change above the seam. Exit 2 (a permanent, honest "no") and exit 64 ("not built
yet") are never conflated.

**Dolt is reached as a subprocess, not linked.** `bd-dolt` spawns and supervises
`dolt sql-server`, talks to it over the MySQL wire with `sqlx`, and does all
version control by calling SQL stored procedures (`CALL DOLT_COMMIT/MERGE/PUSH`).
There is no Go in the build; `dolt` is a runtime dependency, resolved on `PATH`.

## Departures from upstream

Clean-room, so the ugly parts are not preserved:

- **No shadow tables.** Upstream duplicates every table into a `wisp_*` twin, so
  every graph query is written four times. Ephemeral is a flag on `Issue` here;
  where the row physically lands is the backend's business.
- **No `is_blocked` on the domain type.** It is derived state. Putting it on
  `Issue` would invite callers to trust a stale copy.
- **The storage interface is not named after a database.** Upstream's is called
  `DoltStorage`, and the abstraction leaked accordingly.
- **Event ids are UUIDs, not autoincrement integers**, so a Dolt merge between
  clones is a clean union of audit trails rather than a primary-key collision.

The `--json` output shape *is* kept compatible, so existing agent tooling and the
MCP server keep working unchanged.

## Build

```bash
cargo build --release        # binary at target/release/bd
cargo test                   # SQLite suite runs anywhere
```

Requires Rust 1.90+ (edition 2024). The **Dolt backend is optional**: install
[`dolt`](https://github.com/dolthub/dolt) and put it on `PATH` to use
`--backend=dolt` and run the Dolt test suite; without it, SQLite is the default
and needs nothing.

## Status and roadmap

[PORT_STATUS.md](PORT_STATUS.md) is the authoritative per-command manifest and the
list of known design decisions. The remaining stubs are tracked as issues; each
is blocked on a specific, named thing (a storage-seam method, a CLI flag, or an
unbuilt substrate) rather than being merely unwritten.

## Credit and license

A clean-room port: [beads](https://github.com/gastownhall/beads) is the **spec**,
not the source. The hard architectural questions were shaped by upstream's own
`PROPOSAL-pluggable-storage-backends.md` (written from their earlier Rust spike),
so this port starts from field-tested answers rather than rediscovering them. All
credit for the design of beads is upstream's.

Licensed under [MIT](LICENSE).
