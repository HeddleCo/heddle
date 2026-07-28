use std::{
    collections::{HashMap, VecDeque},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Duration,
};

use api::{
    HOSTED_ALPN_V1,
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

#[tokio::test]
async fn clean_first_contact_pins_after_verification_and_reuses_without_discovery() {
    with_isolated_home_async(|_| async {
        let signer = Ed25519Signer::generate().unwrap();
        let server = server_with_pair("first-key", &signer, 2);
        let config = trusted_config(&server);

        resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        let pin = load_automatic_pin(server.authority()).unwrap().unwrap();
        assert_eq!(pin.key_id, "first-key");
        assert_eq!(pin.public_key, hex::encode(signer.public_key()));
        assert_eq!(server.requests(), [KEY_PATH, DESCRIPTOR_PATH]);

        resolve_and_verify_endpoint_descriptor(server.authority(), &config)
            .await
            .unwrap();
        assert_eq!(
            server.requests(),
            [KEY_PATH, DESCRIPTOR_PATH, DESCRIPTOR_PATH]
        );
    })
    .await;
}

#[tokio::test]
async fn tls_authenticates_first_contact_and_configured_ca_allows_it() {
    with_isolated_home_async(|_| async {
        let signer = Ed25519Signer::generate().unwrap();
        let server = server_with_pair("tls-key", &signer, 1);
        let untrusted_error =
            resolve_and_verify_endpoint_descriptor(server.authority(), &Default::default())
                .await
                .unwrap_err();
        assert!(untrusted_error.to_string().contains("HTTPS request failed"));
        assert!(load_automatic_pin(server.authority()).unwrap().is_none());

        resolve_and_verify_endpoint_descriptor(server.authority(), &trusted_config(&server))
            .await
            .unwrap();
        assert!(load_automatic_pin(server.authority()).unwrap().is_some());
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
        let candidate = Ed25519Signer::generate().unwrap();
        let descriptor_signer = Ed25519Signer::generate().unwrap();
        let server = TestHttpsServer::start(routes_for_pair(
            "candidate",
            &candidate,
            &descriptor_signer,
            1,
        ));
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
async fn pin_change_refuses_new_id_and_reused_id_without_state_mutation() {
    for reuse_id in [false, true] {
        with_isolated_home_async(|_| async move {
            let first = Ed25519Signer::generate().unwrap();
            let changed = Ed25519Signer::generate().unwrap();
            let changed_id = if reuse_id { "stable-id" } else { "new-id" };
            let server = TestHttpsServer::start(HashMap::from([
                (
                    KEY_PATH.to_string(),
                    VecDeque::from([TestResponse::json(key_document(
                        1,
                        "stable-id",
                        first.public_key(),
                    ))]),
                ),
                (
                    DESCRIPTOR_PATH.to_string(),
                    VecDeque::from([
                        TestResponse::protobuf(signed_descriptor("stable-id", &first)),
                        TestResponse::protobuf(signed_descriptor(changed_id, &changed)),
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
            .expect("pin change must fail before an Iroh dial can stall")
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Automatic re-pinning was refused")
            );
            assert_eq!(fs::read(super::descriptor_trust_path()).unwrap(), before);
            assert_eq!(
                server.requests(),
                [KEY_PATH, DESCRIPTOR_PATH, DESCRIPTOR_PATH]
            );
        })
        .await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn trust_store_write_failure_prevents_iroh_and_iroh_failure_keeps_pin() {
    use std::os::unix::fs::PermissionsExt;

    with_isolated_home_async(|_| async {
        let signer = Ed25519Signer::generate().unwrap();
        let server = server_with_pair("write-key", &signer, 1);
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
        assert!(!crate::credentials::credentials_path().exists());
        assert_eq!(server.requests(), [KEY_PATH, DESCRIPTOR_PATH]);
    })
    .await;

    with_isolated_home_async(|_| async {
        let signer = Ed25519Signer::generate().unwrap();
        let server = server_with_pair("dial-key", &signer, 1);
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
        assert!(!crate::credentials::credentials_path().exists());
        assert_eq!(server.requests(), [KEY_PATH, DESCRIPTOR_PATH]);
    })
    .await;
}

#[tokio::test]
async fn explicit_pair_skips_discovery_and_old_server_failure_is_actionable() {
    with_isolated_home_async(|_| async {
        let signer = Ed25519Signer::generate().unwrap();
        let server = TestHttpsServer::start(HashMap::from([(
            DESCRIPTOR_PATH.to_string(),
            VecDeque::from([TestResponse::protobuf(signed_descriptor(
                "explicit-id",
                &signer,
            ))]),
        )]));
        let public_key: [u8; 32] = signer.public_key().try_into().unwrap();
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

fn server_with_pair(
    key_id: &str,
    signer: &Ed25519Signer,
    descriptor_count: usize,
) -> TestHttpsServer {
    TestHttpsServer::start(routes_for_pair(key_id, signer, signer, descriptor_count))
}

fn routes_for_pair(
    key_id: &str,
    published_signer: &Ed25519Signer,
    descriptor_signer: &Ed25519Signer,
    descriptor_count: usize,
) -> HashMap<String, VecDeque<TestResponse>> {
    HashMap::from([
        (
            KEY_PATH.to_string(),
            VecDeque::from([TestResponse::json(key_document(
                1,
                key_id,
                published_signer.public_key(),
            ))]),
        ),
        (
            DESCRIPTOR_PATH.to_string(),
            (0..descriptor_count)
                .map(|_| TestResponse::protobuf(signed_descriptor(key_id, descriptor_signer)))
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

fn signed_descriptor(key_id: &str, signer: &Ed25519Signer) -> Vec<u8> {
    let now = chrono::Utc::now().timestamp_millis();
    let descriptor = EndpointDescriptor {
        version: 1,
        endpoint_id: hex::encode([9; 32]),
        relay_urls: Vec::new(),
        direct_addresses: vec!["127.0.0.1:9".to_string()],
        supported_alpns: vec![HOSTED_ALPN_V1.to_vec()],
        issued_at_unix_millis: now - 1_000,
        expires_at_unix_millis: now + 60_000,
        rotation: None,
    };
    SignedEndpointDescriptor {
        signature: signer
            .sign(&endpoint_descriptor_bytes(&descriptor))
            .unwrap(),
        descriptor: Some(descriptor),
        key_id: key_id.to_string(),
    }
    .encode_to_vec()
}

fn trusted_config(server: &TestHttpsServer) -> cli_shared::ClientConfig {
    cli_shared::ClientConfig::default()
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
    let _guard = crate::credentials::lock_test_env();
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
