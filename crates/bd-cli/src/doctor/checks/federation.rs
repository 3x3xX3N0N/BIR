//! Federation — multiple beads workspaces syncing to each other.
//!
//! **These checks may not touch the network by default.** A diagnostic that
//! hangs for thirty seconds on a plane is a diagnostic people stop running, and
//! `bd doctor` is expected to be fast enough to put in a git hook. Anything that
//! would dial a peer belongs behind an explicit opt-in; without it, report on
//! what is *configured* and what the last sync recorded, which is where the bugs
//! are anyway.
//!
//! Belongs here: peer configuration coherence, unresolved federation conflicts,
//! sync staleness (from recorded timestamps, not from asking the peer), remote
//! safety (are we about to push to somewhere we shouldn't), key-value sync
//! status.
//!
//! # What this port actually federates — and what it does not
//!
//! Most of that list describes a feature **this port has not built yet**, and
//! this file does not pretend otherwise. Concretely, as of now:
//!
//! * There is **no peer store**. `bd federation add-peer <name> <url>` parses
//!   both arguments and discards them ([`crate::commands::sync::federation`]
//!   is `require_cap(Cap::Remote)` followed by `stub`). No table, no config key,
//!   nothing on disk. So "peer configuration coherence" has *nothing to read*,
//!   and two peers cannot share an identity because no peer can exist.
//! * There is **no federation conflict record**. `Storage::version_control()`
//!   returns `None` for the only backend the CLI will open, so
//!   `VersionControl::conflicts()` is unreachable. (Conflict *markers* in a
//!   merged file are real — and they belong to the Git family, which lists
//!   "unresolved conflict markers" in its own remit.)
//! * **Nothing pushes.** No `git push`, no `dolt push`, no remote of any kind is
//!   ever written to. Upstream's `CheckRemoteSafety` is not even a doctor check
//!   — it is a pure decision function guarding `bd init` against clobbering a
//!   remote that already has data, and it depends on a `git ls-remote`, which is
//!   a network call. Neither the hazard nor the permission to make that call
//!   exists here.
//! * There is **no recorded `last_sync` timestamp** anywhere: not in the config
//!   table, not in `config.yaml`, not in the schema.
//!
//! Writing checks for those would be writing checks for a thing that cannot be
//! misconfigured. They would return `ok` forever and *report as coverage*, which
//! the module docs on the seam are explicit is worse than having no check at all.
//! So they are not here, and this file is honestly short.
//!
//! What *is* real is this: **`.beads/issues.jsonl` is this port's federation.**
//! `bd hooks install` writes a pre-commit hook that exports the database to it
//! and a post-merge hook that imports it back, so issues travel between machines
//! inside the user's own git history. That transport can rot exactly the way a
//! peer link rots — quietly, weeks ago — and it rots in a way we can see from
//! recorded timestamps alone, with no network. That is [`SyncStaleness`].
//!
//! The other two live checks are about federation state the user *thinks* they
//! configured and did not: [`FederationConfig`] (federation keys in
//! `config.yaml`, which this build's `Config` silently drops) and [`KvStore`]
//! (`kv.*` keys in the config table, which no command in this build can read
//! back, remove, or sync).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Result, bail};
use async_trait::async_trait;
use bd_core::IssueFilter;
use bd_storage::Storage;
use chrono::{DateTime, Utc};

use super::super::{Category, Check, Dx, Finding, Repair};
use crate::cli::ExportArgs;

pub fn checks() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(FederationConfig),
        Box::new(SyncStaleness),
        Box::new(KvStore),
    ]
}

/// The git-tracked text form of the database — the thing that actually carries
/// issues between machines in this port.
///
/// Duplicated from the private `JSONL` in [`crate::commands::setup`], which owns
/// the hooks that write it. If that name ever changes, this check goes quietly
/// blind, which is why [`SyncStaleness`] reports the *path it looked at* in
/// every non-`ok` finding rather than just saying "stale".
const JSONL: &str = "issues.jsonl";

