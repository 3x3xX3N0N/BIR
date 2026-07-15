//! Tests over the command tree itself.
//!
//! The command surface is large and mostly declarative, which is exactly the
//! kind of code that rots without anyone noticing. These tests are cheap and
//! they fail loudly:
//!
//! * `debug_assert` catches a malformed tree (duplicate flags, an alias that
//!   collides, a positional after a variadic) that would otherwise only panic
//!   at runtime, on the one command nobody tried.
//! * The family test catches a command added to the enum but not to the help
//!   map — which would make `bd --help` silently incomplete, the one property
//!   this port promises.

use std::collections::HashSet;
use std::process::Command;

/// Every command bd knows, minus clap's generated `help`.
fn registered() -> Vec<String> {
    bd_cli_command()
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .filter(|n| n != "help")
        .collect()
}

fn bd_cli_command() -> clap::Command {
    // The binary's own builder, so the test sees exactly what users see.
    bd_cli::cli::build()
}

#[test]
fn command_tree_is_well_formed() {
    bd_cli_command().debug_assert();
}

#[test]
fn every_command_appears_in_exactly_one_family() {
    let registered: HashSet<String> = registered().into_iter().collect();
    let mut mapped: HashSet<String> = HashSet::new();

    for (family, names) in bd_cli::cli::FAMILIES {
        for n in *names {
            assert!(
                mapped.insert(n.to_string()),
                "{n} is listed in more than one family (last: {family})"
            );
        }
    }

    let missing: Vec<_> = registered.difference(&mapped).collect();
    assert!(
        missing.is_empty(),
        "commands registered but absent from FAMILIES (they would vanish from `bd --help`): {missing:?}"
    );
    let phantom: Vec<_> = mapped.difference(&registered).collect();
    assert!(
        phantom.is_empty(),
        "FAMILIES names commands that do not exist: {phantom:?}"
    );
}

#[test]
fn root_help_shows_the_whole_map() {
    let help = bd_cli_command().render_help().to_string();
    for family in ["Issues:", "Views:", "Deps:", "Sync:", "Setup:", "Maintenance:", "Advanced:"] {
        assert!(help.contains(family), "`bd --help` is missing the {family} group");
    }
    // Spot-check one command per family: the map is generated, so if these are
    // present the rest are too.
    for cmd in ["create", "ready", "dep", "export", "init", "doctor", "mol"] {
        assert!(help.contains(cmd), "`bd --help` is missing {cmd}");
    }
}

#[test]
fn aliases_resolve() {
    let cases = [
        (vec!["bd", "new", "a title"], "create"),
        (vec!["bd", "view", "bd-1"], "show"),
        (vec!["bd", "done", "bd-1"], "close"),
        (vec!["bd", "stats"], "status"),
        (vec!["bd", "hb", "bd-1"], "heartbeat"),
    ];
    for (argv, expected) in cases {
        let m = bd_cli_command()
            .try_get_matches_from(&argv)
            .unwrap_or_else(|e| panic!("{argv:?} did not parse: {e}"));
        assert_eq!(m.subcommand_name(), Some(expected), "{argv:?}");
    }
}

/// The flags the task's contract names explicitly. A rename here breaks scripts.
#[test]
fn core_flags_are_where_scripts_expect_them() {
    let cmd = bd_cli_command();
    let create = cmd
        .get_subcommands()
        .find(|s| s.get_name() == "create")
        .expect("create");
    let shorts: HashSet<char> = create.get_arguments().filter_map(|a| a.get_short()).collect();
    for c in ['d', 'p', 't', 'a', 'l'] {
        assert!(shorts.contains(&c), "bd create lost -{c}");
    }

    let ready = cmd
        .get_subcommands()
        .find(|s| s.get_name() == "ready")
        .expect("ready");
    // --json is global: it lives on the root and is propagated at build time, so
    // it is not in `ready`'s own arguments. `global_flags_reach_subcommands`
    // covers it by parsing instead.
    let longs: HashSet<&str> = ready.get_arguments().filter_map(|a| a.get_long()).collect();
    for l in ["limit", "priority", "assignee", "type", "label", "sort"] {
        assert!(longs.contains(l), "bd ready lost --{l}");
    }
}

