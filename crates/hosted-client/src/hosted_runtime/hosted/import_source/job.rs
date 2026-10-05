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

    /// Plan independently signed sibling jobs in this destination. Discovery
    /// estimates Git storage, so allow twice that size for converted results.
    /// Ref counts weight the allocation; they are not branch size estimates.
    /// Every job gets a positive total bounded only by the current host maximum.
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
            1 => Some(u128::from(source.source.git_size_kib) * 1024 * 2),
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
                    let allocated = (bytes * branches.len() as u128).div_ceil(count as u128);
                    allocated.max(1).min(u128::from(limits.max_result_bytes)) as u64
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

/// Authenticated destination-writer snapshot for reviewing the remaining scope.
/// Prepare must return this exact snapshot before the caller signs a renewal.
#[derive(Clone, Debug)]
pub struct ImportJobState {
    request: wire::GetImportJobStateRequest,
    response: wire::GetImportJobStateResponse,
    verified_history: Option<authority::VerifiedImportBundleWitnesses>,
}
impl ImportJobState {
    pub fn response(&self) -> &wire::GetImportJobStateResponse {
        &self.response
    }

    /// Exact custody retained by the host, including after loss of client state.
    pub fn retained_source(&self) -> Result<&wire::ImportSourceSelectionV1> {
        Ok(self
            .response
            .retained_source
            .as_ref()
            .ok_or(Reject::SourceSelection)?)
    }

    /// Retry uses the writer-only target and active authority from the same
    /// authenticated read, even when ordinary operation visibility differs.
    pub(super) fn retry_request(
        &self,
        client_operation_id: String,
    ) -> Result<wire::RetryImportSourceRequest> {
        use wire::get_import_job_state_response::RetryAvailability;
        require_control(
            self.response
                .control_availability
                .as_ref()
                .and_then(|c| c.retry.as_ref()),
        )?;
        let Some(RetryAvailability::EligibleRetryTarget(target)) =
            &self.response.retry_availability
        else {
            return Err(Reject::Transition.into());
        };
        let state = self.response.state.as_ref().ok_or(Reject::Canonical)?;
        let active = state.active_predecessor.as_ref().ok_or(Reject::Canonical)?;
        Ok(wire::RetryImportSourceRequest {
            client_operation_id,
            original_operation: target.operation_ref.as_ref().map(|r| wire::RecordRef {
                spool: r.spool.clone(),
                id: r.id.clone(),
            }),
            expected_operation_version: target.operation_version.clone(),
            logical_job_id: self.request.logical_job_id.clone(),
            active_delegation_digest: authority::signed_delegation_digest(active)?,
            expected_authority_epoch: state.authority_epoch,
        })
    }

    pub(super) fn validate_retry_response(
        &self,
        request: &wire::RetryImportSourceRequest,
        response: &wire::MutationResponse,
        observed: Option<&wire::RecordRef>,
    ) -> Result<()> {
        let state = self.response.state.as_ref().ok_or(Reject::Canonical)?;
        let mut prior = vec![authority::initial_operation_id(
            &state.retry_lineage_id,
            false,
        )?];
        if let Some(observed) = observed {
            prior.push(observed.id.clone());
        }
        let proof = self
            .response
            .retained_proof
            .as_ref()
            .ok_or(Reject::Canonical)?;
        for operation in &proof.operations {
            let body = operation.body.as_ref().ok_or(Reject::Canonical)?;
            prior.push(authority::initial_operation_id(
                &body.physical_operation_id,
                false,
            )?);
        }
        authority::validate_retry_response(request, response, &prior)?;
        Ok(())
    }

    /// Refresh public witness metadata after proof lookup, preserving every
    /// retained original, authority and accepted CAS byte. Reverify afterwards.
    pub fn refresh_receiver_metadata(
        &mut self,
        refreshed: wire::ImportPublicProofBundleV1,
    ) -> Result<()> {
        let retained = self
            .response
            .retained_proof
            .as_mut()
            .ok_or(Reject::Canonical)?;
        thread_api::hybrid::history::replace_receiver_metadata(retained, refreshed)?;
        self.verified_history = None;
        Ok(())
    }

