//! Portable collaboration commands. Original parent records reconstruct both
//! causal frontiers; callers never translate UI version tokens into proofs.
mod references;
mod tags;
use std::collections::BTreeSet;

use crypto::{Signer, thread_operation::SignedOperation};
use heddle_object_model::object::{
    Attribution, CollaborationIdempotencyKey, CollaborationMetadata, CollaborationOperationBodyV1,
    CollaborationOperationEnvelope, ContentHash, ContextRevision, DiscussionRecordId,
    thread_replication::{OPERATION_FORMAT, ThreadOperation, ThreadOperationBody},
};
pub use references::{anchor, anchor_ref, audience, mention, mention_ref, visibility};
pub use tags::{
    annotation_query, annotation_source, annotation_source_ref, annotation_tag, annotation_tag_ref,
    annotation_tags, annotation_value, annotation_value_ref,
};

use crate::{
    contract::{RecordSignature, SignedRecord},
    transport::Error,
};

/// Verified original operation, before host authorization or causal admission.
/// A valid signature never establishes a principal's spool permissions.
pub fn verify(record: &SignedRecord) -> Result<ThreadOperation, Error> {
    if record.format != OPERATION_FORMAT || record.signatures.len() != 1 {
        return Err(Error::Protocol("unsupported collaboration signed record"));
    }
    let operation = SignedOperation {
        canonical: record.canonical_record.clone(),
        signature: record.signatures[0].signature.clone(),
    }
    .verify()
    .map_err(|_| Error::Protocol("invalid collaboration author signature"))?;
    if record.signatures[0].public_key != operation.publisher {
        return Err(Error::Protocol(
            "collaboration signature key differs from publisher",
        ));
    }
    if !matches!(
        operation.body,
        ThreadOperationBody::Discussion(_) | ThreadOperationBody::Context(_)
    ) {
        return Err(Error::Protocol(
            "collaboration requires a discussion or context operation",
        ));
    }
    Ok(operation)
}

/// Typed content plus the account identity which the receiving host will bind
/// to its independently verified delivery credential. Display names are not IDs.
pub struct Command {
    pub discussion: DiscussionRecordId,
    pub operation_id: CollaborationIdempotencyKey,
    pub metadata: CollaborationMetadata,
    pub author: Attribution,
    pub occurred_at_ms: i64,
    pub body: CollaborationOperationBodyV1,
}

impl Command {
    /// Sign after observing original frontier records. Concurrent operations can
    /// share parents; observing both lets a subsequent command join both heads.
    /// Root commands use an empty parent slice. Never pass mutable view versions.
    pub fn sign(
        self,
        parents: &[SignedRecord],
        signer: &impl Signer,
    ) -> Result<SignedRecord, Error> {
        if parents.len() > 128 {
            return Err(Error::Protocol(
                "collaboration has more than 128 causal parents",
            ));
        }
        let thread = self.metadata.scope.thread.ok_or(Error::Protocol(
            "Thread command requires native Thread scope",
        ))?;
        let mut outer = BTreeSet::new();
        let mut inner = BTreeSet::new();
        for record in parents {
            let operation = verify(record)?;
            if operation.thread != thread {
                return Err(Error::Protocol("parent belongs to another Thread"));
            }
            let ThreadOperationBody::Discussion(bytes) = &operation.body else {
                return Err(Error::Protocol("parent is not a collaboration operation"));
            };
            let decoded = CollaborationOperationEnvelope::decode(bytes)
                .map_err(|_| Error::Protocol("invalid collaboration parent"))?;
            if decoded.operation.discussion_id != self.discussion
                || decoded
                    .operation
                    .metadata
                    .as_ref()
                    .is_none_or(|m| m.scope != self.metadata.scope)
            {
                return Err(Error::Protocol(
                    "parent belongs to another discussion or spool",
                ));
            }
            let id = operation
                .id()
                .map_err(|error| Error::Io(error.to_string()))?;
            if !outer.insert(id) {
                return Err(Error::Protocol("duplicate collaboration causal parent"));
            }
            inner.insert(decoded.operation_id);
        }
        let mut body = self.body;
        if let CollaborationOperationBodyV1::Resolve {
            resolution:
                heddle_object_model::object::CollaborationResolution::IntoContext { context },
        } = &mut body
        {
            context.parents = outer.iter().copied().collect();
        }
        let envelope = CollaborationOperationEnvelope::new(
            self.discussion,
            inner.into_iter().collect(),
            self.operation_id,
            self.author,
            self.occurred_at_ms,
            body,
        )
        .and_then(|envelope| envelope.with_metadata(self.metadata))
        .map_err(|error| Error::Io(error.to_string()))?;
        let operation = ThreadOperation {
            version: 1,
            thread,
            parents: outer,
            publisher: signer
                .public_key()
                .try_into()
                .map_err(|_| Error::Protocol("collaboration requires an Ed25519 signer"))?,
            body: ThreadOperationBody::Discussion(
                envelope
                    .encode()
                    .map_err(|error| Error::Io(error.to_string()))?,
            ),
        };
        let signed = SignedOperation::sign(&operation, signer)
            .map_err(|error| Error::Io(error.to_string()))?;
        Ok(SignedRecord {
            format: OPERATION_FORMAT.into(),
            canonical_record: signed.canonical,
            signatures: vec![RecordSignature {
                public_key: operation.publisher.to_vec(),
                signature: signed.signature,
            }],
        })
    }
}

