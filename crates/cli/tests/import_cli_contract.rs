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

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
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
    output.stdout
}

fn import_graph_fixture(side_branches: usize, partial: bool) {
    use objects::{object::ThreadName, store::ObjectStore as _};

    let temp = tempfile::tempdir().expect("fixture");
    let path = temp.path();
    let home = path.join("home");
    fs::create_dir(&home).expect("home");
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Import Graph"]);
    git(path, &["config", "user.email", "import@example.test"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "root"]);
    let mut branches = (0..side_branches)
        .map(|index| format!("branch-{index}"))
        .collect::<Vec<_>>();
    for branch in &branches {
        git(path, &["switch", "-qc", branch, "main"]);
        fs::write(path.join(branch), branch).expect("branch file");
        git(path, &["add", branch]);
        git(path, &["commit", "-qm", branch]);
    }
    git(path, &["switch", "-q", "main"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "main"]);
    if !branches.is_empty() {
        git(path, &["branch", "branch-main"]);
        let mut args = vec!["merge", "-q", "--no-ff", "-m", "merge"];
        args.extend(branches.iter().map(String::as_str));
        git(path, &args);
        branches.push("branch-main".into());
    }
    git(path, &["commit", "-q", "--allow-empty", "-m", "after"]);
    let graph = String::from_utf8(git(
        path,
        &[
            "rev-list",
            "--reverse",
            "--topo-order",
            "--parents",
            "--all",
        ],
    ))
    .expect("Git graph");
    let mut original_states = std::collections::BTreeMap::new();
    let mut original_geneses = std::collections::BTreeMap::new();
    if partial {
        use objects::object::{Tree, thread_replication::initial_base::synthetic_initial_base};

        // Reproduce the failed initial registration, retaining its sidecar,
        // converted graph, original geneses, and already admitted branches.
        let repo = repo::Repository::bootstrap_git_overlay(path).expect("overlay");
        let seed = synthetic_initial_base().expect("seed");
        repo.store().put_tree(&Tree::new()).expect("seed tree");
        repo.store().put_state(&seed).expect("seed State");
        let (_, map) = ingest::import_git_into_with_options(
            path,
            path,
            ingest::ImportOptions {
                root_parent: Some(seed.id()),
                ..Default::default()
            },
        )
        .expect("converted Git graph");
        for line in graph.lines() {
            let oid = line.split_whitespace().next().expect("OID");
            original_states.insert(
                oid.to_string(),
                map.get_commit(oid).expect("map").expect("original State"),
            );
        }
        let mut refused = false;
        for (name, tip) in repo.refs().list_threads_with_states().expect("branches") {
            let replica = repo
                .create_native_thread(name.as_ref(), seed.id(), None, "")
                .expect("original genesis");
            original_geneses.insert(name.to_string(), replica.genesis().expect("genesis"));
            if let Err(error) = repo.record_native_source(name.as_ref(), tip) {
                assert!(
                    error.to_string().contains("multiple source Threads"),
                    "{error}"
                );
                refused = true;
                break;
            }
        }
        assert!(refused, "fixture must retain the failed merge registration");
    }
    let output = run(path, &home, &["import", "local", "--output", "json"]);
    assert!(
        output.status.success(),
        "import local failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("import report");
    assert_eq!(report["commits_imported"], graph.lines().count());
    if partial {
        assert_eq!(report["initialized"], false);
    }
    let repo = repo::Repository::open(path).expect("native repository");
    let map = ingest::ShaMap::open(path.join(".heddle/ingest/sha_map.sqlite")).expect("map");
    let mut merges = 0;
    for line in graph.lines() {
        let mut commits = line.split_whitespace();
        let oid = commits.next().expect("commit OID");
        let parents = commits
            .map(|parent| {
                map.get_commit(parent)
                    .expect("parent lookup")
                    .expect("parent")
            })
            .collect::<Vec<_>>();
        let state_id = map.get_commit(oid).expect("lookup").expect("StateId");
        if let Some(original) = original_states.get(oid) {
            assert_eq!(&state_id, original, "retry preserves State identity");
        }
        let state = repo
            .store()
            .get_state(&state_id)
            .expect("lookup")
            .expect("State");
        // Local publication anchors only Git roots to the synthetic Spool base.
        if !parents.is_empty() {
            assert_eq!(state.parents, parents, "ordered real parents of {oid}");
        }
        if parents.len() > 1 {
            merges += 1;
            assert_eq!(parents.len(), side_branches + 1);
        }
    }
    for branch in branches.iter().map(String::as_str).chain(["main"]) {
        let tip_oid = String::from_utf8(git(path, &["rev-parse", branch])).expect("tip OID");
        let tip = repo
            .refs()
            .get_thread(&ThreadName::new(branch))
            .expect("ref")
            .expect("tip");
        assert_eq!(
            tip,
            map.get_commit(tip_oid.trim())
                .expect("map")
                .expect("tip State")
        );
        let replica = repo.native_thread(branch).expect("native Thread");
        if let Some(original) = original_geneses.get(branch) {
            assert_eq!(&replica.genesis().expect("genesis"), original);
        }
        let reachable =
            String::from_utf8(git(path, &["rev-list", branch, "--"])).expect("ancestry");
        for oid in reachable.lines() {
            let id = map.get_commit(oid).expect("map").expect("ancestor");
            let admitted = replica
                .accepted_source_revision(id)
                .expect("source lookup")
                .expect("admitted ancestor");
            assert_eq!(admitted.id(), id);
        }
    }
    assert_eq!(merges, usize::from(side_branches > 0));
}

#[test]
fn import_local_preserves_merge_graph() {
    import_graph_fixture(1, false);
}

#[test]
fn import_local_preserves_octopus_graph() {
    import_graph_fixture(3, false);
}

#[test]
fn import_local_preserves_linear_graph() {
    import_graph_fixture(0, false);
}

#[test]
fn import_local_resumes_partial_merge_graph() {
    import_graph_fixture(1, true);
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
async fn hosted_import_job_requirement_and_ref_bound_send_no_import_request() {
    use hosted_client::hosted_runtime::hosted::{ImportSourceRefError, ImportSourceRefs};

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
    let error = client
        .import_source(
            &spool.to_string(),
            url,
            &refs,
            uuid::Uuid::now_v7().to_string(),
        )
        .await
        .expect_err("a capable peer still requires an explicit import job");
    assert!(
        error
            .to_string()
            .contains("ImportSource requires CommitImportJob"),
        "{error}"
    );
    assert!(captured.lock().expect("capture").import_requests.is_empty());
    // 3 branches + 4094 tags = 4097, one past the 4096 bound; the annotated
    // tag's peeled line counts once. One `update-ref` writes them all.
    let head = String::from_utf8(git(path, &["rev-parse", "HEAD"])).expect("HEAD oid");
    let commands = (1..4094)
        .map(|index| format!("create refs/tags/tag-{index} {}\n", head.trim()))
        .collect::<String>();
    let mut update = Command::new("git")
        .current_dir(path)
        .args(["update-ref", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn update-ref");
    std::io::Write::write_all(
        update.stdin.as_mut().expect("update-ref stdin"),
        commands.as_bytes(),
    )
    .expect("write tags");
    assert!(update.wait().expect("update-ref").success());
    let calls_before = captured.lock().expect("capture").calls.len();
    let error = ImportSourceRefs::discover(url)
        .await
        .expect_err("ref admission bound");
    assert!(matches!(
        error,
        ImportSourceRefError::TooManyRefs {
            branches: 3,
            tags: 4094,
            total: 4097
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
    assert_eq!(envelope["tags"], 4094);
    assert_eq!(envelope["total_refs"], 4097);
    assert_eq!(envelope["max_refs"], 4096);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("3 branches and 4094 tags (4097 refs); maximum is 4096"),
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
