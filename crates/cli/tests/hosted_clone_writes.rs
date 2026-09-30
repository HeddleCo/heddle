// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "client")]

#[path = "support/native_hosted_https.rs"]
mod native_hosted_https;
#[path = "support/native_hosted_server.rs"]
mod native_hosted_server;
mod support;

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
};

use crypto::{Ed25519Signer, Signer};
use heddle_biscuit_verifier::signature_v1::BiscuitBuilderV1Ext as _;
#[cfg(all(feature = "ci", feature = "preview"))]
use objects::object::StateId;
use support::*;

struct Fixture {
    _temp: TempDir,
    https: native_hosted_https::TestHttpsServer,
    server: tokio::task::JoinHandle<()>,
    client: hosted_client::hosted_runtime::hosted::HostedClient,
    captured: std::sync::Arc<std::sync::Mutex<native_hosted_server::PublicationCapture>>,
    clone: PathBuf,
    home: PathBuf,
    ca: PathBuf,
    credential: PathBuf,
    root_key: String,
    spool: uuid::Uuid,
    #[cfg(all(feature = "ci", feature = "preview"))]
    state: StateId,
    thread_id: objects::object::ContentHash,
    genesis: crypto::thread_operation::SignedGenesis,
}

impl Fixture {
    async fn new() -> Self {
        let temp = TempDir::new().expect("fixture");
        let source = temp.path().join("source");
        let source_home = temp.path().join("source-home");
        std::fs::create_dir_all(&source).expect("source");
        let envs = [("HEDDLE_HOME", source_home.to_str().expect("source home"))];
        heddle_env(&["init"], Some(&source), &envs).expect("native source init");
        std::fs::write(source.join("story.txt"), "source only\n").expect("source file");
        heddle_env(&["capture", "-m", "source seed"], Some(&source), &envs)
            .expect("source capture");
        let repo = Repository::open(&source).expect("source repo");
        let state = repo.head().expect("source HEAD").expect("source state");
        let native = repo.native_thread("main").expect("source identity");
        let spool = native
            .genesis()
            .expect("genesis")
            .spool
            .parse()
            .expect("spool");
        let thread_id = native.thread_id();
        let (mut client, server, captured, addr, secret) =
            native_hosted_server::start_routed(spool, "main", *thread_id.as_bytes()).await;
        assert!(
            client
                .push_profiled(&repo, "spool/acme", state, "main", false, "seed".into())
                .await
                .expect("seed hosted source")
                .0
                .success
        );
        let root = Ed25519Signer::generate().expect("descriptor root");
        let ephemeral = Ed25519Signer::from_seed(&secret.to_bytes()).expect("endpoint signer");
        let direct = addr.ip_addrs().next().expect("direct address").to_string();
        let descriptor = native_hosted_https::signed_descriptor(
            &addr.id.to_string(),
            &direct,
            &root,
            &ephemeral,
        );
        let https = native_hosted_https::TestHttpsServer::start(HashMap::from([(
            "/.well-known/heddle/iroh-endpoint".into(),
            VecDeque::from(vec![descriptor; 32]),
        )]));
        let ca = temp.path().join("ca.pem");
        std::fs::write(&ca, &https.certificate_pem).expect("test CA");
        let home = temp.path().join("clone-home");
        std::fs::create_dir(&home).expect("clone home");
        // This unclaimed native fixture belongs to one device. Retain that
        // device's key in a disjoint home; clone must carry no private keys.
        let signer = repo
            .native_thread_signer(&native)
            .expect("source owner key");
        let device = repo::identity::DeviceIdentity {
            public_key: hex::encode(signer.public_key()),
            private_key_pem: signer.to_pem().expect("device PEM"),
            server: format!("https://{}", https.authority),
            linked_at: "2026-09-30T00:00:00Z".into(),
            credential_token: None,
            credential_subject: None,
        };
        std::fs::write(
            home.join(repo::identity::DEVICE_IDENTITY_FILE),
            toml::to_string(&device).expect("device"),
        )
        .expect("retain device");
        let proof_signer = Ed25519Signer::from_seed(&[71; 32]).expect("credential signer");
        let mint = biscuit_auth::KeyPair::from(
            &biscuit_auth::PrivateKey::from_bytes(&[71; 32], biscuit_auth::Algorithm::Ed25519)
                .expect("mint key"),
        );
        let token = biscuit_auth::Biscuit::builder()
            .fact(r#"user("clone-test")"#)
            .expect("subject")
            .fact(
                format!(
                    "device_pop_key(\"{}\")",
                    hex::encode(proof_signer.public_key())
                )
                .as_str(),
            )
            .expect("proof key")
            .fact(r#"session("clone-test-session")"#)
            .expect("session")
            .fact("expires_at(2030-01-01T00:00:00Z)")
            .expect("expiry")
            .build_v1(&mint)
            .expect("credential")
            .to_base64()
            .expect("token");
        let credential = temp.path().join("test.hcred");
        std::fs::write(
            &credential,
            serde_json::to_vec(&serde_json::json!({
                "format": "heddle-credential", "version": 1, "server": https.authority,
                "kind": "device", "subject": "clone-test", "token": token,
                "proof_key_pem": proof_signer.to_pem().expect("proof PEM"), "credential_id": null,
            }))
            .expect("credential JSON"),
        )
        .expect("credential file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [
                &credential,
                &home.join(repo::identity::DEVICE_IDENTITY_FILE),
            ] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .expect("private keys");
            }
        }
        let fixture = Self {
            clone: temp.path().join("clone"),
            root_key: hex::encode(root.public_key()),
            _temp: temp,
            https,
            server,
            client,
            captured,
            home,
            ca,
            credential,
            spool,
            #[cfg(all(feature = "ci", feature = "preview"))]
            state,
            thread_id,
            genesis: native.signed_genesis().expect("original genesis"),
        };
        fixture.run_at(
            fixture._temp.path(),
            &[
                "clone",
                &fixture.remote(),
                fixture.clone.to_str().expect("clone path"),
            ],
        );
        fixture
    }