// ---------------------------------------------------------------------------
// Federation configured in a build that has no federation
// ---------------------------------------------------------------------------

/// Upstream configures federation in `.beads/config.yaml` (`federation.remote`,
/// `federation.sovereignty`, and a `repos:` list). This build's [`Config`] knows
/// only `prefix`, `actor`, `claim` and `defaults` — and it derives
/// `#[serde(default)]` *without* `deny_unknown_fields`, so every one of those
/// keys parses cleanly and is then dropped on the floor.
///
/// That is the whole bug: the user writes a peer URL into the config file, gets
/// no error, and gets no federation. Nothing else in the program will ever tell
/// them. The point of this check is to be the thing that does.
///
/// [`Config`]: crate::context::Config
struct FederationConfig;

/// Top-level `config.yaml` keys that mean "I am configuring federation" and that
/// this build reads as nothing at all.
const IGNORED_KEYS: &[&str] = &["federation", "repos", "peers", "remotes", "remote"];

#[async_trait]
impl Check for FederationConfig {
    fn name(&self) -> &'static str {
        "federation config"
    }

    fn category(&self) -> Category {
        Category::Federation
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let name = self.name();

        // No workspace is not a federation problem. Absence is not failure.
        let Some(path) = dx.beads_path(crate::context::CONFIG_FILE) else {
            return Finding::ok(name, "no workspace, so no federation config");
        };

        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Finding::ok(name, "no federation configured");
            }
            // We could not read the file, so we do not know what is in it. That
            // is a `warn`, not an `ok` — see the seam's docs on `unknown`.
            Err(e) => {
                return Finding::unknown(name, format!("cannot read {}: {e}", path.display()));
            }
        };

        let doc: serde_yaml::Value = match serde_yaml::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                return Finding::unknown(name, format!("cannot parse {}: {e}", path.display()));
            }
        };

        let found = ignored_federation_keys(&doc);
        if found.is_empty() {
            return Finding::ok(name, "no federation configured");
        }

        let keys: Vec<&str> = found.iter().copied().collect();
        Finding::warn(
            name,
            format!(
                "{} federation {} in config.yaml that this build ignores",
                keys.len(),
                if keys.len() == 1 { "key" } else { "keys" }
            ),
        )
        .detail(format!(
            "{} sets: {}\n\
             This build's config knows only prefix, actor, claim and defaults. \
             Everything else parses and is discarded — so this workspace looks \
             federated and is not.",
            path.display(),
            keys.join(", ")
        ))
        .fix(
            "this build has no federation (`bd federation ...` exits 2, a capability gap). \
             Either remove these keys, or keep using upstream bd for federated workspaces.",
        )
    }
}

