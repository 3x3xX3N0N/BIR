//! `bd doctor`'s Metadata family, end to end through the real binary.
//!
//! What is being pinned here is not "the checks run". It is the *statuses* — and
//! specifically the two the family exists for:
//!
//! * A workspace whose configured prefix is not the prefix on its own ids is an
//!   **error**. Nothing is broken today, which is exactly why it has to be loud:
//!   the cost lands weeks later, in someone else's repository, as a merge
//!   conflict between two ids that were both minted as `bd-a3f2`.
//! * A healthy workspace is **silent**. A check that cries wolf on a fresh
//!   `bd init` is a check people learn to skip, and then it is worth nothing on
//!   the day it is right.
//!
//! These go through the binary rather than calling `Check::run` directly,
//! because half of what is being tested is the wiring: that the finding reaches
//! `--json` under a stable name, with the evidence attached.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

struct Ws(PathBuf);

impl Ws {
    fn new(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-doctorid-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        let ws = Ws(std::fs::canonicalize(&p).unwrap());
        assert_eq!(ws.run(&["init", "--prefix", "acme"]).1, 0, "init");
        ws
    }

    /// (stdout, exit code)
    fn run(&self, args: &[&str]) -> (String, i32) {
        let out = bd()
            .args(["-C", self.0.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", "agent-7")
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn beads(&self) -> PathBuf {
        self.0.join(".beads")
    }

    fn config(&self) -> PathBuf {
        self.beads().join("config.yaml")
    }

    fn write_config(&self, yaml: &str) {
        std::fs::write(self.config(), yaml).unwrap();
    }

    fn create(&self, title: &str) -> String {
        let (out, code) = self.run(&["--json", "create", title]);
        assert_eq!(code, 0, "create failed: {out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        v["id"].as_str().expect("created id").to_string()
    }

    /// Every finding from the Metadata family, by check name.
    ///
    /// Deliberately does *not* assert doctor's own exit code: eight other
    /// families are landing checks into the same registry, and a test that
    /// asserted the whole run was green would break every time one of them found
    /// something real. Only this family's findings are this test's business.
    fn findings(&self) -> Vec<Value> {
        let (out, _) = self.run(&["--json", "doctor"]);
        let v: Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("doctor did not emit JSON ({e}): {out}"));
        v["checks"]
            .as_array()
            .expect("checks[]")
            .iter()
            .filter(|c| c["category"] == "metadata")
            .cloned()
            .collect()
    }

    fn finding(&self, name: &str) -> Value {
        self.findings()
            .into_iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no finding named {name:?}"))
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn status(f: &Value) -> &str {
    f["status"].as_str().unwrap_or("<missing>")
}

fn evidence(f: &Value) -> String {
    format!(
        "{} | {} | {}",
        f["message"].as_str().unwrap_or(""),
        f["detail"].as_str().unwrap_or(""),
        f["fix"].as_str().unwrap_or("")
    )
}

// ---------------------------------------------------------------------------

/// The floor. A workspace `bd init` just made must not produce a single
/// metadata finding — if it does, every user learns on day one that this family
/// is noise, and stops reading it.
#[test]
fn a_fresh_workspace_is_silent() {
    let ws = Ws::new("fresh");
    ws.create("first");

    for f in ws.findings() {
        assert_eq!(
            status(&f),
            "ok",
            "a fresh workspace tripped {}: {}",
            f["name"],
            evidence(&f)
        );
    }
}

/// The failure the family exists for.
///
/// `config.yaml` loses its `prefix:` key — the single most common way this
/// happens is a `.beads/` that was never fully committed — and the store's key
/// goes with it. bd falls back to `bd`, which is what *every* unconfigured beads
/// workspace falls back to. From here on, this project mints ids into the same
/// namespace as every other project on earth, and nothing tells anyone.
#[test]
fn a_prefix_that_no_longer_matches_the_ids_in_the_database_is_an_error() {
    let ws = Ws::new("drift");
    let id = ws.create("a real issue");
    assert!(id.starts_with("acme-"), "expected an acme- id, got {id}");

    // The drift: nothing declares a prefix any more, but the database is full
    // of `acme-` ids.
    ws.write_config("claim:\n  lease: 1h\n");
    assert_eq!(ws.run(&["config", "set", "issue.prefix", ""]).1, 0);

    let f = ws.finding("id prefix");
    assert_eq!(
        status(&f),
        "error",
        "prefix drift must fail the run, not whisper: {}",
        evidence(&f)
    );

    // The finding has to name both halves, or it is a bug report you cannot act
    // on: which prefix bd is about to mint with, and which one the ids carry.
    let e = evidence(&f);
    assert!(
        e.contains("bd-"),
        "the finding never says what bd will mint: {e}"
    );
    assert!(
        e.contains("acme"),
        "the finding never names the real prefix: {e}"
    );
}

/// `--fix` adopts the database's prefix — but *only* because nobody had declared
/// one. There is no decision here to overwrite, only an absence.
#[test]
fn fix_adopts_the_prefix_the_database_already_uses() {
    let ws = Ws::new("adopt");
    ws.create("a real issue");
    ws.write_config("claim:\n  lease: 1h\n");
    assert_eq!(ws.run(&["config", "set", "issue.prefix", ""]).1, 0);
    assert_eq!(status(&ws.finding("id prefix")), "error");

    let (out, _) = ws.run(&["doctor", "--fix"]);
    assert!(out.contains("acme"), "fix did not adopt the prefix: {out}");

    assert_eq!(
        status(&ws.finding("id prefix")),
        "ok",
        "the repair did not actually take"
    );
    let (prefix, _) = ws.run(&["config", "get", "issue.prefix"]);
    assert_eq!(prefix, "acme");
}

/// A deliberate rename must survive `--fix`.
///
/// If `config.yaml` names a prefix, the disagreement with the ids on disk may be
/// somebody halfway through renaming the project. A repair that silently reverted
/// that would be worse than the drift it was fixing — so it declines.
#[test]
fn fix_does_not_revert_a_prefix_someone_actually_chose() {
    let ws = Ws::new("rename");
    ws.create("an old issue");
    ws.write_config("prefix: newname\nclaim:\n  lease: 1h\n");

    assert_eq!(
        status(&ws.finding("id prefix")),
        "error",
        "drift is still reported"
    );

    ws.run(&["doctor", "--fix"]);
    // The declared prefix is untouched: doctor reported, and kept its hands off.
    let cfg = std::fs::read_to_string(ws.config()).unwrap();
    assert!(
        cfg.contains("newname"),
        "--fix reverted a deliberate rename: {cfg}"
    );
    assert_eq!(status(&ws.finding("id prefix")), "error");
}

/// The trap: `bd config set issue.prefix` writes the *store*, and `Ctx::prefix`
/// reads `config.yaml` first. A user runs it, sees the new value echoed back,
/// and keeps minting the old prefix. Nothing else in the program will ever tell
/// them.
#[test]
fn a_config_set_that_never_took_effect_is_an_error() {
    let ws = Ws::new("authority");
    assert_eq!(ws.run(&["config", "set", "issue.prefix", "widgets"]).1, 0);

    let f = ws.finding("prefix config");
    assert_eq!(
        status(&f),
        "error",
        "silently ignored setting: {}",
        evidence(&f)
    );
    let e = evidence(&f);
    assert!(
        e.contains("acme") && e.contains("widgets"),
        "both halves must be named: {e}"
    );

    // Ids keep coming out with config.yaml's prefix — which is the whole point.
    assert!(ws.create("proof").starts_with("acme-"));

    // And the repair is not a guess: config.yaml is authoritative, so it wins.
    ws.run(&["doctor", "--fix"]);
    let (stored, _) = ws.run(&["config", "get", "issue.prefix"]);
    assert_eq!(stored, "acme");
    assert_eq!(status(&ws.finding("prefix config")), "ok");
}

/// Values that parse and then mean nothing. Each of these is accepted by serde,
/// silently discarded at the point of use, and never mentioned again.
#[test]
fn config_values_that_parse_but_are_nonsense() {
    let ws = Ws::new("nonsense");
    ws.write_config(
        "prefix: acme\n\
         claim:\n  lease: 0s\n\
         defaults:\n  priority: 7\n  issue_type: tsak\n",
    );

    let f = ws.finding("config values");
    assert_eq!(status(&f), "error", "{}", evidence(&f));

    let e = evidence(&f);
    // A zero lease hands every claim straight back.
    assert!(e.contains("lease"), "no lease finding: {e}");
    // An out-of-range priority is silently replaced with P2.
    assert!(e.contains("priority"), "no priority finding: {e}");
    // A typo'd type does not fail — it invents a type.
    assert!(e.contains("tsak"), "no issue_type finding: {e}");
}

/// An unparseable lease is not a hard error at startup — `Ctx::lease` swallows
/// it and hands back one hour. That is exactly the afternoon-losing failure the
/// config docs warn about, and doctor is the only thing that can see it.
#[test]
fn an_unparseable_lease_is_reported_rather_than_silently_defaulted() {
    let ws = Ws::new("lease");
    ws.write_config("prefix: acme\nclaim:\n  lease: soon\n");

    let f = ws.finding("config values");
    assert_eq!(status(&f), "error", "{}", evidence(&f));
    assert!(
        evidence(&f).contains("1h"),
        "say what it silently became: {}",
        evidence(&f)
    );
}

/// Absence is not failure. A workspace that simply does not set the optional
/// keys is a normal workspace.
#[test]
fn defaults_alone_are_not_a_finding() {
    let ws = Ws::new("defaults");
    ws.write_config("prefix: acme\n");
    assert_eq!(status(&ws.finding("config values")), "ok");
}

/// The acked version lives in the store, so it travels with the repo: a clone
/// inherits whatever version its last collaborator acknowledged. When the tool
/// has moved on since, an agent primed against the older one may be carrying
/// stale instructions — and this is the only thing that will say so.
///
/// A never-acked workspace is *not* a finding: that is the state of every
/// workspace `bd init` has ever made, and painting a fresh one yellow on day one
/// is how a check teaches people to ignore it.
#[test]
fn version_drift_is_reported_but_a_never_acked_workspace_is_not() {
    let ws = Ws::new("version");
    assert_eq!(status(&ws.finding("bd version tracking")), "ok");

    // A clone-mate acked at an older bd, and that value came along with the repo.
    assert_eq!(
        ws.run(&["config", "set", "upgrade.acked_version", "0.0.1"])
            .1,
        0
    );
    let f = ws.finding("bd version tracking");
    assert_eq!(status(&f), "warning", "{}", evidence(&f));
    assert!(evidence(&f).contains("0.0.1"), "{}", evidence(&f));

    // ...and doctor must not simply ack it on your behalf. Acknowledging an
    // upgrade asserts that somebody *looked*; a machine silently acking erases
    // the only signal that anyone was ever supposed to.
    ws.run(&["doctor", "--fix"]);
    assert_eq!(
        status(&ws.finding("bd version tracking")),
        "warning",
        "--fix acked the upgrade on the user's behalf, which defeats the point"
    );
    let (acked, _) = ws.run(&["config", "get", "upgrade.acked_version"]);
    assert_eq!(acked, "0.0.1");

    // The real thing does clear it.
    assert_eq!(ws.run(&["upgrade", "ack"]).1, 0);
    assert_eq!(status(&ws.finding("bd version tracking")), "ok");
}

/// Losing `config.yaml` is how a workspace forgets its own prefix. Doctor can
/// rebuild it, because the store still remembers.
#[test]
fn a_missing_config_file_is_found_and_rebuilt_from_the_store() {
    let ws = Ws::new("noconfig");
    ws.create("an issue");
    std::fs::remove_file(ws.config()).unwrap();

    let f = ws.finding("project identity");
    assert_eq!(status(&f), "warning", "{}", evidence(&f));
    assert!(evidence(&f).contains("config.yaml"));

    ws.run(&["doctor", "--fix"]);
    assert!(ws.config().exists(), "--fix did not rebuild config.yaml");

    // And it recovered the *right* prefix rather than defaulting it — writing
    // `prefix: null` here would have manufactured the very drift the family
    // exists to prevent.
    let cfg = std::fs::read_to_string(ws.config()).unwrap();
    assert!(
        cfg.contains("acme"),
        "rebuilt config lost the prefix: {cfg}"
    );
    assert_eq!(status(&ws.finding("id prefix")), "ok");
}

/// Ids bd could not have minted are reported — and are *not* an error. `bd
/// import` faithfully preserves the ids of beads authored in another repo, and
/// failing a pre-commit hook because a peer's id scheme is wider than ours would
/// be absurd.
#[test]
fn foreign_ids_are_a_warning_not_a_failure() {
    let ws = Ws::new("idformat");
    ws.create("a normal issue");

    let path = ws.0.join("in.jsonl");
    std::fs::write(
        &path,
        "{\"id\":\"acme-1\",\"title\":\"legacy sequential\",\"created_at\":\"2024-01-01T00:00:00Z\",\"updated_at\":\"2024-01-01T00:00:00Z\"}\n",
    )
    .unwrap();
    let (out, code) = ws.run(&["import", path.to_str().unwrap()]);
    assert_eq!(code, 0, "import failed: {out}");

    let f = ws.finding("id format");
    assert_eq!(status(&f), "warning", "{}", evidence(&f));
    // The evidence has to name the id. "1 id is malformed" is unactionable.
    assert!(
        evidence(&f).contains("acme-1"),
        "the id is not named: {}",
        evidence(&f)
    );

    // ...and it must not have polluted the prefix census, which is a different
    // question with a different answer.
    assert_eq!(status(&ws.finding("id prefix")), "ok");
}

/// Doctor runs on workspaces too broken to open — that is the job, not an edge
/// case. Standing outside a workspace entirely, this family must produce neither
/// errors nor a wall of yellow: there is no identity here to have drifted.
#[test]
fn outside_a_workspace_the_family_is_quiet() {
    let dir = std::env::temp_dir().join(format!("bd-doctorid-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    assert!(!dir.join(".beads").exists());

    let out = bd()
        .args(["-C", dir.to_str().unwrap(), "--json", "doctor"])
        .output()
        .expect("run bd");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("doctor did not emit JSON ({e}): {stdout}"));

    for f in v["checks"].as_array().expect("checks[]") {
        if f["category"] != "metadata" {
            continue;
        }
        assert_eq!(
            status(f),
            "ok",
            "{} fired outside a workspace: {}",
            f["name"],
            evidence(f)
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// `run()` must never mutate. The rule is stated in the seam docs; here it is
/// enforced against the two checks in this family that *could* write — both of
/// them reach for the store's config table, and one of them rewrites
/// `config.yaml`.
#[test]
fn running_doctor_without_fix_changes_nothing() {
    let ws = Ws::new("readonly");
    ws.create("an issue");
    ws.write_config("claim:\n  lease: 1h\n");
    assert_eq!(ws.run(&["config", "set", "issue.prefix", ""]).1, 0);

    let before_cfg = std::fs::read_to_string(ws.config()).unwrap();
    let before_store = ws.run(&["config", "list"]).0;
    let before_ids = ws.run(&["--json", "list"]).0;

    // Twice: a check that mutated on the first run and then reported itself
    // clean on the second would still be caught by the file comparison below.
    ws.run(&["doctor"]);
    ws.run(&["doctor"]);

    assert_eq!(std::fs::read_to_string(ws.config()).unwrap(), before_cfg);
    assert_eq!(ws.run(&["config", "list"]).0, before_store);
    assert_eq!(ws.run(&["--json", "list"]).0, before_ids);
    assert_eq!(
        status(&ws.finding("id prefix")),
        "error",
        "doctor healed the workspace it was only supposed to look at"
    );
}

/// The check names are the key agents grep for in `--json`. They are a public
/// interface; this pins them.
#[test]
fn the_family_reports_under_stable_names() {
    let ws = Ws::new("names");
    let names: Vec<String> = ws
        .findings()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();

    for want in [
        "id prefix",
        "prefix config",
        "id format",
        "config values",
        "project identity",
        "repo fingerprint",
        "bd version tracking",
    ] {
        assert!(
            names.contains(&want.to_string()),
            "missing check {want:?} in {names:?}"
        );
    }
}

/// A workspace that is not in a git repository has no repository to be
/// fingerprinted against. Beads does not require git, and a check that warned
/// here would be warning about a feature the user simply does not use.
#[test]
fn a_workspace_without_git_is_not_a_fingerprint_failure() {
    let ws = Ws::new("nogit");
    let f = ws.finding("repo fingerprint");
    assert_eq!(status(&f), "ok", "{}", evidence(&f));
    // Guard against the test lying to itself: if the temp dir somehow sits
    // inside a git repo, the message differs and the assertion above is only
    // passing by luck.
    assert!(!has_git_root(&ws.0) || evidence(&f).contains("belongs to this repository"));
}

fn has_git_root(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
