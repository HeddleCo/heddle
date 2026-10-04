//! Explicit job lifecycle. Preparation carries a proposal; only a caller-signed
//! commit/renewal can authorize execution. Never sign or renew automatically.
use api::{heddle::api::v1alpha2 as wire, hybrid_codec::Reject, import_authority as authority};
use prost::Message;

use super::super::{HostedClient, Result};

/// Authenticated destination-writer discovery. Options are exact advertised
/// octets; the host rechecks support and budgets when preparing the job.
#[derive(Clone, Debug)]
pub struct ImportConfiguration {
    destination: wire::SpoolRef,
    response: wire::GetImportConfigurationResponse,
}
impl ImportConfiguration {
    pub fn response(&self) -> &wire::GetImportConfigurationResponse {
        &self.response
    }

    /// Absent means the caller must choose; converter ordering is not a rank.
    pub fn default_converter(&self) -> Option<&wire::ImportConverterConfigurationV1> {
        let version = self.response.default_converter_version.as_ref()?;
        self.response
            .converters
            .iter()
            .find(|c| &c.converter_version == version)
    }
}

/// Caller-bound source discovery, obtained only from ResolveImportSource.
/// Repository format is independent of the width or availability of ref OIDs.
#[derive(Clone, Debug)]
pub struct ResolvedImportSource {
    request: wire::ResolveImportSourceRequest,
    source: wire::ProviderRepository,
    provider: &'static str,
}
impl ResolvedImportSource {
    pub fn source(&self) -> &wire::ProviderRepository {
        &self.source
    }
    pub fn provider(&self) -> &'static str {
        self.provider
    }
}

/// Authenticated destination-writer snapshot for reviewing the remaining scope.
/// Prepare must return this exact snapshot before the caller signs a renewal.
#[derive(Clone, Debug)]
pub struct ImportJobState {
    request: wire::GetImportJobStateRequest,
    response: wire::GetImportJobStateResponse,
}
impl ImportJobState {
    pub fn response(&self) -> &wire::GetImportJobStateResponse {
        &self.response
    }

    /// An expired predecessor is a CAS handle, never executable permission.
    pub fn predecessor(
        &self,
        expected: &authority::ImportOwnerExpectation<'_>,
    ) -> Result<authority::VerifiedImportRenewalPredecessor> {
        let state = self.response.state.as_ref().ok_or(Reject::StaleContext)?;
        let signed = state.active_predecessor.as_ref().ok_or(Reject::Canonical)?;
        let proof = self
            .response
            .retained_proof
            .as_ref()
            .ok_or(Reject::Canonical)?;
        Ok(authority::verify_renewal_predecessor(
            state,
            member_permission(proof, signed)?,
            expected,
        )?)
    }
}

/// Complete validated request bytes. Replays retain this body, including its
/// operation ID; the transport signs a fresh request PoP outside the body.
pub struct ImportRenewalSubmission {
    bytes: Vec<u8>,
}
impl ImportRenewalSubmission {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Exact authenticated proposal retained until the caller signs. This value is
/// not delegated permission and cannot authorize native installation.
#[derive(Clone, Debug)]
pub struct PreparedImportJob {
    destination: wire::SpoolRef,
    response: wire::PrepareImportJobResponse,
    renewal_read: Option<ImportJobState>,
    configuration: ImportConfiguration,
    source: ResolvedImportSource,
}
impl PreparedImportJob {
    pub fn response(&self) -> &wire::PrepareImportJobResponse {
        &self.response
    }
    pub fn destination(&self) -> &wire::SpoolRef {
        &self.destination
    }

    pub fn renewal_submission(
        &self,
        request: &wire::RenewImportJobRequest,
        predecessor_context: &authority::ImportOwnerExpectation<'_>,
        current_context: &authority::ImportOwnerExpectation<'_>,
    ) -> Result<ImportRenewalSubmission> {
        validate_renewal(self, request, predecessor_context, current_context)?;
        Ok(ImportRenewalSubmission {
            bytes: request.encode_to_vec(),
        })
    }

    /// The owner context must come from independent owner/keyring verification.
    /// Browser clock skew permits bounded future starts, never expiry grace.
    pub fn preflight(
        &self,
        proof: &wire::ImportPublicProofBundleV1,
        expected: &authority::ImportOwnerExpectation<'_>,
    ) -> Result<()> {
        let signed = exact_signed_proposal(self, proof, expected.now_unix_seconds)?;
        let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
        let geneses = body
            .branch_manifest
            .iter()
            .map(|branch| {
                proof
                    .genesis_authorities
                    .iter()
                    .find(|genesis| {
                        authority::signed_genesis_digest(genesis)
                            .is_ok_and(|digest| digest == branch.genesis_authority_digest)
                    })
                    .cloned()
                    .ok_or(Reject::GenesisBinding)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        authority::preflight_prepared_delegation(
            &self.response,
            signed,
            member_permission(proof, signed)?,
            &geneses,
            expected,
        )?;
        Ok(())
    }
}

impl HostedClient {
    pub async fn resolve_import_source(
        &self,
        request: &wire::ResolveImportSourceRequest,
    ) -> Result<ResolvedImportSource> {
        let selected = request.source.as_ref().ok_or(Reject::SourceSelection)?;
        // The authenticated resolver checks the connection's current GitHub
        // grant. Public GitHub URLs remain public-git without a connection.
        authority::resolve_import_provider(
            selected,
            selected.connection.as_ref().map(|_| "github"),
        )?;
        if request.encoded_len() > authority::MAX_BUNDLE_BYTES
            || request.page.as_ref().is_some_and(|page| page.size > 512)
        {
            return Err(Reject::Bounds.into());
        }
        self.require_import_authority_protocol().await?;
        let response: wire::ResolveImportSourceResponse = self
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/ResolveImportSource",
                request,
            )
            .await?;
        resolved_source(request, &response)
    }

    pub async fn get_import_job_state(
        &self,
        request: &wire::GetImportJobStateRequest,
    ) -> Result<ImportJobState> {
        authority::validate_job_state_request(request)?;
        self.require_import_authority_protocol().await?;
        let response = self
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/GetImportJobState",
                request,
            )
            .await?;
        authority::validate_job_state_response(request, &response)?;
        Ok(ImportJobState {
            request: request.clone(),
            response,
        })
    }
    /// Check semantic support before provisioning an import destination or
    /// staging an import job. Method discovery alone cannot establish support.
    pub async fn require_import_authority_protocol(&self) -> Result<()> {
        let remote = self
            .native()
            .await
            .map_err(super::super::HostedError::transport)?;
        authority::require_hybrid_peer(remote.description.protocol.as_ref())?;
        Ok(())
    }

