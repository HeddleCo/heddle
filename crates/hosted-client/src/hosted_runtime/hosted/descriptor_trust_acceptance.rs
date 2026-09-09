use std::{
    collections::{HashMap, VecDeque},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Duration,
};

use api::{
    HOSTED_ALPN_V1,
    descriptor_trust::{
        AttestedEndpointDescriptorEntry, EndpointDescriptorSetDocument, ephemeral_attestation_bytes,
    },
    heddle::api::v1alpha1::{EndpointDescriptor, SignedEndpointDescriptor},
    signing::endpoint_descriptor_bytes,
};
use crypto::{Ed25519Signer, Signer};
use futures::FutureExt;
use prost::Message;

use super::{
    descriptor_trust::load_automatic_pin,
    resolver::resolve_and_verify_endpoint_descriptor,
    test_https::{TestHttpsServer, TestResponse},
};

const KEY_PATH: &str = "/.well-known/heddle/iroh-descriptor-key";
const DESCRIPTOR_PATH: &str = "/.well-known/heddle/iroh-endpoint";
const NOW_SKEW_BEFORE: i64 = 1_000;
const NOW_SKEW_AFTER: i64 = 60_000;

#[tokio::test]
async fn clean_first_contact_pins_the_root_and_reuses_without_rediscovery() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let server = server_with_root("first-key", &root, 2);
        let config = trusted_config(&server);

        resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        let pin = load_automatic_pin(server.authority()).unwrap().unwrap();
        assert_eq!(pin.key_id, "first-key");
        assert_eq!(pin.public_key, hex::encode(root.public_key()));
        assert_eq!(server.requests(), [KEY_PATH, DESCRIPTOR_PATH]);

        resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        assert_eq!(
            server.requests(),
            [KEY_PATH, DESCRIPTOR_PATH, DESCRIPTOR_PATH]
        );
        let pin_again = load_automatic_pin(server.authority()).unwrap().unwrap();
        assert_eq!(pin_again.public_key, pin.public_key);
    })
    .await;
}

#[tokio::test]
async fn tls_authenticates_first_contact_and_configured_ca_allows_it() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let server = server_with_root("tls-key", &root, 1);
        let untrusted_error =
            resolve_and_verify_endpoint_descriptor(server.authority(), &Default::default())
                .await
                .unwrap_err();
        let message = untrusted_error.to_string();
        assert!(message.contains("HTTPS request failed"));
        assert!(
            message.contains("HEDDLE_REMOTE_TLS_CA_CERT"),
            "UnknownIssuer must name the CA configuration: {message}"
        );
        assert!(load_automatic_pin(server.authority()).unwrap().is_none());

        resolve_and_verify_endpoint_descriptor(server.authority(), &trusted_config(&server))
            .await
            .unwrap();
        assert!(load_automatic_pin(server.authority()).unwrap().is_some());
    })
    .await;
}

