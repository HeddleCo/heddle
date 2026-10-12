//! Explicit job lifecycle. Preparation carries a proposal; only a caller-signed
//! Commit can authorize execution. Never sign automatically.
use api::{heddle::api::v1alpha2 as wire, hybrid_codec::Reject, import_authority as authority};
use prost::Message;

use super::super::{HostedClient, HostedError, Result};
use super::discovery::ImportDiscovery;

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

    /// Plan independently signed sibling jobs in this destination. Discovery
    /// estimates Git storage. A 32x decoded-size forecast allows headroom over
    /// the observed 27x expansion, capped by the advertised host maximum.
    /// Branch counts cannot predict shared history: each sibling gets the full
    /// forecast as its exact signed budget. Unknown sizes use the host maximum.
    pub fn sibling_scopes(
        &self,
        source: &ResolvedImportSource,
        selected: &wire::ImportPermissionScopeV1,
    ) -> Result<Vec<wire::ImportPermissionScopeV1>> {
        authority::validate_import_configuration(&self.response)?;
        authority::validate_repository_size_estimate(&source.source)?;
        let limits = self.response.limits.as_ref().ok_or(Reject::Canonical)?;
        let count = selected.branches.len();
        if count == 0
            || selected
                .branches
                .windows(2)
                .any(|b| b[0].ref_name >= b[1].ref_name)
        {
            return Err(Reject::Scope.into());
        }
        let per_job = limits.max_branches.min(limits.max_operations) as usize;
        let estimate = match source.source.size_estimate_state {
            0 => None,
            1 => Some(u128::from(source.source.git_size_kib) * 1024 * 32),
            _ => return Err(Reject::Canonical.into()),
        };
        selected
            .branches
            .chunks(per_job)
            .map(|branches| {
                let mut scope = selected.clone();
                scope.branches = branches.to_vec();
                scope.max_operations = branches.len() as u32;
                scope.max_result_bytes = estimate.map_or(limits.max_result_bytes, |bytes| {
                    bytes.max(1).min(u128::from(limits.max_result_bytes)) as u64
                });
                authority::validate_discovered_import_scope(&scope, &source.source)?;
                Ok(scope)
            })
            .collect()
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

/// Authenticated writer-only job snapshot. Public Fetch/export evidence carries
/// its own proof bundle; a state read grants no native admission.
#[derive(Clone, Debug)]
pub struct ImportJobState {
    request: wire::GetImportJobStateRequest,
    response: wire::GetImportJobStateResponse,
}
impl ImportJobState {
    pub fn response(&self) -> &wire::GetImportJobStateResponse {
        &self.response
    }
    pub(super) fn retry_request(
        &self,
        client_operation_id: String,
    ) -> Result<wire::RetryImportSourceRequest> {
        use self::wire::get_import_job_state_response::RetryAvailability;
        authority::validate_job_state_response(&self.request, &self.response)?;
        let Some(RetryAvailability::EligibleRetryTarget(target)) =
            &self.response.retry_availability
        else {
            return Err(Reject::Transition.into());
        };
        Ok(wire::RetryImportSourceRequest {
            client_operation_id,
            original_operation: target.operation_ref.as_ref().map(|r| wire::RecordRef {
                spool: r.spool.clone(),
                id: r.id.clone(),
            }),
            expected_operation_version: target.operation_version.clone(),
            logical_job_id: self.request.logical_job_id.clone(),
            active_delegation_digest: self.response.active_delegation_digest.clone(),
            expected_authority_epoch: self.response.authority_epoch,
        })
    }
    pub(super) fn validate_retry_response(
        &self,
        request: &wire::RetryImportSourceRequest,
        response: &wire::MutationResponse,
        observed: Option<&wire::RecordRef>,
    ) -> Result<()> {
        let mut prior = Vec::new();
        if let Some(original) = &request.original_operation {
            prior.push(original.id.clone());
        }
        if let Some(observed) = observed {
            prior.push(observed.id.clone());
        }
        authority::validate_retry_response(request, response, &prior)?;
        Ok(())
    }
}

/// Exact authenticated proposal retained until the caller signs. This value is
/// not delegated permission and cannot authorize native installation.
#[derive(Clone, Debug)]
pub struct PreparedImportJob {
    destination: wire::SpoolRef,
    response: wire::PrepareImportJobResponse,
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
            || request
                .page
                .as_ref()
                .is_some_and(|page| page.size as usize > authority::MAX_IMPORT_SOURCE_REF_PAGE)
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

    /// Cursor continuity is verified by the authenticated resolver. Start from
    /// the first page and discard each advertisement after bounded validation.
    async fn discover_import_source(
        &self,
        source: &ResolvedImportSource,
        selected: &[wire::ImportBranchLimitV1],
        mode: authority::DiscoverySelectionMode,
    ) -> Result<(wire::ProviderRepository, authority::DiscoveryDigest)> {
        let mut request = source.request.clone();
        request.include_refs = true;
        request.page = Some(wire::PageRequest {
            size: authority::MAX_IMPORT_SOURCE_REF_PAGE as u32,
            after_page: Vec::new(),
        });
        let options = authority::DiscoveryOptions {
            include_pull_request_refs: request.include_pull_request_refs,
        };
        let first = self.resolve_import_source(&request).await?;
        let mut discovery = ImportDiscovery::new(&first.source, selected, options, mode)?;
        let mut next = discovery.push_page(&first.source)?;
        while !next.is_empty() {
            request.page.as_mut().ok_or(Reject::Canonical)?.after_page = next;
            let page = self.resolve_import_source(&request).await?;
            next = discovery.push_page(&page.source)?;
            if !next.is_empty() && request.page.as_ref().is_some_and(|p| p.after_page == next) {
                return Err(Reject::Canonical.into());
            }
        }
        discovery.finish()
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
    /// Recover the logical job from the operation's durable destination/job
    /// association. Missing metadata is unavailable; an attempt ID is never a job ID.
    pub async fn get_import_job_state_from_operation(
        &self,
        operation: &wire::OperationRecord,
    ) -> Result<Option<ImportJobState>> {
        let Some(request) = authority::import_job_state_request_from_operation(operation)? else {
            return Ok(None);
        };
        Ok(Some(self.get_import_job_state(&request).await?))
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
        let destination = request.destination.as_ref().ok_or(Reject::Canonical)?;
        let identity = request.identity.as_ref().ok_or(Reject::Canonical)?;
        let spool = uuid::Uuid::parse_str(&destination.id).map_err(|_| Reject::Canonical)?;
        if destination != &configuration.destination
            || identity.spool_uuid != spool.as_bytes()
            || request.retry_lineage_id.len() != 16
        {
            return Err(Reject::Scope.into());
        }
        if request.encoded_len() > authority::MAX_BUNDLE_BYTES {
            return Err(Reject::Bounds.into());
        }
        authority::initial_operation_id(&request.retry_lineage_id, false)?;
        let proposed = request.proposed_scope.as_ref().ok_or(Reject::Canonical)?;
        validate_source_identity(request, source)?;
        let (current_source, discovery) = self
            .discover_import_source(
                source,
                &proposed.branches,
                authority::DiscoverySelectionMode::Prepare,
            )
            .await?;
        // Empty explicitly asks Prepare for the current opaque CAS token.
        // Source format/custody are checked before RPC; the complete prepared
        // scope is checked against the issued token before it reaches signing.
        if !proposed.destination_version.is_empty() {
            authority::prepare_import_source_scope_with_discovery(
                request,
                &current_source,
                source.source.connection.as_ref().map(|_| source.provider),
                &configuration.response,
                &proposed.destination_version,
                &discovery,
            )?;
        }
        let response: wire::PrepareImportJobResponse = self
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/PrepareImportJob",
                request,
            )
            .await?;
        validate_preparation(request, &response, chrono::Utc::now().timestamp())?;
        let returned = response
            .proposal
            .as_ref()
            .and_then(|p| p.scope.as_ref())
            .ok_or(Reject::Canonical)?;
        authority::prepare_import_source_scope_with_discovery(
            request,
            &current_source,
            source.source.connection.as_ref().map(|_| source.provider),
            &configuration.response,
            &returned.destination_version,
            &discovery,
        )?;
        Ok(PreparedImportJob {
            destination: destination.clone(),
            response,
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
        let signed = request
            .proof
            .as_ref()
            .and_then(|p| p.delegations.first())
            .and_then(|d| d.body.as_ref())
            .and_then(|b| b.scope.as_ref())
            .ok_or(Reject::Canonical)?;
        let (source, discovery) = self
            .discover_import_source(
                &prepared.source,
                &signed.branches,
                authority::DiscoverySelectionMode::Commit,
            )
            .await?;
        current.source.source = source;
        validate_commit(&current, request, resolved_provider, expected, &discovery)?;
        let response: wire::MutationResponse = self
            .call_unary(
                "/heddle.api.v1alpha2.IntegrationService/CommitImportJob",
                request,
            )
            .await
            .map_err(commit_failure)?;
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

    /// Select the active delegation's host-issued cancellation ID, not a
    /// parent permission. The host resolves stored replay first.
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

// Commit admission refusals have no accepted receipt and are not replay-frozen.
// Match both wire fields so unrelated ALREADY_EXISTS/ABORTED failures keep their
// original detail. Callers can distinguish a reserved destination from stale CAS.
fn commit_failure(error: HostedError) -> HostedError {
    use api::heddle::api::common::{CallFailureCode, ErrorReason};
    match &error {
        HostedError::Call {
            code: CallFailureCode::AlreadyExists,
            error: Some(detail),
            ..
        } if detail.reason == ErrorReason::ImportDestinationConflict as i32 => {
            Reject::PreparationRefused(wire::ImportPreparationRefusalReason::DestinationConflict)
                .into()
        }
        HostedError::Call {
            code: CallFailureCode::Aborted,
            error: Some(detail),
            ..
        } if detail.reason == ErrorReason::VersionConflict as i32 => Reject::StaleContext.into(),
        _ => error,
    }
}

fn resolved_source(
    request: &wire::ResolveImportSourceRequest,
    response: &wire::ResolveImportSourceResponse,
) -> Result<ResolvedImportSource> {
    let selected = request.source.as_ref().ok_or(Reject::SourceSelection)?;
    authority::validate_resolve_import_source_response(
        request,
        response,
        selected.connection.as_ref().map(|_| "github"),
    )?;
    let source = response.source.as_ref().ok_or(Reject::SourceSelection)?;
    let provider =
        authority::resolve_import_provider(source, source.connection.as_ref().map(|_| "github"))?;
    Ok(ResolvedImportSource {
        request: request.clone(),
        source: source.clone(),
        provider,
    })
}

fn validate_source_identity(
    request: &wire::PrepareImportJobRequest,
    source: &ResolvedImportSource,
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
    Ok(())
}

fn validate_commit(
    prepared: &PreparedImportJob,
    request: &wire::CommitImportJobRequest,
    resolved_provider: &str,
    expected: &authority::ImportOwnerExpectation<'_>,
    discovery: &authority::DiscoveryDigest,
) -> Result<()> {
    if request.destination.as_ref() != Some(&prepared.destination) {
        return Err(Reject::StaleContext.into());
    }
    if resolved_provider != prepared.source.provider {
        return Err(Reject::SourceSelection.into());
    }
    authority::validate_commit_request_with_discovery(
        request,
        resolved_provider,
        &prepared.source.source,
        &prepared.configuration.response,
        discovery,
    )?;
    prepared.preflight(request.proof.as_ref().ok_or(Reject::Canonical)?, expected)
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
        || response.max_validity_duration_seconds > authority::MAX_DELEGATION_WINDOW_SECONDS
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
    let [signed] = proof.delegations.as_slice() else {
        return Err(Reject::ImportPermission.into());
    };
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
        || prepared.response.max_validity_duration_seconds
            > authority::MAX_DELEGATION_WINDOW_SECONDS
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
        let fixtures = [
            include_str!("../../../../../thread-api/tests/fixtures/hybrid-alpha33.json"),
            include_str!("../../../../tests/fixtures/import-sibling-jobs-alpha32.json"),
        ];
        for fixture in fixtures {
            let f: serde_json::Value = serde_json::from_str(fixture).expect("alpha.33 vectors");
            if let Some(bytes) = f[section][name]["wire_hex"].as_str() {
                return T::decode(hex::decode(bytes).expect("hex").as_slice()).expect("fixed wire");
            }
        }
        panic!("missing vector {name}");
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
            "/../thread-api/tests/fixtures/hybrid-alpha33.json"
        )))
        .expect("published alpha.25 vectors")
    }

    fn with_context<T>(
        now: i64,
        check: impl FnOnce(&authority::ImportOwnerExpectation<'_>) -> T,
    ) -> T {
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
        })
    }

    fn prepared(response: &str) -> PreparedImportJob {
        PreparedImportJob {
            destination: wire::<wire::CommitImportJobRequest>("wire_vectors", "commit_request")
                .destination
                .expect("destination"),
            response: wire("wire_vectors", response),
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
    fn validate_commit_fixture(
        prepared: &PreparedImportJob,
        request: &wire::CommitImportJobRequest,
        provider: &str,
        expected: &authority::ImportOwnerExpectation<'_>,
    ) -> Result<()> {
        let scope = request
            .proof
            .as_ref()
            .and_then(|p| p.delegations.first())
            .and_then(|d| d.body.as_ref())
            .and_then(|b| b.scope.as_ref())
            .ok_or(Reject::Canonical)?;
        let discovery = authority::discover_repository(
            &prepared.source.source,
            &scope.branches,
            authority::DiscoveryOptions::default(),
            authority::DiscoverySelectionMode::Commit,
        )?;
        validate_commit(prepared, request, provider, expected, &discovery)
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
        validate_source_identity(&prepare, &resolved).expect("independent SHA-256 format");
        let unknown = wire("wire_vectors", "resolve_unknown_response");
        let unknown = resolved_source(&request, &unknown).expect("unknown is valid discovery");
        assert!(matches!(
            validate_source_identity(&prepare, &unknown),
            Err(super::super::super::HostedError::Hybrid(Reject::Version))
        ));
        let mut changed = resolved.clone();
        changed.source.connection = source("source_connected").source.connection;
        assert!(validate_source_identity(&prepare, &changed).is_err());
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
        validate_source_identity(&prepare, &partial)
            .expect("identity is independent of page coverage");
        validate_source_identity(&prepare, &resolved).expect("unchanged complete discovery");
    }

    #[test]
    fn operation_job_discovery_preserves_destination_and_never_guesses_from_attempts() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/hybrid-job-selector-v1.json"
        ))
        .expect("published operation discovery vectors");
        for row in fixture["operations"].as_array().expect("operations") {
            let operation = wire::OperationRecord::decode(
                hex::decode(row["wire_hex"].as_str().expect("operation bytes"))
                    .expect("hex")
                    .as_slice(),
            )
            .expect("operation");
            let result = authority::import_job_state_request_from_operation(&operation);
            match row["expected"].as_str().expect("expected") {
                "OK" => {
                    let request = result.expect("valid selector").expect("available");
                    assert_eq!(
                        hex::encode(request.encode_to_vec()),
                        row["request_wire_hex"],
                        "{row}"
                    );
                }
                "unavailable" => assert_eq!(result.expect("absent metadata"), None, "{row}"),
                _ => assert!(result.is_err(), "malformed metadata: {row}"),
            }
        }
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
                    validate_commit_fixture(&current, &request, "github", expected).is_err(),
                    "{row}"
                );
            }
            let current = prepared("commit_preparation");
            let control = wire("wire_vectors", "commit_request");
            validate_commit_fixture(&current, &control, "github", expected)
                .expect("unchanged current source and support");
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
            validate_commit_fixture(&prepared, &control, "github", expected)
                .expect("signed initial Commit");
            for row in fixture()["submission_vectors"]["commit_negatives"]
                .as_array()
                .expect("rows")
            {
                let request = wire("wire_vectors", row["request"].as_str().expect("request"));
                assert!(
                    validate_commit_fixture(&prepared, &request, "github", expected).is_err(),
                    "{row}"
                );
            }
            let mut multiple = control.clone();
            let proof = multiple.proof.as_mut().expect("proof");
            proof.delegations.push(proof.delegations[0].clone());
            assert!(validate_commit_fixture(&prepared, &multiple, "github", expected).is_err());
            let mut unsigned = control.clone();
            unsigned.proof.as_mut().expect("proof").delegations[0].delegating_signature = None;
            assert!(validate_commit_fixture(&prepared, &unsigned, "github", expected).is_err());
            assert!(
                validate_commit_fixture(&prepared, &control, "another-provider", expected).is_err()
            );
            validate_commit_fixture(&prepared, &control, "github", expected)
                .expect("unchanged signed control");
        });
    }

    #[test]
    fn browser_preflight_allows_only_advertised_skew_and_never_expiry_grace() {
        let proof = wire::<wire::CommitImportJobRequest>("wire_vectors", "commit_request")
            .proof
            .expect("proof");
        let prepared = prepared("commit_preparation");
        let start = proof.delegations[0]
            .body
            .as_ref()
            .expect("delegation")
            .not_before_unix_seconds;
        let skew = i64::try_from(prepared.response.clock_skew_allowance_seconds).expect("skew");
        for (now, accepted) in [
            (start - skew - 1, false),
            (start - skew, true),
            (start - 1, true),
            (1100, true),
            (1300, false),
        ] {
            with_context(now, |expected| {
                assert_eq!(
                    prepared.preflight(&proof, expected).is_ok(),
                    accepted,
                    "advertised skew or exclusive expiry at {now}"
                )
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
        let mut other = response.clone();
        let Some(wire::mutation_receipt::Outcome::PendingOperation(operation)) =
            other.receipt.as_mut().expect("receipt").outcome.as_mut()
        else {
            panic!("pending operation");
        };
        operation.id = uuid::Uuid::from_u128(7).to_string();
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
        let active: wire::SignedImportJobDelegationV1 = wire("signed_vectors", "delegation");
        let body = active.body.as_ref().expect("delegation");
        let request = wire::CancelImportJobRequest {
            client_operation_id: "cancel-request".into(),
            destination: Some(configuration().destination),
            logical_job_id: body.logical_job_id.clone(),
            cancellation_id: body.cancellation_id.clone(),
            expected_authority_epoch: 1,
        };
        authority::check_cancel_request(&request, &active, 1, false)
            .expect("sole active delegation");
        let mut changed = request.clone();
        changed.cancellation_id[0] ^= 1;
        assert!(authority::check_cancel_request(&changed, &active, 1, false).is_err());
        assert!(authority::check_cancel_request(&request, &active, 2, false).is_err());
        authority::check_cancel_replay(&request, &request).expect("exact replay");
        assert_eq!(
            authority::check_cancel_replay(&changed, &request),
            Err(Reject::OperationIdReused)
        );
    }

    #[test]
    fn alpha33_sibling_preparations_survive_independent_commits() {
        let configuration_response: wire::GetImportConfigurationResponse =
            wire("wire_vectors", "configuration");
        let mut resolved = source("source_connected");
        resolved.source = wire("wire_vectors", "source");
        let prepare = |name| {
            let request: wire::CommitImportJobRequest =
                wire("wire_vectors", &format!("commit_{name}"));
            let destination = request.destination.clone().expect("destination");
            let prepared = PreparedImportJob {
                destination: destination.clone(),
                response: wire("wire_vectors", &format!("prepared_{name}")),

                configuration: ImportConfiguration {
                    destination,
                    response: configuration_response.clone(),
                },
                source: resolved.clone(),
            };
            (prepared, request)
        };
        let a = prepare("a");
        let b = prepare("b");
        let a_id =
            &a.0.response
                .proposal
                .as_ref()
                .expect("proposal")
                .logical_job_id;
        let b_id =
            &b.0.response
                .proposal
                .as_ref()
                .expect("proposal")
                .logical_job_id;
        assert_ne!(a_id, b_id);
        assert_eq!(a.0.destination, b.0.destination);
        let retained_a = a.0.response.clone();
        let retained_b = b.0.response.clone();
        with_context(1100, |expected| {
            for jobs in [[&a, &b], [&b, &a]] {
                for (prepared, request) in jobs {
                    validate_commit_fixture(prepared, request, "github", expected)
                        .expect("each disjoint signed job can activate in either order");
                }
            }
            assert!(validate_commit_fixture(&a.0, &b.1, "github", expected).is_err());
            assert!(validate_commit_fixture(&b.0, &a.1, "github", expected).is_err());
        });
        assert_eq!(a.0.response, retained_a);
        assert_eq!(b.0.response, retained_b);
    }

    #[test]
    fn import_budget_covers_decoded_forecast_without_exceeding_host_maximum() {
        let mut configuration = configuration();
        let host_maximum = 16 * 1024 * 1024 * 1024;
        configuration
            .response
            .limits
            .as_mut()
            .expect("limits")
            .max_result_bytes = host_maximum;
        let mut source = source("source_connected");
        let scope: wire::ImportPermissionScopeV1 = wire("wire_vectors", "scope");
        source.source.size_estimate_state = 1;
        source.source.git_size_kib = 400 * 1024;
        let git_bytes = source.source.git_size_kib * 1024;
        for job in configuration
            .sibling_scopes(&source, &scope)
            .expect("forecast")
        {
            assert!(
                job.max_result_bytes >= git_bytes * 27,
                "decoded native bytes fit"
            );
            assert_eq!(job.max_result_bytes, git_bytes * 32);
            assert!(job.max_result_bytes <= host_maximum);
        }
        source.source.git_size_kib = 1024 * 1024;
        assert!(
            configuration
                .sibling_scopes(&source, &scope)
                .expect("host cap")
                .iter()
                .all(|s| s.max_result_bytes == host_maximum)
        );
    }

    #[test]
    fn alpha33_discovery_plans_disjoint_jobs_with_one_positive_total_each() {
        let mut configuration = configuration();
        let limits = configuration.response.limits.as_mut().expect("limits");
        limits.max_branches = 256;
        limits.max_operations = 256;
        limits.max_result_bytes = u64::MAX;
        let mut source = source("source_connected");
        let mut scope: wire::ImportPermissionScopeV1 = wire("wire_vectors", "scope");
        let base = scope.branches[0].clone();
        scope.branches = (0..512)
            .map(|i: u16| {
                let mut b = base.clone();
                b.ref_name = format!("refs/heads/branch-{i:04}");
                b.genesis_digest[..2].copy_from_slice(&i.to_be_bytes());
                b.target_thread_id = b.genesis_digest.clone();
                b
            })
            .collect();
        source.source.refs.clear();
        source.source.size_estimate_state = 1;
        source.source.git_size_kib = 300 * 1024;
        let jobs = configuration
            .sibling_scopes(&source, &scope)
            .expect("whole repository");
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs.iter().map(|s| s.branches.len()).sum::<usize>(), 512);
        assert!(
            jobs.iter()
                .all(|s| s.max_operations == 256 && s.max_result_bytes == 300 * 1024 * 1024 * 32)
        );
        assert!(jobs[0].branches.last().expect("last").ref_name < jobs[1].branches[0].ref_name);
        assert!(
            jobs.iter()
                .all(|s| s.destination_version == scope.destination_version)
        );
        configuration
            .response
            .limits
            .as_mut()
            .expect("limits")
            .max_result_bytes = 123;
        let capped = configuration
            .sibling_scopes(&source, &scope)
            .expect("current host cap");
        assert!(capped.iter().all(|s| s.max_result_bytes == 123));
        source.source.git_size_kib = 0;
        assert!(
            configuration
                .sibling_scopes(&source, &scope)
                .expect("empty estimate")
                .iter()
                .all(|s| s.max_result_bytes == 1)
        );
        source.source.size_estimate_state = 0;
        assert!(
            configuration
                .sibling_scopes(&source, &scope)
                .expect("unknown")
                .iter()
                .all(|s| s.max_result_bytes == 123)
        );
        source.source.git_size_kib = 1;
        assert!(
            configuration.sibling_scopes(&source, &scope).is_err(),
            "UNKNOWN cannot carry a size"
        );
        source.source.size_estimate_state = 1;
        source.source.git_size_kib = u64::MAX;
        configuration
            .response
            .limits
            .as_mut()
            .expect("limits")
            .max_result_bytes = u64::MAX;
        assert!(
            configuration
                .sibling_scopes(&source, &scope)
                .expect("widened arithmetic")
                .iter()
                .all(|s| s.max_result_bytes == u64::MAX)
        );
        scope.branches[256].ref_name = scope.branches[255].ref_name.clone();
        assert!(
            configuration.sibling_scopes(&source, &scope).is_err(),
            "cross-job duplicate ref"
        );
    }

    #[test]
    fn alpha33_writer_snapshot_selects_retry_and_rejects_reused_attempts() {
        let read = ImportJobState {
            request: wire("wire_vectors", "job_state_request"),
            response: wire("wire_vectors", "job_state"),
        };
        let request = read
            .retry_request("retry-alpha33".into())
            .expect("writer Retry target");
        assert_eq!(request, wire("wire_vectors", "retry_request"));
        let mut response: wire::MutationResponse = wire("wire_vectors", "commit_response");
        response
            .receipt
            .as_mut()
            .expect("receipt")
            .client_operation_id = request.client_operation_id.clone();
        let Some(wire::mutation_receipt::Outcome::PendingOperation(operation)) =
            response.receipt.as_mut().expect("receipt").outcome.as_mut()
        else {
            panic!("pending");
        };
        operation.id = uuid::Uuid::from_u128(99).to_string();
        read.validate_retry_response(&request, &response, None)
            .expect("fresh host attempt UUID");
        let original = request.original_operation.as_ref().expect("original");
        let observed = wire::RecordRef {
            spool: original.spool.clone(),
            id: uuid::Uuid::from_u128(98).to_string(),
        };
        for id in [&original.id, &observed.id] {
            let mut reused = response.clone();
            let Some(wire::mutation_receipt::Outcome::PendingOperation(operation)) =
                reused.receipt.as_mut().expect("receipt").outcome.as_mut()
            else {
                panic!("pending");
            };
            operation.id = id.clone();
            assert!(
                read.validate_retry_response(&request, &reused, Some(&observed))
                    .is_err(),
                "Retry must not reuse a known physical attempt"
            );
        }
        let mut expired = read.clone();
        expired.response.status = wire::ImportJobStatus::Expired as i32;
        expired.response.retry_availability = Some(
            wire::get_import_job_state_response::RetryAvailability::RetryUnavailable(
                wire::ImportRetryUnavailableReason::AuthorityExpired as i32,
            ),
        );
        authority::validate_job_state_response(&expired.request, &expired.response)
            .expect("typed expired snapshot");
        assert!(expired.retry_request("retry-expired".into()).is_err());
        let mut missing = read.clone();
        missing.response.retry_availability = None;
        assert!(missing.retry_request("retry-missing".into()).is_err());
    }

    #[test]
    fn alpha33_preflight_accepts_the_advertised_24_hour_window() {
        let prepared = prepared("window_24h_preparation");
        let signed: wire::SignedImportJobDelegationV1 = wire("signed_vectors", "window_24h");
        exact_signed_delegation(&prepared, &signed, 1100).expect("host-advertised 24 hours");
        let mut too_short = prepared.clone();
        too_short.response.max_validity_duration_seconds = 86399;
        assert!(
            exact_signed_delegation(&too_short, &signed, 1100).is_err(),
            "duration stays bounded by the advertisement"
        );
    }

    #[test]
    fn alpha33_preflight_refuses_host_windows_above_seven_days() {
        let mut prepared = prepared("window_24h_preparation");
        let signed: wire::SignedImportJobDelegationV1 = wire("signed_vectors", "window_24h");
        prepared.response.max_validity_duration_seconds = authority::MAX_DELEGATION_WINDOW_SECONDS;
        exact_signed_delegation(&prepared, &signed, 1100).expect("seven-day advertised ceiling");
        prepared.response.max_validity_duration_seconds += 1;
        assert!(
            matches!(
                exact_signed_delegation(&prepared, &signed, 1100),
                Err(HostedError::Hybrid(Reject::ValidityBounds))
            ),
            "even a shorter signed window cannot accept an excessive host D"
        );
    }

    #[test]
    fn alpha33_commit_conflicts_have_distinct_typed_outcomes() {
        use api::heddle::api::common::{CallFailure, CallFailureCode, ErrorDetail, ErrorReason};
        for (name, expected) in [
            (
                "commit_destination_conflict",
                Reject::PreparationRefused(
                    wire::ImportPreparationRefusalReason::DestinationConflict,
                ),
            ),
            ("commit_stale_destination", Reject::StaleContext),
        ] {
            let failure: CallFailure = wire("wire_vectors", name);
            assert_eq!(
                authority::import_commit_conflict_failure(&expected),
                Some(failure.clone())
            );
            assert!(
                matches!(commit_failure(failure.into()), HostedError::Hybrid(ref actual) if actual == &expected),
                "Commit wire failures must preserve their typed distinction"
            );
        }
        // Neither a generic code nor the import reason on a different code
        // authorizes this mapping. Keep the original structured failure.
        for (code, reason) in [
            (CallFailureCode::AlreadyExists, ErrorReason::AlreadyExists),
            (
                CallFailureCode::Aborted,
                ErrorReason::ImportDestinationConflict,
            ),
            (CallFailureCode::AlreadyExists, ErrorReason::VersionConflict),
        ] {
            let failure = CallFailure {
                code: code as i32,
                error: Some(ErrorDetail {
                    reason: reason as i32,
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert!(
                matches!(commit_failure(failure.into()), HostedError::Call { code: actual, .. } if actual == code)
            );
        }
    }

    #[test]
    fn alpha33_job_state_requires_status_and_retry_reason_agreement() {
        use self::wire::get_import_job_state_response::RetryAvailability;
        let original = ImportJobState {
            request: wire("wire_vectors", "job_state_request"),
            response: wire("wire_vectors", "job_state"),
        };
        for status in 1..=5 {
            for reason in 1..=7 {
                let mut state = original.clone();
                state.response.status = status;
                state.response.retry_availability =
                    Some(RetryAvailability::RetryUnavailable(reason));
                let agrees = matches!(
                    (status, reason),
                    (1, 1 | 2 | 6) | (2, 3) | (3, 4) | (4, 5) | (5, 7)
                );
                assert_eq!(
                    authority::validate_job_state_response(&state.request, &state.response).is_ok(),
                    agrees,
                    "status {status} and retry refusal {reason}"
                );
            }
        }
    }
}
