// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "client")]

#[path = "support/native_hosted_server.rs"]
mod native_hosted_server;

use std::{fs, process::Command};

use api::heddle::api::v1alpha2 as v2;
use crypto::{Ed25519Signer, Signer};
use heddle_biscuit_verifier::signature_v1::BiscuitBuilderV1Ext as _;
use hosted_client::hosted_runtime::hosted::HostedClient;
use prost::Message;

struct Fixture {
    temp: tempfile::TempDir,
    https: std::sync::Arc<native_hosted_server::https::TestHttpsServer>,
    server: tokio::task::JoinHandle<()>,
    client: HostedClient,
    captured: std::sync::Arc<std::sync::Mutex<native_hosted_server::PublicationCapture>>,
    operation: v2::RecordRef,
    root_key: String,
}

impl Fixture {
    async fn new() -> Self {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../thread-api/tests/fixtures/hybrid-alpha33.json"
        ))
        .expect("import wire vectors");
        let bytes = hex::decode(
            vectors["wire_vectors"]["commit_public_source"]["wire_hex"]
                .as_str()
                .expect("wire"),
        )
        .expect("hex");
        let mut request =
            v2::CommitImportJobRequest::decode(bytes.as_slice()).expect("public import request");
        request.client_operation_id = uuid::Uuid::new_v4().to_string();
        let spool = request
            .destination
            .as_ref()
            .expect("destination")
            .id
            .parse()
            .expect("spool UUID");
        let (unused_client, server, captured, _, _, https) =
            native_hosted_server::start_routed(spool, "main", [7; 32]).await;
        unused_client.close().await;
        let temp = tempfile::tempdir().expect("fixture home");
        let root_key = hex::encode(
            Ed25519Signer::from_seed(&[7; 32])
                .expect("descriptor root")
                .public_key(),
        );
        fs::write(temp.path().join("ca.pem"), &https.certificate_pem).expect("CA");
        let (token, signer) = credential(uuid::Uuid::from_u128(2));
        let config = config::ClientConfig::default()
            .with_token(wire::AuthToken::new(
                token,
                uuid::Uuid::from_u128(2).to_string(),
            ))
            .with_auth_proof_key_pem(signer.to_pem().expect("PEM"))
            .with_authenticated_principal(uuid::Uuid::from_u128(2).to_string())
            .with_descriptor_trust(
                "clone-test-key",
                hex::decode(&root_key)
                    .expect("root")
                    .try_into()
                    .expect("key"),
            )
            .with_tls_ca_certificate_pem(https.certificate_pem.clone());
        let client = HostedClient::connect_server(&https.authority, &config)
            .await
            .expect("owner client");
        let response: v2::MutationResponse = client
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/CommitImportJob",
                &request,
            )
            .await
            .expect("start hosted import");
        let Some(v2::mutation_receipt::Outcome::PendingOperation(operation)) =
            response.receipt.expect("receipt").outcome
        else {
            panic!("pending operation")
        };
        Self {
            temp,
            https,
            server,
            client,
            captured,
            operation,
            root_key,
        }
    }

    fn run(&self, caller: uuid::Uuid, args: &[&str]) -> std::process::Output {
        let home = tempfile::tempdir().expect("fresh HEDDLE_HOME");
        let (token, signer) = credential(caller);
        let credential = home.path().join("caller.hcred");
        fs::write(
            &credential,
            serde_json::to_vec(&serde_json::json!({
                "format": "heddle-credential", "version": 1, "server": self.https.authority,
                "kind": "device", "subject": caller.to_string(), "token": token,
                "proof_key_pem": signer.to_pem().expect("proof PEM"), "credential_id": null,
            }))
            .expect("credential JSON"),
        )
        .expect("credential file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&credential, fs::Permissions::from_mode(0o600))
                .expect("private credential");
        }
        Command::new(env!("CARGO_BIN_EXE_heddle"))
            .current_dir(self.temp.path())
            .env("HEDDLE_HOME", home.path())
            .env("HEDDLE_CREDENTIAL", credential)
            .env("HTTPS_PROXY", &self.https.proxy_uri)
            .env("HEDDLE_REMOTE_TLS_CA_CERT", self.temp.path().join("ca.pem"))
            .env("HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID", "clone-test-key")
            .env("HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY", &self.root_key)
            .args(args)
            .args([
                "--to",
                &format!("https://{}/spool/acme", self.https.authority),
            ])
            .output()
            .expect("CLI process")
    }

    async fn close(self) {
        self.client.close().await;
        self.server.abort();
        let _ = self.server.await;
    }
}

