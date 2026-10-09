//! heddle#1748: netd's warm Weft session must never carry credentials to an
//! endpoint the caller's configured descriptor/TLS trust did not verify.
//!
//! Every fixture runs two Weft endpoints: `weft`, attested by the descriptor
//! root served over HTTPS, and `rogue`, which is not. netd's warm session is
//! pointed at one of them, standing in for a daemon that bootstrapped with its
//! own default (automatic/TOFU) trust.

use std::{
    collections::{HashMap, VecDeque},
    net::Ipv4Addr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use api::{
    descriptor_trust::{
        AttestedEndpointDescriptorEntry, EndpointDescriptorSetDocument, ephemeral_attestation_bytes,
    },
    heddle::api::{
        common::{EndpointDescriptor, SignedEndpointDescriptor},
        v1alpha2::{DescribeEndpointResponse, EndpointKind, EndpointRef},
    },
    signing::endpoint_descriptor_bytes,
};
use biscuit_verifier::signature_v1::BiscuitBuilderV1Ext as _;
use config::ClientConfig;
use crypto::{Ed25519Signer, Signer};
use iroh::{Endpoint, RelayMode, SecretKey, endpoint::presets};
use prost::Message;

use super::{
    HostedClient,
    hosted_bridge::{HostedBridge, hosted_bridge_socket_path, tests::PinHeddleHome},
    test_https::{TestHttpsServer, TestResponse},
};

const DESCRIPTOR_PATH: &str = "/.well-known/heddle/iroh-endpoint";

/// A Weft endpoint answering every stream with `DescribeEndpoint`, counting
/// calls and the calls that carried a bearer capability or request proof.
struct TestWeft {
    endpoint: Endpoint,
    signer: Ed25519Signer,
    calls: Arc<AtomicUsize>,
    authenticated: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl TestWeft {
    async fn start() -> Self {
        let secret = SecretKey::generate();
        let signer = Ed25519Signer::from_seed(&secret.to_bytes()).expect("endpoint signer");
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("Weft address")
            .bind()
            .await
            .expect("Weft endpoint");
        let calls = Arc::new(AtomicUsize::new(0));
        let authenticated = Arc::new(AtomicUsize::new(0));
        let accept_endpoint = endpoint.clone();
        let call_counter = calls.clone();
        let auth_counter = authenticated.clone();
        let task = tokio::spawn(async move {
            while let Some(incoming) = accept_endpoint.accept().await {
                let Ok(connection) = incoming.await else {
                    continue;
                };
                let call_counter = call_counter.clone();
                let auth_counter = auth_counter.clone();
                let key = accept_endpoint.id().as_bytes().to_vec();
                tokio::spawn(async move {
                    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                        let bytes = recv.read_to_end(64 * 1024).await.expect("RPC request");
                        let frame =
                            api::framing::decode_request_frame(&bytes).expect("request frame");
                        call_counter.fetch_add(1, Ordering::SeqCst);
                        if !frame.context.bearer_capability.is_empty()
                            || frame.context.request_proof.is_some()
                        {
                            auth_counter.fetch_add(1, Ordering::SeqCst);
                        }
                        let description = DescribeEndpointResponse {
                            endpoint: Some(EndpointRef {
                                kind: EndpointKind::Weft as i32,
                                public_key: key.clone(),
                            }),
                            supported_packages: vec!["heddle.api.v1alpha2".into()],
                            implemented_methods: vec![
                                "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint".into(),
                            ],
                            ..Default::default()
                        };
                        let response =
                            api::framing::encode_success_response(&description.encode_to_vec())
                                .expect("response");
                        send.write_all(&response).await.expect("RPC response");
                        send.finish().expect("finish response");
                    }
                });
            }
        });
        Self {
            endpoint,
            signer,
            calls,
            authenticated,
            task,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn authenticated(&self) -> usize {
        self.authenticated.load(Ordering::SeqCst)
    }

    async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
        self.endpoint.close().await;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NetdWarm {
    /// netd holds a session to the root-attested Weft.
    Attested,
    /// netd holds a session to an endpoint the caller's root never attested.
    Rogue,
}

struct Fixture {
    home: tempfile::TempDir,
    https: TestHttpsServer,
    /// Pinned root + custom CA for the test HTTPS server.
    config: ClientConfig,
    weft: TestWeft,
    rogue: TestWeft,
    netd: Endpoint,
    bridge_task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(warm: NetdWarm) -> Self {
        Self::start_with_entries(warm, false).await
    }

    /// `extra_first_entry` puts another root-attested endpoint first in the
    /// live set, so the caller's own preferred pick is not netd's endpoint.
    async fn start_with_entries(warm: NetdWarm, extra_first_entry: bool) -> Self {
        let root = Ed25519Signer::generate().expect("root signer");
        let weft = TestWeft::start().await;
        let rogue = TestWeft::start().await;
        let mut entries = vec![attested_entry(&root, &weft.signer, &weft.endpoint.addr())];
        if extra_first_entry {
            entries.insert(
                0,
                attested_entry(
                    &root,
                    &Ed25519Signer::generate().expect("another attested endpoint"),
                    &weft.endpoint.addr(),
                ),
            );
        }
        let document = serde_json::to_vec(&EndpointDescriptorSetDocument {
            version: 1,
            root_key_id: "root".into(),
            entries,
        })
        .expect("descriptor set");
        let https = TestHttpsServer::start(HashMap::from([(
            DESCRIPTOR_PATH.into(),
            VecDeque::from(vec![TestResponse::json(document); 8]),
        )]));
        let bearer = biscuit_auth::Biscuit::builder()
            .fact("user(\"netd-trust-test\")")
            .expect("user fact")
            .build_v1(&biscuit_auth::KeyPair::new())
            .expect("bearer")
            .to_base64()
            .expect("encoded bearer");
        let config = ClientConfig::default()
            .with_descriptor_trust("root", root.public_key().try_into().expect("root key"))
            .with_tls_ca_certificate_pem(https.certificate_pem())
            .with_token(wire::AuthToken::new(bearer, "test"))
            .with_auth_proof_key_pem(hex::encode([42; 32]))
            .with_authenticated_principal("principal:netd-trust-test")
            .with_server_key(https.authority());

        let netd = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("netd address")
            .bind()
            .await
            .expect("netd endpoint");
        let target = match warm {
            NetdWarm::Attested => weft.endpoint.addr(),
            NetdWarm::Rogue => rogue.endpoint.addr(),
        };
        let connection = netd
            .connect(target, api::HOSTED_ALPN_V1)
            .await
            .expect("warm session");
        let home = tempfile::TempDir::new().expect("home");
        let socket = hosted_bridge_socket_path(home.path());
        std::fs::create_dir_all(socket.parent().expect("socket parent")).expect("state dir");
        let bridge = HostedBridge::new(netd.clone());
        bridge
            .insert_weft_for_test(https.authority(), connection)
            .await;
        let bridge_socket = socket.clone();
        let bridge_task = tokio::spawn(async move {
            bridge.serve(bridge_socket).await.expect("bridge");
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("bridge socket");
        Self {
            home,
            https,
            config,
            weft,
            rogue,
            netd,
            bridge_task,
        }
    }

    fn default_trust_config(&self) -> ClientConfig {
        let mut config = self.config.clone();
        config.descriptor_key_id = None;
        config.descriptor_public_key = None;
        config.tls_ca_certificate_pem = None;
        config
    }

    /// A CLI session built from user config, optionally pinning the root and
    /// the test server's CA exactly as `[remote]` settings would.
    fn session(&self, pinned: bool) -> super::HostedSession {
        let mut user_config = config::UserConfig::default();
        if pinned {
            let root_path = self.home.path().join("root.pub");
            let ca_path = self.home.path().join("ca.pem");
            std::fs::write(
                &root_path,
                hex::encode(self.config.descriptor_public_key.expect("pin")),
            )
            .expect("root file");
            std::fs::write(&ca_path, self.https.certificate_pem()).expect("CA file");
            user_config.remote.iroh_descriptor_key_id = Some("root".into());
            user_config.remote.iroh_descriptor_public_key_path = Some(root_path);
            user_config.remote.tls_ca_certificate_path = Some(ca_path);
        }
        super::HostedSession::build(
            &user_config,
            Some(self.https.authority().into()),
            super::HostedAuthMode::PresentedDevice {
                token: self.config.token.as_ref().expect("bearer").id.clone(),
                proof_key_pem: self.config.auth_proof_key_pem.clone().expect("proof key"),
                subject: "netd-trust-test".into(),
            },
        )
        .expect("session")
    }

    fn automatic_pin_stored(&self) -> bool {
        super::descriptor_trust::load_automatic_pin(self.https.authority())
            .expect("pin store")
            .is_some()
    }

    async fn close(self) {
        self.bridge_task.abort();
        let _ = self.bridge_task.await;
        self.netd.close().await;
        self.weft.close().await;
        self.rogue.close().await;
    }
}

/// The issue's exploit shape end to end: a pinned CLI session, netd warm to an
/// endpoint the pinned root never attested. The session must not send a
/// single request to that endpoint; it bypasses netd and dials the endpoint
/// its own pinned trust verified.
#[tokio::test]
async fn pinned_session_never_sends_credentials_to_unverified_netd_weft() {
    let _env_guard = crate::test_process_env::exclusive().await;
    let fixture = Fixture::start(NetdWarm::Rogue).await;
    let _home = PinHeddleHome::new(fixture.home.path());
    let session = fixture.session(true);
    for outbound in [false, true] {
        let result = if outbound {
            session.connect_outbound(fixture.https.authority()).await
        } else {
            session.connect(fixture.https.authority()).await
        };
        let reused_warm = result
            .as_ref()
            .map(HostedClient::reused_warm_connection)
            .ok();
        println!(
            "NETD_PIN outbound={outbound} connected={} reused_warm={reused_warm:?} \
             rogue_calls={} rogue_authenticated_calls={} attested_authenticated_calls={}",
            result.is_ok(),
            fixture.rogue.calls(),
            fixture.rogue.authenticated(),
            fixture.weft.authenticated(),
        );
        assert_eq!(
            fixture.rogue.authenticated(),
            0,
            "credentials reached a Weft endpoint outside the caller's pinned root (outbound={outbound})"
        );
        assert_eq!(fixture.rogue.calls(), 0, "no request may reach it at all");
        let client = result.expect("pinned session connects to the verified endpoint directly");
        assert!(!client.reused_warm_connection(), "netd must be bypassed");
        client.close().await;
    }
    assert_eq!(fixture.weft.authenticated(), 2);
    assert!(
        !fixture.automatic_pin_stored(),
        "an explicit pin never falls back to automatic (TOFU) trust"
    );
    fixture.close().await;
}

#[tokio::test]
async fn pinned_session_reuses_netd_when_it_holds_the_attested_weft() {
    let _env_guard = crate::test_process_env::exclusive().await;
    let fixture = Fixture::start(NetdWarm::Attested).await;
    let _home = PinHeddleHome::new(fixture.home.path());
    let session = fixture.session(true);
    for outbound in [false, true] {
        let client = if outbound {
            session.connect_outbound(fixture.https.authority()).await
        } else {
            session.connect(fixture.https.authority()).await
        }
        .expect("matching pinned identity through netd");
        assert!(client.reused_warm_connection(), "outbound={outbound}");
        client.close().await;
    }
    assert_eq!(fixture.weft.authenticated(), 2);
    assert_eq!(fixture.https.requests(), [DESCRIPTOR_PATH, DESCRIPTOR_PATH]);
    assert!(!fixture.automatic_pin_stored());
    fixture.close().await;
}

/// No pin, CA, or TLS name: netd's own default trust is the caller's default
/// trust, so the warm route is used exactly as before, with no client-side
/// descriptor fetch.
#[tokio::test]
async fn default_trust_session_keeps_the_netd_warm_path() {
    let _env_guard = crate::test_process_env::exclusive().await;
    let fixture = Fixture::start(NetdWarm::Rogue).await;
    let _home = PinHeddleHome::new(fixture.home.path());
    let session = fixture.session(false);
    let client = session
        .connect(fixture.https.authority())
        .await
        .expect("default trust warm route");
    assert!(client.reused_warm_connection());
    client.close().await;
    assert!(fixture.https.requests().is_empty());
    assert_eq!(fixture.rogue.authenticated(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn netd_pinned_identity_mismatch_rejected_before_any_request() {
    let _env_guard = crate::test_process_env::exclusive().await;
    let fixture = Fixture::start(NetdWarm::Rogue).await;
    let _home = PinHeddleHome::new(fixture.home.path());
    let error = netd_rejection(
        fixture.https.authority(),
        &fixture.config,
        "netd endpoint outside the pinned root is rejected",
    )
    .await;
    assert!(
        matches!(error, super::HostedError::DescriptorTrust(_)),
        "{error}"
    );
    assert_eq!(fixture.rogue.calls(), 0, "reject before any RPC stream");
    assert_eq!(fixture.weft.calls(), 0);
    fixture.close().await;
}

#[tokio::test]
async fn netd_matching_pin_honours_custom_ca_and_reuses_warm_session() {
    let _env_guard = crate::test_process_env::exclusive().await;
    let fixture = Fixture::start(NetdWarm::Attested).await;
    let _home = PinHeddleHome::new(fixture.home.path());
    for _ in 0..2 {
        let client = HostedClient::connect_via_netd(
            fixture.https.authority(),
            &fixture.config,
            None,
            &crate::hosted_runtime::hosted::BootstrapHttp::new(&fixture.config),
        )
        .await
        .expect("matching pinned route");
        assert!(client.reused_warm_connection());
        client.close().await;
    }
    assert_eq!(fixture.weft.authenticated(), 2);
    assert_eq!(fixture.https.requests(), [DESCRIPTOR_PATH, DESCRIPTOR_PATH]);
    assert!(!fixture.automatic_pin_stored());
    fixture.close().await;
}

#[tokio::test]
async fn netd_pin_accepts_any_attested_live_endpoint_in_the_set() {
    let _env_guard = crate::test_process_env::exclusive().await;
    let fixture = Fixture::start_with_entries(NetdWarm::Attested, true).await;
    let _home = PinHeddleHome::new(fixture.home.path());
    let preferred = super::resolver::resolve_and_verify_endpoint_descriptor(
        fixture.https.authority(),
        &fixture.config,
        &crate::hosted_runtime::hosted::BootstrapHttp::new(&fixture.config),
    )
    .await
    .expect("first verified endpoint");
    assert_ne!(
        preferred.endpoint_addr().expect("preferred address").id,
        fixture.weft.endpoint.id()
    );
    let client = HostedClient::connect_via_netd(
        fixture.https.authority(),
        &fixture.config,
        None,
        &crate::hosted_runtime::hosted::BootstrapHttp::new(&fixture.config),
    )
    .await
    .expect("warm endpoint is attested even though it is not first");
    assert!(client.reused_warm_connection());
    assert_eq!(fixture.weft.authenticated(), 1);
    client.close().await;
    fixture.close().await;
}

/// `connect_with_config` already holds a verified descriptor: netd is used
/// only for that exact identity, otherwise the descriptor is dialed directly.
/// It is never rediscovered and never replaced by netd's choice.
#[tokio::test]
async fn netd_preserves_an_already_verified_descriptor() {
    let _env_guard = crate::test_process_env::exclusive().await;
    for warm in [NetdWarm::Rogue, NetdWarm::Attested] {
        let fixture = Fixture::start(warm).await;
        let _home = PinHeddleHome::new(fixture.home.path());
        let descriptor = super::resolver::resolve_and_verify_endpoint_descriptor(
            fixture.https.authority(),
            &fixture.config,
            &crate::hosted_runtime::hosted::BootstrapHttp::new(&fixture.config),
        )
        .await
        .expect("caller verified descriptor");
        for config in [fixture.default_trust_config(), fixture.config.clone()] {
            let client = HostedClient::connect_with_config(&descriptor, &config)
                .await
                .expect("verified descriptor connects");
            assert_eq!(
                client.reused_warm_connection(),
                warm == NetdWarm::Attested,
                "{warm:?}"
            );
            client.close().await;
        }
        assert_eq!(fixture.rogue.calls(), 0, "{warm:?}");
        assert_eq!(fixture.weft.authenticated(), 2, "{warm:?}");
        assert_eq!(
            fixture.https.requests(),
            [DESCRIPTOR_PATH],
            "no descriptor rediscovery"
        );
        fixture.close().await;
    }
}

#[tokio::test]
async fn netd_rejects_invalid_root_attestation_and_partial_pins_without_tofu() {
    let _env_guard = crate::test_process_env::exclusive().await;
    let fixture = Fixture::start(NetdWarm::Attested).await;
    let _home = PinHeddleHome::new(fixture.home.path());
    let mut wrong_root = fixture.config.clone();
    wrong_root.descriptor_public_key = Some(
        Ed25519Signer::generate()
            .expect("other root")
            .public_key()
            .try_into()
            .expect("root key"),
    );
    let error = netd_rejection(
        fixture.https.authority(),
        &wrong_root,
        "wrong root rejected",
    )
    .await;
    assert!(
        matches!(error, super::HostedError::InvalidDescriptorSignature),
        "{error}"
    );
    for public_key_only in [false, true] {
        let mut partial = fixture.config.clone();
        if public_key_only {
            partial.descriptor_key_id = None;
        } else {
            partial.descriptor_public_key = None;
        }
        let error =
            netd_rejection(fixture.https.authority(), &partial, "partial pin rejected").await;
        assert!(
            matches!(error, super::HostedError::DescriptorTrust(_)),
            "{error}"
        );
    }
    assert_eq!(fixture.weft.calls(), 0);
    assert_eq!(fixture.https.requests(), [DESCRIPTOR_PATH]);
    assert!(!fixture.automatic_pin_stored());
    fixture.close().await;
}

/// A TLS name override and custom CA are applied to the client's own
/// descriptor fetch before netd's route is accepted; an invalid policy fails
/// closed without sending credentials.
#[tokio::test]
async fn netd_honours_tls_name_and_rejects_invalid_tls_policy() {
    let _env_guard = crate::test_process_env::exclusive().await;
    let fixture = Fixture::start(NetdWarm::Attested).await;
    let _home = PinHeddleHome::new(fixture.home.path());
    let config = fixture.config.clone().with_tls_domain_name("127.0.0.1");
    let client = HostedClient::connect_via_netd(
        fixture.https.authority(),
        &config,
        None,
        &crate::hosted_runtime::hosted::BootstrapHttp::new(&config),
    )
    .await
    .expect("matching TLS name");
    assert!(client.reused_warm_connection());
    client.close().await;
    for invalid in [
        fixture
            .config
            .clone()
            .with_tls_ca_certificate_pem("invalid certificate"),
        fixture
            .config
            .clone()
            .with_tls_domain_name("wrong-name.example"),
    ] {
        let error =
            netd_rejection(fixture.https.authority(), &invalid, "TLS policy rejected").await;
        assert!(
            matches!(
                error,
                super::HostedError::BootstrapHttp(_) | super::HostedError::InvalidDescriptor(_)
            ),
            "{error}"
        );
    }
    assert_eq!(
        fixture.weft.authenticated(),
        1,
        "invalid TLS policy must not send credentials"
    );
    assert_eq!(fixture.https.requests(), [DESCRIPTOR_PATH]);
    fixture.close().await;
}

/// The error from a netd route that must be refused; an accepted route fails
/// the test.
async fn netd_rejection(
    server: &str,
    config: &ClientConfig,
    expectation: &str,
) -> super::HostedError {
    match HostedClient::connect_via_netd(
        server,
        config,
        None,
        &crate::hosted_runtime::hosted::BootstrapHttp::new(config),
    )
    .await
    {
        Ok(client) => {
            client.close().await;
            panic!("netd route accepted: {expectation}");
        }
        Err(error) => error,
    }
}

fn attested_entry(
    root: &Ed25519Signer,
    attested: &Ed25519Signer,
    address: &iroh::EndpointAddr,
) -> AttestedEndpointDescriptorEntry {
    let now = chrono::Utc::now().timestamp_millis();
    let descriptor = EndpointDescriptor {
        version: 1,
        endpoint_id: hex::encode(attested.public_key()),
        direct_addresses: address.ip_addrs().map(ToString::to_string).collect(),
        supported_alpns: vec![api::HOSTED_ALPN_V1.to_vec()],
        issued_at_unix_millis: now - 1_000,
        expires_at_unix_millis: now + 60_000,
        ..Default::default()
    };
    let signed = SignedEndpointDescriptor {
        signature: attested
            .sign(&endpoint_descriptor_bytes(&descriptor))
            .expect("descriptor signature"),
        descriptor: Some(descriptor),
        key_id: "endpoint".into(),
    };
    AttestedEndpointDescriptorEntry {
        ephemeral_key_id: "endpoint".into(),
        ephemeral_public_key: hex::encode(attested.public_key()),
        not_before_unix_millis: now - 1_000,
        not_after_unix_millis: now + 60_000,
        region: "test".into(),
        attestation_signature: hex::encode(
            root.sign(&ephemeral_attestation_bytes(
                "endpoint",
                &attested.public_key().try_into().expect("endpoint key"),
                now - 1_000,
                now + 60_000,
                "test",
            ))
            .expect("root attestation"),
        ),
        signed_descriptor: hex::encode(signed.encode_to_vec()),
    }
}
