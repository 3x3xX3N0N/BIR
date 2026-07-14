//! End-to-end tests for the Runtime family of `bd doctor`.
//!
//! (Named `doctor_runtime`, not `doctor_install`. Cargo names the test binary
//! after this file, and Windows auto-elevates any executable whose filename
//! contains "install", "setup", "update" or "patch" — the binary would prompt
//! for admin rights before a single test ran. `wiring_cli.rs` in this directory
//! exists for the same reason.)
//!
//! The one worth having: a real `bd`, run with a real PATH containing two other
//! `bd` binaries, and the finding that says which one actually wins. The unit
//! tests in `doctor/checks/runtime.rs` cover the shapes; this covers the wiring
//! — that the check is registered, runs with no workspace at all, and puts the
//! evidence somewhere an agent can read it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `bd doctor --json` with a PATH we control.
///
/// The exit code is deliberately not asserted anywhere in this file: nine
/// families are filling in checks, and any one of them may legitimately report
/// an `Error` in a bare temp directory. The Runtime findings are read out of the
/// JSON instead, which is the contract that actually matters.
fn doctor(dir: &Path, path_var: &str) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_bd"))
        .args(["-C", dir.to_str().unwrap(), "doctor", "--json"])
        .env("PATH", path_var)
        .env("BEADS_ACTOR", "agent-7")
        .output()
        .expect("run bd doctor");

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "bd doctor --json did not emit JSON ({e})\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn finding<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .expect("checks is an array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no `{name}` check in {report:#}"))
}

fn detail(f: &serde_json::Value) -> String {
    f["detail"]
        .as_str()
        .unwrap_or_else(|| panic!("`{}` reported nothing to act on: {f:#}", f["name"]))
        .to_string()
}

// ---------------------------------------------------------------------------
// The test this file exists for
// ---------------------------------------------------------------------------

