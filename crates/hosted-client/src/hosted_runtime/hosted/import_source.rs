// SPDX-License-Identifier: Apache-2.0
//! Submit a server-side public Git import and observe its durable operation.

use api::heddle::api::v1alpha2 as contract;
use crypto::Signer as _;
use objects::object::thread_replication::{GenesisOwner, ThreadGenesis, hosted_import};
use thread_api::{creation::ThreadCreation, rpc};
use uuid::Uuid;
use wire::ProtocolError;

use super::{HostedClient, operation_id::ClientOperationId};

const IMPORT_SOURCE: &str = "/heddle.api.v1alpha2.IntegrationService/ImportSource";

/// Stable identities minted for one accepted hosted source import.
#[derive(Clone, Debug)]
pub struct ImportSourceStart {
    pub client_operation_id: String,
    pub destination: contract::SpoolRef,
    pub thread: contract::ThreadRef,
    pub operation: contract::RecordRef,
}

impl HostedClient {
    /// Ask weft to fetch a public Git repository and import it asynchronously.
    ///
    /// The destination Spool already exists. The request carries the canonical
    /// empty State so the same admission can create its first native Thread.
    pub async fn import_source(
        &mut self,
        destination_path: &str,
        clone_url: &str,
        thread_name: &str,
        caller_operation_id: impl Into<String>,
    ) -> Result<ImportSourceStart, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(IMPORT_SOURCE, caller_operation_id);
        let overview = self.native_spool_overview(destination_path).await?;
        let destination = overview.r#ref.clone().ok_or_else(|| {
            ProtocolError::InvalidState("hosted import destination identity is absent".into())
        })?;
        let (owner, creator_authority, _) = self.current_creator_authority().await?;
        let initial_base = hosted_import::synthetic_initial_base()
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;

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
                creator_authority,
            )
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?
        };
        let thread = creation.reference().clone();
        let creation_request = creation.request();
        let request = contract::ImportSourceRequest {
            client_operation_id: operation_id.to_wire(),
            destination: Some(destination.clone()),
            source: Some(contract::ProviderRepository {
                connection: None,
                provider_repository_id: clone_url.to_string(),
                clone_url: clone_url.to_string(),
                name: thread_name.to_string(),
                private: false,
                installation_id: String::new(),
            }),
            expected_destination_version: overview.version,
            thread_genesis: creation_request.thread_genesis.clone(),
            creator_authority: creation_request.creator_authority.clone(),
            initial_base_state: initial_base
                .encode_current_msgpack()
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?,
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
            thread,
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
    use super::*;
    use api::v2::client::Rpc as _;

    #[test]
    fn public_source_shape_uses_the_url_as_its_provider_identity() {
        let url = "https://github.com/octocat/Hello-World.git";
        let source = contract::ProviderRepository {
            connection: None,
            provider_repository_id: url.into(),
            clone_url: url.into(),
            name: "main".into(),
            private: false,
            installation_id: String::new(),
        };
        assert!(source.connection.is_none());
        assert_eq!(source.provider_repository_id, source.clone_url);
        assert!(!source.private);
        assert!(source.installation_id.is_empty());
    }

    #[test]
    fn canonical_initial_base_fits_the_import_bootstrap_bound() {
        let state = hosted_import::synthetic_initial_base().expect("stable initial base");
        let bytes = state.encode_current_msgpack().expect("canonical state");
        assert!(bytes.len() <= 4096);
    }

    #[test]
    fn import_source_uses_the_declared_v2_method() {
        assert_eq!(
            rpc::IntegrationServiceImportSource::METHOD.path,
            IMPORT_SOURCE
        );
    }

    #[test]
    fn pending_receipt_is_bound_to_the_destination_and_operation() {
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

    #[tokio::test]
    async fn import_source_round_trips_request_and_live_operation_updates() {
        let (mut client, server, captured) =
            crate::hosted_runtime::hosted::test_server::start_recording_import_source().await;
        let source_url = "https://github.com/octocat/Hello-World.git";
        let operation_id = Uuid::now_v7().to_string();
        let started = client
            .import_source("acme", source_url, "main", operation_id.clone())
            .await
            .expect("ImportSource reaches the hosted transport");
        let mut updates = Vec::new();
        let terminal = client
            .observe_import_source(&started, |record| {
                updates.push((record.state, record.completed_units));
                Ok(())
            })
            .await
            .expect("ObserveOperations reaches a terminal import state");
        assert_eq!(
            terminal.state,
            contract::operation_record::State::Completed as i32
        );
        assert_eq!(
            updates,
            vec![
                (contract::operation_record::State::Queued as i32, 0),
                (
                    contract::operation_record::State::Running as i32,
                    8 * 1024 * 1024
                ),
                (
                    contract::operation_record::State::Running as i32,
                    16 * 1024 * 1024
                ),
                (
                    contract::operation_record::State::Completed as i32,
                    32 * 1024 * 1024
                ),
            ]
        );

        client.close().await;
        server.await.expect("hosted test server");
        let captured = captured.lock().unwrap_or_else(|poison| poison.into_inner());
        let request = captured.requests.first().expect("captured ImportSource");
        assert_eq!(request.client_operation_id, started.client_operation_id);
        assert!(Uuid::parse_str(&request.client_operation_id).is_ok());
        assert_eq!(request.destination, Some(started.destination.clone()));
        assert_eq!(request.expected_destination_version, vec![7; 32]);
        let source = request.source.as_ref().expect("public Git source");
        assert!(source.connection.is_none());
        assert_eq!(source.provider_repository_id, source_url);
        assert_eq!(source.clone_url, source_url);
        assert!(!source.private);
        assert!(source.installation_id.is_empty());
        assert!(!request.creator_authority.is_empty());
        let signed = request.thread_genesis.as_ref().expect("signed genesis");
        let genesis = ThreadGenesis::decode(&signed.canonical_record).expect("canonical genesis");
        let base = objects::object::State::decode_current_msgpack(&request.initial_base_state)
            .expect("canonical initial base");
        assert_eq!(genesis.base, base.id());
        assert_eq!(genesis.spool, started.destination.id);
        assert_eq!(genesis.name, "main");
        assert_eq!(
            started.thread,
            ThreadCreation::from_signed_with_authority(
                request.client_operation_id.clone(),
                signed.clone(),
                request.creator_authority.clone(),
            )
            .expect("valid signed genesis")
            .reference()
            .clone()
        );

        let observation = captured
            .observations
            .first()
            .expect("captured ObserveOperations");
        assert_eq!(
            observation.client_operation_ids,
            [started.client_operation_id]
        );
        assert_eq!(observation.spools, [started.destination]);
        assert_eq!(observation.operations, [started.operation]);
    }
}