/// Duration arguments are parsed by clap, not by the handler.
///
/// The difference is where the failure lands. A bad duration that reaches the
/// handler opens the workspace, opens the database, and *then* exits 1 with a
/// bare message — indistinguishable from beads genuinely failing. Parsed by clap
/// it is a usage error, printed with the flag's own help, before anything is
/// touched.
#[test]
fn duration_flags_are_a_usage_error_when_they_are_typos() {
    for argv in [
        vec!["bd", "stale", "--older-than", "a fortnight"],
        vec!["bd", "purge", "--older-than", "a fortnight"],
        vec!["bd", "update", "x-1", "--claim", "--lease", "a while"],
    ] {
        let err = bd_cli_command()
            .try_get_matches_from(&argv)
            .expect_err(&format!("{argv:?} must not parse"));
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ValueValidation,
            "{argv:?} must fail in clap, not at runtime: {err}"
        );
    }

    // And the good ones still parse, or this test would pass on a broken parser.
    for argv in [
        vec!["bd", "stale", "--older-than", "30d"],
        vec!["bd", "purge", "--older-than", "1w"],
    ] {
        bd_cli_command()
            .try_get_matches_from(&argv)
            .unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"));
    }
}

/// Commands whose handlers exist and were simply unreachable from the command
/// tree. Each of these used to be a parse error.
#[test]
fn the_commands_that_could_not_be_typed_can_now_be_typed() {
    for argv in [
        vec!["bd", "setup", "claude"],
        vec!["bd", "ship", "parser"],
        vec!["bd", "ship", "parser", "--force", "--dry-run"],
        vec!["bd", "gc", "--dry-run"],
        vec!["bd", "prune", "--dry-run"],
        vec!["bd", "purge", "--dry-run", "--yes"],
        vec!["bd", "dep", "remove", "x-1", "x-2", "--type", "related"],
    ] {
        bd_cli_command()
            .try_get_matches_from(&argv)
            .unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
    }

    // `bd link` is for edges that do not gate work, and clap is where that is
    // enforced — a flag the handler would reject anyway should never have
    // parsed, tab-completed, or appeared in --help as usable.
    let err = bd_cli_command()
        .try_get_matches_from(["bd", "link", "x-1", "x-2", "--type", "blocks"])
        .expect_err("`bd link --type blocks` must not parse");
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);

    // Variadic text arguments are required, all of them: `bd audit record` with
    // nothing to record is a usage error, not an empty audit entry.
    assert!(
        bd_cli_command()
            .try_get_matches_from(["bd", "audit", "record"])
            .is_err(),
        "`bd audit record` with no text must be a usage error"
    );
}

#[test]
fn global_flags_reach_subcommands() {
    let m = bd_cli_command()
        .try_get_matches_from(["bd", "ready", "--json", "--actor", "agent-7"])
        .expect("globals should be accepted after a subcommand");
    assert_eq!(m.get_one::<String>("actor").map(String::as_str), Some("agent-7"));
}

// ---------------------------------------------------------------------------
// End-to-end: the exit-code scheme is the port's contract with scripts.
// ---------------------------------------------------------------------------

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

