//! Explicit job lifecycle. Preparation carries a proposal; only a caller-signed
//! commit/renewal can authorize execution. Never sign or renew automatically.
use api::{heddle::api::v1alpha2 as wire, hybrid_codec::Reject, import_authority as authority};
use prost::Message;

use super::super::{HostedClient, Result};

/// Exact authenticated proposal retained until the caller signs. This value is
/// not delegated permission and cannot authorize native installation.
#[derive(Clone, Debug)]
pub struct PreparedImportJob {
    destination: wire::SpoolRef,
    response: wire::PrepareImportJobResponse,
}
impl PreparedImportJob {
    pub fn response(&self) -> &wire::PrepareImportJobResponse {
        &self.response
    }
    pub fn destination(&self) -> &wire::SpoolRef {
        &self.destination
    }
}

impl HostedClient {
    pub async fn prepare_import_job(
        &self,
        request: &wire::PrepareImportJobRequest,
    ) -> Result<PreparedImportJob> {
        let destination = request.destination.as_ref().ok_or(Reject::Canonical)?;
        let identity = request.identity.as_ref().ok_or(Reject::Canonical)?;
        let spool = uuid::Uuid::parse_str(&destination.id).map_err(|_| Reject::Canonical)?;
        if identity.spool_uuid != spool.as_bytes()
            || request.retry_lineage_id.len() != 16
            || (!request.renew_logical_job_id.is_empty()
                && request.renew_logical_job_id.len() != 16)
        {
            return Err(Reject::Scope.into());
        }
        authority::validate_scope(request.proposed_scope.as_ref().ok_or(Reject::Canonical)?)?;
        let response: wire::PrepareImportJobResponse = self
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/PrepareImportJob",
                request,
            )
            .await?;
        validate_preparation(request, &response, chrono::Utc::now().timestamp())?;
        Ok(PreparedImportJob {
            destination: destination.clone(),
            response,
        })
    }

    /// The proof carries original caller signatures. Weft independently checks
    /// owner permission/current policy under its activation transaction fence.
    pub async fn commit_import_job(
        &self,
        prepared: &PreparedImportJob,
        proof: &wire::ImportPublicProofBundleV1,
        client_operation_id: String,
    ) -> Result<wire::MutationResponse> {
        let signed = exact_signed_proposal(prepared, proof)?;
        verify_delegating_signature(signed)?;
        if prepared.response.renewal_state.is_some() {
            return Err(Reject::StaleContext.into());
        }
        self.call_unary(
            "/heddle.api.v1alpha2.IntegrationService/CommitImportJob",
            &wire::CommitImportJobRequest {
                client_operation_id,
                destination: Some(prepared.destination.clone()),
                proof: Some(proof.clone()),
            },
        )
        .await
    }

    pub async fn renew_import_job(
        &self,
        prepared: &PreparedImportJob,
        renewal: &wire::SignedImportJobRenewalV1,
        proof: &wire::ImportPublicProofBundleV1,
        client_operation_id: String,
    ) -> Result<wire::MutationResponse> {
        authority::validate_renewal_preparation(&prepared.response)?;
        let signed = exact_signed_proposal(prepared, proof)?;
        verify_delegating_signature(signed)?;
        let state = prepared
            .response
            .renewal_state
            .as_ref()
            .ok_or(Reject::Canonical)?;
        let body = renewal.body.as_ref().ok_or(Reject::Canonical)?;
        let proposal = signed.body.as_ref().ok_or(Reject::Canonical)?;
        if body.replacement.as_ref() != Some(signed)
            || body.expected_authority_epoch != state.authority_epoch
            || body.predecessor_delegation_digest != proposal.predecessor_delegation_digest
            || body.committed_manifest_digest
                != authority::manifest_digest(
                    state.committed_manifest.as_ref().ok_or(Reject::Canonical)?,
                )?
        {
            return Err(Reject::StaleContext.into());
        }
        authority::verify_authorization_signature(
            &proposal.delegating_public_key,
            authority::RENEWAL_DOMAIN,
            body,
            renewal
                .delegating_signature
                .as_ref()
                .ok_or(Reject::Signature)?,
        )?;
        self.call_unary(
            "/heddle.api.v1alpha2.IntegrationService/RenewImportJob",
            &wire::RenewImportJobRequest {
                client_operation_id,
                destination: Some(prepared.destination.clone()),
                renewal: Some(renewal.clone()),
                proof: Some(proof.clone()),
            },
        )
        .await
    }

    pub async fn cancel_import_job(
        &self,
        request: &wire::CancelImportJobRequest,
    ) -> Result<wire::MutationResponse> {
        if request.logical_job_id.len() != 16
            || request.cancellation_id.len() != 32
            || request.expected_authority_epoch == 0
            || request.destination.is_none()
        {
            return Err(Reject::Canonical.into());
        }
        self.call_unary(
            "/heddle.api.v1alpha2.IntegrationService/CancelImportJob",
            request,
        )
        .await
    }
}

