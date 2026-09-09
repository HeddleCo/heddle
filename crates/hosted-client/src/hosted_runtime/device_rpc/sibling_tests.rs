//! A browser with an independently certified mint root reads its owned device offline.
use std::sync::Arc;

use crypto::{Ed25519Signer, Signer};

use super::*;
use crate::hosted_runtime::{hosted::claim_protocol::NATIVE_ALPN, root_mint::mint_agent_root};

pub(super) async fn roundtrip(
    home: &std::path::Path,
    browser: &iroh::Endpoint,
    address: iroh::EndpointAddr,
    endpoint: [u8; 32],
    owner: &OwnerState,
) {
    let now = chrono::Utc::now().timestamp();
    let root = Ed25519Signer::from_seed(&[71; 32]).expect("owner");
    let sibling = Ed25519Signer::from_seed(&[73; 32]).expect("sibling");
    let certificate = repo::sign_mint_root_attachment(
        &root,
        owner,
        sibling.public_key(),
        now - 1,
        now + 3600,
        [11; 32],
    )
    .expect("sibling owner certificate");
    let sender = repo::device_authority::DeviceAuthority {
        owner: owner.clone(),
        mint_roots: vec![certificate],
        revoked_ids: vec![],
        revoked_mint_roots: vec![],
        revoked_publishers: vec![],
    };
    let encoded = mint_agent_root(&[73; 32]).expect("sibling token").token;
    let public =
        biscuit_auth::PublicKey::from_bytes(sibling.public_key(), biscuit_auth::Algorithm::Ed25519)
            .expect("public");
    let token = biscuit_auth::Biscuit::from_base64(encoded, public)
        .expect("signed token")
        .seal()
        .expect("sealed");
    let mint: [u8; 32] = sibling.public_key().try_into().expect("mint");
    let proof =
        repo::thread_replication::metadata::prepare_control_authority(&sender, &mint, &token, now)
            .expect("portable authority");
    let credential = thread_api::credentials::Credentials::owned_device(Arc::new(sibling), &proof)
        .expect("owned-device SDK credential");
    let transport = thread_api::transport::IrohTransport::new(
        browser
            .connect(address, NATIVE_ALPN)
            .await
            .expect("sibling Iroh"),
        credential,
        256 * 1024,
        std::time::Duration::from_secs(10),
    )
    .expect("transport");
    let remote = thread_api::Remote::discover(transport, endpoint, EndpointKind::Device)
        .await
        .expect("discover sibling endpoint");
    let request = ObserveWorkspaceRequest {
        observe: Some(ObserveOptions {
            mode: ObservationMode::Once as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut view = remote
        .observe::<thread_api::rpc::WorkspaceServiceObserveWorkspace>(request.clone(), None)
        .await
        .expect("sibling view opens");
    view.next_commit()
        .await
        .expect("sibling authenticated snapshot")
        .expect("snapshot");
    let mut receiver = repo::device_authority::load(home, now).expect("current local authority");
    assert!(
        receiver.mint_roots.is_empty(),
        "incoming proof does not enroll sibling"
    );
    receiver.revoked_mint_roots.push(mint);
    repo::device_authority::publish(home, &receiver, now).expect("owner revokes sibling");
    let result = remote
        .observe::<thread_api::rpc::WorkspaceServiceObserveWorkspace>(request, None)
        .await;
    match result {
        Err(_) => {}
        Ok(mut view) => assert!(
            view.next_commit().await.is_err(),
            "current local revocation must deny sibling before any snapshot"
        ),
    }
}
