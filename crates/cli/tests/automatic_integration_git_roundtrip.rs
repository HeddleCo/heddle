// SPDX-License-Identifier: Apache-2.0
//! Issue #1895: isolated peer threads must survive land → push → Git clone.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use objects::{
    object::{HeddleNote, StateId},
    store::ObjectStore,
};
use repo::Repository;
use tempfile::TempDir;

const BASE: &str = "def alpha():\n    return \"alpha\"\n\ndef beta():\n    return \"beta\"\n";
const MERGED: &str =
    "def alpha():\n    return \"alpha edited\"\n\ndef beta():\n    return \"beta edited\"\n";

fn checked(output: Output, args: &[&str]) -> Output {
    assert!(
        output.status.success(),
        "{args:?}: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    checked(
        Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run git"),
        args,
    )
}

fn heddle(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .current_dir(cwd)
        .args(args)
        .env("HEDDLE_HOME", home)
        .env("HEDDLE_CONFIG", home.join("config.toml"))
        .env("HEDDLE_PRINCIPAL_NAME", "Roundtrip Test")
        .env("HEDDLE_PRINCIPAL_EMAIL", "roundtrip@example.com")
        .env("HEDDLE_FSMONITOR", "off")
        .env("NO_COLOR", "1")
        .output()
        .expect("run Heddle")
}

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    checked(heddle(home, cwd, args), args)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Workflow {
    Single,
    Automatic,
    Manual,
}