#[tokio::test]
async fn failed_tls_chain_is_rejected_outright() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let server = server_with_root("tls-fail", &root, 1);
        let error = resolve_and_verify_endpoint_descriptor(server.authority(), &Default::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("HTTPS request failed"));
        assert!(load_automatic_pin(server.authority()).unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn invalid_key_documents_and_unverified_candidates_never_pin() {
    let cases = [
        ("malformed", TestResponse::json(b"{".to_vec())),
        ("oversized", TestResponse::json(vec![b'x'; 4 * 1024 + 1])),
        (
            "wrong-version",
            TestResponse::json(key_document(2, "key", &[1; 32])),
        ),
        (
            "empty-id",
            TestResponse::json(key_document(1, "", &[1; 32])),
        ),
        (
            "bad-hex",
            TestResponse::json(br#"{"version":1,"key_id":"key","public_key":"zz"}"#.to_vec()),
        ),
        (
            "wrong-length",
            TestResponse::json(key_document(1, "key", &[1; 31])),
        ),
        ("non-200", TestResponse::status(500)),
        (
            "redirect",
            TestResponse::redirect("https://example.invalid/key"),
        ),
    ];
    for (name, response) in cases {
        with_isolated_home_async(|_| async move {
            let server = TestHttpsServer::start(HashMap::from([(
                KEY_PATH.to_string(),
                VecDeque::from([response]),
            )]));
            let result = resolve_and_verify_endpoint_descriptor(
                server.authority(),
                &trusted_config(&server),
            )
            .await;
            assert!(result.is_err(), "{name} unexpectedly succeeded");
            assert!(
                load_automatic_pin(server.authority()).unwrap().is_none(),
                "{name} wrote a pin"
            );
            assert_eq!(server.requests(), [KEY_PATH], "{name}");
        })
        .await;
    }

    with_isolated_home_async(|_| async {
        let published_root = Ed25519Signer::generate().unwrap();
        let attestor = Ed25519Signer::generate().unwrap();
        let server =
            TestHttpsServer::start(routes_for_root("candidate", &published_root, &attestor, 1));
        assert!(
            resolve_and_verify_endpoint_descriptor(server.authority(), &trusted_config(&server))
                .await
                .is_err()
        );
        assert!(load_automatic_pin(server.authority()).unwrap().is_none());
        assert_eq!(server.requests(), [KEY_PATH, DESCRIPTOR_PATH]);
    })
    .await;
}

#[tokio::test]
async fn unattested_and_tampered_entries_are_never_dialed() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let ephemeral = Ed25519Signer::generate().unwrap();
        let mut entry = attested_entry(&root, &ephemeral, "live", "hel");
        entry.attestation_signature = "00".repeat(64);
        let server = TestHttpsServer::start(HashMap::from([
            (
                KEY_PATH.to_string(),
                VecDeque::from([TestResponse::json(key_document(
                    1,
                    "root-id",
                    root.public_key(),
                ))]),
            ),
            (
                DESCRIPTOR_PATH.to_string(),
                VecDeque::from([TestResponse::json(set_document(&[entry]))]),
            ),
        ]));
        let error =
            resolve_and_verify_endpoint_descriptor(server.authority(), &trusted_config(&server))
                .await
                .unwrap_err();
        assert!(
            error.to_string().contains("signature is invalid"),
            "{error}"
        );
        assert!(load_automatic_pin(server.authority()).unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn ephemeral_rotation_under_the_pinned_root_does_not_change_the_pin() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let first_ephemeral = Ed25519Signer::generate().unwrap();
        let rotated_ephemeral = Ed25519Signer::generate().unwrap();
        let first = attested_entry(&root, &first_ephemeral, "ephemeral-a", "hel");
        let rotated = attested_entry(&root, &rotated_ephemeral, "ephemeral-b", "sjc");
        let server = TestHttpsServer::start(HashMap::from([
            (
                KEY_PATH.to_string(),
                VecDeque::from([TestResponse::json(key_document(
                    1,
                    "stable-root",
                    root.public_key(),
                ))]),
            ),
            (
                DESCRIPTOR_PATH.to_string(),
                VecDeque::from([
                    TestResponse::json(set_document(&[first])),
                    TestResponse::json(set_document(&[rotated])),
                ]),
            ),
        ]));
        let config = trusted_config(&server);
        resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        let before = fs::read(super::descriptor_trust_path()).unwrap();
        resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        assert_eq!(fs::read(super::descriptor_trust_path()).unwrap(), before);
        let pin = load_automatic_pin(server.authority()).unwrap().unwrap();
        assert_eq!(pin.key_id, "stable-root");
        assert_eq!(pin.public_key, hex::encode(root.public_key()));
    })
    .await;
}

