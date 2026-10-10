#![cfg(feature = "gateway-publication")]
//! The actual Git conversion feeds ordinary native source packs, with references.
//! Like other repository tests, run with an isolated writable HEDDLE_HOME.
#[path = "gateway_publication/support.rs"]
mod support;
use crypto::{Signer, thread_operation::SignedOperation};
use heddle_git_projection::{
    gateway_publication::{HistoryBudget, PreparedHistory, PublicationScope},
    gateway_view::{HistoryTip, ViewLimits, export_public_git_history},
    gateway_write::{LocalPush, PushAuthor, PushUpdate, WriteLimits, prepare_git_push},
};
use objects::object::{
    Attribution, CollaborationActor, CollaborationAnchor, CollaborationMetadata,
    CollaborationRevision, CollaborationSourceAnchor, ContentHash, ContextRevision, Principal,
    StateId,
    source_target::{
        SourceAffinity, SourceFileCore, SourceLineRange, SourceSelector, SourceTargetBinding,
        SourceTargetCore, SourceTargetReference,
    },
    thread_replication::{Admission, SourceAuthor, ThreadOperation, ThreadOperationBody},
};
use thread_api::contract::*;

fn track_annotation(
    native: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    state: StateId,
    signer: &crypto::Ed25519Signer,
) {
    let scope = objects::object::CollaborationScope {
        spool: replica
            .genesis()
            .expect("genesis")
            .spool
            .parse()
            .expect("Spool UUID"),
        thread: Some(replica.thread_id()),
    };
    let file = SourceFileCore {
        scope: scope.clone(),
        revision: CollaborationRevision::State { state_id: state },
        path: "base.txt".into(),
    };
    let target = SourceTargetCore {
        file: file.id().expect("source file identity"),
        revision: file.revision.clone(),
        selector: SourceSelector::Lines {
            range: SourceLineRange {
                start: 0,
                end: 1,
                start_affinity: SourceAffinity::After,
                end_affinity: SourceAffinity::Before,
            },
        },
    };
    let context = ContextRevision {
        version: 2,
        id: uuid::Uuid::from_u128(700),
        parents: vec![],
        metadata: CollaborationMetadata {
            scope,
            actor: CollaborationActor {
                principal_id: uuid::Uuid::from_u128(701),
                agent_id: None,
            },
            mentions: vec![],
        },
        anchor: CollaborationAnchor::Source {
            source: CollaborationSourceAnchor {
                revision: file.revision,
                path: file.path,
                symbol_id: String::new(),
                start_line: Some(1),
                end_line: Some(1),
                target: Some(SourceTargetReference {
                    target: target.id().expect("tracked source target"),
                    binding: SourceTargetBinding::ViewedThread,
                }),
            },
        },
        content: "Fixture tracked reference".into(),
        tags: vec![],
        supersedes: None,
        extracted_from: None,
        occurred_at_ms: 100,
        provenance: None,
        canonical_body: Default::default(),
    };
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: Default::default(),
        publisher: signer.public_key().try_into().expect("fixture publisher"),
        body: ThreadOperationBody::Context(context.encode().expect("canonical context")),
    };
    let signed = SignedOperation::sign(&operation, signer).expect("signed fixture context");
    assert_eq!(
        replica
            .receive(&signed, native.store(), |_| Ok(()))
            .expect("fixture context admission"),
        Admission::Accepted
    );
}