fn roundtrip(workflow: Workflow) {
    let temp = TempDir::new().expect("repro directory");
    let home = TempDir::new().expect("isolated HEDDLE_HOME");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("source directory");
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "Roundtrip Test"]);
    git(&source, &["config", "user.email", "roundtrip@example.com"]);
    fs::write(source.join("app.py"), BASE).expect("base");
    git(&source, &["add", "app.py"]);
    git(&source, &["commit", "-m", "base"]);
    run(home.path(), &source, &["init", "--no-harness-install"]);

    let a = temp.path().join("a");
    let b = temp.path().join("b");
    run(
        home.path(),
        &source,
        &["start", "a", "--path", a.to_str().expect("path")],
    );
    if workflow != Workflow::Single {
        run(
            home.path(),
            &source,
            &["start", "b", "--path", b.to_str().expect("path")],
        );
    }
    let alpha = BASE.replace("\"alpha\"", "\"alpha edited\"");
    fs::write(a.join("app.py"), &alpha).expect("alpha edit");
    run(home.path(), &a, &["capture", "-m", "edit alpha"]);
    if workflow != Workflow::Single {
        let edit = if workflow == Workflow::Manual {
            BASE.replace("\"alpha\"", "\"alpha conflicting\"")
        } else {
            BASE.replace("\"beta\"", "\"beta edited\"")
        };
        fs::write(b.join("app.py"), edit).expect("beta edit or conflict");
        run(home.path(), &b, &["capture", "-m", "edit beta"]);
    }
    run(home.path(), &a, &["ready"]);
    run(home.path(), &a, &["land"]);
    if workflow != Workflow::Single {
        if workflow == Workflow::Manual {
            // Landing `a` restacks the stale sibling `b`, which materializes
            // the conflicting merge in b's checkout. `ready` must refuse with
            // the same continuation advice `status` gives (#1896): a typed
            // stderr envelope, exit 65, and nothing on stdout.
            let status = run(home.path(), &b, &["--output", "json", "status"]);
            let status: serde_json::Value =
                serde_json::from_slice(&status.stdout).expect("status JSON");
            assert_eq!(status["operation"]["kind"], "merge", "{status}");
            let next = status["operation"]["next_action"]
                .as_str()
                .expect("merge recovery action");
            let ready = heddle(home.path(), &b, &["--output", "json", "ready"]);
            let stderr = String::from_utf8_lossy(&ready.stderr);
            assert_eq!(ready.status.code(), Some(65), "{stderr}");
            assert!(ready.stdout.is_empty(), "{stderr}");
            let error: serde_json::Value =
                serde_json::from_slice(&ready.stderr).expect("typed ready refusal");
            assert_eq!(error["kind"], "merge_in_progress", "{error}");
            assert_eq!(error["primary_command"], next, "{error}");
            fs::write(b.join("app.py"), MERGED).expect("manual resolution");
            run(home.path(), &b, &["resolve", "app.py"]);
            run(home.path(), &b, &["continue"]);
        }
        run(home.path(), &b, &["ready"]);
        let landed = run(home.path(), &b, &["--output", "json", "land"]);
        let value: serde_json::Value = serde_json::from_slice(&landed.stdout).expect("land JSON");
        let message = value["message"].as_str().expect("land message");
        assert!(
            message.contains(if workflow == Workflow::Automatic {
                "automatic integration merge"
            } else {
                "manually resolved"
            }),
            "{value}"
        );
    }
    run(home.path(), &source, &["verify"]);
    let expected = if workflow == Workflow::Single {
        alpha.as_str()
    } else {
        MERGED
    };
    assert_eq!(
        fs::read_to_string(source.join("app.py")).expect("landed content"),
        expected
    );

    // Compare the durable State, portable note, and ordered Git parents.
    // Print each checkpoint so a failing round-trip identifies the exact seam.
    let repo = Repository::open(&source).expect("open Heddle source");
    for oid in String::from_utf8(git(&source, &["rev-list", "main"]).stdout)
        .expect("OIDs")
        .lines()
    {
        let output = Command::new("git")
            .current_dir(&source)
            .args(["notes", "--ref=heddle", "show", oid])
            .output()
            .expect("read note");
        if !output.status.success() {
            continue;
        }
        let note = HeddleNote::from_json_bytes(&output.stdout).expect("Heddle note");
        if let Some(state) = &note.source_state {
            let stored = repo
                .store()
                .get_state(&state.id())
                .expect("read State")
                .expect("stored State");
            assert_eq!(
                stored.encode_current_msgpack().expect("stored bytes"),
                state.encode_current_msgpack().expect("embedded bytes")
            );
            assert_eq!(state.id().to_string_full(), note.state_id);
            assert_eq!(state.change_id.to_string_full(), note.change_id);
            eprintln!(
                "commit {oid}, state {}, tree {}, State parents {:?}, parents_rewritten {}\n{}",
                note.state_id,
                state.tree,
                state.parents,
                note.parents_rewritten,
                String::from_utf8_lossy(
                    &git(&source, &["show", "-s", "--format=%T %P", oid]).stdout
                )
            );
        }
    }

    let remote = temp.path().join("remote.git");
    git(
        temp.path(),
        &[
            "init",
            "--bare",
            "-b",
            "main",
            remote.to_str().expect("remote"),
        ],
    );
    git(
        &source,
        &["remote", "add", "origin", remote.to_str().expect("remote")],
    );
    run(home.path(), &source, &["push"]);
    let plain = temp.path().join("plain");
    git(
        temp.path(),
        &[
            "clone",
            remote.to_str().expect("remote"),
            plain.to_str().expect("plain"),
        ],
    );
    git(&plain, &["fsck"]);
    assert_eq!(
        fs::read_to_string(plain.join("app.py")).expect("Git content"),
        expected
    );

    let cloned = temp.path().join("cloned");
    run(
        home.path(),
        temp.path(),
        &[
            "--output",
            "json",
            "clone",
            "--source",
            "git",
            remote.to_str().expect("remote"),
            cloned.to_str().expect("cloned"),
        ],
    );
    assert_eq!(
        fs::read_to_string(cloned.join("app.py")).expect("cloned edits"),
        expected
    );
    run(home.path(), &cloned, &["verify"]);
    if workflow == Workflow::Automatic {
        let git_repo = sley::Repository::open(&remote).expect("bare remote");
        let oid = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)
            .expect("head OID")
            .trim()
            .parse()
            .expect("parse OID");
        let mut note = heddle_git_projection::git_notes::read_note(&git_repo, oid)
            .expect("read integration note")
            .expect("integration note");
        assert!(
            note.parents_rewritten,
            "the portable note must explain identity loss"
        );
        let original = note
            .source_state
            .as_ref()
            .expect("embedded integration")
            .id();
        // Reopen the map: descendants must have durable evidence, not just
        // an in-memory exception for this clone's integration tip.
        let map = ingest::ShaMap::open(cloned.join(".heddle/ingest/sha_map.sqlite"))
            .expect("reopen durable import map");
        let actual = map
            .get_commit(&oid.to_hex())
            .expect("mapped integration")
            .expect("StateId");
        assert_ne!(original, actual);
        assert_eq!(
            map.get_rewritten_state(original).expect("durable rewrite"),
            Some(actual)
        );

        // A self-consistent forged State with an unexplained parent still
        // fails through the same full import path.
        note.parents_rewritten = false;
        let state = note.source_state.as_mut().expect("embedded integration");
        state.parents = vec![StateId::from_bytes([0x99; 32])];
        note.state_id = state.id().to_string_full();
        heddle_git_projection::git_notes::write_note(&git_repo, oid, &note)
            .expect("write forged note");
        let forged = temp.path().join("forged");
        let rejected = heddle(
            home.path(),
            temp.path(),
            &[
                "--output",
                "json",
                "clone",
                "--source",
                "git",
                remote.to_str().expect("remote"),
                forged.to_str().expect("forged"),
            ],
        );
        assert_eq!(rejected.status.code(), Some(74));
        let value: serde_json::Value =
            serde_json::from_slice(&rejected.stderr).expect("rejection JSON on stderr");
        assert_eq!(value["kind"], "git_overlay_clone_import_failed", "{value}");
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("embedded Heddle State differs"),
            "{value}"
        );
        assert!(
            !forged.exists(),
            "failed clone must clean up its destination"
        );
    }
}

#[test]
fn automatic_integration_push_git_clone_roundtrip() {
    roundtrip(Workflow::Automatic);
}

#[test]
fn single_thread_land_push_git_clone_control() {
    roundtrip(Workflow::Single);
}

#[test]
fn manual_conflict_resolution_push_git_clone_control() {
    roundtrip(Workflow::Manual);
}
