//! Proof-only recovery for retained originals. Lookup is public metadata and
//! cannot restore content access or authorize an installation.
use std::future::Future;

use api::{
    heddle::api::common::{HostedWitnessHistoryProofV1, SignedHostedWitnessStatementV1},
    hybrid_codec::{Reject, canonical},
    witness_trust::{self, VerifiedWitnessSet},
};
use prost::Message;

use crate::contract::{GetHostedWitnessHistoryProofRequest, GetHostedWitnessHistoryProofResponse};

#[derive(Debug, thiserror::Error)]
pub enum Error<E: std::error::Error> {
    #[error("HYBRID history proof rejected: {0}")]
    Rejected(#[from] Reject),
    #[error("history proof lookup failed: {0}")]
    Lookup(E),
    #[error("history proof is unavailable")]
    NotFound,
}

/// Implementations return uniform misses without originals or resource metadata.
/// Requests and responses are bounded before archive I/O and protobuf decoding.
pub trait HistoryProofLookup: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    fn lookup(
        &self,
        request: &GetHostedWitnessHistoryProofRequest,
    ) -> impl Future<Output = Result<Option<GetHostedWitnessHistoryProofResponse>, Self::Error>> + Send;
}

pub fn request(
    signed: &SignedHostedWitnessStatementV1,
) -> Result<GetHostedWitnessHistoryProofRequest, Reject> {
    let statement = signed.body.as_ref().ok_or(Reject::Canonical)?;
    let request = GetHostedWitnessHistoryProofRequest {
        executor_id: statement.executor_id.clone(),
        statement_leaf_digest: witness_trust::leaf_digest(
            statement.purpose,
            &canonical(statement)?,
            &signed.signature,
        )?,
    };
    witness_trust::validate_lookup(&request)?;
    Ok(request)
}

/// Refresh public receiver metadata without replacing any original authority,
/// operation or accepted witness. This does not authorize installation: the
/// receiver still resolves the complete closure under its mutation lock.
pub fn replace_receiver_metadata(
    original: &mut crate::contract::ImportPublicProofBundleV1,
    refreshed: crate::contract::ImportPublicProofBundleV1,
) -> Result<(), Reject> {
    api::import_authority::validate_public_bundle(&refreshed)?;
    let mut unchanged = refreshed.clone();
    unchanged.witness_set = original.witness_set.clone();
    unchanged.history_proofs = original.history_proofs.clone();
    if &unchanged != original {
        return Err(Reject::Scope);
    }
    *original = refreshed;
    Ok(())
}

/// Recover and verify the exact original's path against independently selected
/// fresh trust. A neighboring statement's valid proof never satisfies this one.
pub async fn retrieve<L: HistoryProofLookup>(
    lookup: &L,
    set: &VerifiedWitnessSet,
    signed: &SignedHostedWitnessStatementV1,
    now_ms: i64,
) -> Result<HostedWitnessHistoryProofV1, Error<L::Error>> {
    let request = request(signed)?;
    let response = lookup
        .lookup(&request)
        .await
        .map_err(Error::Lookup)?
        .ok_or(Error::NotFound)?;
    if response.encoded_len() > witness_trust::MAX_PROOF_BYTES {
        return Err(Reject::Bounds.into());
    }
    let proof = response.proof.ok_or(Reject::Proof)?;
    if proof.siblings.len() > witness_trust::MAX_SIBLINGS {
        return Err(Reject::Bounds.into());
    }
    witness_trust::resolve_statement(set, signed, Some(&proof), false, now_ms)?;
    Ok(proof)
}

/// Fill missing paths after retirement without fetching content or rewriting
/// any original. Existing paths must match exact originals; substitution and
/// unreferenced proof entries reject instead of being hidden by retrieval.
pub async fn complete_bundle<L: HistoryProofLookup>(
    lookup: &L,
    set: &VerifiedWitnessSet,
    bundle: &mut crate::contract::ImportPublicProofBundleV1,
    now_ms: i64,
) -> Result<(), Error<L::Error>> {
    api::import_authority::validate_public_bundle(bundle)?;
    let mut proofs = bundle.history_proofs.clone();
    let mut used = std::collections::BTreeSet::new();
    for signed in &bundle.statements {
        let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
        match witness_trust::resolve_statement(set, signed, None, false, now_ms) {
            Ok(_) => continue,
            Err(Reject::Proof) => {}
            Err(error) => return Err(error.into()),
        }
        let mut matched = None;
        for (index, proof) in proofs.iter().enumerate().filter(|(_, proof)| {
            proof.executor_id == body.executor_id && proof.purpose == body.purpose
        }) {
            if witness_trust::resolve_statement(set, signed, Some(proof), false, now_ms).is_ok()
                && matched.replace(index).is_some()
            {
                return Err(Reject::Proof.into());
            }
        }
        let index = if let Some(index) = matched {
            index
        } else {
            let proof = retrieve(lookup, set, signed, now_ms).await?;
            proofs.push(proof);
            if proofs.len() > 1024 {
                return Err(Reject::Bounds.into());
            }
            proofs.len() - 1
        };
        used.insert(index);
    }
    if used.len() != proofs.len() {
        return Err(Reject::Proof.into());
    }
    let mut completed = bundle.clone();
    completed.history_proofs = proofs;
    api::import_authority::validate_public_bundle(&completed)?;
    *bundle = completed;
    Ok(())
}