    fn original_admitted(&self) -> Result<bool> {
        let verified = self.verified_history.as_ref().ok_or(Reject::Scope)?;
        Ok(verified
            .owner_check_times_unix_seconds
            .first()
            .is_some_and(Option::is_some))
    }

    fn original_admission(
        &self,
        now_unix_seconds: i64,
    ) -> Result<authority::ImportOriginalAdmission<'_>> {
        Ok(authority::ImportOriginalAdmission {
            original: self
                .response
                .retained_proof
                .as_ref()
                .and_then(|b| b.delegations.first())
                .ok_or(Reject::Canonical)?,
            admitted: self.original_admitted()?,
            now_unix_seconds,
        })
    }

    fn logical_job_terminal(&self) -> Result<bool> {
        let cancel = self
            .response
            .control_availability
            .as_ref()
            .and_then(|c| c.cancel.as_ref())
            .ok_or(Reject::Canonical)?;
        Ok(matches!(cancel.availability,
            Some(wire::import_control_availability_v1::Availability::Unavailable(reason))
                if reason == wire::ImportControlUnavailableReason::Terminal as i32))
    }

    /// Verify retained signatures and witness history before reviewing remaining
    /// authority. The API derives historical times and distinguishes recovery.
    /// Persist its optional snapshot under the receiver's existing trust lock.
    pub fn verify_witnesses<'a>(
        &mut self,
        pin: &authority::ImportWitnessRootPin,
        snapshot: Option<&authority::ImportWitnessSnapshot>,
        now_millis: i64,
        owner_at: impl FnMut(
            usize,
            Option<i64>,
        )
            -> std::result::Result<authority::ImportBundleOwnerExpectation<'a>, Reject>,
        verify_policy: impl FnMut(
            &wire::ImportPublicProofBundleV1,
            Option<&api::heddle::api::common::HostedWitnessStatementV1>,
        ) -> std::result::Result<(), Reject>,
    ) -> Result<&authority::VerifiedImportBundleWitnesses> {
        let proof = self
            .response
            .retained_proof
            .as_ref()
            .ok_or(Reject::Canonical)?;
        let verified = authority::verify_import_bundle_witnesses(
            proof,
            pin,
            snapshot,
            now_millis,
            owner_at,
            verify_policy,
        )?;
        if self.response.state.as_ref() != Some(&verified.accepted_history) {
            return Err(Reject::StaleContext.into());
        }
        self.verified_history = Some(verified);
        self.verified_history
            .as_ref()
            .ok_or_else(|| Reject::Canonical.into())
    }

    fn validate_preparation(&self, request: &wire::PrepareImportJobRequest) -> Result<()> {
        if self.request.destination != request.destination
            || self.request.logical_job_id != request.renew_logical_job_id
        {
            return Err(Reject::StaleContext.into());
        }
        if request.source.as_ref() != Some(self.retained_source()?) {
            return Err(Reject::SourceSelection.into());
        }
        Ok(())
    }

    /// An expired predecessor is a CAS handle, never executable permission.
    pub fn predecessor(
        &self,
        expected: &authority::ImportOwnerExpectation<'_>,
    ) -> Result<authority::VerifiedImportRenewalPredecessor> {
        let verified = self.verified_history.as_ref().ok_or(Reject::Scope)?;
        let state = self.response.state.as_ref().ok_or(Reject::StaleContext)?;
        if &verified.accepted_history != state {
            return Err(Reject::StaleContext.into());
        }
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
        authority::validate_control_state_response(request, &response)?;
        Ok(ImportJobState {
            request: request.clone(),
            response,
            verified_history: None,
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
        require_control(
            read.response
                .control_availability
                .as_ref()
                .and_then(|c| c.renew.as_ref()),
        )?;
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
            Some(read) => read.validate_preparation(request)?,
            None if request.renew_logical_job_id.is_empty() => {}
            None => return Err(Reject::StaleContext.into()),
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
                    read.response
                        .retained_source
                        .as_ref()
                        .ok_or(Reject::SourceSelection)?,
                ))
            })
            .transpose()?;
        let original_admission = read
            .map(|read| read.original_admission(chrono::Utc::now().timestamp()))
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
                original_admission.as_ref(),
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
            original_admission.as_ref(),
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

    pub(super) async fn validate_import_endpoint(
        &self,
        response: &wire::MutationResponse,
    ) -> Result<()> {
        let remote = self
            .native()
            .await
            .map_err(super::super::HostedError::transport)?;
        if response
            .receipt
            .as_ref()
            .is_none_or(|r| r.endpoint != remote.description.endpoint)
        {
            return Err(Reject::PendingOperation.into());
        }
        Ok(())
    }

    /// Recover an expired predecessor only as a non-executable authenticated
    /// CAS handle, then independently check current replacement authority.
    pub async fn renew_import_job(
        &self,
        submission: &ImportRenewalSubmission,
    ) -> Result<wire::MutationResponse> {
        let request = api::hybrid_codec::strict_decode::<wire::RenewImportJobRequest>(
            submission.bytes(),
            2 * authority::MAX_BUNDLE_BYTES,
        )?;
        let response = self
            .call_unary_encoded(
                "/heddle.api.v1alpha2.IntegrationService/RenewImportJob",
                submission.bytes(),
            )
            .await?;
        authority::validate_renew_response(&request, &response)?;
        self.validate_import_endpoint(&response).await?;
        Ok(response)
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

fn require_control(control: Option<&wire::ImportControlAvailabilityV1>) -> Result<()> {
    use wire::import_control_availability_v1::Availability;
    match control.and_then(|c| c.availability.as_ref()) {
        Some(Availability::Available(true)) => Ok(()),
        Some(Availability::Unavailable(reason)) => {
            Err(super::super::HostedError::ImportControlUnavailable(
                wire::ImportControlUnavailableReason::try_from(*reason)
                    .map_err(|_| Reject::Canonical)?,
            ))
        }
        _ => Err(Reject::Canonical.into()),
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
    read.predecessor(predecessor_context)?;
    require_control(
        read.response
            .control_availability
            .as_ref()
            .and_then(|c| c.renew.as_ref()),
    )?;
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
        read.logical_job_terminal()?,
        read.original_admitted()?,
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
        let fixtures = [
            include_str!("../../../../../thread-api/tests/fixtures/hybrid-alpha32.json"),
            include_str!("../../../../tests/fixtures/import-job-control-alpha31.json"),
            include_str!("../../../../tests/fixtures/import-consumer-alpha32.json"),
            include_str!("../../../../tests/fixtures/import-sibling-jobs-alpha32.json"),
        ];
        for fixture in fixtures {
            let f: serde_json::Value = serde_json::from_str(fixture).expect("alpha.32 vectors");
            if let Some(bytes) = f[section][name]["wire_hex"].as_str() {
                return T::decode(hex::decode(bytes).expect("hex").as_slice()).expect("fixed wire");
            }
        }
        panic!("missing vector {name}");
    }

    fn verified_read(name: &str) -> ImportJobState {
        let mut response: wire::GetImportJobStateResponse = wire("wire_vectors", name);
        // Retain paths only for this read's included statements. The complete
        // frozen archive also carries a path for the not-yet-published result.
        let proof = response.retained_proof.as_mut().expect("proof");
        proof.history_proofs =
            wire::<wire::ImportPublicProofBundleV1>("wire_vectors", "alpha31_bundle")
                .history_proofs;
        let entries = &proof
            .witness_set
            .as_ref()
            .expect("set")
            .body
            .as_ref()
            .expect("body")
            .entries;
        proof.history_proofs.retain(|path| {
            proof.statements.iter().any(|signed| {
                let body = signed.body.as_ref().expect("statement");
                let leaf = api::witness_trust::leaf_digest(
                    body.purpose,
                    &api::hybrid_codec::canonical(body).expect("canonical"),
                    &signed.signature,
                )
                .expect("leaf");
                entries
                    .iter()
                    .any(|entry| api::witness_trust::verify_inclusion(&leaf, path, entry).is_ok())
            })
        });
        let controls: wire::GetImportJobStateResponse =
            wire("wire_vectors", "consumer_controls_owner");
        response.retry_availability = controls.retry_availability;
        response.control_availability = controls.control_availability;
        let mut read = ImportJobState {
            request: wire("wire_vectors", "job_state_request"),
            response,
            verified_history: None,
        };
        with_context(1350, |expected| {
            let owners = [authority::ImportBundleOwnerExpectation {
                identity: expected.identity,
                owner_public_key: expected.owner_public_key,
                owner_chain_digest: expected.owner_chain_digest,
                authority_expires_at_seconds: expected.authority_expires_at_seconds,
                effective_from_unix_seconds: 1000,
                effective_until_unix_seconds: None,
                forbidden_job_keys: expected.forbidden_job_keys,
                known_job_associations: expected.known_job_associations,
            }];
            let f = fixture();
            let root = hex::decode(f["keys"]["root"]["public_key_hex"].as_str().expect("root"))
                .expect("key");
            let history: wire::OwnerHistory = wire("wire_vectors", "owner_history");
            let owner = heddleco_capability_verifier::verify_owner_root(
                history.root.as_ref().expect("owner root"),
            )
            .expect("independent owner");
            read.verify_witnesses(
                &authority::ImportWitnessRootPin {
                    authority: f["context"]["authority"]
                        .as_str()
                        .expect("authority")
                        .into(),
                    root_id: f["context"]["root_id"].as_str().expect("root id").into(),
                    public_key: root,
                    epoch: 1,
                },
                None,
                1_350_000,
                |index, _| owners.get(index).copied().ok_or(Reject::Root),
                |bundle, _| {
                    for policy in &bundle.policies {
                        heddleco_capability_verifier::import_delegation::verify_policy_record(
                            policy,
                            &[&owner],
                        )
                        .map_err(|_| Reject::Signature)?;
                    }
                    Ok(())
                },
            )
            .expect("verified retained history");
        });
        read
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
            "/../thread-api/tests/fixtures/hybrid-alpha32.json"
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
            renewal_read: (response == "renewal_preparation")
                .then(|| verified_read("job_state_partial")),
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
    fn resolve_preserves_exact_selected_identity_and_allows_changed_observations() {
        for row in fixture()["custody_vectors"]["resolve"]
            .as_array()
            .expect("vectors")
        {
            let request = wire("wire_vectors", row["request"].as_str().expect("request"));
            let response = wire("wire_vectors", row["response"].as_str().expect("response"));
            let result = resolved_source(&request, &response);
            assert_eq!(result.is_ok(), row["expected"] == "OK", "{row}: {result:?}");
        }
    }

    #[test]
    fn recovered_renewal_uses_retained_custody_even_with_replacement_grants() {
        let read = prepared("renewal_preparation")
            .renewal_read
            .expect("authenticated read");
        for row in fixture()["custody_vectors"]["read"]
            .as_array()
            .expect("read vectors")
        {
            let response = wire("wire_vectors", row["response"].as_str().expect("response"));
            let result = authority::validate_job_state_response(&read.request, &response);
            assert_eq!(result.is_ok(), row["expected"] == "OK", "{row}: {result:?}");
        }
        with_context(1350, |expected| {
            let predecessor = read.predecessor(expected).expect("predecessor");
            for row in fixture()["custody_vectors"]["prepare"]
                .as_array()
                .expect("prepare vectors")
            {
                // Revocation of the current grant is checked by the host.
                if row["revoked"] == true {
                    continue;
                }
                let request = wire("wire_vectors", row["request"].as_str().expect("request"));
                let current = source(row["source"].as_str().expect("source"));
                let result = read.validate_preparation(&request);
                assert_eq!(
                    result.is_ok(),
                    row["expected"] == "OK",
                    "pre-RPC {row}: {result:?}"
                );
                let result = authority::prepare_import_source_scope(
                    &request,
                    &current.source,
                    Some("github"),
                    &configuration().response,
                    &wire::<wire::ImportPermissionScopeV1>("wire_vectors", "scope")
                        .destination_version,
                    Some(&read.original_admission(1350).expect("original admission")),
                    Some((
                        &predecessor,
                        read.response.state.as_ref().expect("state"),
                        read.retained_source().expect("custody"),
                    )),
                );
                assert_eq!(result.is_ok(), row["expected"] == "OK", "{row}: {result:?}");
            }
        });
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
            authority::prepare_import_source_scope(&request, &discovered.source, Some("github"), &configuration().response, &wire::<wire::ImportPermissionScopeV1>("wire_vectors", "scope").destination_version, Some(&read.original_admission(1350).expect("original admission")), Some((&predecessor, read.response.state.as_ref().expect("state"), read.response.retained_source.as_ref().expect("retained source")))).expect("the exact signed predecessor and retained CAS still authorize only the original pin");
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
                let retained = row["retained"].as_bool().expect("retained").then_some((
                    &predecessor,
                    changed_state.as_ref().unwrap_or(state),
                    read.response
                        .retained_source
                        .as_ref()
                        .expect("retained source"),
                ));
                let result = authority::prepare_import_source_scope(
                    &request,
                    &current_source.source,
                    Some("github"),
                    &configuration,
                    &wire::<wire::ImportPermissionScopeV1>("wire_vectors", "scope")
                        .destination_version,
                    Some(&read.original_admission(1350).expect("original admission")),
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
        let mut request: wire::RenewImportJobRequest =
            wire("wire_vectors", "renew_request_partial");
        request.proof.as_mut().expect("proof").history_proofs = prepared
            .renewal_read
            .as_ref()
            .expect("read")
            .response
            .retained_proof
            .as_ref()
            .expect("retained")
            .history_proofs
            .clone();
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
        let mut request: wire::RenewImportJobRequest =
            wire("wire_vectors", "renew_request_partial");
        request.proof.as_mut().expect("proof").history_proofs = prepared
            .renewal_read
            .as_ref()
            .expect("read")
            .response
            .retained_proof
            .as_ref()
            .expect("retained")
            .history_proofs
            .clone();
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
    #[test]
    fn alpha32_sibling_preparations_survive_independent_commits() {
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
                renewal_read: None,
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
                    validate_commit(prepared, request, "github", expected)
                        .expect("each disjoint signed job can activate in either order");
                }
            }
            assert!(validate_commit(&a.0, &b.1, "github", expected).is_err());
            assert!(validate_commit(&b.0, &a.1, "github", expected).is_err());
        });
        assert_eq!(a.0.response, retained_a);
        assert_eq!(b.0.response, retained_b);
    }

    #[test]
    fn alpha32_discovery_plans_disjoint_jobs_with_one_positive_total_each() {
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
                .all(|s| s.max_operations == 256 && s.max_result_bytes == 300 * 1024 * 1024)
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
    fn alpha32_control_reads_select_retry_and_report_custody_and_original_window() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/import-consumer-alpha32.json"
        ))
        .expect("controls");
        for row in corpus["availability"].as_array().expect("availability") {
            let response: wire::GetImportJobStateResponse =
                wire("wire_vectors", row["read"].as_str().expect("read"));
            let mut request: wire::GetImportJobStateRequest =
                wire("wire_vectors", "job_state_request");
            request.logical_job_id = response
                .state
                .as_ref()
                .expect("state")
                .logical_job_id
                .clone();
            authority::validate_control_state_response(&request, &response)
                .expect("current disclosures");
            let read = ImportJobState {
                request,
                response,
                verified_history: None,
            };
            let retry = read.retry_request("fresh-retry-request".into());
            if row["expected"][0] == "AVAILABLE" {
                let retry = retry.expect("eligible co-writer retry");
                let Some(
                    wire::get_import_job_state_response::RetryAvailability::EligibleRetryTarget(
                        target,
                    ),
                ) = &read.response.retry_availability
                else {
                    panic!("target");
                };
                assert_eq!(
                    retry.original_operation.as_ref().expect("operation").id,
                    target.operation_ref.as_ref().expect("target").id
                );
                assert_eq!(retry.expected_operation_version, target.operation_version);
                assert_eq!(
                    retry.expected_authority_epoch,
                    read.response.state.as_ref().expect("state").authority_epoch
                );
            } else {
                assert!(
                    matches!(
                        retry,
                        Err(super::super::super::HostedError::ImportControlUnavailable(
                            _
                        ))
                    ),
                    "{row}"
                );
            }
            assert_eq!(
                read.logical_job_terminal().expect("terminal fact"),
                row["id"] == "cancelled"
            );
        }
        for row in corpus["state_negatives"].as_array().expect("negatives") {
            let response = wire("wire_vectors", row["read"].as_str().expect("read"));
            assert!(
                authority::validate_control_state_response(
                    &wire("wire_vectors", "job_state_request"),
                    &response
                )
                .is_err()
            );
            authority::validate_control_state_response(
                &wire("wire_vectors", "job_state_request"),
                &wire("wire_vectors", row["control"].as_str().expect("control")),
            )
            .expect("unchanged control");
        }
    }

    #[test]
    fn alpha31_renew_is_authority_only_and_retry_has_an_independent_fresh_uuid() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/import-job-control-alpha31.json"
        ))
        .expect("job controls");
        let prior = corpus["prior_attempt_ids"]
            .as_array()
            .expect("attempts")
            .iter()
            .map(|v| v.as_str().expect("uuid").to_owned())
            .collect::<Vec<_>>();
        let renewal = wire("wire_vectors", "renew_request_partial");
        authority::validate_renew_response(
            &renewal,
            &wire("wire_vectors", "control_renew_applied"),
        )
        .expect("Applied without scheduling");
        let retry: wire::RetryImportSourceRequest = wire("wire_vectors", "control_retry_connected");
        let read = ImportJobState {
            request: wire("wire_vectors", "job_state_request"),
            response: wire("wire_vectors", "control_connected_state"),
            verified_history: None,
        };
        let observed = wire::RecordRef {
            spool: retry
                .original_operation
                .as_ref()
                .expect("target")
                .spool
                .clone(),
            id: prior[1].clone(),
        };
        authority::validate_retry_response(
            &retry,
            &wire("wire_vectors", "control_retry_pending"),
            &prior,
        )
        .expect("independent UUID");
        read.validate_retry_response(&retry, &wire("wire_vectors", "control_retry_pending"), None)
            .expect("fresh UUID without ordinary operation visibility");
        let mut reused: wire::MutationResponse = wire("wire_vectors", "control_retry_pending");
        let Some(wire::mutation_receipt::Outcome::PendingOperation(operation)) =
            &mut reused.receipt.as_mut().expect("receipt").outcome
        else {
            panic!("pending");
        };
        operation.id = authority::initial_operation_id(
            &read
                .response
                .retained_proof
                .as_ref()
                .expect("proof")
                .operations[0]
                .body
                .as_ref()
                .expect("operation")
                .physical_operation_id,
            false,
        )
        .expect("retained publication UUID");
        authority::validate_retry_response(&retry, &reused, &prior)
            .expect("first and observed IDs alone do not detect this reuse");
        assert!(
            read.validate_retry_response(&retry, &reused, None).is_err(),
            "retry must not reuse a retained publication attempt"
        );
        for row in corpus["receipt_negatives"].as_array().expect("negatives") {
            let response = wire("wire_vectors", row["response"].as_str().expect("response"));
            if row["kind"] == "Renew" {
                assert!(
                    authority::validate_renew_response(
                        &wire("wire_vectors", row["request"].as_str().expect("request")),
                        &response
                    )
                    .is_err()
                );
                authority::validate_renew_response(
                    &renewal,
                    &wire("wire_vectors", "control_renew_applied"),
                )
                .expect("Renew control");
            } else {
                assert!(
                    read.validate_retry_response(
                        &wire("wire_vectors", row["request"].as_str().expect("request")),
                        &response,
                        Some(&observed)
                    )
                    .is_err()
                );
                read.validate_retry_response(
                    &retry,
                    &wire("wire_vectors", "control_retry_pending"),
                    Some(&observed),
                )
                .expect("Retry control");
            }
        }
    }

    #[test]
    fn alpha32_scheduled_recovery_preserves_only_authenticated_snapshot_history() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/import-consumer-alpha32.json"
        ))
        .expect("consumer vectors");
        let f = fixture();
        let root =
            hex::decode(f["keys"]["root"]["public_key_hex"].as_str().expect("root")).expect("key");
        let pin = authority::ImportWitnessRootPin {
            authority: f["context"]["authority"]
                .as_str()
                .expect("authority")
                .into(),
            root_id: f["context"]["root_id"].as_str().expect("root id").into(),
            public_key: root,
            epoch: 1,
        };
        let verify = |bundle: wire::ImportPublicProofBundleV1,
                      snapshot: Option<&authority::ImportWitnessSnapshot>,
                      now: i64,
                      pin: &authority::ImportWitnessRootPin| {
            let active = bundle.delegations.last().expect("active").clone();
            let body = active.body.as_ref().expect("body");
            let state = wire::ImportJobCasStateV1 {
                format_version: 1,
                logical_job_id: body.logical_job_id.clone(),
                retry_lineage_id: body.retry_lineage_id.clone(),
                active_predecessor: Some(active),
                authority_epoch: bundle.delegations.len() as u64,
                committed_manifest: bundle.terminal_manifest.clone(),
            };
            let mut read = ImportJobState {
                request: wire("wire_vectors", "job_state_request"),
                response: wire::GetImportJobStateResponse {
                    state: Some(state),
                    retained_proof: Some(bundle),
                    ..Default::default()
                },
                verified_history: None,
            };
            with_context(1350, |expected| {
                let owners = (0..read
                    .response
                    .retained_proof
                    .as_ref()
                    .expect("proof")
                    .delegations
                    .len())
                    .map(|_| authority::ImportBundleOwnerExpectation {
                        identity: expected.identity,
                        owner_public_key: expected.owner_public_key,
                        owner_chain_digest: expected.owner_chain_digest,
                        authority_expires_at_seconds: expected.authority_expires_at_seconds,
                        effective_from_unix_seconds: 1000,
                        effective_until_unix_seconds: None,
                        forbidden_job_keys: expected.forbidden_job_keys,
                        known_job_associations: &[],
                    })
                    .collect::<Vec<_>>();
                let history: wire::OwnerHistory = wire("wire_vectors", "owner_history");
                let owner = heddleco_capability_verifier::verify_owner_root(
                    history.root.as_ref().expect("root"),
                )
                .expect("independent owner");
                read.verify_witnesses(
                    pin,
                    snapshot,
                    now,
                    |index, _| owners.get(index).copied().ok_or(Reject::Root),
                    |b, _| {
                        for p in &b.policies {
                            heddleco_capability_verifier::import_delegation::verify_policy_record(
                                p,
                                &[&owner],
                            )
                            .map_err(|_| Reject::Signature)?;
                        }
                        Ok(())
                    },
                )
                .expect("verified recovery")
                .clone()
            })
        };
        for row in corpus["recovery"].as_array().expect("recovery cases") {
            let mut initial: wire::ImportPublicProofBundleV1 =
                wire("wire_vectors", "review_scheduled_recovery");
            let initial_now = if let Some(name) = row["initial_set"].as_str() {
                initial.witness_set = Some(wire("signed_vectors", name));
                1_350_000
            } else {
                1_100_000
            };
            let input = (row["input"] == true).then(|| verify(initial, None, initial_now, &pin));
            let snapshot = input.as_ref().and_then(|r| r.snapshot.as_ref());
            let before = snapshot.cloned();
            let mut bundle: wire::ImportPublicProofBundleV1 =
                wire("wire_vectors", row["bundle"].as_str().expect("bundle"));
            if row["remove_set"] == true {
                bundle.witness_set = None;
            }
            assert!(
                bundle.genesis_witnesses.is_empty(),
                "scheduled Commit has no native admission"
            );
            let mut selected = pin.clone();
            let now = if row["replacement"] == true {
                selected.epoch = 2;
                selected.root_id = "descriptor-root-2".into();
                selected.public_key = hex::decode(
                    f["keys"]["wrong_root"]["public_key_hex"]
                        .as_str()
                        .expect("root"),
                )
                .expect("key");
                bundle.witness_set = Some(wire("signed_vectors", "alpha31_replacement_set"));
                1_350_000
            } else {
                1_150_000
            };
            let result = verify(bundle, snapshot, now, &selected);
            assert_eq!(result.evidence, authority::ImportBundleEvidence::Recovery);
            assert_eq!(result.owner_check_times_unix_seconds, vec![None]);
            assert_eq!(result.snapshot_advanced, row["expected_advanced"] == true);
            assert_eq!(result.snapshot.is_some(), row["expected_snapshot"] == true);
            assert_eq!(snapshot, before.as_ref());
            if !result.snapshot_advanced {
                assert_eq!(result.snapshot, before);
            }
            if let Some(snapshot) = result.snapshot {
                assert!(snapshot.accepted_history.is_empty());
                assert!(snapshot.job_associations.is_empty());
            }
        }
    }
}