    pub async fn get_import_configuration(
        &self,
        destination: &wire::SpoolRef,
    ) -> Result<ImportConfiguration> {
        self.require_import_authority_protocol().await?;
        uuid::Uuid::parse_str(&destination.id).map_err(|_| Reject::Canonical)?;
        let response: wire::GetImportConfigurationResponse = self
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/GetImportConfiguration",
                &wire::GetImportConfigurationRequest {
                    destination: Some(destination.clone()),
                },
            )
            .await?;
        authority::validate_import_configuration(&response)?;
        Ok(ImportConfiguration {
            destination: destination.clone(),
            response,
        })
    }

    pub async fn prepare_import_job(
        &self,
        configuration: &ImportConfiguration,
        source: &ResolvedImportSource,
        request: &wire::PrepareImportJobRequest,
    ) -> Result<PreparedImportJob> {
        self.prepare_import_job_from_read(configuration, source, request, None)
            .await
    }

    pub async fn prepare_import_renewal(
        &self,
        configuration: &ImportConfiguration,
        source: &ResolvedImportSource,
        request: &wire::PrepareImportJobRequest,
        read: &ImportJobState,
        predecessor_context: &authority::ImportOwnerExpectation<'_>,
    ) -> Result<PreparedImportJob> {
        let predecessor = read.predecessor(predecessor_context)?;
        self.prepare_import_job_from_read(
            configuration,
            source,
            request,
            Some((read, &predecessor)),
        )
        .await
    }

    async fn prepare_import_job_from_read(
        &self,
        configuration: &ImportConfiguration,
        source: &ResolvedImportSource,
        request: &wire::PrepareImportJobRequest,
        retained: Option<(
            &ImportJobState,
            &authority::VerifiedImportRenewalPredecessor,
        )>,
    ) -> Result<PreparedImportJob> {
        let read = retained.map(|(read, _)| read);
        match read {
            Some(read)
                if read.request.destination == request.destination
                    && read.request.logical_job_id == request.renew_logical_job_id => {}
            None if request.renew_logical_job_id.is_empty() => {}
            _ => return Err(Reject::StaleContext.into()),
        }
        let destination = request.destination.as_ref().ok_or(Reject::Canonical)?;
        let identity = request.identity.as_ref().ok_or(Reject::Canonical)?;
        let spool = uuid::Uuid::parse_str(&destination.id).map_err(|_| Reject::Canonical)?;
        if destination != &configuration.destination
            || identity.spool_uuid != spool.as_bytes()
            || request.retry_lineage_id.len() != 16
            || (!request.renew_logical_job_id.is_empty()
                && request.renew_logical_job_id.len() != 16)
        {
            return Err(Reject::Scope.into());
        }
        if request.encoded_len() > authority::MAX_BUNDLE_BYTES {
            return Err(Reject::Bounds.into());
        }
        authority::initial_operation_id(&request.retry_lineage_id, false)?;
        let proposed = request.proposed_scope.as_ref().ok_or(Reject::Canonical)?;
        validate_source_selection(
            request,
            source,
            retained.map(|(_, predecessor)| predecessor),
        )?;
        let retained_state = retained
            .map(|(read, predecessor)| {
                Ok::<_, Reject>((
                    predecessor,
                    read.response.state.as_ref().ok_or(Reject::StaleContext)?,
                ))
            })
            .transpose()?;
        // Empty explicitly asks Prepare for the current opaque CAS token.
        // Source format/custody are checked before RPC; the complete prepared
        // scope is checked against the issued token before it reaches signing.
        if !proposed.destination_version.is_empty() {
            authority::prepare_import_source_scope(
                request,
                &source.source,
                source.source.connection.as_ref().map(|_| source.provider),
                &configuration.response,
                &proposed.destination_version,
                retained_state,
            )?;
        } else if retained_state.is_none() {
            authority::validate_discovered_import_scope(proposed, &source.source)?;
        }
        let response: wire::PrepareImportJobResponse = self
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/PrepareImportJob",
                request,
            )
            .await?;
        validate_preparation(request, &response, chrono::Utc::now().timestamp())?;
        if let Some(read) = read {
            authority::validate_renewal_preparation_from_read(request, &response, &read.response)?;
        }
        let returned = response
            .proposal
            .as_ref()
            .and_then(|p| p.scope.as_ref())
            .ok_or(Reject::Canonical)?;
        authority::prepare_import_source_scope(
            request,
            &source.source,
            source.source.connection.as_ref().map(|_| source.provider),
            &configuration.response,
            &returned.destination_version,
            retained_state,
        )?;
        Ok(PreparedImportJob {
            destination: destination.clone(),
            response,
            renewal_read: read.cloned(),
            configuration: configuration.clone(),
            source: source.clone(),
        })
    }

    /// Commit is the sole initial submission. Source and optional empty base
    /// accompany the unchanged proof; no second branch carrier is generated.
    /// Provider comes from an independently selected source association.
    pub async fn commit_import_job(
        &self,
        prepared: &PreparedImportJob,
        request: &wire::CommitImportJobRequest,
        resolved_provider: &str,
        expected: &authority::ImportOwnerExpectation<'_>,
    ) -> Result<wire::MutationResponse> {
        let mut current = prepared.clone();
        current.configuration = self.get_import_configuration(&prepared.destination).await?;
        current.source = self.resolve_import_source(&prepared.source.request).await?;
        validate_commit(&current, request, resolved_provider, expected)?;
        let response: wire::MutationResponse = self
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/CommitImportJob",
                request,
            )
            .await?;
        authority::validate_commit_response(request, &response)?;
        let endpoint = self
            .native()
            .await
            .map_err(super::super::HostedError::transport)?
            .description
            .endpoint
            .clone();
        if response
            .receipt
            .as_ref()
            .is_none_or(|r| r.endpoint != endpoint)
        {
            return Err(Reject::PendingOperation.into());
        }
        Ok(response)
    }

    /// Recover an expired predecessor only as a non-executable authenticated
    /// CAS handle, then independently check current replacement authority.
    pub async fn renew_import_job(
        &self,
        submission: &ImportRenewalSubmission,
    ) -> Result<wire::MutationResponse> {
        self.call_unary_encoded(
            "/heddle.api.v1alpha2.IntegrationService/RenewImportJob",
            submission.bytes(),
        )
        .await
    }

    /// Select the active delegation's host-issued cancellation ID, not a
    /// parent permission or predecessor. The host resolves stored replay first.
    pub async fn cancel_import_job(
        &self,
        request: &wire::CancelImportJobRequest,
        active: &wire::SignedImportJobDelegationV1,
    ) -> Result<wire::MutationResponse> {
        authority::check_cancel_request(request, active, request.expected_authority_epoch, false)?;
        self.call_unary(
            "/heddle.api.v1alpha2.IntegrationService/CancelImportJob",
            request,
        )
        .await
    }
}

