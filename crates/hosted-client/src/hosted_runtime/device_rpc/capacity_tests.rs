//! Retained authenticated views cannot consume command admission indefinitely.
use std::{sync::Arc, time::Duration};

use crypto::{Ed25519Signer, Signer};
use objects::object::thread_replication::ThreadGenesis;
use thread_api::{Remote, credentials::Credentials, transport::IrohTransport};
use tokio::sync::Semaphore;

use super::*;

pub(super) async fn roundtrip(
    remote: &Remote<IrohTransport<Credentials>>,
    device: &DeviceRpc,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    budgets: &(Arc<Semaphore>, Arc<Semaphore>),
) {
    let genesis = replica.genesis().expect("genesis");
    let spool = SpoolRef {
        id: genesis.spool.clone(),
    };
    let reference = ThreadRef {
        spool: Some(spool.clone()),
        id: Some(ThreadId {
            value: replica.thread_id().as_bytes().to_vec(),
        }),
    };
    for round in 0..2 {
        drained(device, budgets).await;
        let mut views = Vec::new();
        for index in 0..40 {
            let mut view = remote
                .observe::<thread_api::rpc::ThreadServiceObserveThread>(
                    ObserveThreadRequest {
                        thread: Some(reference.clone()),
                        sections: vec![ThreadSection::Overview as i32],
                        observe: Some(ObserveOptions {
                            mode: ObservationMode::Follow as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap_or_else(|error| panic!("round {round} view {index} opens: {error}"));
            tokio::time::timeout(Duration::from_secs(5), view.next_commit())
                .await
                .expect("initial commit deadline")
                .unwrap_or_else(|error| {
                    panic!("round {round} view {index} snapshot protocol: {error}")
                })
                .expect("snapshot");
            views.push(view);
        }
        assert_eq!(
            budgets.0.available_permits(),
            32,
            "authenticated views release admission slots"
        );
        assert_eq!(
            budgets.1.available_permits(),
            2048 - 40,
            "one retained slot per live view"
        );
        assert_eq!(
            device
                .feeds
                .lock()
                .expect("feeds")
                .values()
                .filter(|feed| feed.strong_count() > 0)
                .count(),
            1,
            "forty same-Spool views share one filesystem feed"
        );
        let signer = Ed25519Signer::from_seed(&[71; 32]).expect("owner signer");
        let authority = repo::device_authority::load(&device.home, chrono::Utc::now().timestamp())
            .expect("authority");
        let token =
            crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32]).expect("root token");
        let key = biscuit_verifier::PublicKey::from_bytes(
            signer.public_key(),
            biscuit_auth::Algorithm::Ed25519,
        )
        .expect("root key");
        let parsed = biscuit_verifier::parse_token(&token.token, &[key]).expect("token");
        let proof = repo::thread_replication::metadata::prepare_control_authority(
            &authority,
            &signer.public_key().try_into().expect("key"),
            &parsed,
            chrono::Utc::now().timestamp(),
        )
        .expect("proof");
        let created = ThreadGenesis {
            owner: objects::object::thread_replication::GenesisOwner::Account(
                uuid::Uuid::from_bytes([9; 16]),
            ),
            version: 1,
            spool: spool.id.clone(),
            parent: None,
            base: repository.head().expect("head").expect("base"),
            name: format!("live-capacity-{round}"),
            intent: "mutation while forty views are retained".into(),
            creator: signer.public_key().try_into().expect("key"),
            nonce: vec![110 + round; 32],
        };
        let snapshots = device
            .thread_snapshots
            .load(std::sync::atomic::Ordering::Relaxed);
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            remote
                .api
                .call::<thread_api::rpc::ThreadServiceStartThread>(&StartThreadRequest {
                    creator_authority: proof.clone(),
                    client_operation_id: uuid::Uuid::now_v7().to_string(),
                    spool: Some(spool.clone()),
                    thread_genesis: Some(
                        thread_api::replication::opening::sign_genesis(&created, &signer)
                            .expect("signed genesis"),
                    ),
                }),
        )
        .await
        .expect("mutation deadline with forty live views")
        .expect("mutation admitted");
        assert_eq!(
            response.thread.expect("created overview").name,
            created.name
        );
        assert_eq!(
            repo::thread_replication::ThreadReplica::open(
                repository.heddle_dir(),
                created.id().expect("ID")
            )
            .expect("durable creation")
            .genesis()
            .expect("persisted genesis"),
            created
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            device
                .thread_snapshots
                .load(std::sync::atomic::Ordering::Relaxed),
            snapshots,
            "unrelated Thread creation does not rebuild forty overview snapshots"
        );
        drop(views);
        drained(device, budgets).await;
    }
    attachment_wakes(remote, device, repository, &reference, budgets).await;
}

async fn attachment_wakes(
    remote: &Remote<IrohTransport<Credentials>>,
    device: &DeviceRpc,
    repository: &repo::Repository,
    reference: &ThreadRef,
    budgets: &(Arc<Semaphore>, Arc<Semaphore>),
) {
    use objects::store::ObjectStore as _;
    let mut view = remote
        .observe::<thread_api::rpc::ThreadServiceObserveThread>(
            ObserveThreadRequest {
                thread: Some(reference.clone()),
                sections: vec![ThreadSection::Analysis as i32],
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Follow as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("attachment view");
    view.next_commit()
        .await
        .expect("initial attachment commit")
        .expect("snapshot");
    let before = device
        .thread_snapshots
        .load(std::sync::atomic::Ordering::Relaxed);
    let context = objects::object::Tree::new();
    let root = repository
        .store()
        .put_tree(&context)
        .expect("empty context tree");
    repository
        .store()
        .put_state_attachment(&objects::object::StateAttachment {
            state_id: repository.head().expect("head").expect("state"),
            body: objects::object::StateAttachmentBody::Context(root),
            attribution: objects::object::Attribution::human(objects::object::Principal::new(
                "Owner", "",
            )),
            created_at: chrono::Utc::now(),
            supersedes: None,
        })
        .expect("attachment-only publication");
    tokio::time::timeout(Duration::from_secs(5), async {
        while device
            .thread_snapshots
            .load(std::sync::atomic::Ordering::Relaxed)
            == before
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("committed attachment index must wake the selected content view");
    drop(view);
    drained(device, budgets).await;
}

async fn drained(device: &DeviceRpc, budgets: &(Arc<Semaphore>, Arc<Semaphore>)) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let active_feeds = device
                .feeds
                .lock()
                .expect("feeds")
                .values()
                .any(|feed| feed.strong_count() > 0);
            if budgets.0.available_permits() == 32
                && budgets.1.available_permits() == 2048
                && !active_feeds
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all request/stream permits and filesystem feeds released after cancellation");
}
