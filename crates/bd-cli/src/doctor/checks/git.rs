//! Git Integration — beads lives inside a git repo, but does not require one.
//!
//! Every check here must survive `git` being absent, the directory not being a
//! repository, and `git` being present but the repo being mid-rebase. `Dx::root`
//! is `None` in the first two cases; report [`Finding::unknown`], never an error.
//! "You are not using git" is not a problem with your workspace.
//!
//! The one that bites people: **runtime files that got committed.** A database
//! or a lock file tracked in git turns every pull into a spurious conflict, and
//! the user has no idea why.
//!
//! Belongs here: unresolved conflict markers, git hooks installed or stale,
//! upstream configured, working tree clean, `.gitignore` covering the runtime
//! files, runtime files tracked anyway, `.beads` files untracked that shouldn't
//! be.
//!
//! # The distinction this family is built around
//!
//! `Dx::root` is `None` for three situations that are not the same situation:
//!
//! * **git is not installed.** We cannot read an index we have no tool for. If a
//!   `.git` exists, whether the database is committed is genuinely *unknown*.
//! * **this is not a repository.** Then nothing is tracked, nothing is ignored,
//!   and there is no working tree — every question this family asks is answered,
//!   and the answer is *fine*. Reporting a warning here would fire on every user
//!   who does not use git, which is the definition of noise (seam rule 4).
//! * **git refused.** "detected dubious ownership", a permissions failure, a
//!   corrupt `.git`. There *is* a repository and we could not read it. That is a
//!   warning, and the git error text is the finding.
//!
//! Collapsing those three into "N/A (not a git repository)" — which is what
//! upstream does — reports a repository it could not read as a repository that
//! does not exist. So the family re-asks git once, keeps the reason, and every
//! check branches on [`Git`], never on `Option`.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use tokio::sync::OnceCell;

use super::super::{Category, Check, Dx, Finding, Repair};

/// The git-tracked text form of the issue database.
///
/// Duplicated from `commands::setup`, where it is private. If the two ever
/// disagree, doctor reports on a file nobody writes — which is why
/// `tests/doctor_git.rs` installs the real hook and asserts doctor recognises
/// it, rather than trusting this constant.
const JSONL: &str = "issues.jsonl";

/// The line that marks a git hook as beads'. Also duplicated from
/// `commands::setup` — see the note on [`JSONL`].
const HOOK_MARKER: &str = "beads-managed-hook";

/// The hooks `bd hooks install` writes.
const KNOWN_HOOKS: &[&str] = &["pre-commit", "post-merge"];

pub fn checks() -> Vec<Box<dyn Check>> {
    // One probe, shared by the family. Eight checks each shelling out to
    // `git rev-parse` is eight processes to answer one question, and doctor is
    // expected to be runnable from a git hook (seam rule 5).
    let g = Arc::new(GitCtx::default());
    vec![
        Box::new(Repository(g.clone())),
        // Ignore rules *before* tracked files. `--fix` repairs in registration
        // order, and untracking a database that nothing ignores only lets the
        // next `git add -A` put it straight back.
        Box::new(IgnoreRules(g.clone())),
        Box::new(TrackedRuntimeFiles(g.clone())),
        Box::new(TrackedIssueData(g.clone())),
        Box::new(ConflictMarkers),
        Box::new(UnmergedFiles(g.clone())),
        Box::new(Hooks(g.clone())),
        Box::new(Upstream(g)),
    ]
}

// ---------------------------------------------------------------------------
// What we know about git, resolved once
// ---------------------------------------------------------------------------

/// The three answers, kept apart. See the module docs.
enum Git {
    Repo(Repo),
    /// Determined: there is no git repository here. Not a problem.
    NotARepo,
    /// Undeterminable. Never `Ok`.
    Unknown(String),
}

struct Repo {
    /// As git spells it. Used as the working directory for every `git` we run,
    /// so it is deliberately *not* canonicalised — a Windows verbatim path
    /// (`\\?\C:\…`) is a poor thing to hand to `CreateProcess`.
    root: PathBuf,
    /// `.beads`, relative to the repo root, `/`-separated. `None` when there is
    /// no workspace at all, or when it lies outside this repository.
    beads: Option<String>,
    /// There is a workspace, and it is not under this repository.
    outside: bool,
}

#[derive(Default)]
struct GitCtx {
    git: OnceCell<Git>,
    /// `git ls-files -- .beads`, once. Two checks want it.
    tracked: OnceCell<Result<Vec<String>, String>>,
    /// Which of the [`watched`] paths git's ignore rules cover, once. Two checks
    /// want it.
    ignored: OnceCell<Result<BTreeSet<String>, String>>,
}

impl GitCtx {
    async fn git(&self, dx: &Dx<'_>) -> &Git {
        self.git.get_or_init(|| async { probe(dx) }).await
    }

