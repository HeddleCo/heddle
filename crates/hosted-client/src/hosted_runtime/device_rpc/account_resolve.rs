//! Resolve local stable identities without crossing the hosted boundary.
use anyhow::{Context, Result, ensure};
use api::heddle::api::v2alpha1::*;
use objects::object::ContentHash;
use prost::Message;

use super::{DeviceRpc, account_auth::AccountSession, account_spool::scope_id};
impl DeviceRpc {
    pub(super) fn resolve_local_resources(
        &self,
        session: &AccountSession,
        request: &ResolveResourcesRequest,
    ) -> Result<ResolveResourcesResponse> {
        let budget = super::stream::budget(request.budget.clone());
        ensure!(
            !request.selectors.is_empty() && request.selectors.len() <= budget.max_items as usize,
            "resource selector count exceeds budget"
        );
        let catalog = repo::device_catalog::store::Catalog::read(&self.home)?;
        let mut results = Vec::new();
        for (index, selector) in request.selectors.iter().enumerate() {
            let mut resource = None;
            let mut coverage = Coverage::Unavailable;
            if let Some(catalog) = &catalog {
                match selector.selector.as_ref() {
                    Some(resource_selector::Selector::SpoolAddress(address)) => {
                        if let Some(record) = catalog.find_spool_address(address)? {
                            if session.permits(&record.registration.capability_path) {
                                resource = Some(EntityRef {
                                    entity: Some(entity_ref::Entity::Spool(SpoolRef {
                                        id: record.registration.id.to_string(),
                                    })),
                                });
                                coverage = Coverage::Complete;
                            }
                        }
                    }
                    Some(resource_selector::Selector::Resource(reference)) => {
                        let scope = match reference.entity.as_ref() {
                            Some(entity_ref::Entity::Spool(r)) => Some(r),
                            Some(entity_ref::Entity::Thread(r)) => r.spool.as_ref(),
                            _ => None,
                        };
                        if let Some(scope) = scope {
                            if let Some(record) = catalog.spool(scope_id(Some(scope))?)? {
                                if session.permits(&record.registration.capability_path) {
                                    let available = match reference.entity.as_ref() {
                                        Some(entity_ref::Entity::Spool(_)) => true,
                                        Some(entity_ref::Entity::Thread(thread)) => {
                                            let id = thread
                                                .id
                                                .as_ref()
                                                .context("Thread identity required")?;
                                            let hash = ContentHash::from_bytes(
                                                id.value.as_slice().try_into().context(
                                                    "Thread identity must contain32bytes",
                                                )?,
                                            );
                                            repo::thread_replication::ThreadReplica::open(
                                                &record.registration.heddle_dir,
                                                hash,
                                            )
                                            .is_ok()
                                        }
                                        _ => false,
                                    };
                                    if available {
                                        resource = Some(reference.clone());
                                        coverage = Coverage::Complete;
                                    }
                                }
                            }
                        }
                    }
                    Some(resource_selector::Selector::ThreadName(selector)) => {
                        ensure!(
                            !selector.name.is_empty() && selector.name.len() <= 4096,
                            "Thread name bound"
                        );
                        if let Some(record) = catalog.spool(scope_id(selector.spool.as_ref())?)? {
                            if session.permits(&record.registration.capability_path) {
                                let cursor = repo::thread_replication::listing::Cursor {
                                    name: selector.name.clone(),
                                    updated: 0,
                                    thread: Vec::new(),
                                };
                                let rows = repo::thread_replication::listing::page(
                                    &record.registration.heddle_dir,
                                    true,
                                    Some(&cursor),
                                    2,
                                )?;
                                let matches: Vec<_> = rows
                                    .into_iter()
                                    .filter(|r| r.name == selector.name)
                                    .collect();
                                if matches.len() == 1 {
                                    resource = Some(EntityRef {
                                        entity: Some(entity_ref::Entity::Thread(ThreadRef {
                                            spool: selector.spool.clone(),
                                            id: Some(ThreadId {
                                                value: matches[0].thread.as_bytes().to_vec(),
                                            }),
                                        })),
                                    });
                                    coverage = Coverage::Complete;
                                }
                            }
                        }
                    }
                    Some(resource_selector::Selector::PrincipalHandle(_)) => {}
                    None => ensure!(false, "resource selector required"),
                }
            }
            results.push(ResourceResolution {
                selection_index: index as u32,
                resource,
                principal_id: String::new(),
                coverage: coverage as i32,
            });
        }
        let response = ResolveResourcesResponse { results };
        ensure!(
            response.encoded_len() <= budget.max_snapshot_bytes as usize,
            "resolved resources exceed byte budget"
        );
        Ok(response)
    }
}