fn resolved_source(
    request: &wire::ResolveImportSourceRequest,
    response: &wire::ResolveImportSourceResponse,
) -> Result<ResolvedImportSource> {
    if response.encoded_len() > authority::MAX_BUNDLE_BYTES {
        return Err(Reject::Bounds.into());
    }
    let selected = request.source.as_ref().ok_or(Reject::SourceSelection)?;
    let source = response.source.as_ref().ok_or(Reject::SourceSelection)?;
    if selected.connection != source.connection
        || selected.clone_url != source.clone_url
        || selected.installation_id != source.installation_id
        || (!selected.provider_repository_id.is_empty()
            && selected.provider_repository_id != source.provider_repository_id)
    {
        return Err(Reject::SourceSelection.into());
    }
    let provider =
        authority::resolve_import_provider(source, source.connection.as_ref().map(|_| "github"))?;
    authority::validate_repository_hash_algorithm(source, false)?;
    Ok(ResolvedImportSource {
        request: request.clone(),
        source: source.clone(),
        provider,
    })
}

fn validate_source_selection(
    request: &wire::PrepareImportJobRequest,
    source: &ResolvedImportSource,
    retained: Option<&authority::VerifiedImportRenewalPredecessor>,
) -> Result<()> {
    let selector = request.source.as_ref().ok_or(Reject::SourceSelection)?;
    let scope = request.proposed_scope.as_ref().ok_or(Reject::Canonical)?;
    if selector.connection != source.source.connection
        || selector.installation_id != source.source.installation_id
        || selector.private != source.source.private
        || (!selector.provider_repository_id.is_empty()
            && selector.provider_repository_id != source.source.provider_repository_id)
        || scope.provider != source.provider
        || scope.source_url != source.source.clone_url
    {
        return Err(Reject::SourceSelection.into());
    }
    authority::validate_repository_hash_algorithm(&source.source, true)?;
    if !source.request.include_refs {
        return Err(Reject::SourceSelection.into());
    }
    // Known selected OIDs need only their page. Missing refs require complete
    // coverage; an authenticated retained pin selects its original commit.
    let complete = source
        .request
        .page
        .as_ref()
        .is_none_or(|p| p.after_page.is_empty())
        && source.source.refs_status.as_ref().is_some_and(|s| {
            s.section == "provider_refs"
                && s.coverage == wire::Coverage::Complete as i32
                && s.page
                    .as_ref()
                    .is_some_and(|p| p.exhausted && p.next_page.is_empty())
        });
    if !complete
        && !scope.branches.iter().all(|branch| {
            (retained.is_some() && branch.ref_mode == 1)
                || source
                    .source
                    .refs
                    .iter()
                    .any(|r| r.name == branch.ref_name && !r.head_oid.is_empty())
        })
    {
        return Err(Reject::SourceSelection.into());
    }
    Ok(())
}

fn validate_commit(
    prepared: &PreparedImportJob,
    request: &wire::CommitImportJobRequest,
    resolved_provider: &str,
    expected: &authority::ImportOwnerExpectation<'_>,
) -> Result<()> {
    if request.destination.as_ref() != Some(&prepared.destination)
        || prepared.response.renewal_state.is_some()
    {
        return Err(Reject::StaleContext.into());
    }
    if resolved_provider != prepared.source.provider {
        return Err(Reject::SourceSelection.into());
    }
    authority::validate_commit_request(
        request,
        resolved_provider,
        &prepared.source.source,
        &prepared.configuration.response,
    )?;
    prepared.preflight(request.proof.as_ref().ok_or(Reject::Canonical)?, expected)
}