/// Append an immutable context revision. Original signed parent records bind
/// the same stable record and scope; concurrent revisions remain separate heads.
pub fn sign_context(
    mut context: ContextRevision,
    parents: &[SignedRecord],
    signer: &impl Signer,
) -> Result<SignedRecord, Error> {
    if parents.len() > 128 {
        return Err(Error::Protocol("context has more than 128 causal parents"));
    }
    let thread = context.metadata.scope.thread.ok_or(Error::Protocol(
        "Thread context requires native Thread scope",
    ))?;
    if parents.is_empty() && context.extracted_from.is_some() {
        return Err(Error::Protocol(
            "context extraction requires a signed discussion resolution",
        ));
    }
    let mut ids = BTreeSet::new();
    for record in parents {
        let parent = verify(record)?;
        let previous = parent
            .context_revision()
            .map_err(|error| Error::Io(error.to_string()))?
            .ok_or(Error::Protocol(
                "context parent is not a context revision or extraction",
            ))?;
        if parent.thread != thread
            || previous.id != context.id
            || previous.metadata.scope != context.metadata.scope
            || previous.extracted_from != context.extracted_from
        {
            return Err(Error::Protocol(
                "context parent belongs to another record or scope",
            ));
        }
        if !ids.insert(parent.id().map_err(|error| Error::Io(error.to_string()))?) {
            return Err(Error::Protocol("duplicate context causal parent"));
        }
    }
    context.parents = ids.iter().copied().collect();
    let operation = ThreadOperation {
        version: 1,
        thread,
        parents: ids,
        publisher: signer
            .public_key()
            .try_into()
            .map_err(|_| Error::Protocol("context requires an Ed25519 signer"))?,
        body: ThreadOperationBody::Context(
            context
                .encode()
                .map_err(|error| Error::Io(error.to_string()))?,
        ),
    };
    let signed =
        SignedOperation::sign(&operation, signer).map_err(|error| Error::Io(error.to_string()))?;
    Ok(SignedRecord {
        format: OPERATION_FORMAT.into(),
        canonical_record: signed.canonical,
        signatures: vec![RecordSignature {
            public_key: operation.publisher.to_vec(),
            signature: signed.signature,
        }],
    })
}

/// The public causal vocabulary is the outer replication operation identity.
pub fn operation_id(record: &SignedRecord) -> Result<ContentHash, Error> {
    verify(record)?
        .id()
        .map_err(|error| Error::Io(error.to_string()))
}

#[cfg(test)]
mod tests {
    use crypto::Ed25519Signer;
    use heddle_object_model::object::{
        CollaborationActor, CollaborationAnchor, CollaborationScope, DiscussionTurnV1, Principal,
        VisibilityTier,
    };
    use uuid::Uuid;

