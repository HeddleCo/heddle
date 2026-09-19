// SPDX-License-Identifier: Apache-2.0
//! Local Git adoption through hosted v2 publication and native Fetch.

#[path = "support/mod.rs"]
mod support;
use support::*;

#[path = "support/native_hosted_server.rs"]
mod native_hosted_server;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit_file(path: &Path, body: &str, message: &str) {
    std::fs::write(path.join("story.txt"), body).expect("write Git fixture");
    git(path, &["add", "story.txt"]);
    git(path, &["commit", "-m", message]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_history_round_trips_through_hosted_publication_and_fetch() {
    let temp = TempDir::new().expect("test directory");
    let source_home = temp.path().join("source-home");
    std::fs::create_dir(&source_home).expect("source HEDDLE_HOME");
    let clone_home = std::path::PathBuf::from(
        std::env::var_os("HEDDLE_HOME").expect("test command must set a fresh HEDDLE_HOME"),
    );
    let work = temp.path().join("work");
    std::fs::create_dir(&work).expect("Git worktree");
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.name", "Git Author"]);
    git(&work, &["config", "user.email", "author@example.com"]);
    commit_file(&work, "one\n", "root by Git author");
    commit_file(&work, "one\ntwo\n", "head by Git author");

    heddle_env(
        &["adopt"],
        Some(&work),
        &[(
            "HEDDLE_HOME",
            source_home.to_str().expect("source home path"),
        )],
    )
    .expect("adopt Git repository");
    let adopted = repo::Repository::open(&work).expect("open adopted repository");
    let source_state = adopted
        .current_state()
        .expect("read adopted HEAD")
        .expect("adopted HEAD");
    let source_attribution = source_state.attribution.clone();
    assert_eq!(source_attribution.principal.name_lossy(), "Git Author");
    assert_eq!(
        source_attribution.principal.email_lossy(),
        "author@example.com"
    );
    let spool = std::fs::read_to_string(work.join(".heddle/spool-id"))
        .expect("adopted Spool identity")
        .trim()
        .parse()
        .expect("Spool UUID");
    let thread = adopted
        .native_thread("main")
        .expect("adopted main identity");
    let thread_id = *thread.thread_id().as_bytes();

    let (mut hosted, server, captured) =
        native_hosted_server::start(spool, "main", thread_id).await;
    let pushed = hosted
        .push_profiled(
            &adopted,
            "acme/widgets",
            source_state.id(),
            "main",
            false,
            "adopt-hosted-round-trip".into(),
        )
        .await
        .expect("publish adopted native source")
        .0;
    assert!(pushed.success, "hosted publication must be accepted");
    assert_eq!(pushed.new_state, Some(source_state.id()));
    {
        let publication = captured.lock().unwrap_or_else(|poison| poison.into_inner());
        assert!(publication.thread_genesis.is_some());
        assert!(!publication.operations.is_empty());
        assert!(!publication.pack_data.is_empty());
        assert!(!publication.index_data.is_empty());
    }

    // The outer test command supplies the clone process's fresh HEDDLE_HOME;
    // adoption above explicitly used the disjoint source home.
    assert_ne!(source_home, clone_home);
    let clone = temp.path().join("clone");
    let (pulled, cloned) = hosted
        .clone_pull_with_depth_and_materialization(
            "acme/widgets",
            Some("main"),
            None,
            hosted_client::hosted_runtime::hosted::PullMaterialization::Full,
            |_| {
                let repo = repo::Repository::init(&clone).map_err(wire::ProtocolError::from)?;
                repo.install_native_spool_id(spool)
                    .map_err(|error| wire::ProtocolError::InvalidState(error.to_string()))?;
                Ok(repo)
            },
        )
        .await
        .expect("clone published adopted source");
    assert!(pulled.success);
    assert_eq!(pulled.final_state, Some(source_state.id()));
    let cloned_state = cloned
        .store()
        .get_state(&source_state.id())
        .expect("read cloned HEAD")
        .expect("cloned HEAD state");
    assert_eq!(cloned_state.attribution, source_attribution);
    let cloned_tree = cloned
        .store()
        .get_tree(&cloned_state.tree)
        .expect("read cloned HEAD tree")
        .expect("cloned HEAD tree");
    let story = cloned_tree
        .entries()
        .iter()
        .find(|entry| entry.name() == "story.txt")
        .and_then(|entry| entry.blob_hash())
        .expect("cloned story blob");
    assert_eq!(
        cloned
            .store()
            .get_blob(&story)
            .expect("read cloned story")
            .expect("cloned story")
            .content(),
        b"one\ntwo\n"
    );

    hosted.close().await;
    server.await.expect("hosted test server");
}
