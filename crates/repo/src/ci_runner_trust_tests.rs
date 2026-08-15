// SPDX-License-Identifier: Apache-2.0

use chrono::{Duration, TimeZone, Utc};
use crypto::{Ed25519Signer, Signer};
use objects::object::{KeyBinding, KeyBindingRegistry, KeyRole, StateSignature};
use tempfile::TempDir;

use super::{CiRunnerTrustSet, Repository};
use crate::{AuthorshipVerification, TrustedKey};

fn setup() -> (TempDir, Repository) {
    let temp = TempDir::new().expect("temp dir");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    (temp, repo)
}

fn trusted_key(signer: &Ed25519Signer) -> TrustedKey {
    TrustedKey {
        algorithm: signer.algorithm().to_string(),
        public_key: hex::encode(signer.public_key()),
        label: None,
    }
}

fn sign_binding(signer: &Ed25519Signer, mut binding: KeyBinding) -> KeyBinding {
    binding.added_by_sig.signature = hex::encode(
        signer
            .sign(&binding.canonical_signing_payload())
            .expect("sign binding"),
    );
    binding
}

fn binding_with(
    signer: &Ed25519Signer,
    role: KeyRole,
    identity: &str,
    valid_from: chrono::DateTime<Utc>,
    revoked_at: Option<chrono::DateTime<Utc>>,
) -> KeyBinding {
    let public_key = hex::encode(signer.public_key());
    sign_binding(
        signer,
        KeyBinding {
            algorithm: signer.algorithm().to_string(),
            public_key: public_key.clone(),
            identity_ref: identity.to_string(),
            role,
            added_by_sig: StateSignature {
                algorithm: signer.algorithm().to_string(),
                public_key,
                signature: String::new(),
            },
            valid_from,
            revoked_at,
            delegated_from: None,
        },
    )
}

fn past() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_000, 0).unwrap()
}

fn in_window_ci_runner(signer: &Ed25519Signer, identity: &str) -> KeyBinding {
    binding_with(signer, KeyRole::CiRunner, identity, past(), None)
}

fn signed_registry(authority: &Ed25519Signer, bindings: Vec<KeyBinding>) -> KeyBindingRegistry {
    let mut registry = KeyBindingRegistry::new(
        0,
        None,
        StateSignature {
            algorithm: authority.algorithm().to_string(),
            public_key: hex::encode(authority.public_key()),
            signature: String::new(),
        },
        bindings,
    );
    registry.authority_signature.signature = hex::encode(
        authority
            .sign(
                &registry
                    .canonical_checkpoint_signing_payload()
                    .expect("registry payload"),
            )
            .expect("sign registry"),
    );
    registry
}

fn resolve(
    repo: &Repository,
    authority: &Ed25519Signer,
    bindings: Vec<KeyBinding>,
) -> CiRunnerTrustSet {
    let registry = signed_registry(authority, bindings);
    repo.resolve_ci_runner_trust_set(&registry, &trusted_key(authority))
}

fn hex_key(signer: &Ed25519Signer) -> String {
    hex::encode(signer.public_key())
}

#[test]
fn unrevoked_in_window_ci_runner_is_included() {
    let (_temp, repo) = setup();
    let authority = Ed25519Signer::generate().expect("authority");
    let runner = Ed25519Signer::generate().expect("runner");
    let binding = in_window_ci_runner(&runner, "identity:runner");

    let set = resolve(&repo, &authority, vec![binding]);

    assert_eq!(set.len(), 1);
    assert!(set.contains(runner.algorithm(), &hex_key(&runner)));
    assert_eq!(set.entries()[0].identity_ref, "identity:runner");
}

#[test]
fn revoked_ci_runner_is_excluded() {
    let (_temp, repo) = setup();
    let authority = Ed25519Signer::generate().expect("authority");
    let runner = Ed25519Signer::generate().expect("runner");
    let revoked = binding_with(
        &runner,
        KeyRole::CiRunner,
        "identity:revoked",
        past(),
        Some(Utc.timestamp_opt(1_500, 0).unwrap()),
    );

    let set = resolve(&repo, &authority, vec![revoked]);

    assert!(
        set.is_empty(),
        "revoked ci-runner must fail closed to an empty trust set, got {set:?}"
    );
    assert!(
        !set.contains(runner.algorithm(), &hex_key(&runner)),
        "revoked ci-runner key must not be a trusted verdict signer"
    );
}

#[test]
fn out_of_window_ci_runner_is_excluded() {
    let (_temp, repo) = setup();
    let authority = Ed25519Signer::generate().expect("authority");
    let not_yet_valid = Ed25519Signer::generate().expect("future runner");
    let future = binding_with(
        &not_yet_valid,
        KeyRole::CiRunner,
        "identity:future",
        Utc::now() + Duration::days(1),
        None,
    );

    let set = resolve(&repo, &authority, vec![future]);

    assert!(
        set.is_empty(),
        "not-yet-valid ci-runner must fail closed to an empty trust set, got {set:?}"
    );
    assert!(
        !set.contains(not_yet_valid.algorithm(), &hex_key(&not_yet_valid)),
        "out-of-window ci-runner key must not be a trusted verdict signer"
    );
}

