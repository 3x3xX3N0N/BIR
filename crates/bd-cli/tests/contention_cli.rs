//! Many sessions, one board, real processes.
//!
//! Ultraphrenia — several agents working the same graph at once — is this
//! port's headline feature, and it is exactly the load profile that melted the
//! upstream deployment this test is modeled on: concurrent sessions hammering
//! one store until latency tripped a fail-open gate and claims started
//! colliding. The unit suites prove the claim SQL is atomic *in-process*; this
//! is the only test that proves it across **processes**, where the SQLite
//! single-writer lock and the 10s busy timeout are the actual arbiters.
//!
//! The invariant under test is the claim exclusivity contract from the
//! README: *"Two sessions never pick up the same issue."* Every worker races
//! `bd ready` → `bd update --claim` at maximum contention (they all want the
//! head of the same list), and at the end every issue must have been won by
//! **exactly one** worker and closed **exactly once**. A double-grant
//! anywhere is the bug this file exists to catch.
//!
//! Failure modes this has to stay honest about:
//! * a worker whose *claim* succeeds must close the issue — an abandoned claim
//!   would deadlock the pool if leases did not lapse, so the final sweep
//!   asserts nothing is left in progress;
//! * `SQLITE_BUSY` under load is allowed to fail an individual command (that
//!   is the backend saying "not now"), but a *lost* claim must never look like
//!   a *won* one — exit code 0 is the only thing a worker trusts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

fn bd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bd"))
}

struct Ws(PathBuf);

impl Ws {
    fn new(tag: &str) -> Ws {
        let p = std::env::temp_dir().join(format!(
            "bd-contend-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        let ws = Ws(std::fs::canonicalize(&p).unwrap());
        assert_eq!(ws.run("setup", &["init", "--prefix", "race"]).2, 0, "init");
        ws
    }

    /// (stdout, stderr, exit code), as `actor`. stderr comes back separately —
    /// when a command fails under contention, the *reason* is the entire
    /// finding.
    fn run(&self, actor: &str, args: &[&str]) -> (String, String, i32) {
        let out = bd()
            .args(["-C", self.0.to_str().unwrap()])
            .args(args)
            .env("BEADS_ACTOR", actor)
            .env("BEADS_SESSION", actor)
            .output()
            .expect("run bd");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn ready_ids(&self, actor: &str) -> Vec<String> {
        let (out, err, code) = self.run(actor, &["--json", "ready", "--limit", "0"]);
        assert_eq!(code, 0, "`bd ready` must not fail under contention: {err}");
        serde_json::from_str::<serde_json::Value>(&out)
            .unwrap_or_else(|e| panic!("ready --json did not emit JSON ({e}): {out}"))
            .as_array()
            .expect("ready is an array")
            .iter()
            .map(|i| i["id"].as_str().expect("issue has an id").to_string())
            .collect()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

const WORKERS: usize = 6;
const ISSUES: usize = 18;

#[test]
fn concurrent_workers_never_win_the_same_claim() {
    let ws = Ws::new("claims");

    for n in 0..ISSUES {
        let (out, err, code) = ws.run("setup", &["create", &format!("task {n}")]);
        assert_eq!(code, 0, "create failed: {out} {err}");
    }

    // Every worker runs the README's four-line loop, all through the head of
    // the same ready list — maximal collision on every claim.
    let claims: Vec<(String, Vec<String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..WORKERS)
            .map(|w| {
                let ws = &ws;
                scope.spawn(move || {
                    let me = format!("worker-{w}");
                    let mut won = Vec::new();
                    loop {
                        let ready = ws.ready_ids(&me);
                        let Some(id) = ready.first() else { break };

                        let (_, _, code) = ws.run(&me, &["update", id, "--claim"]);
                        if code != 0 {
                            // Lost the race (or the store said "not now").
                            // Either way: not ours, try again. What a loser
                            // must never do is proceed as if it had won.
                            continue;
                        }
                        let (out, err, code) =
                            ws.run(&me, &["close", id, "--reason", "done"]);
                        assert_eq!(
                            code, 0,
                            "{me} won the claim on {id} and must be able to \
                             close it: {out} {err}"
                        );
                        won.push(id.clone());
                    }
                    (me, won)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Exactly-once: every issue was won by one worker, and no issue by two.
    let mut owner: HashMap<&str, &str> = HashMap::new();
    for (worker, won) in &claims {
        for id in won {
            if let Some(prev) = owner.insert(id, worker) {
                panic!(
                    "{id} was claim-granted to BOTH {prev} and {worker} — \
                     the claim exclusivity contract is broken"
                );
            }
        }
    }
    assert_eq!(
        owner.len(),
        ISSUES,
        "every issue must have been worked exactly once; winners by worker: {:?}",
        claims
            .iter()
            .map(|(w, v)| (w.as_str(), v.len()))
            .collect::<Vec<_>>()
    );

    // And the board agrees: nothing ready, nothing stuck in progress.
    assert!(
        ws.ready_ids("auditor").is_empty(),
        "the board still offers work after every issue was closed"
    );
    let (out, err, code) = ws.run("auditor", &["--json", "list", "--status", "in_progress"]);
    assert_eq!(code, 0, "{err}");
    let stuck = serde_json::from_str::<serde_json::Value>(&out).unwrap();
    assert_eq!(
        stuck.as_array().map(Vec::len),
        Some(0),
        "an issue is stranded in progress — a worker's claim outlived its close: {out}"
    );
}