    use super::*;

    fn command(discussion: DiscussionRecordId, body: CollaborationOperationBodyV1) -> Command {
        Command {
            discussion,
            operation_id: CollaborationIdempotencyKey::new("operation-1").expect("operation ID"),
            metadata: CollaborationMetadata {
                scope: CollaborationScope {
                    spool: Uuid::from_u128(1),
                    thread: Some(ContentHash::from_bytes([2; 32])),
                },
                actor: CollaborationActor {
                    principal_id: Uuid::from_u128(3),
                    agent_id: Some("agent-4".into()),
                },
                mentions: vec![],
            },
            author: Attribution::human(Principal::new("Account", "")),
            occurred_at_ms: 100,
            body,
        }
    }
    fn append(body: &str) -> CollaborationOperationBodyV1 {
        CollaborationOperationBodyV1::AppendTurn {
            turn: DiscussionTurnV1::new(body).expect("turn"),
        }
    }
    #[test]
    fn signed_parents_preserve_concurrent_heads_and_reject_changed_proofs() {
        let signer = Ed25519Signer::from_seed(&[7; 32]).expect("signer");
        let discussion = DiscussionRecordId::generate();
        let root = command(
            discussion,
            CollaborationOperationBodyV1::Open {
                blocking: false,
                title: "Review".into(),
                anchor: CollaborationAnchor::Repository,
                visibility: VisibilityTier::Private {
                    scope_label: "owner".into(),
                },
                turn: DiscussionTurnV1::new("Review this Thread").expect("turn"),
                thread_ref: None,
            },
        )
        .sign(&[], &signer)
        .expect("root");
        let left = command(discussion, append("left"))
            .sign(std::slice::from_ref(&root), &signer)
            .expect("left");
        let right = command(discussion, append("right"))
            .sign(std::slice::from_ref(&root), &signer)
            .expect("right");
        assert_ne!(
            operation_id(&left).expect("left ID"),
            operation_id(&right).expect("right ID")
        );
        let joined = command(discussion, append("both observed"))
            .sign(&[left.clone(), right.clone()], &signer)
            .expect("join");
        assert_eq!(
            verify(&joined).expect("verified").parents,
            BTreeSet::from([
                operation_id(&left).expect("left ID"),
                operation_id(&right).expect("right ID")
            ])
        );
        let mut changed = root.clone();
        changed.signatures[0].signature[0] ^= 1;
        assert!(
            command(discussion, append("bad proof"))
                .sign(&[changed], &signer)
                .is_err()
        );
        assert!(
            command(DiscussionRecordId::generate(), append("wrong discussion"))
                .sign(std::slice::from_ref(&root), &signer)
                .is_err()
        );
        assert!(
            command(discussion, append("duplicate parent"))
                .sign(&[root.clone(), root], &signer)
                .is_err()
        );
    }
    #[test]
    fn context_revisions_retain_history_and_bind_parent_record_identity() {
        let signer = Ed25519Signer::from_seed(&[7; 32]).expect("signer");
        let context = ContextRevision {
            version: 2,
            id: Uuid::from_u128(9),
            parents: vec![],
            metadata: command(DiscussionRecordId::generate(), append("metadata")).metadata,
            anchor: CollaborationAnchor::Repository,
            content: "Original rationale".into(),
            tags: vec!["decision".into()],
            supersedes: None,
            extracted_from: None,
            occurred_at_ms: 100,
        };
        let first = sign_context(context.clone(), &[], &signer).expect("first context");
        let mut revised = context.clone();
        revised.content = "Revised rationale".into();
        let second =
            sign_context(revised.clone(), std::slice::from_ref(&first), &signer).expect("revision");
        assert_eq!(
            verify(&second).expect("second").parents,
            BTreeSet::from([operation_id(&first).expect("first ID")])
        );
        let ThreadOperationBody::Context(bytes) = verify(&first).expect("first").body else {
            panic!("context");
        };
        assert_eq!(
            ContextRevision::decode(&bytes)
                .expect("original context")
                .content,
            "Original rationale"
        );
        revised.id = Uuid::from_u128(10);
        assert!(
            sign_context(revised, std::slice::from_ref(&first), &signer).is_err(),
            "parent cannot silently change record identity"
        );
    }
    #[test]
    fn extracted_context_keeps_original_resolution_proof_and_actor_binding() {
        use heddle_object_model::object::{
            CollaborationResolution, StateId, thread_replication::ThreadGenesis,
        };
        let signer = Ed25519Signer::from_seed(&[7; 32]).expect("signer");
        let genesis = ThreadGenesis {
            version: 1,
            spool: Uuid::from_u128(1).to_string(),
            parent: None,
            base: StateId::from_bytes([3; 32]),
            name: "extraction".into(),
            intent: "retain proof".into(),
            creator: signer.public_key().try_into().expect("key"),
            owner: heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(
                signer.public_key().try_into().expect("key"),
            ),
            nonce: vec![1; 16],
        };
        let discussion = DiscussionRecordId::generate();
        let make = |body| {
            let mut command = command(discussion, body);
            command.metadata.scope.thread = Some(genesis.id().expect("Thread"));
            command
        };
        let open = make(CollaborationOperationBodyV1::Open {
            blocking: true,
            title: "Review".into(),
            anchor: CollaborationAnchor::Repository,
            visibility: VisibilityTier::Public,
            turn: DiscussionTurnV1::new("Evidence").expect("turn"),
            thread_ref: None,
        })
        .sign(&[], &signer)
        .expect("root");
        let mut context = ContextRevision {
            version: 2,
            id: Uuid::from_u128(9),
            parents: vec![],
            metadata: make(append("metadata")).metadata,
            anchor: CollaborationAnchor::Repository,
            content: "Durable rationale".into(),
            tags: vec!["decision".into()],
            supersedes: None,
            extracted_from: Some(discussion),
            occurred_at_ms: 100,
        };
        let extracted = make(CollaborationOperationBodyV1::Resolve {
            resolution: CollaborationResolution::IntoContext {
                context: context.clone(),
            },
        })
        .sign(std::slice::from_ref(&open), &signer)
        .expect("atomic extraction");
        let extracted_operation = verify(&extracted).expect("original resolution");
        extracted_operation
            .validate_parents(&genesis, &[verify(&open).expect("root")])
            .expect("discussion causal proof");
        assert_eq!(
            extracted_operation
                .context_revision()
                .expect("extracted context")
                .expect("context")
                .parents,
            vec![operation_id(&open).expect("root ID")]
        );
        context.content = "Refined rationale".into();
        let revision = sign_context(context.clone(), std::slice::from_ref(&extracted), &signer)
            .expect("revise directly from original extraction");
        verify(&revision)
            .expect("revision")
            .validate_parents(&genesis, &[extracted_operation])
            .expect("context retains discussion extraction as causal root");
        let mut detached = verify(&revision).expect("context operation");
        let mut detached_context = detached
            .context_revision()
            .expect("decode")
            .expect("context");
        detached_context.extracted_from = None;
        detached.body = ThreadOperationBody::Context(detached_context.encode().expect("context"));
        assert!(
            detached
                .validate_parents(
                    &genesis,
                    &[verify(&extracted).expect("original extraction")]
                )
                .is_err(),
            "context revision cannot strip its extraction provenance"
        );
        let mut invented_root = verify(&revision).expect("context operation");
        let mut invented_context = invented_root
            .context_revision()
            .expect("decode")
            .expect("context");
        invented_context.parents.clear();
        invented_root.parents.clear();
        invented_root.body =
            ThreadOperationBody::Context(invented_context.encode().expect("context"));
        assert!(
            invented_root.validate_parents(&genesis, &[]).is_err(),
            "extraction requires an original signed discussion resolution"
        );
        context.metadata.actor.principal_id = Uuid::from_u128(10);
        assert!(
            make(CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::IntoContext { context }
            })
            .sign(&[open], &signer)
            .is_err(),
            "extraction cannot assert a different author than the signed resolution"
        );
    }
    #[test]
    fn canonical_browser_interop_vectors() {
        use heddle_object_model::object::{CollaborationMention, CollaborationResolution, StateId};
        let signer = Ed25519Signer::from_seed(&[7; 32]).expect("signer");
        let discussion: DiscussionRecordId = "disc-01980000-0000-7000-8000-000000000123"
            .parse()
            .expect("stable discussion ID");
        let open = command(
            discussion,
            CollaborationOperationBodyV1::Open {
                blocking: true,
                title: "Review".into(),
                anchor: CollaborationAnchor::Repository,
                visibility: VisibilityTier::Public,
                turn: DiscussionTurnV1::new("First turn").expect("turn"),
                thread_ref: None,
            },
        )
        .sign(&[], &signer)
        .expect("open");
        let mut append_command = command(discussion, append("See the reviewed state"));
        append_command.metadata.mentions = vec![CollaborationMention::State {
            spool: Uuid::from_u128(1),
            state: StateId::from_bytes([5; 32]),
        }];
        let append = append_command
            .sign(std::slice::from_ref(&open), &signer)
            .expect("append");
        let resolve = command(
            discussion,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::Dismissed {
                    reason: "Verified".into(),
                },
            },
        )
        .sign(std::slice::from_ref(&append), &signer)
        .expect("resolve");
        let reopen = command(
            discussion,
            CollaborationOperationBodyV1::Reopen {
                reason: "New evidence".into(),
            },
        )
        .sign(std::slice::from_ref(&resolve), &signer)
        .expect("reopen");
        let context = ContextRevision {
            version: 2,
            id: Uuid::from_u128(9),
            parents: vec![],
            metadata: command(
                discussion,
                CollaborationOperationBodyV1::Reopen {
                    reason: "metadata".into(),
                },
            )
            .metadata,
            anchor: CollaborationAnchor::Repository,
            content: "Design rationale".into(),
            tags: vec!["decision".into()],
            supersedes: None,
            extracted_from: Some(discussion),
            occurred_at_ms: 100,
        };
        let extraction = command(
            discussion,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::IntoContext {
                    context: context.clone(),
                },
            },
        )
        .sign(std::slice::from_ref(&reopen), &signer)
        .expect("extraction");
        let mut revision = context.clone();
        revision.content = "Refined extracted rationale".into();
        let extracted_revision = sign_context(revision, std::slice::from_ref(&extraction), &signer)
            .expect("extracted context revision");
        let mut standalone_context = context;
        standalone_context.extracted_from = None;
        let context = sign_context(standalone_context, &[], &signer).expect("context");
        let source_record = |revision, target| {
            command(
                discussion,
                CollaborationOperationBodyV1::Open {
                    blocking: true,
                    title: "Review source".into(),
                    visibility: VisibilityTier::Public,
                    anchor: CollaborationAnchor::Source {
                        source: heddle_object_model::object::CollaborationSourceAnchor {
                            revision,
                            path: "src/main.rs".into(),
                            symbol_id: "run".into(),
                            start_line: Some(12),
                            end_line: Some(18),
                            target,
                        },
                    },
                    turn: DiscussionTurnV1::new("Check these lines").expect("turn"),
                    thread_ref: None,
                },
            )
            .sign(&[], &signer)
            .expect("source root")
        };
        let source_state = source_record(
            heddle_object_model::object::CollaborationRevision::State {
                state_id: StateId::from_bytes([5; 32]),
            },
            None,
        );
        let source_git = source_record(
            heddle_object_model::object::CollaborationRevision::GitCommit {
                oid: "a".repeat(40),
            },
            None,
        );
        let mut structured = verify(&context)
            .expect("context proof")
            .context_revision()
            .expect("decode")
            .expect("context");
        structured.tags = vec![
            "decision".into(),
            heddle_object_model::object::AnnotationTag::Symbol {
                name: "authorize".into(),
                target: Some(heddle_object_model::object::AnnotationSourceReference {
                    scope: structured.metadata.scope.clone(),
                    source: heddle_object_model::object::CollaborationSourceAnchor {
                        revision: heddle_object_model::object::CollaborationRevision::GitCommit {
                            oid: "a".repeat(40),
                        },
                        path: "src/auth.rs".into(),
                        symbol_id: "auth::authorize".into(),
                        start_line: Some(12),
                        end_line: Some(18),
                        target: None,
                    },
                }),
            },
            heddle_object_model::object::AnnotationTag::Property {
                key: "confidence".into(),
                value: heddle_object_model::object::AnnotationValue::Decimal(
                    heddle_object_model::object::AnnotationDecimal {
                        coefficient: 9,
                        scale: 1,
                    },
                ),
            },
            heddle_object_model::object::AnnotationTag::Property {
                key: "requires_review".into(),
                value: heddle_object_model::object::AnnotationValue::Boolean(false),
            },
            heddle_object_model::object::AnnotationTag::Property {
                key: "large".into(),
                value: heddle_object_model::object::AnnotationValue::Integer(i64::MAX),
            },
            heddle_object_model::object::AnnotationTag::Property {
                key: "severity".into(),
                value: heddle_object_model::object::AnnotationValue::Text("high".into()),
            },
        ];
        let structured_context =
            sign_context(structured, &[], &signer).expect("structured context");
        use heddle_object_model::object::{
            AnnotationSourceReference, AnnotationTag, CollaborationRevision, CollaborationScope,
            CollaborationSourceAnchor,
            source_target::{SourceTargetBinding, SourceTargetReference},
        };
        let mut tracked = Vec::new();
        for (name, tag_name, binding) in [
            (
                "target_viewed",
                "tag_viewed",
                SourceTargetBinding::ViewedThread,
            ),
            (
                "target_named",
                "tag_named",
                SourceTargetBinding::NamedThread {
                    scope: CollaborationScope {
                        spool: Uuid::from_u128(1),
                        thread: Some(ContentHash::from_bytes([8; 32])),
                    },
                },
            ),
            (
                "target_pinned",
                "tag_pinned",
                SourceTargetBinding::PinnedRevision {
                    scope: CollaborationScope {
                        spool: Uuid::from_u128(1),
                        thread: None,
                    },
                    revision: CollaborationRevision::State {
                        state_id: StateId::from_bytes([5; 32]),
                    },
                },
            ),
        ] {
            let target = SourceTargetReference {
                target: ContentHash::from_bytes([6; 32]),
                binding,
            };
            let record = source_record(
                CollaborationRevision::State {
                    state_id: StateId::from_bytes([5; 32]),
                },
                Some(target.clone()),
            );
            let mut revision = verify(&context)
                .expect("context proof")
                .context_revision()
                .expect("decode")
                .expect("context");
            revision.tags = vec![AnnotationTag::Source {
                target: AnnotationSourceReference {
                    scope: revision.metadata.scope.clone(),
                    source: CollaborationSourceAnchor {
                        revision: CollaborationRevision::State {
                            state_id: StateId::from_bytes([5; 32]),
                        },
                        path: "src/main.rs".into(),
                        symbol_id: String::new(),
                        start_line: None,
                        end_line: None,
                        target: Some(target),
                    },
                },
            }];
            tracked.push((name, record));
            tracked.push((
                tag_name,
                sign_context(revision, &[], &signer).expect("tracked context"),
            ));
        }
        let mut vectors = String::new();
        for (name, record) in [
            ("open", open),
            ("append", append),
            ("resolve", resolve),
            ("reopen", reopen),
            ("context", context),
            ("structured_context", structured_context),
            ("source_state", source_state),
            ("source_git", source_git),
            ("extract_context", extraction),
            ("extracted_revision", extracted_revision),
        ]
        .into_iter()
        .chain(tracked)
        {
            vectors.push_str(&format!(
                "{} {} {} {} {}\n",
                name,
                hex::encode(&record.signatures[0].public_key),
                hex::encode(&record.canonical_record),
                hex::encode(&record.signatures[0].signature),
                operation_id(&record).expect("operation ID").to_hex()
            ));
        }
        assert_eq!(
            vectors,
            include_str!("../tests/fixtures/collaboration_v2.txt")
        );
    }
}
