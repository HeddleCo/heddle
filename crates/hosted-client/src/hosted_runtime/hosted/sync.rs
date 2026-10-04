use std::time::Duration;

use objects::object::StateId;
use repo::{Repository, ThreadManager};
use wire::{RefEntry, RefKind};

#[derive(Debug, Clone)]
pub struct PullBootstrapRefs {
    pub head_thread: Option<String>,
    pub refs: Vec<HostedRefEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct PullObjectMix {
    pub blobs: usize,
    pub trees: usize,
    pub states: usize,
    pub actions: usize,
    pub annotated_tags: usize,
    pub redactions: usize,
    pub purges: usize,
    pub state_visibilities: usize,
    pub state_attachments: usize,
    pub key_bindings: usize,
}

impl PullObjectMix {
    pub fn total(&self) -> usize {
        self.blobs
            + self.trees
            + self.states
            + self.actions
            + self.annotated_tags
            + self.redactions
            + self.purges
            + self.state_visibilities
            + self.state_attachments
            + self.key_bindings
    }
}

#[derive(Debug, Clone)]
pub struct HostedRefEntry {
    pub name: String,
    pub state_id: StateId,
    pub kind: RefKind,
    pub revision_address: String,
    pub thread_id: Option<String>,
}

impl HostedRefEntry {
    pub fn from_advertised(
        name: String,
        state_id: StateId,
        advertised_as_thread: bool,
        revision_address: String,
        thread_id: Option<String>,
    ) -> Self {
        let kind = RefKind::from_advertised_name(&name, advertised_as_thread);
        let thread_id = if kind.is_user_thread() {
            thread_id
        } else {
            None
        };
        Self {
            name,
            state_id,
            kind,
            revision_address,
            thread_id,
        }
    }

    pub fn is_user_thread(&self) -> bool {
        self.kind.is_user_thread()
    }

    pub fn is_marker(&self) -> bool {
        self.kind.is_marker()
    }

    pub fn to_wire_entry(&self) -> RefEntry {
        RefEntry {
            name: self.name.clone(),
            state_id: self.state_id,
            kind: self.kind,
        }
    }
}

/// Advertised ListRefs `thread_id` for a user thread, if present and non-empty.
pub fn advertised_user_thread_id<'a>(
    remote_refs: &'a [HostedRefEntry],
    track_name: &str,
) -> Option<&'a str> {
    remote_refs
        .iter()
        .find(|entry| entry.is_user_thread() && entry.name == track_name)
        .and_then(|entry| entry.thread_id.as_deref())
        .filter(|id| !id.is_empty())
}

/// Persist the ListRefs-advertised stable id as the local thread identity.
///
/// Clone uses this when GetThread is missing; tests drive the same helper so
/// a later push cannot mint a different UUIDv7.
pub fn persist_advertised_thread_identity(
    repo: &Repository,
    remote_refs: &[HostedRefEntry],
    track_name: &str,
    final_state: &StateId,
) -> objects::error::Result<()> {
    let Some(stable_id) = advertised_user_thread_id(remote_refs, track_name) else {
        return Ok(());
    };
    ThreadManager::new(repo.heddle_dir()).adopt_stable_identity_for_thread(
        repo,
        track_name,
        stable_id,
        *final_state,
    )?;
    Ok(())
}

/// Persist from advertised pull refs when they carry an id; otherwise
/// from a live ListRefs advertisement. Empty `thread_id` stays absent.
pub fn persist_advertised_thread_identity_with_live_fallback(
    repo: &Repository,
    advertised_refs: &[HostedRefEntry],
    live_refs: Option<&[HostedRefEntry]>,
    track_name: &str,
    final_state: &StateId,
) -> objects::error::Result<()> {
    if advertised_user_thread_id(advertised_refs, track_name).is_some() {
        return persist_advertised_thread_identity(repo, advertised_refs, track_name, final_state);
    }
    persist_advertised_thread_identity(
        repo,
        live_refs.unwrap_or(advertised_refs),
        track_name,
        final_state,
    )
}