/// The federation-flavoured top-level keys present in the document.
///
/// Split out so it can be tested without a workspace on disk.
fn ignored_federation_keys(doc: &serde_yaml::Value) -> BTreeSet<&'static str> {
    let mut found = BTreeSet::new();
    let serde_yaml::Value::Mapping(map) = doc else {
        // An empty `config.yaml` parses to `Null`, which is normal, not broken.
        return found;
    };
    for &key in IGNORED_KEYS {
        if map.contains_key(serde_yaml::Value::String(key.to_string())) {
            found.insert(key);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Sync staleness — from recorded timestamps, never from asking a peer
// ---------------------------------------------------------------------------

/// Is the text form that git carries still in step with the database?
///
/// This is the one federation failure this port can actually *have*. The
/// pre-commit hook exports the database to `.beads/issues.jsonl` and the
/// post-merge hook imports it back, so that file — and only that file — is how
/// a teammate ever sees your issues. Uninstall the hooks, or install them into a
/// worktree where git never looks for them, and the export simply stops running.
/// Nothing breaks. `bd ready` still works. Your issues just quietly stop leaving
/// the machine, and you find out weeks later.
///
/// Both sides of the comparison are *recorded* timestamps already on this
/// machine — `Issue::updated_at` from the database, and the mtime of the JSONL.
/// No peer is asked anything.
struct SyncStaleness;

/// Below this, a lag means nothing. The pre-commit hook re-exports on *every*
/// commit, so between commits the JSONL is *supposed* to trail the database —
/// warning on any lag at all would fire on nearly every run, and a check that
/// cries wolf on the happy path is a check people learn to skip. A week is long
/// enough that the hook has plainly not run, which is the bug we are hunting.
const STALE_AFTER_DAYS: i64 = 7;

/// How many changed ids to name before summarising. A finding that says "43
/// issues are unexported" without naming any is a bug report you cannot act on;
/// one that names all 43 is a wall of text. Name a few.
const MAX_NAMED: usize = 5;

#[async_trait]
impl Check for SyncStaleness {
    fn name(&self) -> &'static str {
        "federation sync staleness"
    }

    fn category(&self) -> Category {
        Category::Federation
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let name = self.name();

        let Some(path) = dx.beads_path(JSONL) else {
            return Finding::ok(name, "no workspace, so nothing to sync");
        };

        // No JSONL means this workspace does not carry its issues in git. That
        // is a perfectly ordinary way to use beads — a solo local tracker — and
        // it is emphatically not a federation *problem*. Absence is not failure.
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Finding::ok(name, "issues are not carried in git here");
            }
            Err(e) => {
                return Finding::unknown(name, format!("cannot stat {}: {e}", path.display()));
            }
        };
        let exported_at: DateTime<Utc> = match meta.modified() {
            Ok(t) => t.into(),
            // Some filesystems have no mtime. We cannot answer, so we say so.
            Err(e) => {
                return Finding::unknown(
                    name,
                    format!("{} has no modification time: {e}", path.display()),
                );
            }
        };

        let Some(store) = dx.store().await else {
            return Finding::unknown(
                name,
                dx.store_error()
                    .unwrap_or("there is no database to compare against"),
            );
        };

        let issues = match store.list_issues(&IssueFilter::new()).await {
            Ok(i) => i,
            Err(e) => return Finding::unknown(name, format!("cannot read issues: {e:#}")),
        };

        // Everything the database has learned since the last export. Closed
        // issues count: closing one is a change that has to reach the team.
        let mut unexported: Vec<(&str, DateTime<Utc>)> = issues
            .iter()
            .filter(|i| i.updated_at > exported_at)
            .map(|i| (i.id.as_str(), i.updated_at))
            .collect();

        let Some(newest) = unexported.iter().map(|(_, t)| *t).max() else {
            return Finding::ok(name, "issues.jsonl is in step with the database");
        };

        let lag = newest - exported_at;
        if lag.num_days() < STALE_AFTER_DAYS {
            // Behind, but only by the ordinary edit-then-commit gap.
            return Finding::ok(name, "issues.jsonl is in step with the database");
        }

        // Newest first: the most recent change is the most useful evidence.
        unexported.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
        let named: Vec<&str> = unexported.iter().take(MAX_NAMED).map(|(id, _)| *id).collect();
        let more = unexported.len().saturating_sub(named.len());
        let mut ids = named.join(", ");
        if more > 0 {
            ids.push_str(&format!(", and {more} more"));
        }

        Finding::warn(
            name,
            format!(
                "the issues.jsonl git carries is {} days behind the database",
                lag.num_days()
            ),
        )
        .detail(format!(
            "{} was last written {}\n\
             the newest change in the database is {}\n\
             {} unexported: {}",
            path.display(),
            exported_at.format("%Y-%m-%d %H:%M UTC"),
            newest.format("%Y-%m-%d %H:%M UTC"),
            unexported.len(),
            ids
        ))
        .fix(
            "`bd export -o .beads/issues.jsonl` and commit it — or `bd hooks install`, \
             so that every commit does it for you",
        )
    }

    /// Re-export, but **only** if that cannot destroy anything.
    ///
    /// Exporting *overwrites* the JSONL. If that file holds issues the database
    /// has never seen — a pull landed them and the post-merge hook that would
    /// have imported them was never installed — then overwriting it deletes a
    /// teammate's work, and `--fix` becomes the very bug it was run to cure.
    /// Both halves of the hook pair are installed and removed together, so
    /// "export never ran" and "import never ran" are the *same* workspace: this
    /// is not a hypothetical.
    ///
    /// So: look first, and refuse if the file is carrying anything we would lose.
    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(path) = dx.beads_path(JSONL) else {
            return Ok(Repair::Unfixable);
        };
        let Some(store) = dx.store().await else {
            return Ok(Repair::Unfixable);
        };

        let unimported = unimported_ids(&path, store).await?;
        if !unimported.is_empty() {
            // Deliberately an `Err`, not `Unfixable`: `Unfixable`'s message is
            // hardcoded by the seam ("no automatic repair; fix it by hand"), and
            // that would throw away the one sentence the user needs — that they
            // are about to lose issues unless they import first.
            bail!(
                "refusing to export: {} holds {} issue(s) the database has never seen ({}). \
                 Exporting would overwrite and destroy them. Run `bd import .beads/{}` first, \
                 then re-run `bd doctor --fix`.",
                path.display(),
                unimported.len(),
                preview(&unimported),
                JSONL,
            );
        }

        let n = store.count_issues(&IssueFilter::new()).await?;
        crate::commands::sync::export(
            dx.ctx,
            ExportArgs {
                output: Some(path.clone()),
                open_only: false,
            },
        )
        .await?;

        Ok(Repair::Did(format!(
            "re-exported {n} issues to {}; `git add` it so the commit carries them",
            path.display()
        )))
    }
}