    /// The repository, or the finding to return instead of one.
    ///
    /// Every git-dependent check begins here, so that "you are not using git"
    /// (fine, `Ok`) and "git could not tell us" (a warning) can never be
    /// confused by an author who forgot the difference.
    async fn repo<'g>(&'g self, dx: &Dx<'_>, name: &'static str) -> Result<&'g Repo, Finding> {
        match self.git(dx).await {
            Git::Repo(r) => Ok(r),
            Git::NotARepo => Err(Finding::na(name, "not a git repository")),
            Git::Unknown(why) => Err(Finding::unknown(name, why.clone())),
        }
    }

    /// The repository *and* the workspace inside it. For checks that have
    /// nothing to say about a repository with no beads in it.
    async fn beads<'g>(
        &'g self,
        dx: &Dx<'_>,
        name: &'static str,
    ) -> Result<(&'g Repo, &'g str), Finding> {
        let repo = self.repo(dx, name).await?;
        match &repo.beads {
            Some(b) => Ok((repo, b.as_str())),
            // Both are `Ok`: neither is a fault, and the one that *is* worth
            // saying out loud (a workspace outside the repo) is said once, by
            // `git repository`, rather than seven times by seven checks.
            None if repo.outside => Err(Finding::ok(name, "the workspace is outside this repository")),
            None => Err(Finding::na(name, "no beads workspace here")),
        }
    }

    async fn tracked<'g>(&'g self, repo: &Repo, beads: &str) -> Result<&'g [String], String> {
        self.tracked
            .get_or_init(|| async { ls_files(&repo.root, beads) })
            .await
            .as_deref()
            .map_err(String::clone)
    }

    async fn ignored<'g>(
        &'g self,
        dx: &Dx<'_>,
        repo: &Repo,
        beads: &str,
    ) -> Result<&'g BTreeSet<String>, String> {
        self.ignored
            .get_or_init(|| async { check_ignore(&repo.root, &watched(dx, beads)) })
            .await
            .as_ref()
            .map_err(String::clone)
    }
}

fn probe(dx: &Dx<'_>) -> Git {
    let root = match &dx.root {
        Some(r) => r.clone(),
        None => match reask(&dx.ctx.cwd) {
            Ok(r) => r,
            Err(g) => return g,
        },
    };
    Git::Repo(locate_beads(root, dx.dir.as_deref()))
}

/// `Dx::root` said `None` and did not say why. Ask again, and keep the reason.
fn reask(cwd: &Path) -> Result<PathBuf, Git> {
    let out = match Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
    {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No git binary. Exactly one question is left that does not need
            // one, and it happens to be the question that decides between "fine"
            // and "cannot say": is there a repository here at all?
            return Err(if dot_git_above(cwd) {
                Git::Unknown(
                    "git is not installed, but there is a .git here — \
                     the repository cannot be inspected"
                        .to_string(),
                )
            } else {
                Git::NotARepo
            });
        }
        Err(e) => return Err(Git::Unknown(format!("cannot run git: {e}"))),
    };

    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return if s.is_empty() {
            Err(Git::Unknown(
                "git rev-parse --show-toplevel succeeded and printed nothing".to_string(),
            ))
        } else {
            Ok(PathBuf::from(s))
        };
    }

    // The *only* determined negative answer. Everything else — dubious
    // ownership, a permissions failure, a corrupt .git — is a repository we
    // could not read, and reporting that as "you have no repository" is a lie
    // that reads as green.
    let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if why.to_ascii_lowercase().contains("not a git repository") {
        Git::NotARepo
    } else if why.is_empty() {
        Git::Unknown(format!("git rev-parse --show-toplevel failed ({})", out.status))
    } else {
        Git::Unknown(why)
    })
}

fn dot_git_above(cwd: &Path) -> bool {
    // `.git` is a directory in a normal clone and a *file* in a linked worktree
    // or a submodule, so this asks about existence, not kind.
    cwd.ancestors().any(|d| d.join(".git").exists())
}

fn locate_beads(root: PathBuf, dir: Option<&Path>) -> Repo {
    let Some(dir) = dir else {
        return Repo { root, beads: None, outside: false };
    };
    // Canonicalise *both* sides before comparing, and neither one after. git
    // prints the root in its own spelling (forward slashes, the drive letter as
    // it found it) while `Dx::dir` is built from the process's cwd; on Windows
    // those are two spellings of one directory that do not compare equal, and a
    // naive `strip_prefix` would conclude that every workspace on the platform
    // is outside its own repository.
    let root_c = canon(&root);
    let dir_c = canon(dir);
    match dir_c.strip_prefix(&root_c) {
        Ok(rel) => {
            let rel: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let beads = (!rel.is_empty()).then(|| rel.join("/"));
            Repo { root, beads, outside: false }
        }
        Err(_) => Repo { root, beads: None, outside: true },
    }
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ---------------------------------------------------------------------------
// Shelling out
// ---------------------------------------------------------------------------

fn git_err(what: &str, out: &Output) -> String {
    let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if e.is_empty() {
        format!("{what} failed ({})", out.status)
    } else {
        format!("{what}: {e}")
    }
}

/// Everything git tracks under `.beads`, relative to the repo root.
fn ls_files(root: &Path, beads: &str) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        // `-z`, because without it git *quotes* any path holding a space or a
        // non-ASCII byte, and a quoted path silently matches none of the
        // patterns below — the file with the awkward name is the one that gets
        // missed, which is precisely the wrong one to miss.
        .args(["ls-files", "-z", "--", beads])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git ls-files: {e}"))?;
    if !out.status.success() {
        return Err(git_err("git ls-files", &out));
    }
    Ok(nul_split(&out.stdout))
}

