// SPDX-License-Identifier: Apache-2.0
//! Submit a server-side public Git import and observe its durable operation.

use api::heddle::api::v1alpha2 as contract;
use crypto::Signer as _;
use objects::object::thread_replication::{
    GenesisOwner, ThreadGenesis, git_import_graph::MAX_IMPORT_REFS, hosted_import,
};
use thread_api::{creation::ThreadCreation, rpc};
use uuid::Uuid;
use wire::ProtocolError;

use super::{HostedClient, operation_id::ClientOperationId};

mod job;
pub use job::PreparedImportJob;

const IMPORT_SOURCE: &str = "/heddle.api.v1alpha2.IntegrationService/ImportSource";
const RETRY_IMPORT_SOURCE: &str = "/heddle.api.v1alpha2.IntegrationService/RetryImportSource";

/// Completeness at the client transport boundary. This grants no authority;
/// the host still verifies independently selected owner permission and the
/// current job/CAS fence. An old unsigned request never reaches a capable peer.
pub(super) fn require_request_authority(method: &str, encoded: &[u8]) -> super::Result<()> {
    use api::hybrid_codec::Reject;
    use prost::Message;
    match method.trim_start_matches('/') {
        "heddle.api.v1alpha2.IntegrationService/ImportSource" => {
            let request = contract::ImportSourceRequest::decode(encoded)?;
            let signed = request_delegation(request.import_authority.as_ref())?;
            let scope = signed
                .body
                .as_ref()
                .and_then(|body| body.scope.as_ref())
                .ok_or(Reject::Scope)?;
            if request
                .source
                .as_ref()
                .is_none_or(|source| source.clone_url != scope.source_url)
                || request.expected_destination_version != scope.destination_version
            {
                return Err(Reject::Scope.into());
            }
        }
        "heddle.api.v1alpha2.IntegrationService/SynchronizeRemote" => {
            let request = contract::SynchronizeRemoteRequest::decode(encoded)?;
            // Recurring sync needs its own explicit caller-signed proposal;
            // the host checks Own authority and that proposal's exact scope.
            request_delegation(request.import_authority.as_ref())?;
        }
        "heddle.api.v1alpha2.IntegrationService/RetryImportSource" => {
            let request = contract::RetryImportSourceRequest::decode(encoded)?;
            if request.logical_job_id.len() != 16
                || request.active_delegation_digest.len() != 32
                || request.expected_authority_epoch == 0
            {
                return Err(Reject::ImportPermission.into());
            }
        }
        _ => {}
    }
    Ok(())
}

fn request_delegation(
    proof: Option<&contract::ImportPublicProofBundleV1>,
) -> super::Result<&contract::SignedImportJobDelegationV1> {
    use api::hybrid_codec::Reject;
    use prost::Message;
    let proof = proof.ok_or(Reject::ImportPermission)?;
    if proof.format_version != 1 || proof.encoded_len() > api::import_authority::MAX_BUNDLE_BYTES {
        return Err(Reject::Bounds.into());
    }
    let signed = proof.delegations.last().ok_or(Reject::ImportPermission)?;
    job::verify_delegating_signature(signed)?;
    Ok(signed)
}

/// Source admission failures carry exact branch and tag counts.
#[derive(Debug, thiserror::Error)]
pub enum ImportSourceRefError {
    #[error("source has {branches} branches and {tags} tags ({total} refs); maximum is 512")]
    TooManyRefs {
        branches: usize,
        tags: usize,
        total: usize,
    },
    #[error("source has no refs/heads/* branches")]
    NoBranches,
    #[error("source ref {ref_name:?} is not a valid Thread name: {reason}")]
    InvalidBranch { ref_name: String, reason: String },
    /// Discovery stopped reading an advertisement too large to admit.
    #[error(transparent)]
    AdvertisementOverBudget(heddle_git_projection::source_ref_budget::RefAdvertisementOverBudget),
    #[error("discover source refs: {0}")]
    Discovery(String),
}