/// Issue ids present in the JSONL that the database does not have.
///
/// A parse failure is **not** treated as "no ids". A JSONL we cannot read is a
/// JSONL we cannot prove is safe to overwrite, and the caller uses this to
/// decide exactly that — so a malformed line propagates as an error and the
/// repair declines. Optimism here would be a data-loss bug.
async fn unimported_ids(path: &Path, store: &dyn Storage) -> Result<Vec<String>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("cannot read {}: {e}", path.display()),
    };

    let mut ids: Vec<String> = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("{}:{}: {e}", path.display(), n + 1))?;
        // `bd export` writes one issue per line, tagged `_type: "issue"`. Older
        // files predate the tag, so an untagged line with an id still counts.
        let is_issue = match v.get("_type") {
            Some(t) => t == "issue",
            None => true,
        };
        if !is_issue {
            continue;
        }
        if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
            ids.push(id.to_string());
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let known: BTreeSet<String> = store
        .get_issues(&ids)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();
    Ok(ids.into_iter().filter(|id| !known.contains(id)).collect())
}

fn preview(ids: &[String]) -> String {
    let head: Vec<&str> = ids.iter().take(MAX_NAMED).map(String::as_str).collect();
    let more = ids.len().saturating_sub(head.len());
    let mut s = head.join(", ");
    if more > 0 {
        s.push_str(&format!(", and {more} more"));
    }
    s
}

// ---------------------------------------------------------------------------
// Key-value sync status
// ---------------------------------------------------------------------------

/// `kv.*` keys in the config table that this build cannot reach.
///
/// Upstream keeps its key-value store in the config table under a `kv.` prefix,
/// and its own doctor check reports the count as a cheerful "N KV pairs stored
/// (syncs via Dolt)". Here, none of that sentence is true:
///
/// * `bd kv get|set|list|clear` is unimplemented (exit 64), so a `kv.` key
///   cannot be read back through the surface that is supposed to own it;
/// * `bd config unset` is *also* unimplemented, so it cannot be removed either;
/// * `bd export` carries issues, not config, so it does not sync anywhere.
///
/// A `kv.` key can still be *written*, because `bd config set <key> <value>`
/// takes any key at all. So this is reachable, and when it fires it is telling
/// the user something true and unpleasant: they have written data into a
/// namespace that this build can neither read, delete, nor transport. That is
/// why it warns where upstream merely counted.
struct KvStore;

