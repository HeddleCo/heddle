//! Prepare portable Thread edits directly from an observed field frontier.
//! Signing is local; the resulting typed request is retained unchanged for retries.
use std::collections::BTreeSet;

use crypto::{Signer, thread_operation::SignedOperation};
pub use heddle_object_model::object::thread_replication::metadata::{
    Control, Destination, EndpointKind, Intent, Lifecycle, Review, ReviewKind, SharedFacet,
    SharingPolicy,
};
use heddle_object_model::object::{
    CollaborationActor, ContentHash,
    thread_replication::{
        OPERATION_FORMAT, ThreadOperation, ThreadOperationBody,
        metadata::{AUTHORITY_FORMAT, Property, ThreadControl, property_version},
    },
};
use uuid::Uuid;

use crate::{contract as wire, transport::Error};

/// Public original-author evidence. Build the envelope with the shared capability
/// verifier using independently enrolled account history and the original Biscuit.
/// The receiving endpoint verifies it independently at admission.
pub struct Author<'a> {
    pub account: Uuid,
    pub agent_id: Option<&'a str>,
    pub authority_envelope: &'a [u8],
}

/// Canonical original operation, plus the exact observed versions for its RPC.
/// A command never invents a frontier from a mutable projection or wall clock.
pub struct PreparedControl {
    pub record: wire::SignedRecord,
    pub control: ThreadControl,
    pub thread: wire::ThreadRef,
    pub property_version: Vec<u8>,
    pub thread_version: Vec<u8>,
}
impl PreparedControl {
    pub fn sign(
        observed: &wire::ThreadOverview,
        control: Control,
        author: Author<'_>,
        operation_id: Uuid,
        occurred_at_ms: i64,
        signer: &impl Signer,
    ) -> Result<Self, Error> {
        let reference = observed
            .r#ref
            .as_ref()
            .ok_or(Error::Protocol("Thread reference missing"))?;
        let spool = reference
            .spool
            .as_ref()
            .ok_or(Error::Protocol("Spool missing"))?;
        let thread = hash(
            &reference
                .id
                .as_ref()
                .ok_or(Error::Protocol("Thread ID missing"))?
                .value,
        )?;
        let control = ThreadControl {
            version: 1,
            spool: spool
                .id
                .parse()
                .map_err(|_| Error::Protocol("Spool ID must be UUID"))?,
            actor: CollaborationActor {
                principal_id: author.account,
                agent_id: author.agent_id.map(str::to_owned),
            },
            authority_digest: ContentHash::compute_typed(
                AUTHORITY_FORMAT,
                author.authority_envelope,
            ),
            authority_envelope: author.authority_envelope.to_vec(),
            client_operation_id: operation_id,
            occurred_at_ms,
            control,
        };
        let property = control.property();
        let (kind, record_id) = property_key(&property);
        let mut matches = observed
            .metadata_frontiers
            .iter()
            .filter(|frontier| frontier.property == kind as i32 && frontier.record_id == record_id);
        let empty_review;
        let frontier = match matches.next() {
            Some(frontier) => frontier,
            None if matches!(property, Property::Review(_)) => {
                empty_review = wire::ThreadPropertyFrontier {
                    property: kind as i32,
                    record_id: record_id.clone(),
                    version: property_version(thread, &property, &BTreeSet::new())
                        .map_err(io_error)?
                        .as_bytes()
                        .to_vec(),
                    operation_ids: vec![],
                };
                &empty_review
            }
            None => {
                return Err(Error::Protocol(
                    "observe the exact Thread property frontier before signing",
                ));
            }
        };
        if matches.next().is_some() || frontier.operation_ids.len() > 128 {
            return Err(Error::Protocol(
                "invalid or duplicate Thread property frontier",
            ));
        }
        let parents = frontier
            .operation_ids
            .iter()
            .map(|id| hash(id))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if parents.len() != frontier.operation_ids.len()
            || property_version(thread, &property, &parents)
                .map_err(io_error)?
                .as_bytes()
                .as_slice()
                != frontier.version
        {
            return Err(Error::Protocol(
                "Thread property version does not bind its exact parents",
            ));
        }
        let operation = ThreadOperation {
            version: 1,
            thread,
            parents,
            publisher: signer
                .public_key()
                .try_into()
                .map_err(|_| Error::Protocol("Thread control requires Ed25519"))?,
            body: ThreadOperationBody::Metadata(control.encode().map_err(io_error)?),
        };
        let signed = SignedOperation::sign(&operation, signer).map_err(io_error)?;
        Ok(Self {
            record: wire::SignedRecord {
                format: OPERATION_FORMAT.into(),
                canonical_record: signed.canonical,
                signatures: vec![wire::RecordSignature {
                    public_key: operation.publisher.to_vec(),
                    signature: signed.signature,
                }],
            },
            control,
            thread: reference.clone(),
            property_version: frontier.version.clone(),
            thread_version: observed.version.clone(),
        })
    }
    pub fn revise_intent(&self) -> Result<wire::ReviseIntentRequest, Error> {
        let Control::Intent(value) = &self.control.control else {
            return Err(Error::Protocol("control is not an intent"));
        };
        Ok(wire::ReviseIntentRequest {
            client_operation_id: self.control.client_operation_id.to_string(),
            thread: Some(self.thread.clone()),
            expected_intent_version: self.property_version.clone(),
            proposed_intent: Some(wire::ThreadIntent {
                outcome: value.outcome.clone(),
                acceptance_criteria: value.acceptance_criteria.clone(),
                origin_urls: value.origin_urls.clone(),
                principal_approved: value.principal_approved,
                principal_id: self.control.actor.principal_id.to_string(),
                agent_id: self.control.actor.agent_id.clone().unwrap_or_default(),
                version: vec![],
            }),
            operation: Some(self.record.clone()),
        })
    }
    pub fn rename(&self) -> Result<wire::RenameThreadRequest, Error> {
        let Control::Name(name) = &self.control.control else {
            return Err(Error::Protocol("control is not a name"));
        };
        if self.thread_version.is_empty() {
            return Err(Error::Protocol("observed Thread version missing"));
        }
        Ok(wire::RenameThreadRequest {
            client_operation_id: self.control.client_operation_id.to_string(),
            thread: Some(self.thread.clone()),
            expected_version: self.thread_version.clone(),
            name: name.clone(),
            operation: Some(self.record.clone()),
        })
    }
    pub fn change_lifecycle(&self) -> Result<wire::ChangeThreadLifecycleRequest, Error> {
        let Control::Lifecycle(value) = self.control.control else {
            return Err(Error::Protocol("control is not lifecycle"));
        };
        if self.thread_version.is_empty() {
            return Err(Error::Protocol("observed Thread version missing"));
        }
        Ok(wire::ChangeThreadLifecycleRequest {
            client_operation_id: self.control.client_operation_id.to_string(),
            thread: Some(self.thread.clone()),
            expected_version: self.thread_version.clone(),
            lifecycle: match value {
                Lifecycle::Draft => wire::ThreadLifecycle::Draft,
                Lifecycle::Active => wire::ThreadLifecycle::Active,
                Lifecycle::Ready => wire::ThreadLifecycle::Ready,
                Lifecycle::Abandoned => wire::ThreadLifecycle::Abandoned,
            } as i32,
            operation: Some(self.record.clone()),
        })
    }
    pub fn record_review(&self) -> Result<wire::RecordReviewRequest, Error> {
        let Control::Review(value) = &self.control.control else {
            return Err(Error::Protocol("control is not a review"));
        };
        let record = wire::RecordRef {
            spool: self.thread.spool.clone(),
            id: value.id.to_string(),
        };
        let mut decision = wire::ReviewDecision {
            r#ref: Some(record.clone()),
            thread: Some(self.thread.clone()),
            source: Some(self.revision(value.source)),
            target: Some(self.revision(value.target)),
            policy_version: value.policy_version.as_bytes().to_vec(),
            principal_id: self.control.actor.principal_id.to_string(),
            agent_id: self.control.actor.agent_id.clone().unwrap_or_default(),
            kind: match value.kind {
                ReviewKind::Opinion => wire::review_decision::Kind::Opinion,
                ReviewKind::Approval => wire::review_decision::Kind::Approval,
                ReviewKind::Rejection => wire::review_decision::Kind::Rejection,
                ReviewKind::Revocation => wire::review_decision::Kind::Revocation,
            } as i32,
            explanation: value.explanation.clone(),
            revokes: value.revokes.map(|id| wire::RecordRef {
                spool: self.thread.spool.clone(),
                id: id.to_string(),
            }),
            expires_at: None,
        };
        if let Some(seconds) = value.expires_at_unix_seconds {
            decision.expires_at = Some(Default::default());
            if let Some(timestamp) = decision.expires_at.as_mut() {
                timestamp.seconds = seconds;
            }
        }
        Ok(wire::RecordReviewRequest {
            client_operation_id: self.control.client_operation_id.to_string(),
            decision: Some(decision),
            expected_versions: vec![wire::ExpectedVersion {
                resource: Some(wire::EntityRef {
                    entity: Some(wire::entity_ref::Entity::Review(record)),
                }),
                version: self.property_version.clone(),
            }],
            operation: Some(self.record.clone()),
        })
    }
    fn revision(&self, state: heddle_object_model::object::StateId) -> wire::RevisionRef {
        wire::RevisionRef {
            spool: self.thread.spool.clone(),
            revision: Some(wire::revision_ref::Revision::State(
                api::heddle::api::v1alpha1::StateId {
                    value: state.as_bytes().to_vec(),
                },
            )),
        }
    }
    pub fn set_sharing(&self) -> Result<wire::SetThreadSharingRequest, Error> {
        let Control::Sharing(value) = &self.control.control else {
            return Err(Error::Protocol("control is not sharing"));
        };
        Ok(wire::SetThreadSharingRequest {
            client_operation_id: self.control.client_operation_id.to_string(),
            expected_policy_version: self.property_version.clone(),
            operation: Some(self.record.clone()),
            policy: Some(wire::ThreadSharingPolicy {
                thread: Some(self.thread.clone()),
                version: vec![],
                ongoing: value.ongoing,
                destinations: value
                    .destinations
                    .iter()
                    .map(|destination| wire::SharingDestination {
                        endpoint: Some(wire::EndpointRef {
                            public_key: destination.endpoint.to_vec(),
                            kind: match destination.kind {
                                EndpointKind::Device => wire::EndpointKind::Device,
                                EndpointKind::Weft => wire::EndpointKind::Weft,
                            } as i32,
                        }),
                        spool: Some(wire::SpoolRef {
                            id: destination.spool.to_string(),
                        }),
                        facets: destination
                            .facets
                            .iter()
                            .map(|facet| match facet {
                                SharedFacet::Source => wire::SharedFacet::Source,
                                SharedFacet::Collaboration => wire::SharedFacet::Collaboration,
                                SharedFacet::Evidence => wire::SharedFacet::Evidence,
                                SharedFacet::ScrubbedTimeline => {
                                    wire::SharedFacet::ScrubbedTimeline
                                }
                                SharedFacet::Metadata => wire::SharedFacet::Metadata,
                            } as i32)
                            .collect(),
                    })
                    .collect(),
            }),
        })
    }
}

pub fn property_key(property: &Property) -> (wire::ThreadProperty, String) {
    match property {
        Property::Name => (wire::ThreadProperty::Name, String::new()),
        Property::Intent => (wire::ThreadProperty::Intent, String::new()),
        Property::Lifecycle => (wire::ThreadProperty::Lifecycle, String::new()),
        Property::Sharing => (wire::ThreadProperty::Sharing, String::new()),
        Property::Review(id) => (wire::ThreadProperty::Review, id.to_string()),
    }
}
fn hash(bytes: &[u8]) -> Result<ContentHash, Error> {
    Ok(ContentHash::from_bytes(bytes.try_into().map_err(|_| {
        Error::Protocol("Thread hash must be 32 bytes")
    })?))
}
fn io_error(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
}

#[cfg(test)]
#[path = "thread_control_tests.rs"]
mod tests;
