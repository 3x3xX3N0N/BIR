---
name: bd
description: >-
  Install, set up, and drive `bd` — the beads graph issue tracker for AI agents
  — and coordinate more than one concurrent agent session on a single shared
  board via claim/lease. Use this when a repo has a `.beads/` directory, when the
  user asks to track work as issues or dependencies, or when several agents share
  one task board and must not collide on the same issue.
---

# bd — the beads issue tracker

`bd` is a dependency-graph issue tracker whose primary user is a coding agent.
Issues ("beads") form a graph; the central question the tool answers is **"what
can I work on right now?"** Its point over a private TODO list: it coordinates
*many* sessions on one board, so two agents never work the same issue and nothing
downstream is forgotten.

## 1. Ensure `bd` is installed

Check first — it is a single machine-wide binary, so this is usually already true:

```bash
bd version   # -> "bd 0.1.0 (rust)" if present
```

If it is missing, install it (needs a Rust toolchain, `cargo`):

```bash
cargo install --git https://github.com/3x3xX3N0N/BIR bd-cli   # -> ~/.cargo/bin/bd
```

Then confirm `~/.cargo/bin` is on `PATH`. From a local clone instead:
`cargo install --path crates/bd-cli`.

## 2. Ensure the project has a workspace

One `.beads/` per project. `bd` finds it by walking up from the current directory
(like git finds `.git`), so any subdirectory works.

```bash
bd init --prefix proj   # only if there is no .beads/ yet; pick a short id prefix
```

If a `.beads/` already exists, do **not** re-init — just use it.

## 3. The loop — every session runs these four lines

This is the whole protocol. Each line is load-bearing.

```bash
bd ready --json                 # 1. unblocked AND unclaimed work; pick one
bd update <id> --claim          # 2. take it (a lease; --lease 2h to hold longer)
#   ... do the work; bd comment <id> "<finding>" as you learn ...
bd close <id> --reason done     # 3/4. close a blocker -> its dependents become ready
```

- **Skip the claim** and two sessions collide on one issue.
- **Skip the close** and everything the issue was blocking stays blocked forever.
- `bd prime` prints this loop plus the current board in one screen — run it when
  you start on an unfamiliar workspace.

## 4. Many sessions at once (ultraphrenia)

The board is the coordinator; no session needs to know another exists.

- **`bd ready` only ever offers unclaimed, unblocked work.** A claim is a
  **lease**, not a lock — so a session that dies frees its work automatically
  instead of holding it hostage.
- **Identify each session** so claims and the audit trail record *who* and *which
  run*:
  ```bash
  export BEADS_ACTOR=agent-web       # who is acting (or pass --actor NAME)
  export BEADS_SESSION=web-run-1     # any string unique to this run
  ```
- **A job outlives its lease?** `bd heartbeat <id>` (alias `bd hb`) renews it.
- **A session died mid-work?** `bd reclaim` returns every lapsed lease to the
  pool; `bd gc` sweeps lapsed leases and expired wisps as housekeeping.

## 5. Filing work and dependencies

```bash
bd create "<title>" -t bug|feature|task|epic|chore -p 0..4   # P0 is most urgent
bd q "<title>"              # quick capture; prints ONLY the new id (good for scripts)
bd dep add <issue> <blocker>   # an edge is how you say "not yet"
bd blocked                 # what the graph is currently gating, and on what
```

Found a problem while working on something else? **File it, don't fix it inline** —
`bd q` captures it and keeps you on the current issue.

## 6. Rules of the road

- **Every command takes `--json`** — use it whenever you parse output.
- **Never edit `.beads/` by hand.** Go through `bd`.
- **In a git repo, ignore the database.** Add `.beads/beads.db` (plus `-wal` /
  `-shm`) to `.gitignore`; the *shareable, git-tracked* form is
  `.beads/issues.jsonl`, which `bd export` writes and `bd import` reads.
  `bd hooks install` can automate export-on-commit; `bd doctor` warns if the db
  gets committed by accident.
- **Want real branch/merge of the issue database across machines?**
  `bd init --backend dolt` instead of the default SQLite (needs `dolt` on `PATH`).
- `bd setup` writes a short beads section into your agent's instructions file so
  every future turn already knows the loop. `bd doctor --fix` diagnoses a sick
  workspace.

## Installing this skill

Copy this directory to `~/.claude/skills/bd/` (personal, available in every
workspace) or `.claude/skills/bd/` inside a specific repo. Claude Code discovers
it by the `description` above and loads the body when the work matches.