const KV_PREFIX: &str = "kv.";

#[async_trait]
impl Check for KvStore {
    fn name(&self) -> &'static str {
        "federation kv store"
    }

    fn category(&self) -> Category {
        Category::Federation
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let name = self.name();

        // Outside a workspace there is no config table, and that is not a
        // problem to report — it is the Core family's job to say there is no
        // workspace, and it will.
        if !dx.in_workspace() {
            return Finding::ok(name, "no workspace, so no key-value data");
        }

        let Some(store) = dx.store().await else {
            return Finding::unknown(
                name,
                dx.store_error()
                    .unwrap_or("there is no database to read config from"),
            );
        };

        let entries = match store.list_config().await {
            Ok(e) => e,
            Err(e) => return Finding::unknown(name, format!("cannot read config: {e:#}")),
        };

        let keys: Vec<&str> = entries
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| k.starts_with(KV_PREFIX))
            .collect();

        if keys.is_empty() {
            return Finding::ok(name, "no key-value data");
        }

        Finding::warn(
            name,
            format!(
                "{} key-value {} that this build cannot reach",
                keys.len(),
                if keys.len() == 1 { "entry" } else { "entries" }
            ),
        )
        .detail(format!(
            "in the config table: {}\n\
             `bd kv` is not implemented in this build, and neither is `bd config unset`, \
             so these can be neither read back nor removed. `bd export` carries issues, \
             not config, so they do not travel with the workspace either.",
            keys.join(", ")
        ))
        .fix(
            "if these were set by mistake, `bd config set <key> ''` will blank them; \
             a real `bd kv` is not available in this build",
        )
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of `FederationConfig`: this build's `Config` derives
    /// `#[serde(default)]` and *not* `deny_unknown_fields`, so a federation key
    /// in `config.yaml` parses without complaint and is silently discarded. If
    /// someone ever adds `deny_unknown_fields`, `Ctx::build` starts erroring on
    /// these files instead and this check becomes redundant — but until then it
    /// is the only thing in the program that will tell the user.
    #[test]
    fn a_federation_key_in_config_yaml_is_silently_dropped_by_the_typed_config() {
        let yaml = "prefix: acme\nfederation:\n  remote: dolthub://acme/beads\n";

        let typed: crate::context::Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(typed.prefix.as_deref(), Some("acme"));
        // No error, no field, no federation. That is the bug.

        let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let found = ignored_federation_keys(&doc);
        assert!(found.contains("federation"), "the check must see what the typed config threw away");
    }

    #[test]
    fn an_ordinary_config_has_no_federation_keys() {
        let doc: serde_yaml::Value =
            serde_yaml::from_str("prefix: bd\nclaim:\n  lease: 30m\n").unwrap();
        assert!(ignored_federation_keys(&doc).is_empty());
    }

    /// An empty `config.yaml` parses to `Null`, not to a mapping. A check that
    /// assumed a mapping would panic here — on a file that is perfectly legal.
    #[test]
    fn an_empty_config_is_not_a_federation_problem() {
        let doc: serde_yaml::Value = serde_yaml::from_str("").unwrap();
        assert!(ignored_federation_keys(&doc).is_empty());
    }

    #[test]
    fn every_upstream_federation_key_is_recognised() {
        for key in ["federation", "repos", "peers", "remotes", "remote"] {
            let doc: serde_yaml::Value = serde_yaml::from_str(&format!("{key}: x\n")).unwrap();
            assert!(
                ignored_federation_keys(&doc).contains(key),
                "{key} should be recognised as ignored federation config"
            );
        }
    }
}
