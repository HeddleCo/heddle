// SPDX-License-Identifier: Apache-2.0
//! Public repository-import surface and output contracts.

use std::{fs, path::Path, process::Command};

#[cfg(feature = "client")]
#[path = "support/native_hosted_server.rs"]
mod native_hosted_server;

fn run(cwd: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .current_dir(cwd)
        .env("HEDDLE_HOME", home)
        .args(args)
        .output()
        .expect("run heddle")
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn import_local_json_is_one_final_document_and_creates_native_history() {
    let temp = tempfile::tempdir().expect("temp root");
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir_all(&repo).expect("repo dir");
    fs::create_dir_all(&home).expect("home dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Import Contract"]);
    git(&repo, &["config", "user.email", "import@example.test"]);
    fs::write(repo.join("hello.txt"), b"hello\n").expect("fixture file");
    git(&repo, &["add", "hello.txt"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let output = run(&repo, &home, &["import", "local", "--output", "json"]);
    assert!(
        output.status.success(),
        "import local failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let documents = serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter::<serde_json::Value>()
        .collect::<Result<Vec<_>, _>>()
        .expect("valid JSON documents");
    assert_eq!(documents.len(), 1, "finite command must emit one document");
    let result = &documents[0];
    assert_eq!(result["output_kind"], "import_local");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["commits_imported"], 1);
    assert_eq!(result["verification"]["repository_mode"], "native-heddle");
    assert!(repo.join(".heddle").is_dir());
}

#[test]
fn import_url_parse_error_points_to_nearest_help() {
    let temp = tempfile::tempdir().expect("temp root");
    let output = run(
        temp.path(),
        temp.path(),
        &["--output", "json", "import", "url"],
    );
    assert_eq!(output.status.code(), Some(64));
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("JSON parse-error envelope");
    assert_eq!(error["primary_command"], "heddle import url --help");
    assert_eq!(error["recovery_commands"][0], "heddle import url --help");
}

#[test]
fn malformed_line_parse_error_points_to_nearest_help() {
    let temp = tempfile::tempdir().expect("temp root");
    let output = run(
        temp.path(),
        temp.path(),
        &["--output", "json", "context", "set", "--line", "nope"],
    );
    assert_eq!(output.status.code(), Some(64));
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("JSON parse-error envelope");
    assert_eq!(error["primary_command"], "heddle context set --help");
    assert_eq!(error["recovery_commands"][0], "heddle context set --help");
}

#[test]
fn retired_import_surfaces_are_not_registered() {
    let temp = tempfile::tempdir().expect("temp root");
    for args in [
        vec!["adopt", "--help"],
        vec!["remote", "import-source", "--help"],
    ] {
        let output = run(temp.path(), temp.path(), &args);
        assert_eq!(output.status.code(), Some(64), "retired surface {args:?}");
    }
}

#[cfg(feature = "client")]
#[tokio::test]
async fn hosted_import_signs_every_branch_and_ref_bound_sends_no_request() {
    use hosted_client::hosted_runtime::hosted::{ImportSourceRefError, ImportSourceRefs};
    use objects::object::thread_replication::{
        ThreadGenesis, hosted_import::synthetic_initial_base,
    };
    use thread_api::creation::ThreadCreation;

    let temp = tempfile::tempdir().expect("source");
    let path = temp.path();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "user.email", "test@example.test"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "root"]);
    git(path, &["branch", "feature/auth"]);
    git(path, &["branch", "release"]);
    git(path, &["tag", "-am", "annotated", "v1"]);
    let url = path.to_str().expect("source path");
    let refs = ImportSourceRefs::discover(url)
        .await
        .expect("discover refs");
    let spool = uuid::Uuid::now_v7();
    let (mut client, server, captured) = native_hosted_server::start(spool, "main", [7; 32]).await;
    let started = client
        .import_source(
            &spool.to_string(),
            url,
            &refs,
            uuid::Uuid::now_v7().to_string(),
        )
        .await
        .expect("import request");
    {
        let capture = captured.lock().expect("capture");
        let request = capture.import_requests.first().expect("signed request");
        assert_eq!(request.branches.len(), 3);
        let seed = synthetic_initial_base().expect("seed");
        assert_eq!(
            request.initial_base_state,
            seed.encode_current_msgpack().expect("seed bytes")
        );
        let mut names = Vec::new();
        for (index, branch) in request.branches.iter().enumerate() {
            let signed = branch.thread_genesis.as_ref().expect("signed genesis");
            let genesis = ThreadGenesis::decode(&signed.canonical_record).expect("genesis");
            assert_eq!(branch.ref_name, format!("refs/heads/{}", genesis.name));
            assert_eq!(genesis.base, seed.id());
            assert_eq!(genesis.parent, None);
            assert_eq!(genesis.spool, spool.to_string());
            assert_eq!(branch.creator_authority, vec![6; 32]);
            assert_eq!(
                genesis.owner,
                objects::object::thread_replication::GenesisOwner::Account(uuid::Uuid::from_bytes(
                    [9; 16]
                ))
            );
            let creation = ThreadCreation::from_signed_with_authority(
                request.client_operation_id.clone(),
                signed.clone(),
                branch.creator_authority.clone(),
            )
            .expect("verified signature");
            assert_eq!(creation.reference(), &started.threads[index]);
            names.push(genesis.name);
        }
        assert_eq!(names, ["feature/auth", "main", "release"]);
    }
    // 3 branches + 510 tags = 513; the annotated tag's peeled line counts once.
    for index in 1..510 {
        git(path, &["tag", &format!("tag-{index}")]);
    }
    let calls_before = captured.lock().expect("capture").calls.len();
    let error = ImportSourceRefs::discover(url)
        .await
        .expect_err("ref admission bound");
    assert!(matches!(
        error,
        ImportSourceRefError::TooManyRefs {
            branches: 3,
            tags: 510,
            total: 513
        }
    ));
    assert_eq!(
        captured.lock().expect("capture").calls.len(),
        calls_before,
        "no hosted request before ref admission"
    );
    let refused = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .env("PATH", "")
        .env("HEDDLE_HOME", path)
        .args([
            "--output",
            "json",
            "import",
            "url",
            url,
            "--to",
            "acme/repo",
            "--server",
            "localhost:1",
        ])
        .output()
        .expect("CLI admission without a Git executable");
    assert_eq!(refused.status.code(), Some(76));
    let envelope: serde_json::Value =
        serde_json::from_slice(&refused.stderr).expect("typed ref-limit error");
    assert_eq!(envelope["kind"], "import_source_ref_limit");
    assert_eq!(envelope["branches"], 3);
    assert_eq!(envelope["tags"], 510);
    assert_eq!(envelope["total_refs"], 513);
    assert_eq!(envelope["max_refs"], 512);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("3 branches and 510 tags (513 refs)"),
        "{stderr}"
    );
    assert_eq!(captured.lock().expect("capture").calls.len(), calls_before);
    client.close().await;
    server.await.expect("server");
}

#[test]
fn import_url_thread_flag_is_retired() {
    let temp = tempfile::tempdir().expect("root");
    let output = run(
        temp.path(),
        temp.path(),
        &[
            "import",
            "url",
            "https://example.test/repo.git",
            "--to",
            "acme/repo",
            "--thread",
            "main",
        ],
    );
    assert_eq!(output.status.code(), Some(64));
}
