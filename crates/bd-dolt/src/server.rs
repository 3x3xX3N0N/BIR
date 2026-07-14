//! Spawning and supervising `dolt sql-server`.
//!
//! This module exists to make one outcome impossible: an orphaned
//! `dolt sql-server`. Dolt locks a database to a single server process, so a
//! stray one does not merely waste memory — it makes the *next* `bd` invocation
//! fail with a lock error that reads like corruption. Everything here is
//! organized around that: adopt a server rather than race it, kill and **reap**
//! the one we started, and record enough on disk that the next process can tell
//! a live server from a dead one.
//!
//! # The four hazards, and what is done about each
//!
//! 1. **Drop must not leak the child.** There is no async drop, so [`Drop`]
//!    does a synchronous `start_kill` followed by a *bounded* blocking reap (see
//!    [`DoltServer::drop`] for why reaping, not just killing, is the part that
//!    matters).
//! 2. **Port selection is a TOCTOU race.** [`free_port`] binds port 0, reads the
//!    port and closes the socket; another process can take it in the gap. This is
//!    not airtight and cannot be made so without dolt accepting a listening fd.
//!    It is *mitigated*: a bind failure is recognized in the server's log and the
//!    start is retried on a fresh port.
//! 3. **Readiness is not spawn.** [`DoltServer::wait_ready`] polls until the
//!    server actually speaks MySQL — it requires a connect *and* a server
//!    greeting, because a bound-but-not-serving listener will accept and then
//!    reset, which is exactly the intermittent first-query failure this is here
//!    to prevent.
//! 4. **A server may already be running.** We adopt it and record that we do not
//!    own it; [`Drop`] then leaves it alone. Ownership is explicit, never guessed.

