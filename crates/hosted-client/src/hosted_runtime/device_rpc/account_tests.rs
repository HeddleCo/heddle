//! The complete account/device path runs over the real Iroh transport, offline.
use crypto::Ed25519Signer;
use thread_api::{Remote, credentials::Credentials, transport::IrohTransport};

use super::*;
pub(super) async fn roundtrip(
    remote: &Remote<IrohTransport<Credentials>>,
    device: &DeviceRpc,
    spool: uuid::Uuid,
) {
    let options = Some(ObserveOptions {
        mode: ObservationMode::Once as i32,
        ..Default::default()
    });
    let mut identity = remote
        .observe::<thread_api::rpc::IdentityServiceObserveIdentity>(
            ObserveIdentityRequest {
                devices: Some(PageRequest::default()),
                include_current_credential: true,
                observe: options.clone(),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("local identity");
    let batch = identity
        .next_commit()
        .await
        .expect("identity protocol")
        .expect("identity snapshot");
    assert!(batch.changes.iter().any(|p|matches!(p,identity_event::Payload::CurrentCredential(c) if !c.thread_control_authority.is_empty())));
    let principal = uuid::Uuid::from_bytes([9; 16]).to_string();
    let token = crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32])
        .expect("local root credential")
        .token;
    let inspected = remote
        .api
        .call::<thread_api::rpc::IdentityServiceIntrospectCredential>(
            &IntrospectCredentialRequest {
                biscuit: token.into_bytes(),
            },
        )
        .await
        .expect("local introspection");
    assert!(!inspected.revoked);
    assert!(inspected.authority.is_some());
    let mut owner = remote
        .observe::<thread_api::rpc::OwnerAuthorizationServiceObserveOwnership>(
            ObserveOwnershipRequest {
                observe: options.clone(),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("local owner");
    let batch = owner
        .next_commit()
        .await
        .expect("owner protocol")
        .expect("owner snapshot");
    assert!(batch.changes.iter().any(|p|matches!(p,ownership_event::Payload::Owner(o) if o.owner.as_ref().is_some_and(|p|p.id==principal))));
    let mut workspace = remote
        .observe::<thread_api::rpc::WorkspaceServiceObserveWorkspace>(
            ObserveWorkspaceRequest {
                include_bookmarks: true,
                include_devices: true,
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Follow as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("workspace follow");
    let batch = workspace
        .next_commit()
        .await
        .expect("workspace protocol")
        .expect("workspace snapshot");
    let original = batch
        .changes
        .iter()
        .find_map(|p| {
            if let workspace_event::Payload::Spool(s) = p {
                (s.r#ref.as_ref().is_some_and(|r| r.id == spool.to_string())).then_some(s.clone())
            } else {
                None
            }
        })
        .expect("registered spool");
    let bookmark = SetBookmarkRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        bookmark: Some(BookmarkRef {
            account: Some(PrincipalRef {
                id: principal.clone(),
            }),
            target: Some(bookmark_ref::Target::Spool(SpoolRef {
                id: spool.to_string(),
            })),
        }),
        expected_version: vec![],
        bookmarked: true,
        label: "local favorite".into(),
    };
    let receipt = remote
        .api
        .call::<thread_api::rpc::WorkspaceServiceSetBookmark>(&bookmark)
        .await
        .expect("bookmark");
    assert_eq!(
        receipt,
        remote
            .api
            .call::<thread_api::rpc::WorkspaceServiceSetBookmark>(&bookmark)
            .await
            .expect("exact bookmark retry")
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let batch = workspace
                .next_commit()
                .await
                .expect("workspace follow protocol")
                .expect("retained workspace");
            if batch.changes.iter().any(
                |p| matches!(p,workspace_event::Payload::Bookmark(b) if b.label=="local favorite"),
            ) {
                break;
            }
        }
    })
    .await
    .expect("postcommit bookmark push");
    let mut wrong = bookmark.clone();
    wrong.client_operation_id = uuid::Uuid::new_v4().to_string();
    wrong.bookmark.as_mut().expect("ref").account = Some(PrincipalRef {
        id: uuid::Uuid::new_v4().to_string(),
    });
    assert!(
        remote
            .api
            .call::<thread_api::rpc::WorkspaceServiceSetBookmark>(&wrong)
            .await
            .is_err(),
        "another account's private preference denied"
    );
    let mut threads = remote
        .observe::<thread_api::rpc::ThreadServiceObserveThreads>(
            ObserveThreadsRequest {
                query: Some(ThreadQuery {
                    spools: vec![SpoolRef {
                        id: spool.to_string(),
                    }],
                    order: thread_query::Order::NameAsc as i32,
                    ..Default::default()
                }),
                observe: options.clone(),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("local Thread list");
    let batch = threads
        .next_commit()
        .await
        .expect("Thread list protocol")
        .expect("list snapshot");
    assert!(
        !batch.changes.is_empty(),
        "list returns actual local native Threads"
    );
    let resolved = remote
        .api
        .call::<thread_api::rpc::WorkspaceServiceResolveResources>(&ResolveResourcesRequest {
            selectors: vec![ResourceSelector {
                selector: Some(resource_selector::Selector::SpoolAddress(
                    original.slug.clone(),
                )),
            }],
            budget: None,
        })
        .await
        .expect("local resolution");
    assert_eq!(resolved.results[0].coverage, Coverage::Complete as i32);
    let mut exact = remote
        .observe::<thread_api::rpc::SpoolServiceObserveSpool>(
            ObserveSpoolRequest {
                spool: Some(SpoolRef {
                    id: spool.to_string(),
                }),
                sections: vec![SpoolSection::Overview as i32, SpoolSection::Threads as i32],
                observe: options.clone(),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("Spool composition");
    let batch = exact
        .next_commit()
        .await
        .expect("Spool protocol")
        .expect("Spool snapshot");
    assert!(
        batch
            .changes
            .iter()
            .any(|p| matches!(p, spool_event::Payload::Thread(_)))
    );
    assert!(batch.changes.iter().any(|p|matches!(p,spool_event::Payload::Spool(s) if s.current_bookmark.as_ref().is_some_and(|b|b.bookmarked && !b.version.is_empty()) && s.actions.iter().any(|a|a.method.ends_with("/ReviseSpool") && a.authorized && !a.observed_versions.is_empty()))),"exact Spool view supplies bookmark and mutation CAS without extra calls");
    let foreign = Ed25519Signer::from_seed(&[88; 32]).expect("independent owner");
    let foreign_request = CreateSpoolRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        slug: "different-owner".into(),
        ownership: Some(create_spool_request::Ownership::OwnerGenesis(
            repo::sign_spool_owner_genesis(&foreign, *uuid::Uuid::now_v7().as_bytes())
                .expect("independent genesis"),
        )),
        ..Default::default()
    };
    assert!(
        remote
            .api
            .call::<thread_api::rpc::SpoolServiceCreateSpool>(&foreign_request)
            .await
            .is_err(),
        "local creation requires the admitted current owner"
    );
    let signer = Ed25519Signer::from_seed(&[71; 32]).expect("owner signer");
    let id = uuid::Uuid::now_v7();
    let genesis =
        repo::sign_spool_owner_genesis(&signer, *id.as_bytes()).expect("original genesis");
    let create = CreateSpoolRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        parent: None,
        slug: "new-private".into(),
        ownership: Some(create_spool_request::Ownership::OwnerGenesis(genesis)),
        ..Default::default()
    };
    let created = remote
        .api
        .call::<thread_api::rpc::SpoolServiceCreateSpool>(&create)
        .await
        .expect("local create");
    assert_eq!(
        created,
        remote
            .api
            .call::<thread_api::rpc::SpoolServiceCreateSpool>(&create)
            .await
            .expect("create retry")
    );
    let created = created.spool.expect("created overview");
    assert_eq!(created.r#ref.as_ref().expect("ref").id, id.to_string());
    let revise = ReviseSpoolRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        spool: created.r#ref.clone(),
        expected_version: created.version.clone(),
        name: "Renamed local".into(),
        settings: created.settings.clone(),
    };
    let revised = remote
        .api
        .call::<thread_api::rpc::SpoolServiceReviseSpool>(&revise)
        .await
        .expect("revise")
        .spool
        .expect("revised");
    let mut stale = revise.clone();
    stale.client_operation_id = uuid::Uuid::new_v4().to_string();
    assert!(
        remote
            .api
            .call::<thread_api::rpc::SpoolServiceReviseSpool>(&stale)
            .await
            .is_err(),
        "stale local CAS denied"
    );
    let mount = SpoolMount {
        r#ref: Some(RecordRef {
            spool: Some(SpoolRef {
                id: spool.to_string(),
            }),
            id: uuid::Uuid::new_v4().to_string(),
        }),
        parent: Some(SpoolRef {
            id: spool.to_string(),
        }),
        child: created.r#ref.clone(),
        name: "mounted".into(),
        version: vec![],
    };
    let set = SetSpoolMountRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        mount: Some(mount.clone()),
        expected_version: vec![],
    };
    let response = remote
        .api
        .call::<thread_api::rpc::SpoolServiceSetSpoolMount>(&set)
        .await
        .expect("set mount");
    let Some(mutation_receipt::Outcome::Applied(applied)) =
        response.receipt.expect("receipt").outcome
    else {
        panic!("applied mount")
    };
    let version = applied.resulting_versions[0].version.clone();
    assert!(!version.is_empty());
    assert!(
        remote
            .api
            .call::<thread_api::rpc::SpoolServiceDeleteSpool>(&DeleteSpoolRequest {
                client_operation_id: uuid::Uuid::new_v4().to_string(),
                spool: created.r#ref.clone(),
                expected_version: revised.version.clone()
            })
            .await
            .is_err(),
        "mounted Spool deletion denied"
    );
    remote
        .api
        .call::<thread_api::rpc::SpoolServiceRemoveSpoolMount>(&RemoveSpoolMountRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            mount: Some(mount),
            expected_version: version,
        })
        .await
        .expect("remove mount");
    remote
        .api
        .call::<thread_api::rpc::SpoolServiceDeleteSpool>(&DeleteSpoolRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            spool: created.r#ref.clone(),
            expected_version: revised.version,
        })
        .await
        .expect("delete catalog identity");
    assert!(repo::device_catalog::load(&device.home, id).is_err());
}

