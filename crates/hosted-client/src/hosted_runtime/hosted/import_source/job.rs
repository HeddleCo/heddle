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
}

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
        refs: &super::ImportSourceRefs,
        request: &wire::PrepareImportJobRequest,
    ) -> Result<PreparedImportJob> {
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
        refs.validate_scope(proposed)?;
        // Empty explicitly asks Prepare for the current opaque CAS token.
        if !proposed.destination_version.is_empty() {
            authority::prepare_scope(
                proposed,
                &configuration.response,
                &proposed.destination_version,
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
        authority::prepare_scope(
            proposed,
            &configuration.response,
            &returned.destination_version,
        )?;
        Ok(PreparedImportJob {
            destination: destination.clone(),
            response,
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
        validate_commit(prepared, request, resolved_provider, expected)?;
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
        prepared: &PreparedImportJob,
        request: &wire::RenewImportJobRequest,
        predecessor_context: &authority::ImportOwnerExpectation<'_>,
        current_context: &authority::ImportOwnerExpectation<'_>,
    ) -> Result<wire::MutationResponse> {
        validate_renewal(prepared, request, predecessor_context, current_context)?;
        self.call_unary(
            "/heddle.api.v1alpha2.IntegrationService/RenewImportJob",
            request,
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
    authority::validate_commit_request(request, resolved_provider)?;
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
    authority::validate_renewal_preparation(&prepared.response)?;
    let proof = request.proof.as_ref().ok_or(Reject::Canonical)?;
    prepared.preflight(proof, current_context)?;
    let replacement = proof.delegations.last().ok_or(Reject::ImportPermission)?;
    let state = prepared
        .response
        .renewal_state
        .as_ref()
        .ok_or(Reject::Canonical)?;
    let active = state.active_predecessor.as_ref().ok_or(Reject::Canonical)?;
    let predecessor = authority::verify_renewal_predecessor(
        state,
        member_permission(proof, active)?,
        predecessor_context,
    )?;
    let renewal = request.renewal.as_ref().ok_or(Reject::Canonical)?;
    if renewal.body.as_ref().and_then(|b| b.replacement.as_ref()) != Some(replacement) {
        return Err(Reject::StaleContext.into());
    }
    authority::verify_renewal_from_state(
        renewal,
        &predecessor,
        state,
        member_permission(proof, replacement)?,
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
    Ok(signed)
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
            "/../thread-api/tests/fixtures/hybrid-alpha23.json"
        )))
        .expect("alpha.23 fixed vectors");
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
            "/../thread-api/tests/fixtures/hybrid-alpha23.json"
        )))
        .expect("published alpha.23 vectors")
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
        }
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
        let request = wire::RenewImportJobRequest {
            destination: Some(prepared.destination.clone()),
            renewal: Some(wire("signed_vectors", "renewal")),
            proof: Some(wire("wire_vectors", "complete_renewed_export")),
            client_operation_id: "renew-once".into(),
        };
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
}