/// Which of `paths` git's ignore rules cover.
///
/// This asks *git*, not a `.gitignore` file. A rule in the project root, in
/// `.git/info/exclude`, or in `core.excludesFile` ignores the database just as
/// well as one in `.beads/.gitignore`, and a check that only greps the latter
/// warns at users whose setup is already correct.
///
/// `--no-index` is load-bearing: without it git reports a *tracked* file as "not
/// ignored" regardless of the rules, which would make this check and
/// [`TrackedRuntimeFiles`] answer each other's question and make their two
/// repairs order-dependent.
fn check_ignore(root: &Path, paths: &[String]) -> Result<BTreeSet<String>, String> {
    if paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    // `--stdin`, not argv: `git check-ignore -z` refuses to run any other way
    // ("-z only makes sense with --stdin"), and feeding paths through a pipe also
    // means no argv length limit to trip over on Windows.
    let mut child = Command::new("git")
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git check-ignore: {e}"))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "git check-ignore: no stdin".to_string())?;
        let mut buf = Vec::new();
        for p in paths {
            buf.extend_from_slice(p.as_bytes());
            buf.push(0);
        }
        // Writing before reading cannot deadlock here: the input is a handful of
        // short paths, orders of magnitude under a pipe buffer. `stdin` is
        // dropped at the end of this block, which is git's EOF.
        std::io::Write::write_all(&mut stdin, &buf)
            .map_err(|e| format!("cannot write to git check-ignore: {e}"))?;
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("git check-ignore: {e}"))?;
    // 0 = at least one path is ignored, 1 = none are. Both are answers. Anything
    // else is git failing, and must never be read as "nothing is ignored" — that
    // would be the swallowed error that reports as coverage (seam rule 2).
    match out.status.code() {
        Some(0 | 1) => Ok(nul_split(&out.stdout).into_iter().collect()),
        _ => Err(git_err("git check-ignore", &out)),
    }
}

fn nul_split(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Which files are which
// ---------------------------------------------------------------------------

/// The database and everything the engine writes beside it, relative to the repo
/// root. These must be ignored; committing any of them is the bug this family
/// exists to catch.
fn runtime_paths(dx: &Dx<'_>, beads: &str) -> Vec<String> {
    // Ask the locator what the database is called rather than hardcode it. The
    // name belongs to bd-storage, and a check that guesses it wrong reports green
    // on a workspace it never looked at.
    let db = dx
        .ctx
        .locator
        .as_ref()
        .map(|l| l.db_path())
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "beads.db".to_string());
    [db.clone(), format!("{db}-wal"), format!("{db}-shm"), format!("{db}-journal")]
        .iter()
        .map(|n| format!("{beads}/{n}"))
        .collect()
}

/// Every path whose ignore status this family needs — one `git check-ignore` for
/// all of them, once.
fn watched(dx: &Dx<'_>, beads: &str) -> Vec<String> {
    let mut v = runtime_paths(dx, beads);
    v.push(format!("{beads}/{JSONL}"));
    v
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// Runtime state. Tracking it breaks `git pull`.
    Runtime,
    /// A secret. Tracking it is a disclosure.
    Secret,
}

/// Classify a path *relative to `.beads/`*.
///
/// Deliberately conservative: everything it flags is something no human authors
/// and nothing legitimately carries in git. The files this port does want in git
/// — `issues.jsonl`, `config.yaml`, `workspace.json`, `.gitignore` — must fall
/// through, and the unit tests below pin that they do.
fn classify(rel: &str) -> Option<Kind> {
    let base = rel.rsplit('/').next().unwrap_or(rel);

    if matches!(base, ".beads-credential-key" | "credential-key") {
        return Some(Kind::Secret);
    }

    // Whole directories belonging to a storage engine or to bd's own recovery.
    let first = rel.split('/').next().unwrap_or("");
    if rel.contains('/') && matches!(first, "dolt" | "backup" | "export-state") {
        return Some(Kind::Runtime);
    }
    if rel.split('/').any(|c| c.ends_with(".corrupt.backup")) {
        return Some(Kind::Runtime);
    }

    let runtime = base.ends_with(".db")
        || base.ends_with(".sqlite")
        || base.ends_with(".sqlite3")
        // `beads.db-wal`, `beads.db-shm`, `beads.db-journal`, `…sqlite3-wal`.
        || base.contains(".db-")
        || base.contains(".sqlite-")
        || base.contains(".sqlite3-")
        || base.ends_with(".lock")
        || base.ends_with(".pid")
        || base.ends_with(".sock")
        || base.ends_with(".log")
        // An interrupted `Locator::save` leaves `workspace.json.tmp` behind, and
        // `git add -A` will happily commit it.
        || base.ends_with(".tmp");
    runtime.then_some(Kind::Runtime)
}

/// The flagged subset of what git tracks, as repo-relative paths.
fn flagged(tracked: &[String], beads: &str) -> (Vec<String>, Vec<String>) {
    let prefix = format!("{beads}/");
    let mut runtime = Vec::new();
    let mut secret = Vec::new();
    for p in tracked {
        let Some(rel) = p.strip_prefix(&prefix) else { continue };
        match classify(rel) {
            Some(Kind::Secret) => secret.push(p.clone()),
            Some(Kind::Runtime) => runtime.push(p.clone()),
            None => {}
        }
    }
    (runtime, secret)
}