fn credential(caller: uuid::Uuid) -> (String, Ed25519Signer) {
    let seed = if caller == uuid::Uuid::from_u128(2) {
        71
    } else {
        72
    };
    let signer = Ed25519Signer::from_seed(&[seed; 32]).expect("caller device");
    let mint = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&[seed; 32], biscuit_auth::Algorithm::Ed25519)
            .expect("mint"),
    );
    let token = biscuit_auth::Biscuit::builder()
        .fact(format!("user(\"{caller}\")").as_str())
        .expect("account")
        .fact("subject_kind(\"user\")")
        .expect("subject kind")
        .fact(format!("subject_user_uuid(\"{caller}\")").as_str())
        .expect("account identity")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
        .expect("PoP")
        .fact("session(\"import-cancel-fixture\")")
        .expect("session")
        .fact("expires_at(2030-01-01T00:00:00Z)")
        .expect("expiry")
        .build_v1(&mint)
        .expect("credential")
        .to_base64()
        .expect("token");
    (token, signer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_cancel_reaches_cancelled_terminal_state() {
    for output_args in [
        vec!["--json"],
        vec!["--output", "json-compact"],
        vec!["--output", "text"],
    ] {
        let fixture = Fixture::new().await;
        let cancellation_id = uuid::Uuid::new_v4().to_string();
        let mut args = vec![
            "import",
            "cancel",
            &fixture.operation.id,
            "--op-id",
            &cancellation_id,
        ];
        args.extend(output_args.iter().copied());
        let output = fixture.run(uuid::Uuid::from_u128(2), &args);
        assert!(
            output.status.success(),
            "cancel: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if output_args.contains(&"text") {
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("cancellation requested"), "{text}");
            assert!(text.contains("heddle import status"), "{text}");
        } else {
            let document: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("finite cancel JSON");
            assert_eq!(document["output_kind"], "import_cancel");
            assert_eq!(document["status"], "requested");
            assert_eq!(document["operation_id"], fixture.operation.id);
        }
        {
            let capture = fixture.captured.lock().expect("capture");
            assert_eq!(capture.cancel_requests.len(), 1);
            assert!(uuid::Uuid::parse_str(&capture.cancel_requests[0].client_operation_id).is_ok());
            assert_ne!(
                capture.cancel_requests[0].client_operation_id,
                fixture.operation.id
            );
            assert_eq!(
                capture.cancel_requests[0].operation,
                Some(fixture.operation.clone())
            );
            assert_eq!(capture.cancel_requests[0].expected_version, vec![1; 32]);
            assert!(capture.import_jobs[0].record.cancellation_requested);
            assert_eq!(
                capture.import_jobs[0].record.state,
                v2::operation_record::State::Running as i32
            );
        }
        let status = fixture.run(
            uuid::Uuid::from_u128(2),
            &[
                "import",
                "status",
                &fixture.operation.id,
                "--output",
                "json",
            ],
        );
        // Status reports unsuccessful imports (including canceled ones) as 76.
        assert_eq!(
            status.status.code(),
            Some(76),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        let terminal: serde_json::Value =
            serde_json::from_slice(&status.stdout).expect("terminal operation JSON");
        assert_eq!(terminal["state"], "canceled");
        assert_eq!(terminal["terminal"], true);
        fixture.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_cancel_reports_server_refusal() {
    let fixture = Fixture::new().await;
    fixture.captured.lock().expect("jobs").import_jobs[0]
        .record
        .cancellation_supported = false;
    let output = fixture.run(
        uuid::Uuid::from_u128(2),
        &["import", "cancel", &fixture.operation.id],
    );
    assert_eq!(
        output.status.code(),
        Some(76),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("operation does not accept cancellation"),
        "{stderr}"
    );
    assert!(
        !fixture.captured.lock().expect("jobs").import_jobs[0]
            .record
            .cancellation_requested
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_cancel_denies_another_accounts_operation() {
    let fixture = Fixture::new().await;
    let output = fixture.run(
        uuid::Uuid::from_u128(3),
        &["import", "cancel", &fixture.operation.id, "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(77),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).expect("JSON denial");
    assert!(
        error
            .to_string()
            .contains("caller is not allowed to cancel this operation"),
        "{error}"
    );
    {
        let capture = fixture.captured.lock().expect("jobs");
        assert_eq!(
            capture.cancel_requests.len(),
            1,
            "server must enforce cancel authority"
        );
        assert!(!capture.import_jobs[0].record.cancellation_requested);
    }
    fixture.close().await;
}