#[tokio::test]
async fn served_set_cannot_swap_the_pinned_root() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let other = Ed25519Signer::generate().unwrap();
        let first_ephemeral = Ed25519Signer::generate().unwrap();
        let swapped_ephemeral = Ed25519Signer::generate().unwrap();
        let first = attested_entry(&root, &first_ephemeral, "ephemeral-a", "hel");
        let swapped = attested_entry(&other, &swapped_ephemeral, "ephemeral-b", "hel");
        let server = TestHttpsServer::start(HashMap::from([
            (
                KEY_PATH.to_string(),
                VecDeque::from([TestResponse::json(key_document(
                    1,
                    "stable-root",
                    root.public_key(),
                ))]),
            ),
            (
                DESCRIPTOR_PATH.to_string(),
                VecDeque::from([
                    TestResponse::json(set_document(&[first])),
                    TestResponse::json(set_document(&[swapped])),
                ]),
            ),
        ]));
        let config = trusted_config(&server);
        resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        let before = fs::read(super::descriptor_trust_path()).unwrap();

        let error = tokio::time::timeout(
            Duration::from_secs(1),
            super::HostedClient::connect_server(server.authority(), &config),
        )
        .await
        .expect("root swap must fail before an Iroh dial can stall")
        .unwrap_err();
        assert!(
            error.to_string().contains("signature is invalid")
                || error
                    .to_string()
                    .contains("Automatic root rotation was refused")
                || error.to_string().contains("descriptor root changed"),
            "{error}"
        );
        assert_eq!(fs::read(super::descriptor_trust_path()).unwrap(), before);
    })
    .await;
}

#[tokio::test]
async fn expired_and_not_yet_valid_entries_are_excluded() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let expired_ephemeral = Ed25519Signer::generate().unwrap();
        let pending_ephemeral = Ed25519Signer::generate().unwrap();
        let expired =
            attested_entry_window(&root, &expired_ephemeral, "old", "hel", now - 10_000, now);
        let pending = attested_entry_window(
            &root,
            &pending_ephemeral,
            "next",
            "hel",
            now + 60_000,
            now + 120_000,
        );
        let server = TestHttpsServer::start(HashMap::from([
            (
                KEY_PATH.to_string(),
                VecDeque::from([TestResponse::json(key_document(
                    1,
                    "root-id",
                    root.public_key(),
                ))]),
            ),
            (
                DESCRIPTOR_PATH.to_string(),
                VecDeque::from([TestResponse::json(set_document(&[expired, pending]))]),
            ),
        ]));
        let error =
            resolve_and_verify_endpoint_descriptor(server.authority(), &trusted_config(&server))
                .await
                .unwrap_err();
        assert!(
            error.to_string().contains("expired or not yet valid"),
            "{error}"
        );
        assert!(load_automatic_pin(server.authority()).unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn region_preference_selects_local_then_falls_back_to_remote() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let remote_ephemeral = Ed25519Signer::generate().unwrap();
        let local_ephemeral = Ed25519Signer::generate().unwrap();
        let remote = attested_entry(&root, &remote_ephemeral, "remote", "sjc");
        let local = attested_entry(&root, &local_ephemeral, "local", "hel");
        let server = TestHttpsServer::start(HashMap::from([
            (
                KEY_PATH.to_string(),
                VecDeque::from([TestResponse::json(key_document(
                    1,
                    "root-id",
                    root.public_key(),
                ))]),
            ),
            (
                DESCRIPTOR_PATH.to_string(),
                VecDeque::from([TestResponse::json(set_document(&[remote, local]))]),
            ),
        ]));
        let config = trusted_config(&server).with_preferred_region("hel");
        let verified = resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        assert_eq!(
            verified.document().endpoint_id,
            hex::encode(local_ephemeral.public_key())
        );
    })
    .await;

    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let remote_ephemeral = Ed25519Signer::generate().unwrap();
        let remote = attested_entry(&root, &remote_ephemeral, "remote", "sjc");
        let server = TestHttpsServer::start(HashMap::from([
            (
                KEY_PATH.to_string(),
                VecDeque::from([TestResponse::json(key_document(
                    1,
                    "root-id",
                    root.public_key(),
                ))]),
            ),
            (
                DESCRIPTOR_PATH.to_string(),
                VecDeque::from([TestResponse::json(set_document(&[remote]))]),
            ),
        ]));
        let config = trusted_config(&server).with_preferred_region("hel");
        let verified = resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        assert_eq!(
            verified.document().endpoint_id,
            hex::encode(remote_ephemeral.public_key())
        );
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn trust_store_write_failure_prevents_iroh_and_iroh_failure_keeps_pin() {
    use std::os::unix::fs::PermissionsExt;

    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let server = server_with_root("write-key", &root, 1);
        let home = repo::identity::heddle_home_dir();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap();
        let result =
            super::HostedClient::connect_server(server.authority(), &trusted_config(&server)).await;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();

        let error = result.expect_err("pin persistence must fail before Iroh");
        assert!(
            error.to_string().contains("locking descriptor trust store")
                || error.to_string().contains("Permission denied")
        );
        assert!(!super::descriptor_trust_path().exists());
        assert!(!config::credentials::credentials_path().exists());
        assert_eq!(server.requests(), [KEY_PATH, DESCRIPTOR_PATH]);
    })
    .await;

    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let server = server_with_root("dial-key", &root, 1);
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            super::HostedClient::connect_server(server.authority(), &trusted_config(&server)),
        )
        .await;
        assert!(
            result.is_err() || result.unwrap().is_err(),
            "the unreachable Iroh endpoint must not connect"
        );
        let pin = load_automatic_pin(server.authority()).unwrap().unwrap();
        assert_eq!(pin.key_id, "dial-key");
        assert!(!config::credentials::credentials_path().exists());
        assert_eq!(server.requests(), [KEY_PATH, DESCRIPTOR_PATH]);
    })
    .await;
}

