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
    source: PathBuf,
    addr: iroh::EndpointAddr,
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
        let fixture = Self::uninstalled().await;
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

    async fn uninstalled() -> Self {
        let temp = TempDir::new().expect("fixture");
        let source = temp.path().join("source");
        let source_home = temp.path().join("source-home");
        std::fs::create_dir_all(&source).expect("source");
        let envs = [("HEDDLE_HOME", source_home.to_str().expect("source home"))];
        heddle_env(&["init"], Some(&source), &envs).expect("native source init");
        std::fs::write(source.join("story.txt"), "source only\n").expect("source file");
        std::fs::write(source.join("example.py"), "def amount():\n    return 42\n")
            .expect("symbol fixture");
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
            VecDeque::from(vec![descriptor; 64]),
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
        Self {
            clone: temp.path().join("clone"),
            source,
            addr,
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
        }
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
async fn local_source_fetch_preserves_original_authority_without_executor_enrollment() {
    let fixture = Fixture::uninstalled().await;
    let output = fixture.output_at(
        fixture._temp.path(),
        &[
            "clone",
            &fixture.remote(),
            fixture.clone.to_str().expect("clone path"),
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let clone = Repository::open(&fixture.clone).expect("complete local-authority clone");
    let replica = repo::thread_replication::ThreadReplica::open(
        &fixture.clone.join(".heddle"),
        fixture.thread_id,
    )
    .expect("retained original genesis");
    assert_eq!(replica.signed_genesis().expect("original"), fixture.genesis);
    let tip = Repository::open(&fixture.source)
        .expect("published source")
        .head()
        .expect("source HEAD")
        .expect("source State");
    assert_eq!(clone.head().expect("clone HEAD"), Some(tip));
    let originals = replica
        .accepted_source_originals_for_revisions(&[tip])
        .expect("admitted original source");
    assert!(!originals.is_empty());
    let source = repo::thread_replication::ThreadReplica::open(
        &fixture.source.join(".heddle"),
        fixture.thread_id,
    )
    .expect("source replica");
    assert_eq!(
        originals,
        source
            .accepted_source_originals_for_revisions(&[tip])
            .expect("published source originals")
    );
    for (_, original) in &originals {
        replica
            .verify_local_source_owner(&original.verify().expect("original signature"))
            .expect("original local owner authorizes source");
    }
    let database = repo::local_metadata::open(clone.heddle_dir()).expect("clone metadata");
    let executor_pins: i64 = database
        .query_row("SELECT count(*) FROM hosted_executor_pins", [], |row| {
            row.get(0)
        })
        .expect("executor pins");
    assert_eq!(
        executor_pins, 0,
        "transport must never enroll executor authority"
    );
    fixture.close().await;
}

// The remaining hosted write regressions await fresh independently selected
// HYBRID trust from the Part 2 Fetch adapter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn scoped_derived_agent_clones_and_pushes_its_spool() {
    let mut fixture = Fixture::new().await;
    let child = fixture._temp.path().join("scoped.hcred");
    fixture.run_at(
        fixture._temp.path(),
        &[
            "auth",
            "derive-agent",
            "--server",
            &fixture.https.authority,
            "--scope",
            "spool:acme",
            "--out",
            child.to_str().expect("child path"),
        ],
    );
    fixture.credential = child;
    fixture
        .captured
        .lock()
        .expect("scope enforcement")
        .enforce_scope = true;
    fixture.clone = fixture._temp.path().join("scoped-clone");
    fixture.run_at(
        fixture._temp.path(),
        &[
            "clone",
            &fixture.remote(),
            fixture.clone.to_str().expect("clone path"),
        ],
    );
    fixture.assert_identity();
    fixture.capture();
    assert_push_succeeded(&fixture, &fixture.clone);
    fixture.server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn derive_agent_json_output_contract() {
    let fixture = Fixture::new().await;
    let mode = "json";
    for export in [true, false] {
        let path = fixture._temp.path().join(format!("{mode}.hcred"));
        let mut args = vec![
            "--output",
            mode,
            "auth",
            "derive-agent",
            "--server",
            &fixture.https.authority,
            "--agent-id",
            "scoped-worker",
            "--scope",
            "spool:acme",
            "--template",
            "contributor",
            "--ttl",
            "900",
        ];
        if export {
            args.extend(["--out", path.to_str().expect("path")]);
        }
        let output = fixture.output_at(fixture._temp.path(), &args);
        assert!(
            output.status.success(),
            "derive JSON: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("single JSON document");
        assert_eq!(value["output_kind"], "auth_derive_agent");
        assert_eq!(value["status"], "derived");
        assert_eq!(value["agent_id"], "scoped-worker");
        assert_eq!(value["scopes"], serde_json::json!(["spool:acme"]));
        assert_eq!(value["template"], "contributor");
        assert!(
            value["allowed_operations"]
                .as_array()
                .expect("operations")
                .iter()
                .any(|op| op == "PublishContent")
        );
        assert_eq!(value["installed"], !export);
        assert_eq!(
            value["credential_path"],
            if export {
                serde_json::json!(path)
            } else {
                Value::Null
            }
        );
        let remaining =
            chrono::DateTime::parse_from_rfc3339(value["expires_at"].as_str().expect("expiry"))
                .expect("RFC3339")
                .with_timezone(&chrono::Utc)
                - chrono::Utc::now();
        assert!((850..=900).contains(&remaining.num_seconds()));
        assert!(value.get("token").is_none());
        assert!(value.get("proof_key_pem").is_none());
        let schema = heddle_schema("auth derive-agent");
        assert_eq!(
            schema["properties"]["output_kind"]["enum"],
            serde_json::json!(["auth_derive_agent"])
        );
        for field in schema["required"].as_array().expect("required fields") {
            assert!(value.get(field.as_str().expect("field name")).is_some());
        }
    }
    let catalog: Value =
        serde_json::from_str(&fixture.run(&["help", "--output", "json"])).expect("catalog");
    let commands = catalog["commands"].as_array().expect("commands");
    let entry = commands
        .iter()
        .find(|entry| entry["path"] == serde_json::json!(["auth", "derive-agent"]))
        .expect("derive catalog");
    assert_eq!(entry["supports_json"], true);
    assert_eq!(
        entry["schema_verbs"],
        serde_json::json!(["auth derive-agent"])
    );
    fixture.server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn fresh_clone_capture_push_main() {
    let fixture = Fixture::new().await;
    fixture.capture();
    println!("(a) {}", fixture.run(&["push", "origin"]));
    fixture.assert_identity();
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn fresh_clone_without_owner_key_capture_advises_new_thread_without_capturing() {
    let fixture = Fixture::new().await;
    std::fs::remove_file(fixture.home.join(repo::identity::DEVICE_IDENTITY_FILE))
        .expect("clone has no original owner key");
    std::fs::write(fixture.clone.join("story.txt"), "unsaved clone edit\n").expect("clone edit");
    let cloned = Repository::open(&fixture.clone).expect("clone repo");
    let head = cloned.head().expect("HEAD");
    let mut states = cloned.store().list_states().expect("states before refusal");
    states.sort();

    let human = fixture.output_at(&fixture.clone, &["capture", "-m", "must refuse"]);
    let json = fixture.output_at(
        &fixture.clone,
        &["--output", "json", "capture", "-m", "must refuse"],
    );
    for output in [&human, &json] {
        assert_eq!(output.status.code(), Some(74));
        assert!(output.stdout.is_empty(), "refusal has no capture result");
    }
    let after = Repository::open(&fixture.clone).expect("repo after refusals");
    let mut after_states = after.store().list_states().expect("states after refusal");
    after_states.sort();
    assert_eq!(after.head().expect("HEAD after refusal"), head);
    assert_eq!(after_states, states, "refusal must not store a capture");
    assert_eq!(
        std::fs::read_to_string(fixture.clone.join("story.txt")).expect("preserved edit"),
        "unsaved clone edit\n"
    );
    fixture.assert_identity();

    let human = String::from_utf8_lossy(&human.stderr);
    let json: Value = serde_json::from_slice(&json.stderr).expect("error envelope");
    println!("human refusal:\n{human}\nJSON refusal: {json}");
    assert!(human.contains("keeps its original owner"), "{human}");
    assert!(
        human.contains("a clone without that key cannot capture onto it"),
        "{human}"
    );
    assert!(human.contains("Next: heddle start <name>"), "{human}");
    assert!(human.contains("not copied"), "{human}");
    assert!(human.contains("copy or reapply"), "{human}");
    assert!(human.contains("cd"), "{human}");
    assert!(human.contains("heddle capture -m \"...\""), "{human}");
    assert!(human.contains("heddle push"), "{human}");
    assert_eq!(json["kind"], "native_source_signer_unavailable");
    assert_eq!(json["exit_code"], 74);
    assert_eq!(json["primary_command"], "heddle start <name>");
    assert_eq!(
        json["primary_command_template"]["argv_template"],
        serde_json::json!([env!("CARGO_BIN_EXE_heddle"), "start", "<name>"])
    );
    assert_eq!(json["primary_command_template"]["agent_may_fill"], true);
    assert_eq!(
        json["recovery_commands"],
        serde_json::json!([
            "heddle start <name>",
            "heddle capture -m \"...\"",
            "heddle push"
        ])
    );
    assert_eq!(
        json["recovery_action_templates"]
            .as_array()
            .expect("templates")
            .len(),
        3
    );
    assert!(
        json["error"]
            .as_str()
            .expect("reason")
            .contains("keeps its original owner")
    );
    assert!(json["hint"].as_str().expect("hint").contains("not copied"));
    assert!(
        json["hint"]
            .as_str()
            .expect("hint")
            .contains("copy or reapply")
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn fresh_clone_without_owner_key_start_reapply_capture_push_succeeds() {
    let fixture = Fixture::new().await;
    std::fs::remove_file(fixture.home.join(repo::identity::DEVICE_IDENTITY_FILE))
        .expect("clone has no original owner key");
    std::fs::write(fixture.clone.join("story.txt"), "kept clone edit\n").expect("edit");
    std::fs::write(fixture.clone.join("notes.txt"), "new file\n").expect("new file");
    std::fs::remove_file(fixture.clone.join("example.py")).expect("deleted file");
    let original = Repository::open(&fixture.clone).expect("original checkout");
    let original_head = original.head().expect("original HEAD");

    // The catalog's start command works with unsaved edits when --path is omitted.
    let started: Value =
        serde_json::from_str(&fixture.run(&["--output", "json", "start", "my-change"]))
            .expect("start result");
    let feature = PathBuf::from(
        started["execution_path"]
            .as_str()
            .expect("checkout to cd into"),
    );
    assert_eq!(
        std::fs::read_to_string(feature.join("story.txt")).expect("new checkout baseline"),
        "source only\n",
        "start does not carry unsaved edits into the isolated checkout"
    );
    assert!(
        !feature.join("notes.txt").exists(),
        "untracked file is not copied"
    );
    assert!(
        feature.join("example.py").exists(),
        "deletion is not copied"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.clone.join("story.txt")).expect("original edit"),
        "kept clone edit\n"
    );
    assert!(fixture.clone.join("notes.txt").exists());
    assert!(!fixture.clone.join("example.py").exists());

    // Keep the original edits safe and copy/reapply them after cd'ing into the new checkout.
    for name in ["story.txt", "notes.txt"] {
        std::fs::copy(fixture.clone.join(name), feature.join(name)).expect("copy edited file");
    }
    std::fs::remove_file(feature.join("example.py")).expect("reapply deletion");
    fixture.run_at(&feature, &["capture", "-m", "my change"]);
    let owned = Repository::open(&feature).expect("owned checkout");
    let captured = owned.head().expect("owned HEAD").expect("captured state");
    assert_ne!(Some(captured), original_head);
    assert_eq!(
        original.head().expect("original HEAD after recovery"),
        original_head
    );
    let genesis = owned
        .native_thread("my-change")
        .expect("new Thread")
        .genesis()
        .expect("genesis");
    assert_eq!(genesis.parent, Some(fixture.thread_id));
    assert_ne!(
        genesis.owner,
        fixture.genesis.verify().expect("original genesis").owner
    );
    let pushed = fixture.output_at(&feature, &["--output", "json", "push"]);
    assert!(
        pushed.status.success(),
        "push from the advised checkout: {} {}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    );
    let pushed: Value = serde_json::from_slice(&pushed.stdout).expect("push result");
    assert_eq!(pushed["source"]["status"], "succeeded");
    {
        let published = fixture.captured.lock().expect("server publication");
        assert!(
            matches!(
                published.revision.as_ref().expect("accepted revision").revision.as_ref(),
                Some(api::heddle::api::v1alpha2::revision_ref::Revision::State(state))
                    if state.value == captured.as_bytes()
            ),
            "test server must accept the new capture"
        );
    }
    fixture.assert_identity();
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
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
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
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

fn assert_push_succeeded(fixture: &Fixture, path: &Path) {
    let output = fixture.output_at(path, &["--output", "json", "push", "origin"]);
    let result: Value = serde_json::from_slice(&output.stdout).expect("push result");
    println!("push exit {:?}: {result}", output.status.code());
    assert_eq!(result["source"]["status"], "succeeded");
    assert_eq!(result["context"]["status"], "succeeded");
    assert_eq!(result["discussions"]["status"], "succeeded");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Push once, expecting source and context to land and only the discussion
/// replication to fail; the partial status must say so. A lost receipt is
/// safe to resend unchanged, which exit code 75 declares.
fn assert_push_discussions_failed(fixture: &Fixture, path: &Path, reason: &str) {
    let output = fixture.output_at(path, &["--output", "json", "push", "origin"]);
    let result: Value = serde_json::from_slice(&output.stdout).expect("partial result");
    println!("push exit {:?}: {result}", output.status.code());
    assert_eq!(output.status.code(), Some(75));
    assert_eq!(result["discussions"]["unsent"][0]["kind"], "transient");
    assert_eq!(result["discussions"]["unsent"][0]["retry_unchanged"], true);
    assert_eq!(result["status"], "partial");
    assert_eq!(result["source"]["status"], "succeeded");
    assert_eq!(result["context"]["status"], "succeeded");
    assert_eq!(result["discussions"]["status"], "failed");
    let error = result["discussions"]["error"].as_str().expect("error");
    assert!(error.contains(reason), "{error}");
}

/// The idempotency key the signed discussion operation authenticates.
fn signed_key(signed: Option<&api::heddle::api::v1alpha2::SignedRecord>) -> String {
    let operation =
        thread_api::collaboration::verify(signed.expect("signed original")).expect("verified");
    let objects::object::thread_replication::ThreadOperationBody::Discussion(bytes) =
        operation.body
    else {
        panic!("discussion original")
    };
    objects::object::CollaborationOperationEnvelope::decode(&bytes)
        .expect("envelope")
        .operation
        .idempotency_key
        .as_str()
        .to_string()
}

fn fresh_discussion(fixture: &Fixture, name: &str) -> objects::object::MaterializedDiscussion {
    let path = fixture._temp.path().join(name);
    fixture.run_at(
        fixture._temp.path(),
        &["clone", &fixture.remote(), path.to_str().expect("checkout")],
    );
    let repo = Repository::open(&path).expect("fresh checkout");
    let view = repo::CollaborationStore::open(repo.heddle_dir())
        .expect("discussion store")
        .materialize()
        .expect("discussions");
    assert_eq!(view.discussions.len(), 1, "one reconstructed discussion");
    let discussion = view.discussions.into_values().next().expect("discussion");
    // Exercise the human view as well as reconstruction from signed originals.
    let shown = fixture.run_at(
        &path,
        &["discuss", "show", &discussion.discussion_id.to_string()],
    );
    assert!(shown.contains(&discussion.turns[0].1.body), "{shown}");
    discussion
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn private_symbol_discussion_fixture_publishes_and_clones() {
    let fixture = Fixture::new().await;
    // Source was initialized and captured before the server was routed.
    fixture.run_at(
        &fixture.source,
        &["remote", "add", "origin", &fixture.remote()],
    );
    fixture.run_at(
        &fixture.source,
        &[
            "context",
            "set",
            "--path",
            "example.py",
            "--symbol",
            "amount",
            "--kind",
            "rationale",
            "--body",
            "initial note",
        ],
    );
    fixture.run_at(&fixture.source, &["push", "origin"]);
    fixture.run_at(
        &fixture.source,
        &[
            "context",
            "edit",
            "--path",
            "example.py",
            "--symbol",
            "amount",
            "--body",
            "revised note",
        ],
    );
    fixture.run_at(
        &fixture.source,
        &[
            "discuss",
            "new",
            "--path",
            "example.py",
            "--symbol",
            "amount",
            "--thread",
            "main",
            "--visibility",
            "private:clone",
            "--body",
            "private discussion",
        ],
    );
    assert_push_succeeded(&fixture, &fixture.source);
    let discussion = fresh_discussion(&fixture, "fixture-clone");
    assert_eq!(discussion.turns[0].1.body, "private discussion");
    assert_eq!(
        discussion.visibility,
        objects::object::VisibilityTier::Private {
            scope_label: "clone".into()
        }
    );
    assert_eq!(discussion.thread_ref.as_deref(), Some("main"));
    let objects::object::CollaborationAnchor::Source { source } = discussion.anchor else {
        panic!("native source anchor")
    };
    assert_eq!(source.path, "example.py");
    assert_eq!(source.symbol_id, "amount");
    let capture = fixture.captured.lock().expect("capture").clone();
    assert_eq!(capture.contexts.len(), 2);
    assert_eq!(capture.discussions.len(), 1);
    let request = &capture.discussions[0];
    assert_eq!(
        request.client_operation_id,
        signed_key(request.signed_operation.as_ref()),
        "the wire command ID is the signed operation's key"
    );
    fixture.close().await;
}

/// `(annotation_id, scope)` pairs from `context get` on `path`, optionally
/// narrowed to one symbol.
fn context_get(
    fixture: &Fixture,
    checkout: &Path,
    path: &str,
    symbol: Option<&str>,
) -> Vec<(String, String)> {
    let mut args = vec!["--output", "json", "context", "get", "--path", path];
    if let Some(symbol) = symbol {
        args.extend(["--symbol", symbol]);
    }
    let output: Value =
        serde_json::from_str(&fixture.run_at(checkout, &args)).expect("context get JSON");
    let mut annotations: Vec<(String, String)> = output["annotations"]
        .as_array()
        .expect("annotations")
        .iter()
        .map(|annotation| {
            (
                annotation["annotation_id"]
                    .as_str()
                    .expect("id")
                    .to_string(),
                annotation["scope"].as_str().expect("scope").to_string(),
            )
        })
        .collect();
    annotations.sort();
    annotations
}

/// The annotation IDs attached to `symbol` in `path`. File-wide and
/// symbol-narrowed retrieval must agree, and every scope must still be the
/// authored symbol selector.
fn symbol_context_ids(fixture: &Fixture, checkout: &Path, path: &str, symbol: &str) -> Vec<String> {
    let file_wide = context_get(fixture, checkout, path, None);
    let by_symbol = context_get(fixture, checkout, path, Some(symbol));
    let expected_scope = format!("symbol:{symbol}");
    assert!(
        file_wide.iter().all(|(_, scope)| *scope == expected_scope),
        "file-wide retrieval replaced the symbol selector: {file_wide:?}"
    );
    assert_eq!(
        by_symbol, file_wide,
        "symbol retrieval must return the same annotations as file-wide retrieval"
    );
    file_wide.into_iter().map(|(id, _)| id).collect()
}

/// `context check` counts plus the `(annotation_id, reason)` of each issue.
fn context_check(
    fixture: &Fixture,
    checkout: &Path,
    path: &str,
) -> (u64, u64, Vec<(String, String)>) {
    let output: Value = serde_json::from_str(&fixture.run_at(
        checkout,
        &["--output", "json", "context", "check", "--path", path],
    ))
    .expect("context check JSON");
    let mut issues: Vec<(String, String)> = output["issues"]
        .as_array()
        .map(|issues| {
            issues
                .iter()
                .map(|issue| {
                    (
                        issue["annotation_id"].as_str().expect("id").to_string(),
                        issue["reason"].as_str().expect("reason").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    issues.sort();
    (
        output["fresh"].as_u64().expect("fresh"),
        output["stale"].as_u64().expect("stale"),
        issues,
    )
}

fn capture_source(fixture: &Fixture, checkout: &Path, path: &str, content: &str, message: &str) {
    std::fs::write(checkout.join(path), content).expect("edit source");
    fixture.run_at(checkout, &["capture", "-m", message]);
}

/// heddle#1901: a symbol annotation stays a symbol annotation, with its
/// stable ID, across push -> pull and push -> clone, and keeps travelling
/// with its symbol afterwards. Unresolvable selectors report explicitly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn symbol_context_survives_hosted_push_pull_and_clone() {
    let fixture = Fixture::new().await;
    fixture.run_at(
        &fixture.source,
        &["remote", "add", "origin", &fixture.remote()],
    );
    for (kind, body) in [
        ("constraint", "amount is integer cents"),
        ("invariant", "amount never goes negative"),
        ("rationale", "amount stays pure for caching"),
    ] {
        fixture.run_at(
            &fixture.source,
            &[
                "context",
                "set",
                "--path",
                "example.py",
                "--symbol",
                "amount",
                "--kind",
                kind,
                "--body",
                body,
            ],
        );
    }
    let authored = symbol_context_ids(&fixture, &fixture.source, "example.py", "amount");
    assert_eq!(authored.len(), 3, "three kinds authored");
    assert_push_succeeded(&fixture, &fixture.source);

    // push -> pull: the fixture clone predates the annotations.
    fixture.run(&["pull", "origin"]);
    assert_eq!(
        symbol_context_ids(&fixture, &fixture.clone, "example.py", "amount"),
        authored,
        "push -> pull"
    );

    // push -> clone.
    let clone = fixture._temp.path().join("symbol-clone");
    fixture.run_at(
        fixture._temp.path(),
        &[
            "clone",
            &fixture.remote(),
            clone.to_str().expect("clone path"),
        ],
    );
    assert_eq!(
        symbol_context_ids(&fixture, &clone, "example.py", "amount"),
        authored,
        "push -> clone"
    );
    assert_eq!(
        context_check(&fixture, &clone, "example.py"),
        (3, 0, Vec::new())
    );

    // An edit to an unrelated symbol leaves the attachment fresh.
    capture_source(
        &fixture,
        &clone,
        "example.py",
        "def amount():\n    return 42\n\n\ndef other():\n    return 1\n",
        "unrelated symbol",
    );
    assert_eq!(
        symbol_context_ids(&fixture, &clone, "example.py", "amount"),
        authored
    );
    assert_eq!(
        context_check(&fixture, &clone, "example.py"),
        (3, 0, Vec::new())
    );

    // A line shift moves the symbol; the selector follows it.
    capture_source(
        &fixture,
        &clone,
        "example.py",
        "# pricing helpers\n\n\ndef other():\n    return 1\n\n\ndef amount():\n    return 42\n",
        "line shift",
    );
    assert_eq!(
        symbol_context_ids(&fixture, &clone, "example.py", "amount"),
        authored
    );
    assert_eq!(
        context_check(&fixture, &clone, "example.py"),
        (3, 0, Vec::new())
    );

    // A file rename carries the attachment to the new path.
    std::fs::rename(clone.join("example.py"), clone.join("pricing.py")).expect("rename file");
    fixture.run_at(&clone, &["capture", "-m", "rename file"]);
    assert_eq!(
        symbol_context_ids(&fixture, &clone, "pricing.py", "amount"),
        authored
    );
    assert!(context_get(&fixture, &clone, "example.py", None).is_empty());
    assert_eq!(
        context_check(&fixture, &clone, "pricing.py"),
        (3, 0, Vec::new())
    );

    let explicit = |reason: &str| -> Vec<(String, String)> {
        authored
            .iter()
            .map(|id| (id.clone(), reason.to_string()))
            .collect()
    };

    // Duplicating the symbol makes the selector ambiguous, never fresh.
    capture_source(
        &fixture,
        &clone,
        "pricing.py",
        "def other():\n    return 1\n\n\ndef amount():\n    return 42\n\n\ndef amount():\n    return 43\n",
        "duplicate symbol",
    );
    assert_eq!(
        symbol_context_ids(&fixture, &clone, "pricing.py", "amount"),
        authored
    );
    assert_eq!(
        context_check(&fixture, &clone, "pricing.py"),
        (0, 3, explicit("symbol_ambiguous"))
    );

    // Renaming the symbol leaves the authored selector unresolved.
    capture_source(
        &fixture,
        &clone,
        "pricing.py",
        "def other():\n    return 1\n\n\ndef total_cents():\n    return 42\n",
        "rename symbol",
    );
    assert_eq!(
        symbol_context_ids(&fixture, &clone, "pricing.py", "amount"),
        authored
    );
    assert_eq!(
        context_check(&fixture, &clone, "pricing.py"),
        (0, 3, explicit("symbol_missing"))
    );

    // Deleting the symbol is reported the same way.
    capture_source(
        &fixture,
        &clone,
        "pricing.py",
        "def other():\n    return 1\n",
        "delete symbol",
    );
    assert_eq!(
        symbol_context_ids(&fixture, &clone, "pricing.py", "amount"),
        authored
    );
    assert_eq!(
        context_check(&fixture, &clone, "pricing.py"),
        (0, 3, explicit("symbol_missing"))
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn discussion_open_reply_resolve_publish_and_clone() {
    let fixture = Fixture::new().await;
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "example.py",
        "--symbol",
        "amount",
        "--thread",
        "main",
        "--visibility",
        "private:clone",
        "--body",
        "opening turn",
    ]);
    let repo = Repository::open(&fixture.clone).expect("repo");
    let view = repo::CollaborationStore::open(repo.heddle_dir())
        .expect("store")
        .materialize()
        .expect("view");
    let id = view.discussions.keys().next().expect("id").to_string();
    assert_push_succeeded(&fixture, &fixture.clone);
    fixture.run(&["discuss", "reply", &id, "--body", "reply turn"]);
    assert_push_succeeded(&fixture, &fixture.clone);
    fixture.run(&[
        "discuss",
        "resolve",
        &id,
        "--mode",
        "dismiss",
        "--reason",
        "review complete",
    ]);
    // The resolution is applied but its receipt is lost: push must report
    // the discussion as failed, and the retry must redeliver the persisted
    // signed resolution under the same command ID rather than re-sign it.
    fixture
        .captured
        .lock()
        .expect("lose resolve receipt")
        .lose_next_discussion_receipt = true;
    assert_push_discussions_failed(&fixture, &fixture.clone, "receipt lost after apply");
    assert_push_succeeded(&fixture, &fixture.clone);
    assert_push_succeeded(&fixture, &fixture.clone);
    let discussion = fresh_discussion(&fixture, "resolved-clone");
    assert_eq!(
        discussion
            .turns
            .iter()
            .map(|(_, t)| t.body.as_str())
            .collect::<Vec<_>>(),
        vec!["opening turn", "reply turn"]
    );
    assert_eq!(
        discussion.resolution,
        Some(objects::object::CollaborationResolution::Dismissed {
            reason: "review complete".into()
        })
    );
    let captured = fixture.captured.lock().expect("capture").clone();
    assert_eq!(captured.discussions.len(), 1);
    assert_eq!(captured.appends.len(), 1);
    assert_eq!(captured.resolutions.len(), 1);
    assert_eq!(captured.discussion_operations.len(), 3);
    let open = signed_key(captured.discussions[0].signed_operation.as_ref());
    let append = signed_key(captured.appends[0].signed_operation.as_ref());
    let resolve = signed_key(captured.resolutions[0].signed_operation.as_ref());
    assert_eq!(captured.discussions[0].client_operation_id, open);
    assert_eq!(captured.appends[0].client_operation_id, append);
    assert_eq!(captured.resolutions[0].client_operation_id, resolve);
    assert_eq!(
        captured.delivered_command_ids,
        vec![open, append, resolve.clone(), resolve],
        "one delivery per command, plus one redelivery of the unacknowledged resolve"
    );
    fixture.close().await;
}

fn publish_resolved_discussion(fixture: &Fixture) {
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "example.py",
        "--body",
        "opening turn",
    ]);
    let repo = Repository::open(&fixture.clone).expect("repo");
    let view = repo::CollaborationStore::open(repo.heddle_dir())
        .expect("store")
        .materialize()
        .expect("view");
    let id = view.discussions.keys().next().expect("id").to_string();
    fixture.run(&["discuss", "reply", &id, "--body", "reply turn"]);
    fixture.run(&[
        "discuss",
        "resolve",
        &id,
        "--mode",
        "dismiss",
        "--reason",
        "review complete",
    ]);
    assert_push_succeeded(fixture, &fixture.clone);
    assert_eq!(
        fixture
            .captured
            .lock()
            .expect("operations")
            .discussion_operations
            .len(),
        3
    );
}

fn assert_push_sends_no_discussions(fixture: &Fixture, path: &Path) {
    let before = fixture
        .captured
        .lock()
        .expect("received")
        .received_discussion_operations
        .len();
    let output = fixture.output_at(path, &["--output", "json", "push", "origin"]);
    let result: Value = serde_json::from_slice(&output.stdout).expect("push result");
    let after = fixture
        .captured
        .lock()
        .expect("received")
        .received_discussion_operations
        .len();
    assert_eq!(
        after - before,
        0,
        "push must send zero discussion operations; exit {:?}: {result}",
        output.status.code()
    );
    assert!(output.status.success(), "{result}");
    assert_eq!(result["discussions"]["status"], "succeeded");
    assert_eq!(result["discussions"]["count"], 0);
    assert!(result["discussions"].get("unsent").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn cloned_discussions_are_already_published() {
    let fixture = Fixture::new().await;
    publish_resolved_discussion(&fixture);
    let path = fixture._temp.path().join("author-clone");
    fixture.run_at(
        fixture._temp.path(),
        &["clone", &fixture.remote(), path.to_str().expect("checkout")],
    );
    assert_push_sends_no_discussions(&fixture, &path);
    // Publication markers must only cover fetched operations.
    fixture.run_at(
        &path,
        &[
            "discuss",
            "new",
            "--path",
            "example.py",
            "--body",
            "new local discussion",
        ],
    );
    // Fetching again must not mark the new local operation as accepted.
    fixture.run_at(&path, &["pull", "origin"]);
    let before = fixture
        .captured
        .lock()
        .expect("received")
        .received_discussion_operations
        .len();
    assert_push_succeeded(&fixture, &path);
    let captured = fixture.captured.lock().expect("capture").clone();
    assert_eq!(captured.received_discussion_operations.len() - before, 1);
    assert_eq!(captured.discussion_operations.len(), 4);
    assert_push_sends_no_discussions(&fixture, &path);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn pulled_discussions_are_already_published() {
    let fixture = Fixture::new().await;
    // Clone before the author publishes, then fetch into this existing checkout.
    let path = fixture._temp.path().join("author-pull");
    fixture.run_at(
        fixture._temp.path(),
        &["clone", &fixture.remote(), path.to_str().expect("checkout")],
    );
    publish_resolved_discussion(&fixture);
    fixture.run_at(&path, &["pull", "origin"]);
    assert_push_sends_no_discussions(&fixture, &path);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn pulled_discussions_keep_local_replies_and_resolutions_pending() {
    let fixture = Fixture::new().await;
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "example.py",
        "--body",
        "opening turn",
    ]);
    assert_push_succeeded(&fixture, &fixture.clone);
    let discussion = fresh_discussion(&fixture, "reply-clone");
    let path = fixture._temp.path().join("reply-clone");
    let id = discussion.discussion_id.to_string();
    assert_push_sends_no_discussions(&fixture, &path);
    fixture.run_at(&path, &["discuss", "reply", &id, "--body", "local reply"]);
    fixture.run_at(
        &path,
        &[
            "discuss",
            "resolve",
            &id,
            "--mode",
            "dismiss",
            "--reason",
            "local resolution",
        ],
    );
    fixture.run_at(&path, &["pull", "origin"]);
    assert_push_succeeded(&fixture, &path);
    assert_push_sends_no_discussions(&fixture, &path);
    let captured = fixture.captured.lock().expect("operations").clone();
    assert_eq!(captured.received_discussion_operations.len(), 3);
    assert_eq!(captured.discussion_operations.len(), 3);
    let published = fresh_discussion(&fixture, "local-work-proof");
    assert_eq!(published.turns.len(), 2);
    assert_eq!(published.turns[1].1.body, "local reply");
    assert_eq!(
        published.resolution,
        Some(objects::object::CollaborationResolution::Dismissed {
            reason: "local resolution".into()
        })
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn cloned_discussion_without_publication_links_replays_signed_operations() {
    let fixture = Fixture::new().await;
    publish_resolved_discussion(&fixture);
    let path = fixture._temp.path().join("old-author-clone");
    fixture.run_at(
        fixture._temp.path(),
        &["clone", &fixture.remote(), path.to_str().expect("checkout")],
    );
    // Reproduce the pre-fix mirror: signed originals retained, publication
    // links absent. The resend must use the author's exact signed commands.
    let repo = Repository::open(&path).expect("old clone");
    let mirror_path = repo.heddle_dir().join("collaboration/hosted-mirror.json");
    let mut mirror: Value =
        serde_json::from_slice(&std::fs::read(&mirror_path).expect("mirror")).expect("mirror JSON");
    for repository in mirror["repos"].as_object_mut().expect("repos").values_mut() {
        repository["discussions"] = serde_json::json!([]);
    }
    std::fs::write(
        &mirror_path,
        serde_json::to_vec(&mirror).expect("old mirror"),
    )
    .expect("retain old mirror");
    let before = fixture.captured.lock().expect("capture").clone();
    assert_push_succeeded(&fixture, &path);
    let after = fixture.captured.lock().expect("capture").clone();
    assert_eq!(
        after.discussion_operations, before.discussion_operations,
        "replays must not duplicate operations"
    );
    assert_eq!(
        &after.received_discussion_operations[before.received_discussion_operations.len()..],
        &before.discussion_operations[..2],
        "open and reply must resend the exact signed originals"
    );
    assert_eq!(
        &after.delivered_command_ids[before.delivered_command_ids.len()..],
        &before.delivered_command_ids[..2],
        "replays must keep the signed command IDs"
    );
    assert_push_sends_no_discussions(&fixture, &path);
    let discussion = fresh_discussion(&fixture, "replay-proof");
    assert_eq!(discussion.turns.len(), 2);
    assert!(discussion.resolution.is_some());
    fixture.close().await;
}

/// A rejected delivery's failure code and message.
#[derive(Debug)]
struct Rejected {
    code: i32,
    message: String,
}

/// Send the captured request bytes again through the real hosted framing.
async fn deliver(
    fixture: &Fixture,
    method: &str,
    body: Vec<u8>,
) -> Result<api::heddle::api::v1alpha2::MutationResponse, Rejected> {
    use prost::Message;
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .expect("replay endpoint");
    let connection = endpoint
        .connect(fixture.addr.clone(), api::HOSTED_ALPN_V1)
        .await
        .expect("replay connection");
    let (mut send, mut recv) = connection.open_bi().await.expect("replay stream");
    send.write_all(
        &api::framing::encode_request_frame(method, &Default::default(), &body)
            .expect("request frame"),
    )
    .await
    .expect("send replay");
    send.finish().expect("finish request");
    let bytes = recv
        .read_to_end(api::framing::MAX_CONTROL_BODY)
        .await
        .expect("replay response");
    let result = match api::framing::decode_response_frame(&bytes).expect("response frame") {
        api::framing::ResponseFrame::Success(bytes) => {
            Ok(api::heddle::api::v1alpha2::MutationResponse::decode(bytes).expect("receipt"))
        }
        api::framing::ResponseFrame::Failure(failure) => Err(Rejected {
            code: failure.code,
            message: failure.message,
        }),
    };
    endpoint.close().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn discussion_signed_replay_is_idempotent_and_mismatch_never_mutates() {
    use prost::Message;
    let fixture = Fixture::new().await;
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "example.py",
        "--body",
        "one turn",
    ]);
    // Apply the open but lose its receipt; the retry must redeliver the same
    // signed operation under the same command ID, leaving one discussion.
    fixture
        .captured
        .lock()
        .expect("lose open receipt")
        .lose_next_discussion_receipt = true;
    assert_push_discussions_failed(&fixture, &fixture.clone, "receipt lost after apply");
    assert_push_succeeded(&fixture, &fixture.clone);
    let request = fixture.captured.lock().expect("capture").discussions[0].clone();
    let key = signed_key(request.signed_operation.as_ref());
    assert_eq!(request.client_operation_id, key);
    assert_eq!(
        fixture
            .captured
            .lock()
            .expect("deliveries")
            .delivered_command_ids,
        vec![key.clone(), key.clone()]
    );
    let method = "/heddle.api.v1alpha2.CollaborationService/OpenDiscussion";
    for _ in 0..2 {
        deliver(&fixture, method, request.encode_to_vec())
            .await
            .expect("identical signed replay");
    }
    assert_eq!(
        fixture
            .captured
            .lock()
            .expect("replays")
            .discussion_operations
            .len(),
        1
    );
    // A new signed original makes mutation on rejection observable, even if
    // an implementation accidentally deduplicates before checking the ID.
    let signed = request.signed_operation.as_ref().expect("original");
    let operation = thread_api::collaboration::verify(signed).expect("verified");
    let objects::object::thread_replication::ThreadOperationBody::Discussion(bytes) =
        operation.body
    else {
        panic!("discussion")
    };
    let authored = objects::object::CollaborationOperationEnvelope::decode(&bytes)
        .expect("envelope")
        .operation;
    let different = thread_api::collaboration::Command {
        discussion: objects::object::DiscussionRecordId::generate(),
        operation_id: objects::object::CollaborationIdempotencyKey::new(
            uuid::Uuid::new_v4().to_string(),
        )
        .expect("key"),
        metadata: authored.metadata.expect("metadata"),
        author: authored.author,
        occurred_at_ms: authored.occurred_at_ms,
        body: authored.body,
    }
    .sign(&[], &Ed25519Signer::from_seed(&[71; 32]).expect("signer"))
    .expect("different signed open");
    let mut mismatch = request;
    mismatch.signed_operation = Some(different);
    mismatch.client_operation_id = uuid::Uuid::new_v4().to_string();
    let before = fixture.captured.lock().expect("before").clone();
    let failure = deliver(&fixture, method, mismatch.encode_to_vec())
        .await
        .expect_err("mismatched command rejected");
    assert_eq!(
        failure.code,
        api::heddle::api::common::CallFailureCode::InvalidArgument as i32
    );
    assert_eq!(failure.message, "command ID differs from signed operation");
    let after = fixture.captured.lock().expect("after").clone();
    assert_eq!(after.discussions, before.discussions);
    assert_eq!(after.discussion_operations, before.discussion_operations);
    assert_eq!(after.discussions.len(), 1);
    let discussion = fresh_discussion(&fixture, "replayed-clone");
    assert_eq!(discussion.turns.len(), 1);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn discussion_failure_keeps_push_partial_after_source_success() {
    let fixture = Fixture::new().await;
    fixture.run(&[
        "context",
        "set",
        "--path",
        "example.py",
        "--body",
        "context succeeds",
    ]);
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "example.py",
        "--body",
        "retained discussion",
    ]);
    fixture
        .captured
        .lock()
        .expect("reject discussion")
        .reject_discussions = true;
    fixture.capture();
    let output = fixture.output_at(&fixture.clone, &["--output", "json", "push", "origin"]);
    let result: Value = serde_json::from_slice(&output.stdout).expect("partial result");
    assert_eq!(output.status.code(), Some(65));
    assert_eq!(result["status"], "partial");
    assert_eq!(result["source"]["status"], "succeeded");
    assert_eq!(result["context"]["status"], "succeeded");
    assert_eq!(result["discussions"]["status"], "failed");
    assert!(
        result["discussions"]["error"]
            .as_str()
            .expect("error")
            .contains("injected discussion rejection")
    );
    assert!(
        fixture
            .captured
            .lock()
            .expect("no mutation")
            .discussion_operations
            .is_empty()
    );
    // heddle#1904: the service refused the signed command itself. Retrying
    // it unchanged cannot help, and the operation IDs are kept.
    let repo = Repository::open(&fixture.clone).expect("repo");
    let discussion = repo::CollaborationStore::open(repo.heddle_dir())
        .expect("store")
        .materialize()
        .expect("view")
        .discussions
        .keys()
        .next()
        .expect("discussion")
        .to_string();
    let item = &result["discussions"]["unsent"][0];
    assert_eq!(item["record_id"], discussion.as_str());
    assert_eq!(item["kind"], "invalid_command", "{result}");
    assert_eq!(item["retry_unchanged"], false);
    let command = item["client_operation_id"]
        .as_str()
        .expect("command ID")
        .to_string();
    assert!(item["signed_operation_id"].as_str().is_some());
    assert_eq!(result["context"]["unsent"], Value::Null);
    let show = format!("heddle discuss show {discussion}");
    assert_eq!(result["next_action"], show.as_str());
    assert_eq!(json_recovery_commands(&result), vec![show.clone()]);
    assert_human_agrees(&fixture, &fixture.clone, &result, 65);
    let (_, again) = push_json(&fixture, &fixture.clone);
    assert_eq!(
        again["discussions"]["unsent"][0]["client_operation_id"],
        command.as_str(),
        "a retry keeps the signed command's ID"
    );
    fixture
        .captured
        .lock()
        .expect("allow discussion")
        .reject_discussions = false;
    assert_push_succeeded(&fixture, &fixture.clone);
    assert_eq!(
        fresh_discussion(&fixture, "recovered-clone").turns[0]
            .1
            .body,
        "retained discussion"
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
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
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
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
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
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

/// Run `push origin` for its JSON result and exit code.
fn push_json(fixture: &Fixture, path: &Path) -> (Option<i32>, Value) {
    let output = fixture.output_at(path, &["--output", "json", "push", "origin"]);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "push JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    println!("push exit {:?}: {result}", output.status.code());
    (output.status.code(), result)
}

/// The `Next:` action and every `recovery:` command a human push printed.
fn push_human(fixture: &Fixture, path: &Path) -> (Option<i32>, Option<String>, Vec<String>) {
    let output = fixture.output_at(path, &["push", "origin"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!(
        "human push exit {:?}\nstdout: {stdout}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let next = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Next: "))
        .map(str::to_string);
    let recovery = stdout
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("recovery: "))
        .map(str::to_string)
        .collect();
    (output.status.code(), next, recovery)
}

/// Every recovery command JSON gives, in surface then record order.
fn json_recovery_commands(result: &Value) -> Vec<String> {
    ["discussions", "context"]
        .iter()
        .flat_map(|surface| {
            ["unsent", "local_only"].iter().flat_map(move |list| {
                result[surface][list]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
        })
        .flat_map(|item| {
            item["recovery_commands"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .map(|command| command.as_str().expect("recovery command").to_string())
        .collect()
}

/// Human and JSON output of the same partial push name the same next action
/// and the same bounded recovery, and exit alike.
fn assert_human_agrees(fixture: &Fixture, path: &Path, json: &Value, exit: i32) {
    let (code, next, recovery) = push_human(fixture, path);
    assert_eq!(code, Some(exit));
    assert_eq!(next.as_deref(), json["next_action"].as_str());
    assert_eq!(recovery, json_recovery_commands(json));
}

/// A JSON action template's argv, less the executable path the renderer
/// substitutes for `heddle`.
fn argv_after_executable(template: &Value) -> Vec<String> {
    template["argv_template"]
        .as_array()
        .expect("argv template")
        .iter()
        .skip(1)
        .map(|arg| arg.as_str().expect("argv").to_string())
        .collect()
}

/// Verify a captured context request's original and return it.
fn context_operation(
    request: &api::heddle::api::v1alpha2::PutContextRequest,
) -> objects::object::thread_replication::ThreadOperation {
    thread_api::collaboration::verify(request.signed_operation.as_ref().expect("signed context"))
        .expect("verified context")
}

fn only_context_id(fixture: &Fixture, checkout: &Path, path: &str) -> String {
    let ids: Vec<String> = context_get(fixture, checkout, path, None)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids.len(), 1, "one annotation on {path}: {ids:?}");
    ids.into_iter().next().expect("annotation id")
}

/// heddle#1904: a context record is bound to the Thread it was first
/// published in. A child Thread revising it after a resolve, an edit and a
/// file move must extend that bound frontier, not be refused as stale when
/// nothing else wrote to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn child_thread_revision_extends_the_bound_context_frontier() {
    let fixture = Fixture::new().await;
    fixture.run(&[
        "context",
        "set",
        "--path",
        "example.py",
        "--symbol",
        "amount",
        "--body",
        "amount is cents",
    ]);
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "example.py",
        "--symbol",
        "amount",
        "--thread",
        "main",
        "--body",
        "why cents?",
    ]);
    let repo = Repository::open(&fixture.clone).expect("repo");
    let discussion = repo::CollaborationStore::open(repo.heddle_dir())
        .expect("store")
        .materialize()
        .expect("view")
        .discussions
        .keys()
        .next()
        .expect("discussion")
        .to_string();
    fixture.run(&[
        "discuss",
        "resolve",
        &discussion,
        "--mode",
        "dismiss",
        "--reason",
        "answered in context",
    ]);
    assert_push_succeeded(&fixture, &fixture.clone);
    let annotation = only_context_id(&fixture, &fixture.clone, "example.py");

    let path = fixture.run(&["start", "feature", "--print-cd-path"]);
    let feature = PathBuf::from(path.trim());
    fixture.run_at(
        &feature,
        &[
            "context",
            "edit",
            &annotation,
            "--body",
            "amount is integer cents",
        ],
    );
    std::fs::rename(feature.join("example.py"), feature.join("pricing.py")).expect("move file");
    fixture.run_at(&feature, &["capture", "-m", "move pricing"]);
    assert_push_succeeded(&fixture, &feature);

    let capture = fixture.captured.lock().expect("capture").clone();
    assert_eq!(capture.contexts.len(), 2, "create, then one revision");
    let created = context_operation(&capture.contexts[0]);
    let revised = context_operation(&capture.contexts[1]);
    assert_eq!(
        created.thread.as_bytes().as_slice(),
        fixture.thread_id.as_bytes().as_slice(),
        "the record was first published on main"
    );
    assert_eq!(
        revised.thread, created.thread,
        "the child Thread's revision extends the Thread the record is bound to"
    );
    assert_eq!(
        revised.parents,
        std::collections::BTreeSet::from([created.id().expect("created id")]),
        "and is parented on the observed frontier"
    );
    fixture.close().await;
}

/// heddle#1904: the source is accepted but a context revision is not. The
/// push reports each surface independently with a bounded recovery that JSON
/// and human output agree on. A genuinely concurrent writer is still refused,
/// an unchanged retry sends nothing, and refresh -> compare -> explicit
/// revision recovers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn context_failures_report_recovery_and_concurrent_writer_stays_rejected() {
    let fixture = Fixture::new().await;
    fixture.run(&["context", "set", "--path", "example.py", "--body", "v1"]);
    assert_push_succeeded(&fixture, &fixture.clone);
    let annotation = only_context_id(&fixture, &fixture.clone, "example.py");
    fixture.run(&["context", "edit", &annotation, "--body", "from clone"]);

    // Interrupted in flight: resending the identical signed revision is safe.
    fixture
        .captured
        .lock()
        .expect("interrupt")
        .interrupt_next_context = true;
    let (code, transient) = push_json(&fixture, &fixture.clone);
    assert_eq!(code, Some(75), "only a retry-safe failure remains");
    assert_eq!(transient["status"], "partial");
    assert_eq!(transient["success"], false);
    assert_eq!(transient["pushed"], true);
    assert_eq!(transient["source"]["status"], "succeeded");
    assert_eq!(transient["discussions"]["status"], "succeeded");
    assert_eq!(transient["context"]["status"], "failed");
    let item = &transient["context"]["unsent"][0];
    assert_eq!(item["record_id"], annotation.as_str());
    assert_eq!(item["kind"], "transient");
    assert_eq!(item["retry_unchanged"], true);
    let client_operation_id = item["client_operation_id"]
        .as_str()
        .expect("client operation ID")
        .to_string();
    let signed_operation_id = item["signed_operation_id"]
        .as_str()
        .expect("signed operation ID")
        .to_string();
    assert_eq!(
        json_recovery_commands(&transient),
        vec!["heddle push origin --thread main"],
        "{transient}"
    );
    assert_eq!(
        argv_after_executable(&item["recovery_action_templates"][0]),
        ["push", "origin", "--thread", "main"]
    );
    // Re-running `push` is never a push's next action (the self-loop rule);
    // exit 75 and `retry_unchanged` carry the retry instead.
    assert_eq!(transient["next_action"], Value::Null);
    fixture
        .captured
        .lock()
        .expect("interrupt")
        .interrupt_next_context = true;
    assert_human_agrees(&fixture, &fixture.clone, &transient, 75);

    // Another checkout revises the same annotation first.
    let other = fixture._temp.path().join("concurrent-clone");
    fixture.run_at(
        fixture._temp.path(),
        &[
            "clone",
            &fixture.remote(),
            other.to_str().expect("other checkout"),
        ],
    );
    fixture.run_at(
        &other,
        &["context", "edit", &annotation, "--body", "from concurrent"],
    );
    assert_push_succeeded(&fixture, &other);
    let accepted = fixture.captured.lock().expect("capture").contexts.len();
    assert_eq!(accepted, 2, "v1 and the concurrent revision");

    // The retained revision was signed against the old frontier: refused.
    let (code, stale) = push_json(&fixture, &fixture.clone);
    assert_eq!(code, Some(65));
    assert_eq!(stale["status"], "partial");
    assert_eq!(stale["pushed"], true);
    assert_eq!(stale["context"]["status"], "failed");
    let item = &stale["context"]["unsent"][0];
    assert_eq!(item["kind"], "stale_version", "{stale}");
    assert_eq!(
        item["revision_id"],
        transient["context"]["unsent"][0]["revision_id"]
    );
    assert!(item["revision_id"].is_string());
    assert_eq!(item["retry_unchanged"], false);
    assert_eq!(
        item["client_operation_id"],
        client_operation_id.as_str(),
        "the operation IDs are kept"
    );
    assert_eq!(item["signed_operation_id"], signed_operation_id.as_str());
    let recovery = vec![
        "heddle pull origin".to_string(),
        format!("heddle context history {annotation}"),
        format!("heddle context edit {annotation} --body <text>"),
    ];
    assert_eq!(json_recovery_commands(&stale), recovery);
    assert_eq!(stale["next_action"], "heddle pull origin");
    assert_eq!(stale["recommended_action"], "heddle pull origin");
    assert_eq!(
        argv_after_executable(&stale["next_action_template"]),
        ["pull", "origin"]
    );
    assert_eq!(
        fixture.captured.lock().expect("capture").contexts.len(),
        accepted,
        "a stale revision never lands"
    );

    // Retrying unchanged cannot help and sends nothing.
    let attempts = fixture
        .captured
        .lock()
        .expect("attempts")
        .context_attempts
        .len();
    assert_human_agrees(&fixture, &fixture.clone, &stale, 65);
    let (_, again) = push_json(&fixture, &fixture.clone);
    assert_eq!(again["context"]["unsent"][0]["kind"], "stale_version");
    assert_eq!(
        fixture
            .captured
            .lock()
            .expect("attempts")
            .context_attempts
            .len(),
        attempts,
        "an unchanged retry redelivers nothing"
    );

    // Refresh, compare, then revise explicitly.
    fixture.run(&["pull", "origin"]);
    let history: Value = serde_json::from_str(&fixture.run(&[
        "--output",
        "json",
        "context",
        "history",
        &annotation,
    ]))
    .expect("history JSON");
    let contents: Vec<&str> = history["revisions"]
        .as_array()
        .expect("revisions")
        .iter()
        .map(|revision| revision["content"].as_str().expect("content"))
        .collect();
    assert!(contents.contains(&"from concurrent"), "{contents:?}");
    fixture.run(&["context", "edit", &annotation, "--body", "reconciled"]);
    let (code, recovered) = push_json(&fixture, &fixture.clone);
    assert_eq!(code, Some(0), "{recovered}");
    assert_eq!(recovered["status"], "pushed");
    assert_eq!(recovered["context"]["status"], "succeeded");
    let superseded = &recovered["context"]["local_only"][0];
    assert_eq!(superseded["kind"], "superseded");
    assert_eq!(superseded["record_id"], annotation.as_str());
    let capture = fixture.captured.lock().expect("capture").clone();
    let bodies: Vec<String> = capture
        .contexts
        .iter()
        .map(|request| request.context.as_ref().expect("draft").content.clone())
        .collect();
    assert_eq!(bodies, vec!["v1", "from concurrent", "reconciled"]);
    let concurrent = context_operation(&capture.contexts[1]);
    let reconciled = context_operation(&capture.contexts[2]);
    assert_eq!(
        reconciled.parents,
        std::collections::BTreeSet::from([concurrent.id().expect("concurrent id")]),
        "the explicit revision extends the refreshed frontier"
    );
    fixture.close().await;
}

/// heddle#1928: the scope a hostile remote's attacker-signed head names. Same
/// spool, another Thread.
fn hostile_metadata(fixture: &Fixture) -> objects::object::CollaborationMetadata {
    objects::object::CollaborationMetadata {
        scope: objects::object::CollaborationScope {
            spool: fixture.spool,
            thread: Some(objects::object::ContentHash::from_bytes([92; 32])),
        },
        actor: objects::object::CollaborationActor {
            principal_id: uuid::Uuid::from_u128(93),
            agent_id: None,
        },
        mentions: vec![],
    }
}

fn hostile_signer() -> Ed25519Signer {
    Ed25519Signer::from_seed(&[91; 32]).expect("attacker key")
}

/// An attacker-signed context revision of `context` in another Thread.
fn hostile_context(
    fixture: &Fixture,
    context: uuid::Uuid,
) -> api::heddle::api::v1alpha2::SignedRecord {
    thread_api::collaboration::sign_context(
        objects::object::ContextRevision {
            version: 2,
            id: context,
            parents: vec![],
            metadata: hostile_metadata(fixture),
            anchor: objects::object::CollaborationAnchor::Repository,
            content: "attacker".into(),
            tags: vec![],
            supersedes: None,
            extracted_from: None,
            occurred_at_ms: 1,
            provenance: Some(objects::object::ContextProvenance {
                revision_id: uuid::Uuid::now_v7().to_string(),
                kind: objects::object::AnnotationKind::Rationale,
                attribution: "Attacker <attacker@test>".into(),
                source_hash: None,
                created_at_state: None,
            }),
            canonical_body: Default::default(),
        },
        &[],
        &hostile_signer(),
    )
    .expect("attacker-signed context")
}

/// An attacker-signed discussion operation in another Thread that extracts
/// no context at all.
fn hostile_discussion(fixture: &Fixture) -> api::heddle::api::v1alpha2::SignedRecord {
    thread_api::collaboration::Command {
        discussion: objects::object::DiscussionRecordId::generate(),
        operation_id: objects::object::CollaborationIdempotencyKey::new(
            uuid::Uuid::now_v7().to_string(),
        )
        .expect("operation ID"),
        metadata: hostile_metadata(fixture),
        author: objects::object::Attribution::human(objects::object::Principal::new(
            "Attacker",
            "attacker@test",
        )),
        occurred_at_ms: 1,
        body: objects::object::CollaborationOperationBodyV1::Open {
            blocking: false,
            title: "unrelated".into(),
            anchor: objects::object::CollaborationAnchor::Repository,
            visibility: objects::object::VisibilityTier::Public,
            turn: objects::object::DiscussionTurnV1::new("unrelated").expect("turn"),
            thread_ref: None,
        },
    }
    .sign(&[], &hostile_signer())
    .expect("attacker-signed discussion")
}

/// A remote's signed head cannot choose the scope of a pending append/resolve.
async fn assert_hostile_discussion_heads_refused(resolve: bool, foreign: &str, mixed: bool) {
    let fixture = Fixture::new().await;
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "example.py",
        "--thread",
        "main",
        "--body",
        "opening turn",
    ]);
    let repo = Repository::open(&fixture.clone).expect("repo");
    let view = repo::CollaborationStore::open(repo.heddle_dir())
        .expect("store")
        .materialize()
        .expect("view");
    let discussion = *view.discussions.keys().next().expect("discussion");
    let id = discussion.to_string();
    assert_push_succeeded(&fixture, &fixture.clone);
    let pending = "a realistic discussion reply with details\n".repeat(100);
    if resolve {
        fixture.run(&[
            "discuss", "resolve", &id, "--mode", "dismiss", "--reason", &pending,
        ]);
    } else {
        fixture.run(&["discuss", "reply", &id, "--body", &pending]);
    }
    let attacker = hostile_discussion(&fixture);
    let outer = thread_api::collaboration::verify(&attacker).expect("attacker record");
    let objects::object::thread_replication::ThreadOperationBody::Discussion(bytes) = outer.body
    else {
        panic!("discussion")
    };
    let envelope = objects::object::CollaborationOperationEnvelope::decode(&bytes)
        .expect("envelope")
        .operation;
    let mut metadata = envelope.metadata.expect("metadata");
    if foreign != "thread" {
        metadata.scope.thread = Some(fixture.thread_id);
    }
    if foreign == "spool" {
        metadata.scope.spool = uuid::Uuid::from_u128(94);
    }
    let hostile = thread_api::collaboration::Command {
        discussion: if foreign == "discussion" {
            envelope.discussion_id
        } else {
            discussion
        },
        operation_id: envelope.idempotency_key,
        metadata,
        author: envelope.author,
        occurred_at_ms: envelope.occurred_at_ms,
        body: envelope.body,
    }
    .sign(&[], &hostile_signer())
    .expect("hostile head");
    let mirror_path = repo
        .heddle_dir()
        .join("collaboration")
        .join("hosted-mirror.json");
    let before: Value =
        serde_json::from_slice(&std::fs::read(&mirror_path).expect("mirror")).expect("mirror JSON");
    let attempts = {
        let mut capture = fixture.captured.lock().expect("capture");
        let mut heads = if mixed {
            capture.discussion_operations.clone()
        } else {
            vec![]
        };
        heads.push(hostile);
        capture.hostile_discussion_heads = Some(heads);
        capture.received_discussion_operations.len()
    };
    let (code, result) = push_json(&fixture, &fixture.clone);
    let capture = fixture.captured.lock().expect("capture").clone();
    assert_eq!(
        capture.received_discussion_operations.len(),
        attempts,
        "the client signed and sent a discussion descendant from the hostile remote's {foreign} frontier"
    );
    let after: Value =
        serde_json::from_slice(&std::fs::read(&mirror_path).expect("mirror")).expect("mirror JSON");
    assert_eq!(before, after, "nothing prepared or retained on refusal");
    assert_eq!(code, Some(65), "{result}");
    assert_eq!(result["discussions"]["status"], "failed", "{result}");
    let issue = &result["discussions"]["local_only"][0];
    assert_eq!(issue["kind"], "not_replicable", "{result}");
    assert_eq!(issue["retry_unchanged"], false, "{result}");
    assert!(
        issue["message"]
            .as_str()
            .expect("message")
            .contains("refused to sign discussion"),
        "{result}"
    );
    fixture
        .captured
        .lock()
        .expect("capture")
        .hostile_discussion_heads = None;
    assert_push_succeeded(&fixture, &fixture.clone);
    let capture = fixture.captured.lock().expect("capture").clone();
    assert_eq!(capture.discussion_operations.len(), 2);
    let open = &capture.discussion_operations[0];
    let signed = &capture.discussion_operations[1];
    let operation = thread_api::collaboration::verify(signed).expect("signed descendant");
    assert_eq!(operation.thread, fixture.thread_id);
    assert_eq!(
        operation.parents,
        [thread_api::collaboration::operation_id(open).expect("open ID")]
            .into_iter()
            .collect()
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_append_head_rebinding_discussion_thread_is_refused() {
    assert_hostile_discussion_heads_refused(false, "thread", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_resolve_head_rebinding_discussion_thread_is_refused() {
    assert_hostile_discussion_heads_refused(true, "thread", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_append_head_of_another_discussion_is_refused() {
    assert_hostile_discussion_heads_refused(false, "discussion", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_resolve_head_of_another_spool_is_refused() {
    assert_hostile_discussion_heads_refused(true, "spool", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_append_mixed_discussion_heads_are_refused() {
    assert_hostile_discussion_heads_refused(false, "thread", true).await;
}

/// The Thread of every context operation the clone retained for delivery.
fn retained_context_threads(checkout: &Path) -> Vec<objects::object::ContentHash> {
    use prost::Message;
    let repo = Repository::open(checkout).expect("repo");
    let path = repo
        .heddle_dir()
        .join("collaboration")
        .join("hosted-context-mirror.json");
    let mirror: Value =
        serde_json::from_slice(&std::fs::read(path).expect("context mirror")).expect("mirror");
    mirror["repos"]
        .as_object()
        .expect("repos")
        .values()
        .flat_map(|repository| repository["annotations"].as_array().expect("annotations"))
        .flat_map(|entry| entry["native_operations"].as_array().expect("operations"))
        .map(|operation| {
            let bytes: Vec<u8> = serde_json::from_value(operation["signed_record"].clone())
                .expect("signed record bytes");
            let signed = api::heddle::api::v1alpha2::SignedRecord::decode(bytes.as_slice())
                .expect("signed record");
            thread_api::collaboration::verify(&signed)
                .expect("retained operation")
                .thread
        })
        .collect()
}

/// heddle#1928: a hostile remote answers the observation of a context's
/// frontier with an attacker-signed head in another Thread of the same spool.
/// The client refuses with a typed error before signing: nothing is retained
/// or sent for the other Thread. Once the remote answers honestly, the same
/// pending revision lands on the Thread the record is bound to.
///
/// `published` first publishes the record, so the pending revision is an
/// edit the client already holds a binding for; otherwise it is the record's
/// create, whose intended Thread is the one being pushed.
async fn assert_hostile_context_head_refused(
    published: bool,
    hostile: impl FnOnce(&Fixture, uuid::Uuid) -> api::heddle::api::v1alpha2::SignedRecord,
) {
    let fixture = Fixture::new().await;
    let main = fixture.thread_id;
    fixture.run(&["context", "set", "--path", "example.py", "--body", "v1"]);
    let annotation = only_context_id(&fixture, &fixture.clone, "example.py");
    let context: uuid::Uuid = annotation
        .trim_start_matches("ann-")
        .parse()
        .expect("context UUID");
    let mut expected = vec!["v1"];
    if published {
        assert_push_succeeded(&fixture, &fixture.clone);
        fixture.run(&["context", "edit", &annotation, "--body", "v2"]);
        expected.push("v2");
    }
    let hostile = hostile(&fixture, context);
    let attempts = {
        let mut capture = fixture.captured.lock().expect("capture");
        capture.hostile_context_head = Some(hostile);
        capture.context_attempts.len()
    };
    let (code, result) = push_json(&fixture, &fixture.clone);
    let capture = fixture.captured.lock().expect("capture").clone();
    let redirected: Vec<_> = capture
        .context_attempts
        .iter()
        .map(context_operation)
        .filter(|operation| operation.thread != main)
        .map(|operation| operation.thread)
        .collect();
    assert!(
        redirected.is_empty(),
        "the client signed and sent a revision into the hostile remote's Thread(s) {redirected:?} instead of {main}"
    );
    let retained = retained_context_threads(&fixture.clone);
    assert!(
        retained.iter().all(|thread| *thread == main),
        "the client retained a revision for another Thread: {retained:?}"
    );
    assert_eq!(capture.context_attempts.len(), attempts, "nothing is sent");
    assert_eq!(result["context"]["status"], "failed", "{result}");
    let item = &result["context"]["local_only"][0];
    assert_eq!(item["record_id"], annotation.as_str(), "{result}");
    assert_eq!(item["kind"], "not_replicable", "{result}");
    assert_eq!(item["retry_unchanged"], false, "{result}");
    let message = item["message"].as_str().expect("message");
    assert!(
        message.contains("refused to sign"),
        "the refusal names itself: {message}"
    );
    assert_eq!(code, Some(65), "{result}");

    fixture
        .captured
        .lock()
        .expect("capture")
        .hostile_context_head = None;
    assert_push_succeeded(&fixture, &fixture.clone);
    let capture = fixture.captured.lock().expect("capture").clone();
    let bodies: Vec<String> = capture
        .contexts
        .iter()
        .map(|request| request.context.as_ref().expect("draft").content.clone())
        .collect();
    assert_eq!(bodies, expected);
    let mut parent = None;
    for request in &capture.contexts {
        let operation = context_operation(request);
        assert_eq!(operation.thread, main, "every revision stays on main");
        assert_eq!(
            operation.parents,
            parent.into_iter().collect(),
            "and extends the honest frontier"
        );
        parent = Some(operation.id().expect("operation ID"));
    }
    fixture.close().await;
}

/// The head is another record's revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_head_of_another_context_is_refused() {
    assert_hostile_context_head_refused(false, |fixture, _| {
        hostile_context(fixture, uuid::Uuid::now_v7())
    })
    .await;
}

/// The head is a discussion operation that extracts no context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_discussion_head_without_extraction_is_refused() {
    assert_hostile_context_head_refused(false, |fixture, _| hostile_discussion(fixture)).await;
}

/// The head is a forged revision of a record the client is creating, in
/// another Thread than the one being pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_head_binding_a_new_context_to_another_thread_is_refused() {
    assert_hostile_context_head_refused(false, hostile_context).await;
}

/// The head is a forged revision of a published record in another Thread;
/// the binding the client retained from its own create refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn hostile_head_rebinding_a_published_context_is_refused() {
    assert_hostile_context_head_refused(true, hostile_context).await;
}

// ---------------------------------------------------------------------------
// heddle#1886: a Thread with two hosted heads.
// ---------------------------------------------------------------------------

/// The last JSON document a command printed (clone prints a connection line
/// first).
fn last_json(stdout: &str) -> Value {
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON output: {stdout}"));
    serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}"))
}

fn head_of(path: &Path) -> objects::object::StateId {
    Repository::open(path)
        .expect("checkout")
        .head()
        .expect("HEAD")
        .expect("checked-out State")
}

fn json_error_kind(output: &Output) -> Value {
    assert!(!output.status.success(), "command must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str::<Value>(stderr.trim())
        .unwrap_or_else(|_| panic!("stderr should be a JSON envelope: {stderr}"))["kind"]
        .clone()
}

impl Fixture {
    fn start_decision_thread(&self, capture: bool) -> PathBuf {
        self.run_at(&self.source, &["remote", "add", "origin", &self.remote()]);
        let path = self._temp.path().join("human-thread");
        self.run_at(
            &self.source,
            &["start", "decide", "--path", path.to_str().expect("path")],
        );
        if capture {
            std::fs::write(path.join("TASK.md"), "Agree on the answer\n").expect("task");
            self.run_at(&path, &["capture", "-m", "open decision"]);
            self.run_at(&path, &["push", "origin", "decide"]);
        }
        path
    }

    fn clone_decision_thread(&self) -> PathBuf {
        let path = self._temp.path().join("agent-thread");
        self.run_at(
            self._temp.path(),
            &[
                "clone",
                &self.remote(),
                path.to_str().expect("path"),
                "--thread",
                "decide",
            ],
        );
        path
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn issue_1889_captureless_thread_push_has_actionable_error() {
    let fixture = Fixture::new().await;
    let human = fixture.start_decision_thread(false);
    let before = {
        let capture = fixture.captured.lock().expect("capture");
        (capture.started.len(), capture.published.len())
    };
    let output = fixture.output_at(&human, &["--output", "json", "push", "origin", "decide"]);
    println!("push stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(json_error_kind(&output), "thread_capture_required");
    let error: Value = serde_json::from_slice(&output.stderr).expect("error envelope");
    assert_eq!(output.status.code(), Some(65));
    assert_eq!(error["primary_command"], "heddle capture -m \"...\"");
    let human_error = fixture.output_at(&human, &["push", "origin", "decide"]);
    let stderr = String::from_utf8_lossy(&human_error.stderr);
    assert!(
        stderr.contains("no capture of its own") && stderr.contains("Next: heddle capture"),
        "{stderr}"
    );
    {
        let capture = fixture.captured.lock().expect("capture");
        assert_eq!(
            (capture.started.len(), capture.published.len()),
            before,
            "refuse before hosted mutations"
        );
    }
    std::fs::write(human.join("TASK.md"), "Publish this task\n").expect("task");
    fixture.run_at(&human, &["capture", "-m", "open task"]);
    fixture.run_at(&human, &["push", "origin", "decide"]);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn issue_1889_cloned_thread_reports_real_target() {
    let fixture = Fixture::new().await;
    fixture.start_decision_thread(true);
    let agent = fixture.clone_decision_thread();
    let report =
        last_json(&fixture.run_at(&agent, &["--output", "json", "thread", "show", "decide"]));
    println!("cloned Thread: {report}");
    assert_eq!(report["target_thread"], "main", "{report}");
    assert_eq!(report["parent_thread"], "main", "{report}");
    assert_eq!(
        report["base_state"],
        head_of(&fixture.source).to_string_full()
    );
    let status = last_json(&fixture.run_at(&agent, &["--output", "json", "status"]));
    assert_eq!(status["target_thread"], "main", "{status}");
    // A stacked Thread must use its actual parent, rather than defaulting
    // every cloned Thread to main.
    let nested = fixture._temp.path().join("nested-thread");
    fixture.run_at(
        &agent,
        &["start", "nested", "--path", nested.to_str().expect("path")],
    );
    std::fs::write(nested.join("nested.txt"), "stacked task\n").expect("nested edit");
    fixture.run_at(&nested, &["capture", "-m", "stacked task"]);
    fixture.run_at(&nested, &["push", "origin", "nested"]);
    let cloned = fixture._temp.path().join("cloned-nested");
    fixture.run_at(
        fixture._temp.path(),
        &[
            "clone",
            &fixture.remote(),
            cloned.to_str().expect("path"),
            "--thread",
            "nested",
        ],
    );
    let report =
        last_json(&fixture.run_at(&cloned, &["--output", "json", "thread", "show", "nested"]));
    assert_eq!(report["target_thread"], "decide", "{report}");
    assert_eq!(report["base_state"], head_of(&agent).to_string_full());
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn issue_1889_ready_after_pulling_agent_captures() {
    let fixture = Fixture::new().await;
    let human = fixture.start_decision_thread(true);
    let agent = fixture.clone_decision_thread();
    std::fs::write(agent.join("story.txt"), "agent intermediate\n").expect("agent edit");
    fixture.run_at(&agent, &["capture", "-m", "agent intermediate"]);
    let intermediate = head_of(&agent);
    fixture.run_at(&agent, &["push", "origin", "decide"]);
    std::fs::write(agent.join("story.txt"), "agent answer\n").expect("agent edit");
    fixture.run_at(&agent, &["capture", "-m", "agent answer"]);
    fixture.run_at(&agent, &["push", "origin", "decide"]);
    // The target moves independently; ready must replay both agent captures.
    std::fs::write(fixture.source.join("main.txt"), "target advance\n").expect("main edit");
    fixture.run_at(&fixture.source, &["capture", "-m", "target advance"]);
    fixture.run_at(&fixture.source, &["push", "origin", "main"]);
    fixture.run_at(&human, &["pull", "origin", "--thread", "decide"]);
    let ready = fixture.output_at(&human, &["--output", "json", "ready"]);
    println!(
        "ready stdout: {}\nready stderr: {}",
        String::from_utf8_lossy(&ready.stdout),
        String::from_utf8_lossy(&ready.stderr)
    );
    assert!(
        ready.status.success(),
        "ready must replay fetched source history"
    );
    let report = last_json(&String::from_utf8_lossy(&ready.stdout));
    assert_eq!(report["status"], "completed", "{report}");
    let repo = Repository::open(&human).expect("human checkout");
    let state = repo
        .store()
        .get_state(&intermediate)
        .expect("state lookup")
        .expect("intermediate state");
    assert!(
        repo.store()
            .get_tree(&state.tree)
            .expect("tree lookup")
            .is_some(),
        "pull must fetch the replay parent tree"
    );
    assert_eq!(
        std::fs::read_to_string(human.join("story.txt")).expect("answer"),
        "agent answer\n"
    );
    assert_eq!(
        std::fs::read_to_string(human.join("main.txt")).expect("target edit"),
        "target advance\n"
    );
    fixture.close().await;
}

struct TwoHeads {
    fixture: Fixture,
    /// Writer A's head (the source checkout).
    a: objects::object::StateId,
    /// Writer B's head (the first clone).
    b: objects::object::StateId,
}

impl TwoHeads {
    /// Two writers publish divergent captures from one base on `main`.
    async fn publish(a_file: &str, a_body: &str, b_file: &str, b_body: &str) -> Self {
        let fixture = Fixture::new().await;
        fixture.run_at(
            &fixture.source,
            &["remote", "add", "origin", &fixture.remote()],
        );
        std::fs::write(fixture.source.join(a_file), a_body).expect("writer A edit");
        fixture.run_at(&fixture.source, &["capture", "-m", "writer A"]);
        fixture.run_at(&fixture.source, &["push", "origin"]);
        std::fs::write(fixture.clone.join(b_file), b_body).expect("writer B edit");
        fixture.run(&["capture", "-m", "writer B"]);
        fixture.run(&["push", "origin"]);
        let a = head_of(&fixture.source);
        let b = head_of(&fixture.clone);
        assert_ne!(a, b);
        assert_eq!(
            fixture
                .captured
                .lock()
                .expect("published heads")
                .source_heads()
                .len(),
            2,
            "the hosted Thread holds both writers' heads"
        );
        Self { fixture, a, b }
    }

    fn default_and_alternative(&self) -> (objects::object::StateId, objects::object::StateId) {
        (self.a.max(self.b), self.a.min(self.b))
    }

    fn clone_into(&self, name: &str) -> (PathBuf, Value) {
        let path = self.fixture._temp.path().join(name);
        let output = self.fixture.output_at(
            self.fixture._temp.path(),
            &[
                "--output",
                "json",
                "clone",
                &self.fixture.remote(),
                path.to_str().expect("clone path"),
            ],
        );
        assert!(
            output.status.success(),
            "whole-spool clone of a two-head Thread must succeed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (path, last_json(&String::from_utf8_lossy(&output.stdout)))
    }

    fn json_at(&self, path: &Path, args: &[&str]) -> Value {
        let mut full = vec!["--output", "json"];
        full.extend_from_slice(args);
        last_json(&self.fixture.run_at(path, &full))
    }
}

fn head_states(report: &Value) -> Vec<String> {
    report["heads"]
        .as_array()
        .expect("heads")
        .iter()
        .map(|head| head["state"].as_str().expect("head state").to_string())
        .collect()
}

fn story_of(heads: &TwoHeads, state: objects::object::StateId) -> &'static str {
    if state == heads.a {
        "head A\n"
    } else {
        assert_eq!(state, heads.b);
        "head B\n"
    }
}

/// Whole-spool clone: the default is the greatest State ID, stated in the
/// output, and both heads stay listed in clone and status output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn two_head_thread_whole_spool_clone_checks_out_default_and_lists_alternatives() {
    let heads = TwoHeads::publish("story.txt", "head A\n", "story.txt", "head B\n").await;
    let (default, alternative) = heads.default_and_alternative();
    let (fresh, clone) = heads.clone_into("fresh");
    println!("clone: {clone}");
    assert_eq!(head_of(&fresh), default);
    assert_eq!(
        std::fs::read_to_string(fresh.join("story.txt")).expect("story"),
        story_of(&heads, default)
    );
    let reported = &clone["source_heads"][0];
    assert_eq!(reported["thread"], "main");
    assert_eq!(reported["selected_by"], "greatest_state_id");
    assert_eq!(reported["current"], default.to_string_full());
    assert_eq!(
        head_states(reported),
        vec![default.to_string_full(), alternative.to_string_full()]
    );
    assert_eq!(clone["next_action"], "heddle resolve --heads");
    // Deterministic: another clone checks out the same head, and the human
    // output says which head and why.
    let again = heads.fixture._temp.path().join("fresh-again");
    let text = heads.fixture.run_at(
        heads.fixture._temp.path(),
        &[
            "clone",
            &heads.fixture.remote(),
            again.to_str().expect("clone path"),
        ],
    );
    println!("clone text:\n{text}");
    assert_eq!(head_of(&again), default);
    assert!(
        text.contains(&format!(
            "thread 'main' has 2 concurrent source heads; checked out {}",
            default.to_string_full()
        )),
        "{text}"
    );
    assert!(text.contains("greatest State ID"), "{text}");
    assert!(
        text.contains(&format!(
            "heddle resolve --pick {}",
            alternative.to_string_full()
        )),
        "{text}"
    );
    let listed = heads.fixture.run_at(&again, &["resolve", "--heads"]);
    println!("resolve --heads text:\n{listed}");
    let status_text = heads.fixture.run_at(&again, &["status"]);
    println!("status text:\n{status_text}");
    assert!(
        status_text.contains("2 concurrent source heads; pick or merge one"),
        "{status_text}"
    );
    assert!(
        status_text.contains("heddle resolve --heads"),
        "{status_text}"
    );

    // Status keeps the unresolved alternative visible.
    let status = heads.json_at(&fresh, &["status"]);
    println!("status: {status}");
    assert_eq!(
        head_states(&status["alternative_heads"]),
        vec![default.to_string_full(), alternative.to_string_full()]
    );
    assert_eq!(status["recommended_action"], "heddle resolve --heads");
    assert!(
        status["blockers"]
            .as_array()
            .expect("blockers")
            .iter()
            .any(|blocker| blocker
                .as_str()
                .is_some_and(|text| text.contains("unresolved alternative source heads"))),
        "{status}"
    );
    // Readiness agrees: the Thread is blocked until a pick or merge.
    let ready = heads
        .fixture
        .output_at(&fresh, &["--output", "json", "ready"]);
    println!(
        "ready: {}\nstderr: {}",
        String::from_utf8_lossy(&ready.stdout),
        String::from_utf8_lossy(&ready.stderr)
    );
    let ready = last_json(&String::from_utf8_lossy(&ready.stdout));
    assert_eq!(ready["status"], "blocked", "{ready}");
    assert_eq!(ready["next_action"], "heddle resolve --heads", "{ready}");
    heads.fixture.close().await;
}

/// `pull --thread` on a two-head Thread succeeds and keeps each writer's own
/// head checked out, reporting the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn two_head_thread_pull_keeps_each_writers_head() {
    let heads = TwoHeads::publish("story.txt", "head A\n", "story.txt", "head B\n").await;
    for (checkout, own) in [
        (&heads.fixture.source, heads.a),
        (&heads.fixture.clone, heads.b),
    ] {
        let pulled = heads.json_at(checkout, &["pull", "origin", "--thread", "main"]);
        println!("pull: {pulled}");
        assert_eq!(head_of(checkout), own);
        assert_eq!(
            std::fs::read_to_string(checkout.join("story.txt")).expect("story"),
            story_of(&heads, own)
        );
        assert_eq!(pulled["source_heads"]["selected_by"], "local_tip");
        assert_eq!(pulled["source_heads"]["current"], own.to_string_full());
        assert_eq!(head_states(&pulled["source_heads"]).len(), 2);
        assert_eq!(pulled["next_action"], "heddle resolve --heads");
        let status = heads.json_at(checkout, &["status"]);
        assert_eq!(head_states(&status["alternative_heads"]).len(), 2);
    }
    heads.fixture.close().await;
}

/// Select head B by a unique prefix and pick it: the Thread takes exactly its
/// tree, every head stays in ancestry, status clears, and publishing the pick
/// collapses the hosted heads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn two_head_thread_pick_resolves_every_head() {
    let heads = TwoHeads::publish("story.txt", "head A\n", "story.txt", "head B\n").await;
    let (default, alternative) = heads.default_and_alternative();
    let (fresh, _) = heads.clone_into("fresh");

    // A selector naming no head is a typed refusal that changes nothing.
    for selector in ["hs-zzzzzzzz", "hs-"] {
        let refused = heads
            .fixture
            .output_at(&fresh, &["--output", "json", "resolve", "--pick", selector]);
        assert_eq!(json_error_kind(&refused), "unknown_source_head");
    }
    assert_eq!(head_of(&fresh), default);

    let listed = heads.json_at(&fresh, &["resolve", "--heads"]);
    println!("heads: {listed}");
    assert_eq!(
        head_states(&listed["source_heads"]),
        vec![default.to_string_full(), alternative.to_string_full()]
    );
    let prefix = &alternative.to_string_full()[..20];
    let picked = heads.json_at(&fresh, &["resolve", "--pick", prefix]);
    println!("pick: {picked}");
    let resolution = &picked["head_resolution"];
    assert_eq!(resolution["mode"], "pick");
    assert_eq!(resolution["selected"], alternative.to_string_full());
    assert_eq!(
        resolution["parents"],
        serde_json::json!([alternative.to_string_full(), default.to_string_full()])
    );
    assert!(picked["source_heads"].is_null(), "{picked}");
    assert_eq!(picked["next_action"], "heddle push");
    assert_eq!(
        std::fs::read_to_string(fresh.join("story.txt")).expect("story"),
        story_of(&heads, alternative)
    );
    let status = heads.json_at(&fresh, &["status"]);
    assert!(status.get("alternative_heads").is_none(), "{status}");
    assert_ne!(status["recommended_action"], "heddle resolve --heads");

    // Publishing the pick collapses the hosted heads to the resolution.
    heads.fixture.run_at(&fresh, &["push", "origin"]);
    let hosted = heads
        .fixture
        .captured
        .lock()
        .expect("published heads")
        .source_heads();
    assert_eq!(hosted.len(), 1, "the pick retires both hosted heads");
    let (after, clone) = heads.clone_into("after-pick");
    assert!(clone.get("source_heads").is_none(), "{clone}");
    assert_eq!(head_of(&after), head_of(&fresh));
    heads.fixture.close().await;
}

/// A conflicting merge of the selected head stops in merge state; resolving
/// the conflict finishes one capture naming both heads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn two_head_thread_conflicted_merge_finishes_with_resolve() {
    let heads = TwoHeads::publish("story.txt", "head A\n", "story.txt", "head B\n").await;
    let (default, alternative) = heads.default_and_alternative();
    let (fresh, _) = heads.clone_into("fresh");
    let merged = heads.json_at(
        &fresh,
        &["resolve", "--merge", &alternative.to_string_full()],
    );
    println!("merge: {merged}");
    assert_eq!(merged["conflict_paths"], serde_json::json!(["story.txt"]));
    assert!(merged["head_resolution"]["state"].is_null(), "{merged}");
    let status = heads.json_at(&fresh, &["status"]);
    assert_eq!(status["recommended_action"], "heddle continue");
    let resolved = heads.json_at(&fresh, &["resolve", "--all", "--theirs"]);
    println!("resolved: {resolved}");
    assert_eq!(resolved["continued"], true);
    assert_eq!(
        std::fs::read_to_string(fresh.join("story.txt")).expect("story"),
        story_of(&heads, alternative)
    );
    let tip = Repository::open(&fresh)
        .expect("fresh")
        .store()
        .get_state(&head_of(&fresh))
        .expect("tip")
        .expect("tip State");
    assert_eq!(tip.parents, vec![default, alternative]);
    let status = heads.json_at(&fresh, &["status"]);
    assert!(status.get("alternative_heads").is_none(), "{status}");
    heads.fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn two_head_thread_merges_the_selected_head() {
    let heads = TwoHeads::publish("a.txt", "from A\n", "b.txt", "from B\n").await;
    let (default, alternative) = heads.default_and_alternative();
    let (fresh, _) = heads.clone_into("fresh");
    assert_eq!(head_of(&fresh), default);

    let merged = heads.json_at(
        &fresh,
        &["resolve", "--merge", &alternative.to_string_full()],
    );
    println!("merge: {merged}");
    let resolution = &merged["head_resolution"];
    assert_eq!(resolution["mode"], "merge");
    assert_eq!(
        resolution["parents"],
        serde_json::json!([default.to_string_full(), alternative.to_string_full()])
    );
    assert!(merged["source_heads"].is_null(), "{merged}");
    assert_eq!(
        std::fs::read_to_string(fresh.join("a.txt")).expect("A"),
        "from A\n"
    );
    assert_eq!(
        std::fs::read_to_string(fresh.join("b.txt")).expect("B"),
        "from B\n"
    );
    let status = heads.json_at(&fresh, &["status"]);
    assert!(status["alternative_heads"].is_null(), "{status}");
    heads.fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn single_head_thread_reports_no_alternatives() {
    let fixture = Fixture::new().await;
    let fresh = fixture._temp.path().join("fresh");
    let output = fixture.run_at(
        fixture._temp.path(),
        &[
            "--output",
            "json",
            "clone",
            &fixture.remote(),
            fresh.to_str().expect("fresh"),
        ],
    );
    let clone = last_json(&output);
    assert!(clone.get("source_heads").is_none(), "{clone}");
    assert!(clone.get("next_action").is_none(), "{clone}");
    let status = last_json(&fixture.run_at(&fresh, &["--output", "json", "status"]));
    assert!(status.get("alternative_heads").is_none(), "{status}");
    let listed = last_json(&fixture.run_at(&fresh, &["--output", "json", "resolve", "--heads"]));
    assert!(listed["source_heads"].is_null(), "{listed}");
    let pick = fixture.output_at(&fresh, &["--output", "json", "resolve", "--pick", "hs-0"]);
    assert_eq!(json_error_kind(&pick), "no_alternative_source_heads");
    fixture.close().await;
}

/// heddle#1948: context history must select annotations when discussions share
/// the Spool. Reproduce a clone inheriting context, then publishing a child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn inherited_annotation_and_public_discussion_push() {
    let fixture = Fixture::new().await;
    fixture.run(&[
        "context",
        "set",
        "--path",
        "example.py",
        "--body",
        "inherited context",
    ]);
    assert_push_succeeded(&fixture, &fixture.clone);
    let annotated = fixture._temp.path().join("annotated-clone");
    fixture.run_at(
        fixture._temp.path(),
        &[
            "clone",
            &fixture.remote(),
            annotated.to_str().expect("clone path"),
        ],
    );
    let path = fixture.run_at(&annotated, &["start", "mixed", "--print-cd-path"]);
    let child = PathBuf::from(path.trim());
    std::fs::write(child.join("story.txt"), "child source\n").expect("edit child");
    fixture.run_at(&child, &["capture", "-m", "child edit"]);
    assert_push_succeeded(&fixture, &child);
    fixture.run_at(
        &child,
        &[
            "discuss",
            "new",
            "--path",
            "example.py",
            "--thread",
            "mixed",
            "--visibility",
            "public",
            "--body",
            "opening",
        ],
    );
    let repo = Repository::open(&child).expect("child repo");
    let view = repo::CollaborationStore::open(repo.heddle_dir())
        .expect("store")
        .materialize()
        .expect("view");
    let id = view
        .discussions
        .keys()
        .next()
        .expect("discussion")
        .to_string();
    fixture.run_at(&child, &["discuss", "reply", &id, "--body", "reply"]);
    fixture.run_at(
        &child,
        &[
            "discuss", "resolve", &id, "--mode", "dismiss", "--reason", "answered",
        ],
    );
    assert_push_succeeded(&fixture, &child);
    let count = fixture
        .captured
        .lock()
        .expect("server")
        .discussion_operations
        .len();
    assert_eq!(count, 3, "open, reply, resolve");
    assert_push_succeeded(&fixture, &child);
    assert_eq!(
        fixture
            .captured
            .lock()
            .expect("server")
            .discussion_operations
            .len(),
        count
    );
    fixture.close().await;
}

/// A context-only interruption must leave already published discussion
/// originals on the server exactly once, including across a human retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "heddle#1961 Part 2: route fresh HYBRID witness evidence through Fetch"]
async fn partial_context_push_retry_keeps_published_discussions_once() {
    let fixture = Fixture::new().await;
    fixture.run(&["context", "set", "--path", "example.py", "--body", "v1"]);
    assert_push_succeeded(&fixture, &fixture.clone);
    let annotation = only_context_id(&fixture, &fixture.clone, "example.py");
    fixture.run(&[
        "context",
        "edit",
        &annotation,
        "--body",
        &"realistic context\n".repeat(100),
    ]);
    fixture.capture();
    fixture.run(&[
        "discuss",
        "new",
        "--path",
        "example.py",
        "--thread",
        "main",
        "--visibility",
        "public",
        "--body",
        "opening",
    ]);
    let repo = Repository::open(&fixture.clone).expect("repo");
    let view = repo::CollaborationStore::open(repo.heddle_dir())
        .expect("store")
        .materialize()
        .expect("view");
    let id = view
        .discussions
        .keys()
        .next()
        .expect("discussion")
        .to_string();
    fixture.run(&["discuss", "reply", &id, "--body", "reply"]);
    fixture.run(&[
        "discuss", "resolve", &id, "--mode", "dismiss", "--reason", "answered",
    ]);
    fixture
        .captured
        .lock()
        .expect("interrupt")
        .interrupt_next_context = true;
    let (code, partial) = push_json(&fixture, &fixture.clone);
    assert_eq!(code, Some(75), "{partial}");
    assert_eq!(partial["status"], "partial");
    assert_eq!(partial["success"], false);
    assert_eq!(partial["source"]["status"], "succeeded");
    assert_eq!(partial["discussions"]["status"], "succeeded");
    assert_eq!(partial["discussions"]["count"], 1);
    assert_eq!(partial["context"]["status"], "failed");
    assert_eq!(partial["context"]["unsent"][0]["record_id"], annotation);
    assert_eq!(partial["context"]["unsent"][0]["retry_unchanged"], true);
    assert!(
        partial["context"]["unsent"][0]["guidance"]
            .as_str()
            .expect("guidance")
            .contains("Unsent work stays local")
    );
    assert_eq!(
        json_recovery_commands(&partial),
        ["heddle push origin --thread main"]
    );
    assert_eq!(
        fixture
            .captured
            .lock()
            .expect("server")
            .discussion_operations
            .len(),
        3
    );
    let attempts = fixture
        .captured
        .lock()
        .expect("server")
        .received_discussion_operations
        .len();
    fixture
        .captured
        .lock()
        .expect("interrupt")
        .interrupt_next_context = true;
    let human = fixture.output_at(&fixture.clone, &["push", "origin"]);
    assert_eq!(human.status.code(), Some(75));
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("partial push to main"), "{text}");
    assert!(text.contains("source: published"), "{text}");
    assert!(text.contains("discussions: published"), "{text}");
    assert!(text.contains("Unsent work stays local"), "{text}");
    assert!(
        text.contains("recovery: heddle push origin --thread main"),
        "{text}"
    );
    assert_push_succeeded(&fixture, &fixture.clone);
    let capture = fixture.captured.lock().expect("server").clone();
    assert_eq!(
        capture.discussion_operations.len(),
        3,
        "retry must not duplicate open/reply/resolve"
    );
    assert_eq!(
        capture.received_discussion_operations.len(),
        attempts,
        "already acknowledged discussions are not resent"
    );
    assert_eq!(
        capture.contexts.len(),
        2,
        "only the pending revision is published"
    );
    fixture.close().await;
}