#[derive(Debug, Clone, Default)]
pub struct PullProfile {
    pub ready_wait: Duration,
    pub receive_and_apply: Duration,
    pub decode: Duration,
    pub store_receive_object: Duration,
    pub metadata_sync: Duration,
    pub pack_decode_apply: Duration,
    pub raw_decode_apply: Duration,
    pub pack_decode: Duration,
    pub raw_decode: Duration,
    pub bytes_received: usize,
    pub pack_bytes_received: usize,
    pub raw_bytes_received: usize,
    pub objects_received: usize,
    pub object_mix: PullObjectMix,
}

#[derive(Debug, Clone, Default)]
pub struct PushProfile {
    pub remote_ref_discovery: Duration,
    pub closure_planning: Duration,
    pub request_setup: Duration,
    pub ready_wait: Duration,
    pub pack_and_send: Duration,
    pub sidecars_and_git_send: Duration,
    pub request_finish: Duration,
    pub complete_wait: Duration,
    pub metadata_sync: Duration,
    pub total: Duration,
    pub objects_advertised: usize,
    pub objects_wanted: usize,
    pub pack_objects: usize,
    pub sidecar_objects: usize,
    pub reused_pack: usize,
    pub reused_pack_copy: Duration,
}

/// Compare-and-set precondition for a native push target.
///
/// Callers that retain the head from a successful push or pull can provide it
/// here and avoid a separate `ListRefs` request before every push.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ExpectedRemoteHead {
    Missing,
    State(StateId),
}

#[cfg(test)]
mod native_exchange_tests {
    use objects::object::{Attribution, Principal};
    use repo::{ObjectStore, ThreadManager};
    use tempfile::TempDir;
    use wire::ProtocolError;

    use super::{super::PullMaterialization, *};

    const REMOTE_THREAD: &str = "main";

