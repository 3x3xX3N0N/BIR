//! Metadata — who this workspace is, and whether it still agrees with itself.
//!
//! Identity drift is quiet and expensive: a workspace whose id prefix no longer
//! matches the ids actually in the database will mint colliding ids after a
//! clone, and the collision surfaces as a merge conflict weeks later in someone
//! else's repository.
//!
//! Belongs here: project identity and name/slug agreement, id prefix vs. the ids
//! on disk, id format validity, config values that parse but are nonsense (a
//! lease of `0s`, a negative priority), repo fingerprint, metadata version
//! tracking, multi-repo type coherence.
//!
//! # The failure this family exists to catch
//!
//! The prefix is a *namespace*. Ids are content hashes ([`bd_core::idgen`]), and
//! collision checking is local-only by design — two clones genuinely can mint
//! the same body. The prefix is the only thing keeping two different *projects*
//! out of each other's id space, and its default (`bd`) is the same default
//! every other beads workspace on earth falls back to.
//!
//! So the drift that matters is not "the prefix is ugly". It is: **the prefix
//! bd would mint with today is not the prefix the ids in this database carry**.
//! That happens the moment `.beads/config.yaml` goes missing or its `prefix:`
//! key is dropped — nothing breaks, `bd create` keeps working, and every new id
//! quietly lands in the global `bd-` namespace next to five hundred `acme-`
//! ones. Weeks later somebody merges two repos and both minted `bd-a3f2`.
//!
//! There is a second, sharper edge in this port: [`Ctx::prefix`] prefers
//! `config.yaml` over the store, but `bd config set issue.prefix` writes only
//! the *store*. A user who runs it, sees `issue.prefix = acme` echoed back, and
//! keeps minting `bd-` ids is not doing anything wrong — they were told the
//! setting took. [`PrefixAuthority`] is the check that says otherwise.
//!
//! [`Ctx::prefix`]: crate::context::Ctx::prefix

use std::collections::BTreeMap;

use anyhow::Result;
use async_trait::async_trait;
use bd_core::idgen::{BASE36_ALPHABET, MAX_HIERARCHY_DEPTH, MAX_ID_LENGTH, MIN_ID_LENGTH};
use bd_core::{IssueFilter, IssueType, Priority};
use bd_storage::Storage;

use crate::context::{CONFIG_FILE, Config};
use crate::doctor::{Category, Check, Dx, Finding, Repair, Status};
use crate::parse;

pub fn checks() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(IdPrefix),
        Box::new(PrefixAuthority),
        Box::new(IdFormat),
        Box::new(ConfigValues),
        Box::new(ProjectIdentity),
        Box::new(RepoFingerprint),
        Box::new(VersionTracking),
    ]
}

// ---------------------------------------------------------------------------
// Keys and limits
// ---------------------------------------------------------------------------

/// The store's id-prefix key, and the legacy spelling that older workspaces use.
///
/// String literals rather than `bd_sqlite::PREFIX_KEY` for the reason
/// [`crate::context::Ctx::prefix`] gives: naming a concrete backend here would
/// put one on the far side of the storage seam. The order is the same as
/// `Ctx::prefix` uses, and it must stay that way — a check that resolves the
/// prefix differently from the code that mints ids is worse than no check.
const PREFIX_KEYS: [&str; 2] = ["issue.prefix", "prefix"];

/// Where `bd config set issue.prefix` writes, and what a repair should write.
const PREFIX_KEY: &str = PREFIX_KEYS[0];

/// What [`crate::context::Ctx::prefix`] falls back to when nothing is
/// configured. Duplicated here on purpose: this check exists precisely to notice
/// when a workspace has silently fallen back to it.
const FALLBACK_PREFIX: &str = "bd";

/// The last `bd` version this workspace acknowledged (`bd upgrade ack`).
///
/// Duplicated from `commands::setup`, where it is a private const. See the
/// report — it wants to be `pub`.
const ACKED_KEY: &str = "upgrade.acked_version";

/// Long enough for `platform-infra`, short enough that it is still a prefix.
const MAX_PREFIX_LEN: usize = 20;

/// How many offending ids a finding names before it starts counting instead.
/// A finding that says "3 issues are corrupt" without naming them is a bug
/// report you cannot act on; a finding that names nine hundred is a wall.
const MAX_EXAMPLES: usize = 8;

// ---------------------------------------------------------------------------
// Id shape
// ---------------------------------------------------------------------------

/// Split an id into `(prefix, body)`: `acme-a3f2dd.1` -> `("acme", "a3f2dd")`.
///
/// The split is from the **right**, and that is not an accident: a prefix may
/// contain `-` (`bd init` in `my-project/` derives exactly that), while the body
/// never can, because it is base36. Splitting on the first `-` would read
/// `my-project-a3f2` as the prefix `my`, and every id in such a workspace would
/// look like drift.
fn split_id(id: &str) -> Option<(&str, &str)> {
    let root = id.split_once('.').map_or(id, |(head, _)| head);
    let (prefix, body) = root.rsplit_once('-')?;
    (!prefix.is_empty() && !body.is_empty()).then_some((prefix, body))
}

