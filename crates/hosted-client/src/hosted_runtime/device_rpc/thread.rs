//! Original-author Thread mutations and bounded native projections.
use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};
use api::heddle::api::v2alpha1::*;
use objects::{
    object::thread_replication::{
        Admission, OPERATION_FORMAT, ThreadOperationBody,
        metadata::{Control, Lifecycle, Property, ThreadControl, property_version},
    },
    store::ObjectStore,
};
use prost::Message;
use repo::thread_replication::{ThreadReplica, projection};
use serde::{Deserialize, Serialize};
use thread_api::thread_control::PreparedControl;

use super::{DeviceRpc, auth::Session, checkout, read_bounded};

#[derive(Serialize, Deserialize)]
struct Command {
    method: String,
    actor: String,
    body: Vec<u8>,
    response: Option<Vec<u8>>,
}
impl DeviceRpc {
    pub(super) fn thread_command(
        &self,
        session: &Session,
        method: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        let descriptor = api::v2::method_descriptor(method).context("Thread method required")?;
        let command = descriptor
            .client_operation_id(body)?
            .context("Thread command ID required")?;
        let command = uuid::Uuid::parse_str(command)?;
        let directory = session.spool.heddle_dir.join("device-thread-commands");
        objects::fs_atomic::create_private_dir_all(&directory)?;
        let _guard = objects::lock::RepoLock::at(directory.join(format!("{command}.lock")))
            .try_write()?
            .context("Thread command already executing")?;
        let path = directory.join(format!("{command}.json"));
        let mut journal = if path.exists() {
            let value: Command = serde_json::from_slice(&read_bounded(&path, 1024 * 1024)?)?;
            ensure!(
                value.method == method && value.actor == session.actor && value.body == body,
                "Thread command ID reused with different inputs or delivery actor"
            );
            if let Some(response) = value.response {
                return Ok(response);
            }
            value
        } else {
            let value = Command {
                method: method.into(),
                actor: session.actor.clone(),
                body: body.to_vec(),
                response: None,
            };
            objects::fs_atomic::write_file_atomic_secret(&path, &serde_json::to_vec(&value)?)?;
            value
        };
        let repository = repo::Repository::open(&session.spool.root)?;
        let replica = if method.ends_with("/StartThread") {
            let request = StartThreadRequest::decode(body)?;
            checkout::same_spool(session, request.spool.as_ref())?;
            let record = request
                .thread_genesis
                .context("original creator record required")?;
            let genesis = objects::object::thread_replication::ThreadGenesis::decode(
                &record.canonical_record,
            )?;
            ensure!(
                genesis.spool == session.spool.id.to_string(),
                "Thread genesis belongs to another Spool"
            );
            ensure!(
                genesis.creator == session.publisher,
                "Thread creator differs from authenticated publisher"
            );
            let reference = ThreadRef {
                spool: Some(SpoolRef {
                    id: genesis.spool.clone(),
                }),
                id: Some(ThreadId {
                    value: genesis.id()?.as_bytes().to_vec(),
                }),
            };
            thread_api::replication::opening::verify_genesis(&record, &reference)?;
            let [signature] = record.signatures.as_slice() else {
                bail!("one original creator signature required");
            };
            let signed = crypto::thread_operation::SignedGenesis {
                canonical: record.canonical_record.clone(),
                signature: signature.signature.clone(),
            };
            repository
                .store()
                .get_state(&genesis.base)?
                .context("Thread base source is not available on this device")?;
            if let Some(parent) = genesis.parent {
                let parent = ThreadReplica::open(&session.spool.heddle_dir, parent)?;
                session.authorize_thread(&repository, &parent)?;
            }
            let now = chrono::Utc::now().timestamp();
            let authority = repo::device_authority::load(&self.home, now)?;
            ThreadReplica::create_authorized(
                &session.spool.heddle_dir,
                &signed,
                &request.creator_authority,
                &authority,
                &session.spool.capability_path,
                method,
                now,
            )?
        } else {
            let (reference, record) = match method.rsplit('/').next().context("method")? {
                "RenameThread" => {
                    let r = RenameThreadRequest::decode(body)?;
                    (r.thread, r.operation)
                }
                "ReviseIntent" => {
                    let r = ReviseIntentRequest::decode(body)?;
                    (r.thread, r.operation)
                }
                "ChangeLifecycle" => {
                    let r = ChangeThreadLifecycleRequest::decode(body)?;
                    (r.thread, r.operation)
                }
                "SetSharingPolicy" => {
                    let r = SetThreadSharingRequest::decode(body)?;
                    (r.policy.and_then(|p| p.thread), r.operation)
                }
                "SetAudiencePolicy" => {
                    let r = SetThreadAudienceRequest::decode(body)?;
                    (r.policy.and_then(|p| p.thread), r.operation)
                }
                "SetRetentionPolicy" => {
                    let r = SetThreadRetentionRequest::decode(body)?;
                    (r.policy.and_then(|p| p.thread), r.operation)
                }
                "RecordReview" => {
                    let r = RecordReviewRequest::decode(body)?;
                    (r.decision.and_then(|d| d.thread), r.operation)
                }
                _ => bail!("unsupported native Thread mutation"),
            };
            let reference = reference.context("Thread scope required")?;
            let replica = ThreadReplica::open(
                &session.spool.heddle_dir,
                checkout::thread(session, Some(&reference))?,
            )?;
            let record = record.context("original caller-signed control required")?;
            let signed = thread_api::replication::decode_record(record.clone())?;
            let operation = signed.verify()?;
            let ThreadOperationBody::Metadata(bytes) = &operation.body else {
                bail!("Thread control requires metadata");
            };
            let control = ThreadControl::decode(bytes)?;
            ensure!(
                operation.thread == replica.thread_id()
                    && control.spool == session.spool.id
                    && control.client_operation_id == command
                    && control.authorization_method() == method,
                "signed control scope, command or method differs from request"
            );
            let current = replica.projection()?;
            let already_admitted = replica.control_authority_admitted(&signed)?;
            let parents = operation.parents.clone();
            let prepared = PreparedControl {
                record,
                control: control.clone(),
                thread: reference,
                property_version: property_version(
                    replica.thread_id(),
                    &control.property(),
                    &parents,
                )?
                .as_bytes()
                .to_vec(),
                thread_version: Vec::new(),
            };
            match &control.control {
                Control::Name(_) => {
                    let request = RenameThreadRequest::decode(body)?;
                    ensure!(
                        request == prepared.rename()?,
                        "rename differs from signed control or observed version"
                    );
                }
                Control::Intent(_) => ensure!(
                    ReviseIntentRequest::decode(body)? == prepared.revise_intent()?,
                    "intent differs from signed control or observed version"
                ),
                Control::Lifecycle(_) => {
                    let request = ChangeThreadLifecycleRequest::decode(body)?;
                    ensure!(
                        request == prepared.change_lifecycle()?,
                        "lifecycle differs from signed control or observed version"
                    );
                }
                Control::Sharing(policy) => {
                    ensure!(
                        SetThreadSharingRequest::decode(body)? == prepared.set_sharing()?,
                        "sharing differs from signed control or observed version"
                    );
                    ensure!(
                        policy
                            .destinations
                            .iter()
                            .all(|destination| destination.spool == session.spool.id),
                        "native sharing retains the same stable Spool identity"
                    );
                }
                Control::Audience(_) => ensure!(
                    SetThreadAudienceRequest::decode(body)? == prepared.set_audience()?,
                    "audience differs from signed control or observed version"
                ),
                Control::Retention(_) => ensure!(
                    SetThreadRetentionRequest::decode(body)? == prepared.set_retention()?,
                    "retention differs from signed control or observed version"
                ),
                Control::Review(review) => {
                    ensure!(
                        RecordReviewRequest::decode(body)? == prepared.record_review()?,
                        "review differs from signed control or observed version"
                    );
                    if !already_admitted {
                        if let Some(revokes) = review.revokes {
                            let previous = replica.metadata_frontier(&Property::Review(revokes))?;
                            ensure!(!previous.is_empty(), "revoked review is not admitted");
                            for (_, signed) in previous {
                                let original = signed.verify()?;
                                let ThreadOperationBody::Metadata(bytes) = original.body else {
                                    bail!("review index facet")
                                };
                                let original = ThreadControl::decode(&bytes)?;
                                ensure!(
                                    original.actor == control.actor,
                                    "review revocation belongs to another original actor"
                                );
                            }
                        }
                        ensure!(
                            review.source == current.genesis.base
                                || replica.accepted_source_revision(review.source)?.is_some(),
                            "review source is not admitted in this Thread"
                        );
                        let target_available = if review.target == current.genesis.base {
                            true
                        } else if let Some(parent) = current.genesis.parent {
                            let target = ThreadReplica::open(&session.spool.heddle_dir, parent)?;
                            review.target == target.genesis()?.base
                                || target.accepted_source_revision(review.target)?.is_some()
                        } else {
                            false
                        };
                        ensure!(
                            target_available,
                            "review comparison target is outside this Thread lineage"
                        );
                        ensure!(
                            review.policy_version == super::land::policy_version(&repository)?,
                            "native review policy changed"
                        );
                    }
                }
            }
            let now = chrono::Utc::now().timestamp();
            let authority = repo::device_authority::load(&self.home, now)?;
            let admission =
                replica.receive_control_cas(&signed, repository.store(), |operation| {
                    if already_admitted {
                        Ok(())
                    } else {
                        repo::thread_replication::metadata::verify_control_authority(
                            operation,
                            &authority,
                            &session.spool.capability_path,
                            now,
                        )
                    }
                })?;
            ensure!(
                admission == Admission::Accepted,
                "interactive Thread command requires accepted current causal parents"
            );
            if matches!(control.control, Control::Sharing(_)) {
                ensure!(
                    control.actor.principal_id.to_string() == session.principal,
                    "publication consent requires the device-owning account"
                );
                replica.consent_to_policy_sync(control.actor.principal_id)?;
            }
            replica
        };
        let response = if [
            "/SetSharingPolicy",
            "/SetAudiencePolicy",
            "/SetRetentionPolicy",
            "/RecordReview",
        ]
        .iter()
        .any(|suffix| method.ends_with(suffix))
        {
            MutationResponse {
                receipt: Some(self.receipt(&command.to_string())),
            }
            .encode_to_vec()
        } else {
            ThreadMutationResponse {
                receipt: Some(self.receipt(&command.to_string())),
                thread: Some(self.thread_overview(session, &replica)?),
            }
            .encode_to_vec()
        };
        journal.response = Some(response.clone());
        objects::fs_atomic::write_file_atomic_secret(&path, &serde_json::to_vec(&journal)?)?;
        Ok(response)
    }
    pub(super) fn thread_overview(
        &self,
        session: &Session,
        replica: &ThreadReplica,
    ) -> Result<ThreadOverview> {
        let mut overview = self
            .thread_overview_for_spool(&session.spool, replica, |method| session.permits(method))?;
        if let Some(catalog) = repo::device_catalog::store::Catalog::read(&self.home)? {
            overview.current_bookmark = Some(catalog.bookmark(&BookmarkRef {
                account: Some(PrincipalRef {
                    id: session.principal.clone(),
                }),
                target: Some(bookmark_ref::Target::Thread(
                    overview.r#ref.clone().context("Thread reference")?,
                )),
            })?);
        }
        Ok(overview)
    }
    pub(super) fn thread_overview_for_spool(
        &self,
        spool: &repo::device_catalog::DeviceSpool,
        replica: &ThreadReplica,
        permits: impl Fn(&str) -> bool,
    ) -> Result<ThreadOverview> {
        let view = replica.projection()?;
        let reference = ThreadRef {
            spool: Some(SpoolRef {
                id: spool.id.to_string(),
            }),
            id: Some(ThreadId {
                value: replica.thread_id().as_bytes().to_vec(),
            }),
        };
        let mut overview = ThreadOverview {
            r#ref: Some(reference.clone()),
            name: view.genesis.name.clone(),
            version: projection::version(replica.thread_id(), view.generation)
                .as_bytes()
                .to_vec(),
            intent: Some(ThreadIntent {
                outcome: view.genesis.intent.clone(),
                ..Default::default()
            }),
            source_heads: view
                .source_heads
                .iter()
                .map(|state| revision(&reference, *state))
                .collect(),
            base: Some(revision(&reference, view.genesis.base)),
            lifecycle: ThreadLifecycle::Draft as i32,
            readiness: ReviewReadiness::Unknown as i32,
            capture_count: Some(view.capture_count),
            ..Default::default()
        };
        for (property, candidates) in &view.fields {
            let ids = candidates
                .iter()
                .map(|(id, _)| *id)
                .collect::<BTreeSet<_>>();
            let (kind, record_id) = thread_api::thread_control::property_key(property);
            let frontier = ThreadPropertyFrontier {
                property: kind as i32,
                record_id,
                version: property_version(replica.thread_id(), property, &ids)?
                    .as_bytes()
                    .to_vec(),
                operation_ids: ids.iter().map(|id| id.as_bytes().to_vec()).collect(),
            };
            if *property == Property::Intent {
                if let Some(intent) = overview.intent.as_mut() {
                    intent.version = frontier.version.clone();
                }
            }
            overview.metadata_frontiers.push(frontier.clone());
            if candidates.len() > 1 {
                overview.metadata_conflicts.push(ThreadMetadataConflict {
                    frontier: Some(frontier),
                    candidates: candidates
                        .iter()
                        .map(|(_, signed)| signed_record(signed))
                        .collect::<Result<_>>()?,
                });
                continue;
            }
            if *property == Property::Audience && candidates.is_empty() {
                overview.audience = Some(ThreadAudiencePolicy {
                    thread: Some(reference.clone()),
                    version: frontier.version.clone(),
                    kind: thread_audience_policy::Kind::Owner as i32,
                    ..Default::default()
                });
            }
            if let Some((_, signed)) = candidates.first() {
                let operation = signed.verify()?;
                let ThreadOperationBody::Metadata(bytes) = operation.body else {
                    bail!("field index names another facet");
                };
                let control = ThreadControl::decode(&bytes)?;
                let prepared = PreparedControl {
                    record: signed_record(signed)?,
                    control: control.clone(),
                    thread: reference.clone(),
                    property_version: frontier.version.clone(),
                    thread_version: Vec::new(),
                };
                match control.control {
                    Control::Name(name) => overview.name = name,
                    Control::Intent(intent) => {
                        overview.intent = Some(ThreadIntent {
                            outcome: intent.outcome,
                            acceptance_criteria: intent.acceptance_criteria,
                            origin_urls: intent.origin_urls,
                            principal_approved: intent.principal_approved,
                            principal_id: control.actor.principal_id.to_string(),
                            agent_id: control.actor.agent_id.unwrap_or_default(),
                            version: frontier.version,
                        })
                    }
                    Control::Lifecycle(value) => {
                        overview.lifecycle = match value {
                            Lifecycle::Draft => ThreadLifecycle::Draft,
                            Lifecycle::Active => ThreadLifecycle::Active,
                            Lifecycle::Ready => ThreadLifecycle::Ready,
                            Lifecycle::Abandoned => ThreadLifecycle::Abandoned,
                        } as i32
                    }
                    Control::Sharing(_) => {}
                    Control::Audience(_) => {
                        overview.audience = prepared.set_audience()?.policy;
                        if let Some(policy) = &mut overview.audience {
                            policy.version = frontier.version.clone();
                        }
                    }
                    Control::Retention(_) => {
                        overview.retention = prepared.set_retention()?.policy;
                        if let Some(policy) = &mut overview.retention {
                            policy.version = frontier.version.clone();
                        }
                    }
                    Control::Review(_) => bail!("overview singleton index includes review"),
                }
            }
        }
        let repository = repo::Repository::open(&spool.root)?;
        overview.review_policy_version = super::land::policy_version(&repository)?
            .as_bytes()
            .to_vec();
        for (property, suffix) in [
            (Property::Name, "RenameThread"),
            (Property::Intent, "ReviseIntent"),
            (Property::Lifecycle, "ChangeLifecycle"),
            (Property::Sharing, "SetSharingPolicy"),
            (Property::Audience, "SetAudiencePolicy"),
            (Property::Retention, "SetRetentionPolicy"),
        ] {
            let (kind, _) = thread_api::thread_control::property_key(&property);
            let frontier = overview
                .metadata_frontiers
                .iter()
                .find(|p| p.property == kind as i32)
                .context("action property frontier")?;
            let method = format!("/heddle.api.v2alpha1.ThreadService/{suffix}");
            overview.actions.push(ActionAvailability {
                authorized: permits(&method),
                method,
                endpoint: Some(self.endpoint()),
                target: Some(EntityRef {
                    entity: Some(entity_ref::Entity::Thread(reference.clone())),
                }),
                implemented: true,
                observed_versions: vec![ExpectedVersion {
                    resource: Some(EntityRef {
                        entity: Some(entity_ref::Entity::Thread(reference.clone())),
                    }),
                    version: frontier.version.clone(),
                }],
                ..Default::default()
            });
        }
        let method = "/heddle.api.v2alpha1.ThreadService/RecordReview";
        overview.actions.push(ActionAvailability {
            method: method.into(),
            endpoint: Some(self.endpoint()),
            target: Some(EntityRef {
                entity: Some(entity_ref::Entity::Thread(reference.clone())),
            }),
            implemented: true,
            authorized: permits(method),
            observed_versions: vec![ExpectedVersion {
                resource: Some(EntityRef {
                    entity: Some(entity_ref::Entity::Policy(RecordRef {
                        spool: reference.spool.clone(),
                        id: "device-local-integration-policy".into(),
                    })),
                }),
                version: overview.review_policy_version.clone(),
            }],
            ..Default::default()
        });
        if let Some(parent) = view.genesis.parent {
            overview.relationships.push(ThreadRelationship {
                thread: Some(ThreadRef {
                    spool: reference.spool.clone(),
                    id: Some(ThreadId {
                        value: parent.as_bytes().to_vec(),
                    }),
                }),
                kind: thread_relationship::Kind::Parent as i32,
            });
        }
        Ok(overview)
    }
}
pub(super) fn revision(reference: &ThreadRef, state: objects::object::StateId) -> RevisionRef {
    RevisionRef {
        spool: reference.spool.clone(),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::v1alpha1::StateId {
                value: state.as_bytes().to_vec(),
            },
        )),
    }
}
pub(super) fn signed_record(
    signed: &crypto::thread_operation::SignedOperation,
) -> Result<SignedRecord> {
    let operation = signed.verify()?;
    Ok(SignedRecord {
        format: OPERATION_FORMAT.into(),
        canonical_record: signed.canonical.clone(),
        signatures: vec![RecordSignature {
            public_key: operation.publisher.to_vec(),
            signature: signed.signature.clone(),
        }],
    })
}
