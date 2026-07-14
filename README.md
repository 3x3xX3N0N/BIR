# bd — beads, in Rust

A clean-room Rust port of [beads](https://github.com/gastownhall/beads), the
distributed graph issue tracker for AI agents.

> **Status: early.** The command surface is complete — every command upstream has
> is registered and documented — but most of them are stubs. See
> [PORT_STATUS.md](PORT_STATUS.md) for exactly what works. A stub exits with code
> 64 and says so; it never pretends to succeed.

## What beads is

An issue tracker whose primary user is a coding agent, not a human. Issues
("beads") form a dependency graph, and the central question the tool answers is
**"what can I work on right now?"** — `bd ready` returns the issues that are
open, unblocked, unclaimed, and not deferred. An agent claims one, works it,
closes it, and the beads it was blocking become ready in turn.

## Why port it

Upstream is ~213k lines of Go. This port is not a transliteration. It is scoped
by upstream's own
[`PROPOSAL-pluggable-storage-backends.md`](https://github.com/gastownhall/beads),
which was written from *their* earlier Rust spike — so the hard architectural
questions already have field-tested answers, and this port starts from them
rather than rediscovering them.

## Design

Five crates, layered so that nothing above the storage seam knows what database
it is talking to.

| Crate | What it is |
|---|---|
| `bd-core` | The domain. `Issue`, the dependency graph, ids. Pure data; no I/O. |
| `bd-query` | The `bd query` filter language: lexer, parser, evaluator. |
| `bd-storage` | The seam. An object-safe `Storage` trait plus *optional* capability traits. |
| `bd-sqlite` | A complete store with no commit graph. |
| `bd-cli` | The `bd` binary. Holds a `Box<dyn Storage>` and never learns which engine it got. |

### Two things worth knowing

**`bd ready` does not traverse the graph.** Readiness is a denormalized
`is_blocked` column, recomputed **to a fixpoint** on every write that could
change it. A single update pass is not enough, because blocked-ness propagates
transitively down parent-child edges. This is the most load-bearing — and most
breakable — semantic in the system, and it is why `pull` must always trigger a
full recompute: rows that arrive via sync were never seen by any local write
path, so the cache is stale by definition.

**Backends differ in capability, not in quality.** SQLite is not a degraded Dolt.
It is a complete issue store that happens to have no commit graph, so
`bd branch` and `bd dolt push` are not available — and it says so plainly rather
than failing or, worse, silently doing nothing. Capabilities may make a core
command *better*; they may never make it *possible*.

## Departures from upstream

This is a clean-room port, so the ugly parts are not preserved:

- **No shadow tables.** Upstream duplicates every table into a `wisp_*` twin, so
  every graph query is written four times. Ephemeral is a flag on `Issue` here;
  where the row physically lands is the backend's business.
- **No `is_blocked` on the domain type.** It is derived state. Putting it on
  `Issue` would invite callers to trust a stale copy.
- **The storage interface is not named after a database.** Upstream's is called
  `DoltStorage`, and the abstraction leaked accordingly.

The `--json` output shape *is* kept compatible, so existing agent tooling and the
MCP server keep working.

## Build

```bash
cargo build
cargo test
```

Requires Rust 1.90+.