/// What is wrong with this id, or `None` if it is one bd could have minted.
///
/// The limits come from [`bd_core::idgen`] rather than from literals here, so
/// that widening the id space cannot leave this check quietly condemning
/// perfectly good ids.
fn id_problem(id: &str) -> Option<&'static str> {
    let (root, tail) = match id.split_once('.') {
        Some((head, tail)) => (head, Some(tail)),
        None => (id, None),
    };

    let Some((prefix, body)) = root.rsplit_once('-') else {
        return Some("no `prefix-`: an id is `<prefix>-<base36>`");
    };
    if prefix.is_empty() {
        return Some("empty prefix");
    }
    if body.is_empty() {
        return Some("empty id body");
    }
    if !body.bytes().all(|b| BASE36_ALPHABET.contains(&b)) {
        return Some("id body is not base36 (0-9, a-z)");
    }
    // Shorter than the generator's floor. This is also the only honest way to
    // spot a legacy sequential id (`bd-1`): a purely numeric body is *legal*
    // base36 — see the idgen docs — so `bd-123` is genuinely ambiguous and is
    // left alone. `bd-1` is not: nothing bd mints is that short.
    if body.len() < MIN_ID_LENGTH {
        return Some("id body is shorter than bd ever mints (a legacy sequential id?)");
    }
    if body.len() > MAX_ID_LENGTH {
        return Some("id body is longer than bd ever mints");
    }

    if let Some(tail) = tail {
        if id.matches('.').count() > MAX_HIERARCHY_DEPTH {
            return Some("nested deeper than bd allows");
        }
        for segment in tail.split('.') {
            if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
                return Some("a child segment is not a number");
            }
        }
    }
    None
}