/// Validated source refs, admitted before any destination request or signature.
#[derive(Clone, Debug)]
pub struct ImportSourceRefs {
    branches: Vec<String>,
}

impl ImportSourceRefs {
    fn from_names(names: impl IntoIterator<Item = String>) -> Result<Self, ImportSourceRefError> {
        let names = names.into_iter().collect::<std::collections::BTreeSet<_>>();
        let branches = names
            .iter()
            .filter(|name| name.starts_with("refs/heads/"))
            .cloned()
            .collect::<Vec<_>>();
        let tags = names
            .iter()
            .filter(|name| name.starts_with("refs/tags/"))
            .count();
        let total = branches.len() + tags;
        if total > MAX_IMPORT_REFS {
            return Err(ImportSourceRefError::TooManyRefs {
                branches: branches.len(),
                tags,
                total,
            });
        }
        if branches.is_empty() {
            return Err(ImportSourceRefError::NoBranches);
        }
        for ref_name in &branches {
            verbs::validate_thread_name(&ref_name["refs/heads/".len()..]).map_err(|error| {
                ImportSourceRefError::InvalidBranch {
                    ref_name: ref_name.clone(),
                    reason: error.to_string(),
                }
            })?;
        }
        Ok(Self { branches })
    }

    /// Enumerate branches and tags without peeled-tag duplicates or HEAD.
    pub async fn discover(clone_url: &str) -> Result<Self, ImportSourceRefError> {
        let clone_url = clone_url.to_string();
        let names = tokio::task::spawn_blocking(move || {
            heddle_git_projection::discover_git_source_refs(&clone_url)
        })
        .await
        .map_err(|error| ImportSourceRefError::Discovery(error.to_string()))?
        .map_err(|error| match error {
            heddle_git_projection::GitProjectionError::SourceAdvertisementOverBudget(over) => {
                ImportSourceRefError::AdvertisementOverBudget(over)
            }
            error => ImportSourceRefError::Discovery(error.to_string()),
        })?;
        Self::from_names(names)
    }
}

/// Stable identities minted for one accepted hosted source import.
#[derive(Clone, Debug)]
pub struct ImportSourceStart {
    pub client_operation_id: String,
    pub destination: contract::SpoolRef,
    pub threads: Vec<contract::ThreadRef>,
    pub operation: contract::RecordRef,
}

/// Durable operation created by an explicit retry.
#[derive(Clone, Debug)]
pub struct ImportOperationStart {
    pub client_operation_id: String,
    pub destination: contract::SpoolRef,
    pub operation: contract::RecordRef,
}