#[tokio::test]
async fn explicit_pair_skips_discovery_and_old_server_failure_is_actionable() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let server = TestHttpsServer::start(HashMap::from([(
            DESCRIPTOR_PATH.to_string(),
            VecDeque::from([TestResponse::json(set_document(&[attested_entry(
                &root,
                &Ed25519Signer::generate().unwrap(),
                "explicit-ephemeral",
                "hel",
            )]))]),
        )]));
        let public_key: [u8; 32] = root.public_key().try_into().unwrap();
        let config = trusted_config(&server).with_descriptor_trust("explicit-id", public_key);
        resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        assert_eq!(server.requests(), [DESCRIPTOR_PATH]);
        assert!(!super::descriptor_trust_path().exists());
    })
    .await;

    with_isolated_home_async(|_| async {
        let server = TestHttpsServer::start(HashMap::new());
        let error =
            resolve_and_verify_endpoint_descriptor(server.authority(), &trusted_config(&server))
                .await
                .unwrap_err();
        assert!(error.to_string().contains(
            "server does not publish descriptor trust; configure both values or upgrade the server"
        ));
        assert!(!super::descriptor_trust_path().exists());
    })
    .await;
}

#[tokio::test]
async fn missing_iroh_endpoint_is_named_and_never_downgrades() {
    with_isolated_home_async(|_| async {
        let root = Ed25519Signer::generate().unwrap();
        let server = TestHttpsServer::start(HashMap::from([(
            KEY_PATH.to_string(),
            VecDeque::from([TestResponse::json(key_document(
                1,
                "missing-endpoint",
                root.public_key(),
            ))]),
        )]));

        let error =
            resolve_and_verify_endpoint_descriptor(server.authority(), &trusted_config(&server))
                .await
                .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("server does not advertise an Iroh endpoint"),
            "missing endpoint must be explicit, got: {error}"
        );
        assert!(load_automatic_pin(server.authority()).unwrap().is_none());
        assert_eq!(server.requests(), [KEY_PATH, DESCRIPTOR_PATH]);
    })
    .await;
}

fn server_with_root(
    key_id: &str,
    root: &Ed25519Signer,
    descriptor_count: usize,
) -> TestHttpsServer {
    TestHttpsServer::start(routes_for_root(key_id, root, root, descriptor_count))
}

fn routes_for_root(
    key_id: &str,
    published_root: &Ed25519Signer,
    attestor: &Ed25519Signer,
    descriptor_count: usize,
) -> HashMap<String, VecDeque<TestResponse>> {
    let ephemeral = Ed25519Signer::generate().unwrap();
    let entry = attested_entry(attestor, &ephemeral, "ephemeral-1", "hel");
    HashMap::from([
        (
            KEY_PATH.to_string(),
            VecDeque::from([TestResponse::json(key_document(
                1,
                key_id,
                published_root.public_key(),
            ))]),
        ),
        (
            DESCRIPTOR_PATH.to_string(),
            (0..descriptor_count)
                .map(|_| TestResponse::json(set_document(std::slice::from_ref(&entry))))
                .collect(),
        ),
    ])
}

