// SPDX-License-Identifier: Apache-2.0
//! Integration coverage for the box-scoped network daemon
//! (`heddle netd …`, heddle#1533 piece 1).
//!
//! These spawn the real `heddle` binary so they exercise the whole
//! path: persisted-node-id bind, discovery-file publication, the
//! single-writer guard, and the restart-survival clause #1620 depends
//! on. They are Unix + `client` only — the daemon binds an Iroh
//! endpoint and control-plane UDS, neither of which exists otherwise.

#![cfg(all(unix, feature = "client"))]

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

fn serve(home: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(["netd", "serve"])
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("HOME", home)
        .env("HEDDLE_HOME", home)
        .env_remove("HEDDLE_CONFIG")
        .env_remove("HEDDLE_CREDENTIAL")
        .env("NO_COLOR", "1")
        .spawn()
        .expect("spawn `heddle netd serve`")
}

/// Run a one-shot `heddle netd …` verb to completion, capturing output.
fn run(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(home)
        .stdin(Stdio::null())
        .env("HOME", home)
        .env("HEDDLE_HOME", home)
        .env_remove("HEDDLE_CONFIG")
        .env_remove("HEDDLE_CREDENTIAL")
        .env("NO_COLOR", "1")
        .output()
        .expect("run `heddle netd` verb")
}

fn endpoint_path(home: &Path) -> PathBuf {
    home.join("state").join("heddle-netd.endpoint.json")
}

/// Poll until the discovery file exists and parses, or panic after
/// `timeout`. Binding an Iroh endpoint plus writing the file is fast,
/// but relay-actor startup adds jitter, so the window is generous.
fn wait_for_discovery(home: &Path, timeout: Duration) -> Value {
    let path = endpoint_path(home);
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(&path)
            && let Ok(value) = serde_json::from_str::<Value>(&contents)
        {
            return value;
        }
        if Instant::now() >= deadline {
            panic!(
                "netd discovery file {} did not appear within {:?}",
                path.display(),
                timeout
            );
        }
        sleep(Duration::from_millis(50));
    }
}

fn node_id_of(discovery: &Value) -> String {
    discovery
        .get("node_id")
        .and_then(Value::as_str)
        .expect("discovery file advertises a node_id")
        .to_string()
}

/// Kill and reap a serve child so a panicking assertion never leaks a
/// live daemon into the rest of the run.
struct Daemon(Option<Child>);

impl Daemon {
    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn discovery_advertises_persisted_node_id() {
    let home = TempDir::new().unwrap();
    let mut daemon = Daemon(Some(serve(home.path())));

    let discovery = wait_for_discovery(home.path(), Duration::from_secs(20));
    let node_id = node_id_of(&discovery);

    assert_eq!(node_id.len(), 64, "node id must be a 64-hex ed25519 key");
    assert!(
        node_id.chars().all(|c| c.is_ascii_hexdigit()),
        "node id must be lower-hex, got {node_id}"
    );
    assert_eq!(
        discovery.get("version").and_then(Value::as_u64),
        Some(1),
        "discovery advertises the netd protocol version"
    );
    assert!(
        discovery.get("pid").and_then(Value::as_u64).is_some(),
        "discovery records the serving pid"
    );

    daemon.stop();
}

/// The #1620 acceptance clause: the advertised node id is bound to
/// the *persisted* device identity, so killing and restarting the
/// daemon rebinds the same node id.
#[test]
fn node_id_survives_kill_and_restart() {
    let home = TempDir::new().unwrap();

    let mut first = Daemon(Some(serve(home.path())));
    let before = node_id_of(&wait_for_discovery(home.path(), Duration::from_secs(20)));
    first.stop();

    // Give the OS a moment to release the socket name; the restart
    // reclaims the stale (dead-pid) discovery record on its own.
    sleep(Duration::from_millis(200));

    let mut second = Daemon(Some(serve(home.path())));
    let after = node_id_of(&wait_for_discovery(home.path(), Duration::from_secs(20)));

    assert_eq!(
        before, after,
        "node id must survive a daemon restart (persisted device identity)"
    );
    second.stop();
}

/// Two processes must never both bind the device node id: a second
/// `serve` refuses while the first is alive.
#[test]
fn second_serve_refuses_while_first_is_live() {
    let home = TempDir::new().unwrap();
    let mut daemon = Daemon(Some(serve(home.path())));
    // Ensure the first daemon has published its discovery + socket.
    wait_for_discovery(home.path(), Duration::from_secs(20));

    let second = run(home.path(), &["netd", "serve"]);
    assert!(
        !second.status.success(),
        "second serve must fail while the first is alive"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already serving") || stderr.contains("refusing"),
        "second serve must explain the single-writer refusal, got: {stderr}"
    );

    daemon.stop();
}

/// `status` reports the running daemon and its node id; `stop` drains
/// it and the discovery file disappears.
#[test]
fn status_and_stop_round_trip() {
    let home = TempDir::new().unwrap();
    let mut daemon = Daemon(Some(serve(home.path())));
    let node_id = node_id_of(&wait_for_discovery(home.path(), Duration::from_secs(20)));

    let status = run(home.path(), &["netd", "status", "--output", "json"]);
    assert!(status.status.success(), "status must succeed");
    let status_json: Value =
        serde_json::from_slice(&status.stdout).expect("status emits JSON");
    assert_eq!(status_json.get("running").and_then(Value::as_bool), Some(true));
    assert_eq!(
        status_json.get("node_id").and_then(Value::as_str),
        Some(node_id.as_str()),
        "status reports the advertised node id"
    );

    let stop = run(home.path(), &["netd", "stop"]);
    assert!(stop.status.success(), "stop must succeed");

    // The serve process should now exit on its own; reap it.
    if let Some(mut child) = daemon.0.take() {
        let _ = child.wait();
    }
    assert!(
        !endpoint_path(home.path()).exists(),
        "discovery file must be gone after stop"
    );

    let after = run(home.path(), &["netd", "status"]);
    assert!(after.status.success());
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("not running"),
        "status after stop must report not running"
    );
}
