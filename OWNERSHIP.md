# File ownership during the parallel port

This project is being filled in by waves of agents working at the same time.
Nothing here is about politeness — it is about making collisions *arithmetically
impossible* rather than merely unlikely.

## The three rules

**1. You may only write files on your ownership list.**
If your task needs a change to a file you do not own, **stop and report it**.
Do not edit it. Do not work around it. A workaround invented independently by
six agents is six different workarounds, and reconciling them costs more than
the change would have.

**2. The frozen files belong to the integrator.**
These are edited between waves, never during one:

| File | Why it is frozen |
|---|---|
| `crates/bd-core/src/types.rs` | The domain. Everything depends on it. |
| `crates/bd-core/src/filter.rs` | `IssueFilter` — the SQL pushdown contract. |
| `crates/bd-storage/src/lib.rs` | The `Storage` trait. Changing it changes every backend. |
| `crates/bd-storage/src/capability.rs` | The capability traits. |
| `crates/bd-cli/src/cli.rs` | The whole clap tree. Every command's args live here. |
| `crates/bd-cli/src/commands/mod.rs` | The central dispatch. **See below.** |
| `Cargo.toml` (workspace) | Shared dependency versions. |

**3. The tree is green at every commit.**
`cargo test --workspace` and `cargo clippy --workspace --all-targets` must pass.
A command you cannot finish **reverts to a stub** — never a broken build, never
a command that compiles and lies. Record what you left undone in `PORT_STATUS.md`.

## The dispatch exception

`commands/mod.rs` is frozen but it is also the one file every implementation
must touch, to swap `C::Edit { .. } => stub("edit", ctx)` for a real call. That
is a genuine conflict and pretending otherwise would just produce a pileup.

So: **do not edit it.** Implement your handler in the file you own, export it,
and put the one-line dispatch change you need in your final report. The
integrator applies all of them at the green gate, serially, in one pass.

One line per agent, applied by one hand, is not a merge conflict.

## What you can rely on

- **`cli.rs` already defines every command's arguments**, including the ones that
  are still stubs. You almost certainly do not need to change it. If you think
  you do, say so — do not add a flag yourself.
- **The store opens lazily.** Just call `ctx.store().await?`. There is no list to
  add yourself to, and a command that never opens a store costs nothing.
- **Three exit codes, kept distinct** (`crates/bd-cli/src/exit.rs`):
  `64` = not ported yet · `2` = this backend cannot do that (an honest answer,
  not a gap) · `1` = real failure. Do not collapse them.

## Wave assignments

Each row is one agent. No two rows in a wave name the same file.

### Wave 1 — command families (own one existing file each)
| Agent | Owns | Fills in |
|---|---|---|
| 1 | `commands/issues.rs` | edit, restore, rename, tag, note, duplicate, supersede, link, heartbeat, state, set-state, statuses, types, promote, batch |
| 2 | `commands/views.rs` | epic, info, stale, orphans, duplicates, find-duplicates, lint, kv, audit, context, ping, children |
| 3 | `commands/deps.rs` | graph, graph check, flatten |
| 4 | `commands/maintenance.rs` | gc, purge, prune, admin, rename-prefix, reclaim, merge-slot |
| 5 | `commands/setup.rs` | bootstrap, onboard, quickstart, prime, hooks, upgrade, metrics, config |
| 6 | `commands/sync.rs` | repo, mail, ship, export/import polish |

### Wave 2 — trackers (own one **new** file each; zero collision by construction)
| Agent | Owns |
|---|---|
| 1–6 | `integrations/{linear,jira,github,gitlab,notion,ado}.rs`, one apiece |

Each implements the `Tracker` trait. `integrations/mod.rs` is written by the
integrator *before* the wave, so nobody needs to touch it.

### Wave 3 — the Dolt backend (new crate, isolated worktree)
| Agent | Owns |
|---|---|
| 1 | `bd-dolt/src/server.rs` — spawn and supervise `dolt sql-server` |
| 2 | `bd-dolt/src/store.rs` — `Storage` over MySQL wire |
| 3 | `bd-dolt/src/vc.rs` — `VersionControl` + `RemoteStore` via `CALL DOLT_*()` |

**The one thing agent 3 must not forget:** `pull` has to trigger a full
`recompute_blocked()`. A merge lands closed blockers and new edges that no local
write path ever saw, so the `is_blocked` cache is stale *by definition* the
moment a pull completes. Skip it and `bd ready` is quietly wrong after every
sync — no error, no crash, just the wrong work.

### Wave 4 — doctor (own one **new** check file each)
Each agent owns `doctor/checks/<name>.rs` and implements the `Check` trait.
The registry is written by the integrator before the wave.

### Wave 5 — the formula DSL
Deliberately **not** sharded. It is one compiler — inheritance, loops, gates,
and advice woven over a workflow graph. Splitting a compiler across ten agents
produces mush, not speed. Then mol/swarm/gate, which depend on it.
