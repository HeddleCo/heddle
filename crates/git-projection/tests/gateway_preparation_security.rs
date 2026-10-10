// SPDX-License-Identifier: Apache-2.0
//! Independent adversarial tests for explicit prepare / external-sign / bind.
//! Synthetic local keys exercise byte binding, never hosted authority.
use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use heddle_git_projection::{
    GitProjectionError, GitProjectionResult,
    gateway_view::{HistoryTip, ViewLimits, export_public_git_history},
    gateway_write::{
        LocalPush, PreparedGitPush, PushAuthor, PushScope, PushUpdate, WriteLimits,
        prepare_git_push,
    },
};
use objects::{
    object::{
        Attribution, Blob, CollaborationActor, Principal, Redaction, StateId, StateVisibility,
        VisibilityTier,
        thread_replication::{SourceAuthor, ThreadOperation},
    },
    store::ObjectStore,
};
use repo::{Repository, thread_replication::ThreadReplica};
use sley::{GitObjectType, ObjectId, Repository as GitRepository};
use std::{sync::mpsc, time::Duration};

struct Fixture {
    _temp: tempfile::TempDir,
    native: Repository,
    git: GitRepository,
    base: StateId,
    old: ObjectId,
    new: ObjectId,
    signer: Ed25519Signer,
}
fn attribution() -> Attribution {
    Attribution::human(Principal::new("Synthetic owner", "owner@example.invalid"))
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("fixture");
    let native = Repository::init_default(temp.path().join("native")).expect("native");
    std::fs::write(native.root().join("base.txt"), b"public base\n").expect("file");
    let base = native
        .snapshot_with_attribution(Some("base".into()), None, attribution())
        .expect("snapshot")
        .state_id;
    let git = GitRepository::init_bare(temp.path().join("quarantine.git")).expect("quarantine");
    let mapping = export_public_git_history(
        &native,
        &git,
        &[HistoryTip {
            thread: "main",
            state: base,
        }],
        &["main"],
        ViewLimits::default(),
    )
    .expect("base history");
    let old = mapping.get_git(&base).expect("old");
    let blob = git
        .write_raw_object(GitObjectType::Blob, b"new public content\n".to_vec())
        .expect("blob");
    let mut tree = b"100644 public.txt\0".to_vec();
    tree.extend_from_slice(blob.as_bytes());
    let tree = git
        .write_raw_object(GitObjectType::Tree, tree)
        .expect("tree");
    let mut new = old;
    for index in 0..2 {
        new=git.write_raw_object(GitObjectType::Commit,format!("tree {tree}\nparent {new}\nauthor Untrusted Git <untrusted@example.invalid> 1700000000 +0000\ncommitter Untrusted Git <untrusted@example.invalid> 1700000000 +0000\n\nexternal preparation {index}\n").into_bytes()).expect("commit");
    }
    let signer = native
        .native_thread_signer(&native.native_thread("main").expect("thread"))
        .expect("existing synthetic signer");
    Fixture {
        _temp: temp,
        native,
        git,
        base,
        old,
        new,
        signer,
    }
}
fn deny(message: &str) -> GitProjectionError {
    GitProjectionError::Git(message.into())
}
fn check_scope(f: &Fixture, scope: &PushScope) -> GitProjectionResult<()> {
    if scope.actor != "external-fixture"
        || scope.publisher.as_slice() != f.signer.public_key()
        || scope.policy_generation != "fixture-policy"
        || scope.source_author != SourceAuthor::LocalKey
    {
        return Err(deny("explicit actor/key/policy scope differs"));
    }
    Ok(())
}
fn prepare(f: &Fixture) -> PreparedGitPush {
    let update = PushUpdate {
        thread: "main".into(),
        old: f.old,
        new: f.new,
    };
    prepare_git_push(
        &f.native,
        &f.git,
        LocalPush {
            update: &update,
            expected_native: f.base,
            policy_generation: "fixture-policy",
        },
        PushAuthor {
            actor: "external-fixture",
            publisher: f.signer.public_key().try_into().expect("key"),
            source_author: &SourceAuthor::LocalKey,
        },
        WriteLimits::default(),
        |scope| check_scope(f, scope),
    )
    .expect("unsigned preparation")
}
fn sign(plan: &PreparedGitPush, signer: &Ed25519Signer) -> Vec<SignedOperation> {
    plan.operations()
        .iter()
        .map(|op| SignedOperation::sign(op, signer).expect("external signing"))
        .collect()
}
fn authority(replica: &ThreadReplica, operation: &ThreadOperation) -> GitProjectionResult<()> {
    replica
        .verify_local_source_owner(operation)
        .map_err(|e| deny(&e.to_string()))
}
fn unchanged(f: &Fixture) {
    assert_eq!(
        f.native
            .native_thread("main")
            .expect("thread")
            .projection()
            .expect("view")
            .source_heads,
        vec![f.base]
    );
}
fn hide(f: &Fixture, id: StateId) {
    f.native
        .put_state_visibility(StateVisibility {
            state: id,
            tier: VisibilityTier::Private {
                scope_label: "test".into(),
            },
            embargo_until: None,
            declarer: Principal::new("Synthetic owner", "owner@example.invalid"),
            declared_at: chrono::Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("narrow visibility");
}

#[test]
fn preparation_and_signature_binding_do_not_accept_native_or_hosted_git_writes() {
    let f = fixture();
    let plan = prepare(&f);
    unchanged(&f);
    let generation = plan.scope().expected_generation;
    for op in plan.operations() {
        assert!(
            f.native
                .native_thread("main")
                .expect("thread")
                .operation(&op.id().expect("id"))
                .expect("lookup")
                .is_none()
        );
    }
    let signed = sign(&plan, &f.signer);
    let bound = plan
        .bind_signed(&f.native, signed, |scope| check_scope(&f, scope), authority)
        .expect("bound exact signatures");
    unchanged(&f);
    assert_eq!(
        f.native
            .native_thread("main")
            .expect("thread")
            .generation()
            .expect("generation"),
        generation
    );
    assert_eq!(bound.receipt().new_git, f.new.to_string());
    assert!(
        bound.require_hosted_git_acceptance().is_err(),
        "sender signatures cannot substitute for receiver expected-old CAS"
    );
    for signed in bound.originals() {
        assert!(
            f.native
                .native_thread("main")
                .expect("thread")
                .operation(&signed.verify().expect("verify").id().expect("id"))
                .expect("lookup")
                .is_none()
        );
    }
}

#[test]
fn omitted_reordered_wrong_key_and_substituted_operations_are_rejected() {
    for case in [
        "omitted",
        "reordered",
        "wrong-key",
        "bad-signature",
        "substituted-author",
    ] {
        let f = fixture();
        let plan = prepare(&f);
        let mut signed = sign(&plan, &f.signer);
        match case {
            "omitted" => {
                signed.pop();
            }
            "reordered" => signed.reverse(),
            "wrong-key" => {
                let wrong = Ed25519Signer::from_seed(&[177; 32]).expect("wrong synthetic signer");
                let mut op = plan.operations()[0].clone();
                op.publisher = wrong.public_key().try_into().expect("wrong key");
                signed[0] =
                    SignedOperation::sign(&op, &wrong).expect("valid substituted publisher");
            }
            "bad-signature" => {
                signed[0].signature[0] ^= 1;
            }
            "substituted-author" => {
                let mut op = plan.operations()[0].clone();
                let objects::object::thread_replication::ThreadOperationBody::Capture(capture) =
                    &mut op.body
                else {
                    panic!("capture")
                };
                capture.author = SourceAuthor::account(
                    plan.scope().spool.parse().expect("spool"),
                    CollaborationActor {
                        principal_id: "00000000-0000-0000-0000-000000000099"
                            .parse()
                            .expect("actor"),
                        agent_id: None,
                    },
                    b"synthetic-envelope-not-authority".to_vec(),
                )
                .expect("structural author envelope");
                signed[0] =
                    SignedOperation::sign(&op, &f.signer).expect("valid but substituted signature");
            }
            _ => unreachable!(),
        }
        assert!(
            plan.bind_signed(&f.native, signed, |scope| check_scope(&f, scope), authority)
                .is_err(),
            "{case}"
        );
        unchanged(&f);
    }
}

#[test]
fn unhashed_state_fidelity_change_is_rejected_after_external_signing() {
    let f = fixture();
    let plan = prepare(&f);
    let signed = sign(&plan, &f.signer);
    let mut state = plan.operations()[0]
        .source_state()
        .expect("decode")
        .expect("state");
    let id = state.id();
    state.git_lossy = true;
    assert_eq!(state.id(), id);
    f.native
        .store()
        .put_state(&state)
        .expect("mutate unhashed fidelity field");
    let error = plan
        .bind_signed(&f.native, signed, |scope| check_scope(&f, scope), authority)
        .err()
        .expect("full canonical bytes required");
    assert!(error.to_string().contains("canonical State"), "{error}");
    unchanged(&f);
}

#[test]
fn current_candidate_and_ancestor_visibility_are_rechecked_after_signing_delay() {
    for candidate in [false, true] {
        let f = fixture();
        let plan = prepare(&f);
        let signed = sign(&plan, &f.signer);
        let id = if candidate {
            plan.receipt().native_state
        } else {
            f.base
        };
        hide(&f, id);
        assert!(
            plan.bind_signed(&f.native, signed, |scope| check_scope(&f, scope), authority)
                .is_err()
        );
        unchanged(&f);
    }
}

#[test]
fn new_redaction_is_rechecked_after_signing_delay() {
    let f = fixture();
    let plan = prepare(&f);
    let signed = sign(&plan, &f.signer);
    f.native
        .put_redaction(Redaction {
            redacted_blob: Blob::new(b"new public content\n".to_vec()).hash(),
            state: plan.receipt().native_state,
            path: "public.txt".into(),
            reason: "synthetic revoke".into(),
            redactor: Principal::new("Synthetic owner", "owner@example.invalid"),
            redacted_at: chrono::Utc::now(),
            signature: None,
            purge: None,
            supersedes: None,
        })
        .expect("redaction");
    assert!(
        plan.bind_signed(&f.native, signed, |scope| check_scope(&f, scope), authority)
            .is_err()
    );
    unchanged(&f);
}

#[test]
fn current_actor_policy_and_original_authority_denials_are_not_cached() {
    for authority_revoked in [false, true] {
        let f = fixture();
        let plan = prepare(&f);
        let signed = sign(&plan, &f.signer);
        let result = plan.bind_signed(
            &f.native,
            signed,
            |scope| {
                check_scope(&f, scope)?;
                if !authority_revoked {
                    return Err(deny("actor or policy generation revoked"));
                }
                Ok(())
            },
            |replica, operation| {
                authority(replica, operation)?;
                if authority_revoked {
                    return Err(deny("original source authority revoked"));
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        unchanged(&f);
    }
}

#[test]
fn native_writer_can_advance_during_external_signing_and_stale_plan_refuses() {
    let f = fixture();
    let plan = prepare(&f);
    let signed = sign(&plan, &f.signer);
    let root = f.native.root().to_path_buf();
    let (tx, rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let repo = Repository::open(&root).expect("writer");
        std::fs::write(
            root.join("native-writer.txt"),
            b"concurrent native content\n",
        )
        .expect("file");
        let state = repo
            .snapshot_with_attribution(Some("native writer".into()), None, attribution())
            .expect("native commit")
            .state_id;
        tx.send(state).expect("result");
    });
    let advanced = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("preparation must release lock during external signing");
    writer.join().expect("writer done");
    assert_ne!(advanced, f.base);
    let error = plan
        .bind_signed(&f.native, signed, |scope| check_scope(&f, scope), authority)
        .err()
        .expect("stale expected head rejected");
    assert!(
        error.to_string().contains("scope or head changed"),
        "{error}"
    );
    assert_eq!(
        f.native
            .native_thread("main")
            .expect("thread")
            .projection()
            .expect("projection")
            .source_heads,
        vec![advanced]
    );
}

#[test]
fn another_native_repository_cannot_receive_prepared_sender_binding() {
    let f = fixture();
    let other = fixture();
    let plan = prepare(&f);
    let signed = sign(&plan, &f.signer);
    assert!(
        plan.bind_signed(
            &other.native,
            signed,
            |scope| check_scope(&f, scope),
            authority
        )
        .is_err()
    );
    unchanged(&f);
    unchanged(&other);
}

#[test]
fn explicit_actor_publisher_policy_and_account_spool_mismatches_fail_preparation() {
    let f = fixture();
    let update = PushUpdate {
        thread: "main".into(),
        old: f.old,
        new: f.new,
    };
    for (actor, policy, publisher) in [
        (
            "wrong-actor",
            "fixture-policy",
            f.signer.public_key().try_into().expect("key"),
        ),
        (
            "external-fixture",
            "wrong-policy",
            f.signer.public_key().try_into().expect("key"),
        ),
        ("external-fixture", "fixture-policy", [0; 32]),
    ] {
        assert!(
            prepare_git_push(
                &f.native,
                &f.git,
                LocalPush {
                    update: &update,
                    expected_native: f.base,
                    policy_generation: policy
                },
                PushAuthor {
                    actor,
                    publisher,
                    source_author: &SourceAuthor::LocalKey
                },
                WriteLimits::default(),
                |scope| check_scope(&f, scope)
            )
            .is_err()
        );
    }
    let author = SourceAuthor::account(
        "00000000-0000-0000-0000-000000000099"
            .parse()
            .expect("wrong spool"),
        CollaborationActor {
            principal_id: "00000000-0000-0000-0000-000000000088"
                .parse()
                .expect("actor"),
            agent_id: None,
        },
        b"synthetic-envelope-not-authority".to_vec(),
    )
    .expect("structural envelope");
    assert!(
        prepare_git_push(
            &f.native,
            &f.git,
            LocalPush {
                update: &update,
                expected_native: f.base,
                policy_generation: "fixture-policy"
            },
            PushAuthor {
                actor: "external-fixture",
                publisher: f.signer.public_key().try_into().expect("key"),
                source_author: &author
            },
            WriteLimits::default(),
            |_| Ok(())
        )
        .is_err()
    );
    unchanged(&f);
}

#[test]
fn fresh_repository_handle_detects_export_context_change_during_signing() {
    let f = fixture();
    let plan = prepare(&f);
    let signed = sign(&plan, &f.signer);
    let mut config = f.native.config().clone();
    // This synthetic URL is metadata only; no network operation is performed.
    config.hosted.upstream_url = Some("https://synthetic-new-context.invalid".into());
    config
        .save(&f.native.heddle_dir().join("config.toml"))
        .expect("changed native export context");
    let current = Repository::open(f.native.root()).expect("fresh current repository handle");
    assert_eq!(
        current.config().hosted.upstream_url.as_deref(),
        Some("https://synthetic-new-context.invalid")
    );
    let error = plan
        .bind_signed(&current, signed, |scope| check_scope(&f, scope), authority)
        .err()
        .expect("changed native footer cannot silently alter Git ancestry");
    assert!(error.to_string().contains("identities changed"), "{error}");
    unchanged(&f);
}
