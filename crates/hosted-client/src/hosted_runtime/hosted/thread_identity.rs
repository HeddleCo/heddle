//! Human addresses resolve once; every subsequent command carries stable IDs.
use api::heddle::api::v2alpha1 as contract;
use thread_api::rpc;
use wire::ProtocolError;

use super::HostedClient;

impl HostedClient {
    pub(super) async fn resolve_principal_id(
        &self,
        handle_or_id: &str,
        spool: &contract::SpoolRef,
    ) -> Result<String, ProtocolError> {
        if let Ok(id) = uuid::Uuid::parse_str(handle_or_id) {
            return Ok(id.to_string());
        }
        let remote = self.native().await.map_err(native_error)?;
        let response = remote
            .api
            .call::<rpc::WorkspaceServiceResolveResources>(&contract::ResolveResourcesRequest {
                selectors: vec![
                    contract::ResourceSelector {
                        selector: Some(contract::resource_selector::Selector::Resource(
                            contract::EntityRef {
                                entity: Some(contract::entity_ref::Entity::Spool(spool.clone())),
                            },
                        )),
                    },
                    contract::ResourceSelector {
                        selector: Some(contract::resource_selector::Selector::PrincipalHandle(
                            handle_or_id.to_owned(),
                        )),
                    },
                ],
                budget: None,
            })
            .await
            .map_err(super::helpers::native_client_error)?;
        let mut results = response.results.into_iter();
        let scope = results.next().ok_or_else(|| {
            ProtocolError::InvalidState("principal resolution omitted Spool scope".into())
        })?;
        let result = results.next().ok_or_else(|| {
            ProtocolError::ObjectNotFound("principal handle did not resolve".into())
        })?;
        if results.next().is_some()
            || scope.selection_index != 0
            || scope.coverage != contract::Coverage::Complete as i32
            || !scope.principal_id.is_empty()
            || scope
                .resource
                .as_ref()
                .and_then(|value| value.entity.as_ref())
                != Some(&contract::entity_ref::Entity::Spool(spool.clone()))
            || result.selection_index != 1
            || result.coverage != contract::Coverage::Complete as i32
            || result.resource.is_some()
        {
            return Err(ProtocolError::InvalidState(
                "principal resolution was incomplete or ambiguous".into(),
            ));
        }
        Ok(uuid::Uuid::parse_str(&result.principal_id)
            .map_err(native_error)?
            .to_string())
    }

