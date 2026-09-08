//! Portable collaboration commands. Original parent records reconstruct both
//! causal frontiers; callers never translate UI version tokens into proofs.
use std::collections::BTreeSet;

use crypto::{Signer, thread_operation::SignedOperation};
use heddle_object_model::object::{
    Attribution, CollaborationIdempotencyKey, CollaborationMetadata, CollaborationOperationBodyV1,
    CollaborationOperationEnvelope, ContentHash, DiscussionRecordId,
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
    if !matches!(operation.body, ThreadOperationBody::Discussion(_)) {
        return Err(Error::Protocol(
            "collaboration requires a discussion operation",
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
}
