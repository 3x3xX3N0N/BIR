//! Everything a command needs, resolved exactly once.
//!
//! Upstream does this in a ~550-line `PersistentPreRunE`. The work is genuinely
//! ordered — you cannot resolve identity before you know the working directory,
//! and you cannot open a store before you know the backend — but it is small,
//! and it should be testable. So: one function, six steps, no globals.
//!
//! The rule that matters (storage rule 3): **the backend comes from the
//! locator on disk.** Not from a flag, not from the environment. `--backend`
//! exists only on `bd init`, where there is nothing on disk to contradict.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use bd_storage::{Backend, Identity, Locator, Storage};
use chrono::Duration;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::cli::Cli;
use crate::output::Out;
use crate::parse;

/// Whether a command must be standing in a workspace at all.
///
/// Note what is *not* here: any notion of "this command opens the database".
/// The store opens lazily, on first use, so a command that never asks for one
/// never pays for one — and a stub can still exit cleanly with "not implemented"
/// rather than dying on a database error it had no reason to touch.
///
/// This used to be a third variant, and it was a trap. It meant a
/// hand-maintained list of every command that touches the store, so implementing
/// a command required *also* remembering to reclassify it — and forgetting gave
/// you a command that compiled, passed its tests, and then failed at runtime
/// with a bogus "no beads workspace found". Laziness removes the list, and with
/// it the whole failure mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Runs before a workspace exists: `init`, `version`, `doctor`, …
    Nothing,
    /// Must be inside a workspace. Whether it opens the database is its own
    /// business.
    Workspace,
}

pub struct Ctx {
    pub cwd: PathBuf,
    pub locator: Option<Locator>,
    pub identity: Identity,
    pub config: Config,
    pub out: Out,
    pub readonly: bool,
    /// The `.beads` directory we found, **even if nothing in it would load**.
    ///
    /// `locator` is `None` in two very different situations — there is no
    /// workspace, and there is a workspace whose `workspace.json` is corrupt —
    /// and `bd doctor` exists to tell them apart.
    pub beads_dir: Option<PathBuf>,
    /// Why the locator would not load. `Some` only under [`Need::Nothing`].
    pub locator_error: Option<String>,
    /// Why `config.yaml` would not parse. `Some` only under [`Need::Nothing`].
    pub config_error: Option<String>,
    /// Opened on first [`Ctx::store`], at most once.
    store: OnceCell<Box<dyn Storage>>,
}

impl Ctx {
    pub async fn build(cli: &Cli, need: Need) -> Result<Ctx> {
        // 1. Working directory.
        let cwd = match &cli.directory {
            Some(d) => std::fs::canonicalize(d)
                .with_context(|| format!("-C {}: no such directory", d.display()))?,
            None => std::env::current_dir().context("cannot determine the working directory")?,
        };

        // 2/3. Discover the workspace and read the locator. `--db` short-circuits
        // discovery but still goes through the locator: it names *where* the
        // workspace is, never *what kind* it is.
        let beads_dir = match &cli.db {
            Some(p) => Some(beads_dir_for_db(p)?),
            None => Locator::discover(&cwd),
        };
        // A workspace whose locator will not load is a *fault to diagnose*, not a
        // reason to refuse to exist — but only for the commands that run without
        // a workspace at all. `bd close` on a corrupt workspace must still fail
        // loudly; `bd doctor` on one must still run, because that is the entire
        // reason `bd doctor` exists.
        //
        // This was wrong for one wave, and four separate agents reported it
        // independently: `bd doctor` on a workspace with a corrupt
        // `workspace.json` printed "cannot read the workspace" and ran ZERO
        // checks — on precisely the input the command is for.
        let mut locator_error = None;
        let mut config_error = None;

        let locator = match &beads_dir {
            Some(dir) => match Locator::load(dir) {
                Ok(l) => Some(l),
                Err(e) if need == Need::Nothing => {
                    locator_error = Some(format!("{e:#}"));
                    None
                }
                Err(e) => {
                    return Err(anyhow::Error::new(e)
                        .context(format!("cannot read the workspace at {}", dir.display())));
                }
            },
            None => None,
        };
        if locator.is_none() && need != Need::Nothing {
            bail!("no beads workspace found (run `bd init`)");
        }

        // 5. Config, before identity: it can supply a default actor.
        //
        // Same rule. A single typo in `.beads/config.yaml` used to stop `bd
        // doctor` from starting — which is absurd, since a malformed config is
        // exactly the sort of thing you run the doctor to find.
        let config = match beads_dir.as_deref() {
            Some(dir) => match Config::load(dir) {
                Ok(c) => c,
                Err(e) if need == Need::Nothing => {
                    config_error = Some(format!("{e:#}"));
                    Config::default()
                }
                Err(e) => return Err(e),
            },
            None => Config::default(),
        };

        // 4. Identity. Flag (or its env var, which clap folds in) wins, then
        // config, then git, then a placeholder — never a hard failure, because
        // "who am I" must not be able to stop you from filing a bug.
        let actor = cli
            .actor
            .clone()
            .or_else(|| config.actor.clone())
            .or_else(|| git_email(&cwd))
            .unwrap_or_else(|| "unknown".to_string());
        let identity = Identity {
            actor,
            session: std::env::var("BEADS_SESSION").ok().filter(|s| !s.is_empty()),
        };

        let out = Out::new(cli.json(), cli.no_color, cli.quiet, cli.verbose);

        // 6. The store is NOT opened here. See `Ctx::store`.
        Ok(Ctx {
            cwd,
            locator,
            identity,
            config,
            out,
            readonly: cli.readonly,
            beads_dir,
            locator_error,
            config_error,
            store: OnceCell::new(),
        })
    }

