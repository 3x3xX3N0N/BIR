//! `dolt sql-server` lifecycle, against a real `dolt`.
//!
//! Everything here needs the binary, so everything here is guarded by
//! [`require_dolt!`], which **skips loudly**: without dolt these tests cover
//! nothing and say so on stderr rather than reporting as green coverage.
//!
//! The logic that does not need dolt — adoption, ownership, readiness probing,
//! the pid file, log classification, port selection — is unit-tested in
//! `src/server.rs` and does run everywhere.

use bd_dolt::require_dolt;
use bd_dolt::server::{DoltServer, PID_FILE, ensure_identity};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A dolt workspace in a fresh temp dir.
///
/// `dolt init` needs an identity of its own, so it is passed explicitly rather
/// than relying on whatever the machine running the suite happens to have
/// configured.
async fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bd-dolt-it-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = tokio::process::Command::new("dolt")
        .args([
            "init",
            "--name",
            "beads-test",
            "--email",
            "beads@example.invalid",
        ])
        .current_dir(&dir)
        .output()
        .await
        .expect("dolt init should run");
    assert!(
        out.status.success(),
        "dolt init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

fn cleanup(dir: &Path) {
    std::fs::remove_dir_all(dir).ok();
}

fn answers(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// Poll rather than sleep-and-hope: a kill is asynchronous at the OS level and a
/// fixed sleep is either flaky or slow.
fn wait_until_dead(port: u16, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !answers(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    !answers(port)
}

#[tokio::test]
async fn start_wait_ready_stop() {
    require_dolt!();
    let dir = workspace("lifecycle").await;

    let mut server = DoltServer::start(&dir).await.expect("server should start");
    assert_ne!(server.port(), 0);
    assert!(server.is_owned(), "we started it, so we own it");
    // `start` already waits; a second wait must be cheap and must not re-fail.
    server
        .wait_ready(Duration::from_secs(5))
        .await
        .expect("already ready");
    assert!(answers(server.port()));

    // stdout/stderr went to the log, not to ours — this is what keeps a dolt
    // warning out of `bd --json`.
    assert!(server.log_path().is_file());
    assert!(dir.join(PID_FILE).is_file());

    let port = server.port();
    server.stop().await.unwrap();
    assert!(wait_until_dead(port, Duration::from_secs(10)));
    assert!(!dir.join(PID_FILE).exists(), "the record goes with the server");

    // Idempotent.
    server.stop().await.unwrap();
    cleanup(&dir);
}

#[tokio::test]
async fn drop_kills_the_server_it_started() {
    require_dolt!();
    // The one that matters. A leaked `dolt sql-server` holds the database lock,
    // and the next `bd` fails with something that reads like corruption.
    let dir = workspace("drop").await;

    let port = {
        let server = DoltServer::start(&dir).await.expect("server should start");
        let port = server.port();
        assert!(answers(port));
        port
    };

    assert!(
        wait_until_dead(port, Duration::from_secs(10)),
        "dropping a DoltServer must kill the process it started"
    );
    cleanup(&dir);
}

#[tokio::test]
async fn a_second_start_adopts_the_first_server() {
    require_dolt!();
    let dir = workspace("adopt").await;

    let mut first = DoltServer::start(&dir).await.expect("server should start");

    let second = DoltServer::start(&dir).await.expect("should adopt, not spawn");
    assert_eq!(second.port(), first.port(), "same server, not a second one");
    assert!(!second.is_owned(), "adopted: not ours to stop");

    drop(second);
    assert!(
        answers(first.port()),
        "dropping an adopted server must not kill the server someone else started"
    );

    let port = first.port();
    first.stop().await.unwrap();
    assert!(wait_until_dead(port, Duration::from_secs(10)));
    cleanup(&dir);
}

#[tokio::test]
async fn a_stale_pid_file_does_not_stop_a_start() {
    require_dolt!();
    let dir = workspace("stale").await;
    // A previous `bd` that was SIGKILLed leaves this behind. It must not be
    // mistaken for a running server.
    std::fs::write(dir.join(PID_FILE), r#"{"pid":424242,"port":1}"#).unwrap();

    let mut server = DoltServer::start(&dir).await.expect("stale record, fresh server");
    assert!(server.is_owned());
    assert_ne!(server.port(), 1);
    assert!(answers(server.port()));

    let recorded: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(PID_FILE)).unwrap()).unwrap();
    assert_eq!(recorded["port"].as_u64(), Some(server.port() as u64));

    server.stop().await.unwrap();
    cleanup(&dir);
}

#[tokio::test]
async fn dolt_gets_an_identity_before_it_needs_one() {
    require_dolt!();
    let dir = workspace("identity").await;

    ensure_identity(&dir)
        .await
        .expect("dolt init set one, so this is a no-op");

    let out = tokio::process::Command::new("dolt")
        .args(["config", "--get", "user.email"])
        .current_dir(&dir)
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "dolt refuses to commit without one, and says so confusingly"
    );
    cleanup(&dir);
}
