//! Runtime — is `bd` itself installed and coherent.
//!
//! (Named `runtime`, not `install`, on purpose. Windows' installer-detection
//! heuristic auto-elevates any executable whose filename contains "install" or
//! "setup", and cargo names an integration-test binary after its source file. A
//! `doctor_install.rs` test would compile to `doctor_install-<hash>.exe` and
//! prompt for admin rights on every run. Keep the word out of file names.)
//!
//! These are the checks that run *before* a workspace exists, and they are the
//! ones that matter when someone says "beads isn't working" on a fresh machine.
//! Every one of them must work with `dx.dir == None`.
//!
//! The classic: **two `bd` binaries on PATH.** The one being run is not the one
//! the user just installed, every fix appears to do nothing, and nothing in the
//! output would ever tell them.
//!
//! Belongs here: `bd` on PATH (and *which* one, and whether there are several),
//! version agreement between the binary and the workspace, filesystem quirks
//! that break the database (btrfs copy-on-write, network filesystems), legacy
//! references to commands and MCP tools that no longer exist, agent
//! documentation that has drifted from the CLI it describes.
//!
//! # Nothing in this family is an `Error`
//!
//! Not an oversight. Every finding here is a *hazard* or an *ambiguity*, not a
//! determined breakage: a second `bd` on PATH still runs, a database on a
//! network share still opens, a doc that names a dead command is still a doc.
//! `Status::Error` is what makes `bd doctor` exit nonzero, and this family is
//! expected to run from a git hook — failing a commit because someone's home
//! directory is on NFS would be an overreach. Loud `Warn`, with the evidence.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::super::{Category, Check, Dx, Finding};

pub fn checks() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(BdOnPath),
        Box::new(VersionSkew),
        Box::new(DatabaseFilesystem),
        Box::new(StaleReferences),
    ]
}

// ===========================================================================
// bd on PATH — the one this family exists for
// ===========================================================================

const PATH_CHECK: &str = "bd on PATH";

/// Find *every* `bd` on PATH, not the first one.
///
/// `which bd` answers a different question than the one that ruins the
/// afternoon. The user installs a new `bd`, runs it by its full path, sees it
/// work, and then every hook, every agent, and every shell keeps running the old
/// one that is earlier on PATH. Nothing they do appears to have any effect, and
/// no output anywhere mentions the second binary.
///
/// So: enumerate all of them, in PATH order (which is resolution order — the
/// first one *is* the one that runs), and say where the currently-running
/// executable sits in that list.
struct BdOnPath;

#[async_trait]
impl Check for BdOnPath {
    fn name(&self) -> &'static str {
        PATH_CHECK
    }

    fn category(&self) -> Category {
        Category::Runtime
    }

    async fn run(&self, _dx: &Dx<'_>) -> Finding {
        // No workspace needed, and none consulted: this check is about the
        // machine, which is exactly why it is the one that still works when
        // there is nothing else to look at.
        let found = bd_on_path(
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("PATHEXT").as_deref(),
        );
        let running = std::env::current_exe().ok().map(|p| resolve(&p));
        assess_path(&found, running.as_deref())
    }

    // No `repair`. Rewriting a user's PATH is not something a diagnostic gets to
    // do: the fix lives in their shell profile, their installer, or their head,
    // and doctor cannot know which of the two binaries they meant to keep.
}

/// One `bd` found on PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OnPath {
    /// As PATH spells it. This is the string the user has to go and edit.
    shown: PathBuf,
    /// Symlinks resolved. Identity, for deciding whether two entries are really
    /// two binaries.
    real: PathBuf,
}

/// Every `bd` on PATH, in resolution order.
///
/// Takes the environment as arguments rather than reading it, so the logic is
/// testable without mutating the process-wide environment (which is a data race
/// in a threaded test binary, and `std::env::set_var` is `unsafe` for that
/// reason).
fn bd_on_path(path: Option<&OsStr>, pathext: Option<&OsStr>) -> Vec<OnPath> {
    let Some(path) = path else {
        return Vec::new();
    };
    let names = candidate_names(pathext);

    let mut found = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in &names {
            let candidate = dir.join(name);
            if !is_executable(&candidate) {
                continue;
            }
            let real = resolve(&candidate);
            // A symlink farm (`/usr/local/bin/bd` -> `/opt/beads/bd`) and a PATH
            // that lists one directory twice are both *one* binary. Reporting
            // them as a conflict would be a false alarm, and a false alarm here
            // is how the true alarm gets ignored.
            if !seen.insert(real.clone()) {
                continue;
            }
            found.push(OnPath {
                shown: candidate,
                real,
            });
        }
    }
    found
}

/// The filenames that count as `bd` in one directory.
///
/// On Windows a bare `bd` resolves by trying each PATHEXT suffix in order, so
/// `bd.cmd` and `bd.exe` sitting in the same directory really are two different
/// launchers and PATHEXT decides which wins — exactly the ambiguity this check
/// exists to surface. The iteration order below is the resolution order.
#[cfg(windows)]
fn candidate_names(pathext: Option<&OsStr>) -> Vec<String> {
    const DEFAULT: &str = ".COM;.EXE;.BAT;.CMD";
    let raw = pathext
        .and_then(OsStr::to_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT);

    let mut names: Vec<String> = Vec::new();
    for ext in raw.split(';') {
        let ext = ext.trim();
        if ext.len() < 2 || !ext.starts_with('.') {
            continue;
        }
        let name = format!("bd{}", ext.to_ascii_lowercase());
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        names.push("bd.exe".to_string());
    }
    names
}

