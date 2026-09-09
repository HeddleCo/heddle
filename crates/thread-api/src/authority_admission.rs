//! Hosted original-author receipts. Verification requires receiver-owned executor
//! trust; carried signatures and transport peers never establish that trust.
pub use crypto::thread_authority_admission::SignedAuthorityAdmission;
use crypto::{Signer, thread_operation::SignedOperation};
use heddle_object_model::object::thread_authority_admission::FORMAT;
pub use heddle_object_model::object::{
    thread_authority_admission::ThreadAuthorityAdmission,
    thread_replication::integration::TrustedHostedExecutor,
};

use crate::{contract as wire, transport::Error};

/// Issuers persist this exact statement with original bytes at first admission.
pub fn sign(
    value: &ThreadAuthorityAdmission,
    signer: &impl Signer,
) -> Result<wire::SignedRecord, Error> {
    encode(
        &SignedAuthorityAdmission::sign(value, signer)
            .map_err(|_| Error::Protocol("authority admission signing failed"))?,
    )
}
/// Verify original and executor signatures against independently admitted trust.
pub fn verify(
    receipt: &wire::SignedRecord,
    original: &SignedOperation,
    trust: &TrustedHostedExecutor,
) -> Result<ThreadAuthorityAdmission, Error> {
    decode(receipt)?.verify(original, trust).map_err(|_| {
        Error::Protocol("authority receipt differs from original operation or pinned executor")
    })
}
/// Signature-only decoding for sidecar routing; this does not admit anything.
pub fn verify_signature(receipt: &wire::SignedRecord) -> Result<ThreadAuthorityAdmission, Error> {
    decode(receipt)?
        .verify_signature()
        .map_err(|_| Error::Protocol("invalid authority admission signature"))
}
pub fn decode(receipt: &wire::SignedRecord) -> Result<SignedAuthorityAdmission, Error> {
    if receipt.format != FORMAT {
        return Err(Error::Protocol("invalid authority admission format"));
    }
    let [signature] = receipt.signatures.as_slice() else {
        return Err(Error::Protocol(
            "authority admission requires one executor signature",
        ));
    };
    let value = ThreadAuthorityAdmission::decode(&receipt.canonical_record)
        .map_err(|_| Error::Protocol("invalid canonical authority admission"))?;
    if signature.public_key != value.executor {
        return Err(Error::Protocol(
            "authority admission signature key differs from executor",
        ));
    }
    let signed = SignedAuthorityAdmission {
        canonical: receipt.canonical_record.clone(),
        signature: signature.signature.clone(),
    };
    signed
        .verify_signature()
        .map_err(|_| Error::Protocol("invalid authority admission signature"))?;
    Ok(signed)
}
pub fn encode(receipt: &SignedAuthorityAdmission) -> Result<wire::SignedRecord, Error> {
    let value = receipt
        .verify_signature()
        .map_err(|_| Error::Protocol("invalid authority admission signature"))?;
    Ok(wire::SignedRecord {
        format: FORMAT.into(),
        canonical_record: receipt.canonical.clone(),
        signatures: vec![wire::RecordSignature {
            public_key: value.executor.to_vec(),
            signature: receipt.signature.clone(),
        }],
    })
}

#[cfg(test)]
mod tests {
    use crypto::Ed25519Signer;
    use heddle_object_model::object::{
        CollaborationActor, ContentHash,
        thread_authority_admission::MAX_BYTES,
        thread_replication::{
            ThreadOperation, ThreadOperationBody,
            metadata::{AUTHORITY_FORMAT, Control, ThreadControl},
        },
    };
    use uuid::Uuid;

    use super::*;

