//! Portable collaboration commands. Original parent records reconstruct both
//! causal frontiers; callers never translate UI version tokens into proofs.
use std::collections::BTreeSet;

use crypto::{Signer, thread_operation::SignedOperation};
use heddle_object_model::object::{
    Attribution, CollaborationIdempotencyKey, CollaborationMetadata, CollaborationOperationBodyV1,
    CollaborationOperationEnvelope, ContentHash, ContextRevision, DiscussionRecordId,
    thread_replication::{OPERATION_FORMAT, ThreadOperation, ThreadOperationBody},
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
        let envelope = CollaborationOperationEnvelope::new(
            self.discussion,
            inner.into_iter().collect(),
            self.operation_id,
            self.author,
            self.occurred_at_ms,
            self.body,
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
    let mut ids = BTreeSet::new();
    for record in parents {
        let parent = verify(record)?;
        let ThreadOperationBody::Context(bytes) = &parent.body else {
            return Err(Error::Protocol("context parent is not a context revision"));
        };
        let previous =
            ContextRevision::decode(bytes).map_err(|error| Error::Io(error.to_string()))?;
        if parent.thread != thread
            || previous.id != context.id
            || previous.metadata.scope != context.metadata.scope
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
        let context = sign_context(context, &[], &signer).expect("context");
        let mut vectors = String::new();
        for (name, record) in [
            ("open", open),
            ("append", append),
            ("resolve", resolve),
            ("reopen", reopen),
            ("context", context),
        ] {
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