impl HostedClient {
    /// Ask weft to fetch a public Git repository and import it asynchronously.
    ///
    /// The destination Spool already exists. The request carries the canonical
    /// empty State so the same admission can create one native Thread per branch.
    pub async fn import_source(
        &mut self,
        destination_path: &str,
        clone_url: &str,
        refs: &ImportSourceRefs,
        caller_operation_id: impl Into<String>,
    ) -> Result<ImportSourceStart, ProtocolError> {
        self.require_import_authority_protocol()
            .await
            .map_err(super::helpers::hosted_to_protocol_error)?;
        let operation_id = ClientOperationId::caller_or_fresh(IMPORT_SOURCE, caller_operation_id);
        let overview = self.native_spool_overview(destination_path).await?;
        let destination = overview.r#ref.clone().ok_or_else(|| {
            ProtocolError::InvalidState("hosted import destination identity is absent".into())
        })?;
        let (owner, creator_authority, _) = self.current_creator_authority().await?;
        let initial_base = hosted_import::synthetic_initial_base()
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;

        let mut threads = Vec::with_capacity(refs.branches.len());
        let mut branches = Vec::with_capacity(refs.branches.len());
        for ref_name in &refs.branches {
            let thread_name = &ref_name["refs/heads/".len()..];
            let creation = {
                let signer = self.claim_proof_signer().ok_or_else(|| {
                    ProtocolError::AuthenticationFailed(
                        "hosted source import requires a proof-bound credential".into(),
                    )
                })?;
                let creator = signer.public_key().try_into().map_err(|_| {
                    ProtocolError::InvalidState("invalid hosted creator signing key".into())
                })?;
                let genesis = ThreadGenesis {
                    version: 1,
                    spool: destination.id.clone(),
                    parent: None,
                    base: initial_base.id(),
                    name: thread_name.to_string(),
                    intent: String::new(),
                    creator,
                    owner: GenesisOwner::Account(owner),
                    nonce: Vec::new(),
                };
                ThreadCreation::sign_with_authority(
                    operation_id.to_wire(),
                    &genesis,
                    signer,
                    creator_authority.clone(),
                )
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?
            };
            threads.push(creation.reference().clone());
            let creation_request = creation.request();
            branches.push(contract::ImportBranchGenesis {
                ref_name: ref_name.clone(),
                thread_genesis: creation_request.thread_genesis.clone(),
                creator_authority: creation_request.creator_authority.clone(),
                // No HYBRID import authority is claimed; none is signed.
                genesis_authority: None,
            });
        }
        let request = contract::ImportSourceRequest {
            client_operation_id: operation_id.to_wire(),
            destination: Some(destination.clone()),
            source: Some(contract::ProviderRepository {
                connection: None,
                provider_repository_id: clone_url.to_string(),
                clone_url: clone_url.to_string(),
                name: String::new(),
                private: false,
                installation_id: String::new(),
                // Server-side read projection; an import request names none.
                linked_spools: Vec::new(),
                // Provider read projections; an import request names none.
                default_branch: String::new(),
                refs: Vec::new(),
                refs_status: None,
            }),
            expected_destination_version: overview.version,
            branches,
            initial_base_state: initial_base
                .encode_current_msgpack()
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?,
            // No HYBRID import authority is claimed; no proof bundle.
            import_authority: None,
        };
        let remote = self.native().await.map_err(protocol_error)?;
        let response: contract::MutationResponse =
            self.call_unary(IMPORT_SOURCE, &request)
                .await
                .map_err(super::helpers::hosted_to_protocol_error)?;
        let pending_operation = require_pending_receipt(
            response.receipt,
            operation_id.as_str(),
            &remote.description.endpoint,
            &destination,
            "hosted source import",
        )?;
        Ok(ImportSourceStart {
            client_operation_id: operation_id.to_wire(),
            destination,
            threads,
            operation: pending_operation,
        })
    }

    /// Follow the exact durable operation created by [`Self::import_source`].
    /// Every committed operation version is delivered to `on_progress`.
    pub async fn observe_import_source(
        &self,
        import: &ImportSourceStart,
        mut on_progress: impl FnMut(&contract::OperationRecord) -> Result<(), ProtocolError>,
    ) -> Result<contract::OperationRecord, ProtocolError> {
        let remote = self.native().await.map_err(protocol_error)?;
        let mut observation = remote
            .observe::<rpc::OperationServiceObserveOperations>(
                contract::ObserveOperationsRequest {
                    spools: vec![import.destination.clone()],
                    operations: vec![import.operation.clone()],
                    client_operation_ids: vec![import.client_operation_id.clone()],
                    observe: Some(contract::ObserveOptions {
                        mode: contract::ObservationMode::Follow as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(protocol_error)?;
        while let Some(batch) = observation.next_commit().await.map_err(protocol_error)? {
            for change in batch.changes {
                let contract::operation_event::Payload::Operation(record) = change else {
                    continue;
                };
                if record.client_operation_id != import.client_operation_id
                    || record.r#ref.as_ref() != Some(&import.operation)
                {
                    return Err(ProtocolError::InvalidState(
                        "operation stream returned another hosted source import".into(),
                    ));
                }
                on_progress(&record)?;
                if matches!(
                    contract::operation_record::State::try_from(record.state),
                    Ok(contract::operation_record::State::Completed
                        | contract::operation_record::State::Failed
                        | contract::operation_record::State::Canceled)
                ) {
                    observation.cancel();
                    return Ok(record);
                }
            }
        }
        Err(ProtocolError::Remote(
            "hosted source import observation ended before a terminal state".into(),
        ))
    }

    /// Observe a durable import operation by its record ID.
    pub async fn observe_import_operation(
        &self,
        destination_path: &str,
        operation_id: &str,
        follow: bool,
        mut on_progress: impl FnMut(&contract::OperationRecord) -> Result<(), ProtocolError>,
    ) -> Result<contract::OperationRecord, ProtocolError> {
        let overview = self.native_spool_overview(destination_path).await?;
        let destination = overview.r#ref.ok_or_else(|| {
            ProtocolError::InvalidState("hosted import destination identity is absent".into())
        })?;
        let operation = contract::RecordRef {
            spool: Some(destination.clone()),
            id: operation_id.to_string(),
        };
        self.observe_operation(&destination, &operation, None, follow, &mut on_progress)
            .await
    }

    /// Submit the API's explicit retry against the exact observed version.
    pub async fn retry_import_source(
        &self,
        original: &contract::OperationRecord,
        caller_operation_id: impl Into<String>,
    ) -> Result<ImportOperationStart, ProtocolError> {
        let operation_id =
            ClientOperationId::caller_or_fresh(RETRY_IMPORT_SOURCE, caller_operation_id);
        let request = retry_import_source_request(original, operation_id.to_wire())?;
        let original_ref = request.original_operation.as_ref().ok_or_else(|| {
            ProtocolError::InvalidState("original import operation reference is absent".into())
        })?;
        let destination = original_ref.spool.clone().ok_or_else(|| {
            ProtocolError::InvalidState("original import destination identity is absent".into())
        })?;
        let remote = self.native().await.map_err(protocol_error)?;
        let response: contract::MutationResponse = self
            .call_unary(RETRY_IMPORT_SOURCE, &request)
            .await
            .map_err(super::helpers::hosted_to_protocol_error)?;
        let pending_operation = require_pending_receipt(
            response.receipt,
            operation_id.as_str(),
            &remote.description.endpoint,
            &destination,
            "hosted source import retry",
        )?;
        Ok(ImportOperationStart {
            client_operation_id: operation_id.to_wire(),
            destination,
            operation: pending_operation,
        })
    }

    async fn observe_operation(
        &self,
        destination: &contract::SpoolRef,
        operation: &contract::RecordRef,
        client_operation_id: Option<&str>,
        follow: bool,
        on_progress: &mut impl FnMut(&contract::OperationRecord) -> Result<(), ProtocolError>,
    ) -> Result<contract::OperationRecord, ProtocolError> {
        let remote = self.native().await.map_err(protocol_error)?;
        let mut observation = remote
            .observe::<rpc::OperationServiceObserveOperations>(
                contract::ObserveOperationsRequest {
                    spools: vec![destination.clone()],
                    operations: vec![operation.clone()],
                    client_operation_ids: client_operation_id
                        .map(|id| vec![id.to_string()])
                        .unwrap_or_default(),
                    observe: Some(contract::ObserveOptions {
                        mode: if follow {
                            contract::ObservationMode::Follow as i32
                        } else {
                            contract::ObservationMode::Once as i32
                        },
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(protocol_error)?;
        let mut latest = None;
        while let Some(batch) = observation.next_commit().await.map_err(protocol_error)? {
            for change in batch.changes {
                let contract::operation_event::Payload::Operation(record) = change else {
                    continue;
                };
                if record.r#ref.as_ref() != Some(operation)
                    || client_operation_id
                        .is_some_and(|id| record.client_operation_id.as_str() != id)
                {
                    return Err(ProtocolError::InvalidState(
                        "operation stream returned another hosted source import".into(),
                    ));
                }
                on_progress(&record)?;
                let terminal = is_terminal_state(record.state);
                latest = Some(record);
                if follow && terminal {
                    observation.cancel();
                    return latest.ok_or_else(|| {
                        ProtocolError::InvalidState("import operation observation was empty".into())
                    });
                }
            }
        }
        latest.ok_or_else(|| {
            ProtocolError::ObjectNotFound("hosted import operation was not found".into())
        })
    }
}

fn retry_import_source_request(
    original: &contract::OperationRecord,
    client_operation_id: String,
) -> Result<contract::RetryImportSourceRequest, ProtocolError> {
    let original_operation = original.r#ref.clone().ok_or_else(|| {
        ProtocolError::InvalidState("original import operation reference is absent".into())
    })?;
    if original.version.is_empty() {
        return Err(ProtocolError::InvalidState(
            "original import operation version is absent".into(),
        ));
    }
    Ok(contract::RetryImportSourceRequest {
        client_operation_id,
        original_operation: Some(original_operation),
        expected_operation_version: original.version.clone(),
        // No HYBRID import authority is claimed; no job binding.
        logical_job_id: Vec::new(),
        active_delegation_digest: Vec::new(),
        expected_authority_epoch: 0,
    })
}

fn is_terminal_state(state: i32) -> bool {
    matches!(
        contract::operation_record::State::try_from(state),
        Ok(contract::operation_record::State::Completed
            | contract::operation_record::State::Failed
            | contract::operation_record::State::Canceled)
    )
}

fn require_pending_receipt(
    receipt: Option<contract::MutationReceipt>,
    operation_id: &str,
    endpoint: &Option<contract::EndpointRef>,
    destination: &contract::SpoolRef,
    action: &str,
) -> Result<contract::RecordRef, ProtocolError> {
    let receipt =
        receipt.ok_or_else(|| ProtocolError::InvalidState(format!("{action} receipt absent")))?;
    let Some(contract::mutation_receipt::Outcome::PendingOperation(operation)) = receipt.outcome
    else {
        return Err(ProtocolError::InvalidState(format!(
            "{action} did not return a pending operation"
        )));
    };
    if receipt.client_operation_id != operation_id
        || &receipt.endpoint != endpoint
        || operation.spool.as_ref() != Some(destination)
        || operation.id != operation_id
        || Uuid::parse_str(&operation.id).is_err()
    {
        return Err(ProtocolError::InvalidState(format!(
            "{action} returned an inconsistent pending operation"
        )));
    }
    Ok(operation)
}

fn protocol_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::Remote(error.to_string())
}

#[cfg(test)]
mod tests {
    use api::v2::client::Rpc as _;
    use prost::Message;

    use super::*;

    fn signed_import_request() -> contract::ImportSourceRequest {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../thread-api/tests/fixtures/hybrid-alpha18.json"
        )))
        .expect("alpha.18 fixed vectors");
        let bytes = hex::decode(
            fixture["wire_vectors"]["complete_renewed_export"]["wire_hex"]
                .as_str()
                .expect("wire"),
        )
        .expect("hex");
        let proof = contract::ImportPublicProofBundleV1::decode(bytes.as_slice()).expect("bundle");
        let scope = proof
            .delegations
            .last()
            .expect("delegation")
            .body
            .as_ref()
            .expect("body")
            .scope
            .as_ref()
            .expect("scope");
        contract::ImportSourceRequest {
            source: Some(contract::ProviderRepository {
                clone_url: scope.source_url.clone(),
                ..Default::default()
            }),
            expected_destination_version: scope.destination_version.clone(),
            import_authority: Some(proof),
            ..Default::default()
        }
    }

    #[test]
    fn import_transport_requires_the_original_signature_and_exact_request_scope() {
        let control = signed_import_request();
        require_request_authority(IMPORT_SOURCE, &control.encode_to_vec()).expect("signed control");
        let mut unsigned = control.clone();
        unsigned.import_authority = None;
        assert!(require_request_authority(IMPORT_SOURCE, &unsigned.encode_to_vec()).is_err());
        let mut substituted = control.clone();
        substituted
            .import_authority
            .as_mut()
            .expect("proof")
            .delegations
            .last_mut()
            .expect("delegation")
            .delegating_signature = None;
        assert!(require_request_authority(IMPORT_SOURCE, &substituted.encode_to_vec()).is_err());
        let mut changed = control.clone();
        changed
            .source
            .as_mut()
            .expect("source")
            .clone_url
            .push_str("/other");
        assert!(require_request_authority(IMPORT_SOURCE, &changed.encode_to_vec()).is_err());
        let mut changed = control.clone();
        changed.expected_destination_version[0] ^= 1;
        assert!(require_request_authority(IMPORT_SOURCE, &changed.encode_to_vec()).is_err());
        require_request_authority(IMPORT_SOURCE, &control.encode_to_vec())
            .expect("unchanged control");
    }

    #[test]
    fn retry_transport_requires_the_complete_observed_job_fence() {
        let control = contract::RetryImportSourceRequest {
            logical_job_id: vec![1; 16],
            active_delegation_digest: vec![2; 32],
            expected_authority_epoch: 1,
            ..Default::default()
        };
        require_request_authority(RETRY_IMPORT_SOURCE, &control.encode_to_vec())
            .expect("complete CAS control");
        for field in 0..3 {
            let mut missing = control.clone();
            match field {
                0 => missing.logical_job_id.clear(),
                1 => missing.active_delegation_digest.clear(),
                _ => missing.expected_authority_epoch = 0,
            }
            assert!(
                require_request_authority(RETRY_IMPORT_SOURCE, &missing.encode_to_vec()).is_err()
            );
        }
        require_request_authority(RETRY_IMPORT_SOURCE, &control.encode_to_vec())
            .expect("unchanged CAS control");
    }

    #[test]
    fn recurring_sync_transport_requires_explicit_original_authority() {
        let method = "/heddle.api.v1alpha2.IntegrationService/SynchronizeRemote";
        let control = contract::SynchronizeRemoteRequest {
            import_authority: signed_import_request().import_authority,
            ..Default::default()
        };
        // Completeness only: the host separately checks Own and recurring scope.
        require_request_authority(method, &control.encode_to_vec())
            .expect("explicit signed proposal");
        let unsigned = contract::SynchronizeRemoteRequest::default();
        assert!(require_request_authority(method, &unsigned.encode_to_vec()).is_err());
        let mut substituted = control.clone();
        substituted
            .import_authority
            .as_mut()
            .expect("proof")
            .delegations
            .last_mut()
            .expect("delegation")
            .delegating_signature = None;
        assert!(require_request_authority(method, &substituted.encode_to_vec()).is_err());
        require_request_authority(method, &control.encode_to_vec())
            .expect("original completeness control");
    }

    #[test]
    fn source_refs_reject_empty_and_invalid_branch_names_and_bound_all_tags() {
        assert!(matches!(
            ImportSourceRefs::from_names(["refs/tags/v1".into()]),
            Err(ImportSourceRefError::NoBranches)
        ));
        for ref_name in [
            "refs/heads/-flag",
            "refs/heads/heddle/frontier/main/hc-1",
            "refs/heads/bad name",
        ] {
            assert!(
                matches!(ImportSourceRefs::from_names([ref_name.into()]), Err(ImportSourceRefError::InvalidBranch { ref_name: rejected, .. }) if rejected == ref_name)
            );
        }
        let names = std::iter::once("refs/heads/main".into())
            .chain((0..512).map(|index| format!("refs/tags/{index}")));
        assert!(matches!(
            ImportSourceRefs::from_names(names),
            Err(ImportSourceRefError::TooManyRefs {
                branches: 1,
                tags: 512,
                total: 513
            })
        ));
        assert!(
            ImportSourceRefs::from_names(
                std::iter::once("refs/heads/main".into())
                    .chain((0..511).map(|index| format!("refs/tags/{index}")))
            )
            .is_ok()
        );
    }

    #[test]
    fn public_source_shape_uses_the_url_as_its_provider_identity() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let url = "https://github.com/octocat/Hello-World.git";
        let source = contract::ProviderRepository {
            connection: None,
            provider_repository_id: url.into(),
            clone_url: url.into(),
            name: "main".into(),
            private: false,
            installation_id: String::new(),
            linked_spools: Vec::new(),
            default_branch: String::new(),
            refs: Vec::new(),
            refs_status: None,
        };
        assert!(source.connection.is_none());
        assert_eq!(source.provider_repository_id, source.clone_url);
        assert!(!source.private);
        assert!(source.installation_id.is_empty());
    }

    #[test]
    fn canonical_initial_base_fits_the_import_bootstrap_bound() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let state = hosted_import::synthetic_initial_base().expect("stable initial base");
        let bytes = state.encode_current_msgpack().expect("canonical state");
        assert!(bytes.len() <= 4096);
    }

    #[test]
    fn import_source_uses_the_declared_v2_method() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        assert_eq!(
            rpc::IntegrationServiceImportSource::METHOD.path,
            IMPORT_SOURCE
        );
    }

    #[test]
    fn pending_receipt_is_bound_to_the_destination_and_operation() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let operation_id = Uuid::now_v7().to_string();
        let spool = contract::SpoolRef {
            id: Uuid::now_v7().to_string(),
        };
        let endpoint = Some(contract::EndpointRef {
            kind: contract::EndpointKind::Weft as i32,
            public_key: vec![3; 32],
        });
        let operation = contract::RecordRef {
            spool: Some(spool.clone()),
            id: operation_id.clone(),
        };
        let receipt = contract::MutationReceipt {
            client_operation_id: operation_id.clone(),
            endpoint: endpoint.clone(),
            outcome: Some(contract::mutation_receipt::Outcome::PendingOperation(
                operation.clone(),
            )),
            ..Default::default()
        };
        assert_eq!(
            require_pending_receipt(Some(receipt), &operation_id, &endpoint, &spool, "import")
                .expect("pending receipt"),
            operation
        );
    }

    #[test]
    fn retry_request_uses_the_observed_record_and_version() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let original = contract::OperationRecord {
            r#ref: Some(contract::RecordRef {
                spool: Some(contract::SpoolRef {
                    id: "spool-1".into(),
                }),
                id: "durable-operation".into(),
            }),
            version: vec![7; 32],
            state: contract::operation_record::State::Failed as i32,
            ..Default::default()
        };
        let request = retry_import_source_request(&original, "request-id".into())
            .expect("valid retry request");
        assert_eq!(request.client_operation_id, "request-id");
        assert_eq!(request.original_operation, original.r#ref);
        assert_eq!(request.expected_operation_version, vec![7; 32]);
    }

    #[tokio::test]
    async fn import_source_rejects_an_old_peer_before_sending_unsigned_authority() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let (mut client, server, captured) =
            crate::hosted_runtime::hosted::test_server::start_recording_import_source().await;
        let error = client
            .import_source(
                "acme",
                "https://github.com/octocat/Hello-World.git",
                &ImportSourceRefs::from_names(["refs/heads/main".into()]).expect("refs"),
                Uuid::now_v7().to_string(),
            )
            .await
            .expect_err("old peer cannot ignore new import authority");
        assert!(
            matches!(error, ProtocolError::Remote(ref message)
                if message == &super::super::HostedError::Hybrid(api::hybrid_codec::Reject::Protocol).to_string()),
            "{error}"
        );
        client.close().await;
        server.await.expect("hosted test server");
        assert!(captured.lock().expect("capture").requests.is_empty());
    }
}