    /// The open store, opening it if this is the first ask.
    ///
    /// This is the one place outside `init` that names a concrete backend, and
    /// it reads the backend from the locator — never from a flag or the
    /// environment (seam rule 3). Everything above gets a `&dyn Storage` and
    /// never learns what it got.
    ///
    /// # The version gate
    ///
    /// Opening also checks the database's schema version stamp against
    /// [`bd_storage::SCHEMA_VERSION`], and a mismatch is a refusal with the
    /// exact next step in it — `bd migrate` when the database is behind,
    /// "upgrade bd" when it is ahead. The alternative is what upstream users
    /// live with: a version-skewed database limps into raw SQL errors three
    /// queries in, or worse, into answers that are quietly wrong. One extra
    /// read per process is what that costs.
    ///
    /// Two commands step around the gate via [`Ctx::store_unchecked`]:
    /// `migrate`, which exists to fix exactly what the gate refuses, and
    /// `doctor`, whose job is to examine databases other commands refuse.
    pub async fn store(&self) -> Result<&dyn Storage> {
        self.store_inner(true).await
    }

    /// [`Ctx::store`] without the version gate. For `migrate` and `doctor`
    /// only — anything else that reaches for this is opting into undefined
    /// queries against a shape this build has never seen.
    pub async fn store_unchecked(&self) -> Result<&dyn Storage> {
        self.store_inner(false).await
    }

    async fn store_inner(&self, gate: bool) -> Result<&dyn Storage> {
        let store = self
            .store
            .get_or_try_init(|| async {
                let l = self
                    .locator
                    .as_ref()
                    .ok_or_else(|| anyhow!("no beads workspace found (run `bd init`)"))?;
                self.out.detail(format!(
                    "opening {} workspace at {}",
                    l.backend,
                    l.dir.display()
                ));
                let store = open_store(l, self.identity.clone()).await?;
                if gate {
                    ensure_schema_current(store.as_ref()).await?;
                }
                Ok::<_, anyhow::Error>(store)
            })
            .await?;
        Ok(store.as_ref())
    }

    /// The store *if it is already open* — never opens one.
    ///
    /// For capability probes, which run on the stub path and must not drag a
    /// database into existence just to report that a command is unavailable.
    pub fn try_store(&self) -> Option<&dyn Storage> {
        self.store.get().map(|s| s.as_ref())
    }

    pub fn locator(&self) -> Result<&Locator> {
        self.locator
            .as_ref()
            .ok_or_else(|| anyhow!("no beads workspace found (run `bd init`)"))
    }

    /// The workspace's backend, from disk. `None` outside a workspace.
    pub fn backend(&self) -> Option<Backend> {
        self.locator.as_ref().map(|l| l.backend)
    }

    /// Refuse a write under `--readonly` before it reaches the store, so a
    /// dry-run cannot half-apply.
    pub fn ensure_writable(&self, op: &str) -> Result<()> {
        if self.readonly {
            bail!("--readonly: refusing to {op}");
        }
        Ok(())
    }

    /// The id prefix for newly minted issues.
    ///
    /// The workspace's own config is authoritative (it is what `bd init` wrote);
    /// the store is consulted only as a fallback, for workspaces created by
    /// another beads implementation. The key names are string literals rather
    /// than a backend constant on purpose — naming `bd_sqlite` here would put a
    /// concrete backend on the far side of the seam.
    pub async fn prefix(&self) -> String {
        if let Some(p) = self.config.prefix.clone().filter(|p| !p.is_empty()) {
            return p;
        }
        if let Ok(store) = self.store().await {
            for key in ["issue.prefix", "prefix"] {
                if let Ok(Some(p)) = store.get_config(key).await
                    && !p.is_empty()
                {
                    return p;
                }
            }
        }
        "bd".to_string()
    }

    /// How long a claim is held. Configurable because a five-minute agent and a
    /// day-long human want very different answers.
    pub fn lease(&self) -> Duration {
        parse::duration(&self.config.claim.lease).unwrap_or_else(|_| Duration::hours(1))
    }

    pub async fn close(self) {
        // Only if something actually opened it. A stub never did.
        if let Some(s) = self.store.into_inner() {
            let _ = s.close().await;
        }
    }
}