use bd_storage::error::anyhow_lite;
use bd_storage::{Error, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

/// Server stdout+stderr land here. They are *never* inherited: a single dolt
/// warning printed on our stdout corrupts `bd --json`, and the caller would have
/// no idea why the parse failed.
pub const LOG_FILE: &str = "dolt-server.log";

/// Our record of the server we started, so the next `bd` can find it instead of
/// starting a second one that would fail on dolt's lock.
pub const PID_FILE: &str = "dolt-server.json";

/// Point beads at a server it did not start (docker, a dev's terminal, CI).
///
/// If something is already listening there we adopt it; if nothing is, we start
/// our own server on exactly that port rather than a random one. Either way the
/// port the user named is the port that gets used.
pub const PORT_ENV: &str = "BD_DOLT_PORT";

const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// How long `start` waits for the server to speak MySQL. Dolt is a Go binary
/// opening a database; a cold start on a slow disk is seconds, not milliseconds.
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Fresh port each time. Only a *bind* failure is retried — anything else is a
/// real error and retrying it just delays the report.
const START_ATTEMPTS: u32 = 4;

const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Upper bound on how long `Drop` will block. A `SIGKILL`ed process is reaped in
/// microseconds; this only exists so a pathological kernel cannot hang `bd`.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

const LOG_TAIL_BYTES: u64 = 8 * 1024;

/// A `dolt sql-server` that is serving the database in some directory.
///
/// Possibly one *we* started ([`is_owned`](DoltServer::is_owned) true), possibly
/// one we adopted. The difference is the whole point of the type: dropping the
/// first must kill it, and dropping the second must not — we did not start it,
/// it is not ours to stop.
#[derive(Debug)]
pub struct DoltServer {
    /// Kept public for compatibility with the original stub; prefer
    /// [`DoltServer::port`].
    pub port: u16,
    /// The database directory (the `.beads` dir).
    dir: PathBuf,
    /// `None` when adopted. Behind a `Mutex` because `Drop` and `stop` need
    /// `&mut` access to the child while `wait_ready` only has `&self` — and the
    /// child is how `wait_ready` learns that dolt died instead of getting slow.
    /// Never held across an `.await`.
    child: Mutex<Option<Child>>,
    /// The child's pid, kept after the child is reaped so we can tell whether the
    /// pid file on disk is still *ours* before deleting it. `None` when adopted.
    pid: Option<u32>,
    /// True only when this process spawned the server. Guessing this is how you
    /// end up killing the user's own long-running server.
    owned: bool,
}

impl DoltServer {
    /// Start (or adopt) a `dolt sql-server` for the database in `dir`.
    ///
    /// Returns once the server answers on loopback; the caller does not need to
    /// call [`wait_ready`](DoltServer::wait_ready) first, though doing so is
    /// cheap and idempotent.
    pub async fn start(dir: &Path) -> Result<DoltServer> {
        Self::start_with_timeout(dir, DEFAULT_READY_TIMEOUT).await
    }

    /// [`start`](DoltServer::start) with an explicit readiness budget. The budget
    /// covers *all* start attempts, not each one.
    pub async fn start_with_timeout(dir: &Path, timeout: Duration) -> Result<DoltServer> {
        let dir = abs_dir(dir)?;
        let requested = env_port();

        // Adopt *before* looking for a `dolt` binary. A server run by something
        // else — docker, a dev's terminal — is a perfectly good server, and in
        // that setup there may be no local `dolt` at all. Starting a second one
        // against the same database could only ever fail on dolt's lock.
        if let Some(existing) = Self::try_adopt(&dir, requested).await {
            return Ok(existing);
        }

        let dolt = crate::which_dolt().ok_or_else(|| {
            other(
                "no `dolt` binary on PATH; install dolt \
                 (https://github.com/dolthub/dolt) or use the sqlite backend",
            )
        })?;

        // Not fatal here: reads work fine without an identity. It is fatal at
        // `CALL DOLT_COMMIT()`, which is why `ensure_identity` is public and the
        // commit path is expected to call it and *propagate* the error.
        if let Err(e) = ensure_identity(&dir).await {
            tracing::warn!("dolt has no commit identity: {e}");
        }

        let log = dir.join(LOG_FILE);
        // One log per start, so a failure's log is the failure's log. Retries
        // inside this call append, which is what makes the retry diagnosable.
        let _ = std::fs::File::create(&log);

        let deadline = Instant::now() + timeout;
        let attempts = if requested.is_some() {
            1 // The user named a port. Silently using a different one is a lie.
        } else {
            START_ATTEMPTS
        };

        let mut last = None;
        for attempt in 1..=attempts {
            let port = match requested {
                Some(p) => p,
                None => free_port()?,
            };
            let child = spawn(&dolt, &dir, port, &log)?;
            let pid = child.id();
            let mut server = DoltServer {
                port,
                dir: dir.clone(),
                child: Mutex::new(Some(child)),
                pid,
                owned: true,
            };

            match server.wait_ready_until(deadline).await {
                Ok(()) => {
                    server.write_pidfile()?;
                    tracing::debug!(port, pid = ?server.pid, "dolt sql-server is ready");
                    return Ok(server);
                }
                Err(e) => {
                    // Kill it before we look at anything else: a half-started dolt
                    // still holds the lock.
                    server.stop().await?;
                    let tail = log_tail(&log, LOG_TAIL_BYTES);

                    if is_address_in_use(&tail) && attempt < attempts {
                        // The TOCTOU window in `free_port` closed on us. This is
                        // the mitigation, and it is a mitigation, not a fix.
                        tracing::debug!(port, "port was taken between bind and spawn; retrying");
                        last = Some(e);
                        continue;
                    }

                    if is_database_locked(&tail) {
                        // Someone won the race between our adopt check and our
                        // spawn — most likely a concurrent `bd`. Adopt theirs.
                        if let Some(existing) = Self::try_adopt(&dir, requested).await {
                            return Ok(existing);
                        }
                        return Err(other(format!(
                            "the dolt database in {} is locked by another `dolt sql-server` \
                             whose port is unknown. Stop that server, or point beads at it \
                             with {PORT_ENV}=<port>.\n{}",
                            dir.display(),
                            last_lines(&tail, 10)
                        )));
                    }

                    return Err(other(format!("{e}\n{}", last_lines(&tail, 10))));
                }
            }
        }

        Err(last.unwrap_or_else(|| {
            other(format!(
                "could not find a free port for `dolt sql-server` in {attempts} attempts"
            ))
        }))
    }

    /// The loopback port the server is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The database directory this server serves.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// False when this server was already running and we adopted it. Callers that
    /// want to shut the world down should honor this: it is not ours to stop.
    pub fn is_owned(&self) -> bool {
        self.owned
    }

    /// Where the server's stdout and stderr go.
    pub fn log_path(&self) -> PathBuf {
        self.dir.join(LOG_FILE)
    }

    /// A MySQL DSN for `database` on this server.
    ///
    /// `dolt sql-server` is started with `--user root` and no password, which is
    /// safe only because it is bound to loopback and locked to one process. Do
    /// not make it listen anywhere else.
    pub fn dsn(&self, database: &str) -> String {
        format!("mysql://root@127.0.0.1:{}/{database}", self.port)
    }

    /// Block until the server accepts connections *and* speaks MySQL, or `timeout`
    /// elapses. Cheap and idempotent once ready.
    ///
    /// Fails immediately — rather than waiting out the timeout — if the child has
    /// already exited. "dolt died at once" and "dolt is slow" are different
    /// answers and must not take the same 30 seconds to give.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        self.wait_ready_until(Instant::now() + timeout).await
    }

    async fn wait_ready_until(&self, deadline: Instant) -> Result<()> {
        let mut attempt = 0u32;
        loop {
            if let Some(status) = self.child_exited()? {
                return Err(other(format!(
                    "`dolt sql-server` exited before it was ready ({status}); see {}",
                    self.log_path().display()
                )));
            }
            if probe(self.port, PROBE_TIMEOUT).await {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(other(format!(
                    "`dolt sql-server` did not answer on 127.0.0.1:{} in time; see {}",
                    self.port,
                    self.log_path().display()
                )));
            }
            let wait = backoff(attempt).min(deadline - now);
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// Stop the server. Idempotent, and a no-op for an adopted one.
    pub async fn stop(&mut self) -> Result<()> {
        let child = self.take_child();
        if let Some(mut child) = child {
            if self.owned {
                // `kill()` is SIGKILL / TerminateProcess and it also *waits*, so
                // the pid is reaped before we return. See `drop` for why the
                // waiting half is the important half.
                let _ = child.kill().await;
            }
            self.remove_pidfile_if_ours();
        }
        Ok(())
    }

    // --- internals ---

    /// A server that is already running, that we must not kill.
    fn adopted(dir: &Path, port: u16) -> DoltServer {
        DoltServer {
            port,
            dir: dir.to_path_buf(),
            child: Mutex::new(None),
            pid: None,
            owned: false,
        }
    }

    /// A running server for this directory, if there is one.
    ///
    /// Liveness is decided by *probing the port*, never by asking whether a pid
    /// exists. A pid check needs libc, and it answers the wrong question anyway:
    /// what matters is whether something is serving this database, not whether
    /// some process with that number is alive.
    async fn try_adopt(dir: &Path, requested: Option<u16>) -> Option<DoltServer> {
        if let Some(port) = requested {
            return probe(port, PROBE_TIMEOUT)
                .await
                .then(|| DoltServer::adopted(dir, port));
        }

        let rec = read_pidfile(dir)?;
        if probe(rec.port, PROBE_TIMEOUT).await {
            tracing::debug!(port = rec.port, pid = rec.pid, "adopting a running dolt sql-server");
            return Some(DoltServer::adopted(dir, rec.port));
        }

        // Stale. Dolt's own `.dolt/sql-server.lock` is dolt's to clean up — it
        // checks the recorded pid itself on start — so all we owe is deleting our
        // record before it misleads the next process.
        tracing::debug!(port = rec.port, pid = rec.pid, "removing a stale {PID_FILE}");
        let _ = std::fs::remove_file(pidfile_path(dir));
        None
    }

    fn take_child(&mut self) -> Option<Child> {
        self.child
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// `Ok(Some(status))` once the child has exited. Always `Ok(None)` for an
    /// adopted server: we have no handle on it, and its liveness shows up as a
    /// failed probe.
    fn child_exited(&self) -> Result<Option<std::process::ExitStatus>> {
        let mut guard = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.as_mut() {
            Some(child) => child
                .try_wait()
                .map_err(|e| other(format!("cannot wait on `dolt sql-server`: {e}"))),
            None => Ok(None),
        }
    }

    fn write_pidfile(&self) -> Result<()> {
        let Some(pid) = self.pid else { return Ok(()) };
        let rec = PidFile {
            pid,
            port: self.port,
        };
        let body = serde_json::to_string(&rec)
            .map_err(|e| other(format!("cannot serialize {PID_FILE}: {e}")))?;
        std::fs::write(pidfile_path(&self.dir), body)
            .map_err(|e| other(format!("cannot write {PID_FILE}: {e}")))
    }

    /// Delete the pid file only if it still describes *our* child.
    ///
    /// Without the check, a start that lost the lock race would cheerfully delete
    /// the record written by the process that won it, and the next `bd` would
    /// start a second server against a locked database.
    fn remove_pidfile_if_ours(&self) {
        let Some(pid) = self.pid else { return };
        match read_pidfile(&self.dir) {
            Some(rec) if rec.pid == pid && rec.port == self.port => {
                let _ = std::fs::remove_file(pidfile_path(&self.dir));
            }
            _ => {}
        }
    }
}

/// Kill the server we started — and *reap* it.
///
/// The kill is the obvious half. The reap is the half that bites: on unix a
/// killed-but-unreaped child is a zombie, and a zombie's pid still looks alive to
/// anything that asks the kernel. Dolt's stale-lock check asks exactly that, so a
/// zombie holds the database lock just as effectively as a live server does —
/// within the lifetime of this process, which is precisely when a `bd` command
/// opens a second store. `tokio`'s `kill_on_drop` alone kills without waiting and
/// would leave that zombie behind; so we kill, then block (briefly, boundedly)
/// until the child is actually gone.
///
/// Blocking in `Drop` is not elegant. Async drop does not exist, and the
/// alternative — returning from `bd` while a dolt process still owns the lock —
/// is a bug the user experiences as a corrupt database. The wait is microseconds
/// in practice and capped at [`REAP_TIMEOUT`].
impl Drop for DoltServer {
    fn drop(&mut self) {
        if !self.owned {
            return; // Adopted. Not ours to stop.
        }
        let Some(mut child) = self.take_child() else {
            return; // Already stopped.
        };

        // Synchronous, no runtime required: this must work while a runtime is
        // being torn down, or during an unwind, which is exactly when it matters.
        let _ = child.start_kill();

        let deadline = Instant::now() + REAP_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Err(_) => break, // Nothing more we can do from a destructor.
                Ok(None) => {}
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    port = self.port,
                    pid = ?self.pid,
                    "`dolt sql-server` did not die; it may still hold the database lock"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        self.remove_pidfile_if_ours();
    }
}

/// Make sure dolt has a commit identity, borrowing git's if it has none.
///
/// Dolt refuses to commit without `user.name`/`user.email`, and the error it
/// gives is about a "config" rather than about the commit, which sends people
/// looking in the wrong place. Upstream beads solves this by falling back to
/// git's identity; so do we.
///
/// Scope: `--local` once the directory is a dolt repo (nothing leaks onto the
/// user's machine), `--global` before that — `--local` has nowhere to write until
/// `.dolt/` exists, and `dolt init` itself needs the identity.
pub async fn ensure_identity(dir: &Path) -> Result<()> {
    let dolt = crate::which_dolt().ok_or_else(|| other("no `dolt` binary on PATH"))?;
    let scope = if dir.join(".dolt").is_dir() {
        "--local"
    } else {
        "--global"
    };

    for key in ["user.name", "user.email"] {
        if config_get(&dolt, dir, key).await.is_some() {
            continue;
        }
        let Some(value) = git_config_get(dir, key).await else {
            return Err(other(format!(
                "dolt has no {key} and git has none to borrow. Set one:\n  \
                 git config --global {key} \"...\"\n  or\n  \
                 dolt config --global --add {key} \"...\""
            )));
        };
        let out = Command::new(&dolt)
            .args(["config", scope, "--add", key, &value])
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| other(format!("cannot run `dolt config`: {e}")))?;
        if !out.status.success() {
            return Err(other(format!(
                "`dolt config {scope} --add {key}` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        tracing::debug!("borrowed {key} from git for dolt");
    }
    Ok(())
}

async fn config_get(dolt: &Path, dir: &Path, key: &str) -> Option<String> {
    let out = Command::new(dolt)
        .args(["config", "--get", key])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    non_empty(out.status.success().then(|| stdout_of(&out.stdout))?)
}

async fn git_config_get(dir: &Path, key: &str) -> Option<String> {
    // Run inside `dir` so a repo-local git identity wins over the global one,
    // which is what a user who set one there expects.
    let out = Command::new("git")
        .args(["config", "--get", key])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    non_empty(out.status.success().then(|| stdout_of(&out.stdout))?)
}

fn stdout_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn spawn(dolt: &Path, dir: &Path, port: u16, log: &Path) -> Result<Child> {
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| other(format!("cannot open {}: {e}", log.display())))?;
    let err = out
        .try_clone()
        .map_err(|e| other(format!("cannot open {}: {e}", log.display())))?;

    Command::new(dolt)
        .args(sql_server_args(dir, port))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        // Belt and braces. Our `Drop` does the real work (it also reaps), but if
        // the `Child` ever escapes it — a panic between spawn and construction —
        // tokio still kills it.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| other(format!("cannot spawn `{}`: {e}", dolt.display())))
}

/// `--data-dir` rather than a bare cwd: `bd` runs from wherever the user is, and
/// dolt resolves a relative data dir against *its* cwd. Passing it explicitly is
/// the difference between serving the workspace and serving whatever happens to
/// be under the shell.
fn sql_server_args(dir: &Path, port: u16) -> Vec<OsString> {
    vec![
        OsString::from("sql-server"),
        OsString::from("--host"),
        OsString::from("127.0.0.1"),
        OsString::from("--port"),
        OsString::from(port.to_string()),
        OsString::from("--user"),
        OsString::from("root"),
        OsString::from("--data-dir"),
        dir.as_os_str().to_owned(),
    ]
}

/// Ask the OS for a free loopback port.
///
/// This is the classic bind-0-then-close trick and it is a TOCTOU race: the port
/// is free when we close the socket and can be taken before dolt binds it. There
/// is no way to close the window from here — dolt does not accept an inherited
/// listening socket — so the caller retries on a bind failure instead. Small
/// window, real window; do not describe this as safe.
fn free_port() -> Result<u16> {
    let listener = TcpListener::bind((LOOPBACK, 0))
        .map_err(|e| other(format!("cannot bind a loopback port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| other(format!("cannot read the bound port: {e}")))?
        .port();
    drop(listener);
    Ok(port)
}

fn env_port() -> Option<u16> {
    parse_port(&std::env::var(PORT_ENV).ok()?)
}

fn parse_port(s: &str) -> Option<u16> {
    match s.trim().parse::<u16>() {
        Ok(0) | Err(_) => None,
        Ok(p) => Some(p),
    }
}

/// Is a MySQL server answering on this loopback port?
///
/// A TCP connect alone is not enough. Dolt binds its listener slightly before it
/// can serve, so a connect can succeed against a server that will immediately
/// reset the connection — which the *store* then sees as an intermittent failure
/// on its very first query. Requiring the server's greeting packet costs one
/// round trip and removes that whole class of flake.
fn probe_blocking(addr: SocketAddr, timeout: Duration) -> bool {
    let Ok(mut sock) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    if sock.set_read_timeout(Some(timeout)).is_err() {
        return false;
    }
    // A MySQL server speaks first. Anything at all means it is serving; `Ok(0)`
    // (an immediate EOF) means it accepted and hung up, i.e. not ready.
    let mut buf = [0u8; 16];
    matches!(sock.read(&mut buf), Ok(n) if n > 0)
}

async fn probe(port: u16, timeout: Duration) -> bool {
    let addr = SocketAddr::from((LOOPBACK, port));
    tokio::task::spawn_blocking(move || probe_blocking(addr, timeout))
        .await
        .unwrap_or(false)
}

/// Doubling backoff, capped. Fast enough that a warm server is ready in one or
/// two polls; slow enough that a 30-second cold start is not a spin loop.
fn backoff(attempt: u32) -> Duration {
    const BASE_MS: u64 = 25;
    const CAP_MS: u64 = 250;
    let ms = BASE_MS.saturating_mul(1u64 << attempt.min(8));
    Duration::from_millis(ms.min(CAP_MS))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PidFile {
    pid: u32,
    port: u16,
}

fn pidfile_path(dir: &Path) -> PathBuf {
    dir.join(PID_FILE)
}

fn read_pidfile(dir: &Path) -> Option<PidFile> {
    let body = std::fs::read_to_string(pidfile_path(dir)).ok()?;
    // A truncated or hand-edited file is a stale file, not an error: we are about
    // to probe the port anyway, and failing `bd` over unparseable scratch state
    // would be absurd.
    serde_json::from_str(&body).ok()
}

fn abs_dir(dir: &Path) -> Result<PathBuf> {
    if !dir.is_dir() {
        return Err(other(format!(
            "{} is not a directory; run `bd init` first",
            dir.display()
        )));
    }
    // `absolute`, not `canonicalize`: on Windows the latter returns a `\\?\`
    // verbatim path, and handing one of those to a Go program is a coin flip.
    std::path::absolute(dir).map_err(|e| other(format!("cannot resolve {}: {e}", dir.display())))
}

fn log_tail(path: &Path, max: u64) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max && file.seek(SeekFrom::Start(len - max)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Did dolt fail because the port went away under us?
///
/// Matched on the *log*, because the exit status is a bare 1 for every kind of
/// failure. Both the unix and the Windows phrasings are here; missing one turns a
/// retryable race into a hard failure on that platform only, which is the kind of
/// bug that survives for years.
fn is_address_in_use(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    ["address already in use", "address in use"]
        .iter()
        .any(|needle| log.contains(needle))
        // Windows: "Only one usage of each socket address (protocol/network
        // address/port) is normally permitted."
        || log.contains("only one usage of each socket address")
}

/// Did dolt fail because another server already holds this database?
fn is_database_locked(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    [
        "sql-server.lock",
        "locked by another",
        "database locked",
        "database is locked",
        "another sql-server",
    ]
    .iter()
    .any(|needle| log.contains(needle))
}

fn other(msg: impl Into<String>) -> Error {
    Error::Other(anyhow_lite::Error(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A loopback server that is not dolt.
    ///
    /// Everything about adoption, readiness and ownership can be tested against
    /// this — none of it needs a real dolt, and pretending otherwise would leave
    /// the most important logic in the file covered only on machines that happen
    /// to have dolt installed.
    struct FakeServer {
        port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeServer {
        /// `greet: true` behaves like a MySQL server (speaks first). `false`
        /// behaves like a listener that is bound but not yet serving — the exact
        /// state a connect-only readiness check reports as "ready" and is wrong.
        fn start(greet: bool) -> FakeServer {
            let listener = TcpListener::bind((LOOPBACK, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();

            let thread = std::thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut sock, _)) => {
                            if greet {
                                let _ = sock.write_all(b"\x0a5.7.9-fake-dolt\0");
                                let _ = sock.flush();
                            } else {
                                // Hold the connection open, silent, past the
                                // prober's read timeout.
                                std::thread::sleep(Duration::from_millis(400));
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });

            FakeServer {
                port,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bd-dolt-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn free_port_hands_back_a_port_that_is_actually_free() {
        let port = free_port().unwrap();
        assert_ne!(port, 0);
        // The whole trick is that the listener is closed again, so this must bind.
        // If it cannot, the TOCTOU window is not a window, it is a wall.
        TcpListener::bind((LOOPBACK, port)).expect("port should be free right after free_port");
    }

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        assert_eq!(backoff(0), Duration::from_millis(25));
        assert!(backoff(1) > backoff(0));
        assert!(backoff(5) >= backoff(4));
        for attempt in 0..64 {
            assert!(backoff(attempt) <= Duration::from_millis(250), "no runaway");
        }
    }

    #[test]
    fn a_port_is_only_a_port_if_it_is_a_port() {
        assert_eq!(parse_port("3306"), Some(3306));
        assert_eq!(parse_port(" 3306 "), Some(3306));
        assert_eq!(parse_port("0"), None); // "let the OS pick" is not an override
        assert_eq!(parse_port("99999"), None);
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("mysql"), None);
    }

    #[test]
    fn the_pid_file_round_trips_and_garbage_is_just_stale() {
        let dir = tempdir("pidfile");
        assert_eq!(read_pidfile(&dir), None, "no file is not an error");

        let server = DoltServer {
            port: 4242,
            dir: dir.clone(),
            child: Mutex::new(None),
            pid: Some(777),
            owned: true,
        };
        server.write_pidfile().unwrap();
        assert_eq!(
            read_pidfile(&dir),
            Some(PidFile {
                pid: 777,
                port: 4242
            })
        );

        std::fs::write(pidfile_path(&dir), "{ truncated").unwrap();
        assert_eq!(read_pidfile(&dir), None, "a torn file is stale, not fatal");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_pid_file_belonging_to_someone_else_is_left_alone() {
        // The lock-race path: our spawn lost, theirs won and wrote the record.
        // Deleting it would send the next `bd` off to start a second server
        // against a database that is already locked.
        let dir = tempdir("notours");
        std::fs::write(pidfile_path(&dir), r#"{"pid":111,"port":5000}"#).unwrap();

        let ours = DoltServer {
            port: 6000,
            dir: dir.clone(),
            child: Mutex::new(None),
            pid: Some(222),
            owned: true,
        };
        ours.remove_pidfile_if_ours();
        assert!(pidfile_path(&dir).exists(), "someone else's record survives");

        drop(ours);
        assert!(pidfile_path(&dir).exists(), "and Drop does not take it either");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sql_server_args_pin_the_port_the_host_and_the_data_dir() {
        let args = sql_server_args(Path::new("/w/.beads"), 3307);
        let flat: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(flat[0], "sql-server");
        let at = |flag: &str| flat.iter().position(|a| a == flag).map(|i| flat[i + 1].clone());
        assert_eq!(at("--port").as_deref(), Some("3307"));
        // Loopback only. A dolt sql-server with no password on 0.0.0.0 is an open
        // database on the network.
        assert_eq!(at("--host").as_deref(), Some("127.0.0.1"));
        assert_eq!(at("--data-dir").as_deref(), Some("/w/.beads"));
    }

    #[test]
    fn a_bind_failure_is_recognized_on_both_platforms() {
        assert!(is_address_in_use(
            "error starting server: listen tcp 127.0.0.1:3306: bind: address already in use"
        ));
        assert!(is_address_in_use(
            "listen tcp 127.0.0.1:3306: bind: Only one usage of each socket address \
             (protocol/network address/port) is normally permitted."
        ));
        assert!(!is_address_in_use("Server ready. Accepting connections."));
        // The two failures must not be confused: one is retryable, one is not.
        assert!(!is_address_in_use("database locked by another sql-server"));
    }

    #[test]
    fn a_lock_failure_is_recognized() {
        assert!(is_database_locked(
            "database locked by another sql-server; either clone the database to run a \
             second server, or delete the lock file"
        ));
        assert!(is_database_locked("failed to acquire .dolt/sql-server.lock"));
        assert!(!is_database_locked("Server ready. Accepting connections."));
    }

    #[test]
    fn the_log_tail_survives_a_missing_or_huge_log() {
        let dir = tempdir("tail");
        assert_eq!(log_tail(&dir.join("nope.log"), 1024), "");

        let log = dir.join(LOG_FILE);
        let big: String = (0..500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&log, &big).unwrap();
        let tail = log_tail(&log, 64);
        assert!(tail.len() <= 64);
        assert!(tail.ends_with("line 499\n"));
        assert_eq!(last_lines(&tail, 1), "line 499");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn readiness_means_the_server_spoke_not_merely_that_it_accepted() {
        let mysql = FakeServer::start(true);
        assert!(probe(mysql.port, Duration::from_millis(300)).await);

        // Bound, accepting, silent: exactly the window in which dolt will accept a
        // connection and then reset it. A connect-only check calls this ready and
        // hands the store a doomed first query.
        let silent = FakeServer::start(false);
        assert!(!probe(silent.port, Duration::from_millis(100)).await);

        // Nothing there at all.
        let dead = free_port().unwrap();
        assert!(!probe(dead, Duration::from_millis(100)).await);
    }

    #[tokio::test]
    async fn a_running_server_is_adopted_and_never_killed() {
        // No `dolt` needed, and that is deliberate: the adopt path must work on a
        // machine whose dolt runs in a container.
        let dir = tempdir("adopt");
        let running = FakeServer::start(true);
        std::fs::write(
            pidfile_path(&dir),
            serde_json::to_string(&PidFile {
                pid: 4321,
                port: running.port,
            })
            .unwrap(),
        )
        .unwrap();

        let server = DoltServer::try_adopt(&dir, None)
            .await
            .expect("a live port in the pid file is a live server");
        assert_eq!(server.port(), running.port);
        assert!(!server.is_owned(), "we did not start it");

        drop(server);
        assert!(
            probe(running.port, Duration::from_millis(300)).await,
            "dropping an adopted server must leave it running"
        );
        // And it must not have deleted the record of a server it does not own.
        assert!(pidfile_path(&dir).exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_stale_pid_file_is_not_adopted_but_removed() {
        let dir = tempdir("stale");
        let dead = free_port().unwrap();
        std::fs::write(
            pidfile_path(&dir),
            serde_json::to_string(&PidFile {
                pid: 999_999,
                port: dead,
            })
            .unwrap(),
        )
        .unwrap();

        assert!(DoltServer::try_adopt(&dir, None).await.is_none());
        assert!(
            !pidfile_path(&dir).exists(),
            "a record that points at nothing must not outlive the check"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_explicit_port_is_adopted_when_something_answers_there() {
        let dir = tempdir("envport");
        let running = FakeServer::start(true);

        let server = DoltServer::try_adopt(&dir, Some(running.port))
            .await
            .expect("something is answering on the requested port");
        assert_eq!(server.port(), running.port);
        assert!(!server.is_owned());

        // Nothing answering on the requested port: not adopted, and `start` will
        // go on to spawn there rather than somewhere else.
        let free = free_port().unwrap();
        assert!(DoltServer::try_adopt(&dir, Some(free)).await.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn wait_ready_gives_up_rather_than_hanging_on_a_dead_port() {
        let dir = tempdir("timeout");
        let server = DoltServer::adopted(&dir, free_port().unwrap());

        let start = Instant::now();
        let err = server
            .wait_ready(Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not answer"));
        assert!(start.elapsed() < Duration::from_secs(5), "bounded");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let dir = tempdir("stop");
        let mut server = DoltServer::adopted(&dir, 1);
        server.stop().await.unwrap();
        server.stop().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn start_refuses_a_directory_that_is_not_there() {
        let missing = std::env::temp_dir().join(format!("bd-dolt-absent-{}", uuid::Uuid::new_v4()));
        let err = DoltServer::start_with_timeout(&missing, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a directory"));
    }
}