#[cfg(not(windows))]
fn candidate_names(_pathext: Option<&OsStr>) -> Vec<String> {
    vec!["bd".to_string()]
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Windows has no execute bit; PATHEXT already did the filtering.
#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// Best-effort canonicalization. A path that will not canonicalize is still a
/// path, and losing it would lose the finding.
fn resolve(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// A path a human can read.
///
/// `canonicalize` on Windows hands back the verbatim form — `\\?\C:\bin\bd.exe`
/// — which is correct, unusable, and not what the user typed into PATH. It gets
/// stripped for display and kept for comparison; a finding whose whole value is
/// "here is the path" must print a path the reader recognises.
fn show(p: &Path) -> String {
    let s = p.display().to_string();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix("UNC\\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_string(),
        },
        None => s,
    }
}

/// The whole judgement, as a pure function of what was found — so it can be
/// tested against every shape without an ambient PATH.
fn assess_path(found: &[OnPath], running: Option<&Path>) -> Finding {
    let running_at = running.and_then(|r| found.iter().position(|p| p.real == r));
    let running_str = || running.map(show).unwrap_or_else(|| "unknown".to_string());

    match found {
        // Not an error: you are plainly running bd, so bd works. But everything
        // that invokes it *by name* — git hooks, `bd setup`'s hooks, an agent
        // harness — resolves through PATH and will not find it.
        [] => {
            let detail = match running {
                Some(r) => format!(
                    "running: {}\nnothing on PATH is named `bd`.\n\
                     Anything that runs `bd` by name — a git hook, an agent \
                     integration, a script — will fail with \"command not found\", \
                     even though the binary is right there.",
                    show(r)
                ),
                None => "nothing on PATH is named `bd`, and the running executable \
                     could not be identified either."
                    .to_string(),
            };
            let f = Finding::warn(PATH_CHECK, "no `bd` on PATH").detail(detail);
            match running.and_then(Path::parent) {
                Some(d) => f.fix(format!("put `bd` on PATH — e.g. add {} to it", show(d))),
                None => f.fix("install `bd` somewhere on PATH"),
            }
        }

        // The happy case, and the only one.
        [only] if running_at == Some(0) => {
            Finding::ok(PATH_CHECK, "one `bd` on PATH: the one running").detail(show(&only.shown))
        }

        // One binary, but not this one. Same failure as the two-binary case in
        // miniature: what you just built or installed is not what anything else
        // on this machine will run.
        [only] => Finding::warn(
            PATH_CHECK,
            "the `bd` on PATH is not the binary you are running",
        )
        .detail(format!(
            "on PATH: {}\nrunning: {}\n\
             Every `bd` a shell, a git hook, or an agent starts is the one on PATH. \
             The binary you are running now is not it, so nothing you change about \
             it will have any visible effect.",
            show(&only.shown),
            running_str(),
        ))
        .fix("point PATH at the binary you meant, or run the one PATH already finds"),

        // The classic.
        many => {
            let mut detail = String::new();
            for (i, p) in many.iter().enumerate() {
                let mut tags: Vec<&str> = Vec::new();
                if i == 0 {
                    tags.push("first on PATH — this is the one that runs");
                }
                if running_at == Some(i) {
                    tags.push("currently running");
                }
                let tag = if tags.is_empty() {
                    String::new()
                } else {
                    format!("   <- {}", tags.join(", "))
                };
                detail.push_str(&format!("{}. {}{}\n", i + 1, show(&p.shown), tag));
            }

            let message = format!("{} `bd` binaries on PATH", many.len());
            let (message, why) = match running_at {
                // You are running the loser. This is the afternoon-burner.
                Some(i) if i > 0 => (
                    format!("{message} — you are running #{}, but #1 wins", i + 1),
                    format!(
                        "\nThe `bd` you are running now is #{} on PATH. A shell, a git hook, \
                         or an agent that runs `bd` by name gets #1 instead — a different \
                         binary, possibly a different version. Any fix you apply to the one \
                         you are testing will appear to do nothing.",
                        i + 1
                    ),
                ),
                // The right one wins today, but PATH is still ambiguous and a
                // different shell (or a hook with a different environment) may
                // order it differently.
                Some(_) => (
                    message,
                    "\nThe one you are running is first, so it wins here — but PATH is \
                     ambiguous, and a git hook or a login shell with a different PATH \
                     may resolve `bd` to one of the others."
                        .to_string(),
                ),
                None => (
                    format!("{message}, and none of them is the one you are running"),
                    format!(
                        "\nrunning: {}\nThat binary is not on PATH at all. `bd` by name \
                         resolves to #1, which is a different binary.",
                        running_str()
                    ),
                ),
            };

            Finding::warn(PATH_CHECK, message)
                .detail(format!("{detail}{why}"))
                .fix("remove the `bd` you do not want, or reorder PATH so the one you want is first")
        }
    }
}

// ===========================================================================
// Version skew — the binary vs. what the workspace last saw
// ===========================================================================

const VERSION_CHECK: &str = "bd version skew";

/// The last version of `bd` this workspace was told about.
///
/// This is `commands::setup::ACKED_KEY`, which is private, so the literal is
/// repeated here. It should be shared — see the report; doctor and `bd upgrade`
/// agreeing on this string by coincidence is a bug waiting to happen.
const ACKED_KEY: &str = "upgrade.acked_version";

/// Did the tool move under this workspace?
///
/// # What this check is *not*
///
/// Upstream's `CheckCLIVersion` asks GitHub for the latest release and warns if
/// you are behind. That is not ported, for two reasons, and both are
/// disqualifying on their own:
///
/// 1. **It is a network call.** `bd doctor` is expected to run from a git hook.
///    A hook that blocks for up to five seconds on `api.github.com` — and hangs
///    on a captive-portal wifi — is a hook people disable.
/// 2. **This port's version is not upstream's.** It is `0.1.0`; upstream is on
///    `0.24.x`. A check comparing the two would report *every* installation as
///    catastrophically out of date, forever. A permanently-red check is a check
///    people learn to ignore, and it takes the checks next to it down with it.
///
/// What is left is the version skew that is actually local and actually
/// meaningful: the workspace records the last version of `bd` that was
/// acknowledged against it (`bd upgrade ack`), and an agent primed against that
/// version may be carrying stale instructions. If nobody has ever acknowledged a
/// version, that is not a finding — it means the user does not use the
/// mechanism, and absence is not failure.
struct VersionSkew;

#[async_trait]
impl Check for VersionSkew {
    fn name(&self) -> &'static str {
        VERSION_CHECK
    }

    fn category(&self) -> Category {
        Category::Runtime
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let current = env!("CARGO_PKG_VERSION");

        // No workspace is not "could not check" — there is simply nothing here
        // for the binary to be out of step *with*. Reporting `unknown` on the
        // fresh-machine path (this family's normal case) would put a permanent
        // yellow line in front of every new user.
        if !dx.in_workspace() {
            return Finding::ok(VERSION_CHECK, format!("bd {current}, no workspace"));
        }

        let Some(store) = dx.store().await else {
            return Finding::unknown(
                VERSION_CHECK,
                dx.store_error()
                    .unwrap_or("the store did not open, so the workspace's version is unreadable"),
            );
        };

        let acked = match store.get_config(ACKED_KEY).await {
            Ok(a) => a,
            Err(e) => {
                return Finding::unknown(VERSION_CHECK, format!("cannot read {ACKED_KEY}: {e:#}"));
            }
        };

        let Some(acked) = acked.filter(|a| !a.trim().is_empty()) else {
            // Nobody has run `bd upgrade ack` here. Not a problem — a mechanism
            // the user has not opted into.
            return Finding::ok(
                VERSION_CHECK,
                format!("bd {current}, no version acknowledged in this workspace"),
            );
        };
        let acked = acked.trim();

        match cmp_versions(acked, current) {
            Ordering::Equal => Finding::ok(VERSION_CHECK, format!("bd {current}, acknowledged")),

            // The tool moved forward. Anything primed from this workspace —
            // agent instructions, a cached command list — predates it.
            Ordering::Less => Finding::warn(
                VERSION_CHECK,
                format!("bd moved to {current}; this workspace last acknowledged {acked}"),
            )
            .detail(format!(
                "binary:      {current}\nacknowledged: {acked}\n\
                 Agent instructions primed against {acked} may name commands or flags \
                 that this binary no longer has, or miss ones it gained."
            ))
            .fix("run `bd upgrade review`, then `bd upgrade ack`"),

            // The dangerous direction, and the one upstream does not distinguish:
            // something *newer* than this binary has been writing here. A
            // downgrade, or a colleague on a newer build.
            Ordering::Greater => Finding::warn(
                VERSION_CHECK,
                format!("this workspace was last used by bd {acked}; you are running {current}"),
            )
            .detail(format!(
                "binary:      {current}\nacknowledged: {acked}\n\
                 A newer `bd` has written to this workspace than the one you are \
                 running. Either you downgraded, or PATH is finding an older binary \
                 than the one you installed — see the `bd on PATH` check."
            ))
            .fix("upgrade `bd`, or check which `bd` you are actually running"),
        }
    }

    // No `repair`. `--fix` could write the acknowledgement, and it would be
    // wrong to: the point of the record is that a *human or agent has seen* the
    // change. Silently acking on their behalf destroys the only signal the
    // mechanism carries — and in the `Greater` case it would record a lie.
}