fn validate_renewal(
    prepared: &PreparedImportJob,
    request: &wire::RenewImportJobRequest,
    predecessor_context: &authority::ImportOwnerExpectation<'_>,
    current_context: &authority::ImportOwnerExpectation<'_>,
) -> Result<()> {
    if request.destination.as_ref() != Some(&prepared.destination) {
        return Err(Reject::Scope.into());
    }
    let read = prepared.renewal_read.as_ref().ok_or(Reject::StaleContext)?;
    if prepared.response.renewal_state != read.response.state {
        return Err(Reject::StaleContext.into());
    }
    authority::validate_renewal_preparation(&prepared.response)?;
    let replacement = request
        .renewal
        .as_ref()
        .and_then(|r| r.body.as_ref())
        .and_then(|r| r.replacement.as_ref())
        .ok_or(Reject::Canonical)?;
    exact_signed_delegation(prepared, replacement, current_context.now_unix_seconds)?;
    authority::verify_renew_submission(
        request,
        &read.response,
        predecessor_context,
        current_context,
    )?;
    Ok(())
}

fn member_permission<'a>(
    proof: &'a wire::ImportPublicProofBundleV1,
    signed: &wire::SignedImportJobDelegationV1,
) -> Result<Option<&'a wire::SignedImportMemberPermissionV1>> {
    let digest = &signed
        .body
        .as_ref()
        .ok_or(Reject::Canonical)?
        .parent_permission_digest;
    if let Some(parent) = proof.member_permission.as_ref()
        && authority::signed_permission_digest(parent)? == *digest
    {
        return Ok(Some(parent));
    }
    Ok(authority::resolve_bundle_permission(proof, digest)?)
}

fn validate_preparation(
    request: &wire::PrepareImportJobRequest,
    response: &wire::PrepareImportJobResponse,
    now: i64,
) -> Result<()> {
    authority::validate_preparation_response(request, response)?;
    let proposal = response.proposal.as_ref().ok_or(Reject::Canonical)?;
    if proposal.format_version != 1
        || proposal.purpose != 1
        || proposal.logical_job_id.len() != 16
        || proposal.delegation_id.len() != 16
        || proposal.job_public_key.len() != 32
        || proposal.job_key_id != api::hybrid_codec::key_id(&proposal.job_public_key)
        || proposal.cancellation_id.len() != 32
    {
        return Err(Reject::Scope.into());
    }
    let at = i128::from(response.prepared_at_unix_seconds);
    let skew = i128::from(response.clock_skew_allowance_seconds);
    if at < 0
        || i128::from(now) + skew < at
        || i128::from(response.reservation_expires_at_unix_seconds) != at + 3600
        || response.reservation_expires_at_unix_seconds <= now
        || response.max_validity_duration_seconds == 0
    {
        return Err(Reject::Expired.into());
    }
    Ok(())
}

fn exact_signed_proposal<'a>(
    prepared: &PreparedImportJob,
    proof: &'a wire::ImportPublicProofBundleV1,
    now: i64,
) -> Result<&'a wire::SignedImportJobDelegationV1> {
    if proof.format_version != 1 || proof.encoded_len() > authority::MAX_BUNDLE_BYTES {
        return Err(Reject::Bounds.into());
    }
    let signed = proof.delegations.last().ok_or(Reject::ImportPermission)?;
    exact_signed_delegation(prepared, signed, now)?;
    Ok(signed)
}

fn exact_signed_delegation(
    prepared: &PreparedImportJob,
    signed: &wire::SignedImportJobDelegationV1,
    now: i64,
) -> Result<()> {
    let body = signed.body.as_ref().ok_or(Reject::Canonical)?;
    let proposal = prepared
        .response
        .proposal
        .as_ref()
        .ok_or(Reject::Canonical)?;
    if api::hybrid_codec::canonical(&authority::delegation_preparation(body))?
        != api::hybrid_codec::canonical(proposal)?
    {
        return Err(Reject::PreparedFields.into());
    }
    let scope = proposal.scope.as_ref().ok_or(Reject::Canonical)?;
    if body.branch_manifest.len() != scope.branches.len()
        || body
            .branch_manifest
            .iter()
            .zip(&scope.branches)
            .any(|(manifest, branch)| manifest.limit.as_ref() != Some(branch))
    {
        return Err(Reject::PreparedFields.into());
    }
    if prepared.response.reservation_expires_at_unix_seconds <= now {
        return Err(Reject::Expired.into());
    }
    // This preflight supplies no owner permission. The host repeats the exact
    // preparation comparison and actual-clock checks under its activation CAS.
    let start = i128::from(body.not_before_unix_seconds);
    let end = i128::from(body.expires_at_unix_seconds);
    let at = i128::from(prepared.response.prepared_at_unix_seconds);
    let skew = i128::from(prepared.response.clock_skew_allowance_seconds);
    if start < 0
        || start < at - skew
        || start > i128::from(now) + skew
        || end <= start
        || end <= i128::from(now)
        || prepared.response.max_validity_duration_seconds == 0
        || end - start > i128::from(prepared.response.max_validity_duration_seconds)
    {
        return Err(Reject::ValidityBounds.into());
    }
    Ok(())
}