#[tokio::test]
async fn real_account_rpc_composes_private_views_and_enforces_scoped_mutations() {
    use std::{net::Ipv4Addr, sync::Arc};

    use crypto::Signer;
    use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};

    use crate::hosted_runtime::{
        claim_authorization::StoredClaimAuthorization,
        hosted::claim_protocol::{ClaimProtocol, NATIVE_ALPN},
        root_mint::mint_agent_root,
    };
    let _guard = config::credentials::lock_test_env();
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(value) => std::env::set_var("HEDDLE_HOME", value),
                    None => std::env::remove_var("HEDDLE_HOME"),
                }
            }
        }
    }
    let home = tempfile::tempdir().expect("home");
    let _restore = Restore(std::env::var_os("HEDDLE_HOME"));
    unsafe {
        std::env::set_var("HEDDLE_HOME", home.path());
    }

    let local = tempfile::tempdir().expect("repository");
    let repository = repo::Repository::init_default(local.path()).expect("repo");
    let root = Ed25519Signer::from_seed(&[71; 32]).expect("root");
    let recovery = Ed25519Signer::from_seed(&[72; 32]).expect("recovery");
    let signed =
        repo::sign_custodial_owner_root(&root, &recovery, [9; 16], [5; 32]).expect("owner root");
    let binding = repo::sign_custodial_owner_binding(&root, &signed, [6; 32]).expect("binding");
    let verified = heddleco_capability_verifier::verify_owner_root(&signed).expect("root proof");
    let owner = OwnerState {
        owner: Some(PrincipalRef {
            id: uuid::Uuid::from_bytes([9; 16]).to_string(),
        }),
        root: Some(signed),
        binding: Some(binding),
        version: verified.state_hash().to_vec(),
        ..Default::default()
    };
    repo::device_authority::publish(
        home.path(),
        &repo::device_authority::DeviceAuthority {
            owner: owner.clone(),
            mint_roots: vec![],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        },
        chrono::Utc::now().timestamp(),
    )
    .expect("independent local enrollment");
    let base = repository.head().expect("head").expect("base");
    let replica = repository
        .create_native_thread("device-test", base, None, "device operations")
        .expect("Thread");
    let spool = uuid::Uuid::parse_str(&replica.genesis().expect("genesis").spool).expect("spool");
    repo::device_catalog::register(home.path(), &repository, spool).expect("catalog");
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("address")
        .bind()
        .await
        .expect("endpoint");
    let address = endpoint.addr();
    let key = *endpoint.id().as_bytes();
    let device = Arc::new(DeviceRpc::new(home.path().to_owned(), key));
    let (authorization, _, _) = StoredClaimAuthorization::new();
    let authorization = Arc::new(authorization);
    let protocol =
        ClaimProtocol::new(authorization.clone(), authorization, key).with_device(device.clone());

    let router = Router::builder(endpoint)
        .accept(NATIVE_ALPN, protocol)
        .spawn();
    let browser = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("browser address")
        .bind()
        .await
        .expect("browser");
    let connection = browser
        .connect(address.clone(), NATIVE_ALPN)
        .await
        .expect("direct Iroh");
    let token = mint_agent_root(&[71; 32]).expect("self issued root").token;
    let credentials = thread_api::credentials::Credentials::Signed {
        signer: Arc::new(root),
        biscuit: token.into_bytes(),
        grant_envelope: Vec::new(),
    };
    let transport = thread_api::transport::IrohTransport::new(
        connection,
        credentials,
        256 * 1024,
        std::time::Duration::from_secs(10),
    )
    .expect("transport");
    let remote = thread_api::Remote::discover(transport, key, EndpointKind::Device)
        .await
        .expect("discover");
    roundtrip(&remote, &device, spool).await;
    let other = tempfile::tempdir().expect("other private repository");
    let other_repo = repo::Repository::init_default(other.path()).expect("other repository");
    let other_id = other_repo.native_spool_id().expect("other Spool");
    repo::device_catalog::register(home.path(), &other_repo, other_id).expect("other registration");
    let root = Ed25519Signer::from_seed(&[71; 32]).expect("root signer");
    let key = biscuit_verifier::PublicKey::from_bytes(
        &root.public_key(),
        biscuit_auth::Algorithm::Ed25519,
    )
    .expect("root public key");
    let token = mint_agent_root(&[71; 32]).expect("root credential").token;
    let publisher: [u8; 32] = root.public_key().try_into().expect("public key");
    let payload = biscuit_verifier::key_delegation::statement(&token, &publisher)
        .expect("delegation statement");
    let signature: [u8; 64] = root
        .sign(&payload)
        .expect("delegation signature")
        .try_into()
        .expect("signature bytes");
    let scoped = biscuit_verifier::key_delegation::append(
        &token,
        &publisher,
        &signature,
        biscuit_auth::builder::BlockBuilder::new()
            .code(format!("check if resource(\"spool\", \"{}\");", spool).as_str())
            .expect("resource restriction"),
    )
    .expect("same-key attenuation");
    let verified = biscuit_auth::Biscuit::from_base64(&scoped, |_| Ok(key))
        .expect("verified scoped credential");
    biscuit_verifier::authorize_at(
        &verified,
        "ObserveWorkspace",
        chrono::Utc::now(),
        None,
        &[],
        Some(("spool", &spool.to_string())),
    )
    .expect("exact scoped workspace permission");
    let connection = browser
        .connect(address, NATIVE_ALPN)
        .await
        .expect("scoped connection");
    let transport = IrohTransport::new(
        connection,
        Credentials::Signed {
            signer: Arc::new(root),
            biscuit: scoped.into_bytes(),
            grant_envelope: vec![],
        },
        256 * 1024,
        std::time::Duration::from_secs(10),
    )
    .expect("scoped transport");
    let scoped = Remote::discover(transport, device.endpoint, EndpointKind::Device)
        .await
        .expect("scoped discovery");
    let mut observed = scoped
        .observe::<thread_api::rpc::WorkspaceServiceObserveWorkspace>(
            ObserveWorkspaceRequest {
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("scoped workspace");
    let batch = observed
        .next_commit()
        .await
        .expect("scoped protocol")
        .expect("scoped snapshot");
    let ids: Vec<_> = batch
        .changes
        .iter()
        .filter_map(|p| {
            if let workspace_event::Payload::Spool(s) = p {
                s.r#ref.as_ref().map(|r| r.id.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ids, vec![spool.to_string()]);
    let wrong = SetBookmarkRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        bookmark: Some(BookmarkRef {
            account: Some(PrincipalRef {
                id: uuid::Uuid::from_bytes([9; 16]).to_string(),
            }),
            target: Some(bookmark_ref::Target::Spool(SpoolRef {
                id: other_id.to_string(),
            })),
        }),
        bookmarked: true,
        ..Default::default()
    };
    assert!(
        scoped
            .api
            .call::<thread_api::rpc::WorkspaceServiceSetBookmark>(&wrong)
            .await
            .is_err(),
        "scoped agent may not mutate bookmarks for another Spool"
    );
    super::tests::denied_spool_stream_is_typed(&scoped, other_id).await;
    browser.close().await;
    router.shutdown().await.expect("router shutdown");
}
