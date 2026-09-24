//! A browser with an independently certified mint root reads its owned device offline.
use std::sync::Arc;

use crypto::{Ed25519Signer, Signer};
use sha2::{Digest, Sha256};

use super::*;
use crate::hosted_runtime::{hosted::claim_protocol::NATIVE_ALPN, root_mint::mint_agent_root};

pub(super) async fn roundtrip(
    home: &std::path::Path,
    browser: &iroh::Endpoint,
    address: iroh::EndpointAddr,
    endpoint: [u8; 32],
    owner: &OwnerState,
    repository: &repo::Repository,
    spool: uuid::Uuid,
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
            .connect(address.clone(), NATIVE_ALPN)
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
    let listed = remote
        .api
        .call::<thread_api::rpc::SpoolServiceListSpools>(&ListSpoolsRequest { repos_only: false })
        .await
        .expect("sibling ListSpools");
    let expected_id = spool.to_string();
    assert!(
        listed.spools.iter().any(|row| {
            row.r#ref
                .as_ref()
                .is_some_and(|reference| reference.id == expected_id)
        }),
        "grant-reachable ListSpools must include the owned local Spool: {listed:?}"
    );
    let passkey = Ed25519Signer::from_seed(&[74; 32]).expect("passkey");
    let temporary = Ed25519Signer::from_seed(&[75; 32]).expect("temporary root");
    let verified = repo::verify_account_owner_observation(owner, now).expect("owner");
    let account = verified.signed_root().root.as_ref().expect("account");
    let mut spki = hex::decode("302a300506032b6570032100").expect("Ed25519 SPKI");
    spki.extend_from_slice(passkey.public_key());
    let passkey_authority = PasskeyAuthority {
        format_version: 1,
        account_uuid: account.account_uuid.clone(),
        owner_state_hash: verified.state_hash().to_vec(),
        owner_sequence: verified.sequence(),
        owner_key: Some(verified.authority_key().clone()),
        credential_id: vec![17; 32],
        cose_algorithm: -8,
        public_key_spki: spki,
        relying_party_id: "heddle.test".into(),
        allowed_origins: vec!["https://app.heddle.test".into()],
        max_session_ttl_seconds: 3601,
        nonce: vec![18; 32],
    };
    let certificate = SignedPasskeyAuthority {
        owner_signature: Some(
            repo::sign_canonical(
                &root,
                heddleco_capability_verifier::passkey_delegation::PASSKEY_AUTHORITY_DOMAIN,
                &heddleco_capability_verifier::passkey_delegation::canonical_passkey_authority(
                    &passkey_authority,
                )
                .expect("canonical passkey"),
            )
            .expect("owner passkey certificate"),
        ),
        authority: Some(passkey_authority),
    };
    let grant = PasskeyMintGrant {
        format_version: 1,
        relying_party_id: "heddle.test".into(),
        mint_root_key: Some(
            repo::ed25519_verification_key(temporary.public_key()).expect("mint key"),
        ),
        not_before_unix_seconds: now - 1,
        expires_at_unix_seconds: now + 3600,
        nonce: vec![19; 32],
    };
    let challenge =
        api::passkey_mint_grant::passkey_mint_grant_signing_digest(&grant).expect("challenge");
    let client_data_json = serde_json::to_vec(&serde_json::json!({
        "type": "webauthn.get",
        "challenge": base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, challenge),
        "origin": "https://app.heddle.test",
        "crossOrigin": false,
    })).expect("client data");
    let mut authenticator_data = Sha256::digest(b"heddle.test").to_vec();
    authenticator_data.push(0x05);
    authenticator_data.extend_from_slice(&0_u32.to_be_bytes());
    let mut assertion = authenticator_data.clone();
    assertion.extend_from_slice(&Sha256::digest(&client_data_json));
    let temporary_attachment = SignedMintRootAttachment {
        grant: Some(grant),
        passkey_delegation: Some(PasskeyMintDelegation {
            authority: Some(certificate),
            client_data_json,
            authenticator_data,
            signature: passkey.sign(&assertion).expect("passkey assertion"),
        }),
    };
    let encoded = mint_agent_root(&[75; 32]).expect("temporary token").token;
    let new_spool = uuid::Uuid::now_v7();
    let creation = repo::sign_delegated_spool_creation(
        &temporary,
        repo::SpoolCreationIntent {
            spool_uuid: new_spool,
            parent_spool_uuid: None,
            parent_path_segments: vec![],
            name: "temporary-browser".into(),
        },
        owner,
        &encoded,
        Some(
            spool_creation_proof::MintRootAssociation::PasskeyMintRootAttachment(
                temporary_attachment.clone(),
            ),
        ),
        now,
    )
    .expect("passkey-authorized creation proof");
    repo::verify_spool_owner_genesis(&creation)
        .expect("passkey creation remains portable structural evidence");
    let thread_genesis = objects::object::thread_replication::ThreadGenesis {
        owner: objects::object::thread_replication::GenesisOwner::Account(uuid::Uuid::from_bytes(
            [9; 16],
        )),
        version: 1,
        spool: spool.to_string(),
        parent: None,
        base: repository.head().expect("head").expect("base"),
        name: "temporary-browser-thread".into(),
        intent: "passkey authoring".into(),
        creator: temporary.public_key().try_into().expect("temporary key"),
        nonce: vec![76; 32],
    };
    let signed_thread = thread_api::replication::opening::sign_genesis(&thread_genesis, &temporary)
        .expect("temporary Thread genesis");
    let public = biscuit_auth::PublicKey::from_bytes(
        temporary.public_key(),
        biscuit_auth::Algorithm::Ed25519,
    )
    .expect("temporary public");
    let token = biscuit_auth::Biscuit::from_base64(encoded, public)
        .expect("temporary token")
        .seal()
        .expect("sealed temporary token");
    let temporary_proof = heddleco_capability_verifier::thread_control_authority::encode(
        &OwnerHistory {
            root: owner.root.clone(),
            accepted_transitions: owner.accepted_transitions.clone(),
            state_hash: owner.version.clone(),
        },
        temporary.public_key(),
        Some(
            thread_control_authority::MintRootAssociation::PasskeyMintRootAttachment(
                temporary_attachment.clone(),
            ),
        ),
        &token,
    )
    .expect("temporary portable authority");
    let credential =
        thread_api::credentials::Credentials::owned_device(Arc::new(temporary), &temporary_proof)
            .expect("temporary credential");
    let transport = thread_api::transport::IrohTransport::new(
        browser
            .connect(address.clone(), NATIVE_ALPN)
            .await
            .expect("temporary Iroh"),
        credential,
        256 * 1024,
        std::time::Duration::from_secs(10),
    )
    .expect("temporary transport");
    let temporary_remote = thread_api::Remote::discover(transport, endpoint, EndpointKind::Device)
        .await
        .expect("temporary remote");
    let mut identity = temporary_remote
        .observe::<thread_api::rpc::IdentityServiceObserveIdentity>(
            ObserveIdentityRequest {
                include_current_credential: true,
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("temporary identity view");
    identity
        .next_commit()
        .await
        .expect("temporary identity snapshot")
        .expect("snapshot");
    let created = temporary_remote
        .api
        .call::<thread_api::rpc::SpoolServiceCreateSpool>(&CreateSpoolRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            slug: "temporary-browser".into(),
            ownership: Some(create_spool_request::Ownership::OwnerGenesis(creation)),
            ..Default::default()
        })
        .await
        .expect("fresh passkey root creates Spool on owned device");
    assert_eq!(
        created.spool.expect("new Spool").r#ref.expect("new ref").id,
        new_spool.to_string()
    );
    temporary_remote
        .api
        .call::<thread_api::rpc::ThreadServiceStartThread>(&StartThreadRequest {
            creator_authority: temporary_proof,
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            spool: Some(SpoolRef {
                id: spool.to_string(),
            }),
            thread_genesis: Some(signed_thread),
        })
        .await
        .expect("fresh passkey root authors Thread on owned device");
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