#[tokio::test]
async fn actual_git_preparation_hydrates_causal_reference_closure_without_accepting_head() {
    let root = tempfile::tempdir().expect("fixture");
    let native = repo::Repository::init_default(root.path().join("native")).expect("native");
    std::fs::write(native.root().join("base.txt"), b"older content\n").expect("base file");
    let base = native
        .snapshot_with_attribution(
            Some("base".into()),
            None,
            Attribution::human(Principal::new("Fixture", "fixture@example.invalid")),
        )
        .expect("capture")
        .state_id;
    let replica = native.native_thread("main").expect("Thread");
    // Only this isolated fixture loads its own pre-existing local signer. Neither
    // the preparation nor publication adapter performs identity lookup.
    let signer = native
        .native_thread_signer(&replica)
        .expect("fixture signer");
    track_annotation(&native, &replica, base, &signer);
    let git = sley::Repository::init_bare(root.path().join("quarantine.git")).expect("Git");
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
    .expect("history");
    let old = mapping.get_git(&base).expect("old OID");
    let blob = git
        .write_raw_object(sley::GitObjectType::Blob, b"new content\n".to_vec())
        .expect("blob");
    let mut tree = b"100644 next.txt\0".to_vec();
    tree.extend(blob.as_bytes());
    let tree = git
        .write_raw_object(sley::GitObjectType::Tree, tree)
        .expect("tree");
    let body = format!(
        "tree {tree}\nparent {old}\nauthor Git User <git@example.invalid> 1700000000 +0000\ncommitter Git User <git@example.invalid> 1700000000 +0000\n\nnext\n"
    );
    let new = git
        .write_raw_object(sley::GitObjectType::Commit, body.into_bytes())
        .expect("commit");
    let update = PushUpdate {
        thread: "main".into(),
        old,
        new,
    };
    let prepared = prepare_git_push(
        &native,
        &git,
        LocalPush {
            update: &update,
            expected_native: base,
            policy_generation: "fixture-current-policy",
        },
        PushAuthor {
            actor: "fixture-local-writer",
            publisher: signer.public_key().try_into().expect("key"),
            source_author: &SourceAuthor::LocalKey,
        },
        WriteLimits::default(),
        |_| Ok(()),
    )
    .expect("unsigned Git preparation");
    let originals = prepared
        .operations()
        .iter()
        .map(|operation| SignedOperation::sign(operation, &signer).expect("external signing"))
        .collect();
    let signed = prepared
        .bind_signed(
            &native,
            originals,
            |_| Ok(()),
            |replica, operation| {
                replica
                    .verify_local_source_owner(operation)
                    .map_err(|e| heddle_git_projection::GitProjectionError::Git(e.to_string()))
            },
        )
        .expect("bind signed bytes");
    let genesis = replica.genesis_record().expect("exact original wrapper");
    assert!(
        signed.source_originals().iter().any(|o| o
            .verify()
            .expect("signature")
            .reference_proof(&replica.genesis().expect("genesis"))
            .expect("reference proof")
            .is_some()),
        "exercise real causal reference descriptors"
    );
    let scope = PublicationScope {
        thread: ThreadRef {
            spool: Some(SpoolRef {
                id: signed.scope().spool.clone(),
            }),
            id: Some(ThreadId {
                value: signed.scope().thread_id.as_bytes().to_vec(),
            }),
        },
        source: EndpointRef {
            kind: EndpointKind::Device as i32,
            public_key: vec![2; 32],
        },
        spool_genesis: ContentHash::compute(b"fixture selected Spool genesis"),
        sharing_policy: ContentHash::compute(b"fixture sharing frontier"),
        command: signed.receipt().command_id,
    };
    let spool_genesis = scope.spool_genesis;
    let plan = PreparedHistory::for_git_push(
        &support::remote(),
        &native,
        &signed,
        genesis,
        scope,
        root.path(),
        HistoryBudget::default(),
        |_, _| Ok(()),
    )
    .expect("exact publication adapter");
    for revision in plan.revisions() {
        revision
            .validate_received(support::received(revision.source()).await, spool_genesis)
            .expect("actual source and causal reference closure validator");
    }
    assert_eq!(
        replica
            .projection()
            .expect("unaccepted sender head")
            .source_heads,
        [base]
    );
    assert!(signed.require_hosted_git_acceptance().is_err());
    assert_eq!(signed.git_oid(&plan.tip()), Some(new));
}