/// A finding's detail is evidence, not a dump. Name enough of them to act on.
fn sample(paths: &[String]) -> String {
    const MAX: usize = 10;
    if paths.len() <= MAX {
        return paths.join("\n");
    }
    let mut s = paths[..MAX].join("\n");
    s.push_str(&format!("\n… and {} more", paths.len() - MAX));
    s
}

// ---------------------------------------------------------------------------
// git repository
// ---------------------------------------------------------------------------

/// Says, once, what this family was able to see. The other seven checks lean on
/// the same probe, so when git is unusable this is the one place the git error
/// text appears in full.
struct Repository(Arc<GitCtx>);

impl Repository {
    const NAME: &'static str = "git repository";
}

#[async_trait]
impl Check for Repository {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn category(&self) -> Category {
        Category::Git
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let repo = match self.0.repo(dx, Self::NAME).await {
            Ok(r) => r,
            Err(f) => return f,
        };

        // A `.beads` outside the repository you are standing in is nearly always
        // an accident — a stray workspace in a parent directory shadowing the
        // project's — and the consequence is severe and completely silent: git
        // carries none of your issues, and nothing else in this family will ever
        // fire, because there is nothing under the repo to look at.
        if repo.outside
            && let Some(d) = &dx.dir
        {
            return Finding::warn(Self::NAME, "the beads workspace is outside this git repository")
                .detail(format!(
                    "repository: {}\nworkspace:  {}\n\ngit is not carrying your issues, and none of the other git checks apply",
                    repo.root.display(),
                    d.display()
                ))
                .fix("if the workspace was meant to live in the repository, run `bd init` there");
        }

        Finding::ok(Self::NAME, format!("git repository at {}", repo.root.display()))
    }
}

// ---------------------------------------------------------------------------
// git ignore rules
// ---------------------------------------------------------------------------

/// Is the database actually ignored — by *any* rule git honours?
struct IgnoreRules(Arc<GitCtx>);

impl IgnoreRules {
    const NAME: &'static str = "git ignore rules";
}

#[async_trait]
impl Check for IgnoreRules {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn category(&self) -> Category {
        Category::Git
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let (repo, beads) = match self.0.beads(dx, Self::NAME).await {
            Ok(x) => x,
            Err(f) => return f,
        };
        let ignored = match self.0.ignored(dx, repo, beads).await {
            Ok(i) => i,
            Err(e) => return Finding::unknown(Self::NAME, e),
        };

        let want = runtime_paths(dx, beads);
        let missing: Vec<String> = want.into_iter().filter(|p| !ignored.contains(p)).collect();
        if missing.is_empty() {
            return Finding::ok(Self::NAME, "the database and its journals are ignored");
        }

        Finding::warn(
            Self::NAME,
            format!("{} runtime file(s) are not ignored by git", missing.len()),
        )
        .detail(format!(
            "{}\n\nOne `git add -A` commits the database, and from then on every \
             `git pull` is a merge conflict in a binary nobody edited.",
            sample(&missing)
        ))
        .fix("`bd doctor --fix` writes the patterns into .beads/.gitignore")
    }

    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> anyhow::Result<Repair> {
        // Re-verify. `repair` is called for every finding that is not `Ok`, and
        // `Finding::unknown` is a *warning* — so a check that could not run at
        // all still gets its repair invoked, and must decline rather than act on
        // an answer it never had.
        let Git::Repo(repo) = self.0.git(dx).await else {
            return Ok(Repair::Unfixable);
        };
        let (Some(beads), Some(dir)) = (&repo.beads, &dx.dir) else {
            return Ok(Repair::Unfixable);
        };

        let want = runtime_paths(dx, beads);
        let ignored = check_ignore(&repo.root, &want).map_err(|e| anyhow!(e))?;
        let missing: Vec<String> = want.into_iter().filter(|p| !ignored.contains(p)).collect();
        if missing.is_empty() {
            return Ok(Repair::Unfixable);
        }

        // Written into `.beads/.gitignore`, not the project's. Patterns there are
        // relative to `.beads/`, so a bare name is the whole pattern and it
        // cannot reach out and match something of the user's by accident.
        let path = dir.join(".gitignore");
        let mut body = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => bail!("cannot read {}: {e}", path.display()),
        };
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(
            "\n# Added by `bd doctor --fix`. The database is a cache of issues.jsonl;\n\
             # committing it makes every `git pull` a conflict in a file nobody edited.\n",
        );
        for p in &missing {
            body.push_str(p.rsplit('/').next().unwrap_or(p));
            body.push('\n');
        }
        std::fs::write(&path, body).map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;

        Ok(Repair::Did(format!(
            "added {} pattern(s) to {} — commit it, so other clones get them too",
            missing.len(),
            path.display()
        )))
    }
}

// ---------------------------------------------------------------------------
// git tracked runtime files
// ---------------------------------------------------------------------------

/// The one that actually bites people.
struct TrackedRuntimeFiles(Arc<GitCtx>);

impl TrackedRuntimeFiles {
    const NAME: &'static str = "git tracked runtime files";
}

#[async_trait]
impl Check for TrackedRuntimeFiles {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn category(&self) -> Category {
        Category::Git
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let (repo, beads) = match self.0.beads(dx, Self::NAME).await {
            Ok(x) => x,
            Err(f) => return f,
        };
        let tracked = match self.0.tracked(repo, beads).await {
            Ok(t) => t,
            Err(e) => return Finding::unknown(Self::NAME, e),
        };