#[test]
fn wrong_role_binding_is_excluded() {
    let (_temp, repo) = setup();
    let authority = Ed25519Signer::generate().expect("authority");
    let author = Ed25519Signer::generate().expect("author");
    let reviewer = Ed25519Signer::generate().expect("reviewer");
    let author_binding = binding_with(&author, KeyRole::Author, "identity:author", past(), None);
    let reviewer_binding = binding_with(
        &reviewer,
        KeyRole::Reviewer,
        "identity:reviewer",
        past(),
        None,
    );

    let set = resolve(&repo, &authority, vec![author_binding, reviewer_binding]);

    assert!(
        set.is_empty(),
        "author/reviewer bindings must not leak into the ci-runner trust set, got {set:?}"
    );
    assert!(!set.contains(author.algorithm(), &hex_key(&author)));
    assert!(!set.contains(reviewer.algorithm(), &hex_key(&reviewer)));
}

#[test]
fn empty_registry_yields_empty_trust_set() {
    let (_temp, repo) = setup();
    let authority = Ed25519Signer::generate().expect("authority");

    let set = resolve(&repo, &authority, Vec::new());

    assert!(set.is_empty());
    assert_eq!(set, CiRunnerTrustSet::empty());
}

#[test]
fn mixed_registry_includes_only_the_valid_ci_runner() {
    let (_temp, repo) = setup();
    let authority = Ed25519Signer::generate().expect("authority");
    let valid = Ed25519Signer::generate().expect("valid runner");
    let revoked = Ed25519Signer::generate().expect("revoked runner");
    let future = Ed25519Signer::generate().expect("future runner");
    let author = Ed25519Signer::generate().expect("author");
    let bindings = vec![
        in_window_ci_runner(&valid, "identity:valid"),
        binding_with(
            &revoked,
            KeyRole::CiRunner,
            "identity:revoked",
            past(),
            Some(Utc.timestamp_opt(1_500, 0).unwrap()),
        ),
        binding_with(
            &future,
            KeyRole::CiRunner,
            "identity:future",
            Utc::now() + Duration::days(1),
            None,
        ),
        binding_with(&author, KeyRole::Author, "identity:author", past(), None),
    ];

    let set = resolve(&repo, &authority, bindings);

    assert_eq!(
        set.len(),
        1,
        "only the unrevoked in-window ci-runner belongs"
    );
    assert!(set.contains(valid.algorithm(), &hex_key(&valid)));
    assert!(!set.contains(revoked.algorithm(), &hex_key(&revoked)));
    assert!(!set.contains(future.algorithm(), &hex_key(&future)));
    assert!(!set.contains(author.algorithm(), &hex_key(&author)));
    assert_eq!(set.entries()[0].identity_ref, "identity:valid");
}

#[test]
fn unauthorized_registry_fails_closed_to_empty() {
    let (_temp, repo) = setup();
    let authority = Ed25519Signer::generate().expect("authority");
    let other = Ed25519Signer::generate().expect("other authority");
    let runner = Ed25519Signer::generate().expect("runner");
    let registry = signed_registry(
        &authority,
        vec![in_window_ci_runner(&runner, "identity:runner")],
    );

    let set = repo.resolve_ci_runner_trust_set(&registry, &trusted_key(&other));

    assert!(
        set.is_empty(),
        "a registry not signed by the trusted authority must not yield runner keys, got {set:?}"
    );
}

#[test]
fn membership_agrees_with_known_actor_resolution() {
    let (_temp, repo) = setup();
    let authority = Ed25519Signer::generate().expect("authority");
    let valid = Ed25519Signer::generate().expect("valid runner");
    let revoked = Ed25519Signer::generate().expect("revoked runner");
    let bindings = vec![
        in_window_ci_runner(&valid, "identity:valid"),
        binding_with(
            &revoked,
            KeyRole::CiRunner,
            "identity:revoked",
            past(),
            Some(Utc.timestamp_opt(1_500, 0).unwrap()),
        ),
    ];
    let registry = signed_registry(&authority, bindings);
    let trusted = trusted_key(&authority);
    let set = repo.resolve_ci_runner_trust_set(&registry, &trusted);

    for binding in &registry.bindings {
        let resolved = repo.verify_known_actor_key(
            &binding.algorithm,
            &binding.public_key,
            KeyRole::CiRunner,
            &registry,
            &trusted,
        );
        let in_set = set.contains(&binding.algorithm, &binding.public_key);
        match resolved {
            AuthorshipVerification::Verified(_) => assert!(
                in_set,
                "Verified ci-runner {} must be in the trust set",
                binding.identity_ref
            ),
            other => assert!(
                !in_set,
                "{} resolved as {other:?} must not be in the trust set",
                binding.identity_ref
            ),
        }
    }
}
