//! The CLI trust ceremony updates an enrolled repository with a retained signed
//! history checkpoint; a deployment pin alone cannot silently replace that row.
#![cfg(feature = "client")]
use std::process::Command;

use crypto::{Ed25519Signer, Signer};
use prost::Message;
use repo::thread_replication::hosted_trust::{self, HostedTrust, RootSelection, SystemClock};
#[test]
fn auth_trust_replace_command_cas_preserves_enrolled_repository_checkpoint() {
    let directory = tempfile::tempdir().expect("repo");
    let home = tempfile::tempdir().expect("home");
    let repository = repo::Repository::init(directory.path()).expect("repository");
    let authority = "https://weft.example.test";
    let a = Ed25519Signer::from_seed(&[7; 32]).expect("A");
    let b = Ed25519Signer::from_seed(&[8; 32]).expect("B");
    let root = RootSelection {
        authority: authority.into(),
        root_id: "descriptor-root-1".into(),
        public_key: a.public_key().try_into().expect("key"),
    };
    hosted_trust::select_root(repository.heddle_dir(), &root).expect("enrolled root");
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../thread-api/tests/fixtures/hybrid-alpha32.json"
    ))
    .expect("release fixture");
    let bytes = hex::decode(
        fixture["signed_vectors"]["retired_set"]["wire_hex"]
            .as_str()
            .expect("set"),
    )
    .expect("wire");
    let mut set = api::heddle::api::common::SignedHostedWitnessSetV1::decode(bytes.as_slice())
        .expect("signed set");
    let body = set.body.as_mut().expect("body");
    let now = chrono::Utc::now().timestamp_millis();
    body.issued_at_unix_millis = now - 1000;
    body.valid_until_unix_millis = now + 240_000;
    for e in &mut body.entries {
        if e.state == 1 {
            e.active_until_unix_millis = now + 300_000;
        }
    }
    let bytes = api::witness_trust::set_signing_bytes(body).expect("canonical bytes");
    set.body_digest = api::hybrid_codec::hash(&[&bytes]);
    set.root_signature = a.sign(&bytes).expect("fresh root signature");
    let trust =
        HostedTrust::open(repository.heddle_dir(), authority, SystemClock).expect("retained trust");
    trust.mutate(&set, |_| Ok(())).expect("enrolled checkpoint");
    let before = trust.snapshot().expect("before");
    std::fs::write(home.path().join("descriptor-trust.toml"),format!("version = 1\n[servers.\"{authority}\"]\nkey_id = \"descriptor-root-1\"\npublic_key = \"{}\"\nfirst_verified_unix_millis = {now}\n",hex::encode(a.public_key()))).expect("automatic pin");
    let config = home.path().join("config.toml");
    std::fs::write(&config, "").expect("isolated config");
    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .current_dir(directory.path())
        .env("HEDDLE_HOME", home.path())
        .env("HEDDLE_CONFIG", &config)
        .args([
            "auth",
            "trust",
            "replace",
            "--server",
            authority,
            "--expect-current-public-key",
            &hex::encode(a.public_key()),
            "--key-id",
            "descriptor-root-B",
            "--public-key",
            &hex::encode(b.public_key()),
            "--repository",
        ])
        .arg(directory.path())
        .args(["--output", "json"])
        .output()
        .expect("actual CLI command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON outcome");
    assert_eq!(report["output_kind"], "auth_trust_replace");
    let after = trust
        .snapshot()
        .expect("retained repository client reads replacement");
    assert_eq!(after.root.public_key.as_slice(), b.public_key());
    assert_eq!(after.root_epoch, before.root_epoch + 1);
    assert_eq!(
        after.previous.as_ref().expect("prior checkpoint").body(),
        before.previous.as_ref().expect("before checkpoint").body()
    );
    assert_eq!(after.clock_floor_millis, before.clock_floor_millis);
    assert!(
        trust.mutate(&set, |_| Ok(())).is_err(),
        "staged A set fails after actual CLI replacement"
    );
}
