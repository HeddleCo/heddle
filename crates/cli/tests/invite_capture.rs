// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "client")]

#[path = "support/native_hosted_server.rs"]
mod native_hosted_server;
mod support;

use crypto::{Ed25519Signer, Signer, thread_operation::SignedGenesis};
use objects::object::thread_replication::{GenesisOwner, SourceAuthor, ThreadGenesis};
use support::*;

#[test]
fn invite_enrollment_captures_and_pushes_account_thread_and_refuses_missing_proof() {
    on_large_stack(invite_capture);
}

async fn invite_capture() {
    let temp = TempDir::new().expect("fixture");
    let home = temp.path().join("enrolled-home");
    let source_home = temp.path().join("source-home");
    let source = temp.path().join("source");
    let clone = temp.path().join("clone");
    std::fs::create_dir(&home).expect("fresh enrollment home");
    std::fs::create_dir(&source).expect("source");
    let seed_env = [("HEDDLE_HOME", source_home.to_str().expect("source home"))];
    heddle_env(&["init"], Some(&source), &seed_env).expect("seed init");
    std::fs::write(source.join("story.txt"), "seed\n").expect("seed file");
    heddle_env(&["capture", "-m", "seed"], Some(&source), &seed_env).expect("seed state");
    let repo = Repository::open(&source).expect("source repo");
    let base = repo.head().expect("HEAD").expect("seed state");
    let thread_base = repo
        .store()
        .get_state(&base)
        .expect("seed state")
        .expect("stored seed")
        .parents[0];
    let spool: uuid::Uuid = repo
        .native_thread("main")
        .expect("seed Thread")
        .genesis()
        .expect("seed genesis")
        .spool
        .parse()
        .expect("spool");
    // Choose the client's key deterministically to match the shared hosted
    // fixture's registered account. Enrollment must reuse it for every role.
    let seed = [71; 32];
    let signer = Ed25519Signer::from_seed(&seed).expect("client signer");
    let publisher: [u8; 32] = signer.public_key().try_into().expect("publisher");
    objects::fs_atomic::write_file_atomic_secret(
        &home.join("agent-node-identity.toml"),
        toml::to_string(&serde_json::json!({
            "format": "heddle-agent-node-identity", "version": 1,
            "secret_key": hex::encode(seed), "node_id": hex::encode(publisher),
        }))
        .expect("node identity")
        .as_bytes(),
    )
    .expect("persist client's node key");
    let genesis = ThreadGenesis {
        version: 1,
        spool: spool.to_string(),
        parent: None,
        base: thread_base,
        name: "main".into(),
        intent: "account-owned fixture".into(),
        creator: publisher,
        owner: GenesisOwner::Account(uuid::Uuid::from_u128(2)),
        nonce: vec![1; 32],
    };
    let signed = SignedGenesis::sign(&genesis, &signer).expect("client Thread genesis");
    let (mut client, server, captured, _, _, https) = native_hosted_server::start_routed(
        spool,
        "main",
        *genesis.id().expect("Thread ID").as_bytes(),
    )
    .await;
    let ca = temp.path().join("ca.pem");
    std::fs::write(&ca, &https.certificate_pem).expect("test CA");
    let root = Ed25519Signer::from_seed(&[7; 32]).expect("descriptor root");
    let output_at = |path: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_heddle"))
            .args(["--output", "json"])
            .args(args)
            .current_dir(path)
            .env("HEDDLE_HOME", &home)
            .env_remove("HEDDLE_CREDENTIAL")
            .env_remove("HEDDLE_CONFIG")
            .env("HEDDLE_FSMONITOR", "off")
            .env("HEDDLE_PRINCIPAL_NAME", "Invite test")
            .env("HEDDLE_PRINCIPAL_EMAIL", "invite@example.test")
            .env("HTTPS_PROXY", &https.proxy_uri)
            .env("HEDDLE_REMOTE_TLS_CA_CERT", &ca)
            .env("HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID", "clone-test-key")
            .env(
                "HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY",
                hex::encode(root.public_key()),
            )
            .output()
            .expect("CLI process")
    };
    let run = |path: &Path, args: &[&str]| {
        let output = output_at(path, args);
        assert!(
            output.status.success(),
            "{args:?}: exit {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::Deserializer::from_slice(&output.stdout)
            .into_iter::<Value>()
            .map(|value| value.expect("CLI JSON"))
            .last()
            .expect("CLI result")
    };
    let login = run(
        temp.path(),
        &[
            "auth",
            "login",
            "--server",
            &https.authority,
            "--invite",
            "fixture-invite",
        ],
    );
    assert_eq!(login["output_kind"], "agent_account_created");
    let process_home = ProcessHeddleHome::install(&home);
    let credential = config::credentials::get_server_credential(&https.authority)
        .expect("credential store")
        .expect("enrolled credential");
    drop(process_home);
    assert_eq!(
        Ed25519Signer::from_pem(credential.private_key_pem.as_deref().expect("enrolled PEM"))
            .expect("enrolled signer")
            .public_key(),
        publisher
    );
    let owner = captured
        .lock()
        .expect("hosted account")
        .account_owner
        .clone()
        .expect("client bootstrapped owner");
    assert_eq!(
        repo::seq0_authority_public_key(owner.root.as_ref().expect("client root"))
            .expect("account root key"),
        publisher
    );
    assert_eq!(
        captured
            .lock()
            .expect("provision request")
            .provision_requests[0]
            .agent_public_key,
        publisher
    );

    // Create the server's source in a separate home. This setup must never
    // supply the missing enrollment artifacts to the home under test.
    let process_home = ProcessHeddleHome::install(&source_home);
    let authority = repo::device_authority::DeviceAuthority {
        owner,
        mint_roots: vec![],
        revoked_ids: vec![],
        revoked_mint_roots: vec![],
        revoked_publishers: vec![],
    };
    let now = chrono::Utc::now().timestamp();
    repo::device_authority::publish(&source_home, &authority, now)
        .expect("source account authority");
    let key = biscuit_auth::PublicKey::from_bytes(&publisher, biscuit_auth::Algorithm::Ed25519)
        .expect("mint root key");
    let token =
        heddle_biscuit_verifier::parse_token(&credential.token, &[key]).expect("client credential");
    repo::identity::source_author::publish(
        &source_home,
        &authority,
        &publisher,
        &publisher,
        &token,
        now,
    )
    .expect("source proof");
    repo::identity::link_device_key(
        &publisher,
        &signer.to_pem().expect("source PEM"),
        &https.authority,
    )
    .expect("source signer");
    repo.rename_native_thread("main", "local-seed")
        .expect("free source name");
    let creator_authority = repo::thread_replication::metadata::prepare_control_authority(
        &authority, &publisher, &token, now,
    )
    .expect("original Thread creator proof");
    repo::thread_replication::ThreadReplica::create_authorized(
        repo.heddle_dir(),
        &signed,
        &creator_authority,
        &authority,
        "spool/acme",
        "/heddle.api.v1alpha2.ThreadService/StartThread",
        now,
    )
    .expect("create account Thread")
    .bind_local_name("main")
    .expect("account Thread name");
    repo.record_native_capture("main", base)
        .expect("account source operation");
    assert!(
        client
            .push_profiled(
                &repo,
                "spool/acme",
                base,
                "main",
                false,
                "seed-account-thread".into()
            )
            .await
            .expect("seed hosted Thread")
            .0
            .success
    );
    drop(process_home);

    let remote = format!("https://{}/spool/acme", https.authority);
    run(
        temp.path(),
        &[
            "clone",
            &remote,
            clone.to_str().expect("clone path"),
            "--thread",
            "main",
        ],
    );
    assert_eq!(
        std::fs::read(clone.join("story.txt")).expect("clone contents"),
        b"seed\n"
    );
    std::fs::write(clone.join("story.txt"), "invite edit\n").expect("edit clone");
    let calls_before_capture = captured.lock().expect("RPCs before capture").calls.len();
    run(&clone, &["capture", "-m", "invite edit"]);
    assert_eq!(
        captured.lock().expect("RPCs after capture").calls.len(),
        calls_before_capture,
        "capture uses the enrolled proof offline"
    );
    run(&clone, &["push"]);
    let cloned = Repository::open(&clone).expect("cloned repo");
    let head = cloned
        .head()
        .expect("captured HEAD")
        .expect("captured state");
    let publication = captured.lock().expect("publication");
    assert_eq!(publication.source_heads().len(), 1);
    assert!(matches!(
        &publication.source_heads()[0].revision,
        Some(api::heddle::api::v1alpha2::revision_ref::Revision::State(state))
            if state.value == head.as_bytes()
    ));
    drop(publication);
    let device = repo::identity::load_device(&home.join(repo::identity::DEVICE_IDENTITY_FILE))
        .expect("device identity")
        .expect("enrolled signer");
    assert_eq!(device.public_key, hex::encode(publisher));
    assert_eq!(
        device.credential_token.as_deref(),
        Some(credential.token.as_str())
    );
    assert_eq!(
        credential.device_id.as_deref(),
        Some(device.public_key.as_str())
    );
    assert_eq!(
        repo::device_authority::load(&home, now)
            .expect("enrolled authority")
            .owner
            .root,
        authority.owner.root
    );
    assert!(
        matches!(repo::identity::source_author::load(&home, &publisher, spool).expect("original source proof"), SourceAuthor::Account { actor, spool: bound, .. } if actor.principal_id == uuid::Uuid::from_u128(2) && bound == spool)
    );

    std::fs::remove_file(
        home.join("state/source-authors")
            .join(format!("{}.pb", hex::encode(publisher))),
    )
    .expect("remove proof");
    let process_home = ProcessHeddleHome::install(&home);
    let error = cloned
        .native_thread("main")
        .expect("account Thread")
        .source_author_for(&publisher)
        .expect_err("missing proof refuses account authorship");
    assert!(
        matches!(error, repo::thread_replication::Error::Invalid(ref message) if message == "account Thread requires original device account proof"),
        "{error:?}"
    );
    drop(process_home);
    std::fs::write(clone.join("story.txt"), "missing proof edit\n").expect("negative edit");
    let refused = output_at(&clone, &["capture", "-m", "missing proof"]);
    assert_eq!(
        refused.status.code(),
        Some(78),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(cloned.head().expect("unchanged HEAD"), Some(head));
    client.close().await;
    server.abort();
}
