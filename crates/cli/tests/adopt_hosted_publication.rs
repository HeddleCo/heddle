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
    _guard: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl ProcessHeddleHome {
    fn install(home: &Path) -> Self {
        let guard = ::config::credentials::lock_test_env();
        let previous = std::env::var_os("HEDDLE_HOME");
        unsafe { std::env::set_var("HEDDLE_HOME", home) };
        Self {
            _guard: guard,
            previous,
        }
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

#[test]
fn adopted_history_publication_preserves_authorship_and_evergreen_fetch_fails_closed() {
    on_large_stack(adopted_history_round_trip);
}

async fn adopted_history_round_trip() {
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
        assert_eq!(
            publication.operations.len(),
            1,
            "small pushes use one batch"
        );
        assert!(!publication.pack_data.is_empty());
        assert!(!publication.index_data.is_empty());
    }

    // The in-process clone uses the fresh HEDDLE_HOME created above.
    // Adoption passed the disjoint source home to its own command.
    assert_ne!(source_home, clone_home.path());
    let clone = temp.path().join("clone");
    let error = hosted
        .clone_pull_with_depth_and_materialization(
            "acme/widgets",
            Some("main"),
            None,
            hosted_client::hosted_runtime::hosted::PullMaterialization::Full,
            |_| repo::Repository::init(&clone).map_err(wire::ProtocolError::from),
        )
        .await
        .err()
        .expect("Part 2 must supply fresh witness evidence");
    assert!(
        matches!(error, wire::ProtocolError::InvalidState(reason) if reason == "source preparation: pin hosted executor for native Thread")
    );
    let staged = repo::Repository::open(&clone).expect("staging repository");
    let original = repo::thread_replication::ThreadReplica::open(
        staged.heddle_dir(),
        objects::object::ContentHash::from_bytes(thread_id),
    )
    .expect("signature-only genesis staging");
    assert!(
        original
            .source_operation_page(source_state.id(), None, 1)
            .expect("no admitted source")
            .is_empty()
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

#[test]
fn hosted_publication_batches_140_states_and_rejects_evergreen_fetch() {
    on_large_stack(|| large_history_round_trip(140, false));
}

#[test]
fn hosted_publication_batches_1000_states_and_later_capture() {
    on_large_stack(|| large_history_round_trip(1000, true));
}

async fn large_history_round_trip(states: usize, capture_again: bool) {
    use std::io::Write;

    use prost::Message;

    let temp = TempDir::new().expect("test directory");
    let home = temp.path().join("home");
    std::fs::create_dir(&home).expect("home");
    let _home = ProcessHeddleHome::install(&home);
    let work = temp.path().join("work");
    std::fs::create_dir(&work).expect("work");
    git(&work, &["init", "-q", "-b", "main"]);
    let mut importer = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(&work)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("Git importer");
    {
        let input = importer.stdin.as_mut().expect("import input");
        let git_states = if capture_again { 1 } else { states };
        for index in 0..git_states {
            let content = format!("State {index}\n");
            writeln!(input, "commit refs/heads/main\ncommitter Git Author <author@example.com> {} +0000\ndata 0\nM 100644 inline story.txt\ndata {}\n{}", 1_700_000_000 + index, content.len(), content)
                .expect("write commit");
        }
        writeln!(input, "done").expect("finish import");
    }
    drop(importer.stdin.take());
    assert!(importer.wait().expect("Git importer").success());
    git(&work, &["reset", "--hard"]);
    heddle_env(
        &["import", "local"],
        Some(&work),
        &[("HEDDLE_HOME", home.to_str().expect("home path"))],
    )
    .expect("adopt history");
    let adopted = repo::Repository::open(&work).expect("adopted repo");
    let mut tip = adopted.current_state().expect("HEAD").expect("State");
    if capture_again {
        // Build admitted native originals directly, retaining the real signing
        // and admission path without repeating capture's identity/reference
        // preparation for this unchanged tree. Git adoption is covered above.
        use crypto::{Signer, thread_operation::SignedOperation};
        use objects::object::thread_replication::{
            AuthoredCapture, ThreadOperation, ThreadOperationBody,
        };
        let replica = adopted.native_thread("main").expect("native Thread");
        let signer = adopted
            .native_thread_signer(&replica)
            .expect("source signer");
        let mut parent = replica
            .source_operation_page(tip.id(), None, 1)
            .expect("root original")[0];
        for index in 1..states {
            let state = objects::object::State::new_snapshot(
                tip.tree,
                vec![tip.id()],
                tip.attribution.clone(),
            )
            .with_intent(format!("native capture {index}"));
            adopted.store().put_state(&state).expect("store capture");
            let operation = ThreadOperation {
                version: 1,
                thread: replica.thread_id(),
                parents: std::collections::BTreeSet::from([parent]),
                publisher: signer.public_key().try_into().expect("publisher"),
                body: ThreadOperationBody::Capture(AuthoredCapture::local(
                    state
                        .encode_current_msgpack()
                        .expect("canonical State")
                        .into(),
                )),
            };
            let signed = SignedOperation::sign(&operation, &signer).expect("sign capture");
            assert_eq!(
                replica
                    .receive_prepared_source(&signed, adopted.store(), |_| Ok(()))
                    .expect("admit capture"),
                objects::object::thread_replication::Admission::Accepted
            );
            parent = operation.id().expect("capture ID");
            tip = state;
        }
        adopted
            .set_thread_recorded(&objects::object::ThreadName::new("main"), &tip.id())
            .expect("advance main");
    }
    let spool = std::fs::read_to_string(work.join(".heddle/spool-id"))
        .expect("Spool")
        .trim()
        .parse()
        .expect("UUID");
    let thread_id = *adopted
        .native_thread("main")
        .expect("Thread")
        .thread_id()
        .as_bytes();
    let (mut hosted, server, captured) =
        native_hosted_server::start(spool, "main", thread_id).await;
    let pushed = hosted
        .push_profiled(
            &adopted,
            "acme/widgets",
            tip.id(),
            "main",
            false,
            format!("large-{states}"),
        )
        .await
        .expect("publish full history")
        .0;
    assert!(pushed.success);
    assert_eq!(pushed.new_state, Some(tip.id()));
    {
        let publication = captured.lock().expect("publication");
        assert!(publication.operations.len() > 1);
        assert_eq!(
            publication
                .operations
                .iter()
                .map(|batch| batch.operations.len())
                .sum::<usize>(),
            states
        );
        let mut seen = std::collections::BTreeSet::new();
        for batch in &publication.operations {
            assert!(batch.operations.len() <= 128);
            assert!(batch.encoded_len() <= 256 * 1024);
            for record in &batch.operations {
                let operation = objects::object::thread_replication::ThreadOperation::decode(
                    &record.canonical_record,
                )
                .expect("operation");
                assert!(
                    operation.parents.iter().all(|parent| seen.contains(parent)),
                    "ancestor-first publication"
                );
                seen.insert(operation.id().expect("operation ID"));
            }
        }
        eprintln!(
            "{states} States accepted in {} bounded batches",
            publication.operations.len()
        );
    }
    if !capture_again {
        let clone = temp.path().join("clone");
        let error = hosted
            .clone_pull_with_depth_and_materialization(
                "acme/widgets",
                Some("main"),
                None,
                hosted_client::hosted_runtime::hosted::PullMaterialization::Full,
                |_| repo::Repository::init(&clone).map_err(wire::ProtocolError::from),
            )
            .await
            .err()
            .expect("fresh set/public proof routing belongs to Part 2");
        assert!(
            matches!(error, wire::ProtocolError::InvalidState(reason) if reason == "source preparation: pin hosted executor for native Thread")
        );
        let staged = repo::Repository::open(&clone).expect("staged repository");
        let replica = repo::thread_replication::ThreadReplica::open(
            staged.heddle_dir(),
            objects::object::ContentHash::from_bytes(thread_id),
        )
        .expect("signature-only genesis");
        assert!(
            replica
                .source_operation_page(tip.id(), None, 1)
                .expect("no admitted source")
                .is_empty()
        );
    }
    if capture_again {
        use objects::object::{Blob, State, Tree, TreeEntry};
        let blob = Blob::new(b"later capture\n".to_vec());
        adopted.store().put_blob(&blob).expect("later blob");
        let tree = Tree::from_entries(vec![
            TreeEntry::file("story.txt", blob.hash(), false).expect("later entry"),
        ]);
        adopted.store().put_tree(&tree).expect("later tree");
        let later = State::new_snapshot(tree.hash(), vec![tip.id()], tip.attribution.clone())
            .with_intent("later capture");
        adopted.store().put_state(&later).expect("later State");
        adopted
            .record_native_capture("main", later.id())
            .expect("capture later State");
        let pushed = hosted
            .push_profiled(
                &adopted,
                "acme/widgets",
                later.id(),
                "main",
                false,
                "later-capture".into(),
            )
            .await
            .expect("later push")
            .0;
        assert!(pushed.success);
        assert_eq!(pushed.new_state, Some(later.id()));
        let publication = captured.lock().expect("later publication");
        assert_eq!(publication.published.len(), 2);
        assert_eq!(
            publication
                .operations
                .iter()
                .map(|batch| batch.operations.len())
                .sum::<usize>(),
            states + 1
        );
        eprintln!(
            "later single-capture push accepted with {} operations",
            states + 1
        );
    }
    hosted.close().await;
    server.await.expect("server");
}