/// Compare dotted numeric versions. Trailing junk (`0.2.0-rc1`) compares as its
/// leading number, which is all this check needs: it asks "did the tool move",
/// not "by how much".
fn cmp_versions(a: &str, b: &str) -> Ordering {
    fn parts(v: &str) -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .map(|p| {
                let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    }
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

// ===========================================================================
// The filesystem under the database
// ===========================================================================

const FS_CHECK: &str = "database filesystem";

/// Is the database sitting on a filesystem that will eventually eat it?
///
/// Two hazards, and they are not the same kind of bad:
///
/// * **A network filesystem.** SQLite's locking is ordinary advisory file
///   locking, and it is documented as unreliable over NFS, SMB/CIFS, and the
///   9p/drvfs mounts that back WSL2's `/mnt/c`. It works — right up until two
///   processes on two machines overlap, and then the database is corrupt and
///   nothing told you. This is the single most common cause of "database disk
///   image is malformed" in the wild. On Windows a mapped network drive is
///   invisible in the path (`Z:\work` looks exactly like `C:\work`), which is
///   why this check asks the OS instead of reading the string.
/// * **btrfs without `nodatacow`.** Copy-on-write plus a file that is written in
///   random 4 KiB pages is a fragmentation and write-amplification engine.
///   Nothing corrupts; it just gets slower forever. Only reported when the flag
///   is genuinely missing — nagging a user who already set `+C` is how a check
///   trains people to skim past it.
///
/// Outside a workspace this looks at the current directory instead: telling
/// somebody their database is on a network share is useful, and telling them
/// *before* they run `bd init` there is better.
struct DatabaseFilesystem;

#[async_trait]
impl Check for DatabaseFilesystem {
    fn name(&self) -> &'static str {
        FS_CHECK
    }

    fn category(&self) -> Category {
        Category::Runtime
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        let (target, existing) = match &dx.dir {
            Some(d) => (d.clone(), true),
            None => (dx.ctx.cwd.clone(), false),
        };

        let facts = match probe_fs(&target) {
            Ok(f) => f,
            // Genuinely undeterminable. Not `Ok` — we did not establish that the
            // filesystem is safe, we established that we cannot tell.
            Err(why) => return Finding::unknown(FS_CHECK, why),
        };

        let where_ = show(&target);
        match facts.hazard {
            None => Finding::ok(FS_CHECK, format!("{} ({})", facts.kind, kind_of_place(existing)))
                .detail(where_.clone()),

            Some(Hazard::Network) => Finding::warn(
                FS_CHECK,
                if existing {
                    format!("the database is on a network filesystem ({})", facts.kind)
                } else {
                    format!("this directory is on a network filesystem ({})", facts.kind)
                },
            )
            .detail(format!(
                "{where_}\nfilesystem: {}\n\
                 SQLite locks with ordinary advisory file locks, and they are not \
                 reliable here. Two machines — or one machine and a stale lock — can \
                 overlap and leave the database corrupt, with no error at the time it \
                 happens. \"database disk image is malformed\", days later, is what \
                 this looks like from the inside.",
                facts.kind
            ))
            .fix(if existing {
                "move the workspace onto a local disk. If it has to live here, make sure exactly one machine ever opens it."
            } else {
                "run `bd init` on a local disk instead — or, if you must, accept that only one machine may ever open it"
            }),

            Some(Hazard::Cow) => Finding::warn(
                FS_CHECK,
                "the database is on btrfs without nodatacow",
            )
            .detail(format!(
                "{where_}\nfilesystem: btrfs, FS_NOCOW_FL not set\n\
                 Copy-on-write turns every small random write into a new extent. A \
                 SQLite file written this way fragments without bound and gets slower \
                 forever. Nothing is corrupt; it just degrades.",
            ))
            .fix(format!(
                "chattr +C {where_}  — then rewrite the existing files, because the flag \
                 only takes effect for new ones:  mv {where_} {where_}.cow && mv {where_}.cow {where_}"
            )),
        }
    }

    // No `repair`. `chattr +C` applies to *new* files only: a `--fix` that set
    // the flag would leave the existing, already-fragmented database exactly as
    // it was, and then report "fixed". That is the lie this seam exists to
    // prevent. The `fix` string tells the user the two-step they actually need.
}

fn kind_of_place(existing: bool) -> &'static str {
    if existing {
        "workspace"
    } else {
        "no workspace here yet"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Hazard {
    /// SQLite over the network. Corruption.
    Network,
    /// Copy-on-write under a random-write database file. Decay.
    ///
    /// Only the Linux `probe_fs` constructs this — btrfs does not exist on the
    /// other targets. It is unreachable rather than dead: the arm in `run` that
    /// handles it has to compile everywhere, because the *check* is the same
    /// check on every platform and only the probe under it differs.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Cow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FsFacts {
    /// What the platform calls this filesystem, for the human to recognise.
    kind: String,
    hazard: Option<Hazard>,
}

// --- Windows -------------------------------------------------------------

/// `canonicalize` gives back the verbatim form — `\\?\C:\...` for a volume,
/// `\\?\UNC\server\share\...` for a share — so UNC needs no syscall. A *mapped*
/// drive does: `Z:\` mapped to `\\nas\share` canonicalizes to `\\?\Z:\` and
/// looks local. `GetDriveTypeW` is the only thing that knows.
#[cfg(windows)]
fn probe_fs(path: &Path) -> Result<FsFacts, String> {
    let real = resolve(path);
    let s = real.to_string_lossy().into_owned();
    let bare = s.strip_prefix(r"\\?\").unwrap_or(&s);

    if let Some(rest) = bare.strip_prefix(r"UNC\") {
        return Ok(FsFacts {
            kind: format!(r"SMB share \\{}", head2(rest)),
            hazard: Some(Hazard::Network),
        });
    }
    // A path that would not canonicalize (it may not exist yet) can still be a
    // plain UNC path.
    if let Some(rest) = bare.strip_prefix(r"\\") {
        return Ok(FsFacts {
            kind: format!(r"SMB share \\{}", head2(rest)),
            hazard: Some(Hazard::Network),
        });
    }

    let mut chars = bare.chars();
    let (Some(letter), Some(':')) = (chars.next(), chars.next()) else {
        return Err(format!(
            "cannot tell what filesystem {} is on: no drive letter in {s}",
            path.display()
        ));
    };
    let letter = letter.to_ascii_uppercase();

    // https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-getdrivetypew
    const DRIVE_UNKNOWN: u32 = 0;
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    match drive_type(&format!("{letter}:\\")) {
        DRIVE_REMOTE => Ok(FsFacts {
            kind: format!("mapped network drive {letter}:"),
            hazard: Some(Hazard::Network),
        }),
        DRIVE_FIXED => Ok(FsFacts {
            kind: format!("local disk {letter}:"),
            hazard: None,
        }),
        DRIVE_RAMDISK => Ok(FsFacts {
            kind: format!("RAM disk {letter}:"),
            hazard: None,
        }),
        DRIVE_REMOVABLE => Ok(FsFacts {
            kind: format!("removable drive {letter}:"),
            hazard: None,
        }),
        DRIVE_CDROM => Ok(FsFacts {
            kind: format!("optical drive {letter}:"),
            hazard: None,
        }),
        code @ (DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR) => Err(format!(
            "GetDriveType({letter}:) could not classify the drive (code {code}), \
             so a network drive cannot be ruled out"
        )),
        code => Ok(FsFacts {
            kind: format!("drive {letter}: (GetDriveType code {code})"),
            hazard: None,
        }),
    }
}

/// `server\share` from the tail of a UNC path.
#[cfg(windows)]
fn head2(rest: &str) -> String {
    rest.split('\\')
        .filter(|s| !s.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join("\\")
}

#[cfg(windows)]
fn drive_type(root: &str) -> u32 {
    // kernel32 is already linked by std, so this costs no dependency — which
    // matters, because adding one would mean editing a frozen Cargo.toml.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated, valid UTF-16, and outlives the call.
    // GetDriveTypeW reads it and returns; it has no other preconditions.
    unsafe { GetDriveTypeW(wide.as_ptr()) }
}

// --- Linux ---------------------------------------------------------------

/// `/proc/self/mounts` gives the filesystem type for free — no `statfs`, no
/// `libc`, no new dependency in a frozen `Cargo.toml`.
#[cfg(target_os = "linux")]
fn probe_fs(path: &Path) -> Result<FsFacts, String> {
    let real = std::fs::canonicalize(path)
        .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
    let mounts = std::fs::read_to_string("/proc/self/mounts")
        .map_err(|e| format!("cannot read /proc/self/mounts: {e}"))?;
    let fstype = fstype_from_proc_mounts(&mounts, &real).ok_or_else(|| {
        format!(
            "no mount in /proc/self/mounts covers {}",
            real.display()
        )
    })?;

    if let Some(h) = network_hazard(&fstype) {
        return Ok(FsFacts {
            kind: fstype,
            hazard: Some(h),
        });
    }
    if fstype == "btrfs" {
        // Only ask when it matters: this is the one branch that spawns a
        // process, and it is reached only on a btrfs volume.
        return match nocow_is_set(&real) {
            Some(true) => Ok(FsFacts {
                kind: "btrfs, nodatacow set".to_string(),
                hazard: None,
            }),
            Some(false) => Ok(FsFacts {
                kind: "btrfs".to_string(),
                hazard: Some(Hazard::Cow),
            }),
            // On btrfs, but the flag is unreadable. Saying `Ok` here would claim
            // coverage we do not have.
            None => Err(format!(
                "{} is on btrfs, but FS_NOCOW_FL could not be read (`lsattr` is not \
                 installed or failed), so copy-on-write cannot be ruled out",
                real.display()
            )),
        };
    }
    Ok(FsFacts {
        kind: fstype,
        hazard: None,
    })
}

/// `lsattr -d` reads FS_NOCOW_FL without an ioctl and without `libc`. `None`
/// means "could not tell", which is not the same as "not set".
#[cfg(target_os = "linux")]
fn nocow_is_set(path: &Path) -> Option<bool> {
    let out = std::process::Command::new("lsattr")
        .arg("-d")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let flags = text.split_whitespace().next()?;
    Some(flags.contains('C'))
}

// --- Other unix (macOS, BSD) ---------------------------------------------

/// No `/proc`, and `statfs` would mean `libc`. `mount` is one process and its
/// output is stable across macOS and the BSDs.
#[cfg(all(unix, not(target_os = "linux")))]
fn probe_fs(path: &Path) -> Result<FsFacts, String> {
    let real = std::fs::canonicalize(path)
        .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
    let out = std::process::Command::new("mount")
        .output()
        .map_err(|e| format!("cannot run `mount` to identify the filesystem: {e}"))?;
    if !out.status.success() {
        return Err("`mount` exited nonzero, so the filesystem is unknown".to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let fstype = fstype_from_mount_output(&text, &real)
        .ok_or_else(|| format!("no mount covers {}", real.display()))?;
    Ok(FsFacts {
        hazard: network_hazard(&fstype),
        kind: fstype,
    })
}

#[cfg(not(any(windows, unix)))]
fn probe_fs(path: &Path) -> Result<FsFacts, String> {
    Err(format!(
        "this platform has no filesystem probe, so {} cannot be classified",
        path.display()
    ))
}

// --- Shared, pure, and therefore testable anywhere ------------------------

/// Filesystems on which SQLite's locking is not trustworthy.
///
/// `9p`/`drvfs`/`v9fs` are how WSL2 exposes `/mnt/c`, and `vboxsf`/`virtiofs`
/// are shared folders from a hypervisor. They are network filesystems with a
/// short wire, and they break locking in exactly the same way — a workspace on
/// `/mnt/c` opened from both Windows and WSL is a corruption waiting to be
/// reported as a beads bug.
#[cfg(any(unix, test))]
const NETWORK_FS: &[&str] = &[
    "nfs",
    "nfs3",
    "nfs4",
    "cifs",
    "smbfs",
    "smb2",
    "smb3",
    "afpfs",
    "afs",
    "ncpfs",
    "9p",
    "v9fs",
    "drvfs",
    "lxfs",
    "vboxsf",
    "virtiofs",
    "davfs",
    "webdav",
    "glusterfs",
    "ceph",
    "lustre",
    "fuse.sshfs",
    "fuse.davfs",
    "fuse.rclone",
    "fuse.s3fs",
    "fuse.gcsfuse",
    "fuse.blobfuse",
];

#[cfg(any(unix, test))]
fn network_hazard(fstype: &str) -> Option<Hazard> {
    NETWORK_FS
        .iter()
        .any(|f| fstype.eq_ignore_ascii_case(f))
        .then_some(Hazard::Network)
}

/// The fstype of the mount that covers `target`, from `/proc/self/mounts`.
///
/// Longest match wins: `/` covers everything, so a `/mnt/c` line has to beat it.
#[cfg(any(unix, test))]
fn fstype_from_proc_mounts(text: &str, target: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (_dev, point, fstype) = (f.next()?, f.next(), f.next());
        let (Some(point), Some(fstype)) = (point, fstype) else {
            continue;
        };
        let point = PathBuf::from(unescape_mount(point));
        if !target.starts_with(&point) {
            continue;
        }
        let depth = point.components().count();
        if best.as_ref().is_none_or(|(d, _)| depth > *d) {
            best = Some((depth, fstype.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

/// `/proc` octal-escapes the characters that would otherwise break the columns.
#[cfg(any(unix, test))]
fn unescape_mount(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let octal: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&octal, 8) {
            Ok(b) if octal.len() == 3 => {
                out.push(b as char);
                chars.nth(2);
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// The fstype of the mount that covers `target`, from BSD/macOS `mount` output:
/// `/dev/disk1s5 on / (apfs, local, journaled)`.
#[cfg(any(unix, test))]
fn fstype_from_mount_output(text: &str, target: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in text.lines() {
        // The mount point sits between the first " on " and the last " (" — and
        // it may contain spaces, which is why neither end is found by splitting.
        let Some(after_on) = line.find(" on ").map(|i| i + 4) else {
            continue;
        };
        let Some(paren) = line.rfind(" (") else {
            continue;
        };
        if paren <= after_on {
            continue;
        }
        let point = PathBuf::from(line[after_on..paren].trim());
        let opts = line[paren + 2..].trim_end_matches(')');
        let Some(fstype) = opts.split(',').next().map(str::trim) else {
            continue;
        };
        if fstype.is_empty() || !target.starts_with(&point) {
            continue;
        }
        let depth = point.components().count();
        if best.as_ref().is_none_or(|(d, _)| depth > *d) {
            best = Some((depth, fstype.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

// ===========================================================================
// Legacy references — docs that name a bd surface this port does not have
// ===========================================================================

const STALE_CHECK: &str = "legacy bd references";

/// Agent documentation that points at a *surface* this binary does not ship.
///
/// Upstream's `CheckLegacyBeadsSlashCommands` and `CheckLegacyMCPToolReferences`,
/// and the distinction between them and doc *drift* is the whole point:
///
/// * **Drift** is `CLAUDE.md` naming `bd cursor-hook` when `bd` has no such
///   command. That is the Integrations family's `agent-docs-drift`, which
///   already derives the answer from the clap tree, and it is deliberately **not
///   duplicated here** — one problem must produce one finding.
/// * **A legacy surface** is `CLAUDE.md` telling an agent to call
///   `mcp__beads_beads__list`, or to run `/beads:quickstart`. This port ships no
///   MCP server and no plugin slash commands *at all*, so these do not resolve
///   to a failing command — they resolve to nothing. The agent has no error to
///   react to. It simply does not use beads, and nobody ever finds out why.
///
/// That second one is a Runtime problem, not an integration problem: it is a
/// statement about what this binary *is*.
///
/// **Absence is not failure.** No agent docs is not a finding. Upstream's
/// `CheckAgentDocumentation` warns when `AGENTS.md` is missing; that is a warning
/// about something the user simply does not use, and it is not ported.
struct StaleReferences;

/// Where agent instructions live. Read-only, and small.
const DOC_FILES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    "claude.local.md",
    ".claude/claude.local.md",
    ".github/copilot-instructions.md",
    "GEMINI.md",
    ".cursor/rules/beads.mdc",
];

/// Nothing anybody hand-writes for an agent is larger than this. Something that
/// is gets reported rather than silently skipped.
const MAX_DOC: u64 = 4 * 1024 * 1024;

#[async_trait]
impl Check for StaleReferences {
    fn name(&self) -> &'static str {
        STALE_CHECK
    }

    fn category(&self) -> Category {
        Category::Runtime
    }

    async fn run(&self, dx: &Dx<'_>) -> Finding {
        // Works with no workspace: the git root, else the workspace root, else
        // just where we are standing.
        let root = dx
            .root
            .clone()
            .or_else(|| dx.dir.as_ref().and_then(|d| d.parent().map(Path::to_path_buf)))
            .unwrap_or_else(|| dx.ctx.cwd.clone());

        let mut hits: Vec<(String, Vec<String>)> = Vec::new();
        let mut unreadable: Vec<String> = Vec::new();
        let mut looked_at = 0usize;

        for rel in DOC_FILES {
            let path = root.join(rel);
            match std::fs::metadata(&path) {
                Ok(m) if !m.is_file() => continue,
                Ok(m) if m.len() > MAX_DOC => {
                    unreadable.push(format!("{rel}: {} bytes, not scanned", m.len()));
                    continue;
                }
                Ok(_) => {}
                // Not there is the common case, and it is fine.
                Err(_) => continue,
            }
            looked_at += 1;
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let found = scan_doc(&text);
                    if !found.is_empty() {
                        hits.push(((*rel).to_string(), found));
                    }
                }
                Err(e) => unreadable.push(format!("{rel}: {e}")),
            }
        }

        if hits.is_empty() {
            // We could not read something we found. That is not `Ok` — we did
            // not establish that it is clean.
            if !unreadable.is_empty() {
                return Finding::unknown(STALE_CHECK, unreadable.join("\n"));
            }
            return Finding::ok(
                STALE_CHECK,
                match looked_at {
                    0 => "no agent documentation to check".to_string(),
                    1 => "1 agent doc, no legacy references".to_string(),
                    n => format!("{n} agent docs, no legacy references"),
                },
            );
        }

        let mut detail = String::new();
        for (file, found) in &hits {
            detail.push_str(file);
            detail.push('\n');
            for f in found {
                detail.push_str("  ");
                detail.push_str(f);
                detail.push('\n');
            }
        }
        for u in &unreadable {
            detail.push_str(&format!("(also, not scanned) {u}\n"));
        }
        detail.push_str(
            "\nThese name a beads that does not exist here. An agent told to call an \
             MCP tool that is not registered does not get an error it can react to — \
             it gets nothing, quietly stops using beads, and nobody finds out why.",
        );

        Finding::warn(
            STALE_CHECK,
            match hits.len() {
                1 => "an agent doc points at a beads surface this build does not have".to_string(),
                n => format!("{n} agent docs point at beads surfaces this build does not have"),
            },
        )
        .detail(detail)
        .fix("delete the MCP/slash-command instructions and re-run `bd setup`; this port drives beads through the CLI")
    }
}

/// The surfaces this port does not ship, under every spelling upstream used.
///
/// A literal-substring scan, not a command scan: these strings are unmistakable
/// (nothing in prose says `mcp__beads_beads__list`), so unlike doc *drift* this
/// needs no code-region parsing and cannot produce a false positive.
fn scan_doc(text: &str) -> Vec<String> {
    let mut hits: Vec<String> = Vec::new();

    // A Claude Code plugin surface. This port registers no slash commands.
    if text.contains("/beads:") {
        hits.push("`/beads:…` slash commands — this port registers none".to_string());
    }
    // MCP tool names. There is no MCP server anywhere in this port, so a doc
    // that tells an agent to call one is telling it to call nothing.
    if let Some(pat) = ["mcp__beads", "mcp__plugin_beads", "mcp_beads_"]
        .into_iter()
        .find(|p| text.contains(p))
    {
        hits.push(format!(
            "`{pat}…` MCP tool names — this port ships no MCP server"
        ));
    }

    hits
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    fn on(shown: &str) -> OnPath {
        OnPath {
            shown: PathBuf::from(shown),
            real: PathBuf::from(shown),
        }
    }

    // --- bd on PATH ------------------------------------------------------

    /// The finding this whole family exists for. Two binaries, and the one being
    /// run is *not* the one PATH resolves to: every fix the user applies goes
    /// into a binary nothing else on the machine ever executes.
    #[test]
    fn two_bd_on_path_lists_them_in_path_order_and_names_the_one_that_wins() {
        let found = [on("/usr/local/bin/bd"), on("/home/x/.cargo/bin/bd")];
        let running = PathBuf::from("/home/x/.cargo/bin/bd");

        let f = assess_path(&found, Some(&running));
        assert_eq!(f.status, Status::Warn);

        let detail = f.detail.expect("the paths ARE the finding");
        // PATH order, because PATH order is resolution order.
        let first = detail.find("/usr/local/bin/bd").unwrap();
        let second = detail.find("/home/x/.cargo/bin/bd").unwrap();
        assert!(first < second, "listed out of PATH order:\n{detail}");
        assert!(detail.contains("first on PATH"), "{detail}");
        assert!(detail.contains("currently running"), "{detail}");
        // And it must say, in words, that the running one loses.
        assert!(f.message.contains("#2"), "{}", f.message);
    }

    /// The same two binaries, but the running one is already first. Still a
    /// warning — PATH is ambiguous and a hook's PATH may not be this one — but
    /// it must not accuse the user of running the wrong binary.
    #[test]
    fn two_bd_on_path_with_the_running_one_first_is_still_flagged_but_not_misreported() {
        let found = [on("/home/x/.cargo/bin/bd"), on("/usr/local/bin/bd")];
        let running = PathBuf::from("/home/x/.cargo/bin/bd");

        let f = assess_path(&found, Some(&running));
        assert_eq!(f.status, Status::Warn);
        assert!(f.message.contains('2'), "{}", f.message);
        assert!(!f.message.contains("#2"), "{}", f.message);
    }

    #[test]
    fn one_bd_on_path_and_it_is_the_one_running_is_the_only_ok() {
        let found = [on("/usr/local/bin/bd")];
        let running = PathBuf::from("/usr/local/bin/bd");
        assert_eq!(assess_path(&found, Some(&running)).status, Status::Ok);
    }

    /// A dev running `target/debug/bd` while an installed `bd` sits on PATH.
    /// Everything they test is a binary nothing else runs.
    #[test]
    fn one_bd_on_path_that_is_not_the_running_one_warns() {
        let found = [on("/usr/local/bin/bd")];
        let running = PathBuf::from("/home/x/beads/target/debug/bd");
        let f = assess_path(&found, Some(&running));
        assert_eq!(f.status, Status::Warn);
        let d = f.detail.unwrap();
        assert!(d.contains("/usr/local/bin/bd") && d.contains("target/debug/bd"), "{d}");
    }

    /// Not an error — bd is obviously running — but hooks and agents resolve by
    /// name, and they will not find it.
    #[test]
    fn no_bd_on_path_warns_and_points_at_the_directory_to_add() {
        let running = PathBuf::from("/opt/beads/bin/bd");
        let f = assess_path(&[], Some(&running));
        assert_eq!(f.status, Status::Warn);
        assert!(f.fix.unwrap().contains("/opt/beads/bin"));
    }

    /// The false alarm that would discredit the real one: one binary, reachable
    /// through two PATH entries (a symlink, or a duplicated directory), is one
    /// binary.
    #[test]
    fn the_same_binary_reached_twice_is_not_two_binaries() {
        let dir = tmp("dedupe");
        let exe = write_fake_bd(&dir);
        let path = std::env::join_paths([&dir, &dir]).unwrap();

        let found = bd_on_path(Some(path.as_os_str()), Some(OsStr::new(".EXE")));
        assert_eq!(found.len(), 1, "found: {found:?}");
        assert_eq!(found[0].real, resolve(&exe));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The real thing, on the real filesystem: two directories, two binaries,
    /// reported in PATH order.
    #[test]
    fn bd_on_path_finds_every_binary_in_path_order() {
        let root = tmp("order");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        write_fake_bd(&first);
        write_fake_bd(&second);

        let path = std::env::join_paths([&first, &second]).unwrap();
        let found = bd_on_path(Some(path.as_os_str()), Some(OsStr::new(".EXE")));
        assert_eq!(found.len(), 2, "found: {found:?}");
        assert!(found[0].shown.starts_with(&first));
        assert!(found[1].shown.starts_with(&second));

        // And reversing PATH reverses the answer, because PATH order is the
        // whole point.
        let path = std::env::join_paths([&second, &first]).unwrap();
        let found = bd_on_path(Some(path.as_os_str()), Some(OsStr::new(".EXE")));
        assert!(found[0].shown.starts_with(&second));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_path_variable_at_all_is_not_a_panic() {
        assert!(bd_on_path(None, None).is_empty());
    }

    /// The finding *is* the path, so the path has to be one the reader can find.
    /// `canonicalize` on Windows returns `\\?\C:\bin\bd.exe`, which is nothing
    /// anybody has in their PATH.
    #[test]
    fn paths_are_shown_the_way_the_user_wrote_them() {
        assert_eq!(show(Path::new(r"\\?\C:\bin\bd.exe")), r"C:\bin\bd.exe");
        assert_eq!(show(Path::new(r"\\?\UNC\nas\team\bd.exe")), r"\\nas\team\bd.exe");
        // Anything already readable is left alone.
        assert_eq!(show(Path::new("/usr/local/bin/bd")), "/usr/local/bin/bd");
    }

    #[cfg(windows)]
    #[test]
    fn pathext_decides_which_names_count_and_in_what_order() {
        let names = candidate_names(Some(OsStr::new(".COM;.EXE;.CMD")));
        assert_eq!(names, ["bd.com", "bd.exe", "bd.cmd"]);
        // An empty or missing PATHEXT still has to find bd.exe.
        assert!(candidate_names(None).contains(&"bd.exe".to_string()));
        assert!(candidate_names(Some(OsStr::new(""))).contains(&"bd.exe".to_string()));
    }

    // --- version skew ----------------------------------------------------

    #[test]
    fn version_comparison_orders_by_number_not_by_string() {
        assert_eq!(cmp_versions("0.1.0", "0.1.0"), Ordering::Equal);
        assert_eq!(cmp_versions("0.9.0", "0.10.0"), Ordering::Less);
        assert_eq!(cmp_versions("0.2", "0.2.0"), Ordering::Equal);
        assert_eq!(cmp_versions("1.0.0", "0.99.99"), Ordering::Greater);
        // Junk degrades to its leading number rather than to a panic.
        assert_eq!(cmp_versions("0.2.0-rc1", "0.2.0"), Ordering::Equal);
        assert_eq!(cmp_versions("v0.3.0", "0.2.0"), Ordering::Greater);
        assert_eq!(cmp_versions("", "0.0.0"), Ordering::Equal);
    }

    /// The port's own version must parse, or the check is comparing garbage.
    #[test]
    fn this_binarys_version_is_a_version() {
        assert_eq!(
            cmp_versions(env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_VERSION")),
            Ordering::Equal
        );
        assert_eq!(cmp_versions(env!("CARGO_PKG_VERSION"), "0.0.0"), Ordering::Greater);
    }

    // --- filesystem ------------------------------------------------------

    /// WSL2 mounts the Windows drives over 9p. A workspace on `/mnt/c` opened
    /// from both sides is the corruption story, and `/` is btrfs-or-ext4 above
    /// it — so the longest match has to win.
    #[test]
    fn proc_mounts_picks_the_longest_matching_mount() {
        let text = "\
/dev/sdc / ext4 rw,relatime 0 0
none /usr/lib/wsl/drivers 9p ro,dirsync 0 0
drvfs /mnt/c 9p rw,noatime,dirsync,aname=drvfs 0 0
tmpfs /run tmpfs rw,nosuid 0 0
";
        assert_eq!(
            fstype_from_proc_mounts(text, Path::new("/mnt/c/work/repo/.beads")).as_deref(),
            Some("9p")
        );
        assert_eq!(
            fstype_from_proc_mounts(text, Path::new("/home/x/repo/.beads")).as_deref(),
            Some("ext4")
        );
        assert_eq!(
            fstype_from_proc_mounts(text, Path::new("/mnt/c")).as_deref(),
            Some("9p")
        );
    }

    #[test]
    fn proc_mounts_unescapes_the_octal_it_writes_for_spaces() {
        let text = "\
/dev/sda1 / ext4 rw 0 0
//nas/team /mnt/my\\040share cifs rw 0 0
";
        assert_eq!(
            fstype_from_proc_mounts(text, Path::new("/mnt/my share/beads/.beads")).as_deref(),
            Some("cifs")
        );
        assert_eq!(unescape_mount("/mnt/my\\040share"), "/mnt/my share");
    }

    #[test]
    fn proc_mounts_survives_a_truncated_line() {
        assert_eq!(fstype_from_proc_mounts("garbage\n\n", Path::new("/x")), None);
    }

    /// macOS/BSD `mount` output. The mount point can contain spaces, so neither
    /// end of it can be found by splitting on whitespace.
    #[test]
    fn bsd_mount_output_is_parsed_including_mount_points_with_spaces() {
        let text = "\
/dev/disk1s5s1 on / (apfs, sealed, local, read-only, journaled)
//guest@nas._smb._tcp.local/team on /Volumes/Team Share (smbfs, nodev, nosuid, mounted by x)
map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)
";
        assert_eq!(
            fstype_from_mount_output(text, Path::new("/Users/x/repo/.beads")).as_deref(),
            Some("apfs")
        );
        assert_eq!(
            fstype_from_mount_output(text, Path::new("/Volumes/Team Share/repo/.beads")).as_deref(),
            Some("smbfs")
        );
    }

    #[test]
    fn the_filesystems_that_break_sqlite_are_the_ones_flagged() {
        for fs in ["nfs4", "cifs", "smbfs", "9p", "drvfs", "vboxsf", "fuse.sshfs"] {
            assert_eq!(network_hazard(fs), Some(Hazard::Network), "{fs}");
        }
        // Local disks, and btrfs (which is a *different* hazard), are not this.
        for fs in ["ext4", "xfs", "apfs", "btrfs", "zfs", "tmpfs", "ntfs"] {
            assert_eq!(network_hazard(fs), None, "{fs}");
        }
    }

    /// The check must classify the directory it is actually standing in, without
    /// a workspace, without panicking, and without inventing a hazard on a
    /// perfectly ordinary local disk.
    #[test]
    fn probing_a_real_local_directory_finds_no_hazard() {
        let dir = tmp("fs");
        let facts = probe_fs(&dir).expect("a temp dir must be classifiable");
        assert_eq!(
            facts.hazard, None,
            "the system temp dir was reported as hazardous: {facts:?}"
        );
        assert!(!facts.kind.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- legacy references ------------------------------------------------

    /// Both spellings of "beads, but not this beads": a plugin slash command and
    /// an MCP tool name. Neither resolves to *anything* here — not to a failing
    /// command, to nothing — so the agent has no error to react to.
    #[test]
    fn dead_slash_commands_and_mcp_tool_names_are_reported() {
        let hits = scan_doc("Start with /beads:quickstart for context.\n");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].contains("/beads:"), "{hits:?}");

        for spelling in [
            "Call mcp__beads_beads__list to list issues.",
            "Use mcp__plugin_beads_beads__ready first.",
            "The mcp_beads_show tool shows one.",
        ] {
            let hits = scan_doc(spelling);
            assert_eq!(hits.len(), 1, "{spelling}: {hits:?}");
            assert!(hits[0].contains("MCP"), "{hits:?}");
        }

        // Both at once, in one file, is two things to fix.
        assert_eq!(scan_doc("/beads:ready and mcp__beads_beads__list\n").len(), 2);
    }

    /// The whole reason this check can be a blunt substring scan: a doc that
    /// uses beads the way this port actually works — the CLI — says none of
    /// these strings. Doc *drift* (a `bd` subcommand that no longer exists) is
    /// the Integrations family's `agent-docs-drift`, and duplicating it here
    /// would report one problem twice.
    #[test]
    fn a_doc_that_only_uses_the_cli_is_clean() {
        let doc = "\
# Working here

Run `bd ready` to see what is available.

```sh
bd create -t bug 'it broke'
bd close bd-a3f2
```

bd stores everything in .beads/ and never phones home.
";
        assert!(scan_doc(doc).is_empty(), "{:?}", scan_doc(doc));
    }

    // --- helpers ---------------------------------------------------------

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "bd-doctor-runtime-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    /// A file named like `bd` that the check will find. It is never executed —
    /// the check only looks — so its contents do not matter.
    fn write_fake_bd(dir: &Path) -> PathBuf {
        let name = if cfg!(windows) { "bd.exe" } else { "bd" };
        let p = dir.join(name);
        std::fs::write(&p, b"not a real binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}