/// What is wrong with a prefix, or `None`.
///
/// Deliberately looser than upstream, which also demands a leading letter. That
/// rule would condemn workspaces `bd init` itself creates: `derive_prefix` takes
/// the project directory name, so `2fa-service/` yields the prefix `2fa`, and
/// nothing about it is broken — `2fa-a3f2` splits exactly right. What actually
/// matters is that a prefix contain no `.` (the child-id separator, which would
/// make `my.proj-a3f2` unparseable), no whitespace, and no `:` (the `<id>:<type>`
/// separator in `bd dep add`).
fn prefix_problem(prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some("it is empty; bd would ignore it and fall back".to_string());
    }
    if prefix.chars().count() > MAX_PREFIX_LEN {
        return Some(format!(
            "it is {} characters long (max {MAX_PREFIX_LEN})",
            prefix.chars().count()
        ));
    }
    if let Some(c) = prefix
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return Some(format!(
            "it contains {c:?}; a prefix may hold only letters, digits, `-` and `_`"
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Prefix resolution — the same order `Ctx::prefix` mints with
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// `.beads/config.yaml`. Wins.
    ConfigFile,
    /// The store's config table. Consulted only when the file is silent.
    Store,
    /// Nothing said anything, so `bd`. This is the dangerous one.
    Fallback,
}

/// The prefix `bd create` would mint with right now, and where it came from.
///
/// Provenance is half the finding: "your prefix is `bd`" is not actionable, and
/// "your prefix is `bd` because nobody ever set one, and so is everyone else's"
/// is.
struct Resolved {
    value: String,
    source: Source,
}

fn declared_in_config(dx: &Dx<'_>) -> Option<String> {
    dx.ctx
        .config
        .prefix
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

async fn declared_in_store(store: &dyn Storage) -> Result<Option<String>, bd_storage::Error> {
    for key in PREFIX_KEYS {
        if let Some(p) = store.get_config(key).await?
            && !p.trim().is_empty()
        {
            return Ok(Some(p.trim().to_string()));
        }
    }
    Ok(None)
}

async fn resolve_prefix(dx: &Dx<'_>, store: &dyn Storage) -> Resolved {
    if let Some(value) = declared_in_config(dx) {
        return Resolved {
            value,
            source: Source::ConfigFile,
        };
    }
    if let Ok(Some(value)) = declared_in_store(store).await {
        return Resolved {
            value,
            source: Source::Store,
        };
    }
    Resolved {
        value: FALLBACK_PREFIX.to_string(),
        source: Source::Fallback,
    }
}

/// Every prefix carried by an id in the database, with how many ids carry it.
///
/// Ids that do not parse are counted separately and *not* reported here — they
/// are [`IdFormat`]'s finding, and having two checks indict the same id twice
/// makes both harder to act on.
async fn prefix_census(store: &dyn Storage) -> Result<(BTreeMap<String, usize>, usize), String> {
    let issues = store
        .list_issues(&IssueFilter::default())
        .await
        .map_err(|e| format!("could not list issues: {e}"))?;

    let mut census: BTreeMap<String, usize> = BTreeMap::new();
    let mut unparsed = 0usize;
    for issue in &issues {
        match split_id(&issue.id) {
            Some((prefix, _)) => *census.entry(prefix.to_string()).or_default() += 1,
            None => unparsed += 1,
        }
    }
    Ok((census, unparsed))
}

/// The prefix on the most ids, and whether it is a strict plurality.
fn dominant(census: &BTreeMap<String, usize>) -> Option<(&str, usize, bool)> {
    let top = census.iter().max_by_key(|(_, n)| **n)?;
    let tied = census.iter().filter(|(_, n)| *n == top.1).count() > 1;
    Some((top.0.as_str(), *top.1, !tied))
}

fn census_detail(census: &BTreeMap<String, usize>) -> String {
    let mut rows: Vec<_> = census.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    rows.iter()
        .map(|(p, n)| format!("{p}-  {n} issue(s)"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// id prefix
// ---------------------------------------------------------------------------

/// The headline check: does the prefix bd would mint with match the ids already
/// here?
struct IdPrefix;

const ID_PREFIX: &str = "id prefix";

#[async_trait]
impl Check for IdPrefix {
    fn name(&self) -> &'static str {
        ID_PREFIX
    }
    fn category(&self) -> Category {
        Category::Metadata
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        // No workspace is not a failure — there is no identity to drift. The
        // Core family is the one that says "run `bd init`".
        if !dx.in_workspace() {
            return Finding::ok(ID_PREFIX, "no workspace");
        }
        let Some(store) = dx.store().await else {
            return Finding::unknown(
                ID_PREFIX,
                dx.store_error().unwrap_or("the store would not open"),
            );
        };

        let resolved = resolve_prefix(dx, store).await;
        let (census, unparsed) = match prefix_census(store).await {
            Ok(c) => c,
            Err(e) => return Finding::unknown(ID_PREFIX, e),
        };

        if census.is_empty() {
            let msg = format!("`{}-` (no issues yet)", resolved.value);
            return match resolved.source {
                // A brand-new workspace that never got a prefix will mint into
                // the same `bd-` namespace as every other one. Nothing is wrong
                // *yet*, which is exactly when it is cheap to fix.
                Source::Fallback if unparsed == 0 => Finding::warn(
                    ID_PREFIX,
                    "no id prefix is configured; ids will be minted as `bd-`",
                )
                .detail(
                    "`bd` is what every unconfigured beads workspace falls back to, so ids\n\
                     minted here can collide with ids minted anywhere else.",
                )
                .fix("bd config set issue.prefix <something-project-specific>"),
                _ => Finding::ok(ID_PREFIX, msg),
            };
        }

        let Some((top, top_count, unique)) = dominant(&census) else {
            return Finding::unknown(ID_PREFIX, "no id carried a recognizable prefix");
        };
        let mine = census.get(&resolved.value).copied().unwrap_or(0);
        let total: usize = census.values().sum();

        if top == resolved.value && unique {
            return Finding::ok(
                ID_PREFIX,
                format!("`{}-` on {mine} of {total} issues", resolved.value),
            );
        }

        let provenance = match resolved.source {
            Source::ConfigFile => format!("`{}` comes from .beads/{CONFIG_FILE}", resolved.value),
            Source::Store => format!("`{}` comes from the store's {PREFIX_KEY}", resolved.value),
            Source::Fallback => format!(
                "nothing configures a prefix, so bd fell back to `{FALLBACK_PREFIX}` — \
                 the same fallback every other beads workspace uses"
            ),
        };
        let detail = format!(
            "prefixes actually on issues:\n{}\n\n{provenance}",
            census_detail(&census)
        );

        // Not one id in this database was minted with the prefix bd is about to
        // mint with. That is the drift the family exists for: nothing breaks
        // today, and in a month two clones have both minted `bd-a3f2`.
        if mine == 0 {
            return Finding::error(
                ID_PREFIX,
                format!(
                    "new ids will be minted as `{}-`, but every issue here is `{top}-`",
                    resolved.value
                ),
            )
            .detail(detail)
            .fix(format!(
                "if the ids are right: bd config set issue.prefix {top} \
                 (and remove or correct `prefix:` in .beads/{CONFIG_FILE})\n\
                 if the prefix is right, this was a deliberate rename — nothing to do"
            ));
        }

        // Somebody minted with this prefix, so it is not simply lost — a rename
        // in flight looks exactly like this. Untidy, not broken.
        Finding::warn(
            ID_PREFIX,
            format!(
                "`{}-` is on {mine} of {total} issues; `{top}-` is on {top_count}",
                resolved.value
            ),
        )
        .detail(detail)
        .fix(format!(
            "if this was a rename, nothing to do; otherwise: bd config set issue.prefix {top}"
        ))
    }

    /// Only the unambiguous case.
    ///
    /// If `config.yaml` declares a prefix, the disagreement may be a deliberate
    /// rename, and a `--fix` that silently reverted it would be worse than the
    /// drift. But if *nobody* ever declared one, there is no decision to
    /// overwrite — only an absence — and adopting what the database plainly
    /// already uses is the whole repair.
    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        if declared_in_config(dx).is_some() {
            return Ok(Repair::Unfixable);
        }
        let Some(store) = dx.store().await else {
            return Ok(Repair::Unfixable);
        };
        let (census, _) = match prefix_census(store).await {
            Ok(c) => c,
            Err(_) => return Ok(Repair::Unfixable),
        };
        let Some((top, count, true)) = dominant(&census) else {
            return Ok(Repair::Unfixable);
        };
        if declared_in_store(store).await.ok().flatten().as_deref() == Some(top) {
            return Ok(Repair::Unfixable);
        }
        let top = top.to_string();
        store.set_config(PREFIX_KEY, &top).await?;
        Ok(Repair::Did(format!(
            "set {PREFIX_KEY} = {top}, the prefix already on {count} of this workspace's issues"
        )))
    }
}

// ---------------------------------------------------------------------------
// prefix config
// ---------------------------------------------------------------------------

/// Two places record the prefix. They must not disagree.
struct PrefixAuthority;

const PREFIX_CONFIG: &str = "prefix config";

#[async_trait]
impl Check for PrefixAuthority {
    fn name(&self) -> &'static str {
        PREFIX_CONFIG
    }
    fn category(&self) -> Category {
        Category::Metadata
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        if !dx.in_workspace() {
            return Finding::ok(PREFIX_CONFIG, "no workspace");
        }
        let Some(store) = dx.store().await else {
            return Finding::unknown(
                PREFIX_CONFIG,
                dx.store_error().unwrap_or("the store would not open"),
            );
        };

        let in_file = declared_in_config(dx);
        let in_store = match declared_in_store(store).await {
            Ok(p) => p,
            Err(e) => {
                return Finding::unknown(PREFIX_CONFIG, format!("could not read config: {e}"));
            }
        };

        match (in_file.as_deref(), in_store.as_deref()) {
            (Some(a), Some(b)) if a == b => Finding::ok(
                PREFIX_CONFIG,
                format!("`{a}` in both config.yaml and the store"),
            ),

            // The trap. `bd config set issue.prefix acme` writes the store and
            // echoes back `issue.prefix = acme` — and then `Ctx::prefix` reads
            // config.yaml first and mints `bd-` anyway. The user was told the
            // setting took. It did not.
            (Some(file), Some(store_val)) => Finding::error(
                PREFIX_CONFIG,
                format!("config.yaml says `{file}`, the store says `{store_val}`"),
            )
            .detail(format!(
                ".beads/{CONFIG_FILE} wins, so ids are minted as `{file}-`.\n\
                 `bd config set issue.prefix` writes the store, not the file — if that is\n\
                 how `{store_val}` got there, it never took effect.\n\
                 Another beads implementation reading only the store would mint `{store_val}-`."
            ))
            .fix("bd doctor --fix writes config.yaml's prefix into the store"),

            // The file is authoritative and the store has nothing. Harmless
            // here, wrong for anyone reading the store on its own.
            (Some(file), None) => Finding::warn(
                PREFIX_CONFIG,
                format!("config.yaml says `{file}`; the store records no prefix"),
            )
            .detail(
                "bd itself is fine — config.yaml wins. But a tool that reads only the\n\
                 store (another beads implementation, a script) would fall back to `bd`.",
            )
            .fix("bd doctor --fix writes config.yaml's prefix into the store"),

            // The documented fallback path. Working as designed.
            (None, Some(store_val)) => Finding::ok(
                PREFIX_CONFIG,
                format!("`{store_val}` from the store (config.yaml is silent)"),
            ),

            (None, None) => Finding::warn(
                PREFIX_CONFIG,
                format!("no prefix anywhere; ids are minted as `{FALLBACK_PREFIX}-`"),
            )
            .detail(
                "Neither .beads/config.yaml nor the store names a prefix, so bd uses its\n\
                 fallback — which is the same fallback every unconfigured beads workspace\n\
                 uses. Two such projects mint into one id namespace.",
            )
            .fix("bd config set issue.prefix <something-project-specific>"),
        }
    }

    /// config.yaml is authoritative in this port ([`crate::context::Ctx::prefix`]),
    /// so the repair is not a guess: copy it into the store. The reverse would
    /// change which ids get minted, which a diagnostic has no business doing.
    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(want) = declared_in_config(dx) else {
            // Nothing to propagate. Which prefix a workspace *should* have is
            // not a question doctor can answer.
            return Ok(Repair::Unfixable);
        };
        let Some(store) = dx.store().await else {
            return Ok(Repair::Unfixable);
        };
        if declared_in_store(store).await.ok().flatten().as_deref() == Some(want.as_str()) {
            return Ok(Repair::Unfixable);
        }
        store.set_config(PREFIX_KEY, &want).await?;
        Ok(Repair::Did(format!(
            "set {PREFIX_KEY} = {want} in the store, matching .beads/{CONFIG_FILE}"
        )))
    }
}

// ---------------------------------------------------------------------------
// id format
// ---------------------------------------------------------------------------

/// Ids that bd could not have minted.
struct IdFormat;

const ID_FORMAT: &str = "id format";

#[async_trait]
impl Check for IdFormat {
    fn name(&self) -> &'static str {
        ID_FORMAT
    }
    fn category(&self) -> Category {
        Category::Metadata
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        if !dx.in_workspace() {
            return Finding::ok(ID_FORMAT, "no workspace");
        }
        let Some(store) = dx.store().await else {
            return Finding::unknown(
                ID_FORMAT,
                dx.store_error().unwrap_or("the store would not open"),
            );
        };

        let issues = match store.list_issues(&IssueFilter::default()).await {
            Ok(i) => i,
            Err(e) => return Finding::unknown(ID_FORMAT, format!("could not list issues: {e}")),
        };
        if issues.is_empty() {
            return Finding::ok(ID_FORMAT, "no issues yet");
        }

        let mut bad: Vec<(String, &'static str)> = Vec::new();
        for issue in &issues {
            if let Some(why) = id_problem(&issue.id) {
                bad.push((issue.id.clone(), why));
            }
        }
        if bad.is_empty() {
            return Finding::ok(ID_FORMAT, format!("{} ids, all well-formed", issues.len()));
        }

        let mut detail: String = bad
            .iter()
            .take(MAX_EXAMPLES)
            .map(|(id, why)| format!("{id}: {why}"))
            .collect::<Vec<_>>()
            .join("\n");
        if bad.len() > MAX_EXAMPLES {
            detail.push_str(&format!("\n… and {} more", bad.len() - MAX_EXAMPLES));
        }

        // Warn, not Error. A malformed id breaks nothing on its own — it means
        // the id was not minted here. `bd import` faithfully preserves the ids
        // of beads authored in another repo (that is the point of import), and
        // failing somebody's pre-commit hook because a peer's id scheme is
        // eight characters wide would be absurd.
        Finding::warn(
            ID_FORMAT,
            format!("{} of {} ids are not bd-shaped", bad.len(), issues.len()),
        )
        .detail(detail)
        .fix(
            "these ids were not minted by this workspace (an import, a hand edit, or a\n\
             legacy sequential scheme). They work; they just will not round-trip through\n\
             bd's id generator. There is no safe automatic repair — renaming an id would\n\
             have to rewrite every dependency edge, comment and event that points at it.",
        )
    }

    // Deliberately unfixable. See the fix text above: an id is a foreign key
    // half the database points at, and doctor is not going to rewrite it.
}

// ---------------------------------------------------------------------------
// config values
// ---------------------------------------------------------------------------

/// Config that parses but means nothing good.
///
/// A *malformed* `config.yaml` is already a hard error at startup, so this check
/// will never see one. Its subject is the other kind: `lease: 0s`, `priority: 7`,
/// `issue_type: tsak`. Every one of these is accepted by serde, and every one of
/// them is then silently discarded at the point of use — `Priority::new` fails
/// and `unwrap_or_default()` hands back P2; `parse::duration` fails and
/// `Ctx::lease` hands back one hour. The setting is not honoured and nothing
/// says so. That is the whole finding.
struct ConfigValues;

const CONFIG_VALUES: &str = "config values";

#[async_trait]
impl Check for ConfigValues {
    fn name(&self) -> &'static str {
        CONFIG_VALUES
    }
    fn category(&self) -> Category {
        Category::Metadata
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        if !dx.in_workspace() {
            return Finding::ok(CONFIG_VALUES, "no workspace");
        }
        let cfg = &dx.ctx.config;
        let mut bad: Vec<(Status, String)> = Vec::new();

        // prefix — only when declared. An absent prefix is the business of
        // `prefix config`, not a bad value.
        if let Some(p) = cfg.prefix.as_deref()
            && let Some(why) = prefix_problem(p.trim())
        {
            bad.push((Status::Error, format!("prefix: {p:?} is invalid — {why}")));
        }

        // actor — the fallback stamped on every event and claim. Upstream also
        // demands a regex here, and its regex rejects `first.last+tag@x.com`,
        // which is a perfectly ordinary git email and exactly what `Ctx` uses
        // when config is silent. So only the things that actually corrupt a row
        // are flagged: emptiness, whitespace, control characters.
        if let Some(a) = cfg.actor.as_deref() {
            if a.trim().is_empty() {
                bad.push((
                    Status::Warn,
                    "actor: is empty — bd will fall back to git, then to \"unknown\"".to_string(),
                ));
            } else if a.chars().any(|c| c.is_whitespace() || c.is_control()) {
                bad.push((
                    Status::Warn,
                    format!("actor: {a:?} contains whitespace or control characters"),
                ));
            }
        }

        // claim.lease — the one that quietly does nothing.
        match parse::duration(&cfg.claim.lease) {
            Err(e) => bad.push((
                Status::Error,
                format!(
                    "claim.lease: {:?} is not a duration ({e}) — every claim silently \
                     falls back to 1h",
                    cfg.claim.lease
                ),
            )),
            Ok(d) if d.is_zero() => bad.push((
                Status::Error,
                format!(
                    "claim.lease: {:?} is zero — a claim would expire the instant it was \
                     taken, and `bd ready` would hand the issue straight back out",
                    cfg.claim.lease
                ),
            )),
            Ok(_) => {}
        }

        // defaults.priority — out of range means every `bd create` silently
        // gets P2 instead of what the config asked for.
        if Priority::new(cfg.defaults.priority).is_err() {
            bad.push((
                Status::Error,
                format!(
                    "defaults.priority: {} is out of range ({}–{}) — every `bd create` \
                     silently uses P{} instead",
                    cfg.defaults.priority,
                    Priority::MIN,
                    Priority::MAX,
                    Priority::default().value(),
                ),
            ));
        }

        // defaults.issue_type — a typo here does not fail, it invents a type.
        let t = cfg.defaults.issue_type.trim();
        if t.is_empty() {
            bad.push((
                Status::Error,
                "defaults.issue_type: is empty — new issues would be created with no type"
                    .to_string(),
            ));
        } else if !IssueType::from(t.to_string()).is_builtin() {
            // Warn, not Error: custom types are a real feature, so this may be
            // deliberate. But nothing in this port *declares* a custom type, so
            // an unrecognized default is far more often a typo than a decision.
            bad.push((
                Status::Warn,
                format!(
                    "defaults.issue_type: {t:?} is not a known type, so every new issue \
                     gets the custom type {t:?} (known: bug, feature, task, epic, chore, \
                     decision, spike, story, milestone)"
                ),
            ));
        }

        if bad.is_empty() {
            return Finding::ok(CONFIG_VALUES, "all configured values are usable");
        }

        let worst = bad.iter().map(|(s, _)| *s).max().unwrap_or(Status::Warn);
        let detail = bad
            .iter()
            .map(|(_, line)| line.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let message = format!(
            "{} configuration value(s) parse but are not usable",
            bad.len()
        );

        let finding = match worst {
            Status::Error => Finding::error(CONFIG_VALUES, message),
            _ => Finding::warn(CONFIG_VALUES, message),
        };
        finding
            .detail(detail)
            .fix(format!("edit .beads/{CONFIG_FILE}"))
    }

    // No repair, deliberately. Resetting a bad value to bd's default is doctor
    // choosing your policy for you — and it would erase the evidence of which
    // setting you had wrong, which is the only thing that would let you fix it.
}

// ---------------------------------------------------------------------------
// project identity
// ---------------------------------------------------------------------------

/// Does this workspace still describe itself?
struct ProjectIdentity;

const PROJECT_IDENTITY: &str = "project identity";

#[async_trait]
impl Check for ProjectIdentity {
    fn name(&self) -> &'static str {
        PROJECT_IDENTITY
    }
    fn category(&self) -> Category {
        Category::Metadata
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(dir) = dx.dir.as_ref() else {
            return Finding::ok(PROJECT_IDENTITY, "no workspace");
        };
        // If `dx.dir` is set, `Ctx::build` loaded a locator or died trying, so
        // this is not an `unwrap` waiting to happen — but it is checked anyway,
        // because doctor's input is broken workspaces.
        let Some(locator) = dx.ctx.locator.as_ref() else {
            return Finding::unknown(PROJECT_IDENTITY, "the workspace has no locator");
        };

        let mut problems: Vec<(Status, String)> = Vec::new();

        // The workspace id is how two clones recognize each other as the same
        // project. `bd init` writes a uuid and preserves it across re-inits for
        // exactly that reason; a blank one forks a workspace from itself.
        let id = locator.workspace_id.trim();
        if id.is_empty() {
            problems.push((
                Status::Error,
                "workspace.json has no workspace_id — clones of this project cannot \
                 recognize each other as the same workspace"
                    .to_string(),
            ));
        } else if id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            problems.push((
                Status::Warn,
                format!("workspace.json: workspace_id {id:?} contains whitespace"),
            ));
        }

        // config.yaml is where the prefix lives. Losing it is the single most
        // common way a workspace drifts into the global `bd-` namespace: the
        // store's `issue.prefix` is the only thing left holding the line, and if
        // it is empty too, every new id collides with the rest of the world.
        let config_path = dir.join(CONFIG_FILE);
        if !config_path.exists() {
            problems.push((
                Status::Warn,
                format!(
                    "{CONFIG_FILE} is missing — the workspace no longer records its own \
                     prefix, actor, lease or defaults"
                ),
            ));
        }

        if problems.is_empty() {
            return Finding::ok(
                PROJECT_IDENTITY,
                format!("{} workspace {}", locator.backend, short(id)),
            );
        }

        let worst = problems
            .iter()
            .map(|(s, _)| *s)
            .max()
            .unwrap_or(Status::Warn);
        let detail = problems
            .iter()
            .map(|(_, l)| l.clone())
            .collect::<Vec<_>>()
            .join("\n");

        let finding = match worst {
            Status::Error => Finding::error(PROJECT_IDENTITY, "the workspace cannot name itself"),
            _ => Finding::warn(
                PROJECT_IDENTITY,
                "the workspace is missing part of its identity",
            ),
        };
        finding.detail(detail).fix(format!(
            "a missing {CONFIG_FILE} can be rebuilt with `bd doctor --fix`; a missing \
             workspace_id has to be pasted into .beads/workspace.json by hand (any uuid \
             will do — it only has to be stable and unique)"
        ))
    }

    /// Rebuild `config.yaml` from what the workspace still knows.
    ///
    /// Only when it is actually absent — this never overwrites a file that is
    /// merely wrong, because `Config::save` serializes the whole struct and
    /// would flatten a user's comments and formatting in the process.
    ///
    /// The workspace_id is *not* repaired: minting one needs a uuid, and bd-cli
    /// does not depend on `uuid` (see the report). A wrong-but-stable id is also
    /// far less dangerous than a freshly-minted one, which would fork this
    /// workspace from every clone of it.
    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> Result<Repair> {
        let Some(dir) = dx.dir.clone() else {
            return Ok(Repair::Unfixable);
        };
        if dir.join(CONFIG_FILE).exists() {
            return Ok(Repair::Unfixable);
        }

        // Recover the prefix from the store rather than defaulting it: writing
        // `prefix: null` into a workspace whose issues are all `acme-` would
        // manufacture the exact drift this family exists to prevent.
        let prefix = match dx.store().await {
            Some(store) => declared_in_store(store).await.ok().flatten(),
            None => None,
        };
        let recovered = prefix.clone();
        let config = Config {
            prefix,
            ..Config::default()
        };
        config.save(&dir)?;

        Ok(Repair::Did(match recovered {
            Some(p) => {
                format!("rewrote .beads/{CONFIG_FILE} (prefix `{p}`, recovered from the store)")
            }
            None => format!("rewrote .beads/{CONFIG_FILE} with bd's defaults"),
        }))
    }
}

