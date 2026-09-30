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

/// Installs `home` as this process's `HEDDLE_HOME` until dropped.
///
/// Publication and clone run in-process, so they resolve that variable from
/// this process rather than from a child command. The previous value is
/// restored so a later test in the same process keeps the runner's home.
struct ProcessHeddleHome {
    previous: Option<std::ffi::OsString>,
}

impl ProcessHeddleHome {
    fn install(home: &Path) -> Self {
        let previous = std::env::var_os("HEDDLE_HOME");
        unsafe { std::env::set_var("HEDDLE_HOME", home) };
        Self { previous }
    }
}

impl Drop for ProcessHeddleHome {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var("HEDDLE_HOME", value) },
            None => unsafe { std::env::remove_var("HEDDLE_HOME") },
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_history_round_trips_through_hosted_publication_and_fetch() {
    let temp = TempDir::new().expect("test directory");
    let source_home = temp.path().join("source-home");
    std::fs::create_dir(&source_home).expect("source HEDDLE_HOME");
    let clone_home = TempDir::new().expect("clone HEDDLE_HOME");
    let _clone_home_env = ProcessHeddleHome::install(clone_home.path());
    let work = temp.path().join("work");
    std::fs::create_dir(&work).expect("Git worktree");
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.name", "Git Author"]);
    git(&work, &["config", "user.email", "author@example.com"]);
    commit_file(&work, "one\n", "root by Git author");
    commit_file(&work, "one\ntwo\n", "head by Git author");

    heddle_env(
        &["import", "local"],
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

    // The in-process clone uses the fresh HEDDLE_HOME created above.
    // Adoption passed the disjoint source home to its own command.
    assert_ne!(source_home, clone_home.path());
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
    assert_eq!(
        cloned
            .refs()
            .get_thread(&objects::object::ThreadName::new("main"))
            .expect("read clone thread ref"),
        None,
        "native object installation must not publish the clone checkout ref"
    );
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

    cloned
        .fast_forward_attached_from_materialized_state(&source_state.id(), None)
        .expect("publish materialized clone checkout");
    let newer_local_state = objects::object::State::new(
        cloned_state.tree,
        vec![source_state.id()],
        source_attribution.clone(),
    );
    let newer_local = newer_local_state.id();
    cloned
        .store()
        .put_state(&newer_local_state)
        .expect("store newer local state");
    assert_ne!(newer_local, source_state.id());
    cloned
        .set_thread_recorded(&objects::object::ThreadName::new("main"), &newer_local)
        .expect("advance local main without changing its tree");
    hosted
        .fetch_state(&cloned, "acme/widgets", "main", source_state.id())
        .await
        .expect("hydrate historical hosted revision");
    assert_eq!(
        cloned
            .refs()
            .get_thread(&objects::object::ThreadName::new("main"))
            .expect("read main after hydration"),
        Some(newer_local),
        "historical hydration must leave the current thread at its newer tip"
    );
    assert_eq!(
        cloned.head().expect("HEAD after hydration"),
        Some(newer_local)
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("story.txt")).expect("checkout after hydration"),
        "one\ntwo\n"
    );

    hosted.close().await;
    server.await.expect("hosted test server");
}

#[tokio::test]
async fn native_hosted_review_verbs_fit_weft_budget() {
    let (client, server, _) =
        native_hosted_server::start(uuid::Uuid::from_bytes([22; 16]), "feature", [23; 32]).await;
    // These are the two observation entry points used by the four review verbs.
    // The native server checks every page, including continuation requests.
    for verb in ["show", "list", "readiness", "approve"] {
        let snapshot = if verb == "readiness" {
            client
                .observe_landing_assessment("acme/widgets", "feature", "main")
                .await
        } else {
            client.observe_review("acme/widgets", "feature").await
        };
        let snapshot = snapshot.unwrap_or_else(|error| panic!("review {verb}: {error}"));
        assert_eq!(
            snapshot.decisions.len(),
            65,
            "review {verb} must drain every page"
        );
        if verb == "readiness" {
            let target = snapshot
                .overview
                .landing_assessment
                .expect("landing assessment")
                .target
                .expect("landing target");
            assert_eq!(target.id.expect("landing target ID").value, vec![24; 32]);
        }
    }
    // Verify this CLI fixture also rejects the original production request.
    use api::heddle::api::v1alpha2 as v2;
    let reference = client
        .resolve_thread_ref("acme/widgets", "feature")
        .await
        .expect("Thread");
    let remote = client.native().await.expect("native client");
    let mut observation = remote
        .observe::<thread_api::rpc::ThreadServiceObserveThread>(
            v2::ObserveThreadRequest {
                thread: Some(reference),
                sections: vec![v2::ThreadSection::Review as i32],
                pages: Some(v2::ThreadPages {
                    reviews: Some(v2::PageRequest {
                        size: 128,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                observe: Some(v2::ObserveOptions {
                    mode: v2::ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("open observation");
    let error = observation
        .next_commit()
        .await
        .err()
        .expect("128 rows must be rejected");
    assert!(
        error
            .to_string()
            .contains("Thread section pages exceed shared item budget"),
        "{error}"
    );
    client.close().await;
    server.await.expect("native hosted server");
}