        let (runtime, secret) = flagged(tracked, beads);
        if runtime.is_empty() && secret.is_empty() {
            return Finding::ok(Self::NAME, "no runtime files are tracked");
        }

        // A committed key is the one thing here that is worth failing the run
        // for. Everything else is untidy-and-annoying; this is disclosed.
        if !secret.is_empty() {
            return Finding::error(
                Self::NAME,
                format!("{} credential file(s) are committed to git", secret.len()),
            )
            .detail(format!(
                "{}\n\n`bd doctor --fix` will untrack these, but it CANNOT remove them from \
                 history — every clone and every fork still has the key.",
                sample(&secret)
            ))
            .fix("rotate the key first. Then `bd doctor --fix` to untrack it");
        }

        // Warn, not error: doctor is meant to be runnable from a git hook, and
        // failing the very commit that would fix this helps nobody.
        Finding::warn(
            Self::NAME,
            format!("{} runtime file(s) are tracked by git", runtime.len()),
        )
        .detail(format!(
            "{}\n\nA tracked database makes every `git pull` a merge conflict in a binary \
             nobody edited — and the conflict gives no hint where it came from.",
            sample(&runtime)
        ))
        .fix("`bd doctor --fix` untracks them; the files stay on disk")
    }

    async fn repair(&self, dx: &Dx<'_>, _found: &Finding) -> anyhow::Result<Repair> {
        let Git::Repo(repo) = self.0.git(dx).await else {
            return Ok(Repair::Unfixable);
        };
        let Some(beads) = &repo.beads else {
            return Ok(Repair::Unfixable);
        };

        // Re-derive from git rather than parse the finding: `detail` is written
        // for a human and is truncated at ten paths.
        let tracked = ls_files(&repo.root, beads).map_err(|e| anyhow!(e))?;
        let (mut all, secret) = flagged(&tracked, beads);
        let secrets = secret.len();
        all.extend(secret);
        if all.is_empty() {
            return Ok(Repair::Unfixable);
        }

        // `--cached` is the whole repair. It removes the path from the *index*
        // and never from the working tree, so the database the user is actively
        // using survives untouched. `--force` only silences git's refusal when
        // the index and HEAD disagree; with `--cached` it still cannot delete a
        // file. Nothing here may destroy data.
        for chunk in all.chunks(64) {
            let out = Command::new("git")
                .args(["rm", "--cached", "--force", "--quiet", "--"])
                .args(chunk.iter().map(String::as_str))
                .current_dir(&repo.root)
                .output()
                .map_err(|e| anyhow!("cannot run git rm: {e}"))?;
            if !out.status.success() {
                bail!("{}", git_err("git rm --cached", &out));
            }
        }

        let mut msg = format!(
            "untracked {} file(s) from git — they are still on disk. Commit the removal",
            all.len()
        );
        if secrets > 0 {
            msg.push_str(&format!(
                "; {secrets} of them held credentials and are STILL IN HISTORY — rotate the key"
            ));
        }
        Ok(Repair::Did(msg))
    }
}

// ---------------------------------------------------------------------------
// git tracked issue data
// ---------------------------------------------------------------------------

/// The mirror image: the one file that *should* be in git.
struct TrackedIssueData(Arc<GitCtx>);

impl TrackedIssueData {
    const NAME: &'static str = "git tracked issue data";
}

#[async_trait]
impl Check for TrackedIssueData {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn category(&self) -> Category {
        Category::Git
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let (repo, beads) = match self.0.beads(dx, Self::NAME).await {
            Ok(x) => x,
            Err(f) => return f,
        };
        let tracked = match self.0.tracked(repo, beads).await {
            Ok(t) => t,
            Err(e) => return Finding::unknown(Self::NAME, e),
        };

        let rel = format!("{beads}/{JSONL}");
        if tracked.contains(&rel) {
            return Finding::ok(Self::NAME, "git carries your issues");
        }

        // Absence is not failure: a workspace nobody has exported yet is a
        // perfectly normal workspace, and `bd export` and the pre-commit hook
        // both create this file the first time they run.
        if !dx.beads_path(JSONL).is_some_and(|p| p.is_file()) {
            return Finding::ok(Self::NAME, "nothing exported yet");
        }

        // It exists, and git does not have it. Deliberate, or an accident?
        let ignored = match self.0.ignored(dx, repo, beads).await {
            Ok(i) => i,
            Err(e) => return Finding::unknown(Self::NAME, e),
        };
        if ignored.contains(&rel) {
            return Finding::ok(Self::NAME, "issue data is deliberately gitignored");
        }

        Finding::warn(Self::NAME, "your issues are not in git")
            .detail(format!(
                "{rel} exists but is untracked — a fresh clone of this repository gets no \
                 issues at all, and nobody else can see the ones you have filed."
            ))
            .fix(format!("git add {rel} && git commit -m \"track beads issues\""))
    }
}

// ---------------------------------------------------------------------------
// git conflict markers
// ---------------------------------------------------------------------------

/// Needs no git at all — the markers are just text somebody left in a file. That
/// is the point: they outlive the merge that made them.
struct ConflictMarkers;

impl ConflictMarkers {
    const NAME: &'static str = "git conflict markers";
}

