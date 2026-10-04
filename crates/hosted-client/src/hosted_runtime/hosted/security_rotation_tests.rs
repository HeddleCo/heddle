//! Exercise the actual trust replacement command with a retained connected client.
use std::sync::Arc;

use crypto::{Ed25519Signer, Signer};
use repo::thread_replication::hosted_trust::{HostedTrust, SystemClock};
use thread_api::replication::store::{ReceivedOperation, ReplicaStore};

use super::*;
use crate::hosted_runtime::device_rpc::hybrid_security_tests::{Fixture, sign_set};
#[tokio::test]
async fn security_f1_replacement_command_preserves_checkpoint_and_invalidates_retained_clients() {
    let _guard = crate::test_process_env::exclusive().await;
    let f = Fixture::new();
    let (mut client, server) = test_server::start().await;
    client.hosted_root = Some(descriptor_trust::HostedRootSelection {
        authority: f.root.authority.clone(),
        root_id: f.root.root_id.clone(),
        public_key: f.root.public_key,
        automatic_store: Some(descriptor_trust::descriptor_trust_path()),
    });
    assert!(client.hosted_root().is_some());
    let device = f.device();
    let retained = device
        .hosted_backend(f.local(), Arc::new(f.session().await), f.bundle.clone())
        .expect("retained device relay");
    let trust = HostedTrust::open(f.repository.heddle_dir(), &f.root.authority, SystemClock)
        .expect("trust");
    let before = trust.snapshot().expect("A checkpoint");
    let new = Ed25519Signer::from_seed(&[8; 32])
        .expect("B root")
        .public_key()
        .to_vec();
    let command = |repository| crate::hosted_runtime::auth_requests::AuthCommand::Trust {
        command: crate::hosted_runtime::auth_requests::AuthTrustCommand::Replace {
            repository,
            server: f.root.authority.clone(),
            expected_current_public_key: hex::encode(f.root.public_key),
            key_id: "descriptor-root-B".into(),
            public_key: hex::encode(&new),
        },
    };
    crate::hosted_runtime::auth::execute(Default::default(), command(None), |_| Ok(()))
        .await
        .expect("actual replacement command");
    assert!(
        client.hosted_root().is_none(),
        "retained automatic client must invalidate its A selection"
    );
    let pending = f.snapshot();
    assert!(
        retained
            .receive(ReceivedOperation {
                native_authority: None,
                original: f.original.clone(),
                authority_admission: None,
                import_authority: Some(Arc::new(f.bundle.clone()))
            })
            .await
            .is_err(),
        "old device selections fail closed while the scoped replacement is pending"
    );
    assert_eq!(pending, f.snapshot());
    crate::hosted_runtime::auth::execute(
        Default::default(),
        command(Some(f.repository.root().to_path_buf())),
        |_| Ok(()),
    )
    .await
    .expect("actual scoped repository replacement command after the deployment pin changed");
    let after = trust.snapshot().expect("replacement checkpoint");
    assert_eq!(after.root.public_key.as_slice(), new);
    assert_eq!(after.root_epoch, before.root_epoch + 1);
    assert_eq!(
        after.previous.as_ref().expect("retained A history").body(),
        before.previous.as_ref().expect("old history").body()
    );
    assert_eq!(after.clock_floor_millis, before.clock_floor_millis);
    assert_eq!(after.known_job_associations, before.known_job_associations);
    let before_reject = f.snapshot();
    assert!(
        retained
            .receive(ReceivedOperation {
                native_authority: None,
                original: f.original.clone(),
                authority_admission: None,
                import_authority: Some(Arc::new(f.bundle.clone()))
            })
            .await
            .is_err(),
        "staged A context must fail after explicit replacement"
    );
    assert_eq!(f.snapshot(), before_reject);
    let mut bundle = f.bundle.clone();
    let set = bundle.witness_set.as_mut().expect("set");
    let body = set.body.as_mut().expect("body");
    body.descriptor_root_id = "descriptor-root-B".into();
    body.generation += 1;
    sign_set(set, 8);
    let session = Arc::new(f.session().await);
    let backend = device
        .hosted_backend(f.local(), session, bundle.clone())
        .expect("fresh B continuation");
    backend
        .receive(ReceivedOperation {
            native_authority: None,
            original: f.original.clone(),
            authority_admission: None,
            import_authority: Some(Arc::new(bundle.clone())),
        })
        .await
        .expect("B preserves A history");
    let baseline = f.snapshot();
    let mut changed = bundle.clone();
    let set = changed.witness_set.as_mut().expect("set");
    let body = set.body.as_mut().expect("body");
    body.generation += 1;
    body.entries
        .iter_mut()
        .find(|e| e.state == 2)
        .expect("retirement seal")
        .archive_root[0] ^= 1;
    sign_set(set, 8);
    let backend = device
        .hosted_backend(f.local(), Arc::new(f.session().await), changed.clone())
        .expect("prepare changed seal");
    assert!(
        backend
            .receive(ReceivedOperation {
                native_authority: None,
                original: f.original.clone(),
                authority_admission: None,
                import_authority: Some(Arc::new(changed))
            })
            .await
            .is_err(),
        "replacement does not reset the prior retirement seal"
    );
    assert_eq!(f.snapshot(), baseline);
    let mut resurrected = bundle.clone();
    let set = resurrected.witness_set.as_mut().expect("set");
    let body = set.body.as_mut().expect("body");
    body.generation += 1;
    body.entries
        .iter_mut()
        .find(|e| e.state == 3)
        .expect("tombstone")
        .revoked_at_unix_millis += 1;
    sign_set(set, 8);
    let backend = device
        .hosted_backend(f.local(), Arc::new(f.session().await), resurrected.clone())
        .expect("prepare changed tombstone");
    assert!(
        backend
            .receive(ReceivedOperation {
                native_authority: None,
                original: f.original.clone(),
                authority_admission: None,
                import_authority: Some(Arc::new(resurrected))
            })
            .await
            .is_err(),
        "root replacement preserves revoked issuer tombstones"
    );
    assert_eq!(f.snapshot(), baseline);
    server.abort();
}