fn short(id: &str) -> String {
    match id.char_indices().nth(8) {
        Some((i, _)) => format!("{}…", &id[..i]),
        None => id.to_string(),
    }
}

// ---------------------------------------------------------------------------
// repo fingerprint
// ---------------------------------------------------------------------------

/// Is the database this repository is using actually *this repository's*?
///
/// Upstream answers this properly: it stores a `repo_id` (a hash of the git
/// remote) in the database and compares. This port has nowhere to put one — the
/// `Storage` seam has a config table and no metadata table, and nothing has ever
/// written a fingerprint, so there is nothing to compare against. Inventing the
/// key here would make every existing workspace fail a check about a field bd
/// itself never wrote, which is noise, not a diagnostic.
///
/// What *is* decidable without new machinery is the case the fingerprint was
/// mostly guarding: a git repository whose `.beads` lives outside it.
/// `Locator::discover` walks up like git does, so standing in a clone nested
/// under a directory that has its own `.beads` silently files every issue into
/// the outer project's database — a database that will never travel with the
/// clone.
struct RepoFingerprint;

const REPO_FINGERPRINT: &str = "repo fingerprint";

#[async_trait]
impl Check for RepoFingerprint {
    fn name(&self) -> &'static str {
        REPO_FINGERPRINT
    }
    fn category(&self) -> Category {
        Category::Metadata
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(dir) = dx.dir.as_ref() else {
            return Finding::ok(REPO_FINGERPRINT, "no workspace");
        };
        // Beads does not require git, and a workspace outside one has no
        // repository to be fingerprinted against. Absence is not failure.
        let Some(root) = dx.root.as_ref() else {
            return Finding::ok(REPO_FINGERPRINT, "not a git repository");
        };

        // Both sides must be canonicalized before they are compared. `dx.dir`
        // may carry Windows' `\\?\` verbatim prefix (`Ctx::build` canonicalizes
        // under `-C`) while `dx.root` is whatever `git rev-parse` printed, with
        // forward slashes and no prefix — and `starts_with` between a verbatim
        // and a non-verbatim path is *always false*. Skipping this turns a
        // healthy workspace into a hard finding on Windows only.
        let (Ok(dir), Ok(root)) = (dir.canonicalize(), root.canonicalize()) else {
            return Finding::unknown(
                REPO_FINGERPRINT,
                "could not resolve the workspace and repository paths",
            );
        };

        if dir.starts_with(&root) {
            return Finding::ok(REPO_FINGERPRINT, "the workspace belongs to this repository");
        }

        // Warn, not Error — and this is a deliberate departure from upstream,
        // which fails. One `.beads` deliberately shared across several checkouts
        // is a layout somebody may have chosen on purpose, and `bd doctor` is
        // expected to run from a git hook: failing a commit over a layout that
        // might be intentional is the kind of thing that gets a tool uninstalled.
        // "You have not been told this is fine" is exactly right here.
        Finding::warn(
            REPO_FINGERPRINT,
            "this repository is using a beads workspace that lives outside it",
        )
        .detail(format!(
            "repository: {}\nworkspace:  {}\n\n\
             Issues filed here land in a database that is not part of this repository and\n\
             will not travel with a clone of it. If that is deliberate, ignore this.",
            root.display(),
            dir.display()
        ))
        .fix(format!(
            "to give this repository its own workspace: bd init -C {}",
            root.display()
        ))
    }

    // Unfixable: `bd init` here would create a second workspace, and doctor
    // creating databases behind your back is not a repair.
}