#[async_trait]
impl Check for ConflictMarkers {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn category(&self) -> Category {
        Category::Git
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let Some(dir) = &dx.dir else {
            return Finding::ok(Self::NAME, "no beads workspace here");
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                return Finding::unknown(Self::NAME, format!("cannot read {}: {e}", dir.display()));
            }
        };

        let mut bad = Vec::new();
        let mut unreadable = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            match has_conflict_markers(&p) {
                Ok(true) => bad.push(name),
                Ok(false) => {}
                // A file we could not read is not a file we found clean.
                Err(e) => unreadable.push(format!("{name}: {e}")),
            }
        }

        if !bad.is_empty() {
            return Finding::error(
                Self::NAME,
                format!("{} file(s) hold unresolved conflict markers", bad.len()),
            )
            .detail(format!(
                "{}\n\nThese are not valid JSONL. Any import of them will fail, or worse, \
                 half-succeed.",
                bad.join("\n")
            ))
            .fix("resolve the conflict by hand, then `bd import .beads/issues.jsonl`");
        }
        if !unreadable.is_empty() {
            return Finding::unknown(Self::NAME, unreadable.join("\n"));
        }
        Finding::ok(Self::NAME, "no conflict markers")
    }
}

fn has_conflict_markers(p: &Path) -> std::io::Result<bool> {
    let f = std::fs::File::open(p)?;
    // Streamed, never read whole. This file is the entire issue database in text
    // form, and doctor is expected to be runnable from a pre-commit hook.
    for line in BufReader::new(f).lines() {
        match line {
            Ok(line) if is_conflict_marker(&line) => return Ok(true),
            Ok(_) => {}
            // A line that is not UTF-8 is not a conflict marker. A line we could
            // not read for any *other* reason is a check that did not run.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// Exactly the shapes git writes, and nothing that merely resembles them.
///
/// The looseness is worth avoiding: `starts_with("=======")` also matches the
/// `========` somebody underlined a heading with. Here every line of the file is
/// a JSON object, so these forms cannot occur innocently — but only if they are
/// matched exactly.
fn is_conflict_marker(line: &str) -> bool {
    for m in ["<<<<<<<", "|||||||", ">>>>>>>"] {
        if let Some(rest) = line.strip_prefix(m)
            && (rest.is_empty() || rest.starts_with(' '))
        {
            return true;
        }
    }
    line == "======="
}

// ---------------------------------------------------------------------------
// git unmerged files
// ---------------------------------------------------------------------------

/// The index side of the same wound: git knows the merge is unfinished.
///
/// Scoped to `.beads/`. Warning about the user's dirty working tree in general —
/// which is what upstream does — is not a diagnosis, it is `git status`, and a
/// warning that fires on every developer with uncommitted work trains people to
/// ignore the seven below it.
struct UnmergedFiles(Arc<GitCtx>);

impl UnmergedFiles {
    const NAME: &'static str = "git unmerged files";
}

#[async_trait]
impl Check for UnmergedFiles {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn category(&self) -> Category {
        Category::Git
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let (repo, beads) = match self.0.beads(dx, Self::NAME).await {
            Ok(x) => x,
            Err(f) => return f,
        };

        let out = match Command::new("git")
            .args(["status", "--porcelain", "-z", "--", beads])
            .current_dir(&repo.root)
            .output()
        {
            Ok(o) => o,
            Err(e) => return Finding::unknown(Self::NAME, format!("cannot run git status: {e}")),
        };
        if !out.status.success() {
            return Finding::unknown(Self::NAME, git_err("git status", &out));
        }

        let unmerged: Vec<String> = nul_split(&out.stdout)
            .into_iter()
            .filter_map(|rec| {
                // Porcelain v1 with -z: `XY PATH\0`. A rename emits its original
                // path as a *second* record with no XY; length and the space at
                // index 2 are what tell the two apart.
                let b = rec.as_bytes();
                if b.len() < 4 || b[2] != b' ' {
                    return None;
                }
                let xy = &rec[..2];
                (xy.contains('U') || xy == "DD" || xy == "AA").then(|| rec[3..].to_string())
            })
            .collect();

        if unmerged.is_empty() {
            return Finding::ok(Self::NAME, "no unmerged beads files");
        }
        Finding::error(
            Self::NAME,
            format!("{} beads file(s) are mid-merge", unmerged.len()),
        )
        .detail(format!(
            "{}\n\ngit is holding both sides. Until the merge is finished, the database and \
             the text form disagree about what your issues are.",
            sample(&unmerged)
        ))
        .fix("finish the merge: resolve the file, `git add` it, then `bd import .beads/issues.jsonl`")
    }
}

// ---------------------------------------------------------------------------
// git hooks
// ---------------------------------------------------------------------------

/// Installed, or installed and useless.
///
/// *Not* installed is neither: hooks are opt-in and `bd export` does the same job
/// by hand, so an absent hook is a choice, not a fault (seam rule 4). Upstream
/// warns here, and the warning fires at everyone who never wanted hooks.
struct Hooks(Arc<GitCtx>);

impl Hooks {
    const NAME: &'static str = "git hooks";
}

#[async_trait]
impl Check for Hooks {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn category(&self) -> Category {
        Category::Git
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let repo = match self.0.repo(dx, Self::NAME).await {
            Ok(r) => r,
            Err(f) => return f,
        };
        let dir = match hooks_dir(&repo.root) {
            Ok(d) => d,
            Err(e) => return Finding::unknown(Self::NAME, e),
        };

        let mut ours = Vec::new();
        let mut stale = Vec::new();
        let mut empty = Vec::new();
        let mut foreign = Vec::new();
        for hook in KNOWN_HOOKS {
            let path = dir.join(hook);
            match std::fs::read_to_string(&path) {
                Ok(body) if body.contains(HOOK_MARKER) => {
                    ours.push(*hook);
                    // Carries our marker but does not call us: an install from a
                    // bd that spelled the command differently. It will run, and
                    // it will do nothing but print an error into the commit.
                    if !body.contains(&format!("bd hooks run {hook}")) {
                        stale.push(*hook);
                    }
                }
                Ok(_) => foreign.push(*hook),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => empty.push(*hook),
                Err(e) => {
                    return Finding::unknown(
                        Self::NAME,
                        format!("cannot read {}: {e}", path.display()),
                    );
                }
            }
        }

        if ours.is_empty() {
            let mut f = Finding::ok(Self::NAME, "no beads hooks installed");
            if !foreign.is_empty() {
                f = f.detail(format!(
                    "hooks beads did not write, and will not touch: {}",
                    foreign.join(", ")
                ));
            }
            return f;
        }

        let mut problems = Vec::new();
        if !stale.is_empty() {
            problems.push(format!(
                "stale: {} carry bd's marker but do not call `bd hooks run`",
                stale.join(", ")
            ));
        }
        if !empty.is_empty() {
            problems.push(format!(
                "half-installed: nothing occupies {}",
                empty.join(", ")
            ));
        }
        if !on_path("bd") {
            // The hook opens with `command -v bd || exit 0` — by design, so that
            // an uninstalled bd can never block a commit. The cost of that design
            // is this exact failure: the hook runs, finds nothing, exits green,
            // and every commit from now on carries a stale issues.jsonl.
            problems.push(
                "`bd` is not on PATH: the installed hooks will exit 0 without doing anything, \
                 and your commits will carry a stale issues.jsonl"
                    .to_string(),
            );
        }

        if problems.is_empty() {
            let mut f = Finding::ok(
                Self::NAME,
                format!("beads hooks installed: {}", ours.join(", ")),
            );
            if !foreign.is_empty() {
                f = f.detail(format!("left alone (not written by beads): {}", foreign.join(", ")));
            }
            return f;
        }

        Finding::warn(
            Self::NAME,
            format!("{} beads hook(s) installed, but not working", ours.len()),
        )
        .detail(problems.join("\n"))
        // Not repaired here on purpose. Writing the hook script would mean a
        // second copy of it living in doctor, and the day the two disagree is the
        // day doctor certifies a hook nobody ships.
        .fix("run `bd hooks install` (it will not overwrite a hook beads did not write)")
    }
}

/// Where git will actually look for hooks — which is not always `.git/hooks`.
///
/// `core.hooksPath` moves it, and a linked worktree splits it. `rev-parse
/// --git-path hooks` knows about both, and is what `bd hooks install` itself
/// asks, so the two cannot disagree about where a hook lives.
fn hooks_dir(root: &Path) -> Result<PathBuf, String> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git rev-parse: {e}"))?;
    if !out.status.success() {
        return Err(git_err("git rev-parse --git-path hooks", &out));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err("git rev-parse --git-path hooks printed nothing".to_string());
    }
    let p = PathBuf::from(s);
    // Relative output is relative to the cwd we ran it in.
    Ok(if p.is_absolute() { p } else { root.join(p) })
}

fn on_path(exe: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| {
        d.join(exe).is_file() || (cfg!(windows) && d.join(format!("{exe}.exe")).is_file())
    })
}

// ---------------------------------------------------------------------------
// git upstream
// ---------------------------------------------------------------------------

/// Are there issues upstream that you have not pulled?
///
/// Deliberately *not* "you are 3 commits ahead". Ahead, behind and diverged fire
/// on every developer with unpushed work — upstream warns on all three, and a
/// warning that is true almost always is a warning nobody reads. The one state
/// that is both rare and actionable is this: somebody else changed the issues,
/// you have not merged it, and so `bd ready` is quietly answering from a stale
/// list and you are about to redo work that is already done.
struct Upstream(Arc<GitCtx>);

impl Upstream {
    const NAME: &'static str = "git upstream";
}

#[async_trait]
impl Check for Upstream {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn category(&self) -> Category {
        Category::Git
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let (repo, beads) = match self.0.beads(dx, Self::NAME).await {
            Ok(x) => x,
            Err(f) => return f,
        };

        let out = match Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
            .current_dir(&repo.root)
            .output()
        {
            Ok(o) => o,
            Err(e) => return Finding::unknown(Self::NAME, format!("cannot run git rev-parse: {e}")),
        };
        // No upstream, no remote, detached HEAD, an unborn branch — git fails for
        // all of them, and every one means the same thing here: there is nothing
        // to be behind. None of them is a fault.
        if !out.status.success() {
            return Finding::ok(Self::NAME, "no upstream branch to compare against");
        }
        let upstream = String::from_utf8_lossy(&out.stdout).trim().to_string();

        let rel = format!("{beads}/{JSONL}");
        let out = match Command::new("git")
            // Three dots: what changed on the upstream side since we diverged.
            // Purely local — it reads the last-fetched ref and never touches the
            // network, which is what lets this run from a hook.
            .args(["diff", "--quiet", "HEAD...@{u}", "--", &rel])
            .current_dir(&repo.root)
            .output()
        {
            Ok(o) => o,
            Err(e) => return Finding::unknown(Self::NAME, format!("cannot run git diff: {e}")),
        };

        match out.status.code() {
            Some(0) => Finding::ok(Self::NAME, format!("issue data matches {upstream}")),
            Some(1) => Finding::warn(Self::NAME, "there are issues upstream you have not pulled")
                .detail(format!(
                    "{rel} differs from {upstream}, as of your last `git fetch`.\n\
                     Until you pull, `bd ready` and `bd list` are answering from a stale copy — \
                     and work that is already filed will look like work nobody has filed."
                ))
                .fix("git pull --rebase   (the post-merge hook imports; without it, `bd import .beads/issues.jsonl`)"),
            _ => Finding::unknown(Self::NAME, git_err("git diff", &out)),
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The predicate is the whole check. Everything it flags gets untracked by
    /// `--fix`, so a false positive here is `--fix` quietly removing a file the
    /// user meant to version — which is why the *negative* cases matter at least
    /// as much as the positive ones.
    #[test]
    fn classify_flags_runtime_and_leaves_the_versioned_files_alone() {
        for f in [
            "beads.db",
            "beads.db-wal",
            "beads.db-shm",
            "beads.db-journal",
            "ephemeral.sqlite3",
            "ephemeral.sqlite3-wal",
            "bd.sock",
            "daemon.pid",
            "daemon.log",
            "sync.lock",
            "workspace.json.tmp",
            "dolt/manifest",
            "backup/2026.jsonl",
            "dolt.20260312T123507Z.corrupt.backup/x",
        ] {
            assert_eq!(classify(f), Some(Kind::Runtime), "{f} should be flagged");
        }

        // These are the files this port *wants* in git. `--fix` untracking any
        // of them would delete the user's issue history from the repository.
        for f in ["issues.jsonl", "config.yaml", "workspace.json", ".gitignore", "metadata.json"] {
            assert_eq!(classify(f), None, "{f} must never be flagged");
        }

        assert_eq!(classify(".beads-credential-key"), Some(Kind::Secret));
    }

    #[test]
    fn conflict_markers_are_matched_exactly() {
        for line in ["<<<<<<< HEAD", "=======", ">>>>>>> theirs", "||||||| base", ">>>>>>>"] {
            assert!(is_conflict_marker(line), "{line:?} is a conflict marker");
        }
        for line in [
            r#"{"id":"bd-1","title":"a"}"#,
            "========",       // a heading underline, not a marker
            "<<<<<<<<",       // eight, not seven
            "=== ===",
            "",
            "  <<<<<<< HEAD", // markers are at column zero or they are not markers
        ] {
            assert!(!is_conflict_marker(line), "{line:?} is not a conflict marker");
        }
    }

    #[test]
    fn flagged_splits_runtime_from_secrets_and_ignores_everything_outside_beads() {
        let tracked = vec![
            ".beads/beads.db".to_string(),
            ".beads/issues.jsonl".to_string(),
            ".beads/.beads-credential-key".to_string(),
            "src/main.rs".to_string(), // not under .beads; not ours to judge
        ];
        let (runtime, secret) = flagged(&tracked, ".beads");
        assert_eq!(runtime, vec![".beads/beads.db".to_string()]);
        assert_eq!(secret, vec![".beads/.beads-credential-key".to_string()]);
    }

    /// On Windows `git rev-parse` prints `C:/x/y` and `Dx::dir` holds
    /// `\\?\C:\x\y\.beads`. If those are compared as-is, every workspace on the
    /// platform is "outside its own repository" and the entire family goes
    /// silent — while reporting `Ok`.
    #[test]
    fn the_workspace_is_found_inside_its_repo_whatever_the_platform_spells_it() {
        let tmp = std::env::temp_dir().join(format!("bd-git-loc-{}", std::process::id()));
        let beads = tmp.join(".beads");
        std::fs::create_dir_all(&beads).unwrap();

        // The root, spelled the way git spells it: forward slashes, no verbatim
        // prefix. The workspace, spelled the way std does.
        let root = PathBuf::from(tmp.to_string_lossy().replace('\\', "/"));
        let repo = locate_beads(root, Some(&beads));
        assert_eq!(repo.beads.as_deref(), Some(".beads"), "the workspace is inside the repo");
        assert!(!repo.outside);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_workspace_outside_the_repo_is_seen_as_outside() {
        let tmp = std::env::temp_dir().join(format!("bd-git-out-{}", std::process::id()));
        let root = tmp.join("repo");
        let beads = tmp.join("elsewhere").join(".beads");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&beads).unwrap();

        let repo = locate_beads(root, Some(&beads));
        assert!(repo.outside);
        assert!(repo.beads.is_none());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sample_names_the_evidence_and_stops() {
        let many: Vec<String> = (0..25).map(|i| format!("f{i}")).collect();
        let s = sample(&many);
        assert!(s.contains("f0") && s.contains("f9"));
        assert!(s.contains("… and 15 more"));
        assert!(!s.contains("f10"));
    }
}