    fn repository(temp: &TempDir) -> (Repository, StateId) {
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "content\n").unwrap();
        let state = repo
            .snapshot_with_attribution(
                Some("native exchange fixture".to_string()),
                None,
                Attribution::human(Principal::new("Test", "test@example.com")),
            )
            .unwrap()
            .id();
        (repo, state)
    }

    async fn native_server(
        repo: &Repository,
    ) -> (
        crate::hosted_runtime::hosted::HostedClient,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<
            std::sync::Mutex<
                crate::hosted_runtime::hosted::native_exchange_test_server::PublicationCapture,
            >,
        >,
    ) {
        let thread = repo.native_thread(REMOTE_THREAD).unwrap();
        let spool = uuid::Uuid::parse_str(&thread.genesis().unwrap().spool).unwrap();
        crate::hosted_runtime::hosted::native_exchange_test_server::start(
            spool,
            REMOTE_THREAD,
            *thread.thread_id().as_bytes(),
        )
        .await
    }

    async fn publish(
        client: &mut crate::hosted_runtime::hosted::HostedClient,
        repo: &Repository,
        state: StateId,
    ) {
        let pushed = client
            .push_with_expected_head_profiled(
                repo,
                "acme/widgets",
                state,
                REMOTE_THREAD,
                false,
                ExpectedRemoteHead::Missing,
                format!("publish-{state}"),
            )
            .await
            .unwrap()
            .0;
        assert!(
            pushed.success,
            "native fixture rejected publication: {pushed:?}"
        );
        assert_eq!(pushed.new_state, Some(state));
    }

    fn preserved_local_originals(source: &Repository, destination: &Repository, state: StateId) {
        let original = source
            .native_thread(REMOTE_THREAD)
            .expect("original Thread");
        let replica = repo::thread_replication::ThreadReplica::open(
            destination.heddle_dir(),
            original.thread_id(),
        )
        .expect("admitted original Thread");
        assert_eq!(
            replica.signed_genesis().expect("received original"),
            original.signed_genesis().expect("signed source original")
        );
        let received = replica
            .accepted_source_originals_for_revisions(&[state])
            .expect("admitted source originals");
        assert!(!received.is_empty());
        assert_eq!(
            received,
            original
                .accepted_source_originals_for_revisions(&[state])
                .expect("published source originals")
        );
        for (_, signed) in &received {
            replica
                .verify_local_source_owner(&signed.verify().expect("original signature"))
                .expect("original local ownership");
        }
        assert!(
            destination
                .store()
                .has_state(&state)
                .expect("installed State")
        );
        let database = repo::local_metadata::open(destination.heddle_dir()).expect("metadata");
        let executor_pins: i64 = database
            .query_row("SELECT count(*) FROM hosted_executor_pins", [], |row| {
                row.get(0)
            })
            .expect("executor pins");
        assert_eq!(
            executor_pins, 0,
            "transport cannot enroll executor authority"
        );
    }

    #[test]
    fn persist_advertised_identity_adopts_api_thread_id() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let hosted_id = "019f0000-aaaa-7bbb-8ccc-ddddeeeeffff";
        let minted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("init persists main");
        assert_ne!(minted.id, hosted_id);

        let advertised = [HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            Some(hosted_id.to_string()),
        )];
        assert_eq!(advertised[0].thread_id.as_deref(), Some(hosted_id));

        persist_advertised_thread_identity_with_live_fallback(
            &repo,
            &advertised,
            None,
            "main",
            &state,
        )
        .expect("advertised RefEntry thread_id must be adopted without a second ListRefs");

        let persisted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("clone must persist a main record");
        assert_eq!(persisted.id, hosted_id);
        assert_ne!(persisted.id, minted.id);
    }

    #[test]
    fn persist_advertised_identity_uses_live_refs_when_folded_omit_thread_id() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let hosted_id = "019f0000-aaaa-7bbb-8ccc-ddddeeeeffff";
        let minted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("init persists main");
        assert_ne!(minted.id, hosted_id);

        let folded = [HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            None,
        )];
        let live = [HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            Some(hosted_id.to_string()),
        )];

        persist_advertised_thread_identity_with_live_fallback(
            &repo,
            &folded,
            Some(&live),
            "main",
            &state,
        )
        .expect("live ListRefs must supply the omitted folded thread_id");

        let persisted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("clone must persist a main record");
        assert_eq!(persisted.id, hosted_id);
        assert_ne!(persisted.id, minted.id);
    }

    #[tokio::test]
    async fn native_push_without_local_stable_identity_fails_before_sending_request() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);

        let error = client
            .push_with_expected_head_profiled(
                &repo,
                "acme/widgets",
                state,
                "missing",
                false,
                ExpectedRemoteHead::Missing,
                "missing-local-metadata-test-op".to_string(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid state: Thread \"missing\" has no native identity"
        );
        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_push_and_clone_preserve_original_local_authority() {
        let _process_env_guard = crate::test_process_env::shared().await;
        crate::on_large_stack(native_push_and_clone_pull);
    }

    async fn native_push_and_clone_pull() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let (mut client, server, captured) = native_server(&repo).await;

        let refs = client.list_refs("acme/widgets").await.unwrap();
        assert!(refs.is_empty());
        publish(&mut client, &repo, state).await;
        let refs = client.list_refs("acme/widgets").await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, REMOTE_THREAD);
        let accepted = captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        assert_eq!(
            accepted.revision.and_then(|revision| revision.revision),
            Some(api::heddle::api::v1alpha2::revision_ref::Revision::State(
                api::heddle::api::common::StateId {
                    value: state.as_bytes().to_vec(),
                },
            ))
        );

        let clone = TempDir::new().unwrap();
        let (complete, cloned) = client
            .clone_pull_with_depth_and_materialization(
                "acme/widgets",
                Some("main"),
                None,
                PullMaterialization::Full,
                |_| Repository::init(clone.path()).map_err(ProtocolError::from),
            )
            .await
            .expect("local-authority clone");
        assert!(complete.success);
        assert_eq!(complete.final_state, Some(state));
        preserved_local_originals(&repo, &cloned, state);

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unsupported_hosted_clone_modes_fail_before_local_initialization() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let clone = TempDir::new().unwrap();
        let initialized = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let initialized_in_callback = std::sync::Arc::clone(&initialized);
        let error = client
            .clone_pull_with_depth_and_materialization(
                "acme/widgets",
                Some("main"),
                Some(1),
                PullMaterialization::Lazy,
                |_| {
                    initialized_in_callback.store(true, std::sync::atomic::Ordering::SeqCst);
                    Repository::init(clone.path()).map_err(ProtocolError::from)
                },
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("unsupported hosted clone modes were accepted"));
        assert!(error.to_string().contains("--depth"));
        assert!(error.to_string().contains("--lazy"));
        assert!(
            !initialized.load(std::sync::atomic::Ordering::SeqCst),
            "unsupported hosted clone modes must fail before local initialization"
        );

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn compact_local_pull_preserves_original_authority_and_existing_closure() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let before: Vec<_> = wire::enumerate_state_closure(repo.store(), state)
            .unwrap()
            .into_iter()
            .map(|object| (object.id, object.obj_type, object.size, object.delta_base))
            .collect();
        let (mut client, server, _) = native_server(&repo).await;
        publish(&mut client, &repo, state).await;

        let fetched = client
            .fetch_state(&repo, "acme/widgets", REMOTE_THREAD, state)
            .await
            .expect("original local authority permits fetch");
        assert_eq!(fetched, 1);
        preserved_local_originals(&repo, &repo, state);
        assert!(repo.store().has_state(&state).unwrap());
        assert_eq!(
            wire::enumerate_state_closure(repo.store(), state)
                .unwrap()
                .into_iter()
                .map(|object| (object.id, object.obj_type, object.size, object.delta_base))
                .collect::<Vec<_>>(),
            before,
            "fetching a complete state must not synthesize source objects"
        );

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn supported_local_pull_modes_preserve_original_authority() {
        let _process_env_guard = crate::test_process_env::shared().await;
        crate::on_large_stack(complete_local_state_pull_modes);
    }

    async fn complete_local_state_pull_modes() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let (mut client, server, _) = native_server(&repo).await;
        publish(&mut client, &repo, state).await;
        let bootstrap = REMOTE_THREAD;

        for _ in 0..2 {
            let (complete, _) = client
                .pull_profiled(&repo, "acme/widgets", bootstrap, None)
                .await
                .expect("local-authority replay");
            assert!(complete.success);
            assert_eq!(complete.final_state, Some(state));
            preserved_local_originals(&repo, &repo, state);
        }
        let depth_error = client
            .pull_with_depth(&repo, "acme/widgets", bootstrap, None, Some(1))
            .await
            .unwrap_err();
        assert!(depth_error.to_string().contains("--depth"));
        let lazy_error = client
            .pull_with_depth_and_materialization(
                &repo,
                "acme/widgets",
                bootstrap,
                None,
                None,
                PullMaterialization::Lazy,
            )
            .await
            .unwrap_err();
        assert!(lazy_error.to_string().contains("--lazy"));
        let repaired = client
            .repair_clone_with_depth_and_materialization(
                &repo,
                "acme/widgets",
                bootstrap,
                None,
                PullMaterialization::Full,
            )
            .await
            .expect("local-authority repair");
        assert!(repaired.success);
        assert_eq!(repaired.final_state, Some(state));
        let hydrated = client
            .hydrate_missing_blobs_for_state(&repo, "acme/widgets", bootstrap, state)
            .await
            .expect("local-authority hydration");
        assert_eq!(hydrated, 1);
        preserved_local_originals(&repo, &repo, state);

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn clone_pull_installs_exact_locally_authorized_originals() {
        let _process_env_guard = crate::test_process_env::shared().await;
        crate::on_large_stack(clone_pull_complete_native_pack);
    }

    async fn clone_pull_complete_native_pack() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let (mut client, server, _) = native_server(&repo).await;
        publish(&mut client, &repo, state).await;
        let clone = TempDir::new().unwrap();

        let (complete, cloned) = client
            .clone_pull_with_depth_and_materialization(
                "acme/widgets",
                Some("main"),
                None,
                PullMaterialization::Full,
                |_| Repository::init(clone.path()).map_err(ProtocolError::from),
            )
            .await
            .expect("original local authority permits clone");
        assert!(complete.success);
        assert_eq!(complete.final_state, Some(state));
        preserved_local_originals(&repo, &cloned, state);

        client.close().await;
        server.await.unwrap();
    }
}