    fn fixture() -> (
        ThreadAuthorityAdmission,
        SignedOperation,
        TrustedHostedExecutor,
        Ed25519Signer,
    ) {
        let author = Ed25519Signer::from_seed(&[41; 32]).expect("author");
        let executor = Ed25519Signer::from_seed(&[42; 32]).expect("executor");
        let envelope = b"original authority independently checked at first admission".to_vec();
        let control = ThreadControl {
            version: 1,
            spool: Uuid::from_u128(100),
            actor: CollaborationActor {
                principal_id: Uuid::from_u128(101),
                agent_id: Some("original-agent".into()),
            },
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
            authority_envelope: envelope,
            client_operation_id: Uuid::from_u128(102),
            occurred_at_ms: 1,
            control: Control::Name("original agent work".into()),
        };
        let operation = ThreadOperation {
            version: 1,
            thread: ContentHash::from_bytes([43; 32]),
            parents: Default::default(),
            publisher: author.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Metadata(control.encode().expect("control")),
        };
        let trust = TrustedHostedExecutor {
            spool: control.spool,
            spool_genesis: ContentHash::from_bytes([44; 32]),
            executor: executor.public_key().try_into().expect("key"),
        };
        let receipt = ThreadAuthorityAdmission {
            version: 1,
            spool: trust.spool,
            spool_genesis: trust.spool_genesis,
            thread: operation.thread,
            operation: operation.id().expect("ID"),
            actor: control.actor,
            publisher: operation.publisher,
            authority_digest: control.authority_digest,
            executor: trust.executor,
            admitted_at_ms: 2000,
        };
        (
            receipt,
            SignedOperation::sign(&operation, &author).expect("original signature"),
            trust,
            executor,
        )
    }
    #[test]
    fn admission_requires_independent_executor_and_exact_original_identity() {
        let (value, original, trust, executor) = fixture();
        let signed = sign(&value, &executor).expect("receipt");
        assert_eq!(
            verify(&signed, &original, &trust).expect("independent original admission"),
            value
        );
        for wrong in [
            TrustedHostedExecutor {
                executor: [45; 32],
                ..trust.clone()
            },
            TrustedHostedExecutor {
                spool: Uuid::from_u128(999),
                ..trust.clone()
            },
            TrustedHostedExecutor {
                spool_genesis: ContentHash::from_bytes([46; 32]),
                ..trust.clone()
            },
        ] {
            assert!(
                verify(&signed, &original, &wrong).is_err(),
                "incoming proof never enrolls its own trust"
            );
        }
        let mut mutations = Vec::new();
        let mut changed = value.clone();
        changed.operation = ContentHash::from_bytes([47; 32]);
        mutations.push(changed);
        let mut changed = value.clone();
        changed.thread = ContentHash::from_bytes([48; 32]);
        mutations.push(changed);
        let mut changed = value.clone();
        changed.publisher = [49; 32];
        mutations.push(changed);
        let mut changed = value.clone();
        changed.actor.agent_id = None;
        mutations.push(changed);
        let mut changed = value.clone();
        changed.actor.principal_id = Uuid::from_u128(888);
        mutations.push(changed);
        let mut changed = value;
        changed.authority_digest = ContentHash::from_bytes([50; 32]);
        mutations.push(changed);
        for changed in mutations {
            assert!(
                verify(
                    &sign(&changed, &executor).expect("valid executor signature"),
                    &original,
                    &trust
                )
                .is_err(),
                "receipt cannot substitute original scope or authorship"
            );
        }
    }
    #[test]
    fn admission_verifies_both_signatures_and_canonical_bounds() {
        let (value, original, trust, executor) = fixture();
        let signed = sign(&value, &executor).expect("receipt");
        let mut changed = signed.clone();
        let mut new_value = value;
        new_value.admitted_at_ms += 1;
        changed.canonical_record = new_value.encode().expect("different timestamp");
        assert!(
            verify(&changed, &original, &trust).is_err(),
            "executor signature binds first admission time"
        );
        let mut changed = original.clone();
        changed.signature[0] ^= 1;
        assert!(
            verify(&signed, &changed, &trust).is_err(),
            "receipt never replaces original signature"
        );
        let mut changed = signed.clone();
        changed.signatures.push(changed.signatures[0].clone());
        assert!(
            verify(&changed, &original, &trust).is_err(),
            "exact one executor signature"
        );
        let mut changed = signed;
        changed.canonical_record.resize(MAX_BYTES + 1, 0);
        assert!(
            verify(&changed, &original, &trust).is_err(),
            "bounded before decoding"
        );
        assert!(
            ThreadAuthorityAdmission::decode(&changed.canonical_record)
                .expect_err("oversized canonical bytes rejected before parsing")
                .to_string()
                .contains("byte bound")
        );
    }
}