    pub(super) async fn native_spool_overview(
        &self,
        address: &str,
    ) -> Result<contract::SpoolOverview, ProtocolError> {
        let spool = self.resolve_spool_ref(address).await?;
        let remote = self.native().await.map_err(native_error)?;
        let mut observation = remote
            .observe::<rpc::SpoolServiceObserveSpool>(
                contract::ObserveSpoolRequest {
                    spool: Some(spool.clone()),
                    sections: vec![contract::SpoolSection::Overview as i32],
                    observe: Some(contract::ObserveOptions {
                        mode: contract::ObservationMode::Once as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_error)?;
        let batch = observation
            .next_commit()
            .await
            .map_err(native_error)?
            .ok_or_else(|| {
                ProtocolError::InvalidState("spool observation ended without a checkpoint".into())
            })?;
        let mut overviews = batch.changes.into_iter().filter_map(|change| match change {
            contract::spool_event::Payload::Spool(overview) => Some(overview),
            _ => None,
        });
        let overview = overviews
            .next()
            .ok_or_else(|| ProtocolError::InvalidState("spool overview unavailable".into()))?;
        if overview.r#ref.as_ref() != Some(&spool)
            || overviews.next().is_some()
            || overview.version.is_empty()
        {
            return Err(ProtocolError::InvalidState(
                "spool observation identity or version is inconsistent".into(),
            ));
        }
        Ok(overview)
    }
    pub async fn resolve_spool_ref(
        &self,
        address: &str,
    ) -> Result<contract::SpoolRef, ProtocolError> {
        if let Ok(id) = uuid::Uuid::parse_str(address) {
            return Ok(contract::SpoolRef { id: id.to_string() });
        }
        let remote = self.native().await.map_err(native_error)?;
        let response = remote
            .api
            .call::<rpc::WorkspaceServiceResolveResources>(&contract::ResolveResourcesRequest {
                selectors: vec![contract::ResourceSelector {
                    selector: Some(contract::resource_selector::Selector::SpoolAddress(
                        address.into(),
                    )),
                }],
                budget: None,
            })
            .await
            .map_err(super::helpers::native_client_error)?;
        match resolved_entity(response)? {
            contract::entity_ref::Entity::Spool(spool) => {
                uuid::Uuid::parse_str(&spool.id).map_err(native_error)?;
                Ok(spool)
            }
            _ => Err(ProtocolError::InvalidState(
                "spool resolution returned another resource type".into(),
            )),
        }
    }

    pub async fn resolve_thread_ref(
        &self,
        spool_address: &str,
        name_or_id: &str,
    ) -> Result<contract::ThreadRef, ProtocolError> {
        let spool = self.resolve_spool_ref(spool_address).await?;
        if name_or_id.len() == 64
            && let Ok(value) = hex::decode(name_or_id)
        {
            return Ok(contract::ThreadRef {
                spool: Some(spool),
                id: Some(contract::ThreadId { value }),
            });
        }
        let remote = self.native().await.map_err(native_error)?;
        let mut observation = remote
            .observe::<rpc::ThreadServiceObserveThreads>(
                contract::ObserveThreadsRequest {
                    query: Some(contract::ThreadQuery {
                        spools: vec![spool.clone()],
                        order: contract::thread_query::Order::NameAsc as i32,
                        ..Default::default()
                    }),
                    observe: Some(contract::ObserveOptions {
                        mode: contract::ObservationMode::Once as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_error)?;
        while let Some(batch) = observation.next_commit().await.map_err(native_error)? {
            for change in batch.changes {
                if let contract::thread_list_event::Payload::Thread(overview) = change
                    && overview.name == name_or_id
                    && let Some(reference) = overview.r#ref
                    && reference.spool.as_ref() == Some(&spool)
                    && reference.id.as_ref().is_some_and(|id| id.value.len() == 32)
                {
                    return Ok(reference);
                }
            }
        }
        Err(ProtocolError::ObjectNotFound(format!(
            "Thread '{name_or_id}' is not published on this spool"
        )))
    }

    pub(super) async fn current_owner_state(&self) -> Result<contract::OwnerState, ProtocolError> {
        let remote = self.native().await.map_err(native_error)?;
        let mut observation = remote
            .observe::<rpc::OwnerAuthorizationServiceObserveOwnership>(
                contract::ObserveOwnershipRequest {
                    observe: Some(contract::ObserveOptions {
                        mode: contract::ObservationMode::Once as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_error)?;
        let batch = observation
            .next_commit()
            .await
            .map_err(native_error)?
            .ok_or_else(|| {
                ProtocolError::InvalidState(
                    "ownership observation ended without a checkpoint".into(),
                )
            })?;
        let mut owners = batch.changes.into_iter().filter_map(|change| match change {
            contract::ownership_event::Payload::Owner(owner) => Some(owner),
            _ => None,
        });
        let owner = owners.next().ok_or_else(|| {
            ProtocolError::InvalidState("current owner history unavailable".into())
        })?;
        if owners.next().is_some() {
            return Err(ProtocolError::InvalidState(
                "ownership observation returned ambiguous identity".into(),
            ));
        }
        Ok(owner)
    }

    pub async fn require_thread_id(
        &self,
        spool_address: &str,
        name_or_id: &str,
    ) -> Result<String, ProtocolError> {
        let thread = self.resolve_thread_ref(spool_address, name_or_id).await?;
        let id = thread
            .id
            .ok_or_else(|| ProtocolError::InvalidState("Thread identity absent".into()))?;
        Ok(hex::encode(id.value))
    }
}

fn resolved_entity(
    response: contract::ResolveResourcesResponse,
) -> Result<contract::entity_ref::Entity, ProtocolError> {
    let mut results = response.results.into_iter();
    let resolved = results
        .next()
        .ok_or_else(|| ProtocolError::ObjectNotFound("resource resolution is absent".into()))?;
    if results.next().is_some() || resolved.selection_index != 0 {
        return Err(ProtocolError::InvalidState(
            "resource resolution differs from requested selection".into(),
        ));
    }
    if resolved.coverage != contract::Coverage::Complete as i32 {
        return Err(ProtocolError::ObjectNotFound(
            "resource is unavailable or ambiguous".into(),
        ));
    }
    resolved.resource.and_then(|r| r.entity).ok_or_else(|| {
        ProtocolError::InvalidState("complete resource resolution has no identity".into())
    })
}

fn native_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::InvalidState(error.to_string())
}