/// The version handshake: refuse, precisely, a database whose schema stamp
/// does not match this build.
///
/// A raw stamp of 0 (a database from before this port stamped versions) reads
/// as v1 — the one shape that ever shipped unversioned — so existing
/// workspaces pass with no ceremony. See [`bd_storage::effective_schema_version`].
async fn ensure_schema_current(store: &dyn Storage) -> Result<()> {
    let raw = store.schema_version().await?;
    let v = bd_storage::effective_schema_version(raw);
    let speaks = bd_storage::SCHEMA_VERSION;
    match v.cmp(&speaks) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => bail!(
            "this workspace's database records schema v{v}; this build of bd speaks v{speaks}.\n\
             Run `bd migrate` to bring the database up to date in place."
        ),
        std::cmp::Ordering::Greater => bail!(
            "this workspace's database records schema v{v}, but this build of bd speaks \
             v{speaks} — the database was written by a newer bd.\n\
             Upgrade bd; `bd migrate` cannot downgrade a database."
        ),
    }
}

/// The single call site that names a concrete backend (`bd init` is the other).
/// Everything above this line speaks `Box<dyn Storage>`.
async fn open_store(locator: &Locator, identity: Identity) -> Result<Box<dyn Storage>> {
    match locator.backend {
        Backend::Sqlite => Ok(bd_sqlite::open(locator, identity).await?),
        // Dolt starts (or adopts) a `dolt sql-server` and speaks MySQL to it.
        // Everything above this line still only sees `Box<dyn Storage>` — the
        // difference is that this one answers `Some` from the capability
        // accessors, which is what turns `bd branch` from "exit 2, sqlite has no
        // commit graph" into a working command.
        Backend::Dolt => Ok(bd_dolt::open(locator, identity).await?),
        // Not "unknown backend" — a real backend this port has not built. Say so.
        other => Err(bd_storage::Error::unsupported_hint(
            "open",
            match other {
                Backend::Postgres => "postgres",
                _ => "mysql",
            },
            "this port implements the sqlite and dolt backends",
        )
        .into()),
    }
}

/// `--db` may name the database file or the `.beads` directory holding it.
fn beads_dir_for_db(p: &Path) -> Result<PathBuf> {
    if p.is_dir() {
        return Ok(p.to_path_buf());
    }
    p.parent()
        .map(|d| d.to_path_buf())
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("--db {}: expected a database file or a .beads directory", p.display()))
}

/// git is the best guess at who you are, and costs one process. A failure here
/// is not an error: it just means we fall through to "unknown".
fn git_email(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "user.email"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

pub const CONFIG_FILE: &str = "config.yaml";

/// `.beads/config.yaml`. Every field has a default, so a missing or partial
/// file is normal rather than an error — but a *malformed* one is an error,
/// because silently ignoring a typo'd setting is how you spend an afternoon
/// wondering why your lease is still an hour.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Prefix for minted ids (`bd` in `bd-a3f2`).
    pub prefix: Option<String>,
    /// Fallback actor when neither `--actor` nor `$BEADS_ACTOR` is set.
    pub actor: Option<String>,
    pub claim: ClaimConfig,
    pub defaults: Defaults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaimConfig {
    /// Duration string: `30m`, `1h`, `2d`.
    pub lease: String,
}

impl Default for ClaimConfig {
    fn default() -> Self {
        ClaimConfig {
            lease: "1h".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub priority: i32,
    pub issue_type: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            priority: 2,
            issue_type: "task".to_string(),
        }
    }
}

impl Config {
    pub fn load(beads_dir: &Path) -> Result<Config> {
        let path = beads_dir.join(CONFIG_FILE);
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(anyhow!("cannot read {}: {e}", path.display())),
        };
        serde_yaml::from_str(&raw).with_context(|| format!("invalid {}", path.display()))
    }

    pub fn save(&self, beads_dir: &Path) -> Result<()> {
        let path = beads_dir.join(CONFIG_FILE);
        let yaml = serde_yaml::to_string(self)?;
        std::fs::write(&path, yaml).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_when_absent() {
        let dir = std::env::temp_dir().join(format!("bd-cfg-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Config::load(&dir).unwrap();
        assert_eq!(c.claim.lease, "1h");
        assert_eq!(c.defaults.priority, 2);
        assert!(c.prefix.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_round_trips_and_partial_files_are_fine() {
        let dir = std::env::temp_dir().join(format!("bd-cfg-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), "prefix: acme\nclaim:\n  lease: 30m\n").unwrap();
        let c = Config::load(&dir).unwrap();
        assert_eq!(c.prefix.as_deref(), Some("acme"));
        assert_eq!(c.claim.lease, "30m");
        // Untouched sections keep their defaults.
        assert_eq!(c.defaults.issue_type, "task");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_config_is_an_error_not_a_shrug() {
        let dir = std::env::temp_dir().join(format!("bd-cfg-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), "claim: [not, a, map]\n").unwrap();
        assert!(Config::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