pub(super) fn verify_delegating_signature(
    signed: &wire::SignedImportJobDelegationV1,
) -> Result<()> {
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
            "/../thread-api/tests/fixtures/hybrid-alpha25.json"
        )))
        .expect("alpha.25 fixed vectors");
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

    #[test]
    fn commit_compares_the_frozen_preparation_and_ordered_branch_limits() {
        let response: wire::PrepareImportJobResponse = wire("wire_vectors", "commit_preparation");
        let signed: wire::SignedImportJobDelegationV1 = wire("signed_vectors", "delegation");
        let prepared = PreparedImportJob {
            destination: wire::SpoolRef::default(),
            response,
            renewal_read: None,
            configuration: configuration(),
            source: source("source_connected"),
        };
        let mut proof = wire::ImportPublicProofBundleV1 {
            format_version: 1,
            delegations: vec![signed],
            ..Default::default()
        };
        exact_signed_proposal(&prepared, &proof, 1100).expect("original completed proposal");
        let original = proof.clone();
        proof.delegations[0]
            .body
            .as_mut()
            .expect("body")
            .job_public_key[0] ^= 1;
        assert!(matches!(
            exact_signed_proposal(&prepared, &proof, 1100),
            Err(super::super::super::HostedError::Hybrid(
                Reject::PreparedFields
            ))
        ));
        proof = original.clone();
        proof.delegations[0]
            .body
            .as_mut()
            .expect("body")
            .branch_manifest
            .reverse();
        assert!(matches!(
            exact_signed_proposal(&prepared, &proof, 1100),
            Err(super::super::super::HostedError::Hybrid(
                Reject::PreparedFields
            ))
        ));
        assert!(matches!(
            exact_signed_proposal(
                &prepared,
                &original,
                prepared.response.reservation_expires_at_unix_seconds
            ),
            Err(super::super::super::HostedError::Hybrid(Reject::Expired))
        ));
        exact_signed_proposal(&prepared, &original, 1100).expect("unchanged signed control");
    }

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../thread-api/tests/fixtures/hybrid-alpha25.json"
        )))
        .expect("published alpha.25 vectors")
    }

    fn with_context(now: i64, check: impl FnOnce(&authority::ImportOwnerExpectation<'_>)) {
        let f = fixture();
        let identity: wire::ImportIdentityV1 = wire("wire_vectors", "identity");
        let key = |name: &str| {
            hex::decode(f["keys"][name]["public_key_hex"].as_str().expect("key")).expect("hex")
        };
        let owner = key("owner");
        let chain = hex::decode(
            f["context"]["owner_chain_digest_hex"]
                .as_str()
                .expect("chain"),
        )
        .expect("hex");
        let forbidden = ["owner", "device", "root", "witness", "next_witness"].map(key);
        check(&authority::ImportOwnerExpectation {
            identity: &identity,
            owner_public_key: &owner,
            owner_chain_digest: &chain,
            authority_expires_at_seconds: 2000,
            now_unix_seconds: now,
            forbidden_job_keys: &forbidden,
            known_job_associations: &[],
        });
    }

    fn prepared(response: &str) -> PreparedImportJob {
        PreparedImportJob {
            destination: wire::<wire::CommitImportJobRequest>("wire_vectors", "commit_request")
                .destination
                .expect("destination"),
            response: wire("wire_vectors", response),
            renewal_read: (response == "renewal_preparation").then(|| ImportJobState {
                request: wire("wire_vectors", "job_state_request"),
                response: wire("wire_vectors", "job_state_partial"),
            }),
            configuration: configuration(),
            source: source("source_connected"),
        }
    }

    fn configuration() -> ImportConfiguration {
        ImportConfiguration {
            destination: wire::<wire::CommitImportJobRequest>("wire_vectors", "commit_request")
                .destination
                .expect("destination"),
            response: wire("wire_vectors", "import_configuration"),
        }
    }
    fn source(name: &str) -> ResolvedImportSource {
        let mut source: wire::ProviderRepository = wire("wire_vectors", name);
        source.refs_status = Some(wire::SectionStatus {
            section: "provider_refs".into(),
            coverage: wire::Coverage::Complete as i32,
            page: Some(wire::PageInfo {
                exhausted: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let provider = authority::resolve_import_provider(
            &source,
            source.connection.as_ref().map(|_| "github"),
        )
        .expect("resolved provider");
        ResolvedImportSource {
            request: wire::ResolveImportSourceRequest {
                source: Some(source.clone()),
                include_refs: true,
                ..Default::default()
            },
            source,
            provider,
        }
    }

    #[test]
    fn authenticated_resolution_preserves_custody_and_never_guesses_unknown_format() {
        let request = wire("wire_vectors", "resolve_public_request");
        let mut response: wire::ResolveImportSourceResponse =
            wire("wire_vectors", "resolve_public_response");
        // The published codec vector omits page status; the RPC supplies it.
        response.source.as_mut().expect("source").refs_status =
            source("source_public_sha256").source.refs_status;
        let resolved =
            resolved_source(&request, &response).expect("authenticated public discovery");
        assert_eq!(resolved.provider(), "public-git");
        let prepare: wire::PrepareImportJobRequest = wire("wire_vectors", "prepare_public_sha256");
        validate_source_selection(&prepare, &resolved, None).expect("independent SHA-256 format");
        let unknown = wire("wire_vectors", "resolve_unknown_response");
        let unknown = resolved_source(&request, &unknown).expect("unknown is valid discovery");
        assert!(matches!(
            validate_source_selection(&prepare, &unknown, None),
            Err(super::super::super::HostedError::Hybrid(Reject::Version))
        ));
        let mut changed = resolved.clone();
        changed.source.connection = source("source_connected").source.connection;
        assert!(validate_source_selection(&prepare, &changed, None).is_err());
        let mut partial = resolved.clone();
        partial
            .source
            .refs_status
            .as_mut()
            .expect("page")
            .page
            .as_mut()
            .expect("page")
            .exhausted = false;
        assert!(validate_source_selection(&prepare, &partial, None).is_err());
        validate_source_selection(&prepare, &resolved, None).expect("unchanged complete discovery");
    }

    #[test]
    fn selected_refs_can_prepare_from_one_page_without_inventing_missing_oids() {
        let mut request: wire::PrepareImportJobRequest =
            wire("wire_vectors", "prepare_public_sha256");
        let mut discovered = source("source_sha256_known");
        request
            .proposed_scope
            .as_mut()
            .expect("scope")
            .branches
            .retain(|branch| {
                discovered
                    .source
                    .refs
                    .iter()
                    .any(|r| r.name == branch.ref_name && !r.head_oid.is_empty())
            });
        assert!(
            !request
                .proposed_scope
                .as_ref()
                .expect("scope")
                .branches
                .is_empty()
        );
        for branch in &mut request.proposed_scope.as_mut().expect("scope").branches {
            let known = discovered
                .source
                .refs
                .iter()
                .find(|r| r.name == branch.ref_name)
                .expect("selected known ref");
            branch.ref_mode = 1;
            branch.ref_disclosure = 0;
            branch.pinned_commit_oid = hex::decode(&known.head_oid).expect("known OID");
            assert!(!branch.pinned_commit_oid.is_empty());
        }
        let status = discovered.source.refs_status.as_mut().expect("page status");
        status.page.as_mut().expect("page").exhausted = false;
        authority::validate_discovered_import_scope(
            request.proposed_scope.as_ref().expect("scope"),
            &discovered.source,
        )
        .expect("every discovered selected OID remains exactly pinned");
        validate_source_selection(&request, &discovered, None)
            .expect("selected refs and their OIDs are already on this page");

        let mut unknown = request.clone();
        let branch = unknown
            .proposed_scope
            .as_mut()
            .expect("scope")
            .branches
            .first_mut()
            .expect("branch");
        discovered.source.refs.retain(|r| r.name != branch.ref_name);
        branch.ref_mode = 2;
        branch.pinned_commit_oid.clear();
        branch.ref_disclosure = 1;
        assert!(
            validate_source_selection(&unknown, &discovered, None).is_err(),
            "an omitted ref is not evidence that its OID is unavailable"
        );
        discovered
            .source
            .refs_status
            .as_mut()
            .expect("status")
            .page
            .as_mut()
            .expect("page")
            .exhausted = true;
        validate_source_selection(&unknown, &discovered, None)
            .expect("complete coverage can establish an unavailable OID");
        authority::validate_discovered_import_scope(
            unknown.proposed_scope.as_ref().expect("scope"),
            &discovered.source,
        )
        .expect("observe mode only when the complete discovery has no OID");
        discovered.request.page = Some(wire::PageRequest {
            size: 512,
            after_page: b"later-page".to_vec(),
        });
        assert!(
            validate_source_selection(&unknown, &discovered, None).is_err(),
            "the final page does not establish absence from earlier pages"
        );
    }

    #[test]
    fn a_verified_retained_pin_does_not_require_an_unrelated_ref_page() {
        let read = prepared("renewal_preparation")
            .renewal_read
            .expect("authenticated read");
        with_context(1350, |expected| {
            let predecessor = read
                .predecessor(expected)
                .expect("authenticated predecessor");
            let request: wire::PrepareImportJobRequest =
                wire("wire_vectors", "renew_prepare_request");
            let mut discovered = source("renew_source_moved_head");
            discovered.source.refs.clear();
            discovered
                .source
                .refs_status
                .as_mut()
                .expect("status")
                .page
                .as_mut()
                .expect("page")
                .exhausted = false;
            assert!(validate_source_selection(&request, &discovered, None).is_err());
            validate_source_selection(&request, &discovered, Some(&predecessor))
                .expect("retained commit selection is independent of this ref page");
            authority::prepare_import_source_scope(&request, &discovered.source, Some("github"), &configuration().response, &wire::<wire::ImportPermissionScopeV1>("wire_vectors", "scope").destination_version, Some((&predecessor, read.response.state.as_ref().expect("state")))).expect("the exact signed predecessor and retained CAS still authorize only the original pin");
        });
    }

    #[test]
    fn converter_default_requires_an_explicit_advertised_marker() {
        let mut config = configuration();
        assert!(config.default_converter().is_some());
        config.response = wire("wire_vectors", "configuration_no_default");
        authority::validate_import_configuration(&config.response).expect("explicit chooser");
        assert!(config.default_converter().is_none());
        for row in fixture()["source_vectors"]["configuration_negative"]
            .as_array()
            .expect("vectors")
        {
            let bad = wire(
                "wire_vectors",
                row["configuration"].as_str().expect("configuration"),
            );
            assert!(
                authority::validate_import_configuration(&bad).is_err(),
                "{row}"
            );
        }
    }

    #[test]
    fn commit_rechecks_current_discovery_and_support_before_activation() {
        with_context(1100, |expected| {
            for row in fixture()["source_vectors"]["commit_negative"]
                .as_array()
                .expect("vectors")
            {
                let mut current = prepared("commit_preparation");
                current.source = source(row["source"].as_str().expect("source"));
                current.configuration.response = wire(
                    "wire_vectors",
                    row["configuration"].as_str().expect("configuration"),
                );
                let request = wire("wire_vectors", row["request"].as_str().expect("request"));
                assert!(
                    validate_commit(&current, &request, "github", expected).is_err(),
                    "{row}"
                );
            }
            let current = prepared("commit_preparation");
            let control = wire("wire_vectors", "commit_request");
            validate_commit(&current, &control, "github", expected)
                .expect("unchanged current source and support");
        });
    }

    #[test]
    fn renewal_prepare_retains_authorized_pins_across_head_movement_and_exact_cas() {
        let read = prepared("renewal_preparation")
            .renewal_read
            .expect("authenticated read");
        with_context(1350, |expected| {
            let predecessor = read
                .predecessor(expected)
                .expect("expired predecessor CAS handle");
            let state = read.response.state.as_ref().expect("state");
            for row in fixture()["source_vectors"]["renewal_prepare"]
                .as_array()
                .expect("vectors")
            {
                let request = wire("wire_vectors", row["request"].as_str().expect("request"));
                let current_source = source(row["source"].as_str().expect("source"));
                let configuration = row["configuration"]
                    .as_str()
                    .map(|name| wire("wire_vectors", name))
                    .unwrap_or_else(|| configuration().response);
                let changed_state = row["state"].as_str().map(|name| wire("wire_vectors", name));
                let retained = row["retained"]
                    .as_bool()
                    .expect("retained")
                    .then_some((&predecessor, changed_state.as_ref().unwrap_or(state)));
                let result = authority::prepare_import_source_scope(
                    &request,
                    &current_source.source,
                    Some("github"),
                    &configuration,
                    &wire::<wire::ImportPermissionScopeV1>("wire_vectors", "scope")
                        .destination_version,
                    retained,
                );
                assert_eq!(result.is_ok(), row["expected"] == "OK", "{row}: {result:?}");
            }
        });
    }

    #[test]
    fn prepare_preserves_every_choice_and_accepts_only_the_issued_destination_token() {
        let request: wire::PrepareImportJobRequest = wire("wire_vectors", "prepare_request");
        let response: wire::PrepareImportJobResponse = wire("wire_vectors", "commit_preparation");
        validate_preparation(&request, &response, 1100).expect("unchanged choices");
        for row in fixture()["submission_vectors"]["changed_preparations"]
            .as_array()
            .expect("rows")
        {
            let changed = wire("wire_vectors", row["response"].as_str().expect("response"));
            assert!(
                validate_preparation(&request, &changed, 1100).is_err(),
                "{row}"
            );
        }
        let mut empty = request.clone();
        empty
            .proposed_scope
            .as_mut()
            .expect("scope")
            .destination_version
            .clear();
        let issued: wire::PrepareImportJobRequest = wire("wire_vectors", "prepare_issue_token");
        assert_eq!(empty.proposed_scope, issued.proposed_scope);
        validate_preparation(&issued, &response, 1100).expect("host fills only opaque token");
        validate_preparation(&request, &response, 1100).expect("unchanged control");
    }

    #[test]
    fn configuration_support_is_bounded_and_typed_refusals_cannot_smuggle_a_proposal() {
        let config: wire::GetImportConfigurationResponse =
            wire("wire_vectors", "import_configuration");
        authority::validate_import_configuration(&config).expect("authenticated finite choices");
        let scope: wire::ImportPermissionScopeV1 = wire("wire_vectors", "scope");
        authority::prepare_scope(&scope, &config, &scope.destination_version)
            .expect("supported choice");
        let request: wire::PrepareImportJobRequest = wire("wire_vectors", "prepare_request");
        for name in [
            "converter",
            "options",
            "budget",
            "destination",
            "invalid_scope",
        ] {
            let response: wire::PrepareImportJobResponse =
                wire("wire_vectors", &format!("prepare_refusal_{name}"));
            assert!(matches!(
                validate_preparation(&request, &response, 1100),
                Err(super::super::super::HostedError::Hybrid(
                    Reject::PreparationRefused(_)
                ))
            ));
            let mut smuggled = response;
            smuggled.proposal = prepared("commit_preparation").response.proposal;
            assert!(matches!(
                validate_preparation(&request, &smuggled, 1100),
                Err(super::super::super::HostedError::Hybrid(Reject::Canonical))
            ));
        }
        let mut changed = config.clone();
        changed.converters[0].canonical_options.clear();
        assert!(authority::validate_import_configuration(&changed).is_err());
        authority::validate_import_configuration(&config).expect("unchanged configuration");
    }

    #[test]
    fn commit_requires_exact_source_originals_envelopes_and_initial_preparation() {
        let control: wire::CommitImportJobRequest = wire("wire_vectors", "commit_request");
        let prepared = prepared("commit_preparation");
        with_context(1100, |expected| {
            validate_commit(&prepared, &control, "github", expected)
                .expect("signed initial Commit");
            for row in fixture()["submission_vectors"]["commit_negatives"]
                .as_array()
                .expect("rows")
            {
                let request = wire("wire_vectors", row["request"].as_str().expect("request"));
                assert!(
                    validate_commit(&prepared, &request, "github", expected).is_err(),
                    "{row}"
                );
            }
            let mut unsigned = control.clone();
            unsigned.proof.as_mut().expect("proof").delegations[0].delegating_signature = None;
            assert!(validate_commit(&prepared, &unsigned, "github", expected).is_err());
            assert!(validate_commit(&prepared, &control, "another-provider", expected).is_err());
            validate_commit(&prepared, &control, "github", expected)
                .expect("unchanged signed control");
        });
    }

    #[test]
    fn browser_preflight_allows_only_advertised_skew_and_never_expiry_grace() {
        let proof = wire::<wire::CommitImportJobRequest>("wire_vectors", "commit_request")
            .proof
            .expect("proof");
        let prepared = prepared("commit_preparation");
        for row in fixture()["amendment_vectors"]["preflight"]
            .as_array()
            .expect("rows")
        {
            with_context(row["now"].as_i64().expect("clock"), |expected| {
                let actual = prepared.preflight(&proof, expected);
                assert_eq!(
                    actual.is_ok(),
                    row["preflight"] == "OK",
                    "{row}: {actual:?}"
                );
            });
        }
        with_context(1100, |expected| {
            prepared
                .preflight(&proof, expected)
                .expect("current control")
        });
    }

    #[test]
    fn pending_commit_receipt_uses_the_reserved_lineage_and_exact_replay() {
        let request: wire::CommitImportJobRequest = wire("wire_vectors", "commit_request");
        let response: wire::MutationResponse = wire("wire_vectors", "commit_response");
        authority::validate_commit_response(&request, &response)
            .expect("reserved first physical operation");
        let other: wire::MutationResponse = wire("wire_vectors", "commit_wrong_lineage_response");
        assert_eq!(
            authority::validate_commit_response(&request, &other),
            Err(Reject::PendingOperation)
        );
        authority::check_commit_replay(&request, &request, &response).expect("exact replay");
        let mut changed = request.clone();
        changed
            .source
            .as_mut()
            .expect("source")
            .clone_url
            .push_str("/other");
        assert_eq!(
            authority::check_commit_replay(&changed, &request, &response),
            Err(Reject::OperationIdReused)
        );
        authority::validate_commit_response(&request, &response).expect("unchanged receipt");
    }

    #[test]
    fn cancel_selects_the_active_host_issued_id_and_resolves_exact_replay() {
        let active = wire("signed_vectors", "delegation");
        let request: wire::CancelImportJobRequest = wire("wire_vectors", "cancel_active");
        authority::check_cancel_request(&request, &active, 1, false).expect("active selector");
        for row in fixture()["amendment_vectors"]["cancel_negatives"]
            .as_array()
            .expect("rows")
        {
            let request = wire("wire_vectors", row["request"].as_str().expect("request"));
            let active = wire("signed_vectors", row["active"].as_str().expect("active"));
            assert!(
                authority::check_cancel_request(
                    &request,
                    &active,
                    row["epoch"].as_u64().expect("epoch"),
                    row["cancelled"].as_bool().expect("terminal")
                )
                .is_err(),
                "{row}"
            );
        }
        authority::check_cancel_replay(&request, &request)
            .expect("stored replay before epoch/terminal checks");
        let changed = wire("wire_vectors", "cancel_changed_replay");
        assert_eq!(
            authority::check_cancel_replay(&changed, &request),
            Err(Reject::OperationIdReused)
        );
        authority::check_cancel_request(&request, &active, 1, false).expect("unchanged selector");
    }

    #[test]
    fn renewal_recovers_expired_predecessor_without_turning_it_into_execution_authority() {
        let prepared = prepared("renewal_preparation");
        let request: wire::RenewImportJobRequest = wire("wire_vectors", "renew_request_partial");
        with_context(1350, |expected| {
            validate_renewal(&prepared, &request, expected, expected)
                .expect("expired predecessor with current narrower replacement");
            let mut stale = request.clone();
            stale
                .renewal
                .as_mut()
                .expect("renewal")
                .body
                .as_mut()
                .expect("body")
                .expected_authority_epoch += 1;
            assert!(validate_renewal(&prepared, &stale, expected, expected).is_err());
            let mut changed = request.clone();
            changed
                .renewal
                .as_mut()
                .expect("renewal")
                .delegating_signature = None;
            assert!(validate_renewal(&prepared, &changed, expected, expected).is_err());
            validate_renewal(&prepared, &request, expected, expected)
                .expect("unchanged renewal/CAS");
        });
    }

    #[test]
    fn renewal_read_fences_prepare_and_frozen_submission_preserves_accepted_history() {
        let read: wire::GetImportJobStateResponse = wire("wire_vectors", "job_state_partial");
        let request: wire::PrepareImportJobRequest = wire("wire_vectors", "renew_prepare_request");
        let response: wire::PrepareImportJobResponse = wire("wire_vectors", "renewal_preparation");
        authority::validate_renewal_preparation_from_read(&request, &response, &read)
            .expect("exact authenticated CAS");
        let before_publication = wire("wire_vectors", "job_state_empty");
        assert_eq!(
            authority::validate_renewal_preparation_from_read(
                &request,
                &response,
                &before_publication
            ),
            Err(Reject::StaleContext)
        );
        let prepared = prepared("renewal_preparation");
        let request: wire::RenewImportJobRequest = wire("wire_vectors", "renew_request_partial");
        with_context(1350, |expected| {
            let frozen = prepared
                .renewal_submission(&request, expected, expected)
                .expect("submission with candidate outside accepted history");
            assert_eq!(frozen.bytes(), request.encode_to_vec());
            authority::check_renew_replay(frozen.bytes(), frozen.bytes()).expect("exact replay");
            let mut candidate_in_history = request.clone();
            candidate_in_history
                .proof
                .as_mut()
                .expect("proof")
                .delegations
                .push(
                    request
                        .renewal
                        .as_ref()
                        .expect("renewal")
                        .body
                        .as_ref()
                        .expect("body")
                        .replacement
                        .clone()
                        .expect("candidate"),
                );
            assert!(
                prepared
                    .renewal_submission(&candidate_in_history, expected, expected)
                    .is_err()
            );
            assert_eq!(
                authority::check_renew_replay(
                    &candidate_in_history.encode_to_vec(),
                    frozen.bytes()
                ),
                Err(Reject::OperationIdReused)
            );
            assert_eq!(
                frozen.bytes(),
                request.encode_to_vec(),
                "caller edits cannot change replay"
            );
        });
    }
}