fn key_document(version: u32, key_id: &str, public_key: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": version,
        "key_id": key_id,
        "public_key": hex::encode(public_key),
    }))
    .unwrap()
}

fn set_document(entries: &[AttestedEndpointDescriptorEntry]) -> Vec<u8> {
    serde_json::to_vec(&EndpointDescriptorSetDocument {
        version: 1,
        root_key_id: "test-root".to_string(),
        entries: entries.to_vec(),
    })
    .unwrap()
}

fn attested_entry(
    root: &Ed25519Signer,
    ephemeral: &Ed25519Signer,
    key_id: &str,
    region: &str,
) -> AttestedEndpointDescriptorEntry {
    let now = chrono::Utc::now().timestamp_millis();
    attested_entry_window(
        root,
        ephemeral,
        key_id,
        region,
        now - NOW_SKEW_BEFORE,
        now + NOW_SKEW_AFTER,
    )
}

fn attested_entry_window(
    root: &Ed25519Signer,
    ephemeral: &Ed25519Signer,
    key_id: &str,
    region: &str,
    not_before: i64,
    not_after: i64,
) -> AttestedEndpointDescriptorEntry {
    let public_key: [u8; 32] = ephemeral.public_key().try_into().unwrap();
    let descriptor = EndpointDescriptor {
        version: 1,
        endpoint_id: hex::encode(public_key),
        relay_urls: Vec::new(),
        direct_addresses: vec!["127.0.0.1:9".to_string()],
        supported_alpns: vec![HOSTED_ALPN_V1.to_vec()],
        issued_at_unix_millis: not_before,
        expires_at_unix_millis: not_after,
        rotation: None,
    };
    let descriptor_signature = ephemeral
        .sign(&endpoint_descriptor_bytes(&descriptor))
        .unwrap();
    let signed = SignedEndpointDescriptor {
        descriptor: Some(descriptor),
        key_id: key_id.to_string(),
        signature: descriptor_signature,
    };
    let attestation_signature = root
        .sign(&ephemeral_attestation_bytes(
            key_id,
            &public_key,
            not_before,
            not_after,
            region,
        ))
        .unwrap();
    AttestedEndpointDescriptorEntry {
        ephemeral_key_id: key_id.to_string(),
        ephemeral_public_key: hex::encode(public_key),
        not_before_unix_millis: not_before,
        not_after_unix_millis: not_after,
        region: region.to_string(),
        attestation_signature: hex::encode(attestation_signature),
        signed_descriptor: hex::encode(signed.encode_to_vec()),
    }
}

fn trusted_config(server: &TestHttpsServer) -> config::ClientConfig {
    config::ClientConfig::default()
        .with_tls_ca_certificate_pem(server.certificate_pem().to_string())
}

// HEDDLE_HOME is process-global, so this test helper deliberately holds the
// repository's shared environment lock across each async scenario.
#[allow(clippy::await_holding_lock)]
async fn with_isolated_home_async<F, Fut, T>(test: F) -> T
where
    F: FnOnce(&std::path::Path) -> Fut,
    Fut: Future<Output = T>,
{
    let _guard = config::credentials::lock_test_env();
    let home = tempfile::TempDir::new().unwrap();
    let previous = std::env::var_os("HEDDLE_HOME");
    unsafe {
        std::env::set_var("HEDDLE_HOME", home.path());
    }
    let future = catch_unwind(AssertUnwindSafe(|| test(home.path())));
    let output = match future {
        Ok(future) => match AssertUnwindSafe(future).catch_unwind().await {
            Ok(output) => output,
            Err(payload) => {
                unsafe {
                    match previous.clone() {
                        Some(value) => std::env::set_var("HEDDLE_HOME", value),
                        None => std::env::remove_var("HEDDLE_HOME"),
                    }
                }
                std::panic::resume_unwind(payload)
            }
        },
        Err(payload) => std::panic::resume_unwind(payload),
    };
    unsafe {
        match previous {
            Some(value) => std::env::set_var("HEDDLE_HOME", value),
            None => std::env::remove_var("HEDDLE_HOME"),
        }
    }
    output
}