fn validate_preparation(
    request: &wire::PrepareImportJobRequest,
    response: &wire::PrepareImportJobResponse,
    now: i64,
) -> Result<()> {
    let proposal = response.proposal.as_ref().ok_or(Reject::Canonical)?;
    if proposal.identity != request.identity
        || proposal.scope != request.proposed_scope
        || proposal.retry_lineage_id != request.retry_lineage_id
        || proposal.format_version != 1
        || proposal.purpose != 1
        || proposal.logical_job_id.len() != 16
        || proposal.delegation_id.len() != 16
        || proposal.job_public_key.len() != 32
        || proposal.job_key_id != api::hybrid_codec::key_id(&proposal.job_public_key)
        || proposal.cancellation_id.len() != 32
    {
        return Err(Reject::Scope.into());
    }
    if response.reservation_expires_at_unix_seconds <= now
        || response.reservation_expires_at_unix_seconds
            > now.checked_add(3600).ok_or(Reject::Bounds)?
    {
        return Err(Reject::Expired.into());
    }
    if request.renew_logical_job_id.is_empty() {
        if response.renewal_state.is_some() || proposal.predecessor_delegation_digest != [0; 32] {
            return Err(Reject::StaleContext.into());
        }
    } else {
        if proposal.logical_job_id != request.renew_logical_job_id {
            return Err(Reject::Scope.into());
        }
        authority::validate_renewal_preparation(response)?;
    }
    Ok(())
}

fn exact_signed_proposal<'a>(
    prepared: &PreparedImportJob,
    proof: &'a wire::ImportPublicProofBundleV1,
) -> Result<&'a wire::SignedImportJobDelegationV1> {
    if proof.format_version != 1 || proof.encoded_len() > authority::MAX_BUNDLE_BYTES {
        return Err(Reject::Bounds.into());
    }
    let signed = proof.delegations.last().ok_or(Reject::ImportPermission)?;
    if signed.body != prepared.response.proposal {
        return Err(Reject::Scope.into());
    }
    if prepared.response.reservation_expires_at_unix_seconds <= chrono::Utc::now().timestamp() {
        return Err(Reject::Expired.into());
    }
    Ok(signed)
}

fn verify_delegating_signature(signed: &wire::SignedImportJobDelegationV1) -> Result<()> {
    let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
    authority::verify_authorization_signature(
        &body.delegating_public_key,
        authority::DELEGATION_DOMAIN,
        body,
        signed
            .delegating_signature
            .as_ref()
            .ok_or(Reject::Signature)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T: Message + Default>(section: &str, name: &str) -> T {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../thread-api/tests/fixtures/hybrid-alpha18.json"
        )))
        .expect("alpha.18 fixed vectors");
        let bytes = hex::decode(
            fixture[section][name]["wire_hex"]
                .as_str()
                .expect("wire bytes"),
        )
        .expect("hex bytes");
        T::decode(bytes.as_slice()).expect("fixed protobuf")
    }

    #[test]
    fn renewal_preparation_binds_the_exact_destination_scope_and_cas() {
        let response: wire::PrepareImportJobResponse = wire("wire_vectors", "renewal_preparation");
        let proposal = response.proposal.as_ref().expect("proposal");
        let request = wire::PrepareImportJobRequest {
            identity: proposal.identity.clone(),
            proposed_scope: proposal.scope.clone(),
            retry_lineage_id: proposal.retry_lineage_id.clone(),
            renew_logical_job_id: proposal.logical_job_id.clone(),
            ..Default::default()
        };
        let now = response.reservation_expires_at_unix_seconds - 3600;
        validate_preparation(&request, &response, now).expect("exact prepared renewal");
        let mut changed = response.clone();
        changed
            .proposal
            .as_mut()
            .expect("proposal")
            .retry_lineage_id[0] ^= 1;
        assert!(validate_preparation(&request, &changed, now).is_err());
        let mut changed = response.clone();
        changed
            .renewal_state
            .as_mut()
            .expect("current CAS")
            .authority_epoch = 0;
        assert!(validate_preparation(&request, &changed, now).is_err());
        assert!(
            validate_preparation(
                &request,
                &response,
                response.reservation_expires_at_unix_seconds
            )
            .is_err()
        );
        validate_preparation(&request, &response, now).expect("unchanged proposal remains usable");
    }

    #[test]
    fn a_prepared_job_cannot_replace_the_original_delegating_signature() {
        let signed: wire::SignedImportJobDelegationV1 = wire("signed_vectors", "delegation");
        verify_delegating_signature(&signed).expect("original owner/device signature");
        let mut unsigned = signed.clone();
        unsigned.delegating_signature = None;
        assert!(verify_delegating_signature(&unsigned).is_err());
        let mut changed = signed.clone();
        changed.body.as_mut().expect("body").job_public_key[0] ^= 1;
        assert!(verify_delegating_signature(&changed).is_err());
        verify_delegating_signature(&signed).expect("original proposal/signature pair");
    }
}