// ---------------------------------------------------------------------------
// bd version tracking
// ---------------------------------------------------------------------------

/// Has the tool moved since anyone last looked at this workspace?
struct VersionTracking;

const VERSION_TRACKING: &str = "bd version tracking";

#[async_trait]
impl Check for VersionTracking {
    fn name(&self) -> &'static str {
        VERSION_TRACKING
    }
    fn category(&self) -> Category {
        Category::Metadata
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        if !dx.in_workspace() {
            return Finding::ok(VERSION_TRACKING, "no workspace");
        }
        let Some(store) = dx.store().await else {
            return Finding::unknown(
                VERSION_TRACKING,
                dx.store_error().unwrap_or("the store would not open"),
            );
        };

        let current = env!("CARGO_PKG_VERSION");
        let acked = match store.get_config(ACKED_KEY).await {
            Ok(v) => v,
            Err(e) => {
                return Finding::unknown(
                    VERSION_TRACKING,
                    format!("could not read {ACKED_KEY}: {e}"),
                );
            }
        };

        let Some(acked) = acked.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
            // Never acked. That is the state of every workspace `bd init` has
            // ever created, and nothing is wrong with it — warning here would
            // paint a fresh, healthy workspace yellow on day one. Absence is not
            // failure.
            return Finding::ok(
                VERSION_TRACKING,
                format!("bd {current} (this workspace has acknowledged no version)"),
            );
        };

        if acked == current {
            return Finding::ok(VERSION_TRACKING, format!("bd {current}, acknowledged"));
        }

        // The acked version lives in the store, so it travels with the repo: a
        // clone inherits whatever version its last collaborator acknowledged.
        // The point is not the binary — `bd version` tells you that. It is that
        // an agent primed against an older bd may be carrying stale instructions
        // in CLAUDE.md / AGENTS.md and has no other way to notice.
        match (order(acked), order(current)) {
            (Some(a), Some(c)) if a < c => Finding::warn(
                VERSION_TRACKING,
                format!(
                    "this workspace was last acknowledged at bd {acked}; you are running {current}"
                ),
            )
            .detail(
                "Agent instructions written against the older version may be stale.".to_string(),
            )
            .fix("bd upgrade review, then bd upgrade ack"),

            (Some(a), Some(c)) if a > c => Finding::warn(
                VERSION_TRACKING,
                format!("this workspace has seen bd {acked}; you are running the older {current}"),
            )
            .detail(
                "A collaborator (or a clone-mate) is on a newer bd than you are. Anything\n\
                 they wrote that depends on it will not be understood here."
                    .to_string(),
            )
            .fix("upgrade bd"),

            _ => Finding::warn(
                VERSION_TRACKING,
                format!("acknowledged version {acked:?} does not match {current}"),
            )
            .fix("bd upgrade ack"),
        }
    }

    // Deliberately unfixable, and this one is worth spelling out: `--fix` could
    // trivially write the current version into `upgrade.acked_version` and make
    // the warning go away. That is precisely why it must not. Acknowledging an
    // upgrade is an assertion that somebody *looked* — a machine silently acking
    // on your behalf erases the only signal that anyone was ever supposed to.
}