/// Two `bd` binaries on PATH, and neither is the one running.
///
/// This is the bug that costs an afternoon: every fix goes into a binary that
/// nothing on the machine executes, and no output anywhere mentions the other
/// one. So the finding has to (a) exist, (b) count them, (c) print them **in
/// PATH order**, because the first one is the one that wins, and (d) say that
/// the binary you are running is not among them.
#[test]
fn two_bd_binaries_on_path_are_all_reported_in_path_order() {
    let dir = tempdir("two");
    let a = dir.join("a");
    let b = dir.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let first = fake_bd(&a);
    let second = fake_bd(&b);

    let path = join(&[&a, &b]);
    let f = doctor(&dir, &path);
    let f = finding(&f, "bd on PATH");

    assert_eq!(f["status"], "warning", "{f:#}");
    assert!(
        f["message"].as_str().unwrap().contains('2'),
        "the count is the headline: {f:#}"
    );

    // Both binaries, by full path — a finding that named only one of them would
    // be the very bug this check exists to catch.
    let d = detail(f);
    let (pa, pb) = (shown(&first), shown(&second));
    let ia = d
        .find(&pa)
        .unwrap_or_else(|| panic!("the first `bd` is missing from the detail:\n{d}"));
    let ib = d
        .find(&pb)
        .unwrap_or_else(|| panic!("the second `bd` is missing from the detail:\n{d}"));

    assert!(
        ia < ib,
        "listed out of PATH order — PATH order IS resolution order:\n{d}"
    );
    assert!(
        d.contains("first on PATH"),
        "the user must be told which one wins:\n{d}"
    );
    // The running binary is `target/debug/bd`, which is not on this PATH at all,
    // and the user cannot diagnose anything unless they are told that.
    assert!(
        d.contains("running"),
        "the running binary must be placed against the list:\n{d}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The happy case, and it must actually be reachable: PATH contains exactly the
/// binary we are running, and the check goes green. A check that can only ever
/// warn is one people turn off.
#[test]
fn the_bd_on_path_being_the_bd_that_is_running_is_ok() {
    let dir = tempdir("one");
    let me = PathBuf::from(env!("CARGO_BIN_EXE_bd"));
    let bin = me.parent().unwrap();

    let f = doctor(&dir, &bin.display().to_string());
    let f = finding(&f, "bd on PATH");
    assert_eq!(f["status"], "ok", "{f:#}");

    std::fs::remove_dir_all(&dir).ok();
}

/// bd is obviously *running* — but nothing that invokes it by name (a git hook,
/// an agent harness, `bd setup`'s own hooks) will find it. Warn, and name the
/// directory to add.
#[test]
fn no_bd_on_path_at_all_warns_and_says_what_to_add() {
    let dir = tempdir("none");
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    let f = doctor(&dir, &empty.display().to_string());
    let f = finding(&f, "bd on PATH");

    assert_eq!(f["status"], "warning", "{f:#}");
    assert!(
        f["fix"].as_str().is_some_and(|s| s.contains("PATH")),
        "a finding with no way to act on it is a bug report: {f:#}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The whole family, with no workspace — which is its normal case
// ---------------------------------------------------------------------------

/// Doctor's premise: it runs where there is nothing. No `.beads/`, no database,
/// no git repo. Every Runtime check must still produce a real finding — and none
/// of them may report `Error`, because a bare directory is not a broken one and
/// `bd doctor` in a git hook must not fail a commit over it.
#[test]
fn every_runtime_check_reports_without_a_workspace_and_none_of_them_errors() {
    let dir = tempdir("bare");
    let me = PathBuf::from(env!("CARGO_BIN_EXE_bd"));
    let path = me.parent().unwrap().display().to_string();

    let report = doctor(&dir, &path);
    assert!(
        report["path"].is_null(),
        "there is no workspace here; doctor must say so: {report:#}"
    );

    for name in [
        "bd on PATH",
        "bd version skew",
        "database filesystem",
        "legacy bd references",
    ] {
        let f = finding(&report, name);
        assert_eq!(
            f["category"], "runtime",
            "`{name}` is filed under the wrong heading: {f:#}"
        );
        assert_ne!(
            f["status"], "error",
            "`{name}` failed the build over an empty directory: {f:#}"
        );
        assert!(
            f["message"].as_str().is_some_and(|m| !m.is_empty()),
            "`{name}` said nothing: {f:#}"
        );
    }

    // With no workspace there is nothing to be out of step with, and no docs to
    // be stale. Neither may be reported as "could not check" — that would put a
    // permanent yellow line in front of every new user, which is how a
    // diagnostic teaches people to ignore it.
    assert_eq!(finding(&report, "bd version skew")["status"], "ok");
    assert_eq!(finding(&report, "legacy bd references")["status"], "ok");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Legacy references
// ---------------------------------------------------------------------------

/// A CLAUDE.md carried over from Go beads, telling an agent to call MCP tools and
/// slash commands that this port does not implement *at all*.
///
/// This is worse than a wrong command, and that is why it is a separate finding
/// from doc drift: a wrong command produces an error the agent can react to. An
/// MCP tool that was never registered produces nothing. The agent quietly stops
/// using beads and nobody ever finds out why.
#[test]
fn agent_docs_pointing_at_surfaces_this_port_does_not_ship_are_reported() {
    let dir = tempdir("docs");
    let me = PathBuf::from(env!("CARGO_BIN_EXE_bd"));
    let path = me.parent().unwrap().display().to_string();

    std::fs::write(
        dir.join("CLAUDE.md"),
        "# Working here\n\
         \n\
         Start with /beads:quickstart, then call mcp__beads_beads__list.\n\
         \n\
         ```sh\n\
         bd ready --json\n\
         ```\n",
    )
    .unwrap();

    let report = doctor(&dir, &path);
    let f = finding(&report, "legacy bd references");
    assert_eq!(f["status"], "warning", "{f:#}");

    let d = detail(f);
    assert!(d.contains("CLAUDE.md"), "name the file:\n{d}");
    assert!(d.contains("/beads:"), "the dead slash commands:\n{d}");
    assert!(d.contains("MCP"), "the dead MCP tools:\n{d}");
    assert!(
        f["fix"].as_str().is_some_and(|s| !s.is_empty()),
        "a finding with no way to act on it is a bug report: {f:#}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A doc that drives beads the way this port actually works — the CLI — is
/// clean. And a repo with no agent docs at all has no agent-doc problem:
/// upstream warns here, and that is a warning about something the user simply
/// does not use.
#[test]
fn a_cli_only_doc_and_no_doc_at_all_are_both_fine() {
    let me = PathBuf::from(env!("CARGO_BIN_EXE_bd"));
    let path = me.parent().unwrap().display().to_string();

    let bare = tempdir("nodocs");
    let f = doctor(&bare, &path);
    assert_eq!(
        finding(&f, "legacy bd references")["status"],
        "ok",
        "absence is not failure"
    );
    std::fs::remove_dir_all(&bare).ok();

    let with_doc = tempdir("clean");
    std::fs::write(
        with_doc.join("AGENTS.md"),
        "Run `bd ready`, then `bd close <id>`.\n\nbd stores issues in .beads/.\n",
    )
    .unwrap();
    let f = doctor(&with_doc, &path);
    assert_eq!(finding(&f, "legacy bd references")["status"], "ok", "{f:#}");
    std::fs::remove_dir_all(&with_doc).ok();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-doctor-rt-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&p).ok();
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// A file named `bd` that the check will find on PATH. It is never executed —
/// the check looks, it does not run anything — so the contents are irrelevant.
fn fake_bd(dir: &Path) -> PathBuf {
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

fn join(dirs: &[&Path]) -> String {
    std::env::join_paths(dirs)
        .expect("join PATH")
        .to_string_lossy()
        .into_owned()
}

/// How the check prints a path.
///
/// `tempdir` canonicalizes, which on Windows yields the verbatim `\\?\C:\...`
/// form — and the check deliberately strips that before showing it to a human,
/// because nobody has `\\?\` in their PATH and a path they do not recognise is a
/// path they cannot act on. The test has to strip it too, or it would be
/// asserting against a string the user is never shown.
fn shown(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}