#[test]
fn unported_commands_exit_64_not_1() {
    let tmp = tempdir("stub");
    // A workspace has to exist, or we would be testing the no-workspace path.
    fake_workspace(&tmp);

    // `compact`, not `gc` or `migrate` -- both were stubs when earlier versions
    // of this test were written, and both got implemented (this is the third
    // command this test has been pinned to). `compact` at least names its
    // missing piece (`Storage::compact`); when that lands, move this again.
    //
    // Note the failure this test is guarding against is subtle: `fake_workspace`
    // writes a locator with no database behind it, so a command that opens the
    // store dies with exit 1. That is exactly what a stub must NOT do -- the
    // whole point is that "not built yet" stays distinguishable from "broke".
    let out = bd()
        .args(["-C", tmp.to_str().unwrap(), "compact"])
        .output()
        .expect("run bd");
    assert_eq!(
        out.status.code(),
        Some(64),
        "a stub must be distinguishable from a real failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = bd()
        .args(["-C", tmp.to_str().unwrap(), "--json", "compact"])
        .output()
        .expect("run bd");
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--json stub must emit JSON");
    assert_eq!(doc["error"], "not_implemented");
    assert_eq!(doc["command"], "compact");

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn a_missing_capability_exits_2_and_says_which_backend() {
    let tmp = tempdir("cap");
    fake_workspace(&tmp);

    let out = bd()
        .args(["-C", tmp.to_str().unwrap(), "--json", "branch"])
        .output()
        .expect("run bd");
    assert_eq!(out.status.code(), Some(2), "a capability gap is not a stub");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(doc["error"], "unsupported_backend");
    assert_eq!(doc["backend"], "sqlite");
    assert_eq!(doc["requires"], "dolt");

    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn commands_outside_a_workspace_say_so() {
    let tmp = tempdir("nows");
    let out = bd()
        .args(["-C", tmp.to_str().unwrap(), "list"])
        .output()
        .expect("run bd");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no beads workspace found"), "got: {err}");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn version_and_completion_need_no_workspace() {
    let tmp = tempdir("nodb");
    for args in [vec!["version"], vec!["completion", "bash"]] {
        let mut c = bd();
        c.args(["-C", tmp.to_str().unwrap()]);
        c.args(&args);
        let out = c.output().expect("run bd");
        assert_eq!(
            out.status.code(),
            Some(0),
            "bd {args:?} must work outside a workspace: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.stdout.is_empty());
    }
    std::fs::remove_dir_all(&tmp).ok();
}

/// The whole point of the port, exercised through the real binary and a real
/// database: create work, gate it behind a dependency, watch `ready` and
/// `blocked` disagree about it, close the blocker, watch it become claimable.
///
/// If this passes, the seam is wired up correctly end to end.
#[test]
fn a_real_workflow_from_init_to_ready() {
    let tmp = tempdir("flow");
    let dir = tmp.to_str().unwrap();
    let run = |args: &[&str]| -> (String, i32) {
        let out = bd()
            .args(["-C", dir])
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    };

    assert_eq!(run(&["init", "--prefix", "t"]).1, 0, "init");

    // `bd q` prints the id and nothing else — that is its contract with scripts.
    let (blocker, code) = run(&["q", "Write the schema", "-p", "1"]);
    assert_eq!(code, 0);
    assert!(blocker.starts_with("t-"), "unexpected id: {blocker}");

    let (out, code) = run(&[
        "create",
        "Ship it",
        "-t",
        "feature",
        "-l",
        "release",
        "--deps",
        &blocker,
    ]);
    assert_eq!(code, 0, "{out}");
    let shipit = out
        .rsplit(' ')
        .next()
        .expect("create prints the id")
        .to_string();

    // The gated issue is blocked, and the blocker is ready. This is the whole
    // product in two assertions.
    let (ready, _) = run(&["ready", "--json"]);
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    let ready_ids: Vec<&str> = ready
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(ready_ids.contains(&blocker.as_str()), "blocker should be ready");
    assert!(
        !ready_ids.contains(&shipit.as_str()),
        "a blocked issue must never appear in `bd ready`"
    );

    let (blocked, _) = run(&["blocked", "--json"]);
    let blocked: serde_json::Value = serde_json::from_str(&blocked).unwrap();
    assert_eq!(blocked[0]["id"].as_str(), Some(shipit.as_str()));

    assert_eq!(run(&["close", &blocker, "--reason", "done"]).1, 0);

    let (ready, _) = run(&["ready", "--json"]);
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    let ready_ids: Vec<&str> = ready
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(
        ready_ids.contains(&shipit.as_str()),
        "closing the blocker must free the work it was gating"
    );

    // Export is a backup: it has to carry the relations, not just the columns.
    let (jsonl, code) = run(&["export"]);
    assert_eq!(code, 0);
    let record: serde_json::Value = jsonl
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|r| r["id"] == shipit.as_str())
        .expect("the issue is in the export");
    assert_eq!(record["_type"], "issue");
    assert_eq!(record["labels"][0], "release", "export dropped the labels");
    assert_eq!(
        record["dependencies"][0]["depends_on_id"],
        blocker.as_str(),
        "export dropped the edges"
    );

    // --readonly is a guard, not a suggestion.
    let out = bd()
        .args(["-C", dir, "--readonly", "close", &shipit])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    std::fs::remove_dir_all(&tmp).ok();
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bd-cli-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// A locator and nothing else: enough for every path that must not open the
/// database (stubs, capability probes, `bd where`).
fn fake_workspace(root: &std::path::Path) {
    let beads = root.join(".beads");
    std::fs::create_dir_all(&beads).unwrap();
    std::fs::write(
        beads.join("workspace.json"),
        r#"{"backend":"sqlite","workspace_id":"test-ws"}"#,
    )
    .unwrap();
}