    fn remote(&self) -> String {
        format!("https://{}/spool/acme", self.https.authority)
    }

    fn output_at(&self, path: &Path, args: &[&str]) -> Output {
        heddle_output_env(
            args,
            Some(path),
            &[
                ("HEDDLE_HOME", self.home.to_str().expect("home")),
                ("HEDDLE_REMOTE_TLS_CA_CERT", self.ca.to_str().expect("CA")),
                ("HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID", "clone-test-key"),
                ("HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY", &self.root_key),
                (
                    "HEDDLE_CREDENTIAL",
                    self.credential.to_str().expect("credential"),
                ),
            ],
        )
        .expect("CLI process")
    }
    fn run_at(&self, path: &Path, args: &[&str]) -> String {
        let output = self.output_at(path, args);
        assert!(
            output.status.success(),
            "{args:?}: exit {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
    fn run(&self, args: &[&str]) -> String {
        self.run_at(&self.clone, args)
    }
    fn capture(&self) {
        std::fs::write(self.clone.join("story.txt"), "written from clone\n").expect("local edit");
        self.run(&["capture", "-m", "from clone"]);
    }
    fn assert_identity(&self) {
        let cloned = Repository::open(&self.clone).expect("clone repo");
        assert_eq!(cloned.native_spool_id().expect("spool"), self.spool);
        let thread = cloned.native_thread("main").expect("main");
        assert_eq!(thread.thread_id(), self.thread_id);
        assert_eq!(
            thread.signed_genesis().expect("signed genesis"),
            self.genesis
        );
    }
    #[cfg(all(feature = "ci", feature = "preview"))]
    fn configure_ci(&self) {
        let definition = ci_config::definition(
            "clone",
            "clone",
            vec![ci_config::argv_check(
                "ok",
                "/bin/sh",
                &["-c", "test -f story.txt"],
            )],
        );
        let (bytes, digest) = ci_config::canonical_definition(&definition).expect("definition");
        std::fs::write(
            self.clone
                .join(".heddle")
                .join(ci_config::DEFAULT_DEFINITION_FILE),
            bytes,
        )
        .expect("definition file");
        std::fs::write(
            self.clone
                .join(".heddle")
                .join(ci_config::DEFAULT_LOCK_FILE),
            ci_config::lock_json(&digest),
        )
        .expect("lock");
    }
    async fn close(self) {
        self.client.close().await;
        self.server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_clone_capture_push_main() {
    let fixture = Fixture::new().await;
    fixture.capture();
    println!("(a) {}", fixture.run(&["push", "origin"]));
    fixture.assert_identity();
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_clone_start_capture_push() {
    let fixture = Fixture::new().await;
    let path = fixture.run(&["start", "feature", "--print-cd-path"]);
    let feature = PathBuf::from(path.trim());
    std::fs::write(feature.join("story.txt"), "new thread from clone\n").expect("feature edit");
    fixture.run_at(&feature, &["capture", "-m", "feature capture"]);
    println!("(b) {}", fixture.run_at(&feature, &["push", "origin"]));
    fixture.assert_identity();
    let cloned = Repository::open(&feature).expect("feature checkout");
    let genesis = cloned
        .native_thread("feature")
        .expect("feature")
        .genesis()
        .expect("genesis");
    assert_eq!(genesis.spool, fixture.spool.to_string());
    assert_eq!(genesis.parent, Some(fixture.thread_id));
    fixture.close().await;
}

#[cfg(all(feature = "ci", feature = "preview"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_clone_ci_run_record() {
    let fixture = Fixture::new().await;
    fixture.configure_ci();
    println!("(c) {}", fixture.run(&["ci", "run", "--record"]));
    let evidence = fixture.captured.lock().expect("evidence").evidence.clone();
    assert_eq!(evidence.len(), 1);
    let original = thread_api::evidence::verify_evidence(
        evidence[0]
            .evidence
            .as_ref()
            .expect("projection")
            .evidence
            .as_ref()
            .expect("original"),
    )
    .expect("verified evidence");
    assert_eq!(original.spool, fixture.spool);
    assert_eq!(original.thread, fixture.thread_id);
    assert_eq!(original.revision, fixture.state);
    fixture.assert_identity();
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_clone_context_revision_discussion_push() {
    let fixture = Fixture::new().await;
    fixture.run(&[
        "context",
        "set",
        "--path",
        "story.txt",
        "--kind",
        "rationale",
        "-m",
        "initial note",
    ]);
    fixture.run(&[
        "context",
        "edit",
        "--path",
        "story.txt",
        "-m",
        "revised note",
    ]);
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "story.txt",
        "--visibility",
        "private:clone",
        "-m",
        "private discussion",
    ]);
    let output = fixture.output_at(&fixture.clone, &["--output", "json", "push", "origin"]);
    println!(
        "(d) exit {:?}\n{}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("source push result");
    assert_eq!(result["source"]["status"], "succeeded");
    assert_eq!(result["context"]["status"], "succeeded");
    assert_eq!(result["context"]["count"], 1);
    assert_eq!(
        output.status.code(),
        Some(65),
        "#1900 must be the only remaining failure"
    );
    assert_eq!(result["discussions"]["status"], "failed");
    assert!(
        result["discussions"]["error"]
            .as_str()
            .expect("discussion failure")
            .contains("command ID differs from signed operation")
    );
    let capture = fixture
        .captured
        .lock()
        .expect("collaboration capture")
        .clone();
    assert_eq!(
        capture.contexts.len(),
        2,
        "initial annotation and its revision"
    );
    assert_eq!(capture.discussions.len(), 1);
    fixture.assert_identity();
    fixture.capture();
    let retry = fixture.output_at(&fixture.clone, &["--output", "json", "push", "origin"]);
    let result: Value =
        serde_json::from_slice(&retry.stdout).expect("push after collaboration failure");
    assert_eq!(result["source"]["status"], "succeeded");
    assert_eq!(result["context"]["status"], "succeeded");
    assert_eq!(retry.status.code(), Some(65));
    assert!(
        result["discussions"]["error"]
            .as_str()
            .expect("discussion failure")
            .contains("command ID differs from signed operation")
    );
    println!("(d, after partial) source=succeeded context=succeeded; exit 65 only for #1900");
    fixture.assert_identity();
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_clone_rejects_another_spool_remote() {
    let fixture = Fixture::new().await;
    fixture.capture();
    let before =
        std::fs::read(fixture.clone.join(".heddle/spool-id")).expect("local spool identity");
    let mut other = Fixture::new().await;
    fixture.run(&["remote", "remove", "origin"]);
    other.run_at(
        &fixture.clone,
        &["remote", "add", "origin", &other.remote()],
    );
    other
        .captured
        .lock()
        .expect("clear seed calls")
        .calls
        .clear();
    let output = other.output_at(&fixture.clone, &["push", "origin"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(76), "{error}");
    assert!(
        error.contains("spool") || error.contains("identity differs"),
        "{error}"
    );
    assert!(
        !other
            .captured
            .lock()
            .expect("calls")
            .calls
            .iter()
            .any(|call| call.ends_with("/StartThread") || call.ends_with("/PublishContent"))
    );
    assert_eq!(
        std::fs::read(fixture.clone.join(".heddle/spool-id")).expect("unchanged spool"),
        before
    );
    let local = Repository::open(&fixture.clone).expect("local clone");
    let head = local.head().expect("local HEAD");
    let foreign = Repository::open(&other.clone)
        .expect("foreign clone")
        .head()
        .expect("foreign HEAD")
        .expect("foreign state");
    let error = other
        .client
        .fetch_state(&local, "spool/acme", "main", foreign)
        .await
        .expect_err("pull must not adopt another Spool");
    let message = error.to_string();
    assert!(
        message.contains("spool identity conflicts")
            || message.contains("owner observation differs from immutable clone pin"),
        "{error}"
    );
    assert_eq!(local.head().expect("preserved HEAD"), head);
    assert_eq!(
        std::fs::read(fixture.clone.join(".heddle/spool-id")).expect("preserved identity"),
        before
    );
    println!("negative: exit {:?}: {error}", output.status.code());
    other.close().await;
    fixture.close().await;
}

#[cfg(all(feature = "ci", feature = "preview"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_clone_capture_ci_run_record() {
    let fixture = Fixture::new().await;
    fixture.capture();
    fixture.configure_ci();
    println!(
        "(c, after capture) {}",
        fixture.run(&["ci", "run", "--record"])
    );
    fixture.assert_identity();
    fixture.close().await;
}

#[cfg(all(feature = "ci", feature = "preview"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_clone_ci_rejects_another_spool_remote() {
    let fixture = Fixture::new().await;
    fixture.capture();
    fixture.configure_ci();
    let other = Fixture::new().await;
    fixture.run(&["remote", "remove", "origin"]);
    other.run_at(
        &fixture.clone,
        &["remote", "add", "origin", &other.remote()],
    );
    let output = other.output_at(
        &fixture.clone,
        &["--output", "json", "ci", "run", "--record"],
    );
    let error: Value = serde_json::from_slice(&output.stderr).expect("CI refusal");
    assert_eq!(error["kind"], "ci_record_spool_mismatch");
    assert!(other.captured.lock().expect("evidence").evidence.is_empty());
    other.close().await;
    fixture.close().await;
}