/// The comparable part of a version: leading dot-separated integers.
/// `"0.12.3-rc1"` -> `[0, 12, 3]`. Enough to order two `bd` versions without
/// dragging in a semver dependency; anything it cannot parse falls back to a
/// plain "these differ" warning.
fn order(v: &str) -> Option<Vec<u64>> {
    let head = v.split(['-', '+']).next()?;
    let parts: Vec<u64> = head
        .split('.')
        .map(|p| p.parse::<u64>())
        .collect::<Result<_, _>>()
        .ok()?;
    (!parts.is_empty()).then_some(parts)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_splits_from_the_right_so_dashed_prefixes_survive() {
        // `bd init` in `my-project/` derives the prefix `my-project`. Splitting
        // on the *first* dash would read every one of that workspace's ids as
        // prefix `my` — and this check would report the whole database as drift.
        assert_eq!(
            split_id("my-project-a3f2dd"),
            Some(("my-project", "a3f2dd"))
        );
        assert_eq!(split_id("bd-a3f2"), Some(("bd", "a3f2")));
        // The hierarchy tail is not part of the body.
        assert_eq!(split_id("bd-a3f2.1.2"), Some(("bd", "a3f2")));
        assert_eq!(split_id("nodash"), None);
    }

    #[test]
    fn id_format_accepts_everything_the_generator_can_mint() {
        // The check must never condemn an id bd itself produced — across the
        // whole adaptive length range, and at every hierarchy depth.
        for len in MIN_ID_LENGTH..=MAX_ID_LENGTH {
            let id = bd_core::idgen::generate_hash_id("acme", "t", "d", "c", 1, len, 0);
            assert_eq!(id_problem(&id), None, "condemned a minted id: {id}");
            let child = bd_core::idgen::child_id(&id, 2);
            assert_eq!(id_problem(&child), None, "condemned a child id: {child}");
        }
    }

    #[test]
    fn id_format_catches_what_bd_could_not_have_minted() {
        assert!(id_problem("42").is_some(), "no prefix");
        assert!(id_problem("-a3f2").is_some(), "empty prefix");
        assert!(id_problem("bd-").is_some(), "empty body");
        assert!(id_problem("bd-A3F2").is_some(), "base36 is lowercase");
        assert!(id_problem("bd-a3f2!").is_some(), "not base36");
        assert!(id_problem("bd-a3f2dd0e9").is_some(), "too long");
        assert!(
            id_problem("bd-a3f2.x").is_some(),
            "child segment is not a number"
        );
        assert!(id_problem("bd-a3f2.1.2.3.4").is_some(), "too deep");
    }

    #[test]
    fn a_short_numeric_id_is_the_only_honest_sequential_tell() {
        // Upstream sniffs for "sequential ids" and warns. It cannot work here:
        // a base36 body is allowed to be all digits (the idgen docs say so), so
        // `bd-123` is genuinely ambiguous and must be left alone. `bd-1` is not
        // ambiguous — it is below the generator's floor.
        assert!(id_problem("bd-1").is_some());
        assert!(id_problem("bd-42").is_some());
        assert_eq!(id_problem("bd-123"), None);
    }

    #[test]
    fn prefix_validation_does_not_condemn_what_bd_init_derives() {
        // `derive_prefix` takes the project directory name, so a project called
        // `2fa-service` gets the prefix `2fa`. Upstream's rule ("must start with
        // a letter") would fail a workspace bd itself created.
        assert_eq!(prefix_problem("2fa"), None);
        assert_eq!(prefix_problem("my-project"), None);
        assert_eq!(prefix_problem("acme_x1"), None);

        // These are the ones that actually break: `.` is the child separator,
        // whitespace and `:` corrupt `<id>:<type>` parsing.
        assert!(prefix_problem("my.proj").is_some());
        assert!(prefix_problem("my proj").is_some());
        assert!(prefix_problem("bd:x").is_some());
        assert!(prefix_problem("").is_some());
        assert!(prefix_problem(&"x".repeat(MAX_PREFIX_LEN + 1)).is_some());
    }

    #[test]
    fn dominant_prefix_needs_a_strict_plurality() {
        let mut census = BTreeMap::new();
        census.insert("acme".to_string(), 500);
        census.insert("bd".to_string(), 3);
        assert_eq!(dominant(&census), Some(("acme", 500, true)));

        // A tie is not a verdict. Reporting one prefix as "the" prefix here
        // would be a coin flip dressed up as a diagnosis.
        let mut tied = BTreeMap::new();
        tied.insert("a".to_string(), 2);
        tied.insert("b".to_string(), 2);
        assert_eq!(dominant(&tied).map(|(_, _, u)| u), Some(false));

        assert_eq!(dominant(&BTreeMap::new()), None);
    }

    #[test]
    fn version_ordering_survives_prerelease_tags() {
        assert!(order("0.2.0") > order("0.1.9"));
        assert!(order("0.10.0") > order("0.9.0"), "not a string compare");
        assert_eq!(order("0.1.0-rc1"), order("0.1.0"));
        assert_eq!(order("nightly"), None);
    }
}
