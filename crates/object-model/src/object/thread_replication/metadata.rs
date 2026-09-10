//! Caller-signed Thread controls with independent causal registers. Concurrent
//! writes to one property remain visible candidates; no arrival-time winner is
//! selected. Controls describe intent and policy, never grant recipient rights.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod audience;
pub mod retention;

use super::{MAX_OPERATION_BYTES, ThreadGenesis, ThreadOperation, ThreadOperationBody, invalid};
use crate::{
    error::Result,
    object::{CollaborationActor, ContentHash, StateId},
};

pub const CONTROL_FORMAT: &str = "heddle-thread-control-v1";
pub const PROPERTY_VERSION_FORMAT: &str = "heddle-thread-property-v1";
pub const AUTHORITY_FORMAT: &str = "heddle-thread-control-authority-v1";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Property {
    Name,
    Intent,
    Lifecycle,
    Sharing,
    Audience,
    Retention,
    Review(Uuid),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub outcome: String,
    pub acceptance_criteria: Vec<String>,
    pub origin_urls: Vec<String>,
    pub principal_approved: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Draft,
    Active,
    Ready,
    Abandoned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedFacet {
    Source,
    Collaboration,
    Evidence,
    ScrubbedTimeline,
    Metadata,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    Device,
    Weft,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub endpoint: [u8; 32],
    pub kind: EndpointKind,
    pub spool: Uuid,
    pub facets: BTreeSet<SharedFacet>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharingPolicy {
    pub ongoing: bool,
    pub destinations: Vec<Destination>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Opinion,
    Approval,
    Rejection,
    Revocation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub id: Uuid,
    pub source: StateId,
    pub target: StateId,
    pub policy_version: ContentHash,
    pub kind: ReviewKind,
    pub explanation: String,
    pub revokes: Option<Uuid>,
    pub expires_at_unix_seconds: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Control {
    Name(String),
    Intent(Intent),
    Lifecycle(Lifecycle),
    Sharing(SharingPolicy),
    Audience(audience::Audience),
    Retention(retention::RetentionPolicy),
    Review(Review),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadControl {
    pub version: u16,
    pub spool: Uuid,
    pub actor: CollaborationActor,
    /// Portable original authority, independently checked by the admitting host.
    /// The envelope grants nothing merely because its digest is well formed.
    pub authority_digest: ContentHash,
    pub authority_envelope: Vec<u8>,
    pub client_operation_id: Uuid,
    pub occurred_at_ms: i64,
    pub control: Control,
}
impl ThreadControl {
    /// Exact mutation whose original authority must be checked at admission.
    pub fn authorization_method(&self) -> &'static str {
        match self.control {
            Control::Name(_) => "/heddle.api.v2alpha1.ThreadService/RenameThread",
            Control::Intent(_) => "/heddle.api.v2alpha1.ThreadService/ReviseIntent",
            Control::Lifecycle(_) => "/heddle.api.v2alpha1.ThreadService/ChangeLifecycle",
            Control::Sharing(_) => "/heddle.api.v2alpha1.ThreadService/SetSharingPolicy",
            Control::Audience(_) => "/heddle.api.v2alpha1.ThreadService/SetAudiencePolicy",
            Control::Retention(_) => "/heddle.api.v2alpha1.ThreadService/SetRetentionPolicy",
            Control::Review(_) => "/heddle.api.v2alpha1.ThreadService/RecordReview",
        }
    }
    pub fn property(&self) -> Property {
        match &self.control {
            Control::Name(_) => Property::Name,
            Control::Intent(_) => Property::Intent,
            Control::Lifecycle(_) => Property::Lifecycle,
            Control::Sharing(_) => Property::Sharing,
            Control::Audience(_) => Property::Audience,
            Control::Retention(_) => Property::Retention,
            Control::Review(review) => Property::Review(review.id),
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1
            || self.spool.is_nil()
            || self.actor.principal_id.is_nil()
            || self.client_operation_id.is_nil()
            || self.occurred_at_ms < 0
        {
            return Err(invalid("invalid Thread control identity"));
        }
        if self.authority_envelope.is_empty()
            || self.authority_envelope.len() > 64 * 1024
            || ContentHash::compute_typed(AUTHORITY_FORMAT, &self.authority_envelope)
                != self.authority_digest
        {
            return Err(invalid("invalid Thread control authority binding"));
        }
        if let Some(agent) = &self.actor.agent_id {
            text(agent, 256, false)?;
        }
        match &self.control {
            Control::Name(name) => text(name, 1024, false)?,
            Control::Intent(intent) => {
                text(&intent.outcome, 32768, false)?;
                if intent.acceptance_criteria.len() > 64
                    || intent.origin_urls.len() > 64
                    || (intent.principal_approved && self.actor.agent_id.is_some())
                {
                    return Err(invalid("invalid Thread intent approval or bounds"));
                }
                for criterion in &intent.acceptance_criteria {
                    text(criterion, 4096, false)?;
                }
                for origin in &intent.origin_urls {
                    text(origin, 4096, false)?;
                }
            }
            Control::Lifecycle(_) => {}
            Control::Audience(policy) => policy.validate()?,
            Control::Retention(policy) => policy.validate()?,
            Control::Sharing(policy) => {
                if policy.destinations.len() > 64 {
                    return Err(invalid("Thread sharing destination bound"));
                }
                let mut seen = BTreeSet::new();
                for destination in &policy.destinations {
                    if destination.spool.is_nil()
                        || destination.endpoint == [0; 32]
                        || destination.facets.is_empty()
                        || !seen.insert((destination.endpoint, destination.spool))
                    {
                        return Err(invalid("invalid or duplicate Thread sharing destination"));
                    }
                }
            }
            Control::Review(review) => {
                text(&review.explanation, 32768, true)?;
                if review.id.is_nil()
                    || review
                        .revokes
                        .is_some_and(|id| id.is_nil() || id == review.id)
                    || (review.kind == ReviewKind::Revocation) != review.revokes.is_some()
                    || review
                        .expires_at_unix_seconds
                        .is_some_and(|time| time <= self.occurred_at_ms / 1000)
                {
                    return Err(invalid("invalid Thread review decision"));
                }
            }
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        if bytes.len() > MAX_OPERATION_BYTES / 2 {
            return Err(invalid("Thread control exceeds record budget"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_OPERATION_BYTES / 2 {
            return Err(invalid("Thread control exceeds record budget"));
        }
        let value: Self = rmp_serde::from_slice(bytes)?;
        if value.encode()? != bytes {
            return Err(invalid("non-canonical Thread control"));
        }
        Ok(value)
    }
    pub fn validate_operation(&self, operation: &ThreadOperation) -> Result<()> {
        if operation.parents.len() > 128 {
            return Err(invalid("Thread property frontier exceeds budget"));
        }
        let ThreadOperationBody::Metadata(bytes) = &operation.body else {
            return Err(invalid("Thread control requires metadata operation"));
        };
        if self.encode()? != *bytes {
            return Err(invalid("Thread control differs from signed operation"));
        }
        Ok(())
    }
    pub fn validate_parents(
        &self,
        genesis: &ThreadGenesis,
        parents: &[ThreadOperation],
    ) -> Result<()> {
        if self.spool.to_string() != genesis.spool {
            return Err(invalid("Thread control belongs to another Spool"));
        }
        for parent in parents {
            let ThreadOperationBody::Metadata(bytes) = &parent.body else {
                return Err(invalid("Thread property parent is not metadata"));
            };
            let parent = Self::decode(bytes)?;
            if parent.spool != self.spool || parent.property() != self.property() {
                return Err(invalid("Thread property causal parents cross fields"));
            }
            if matches!(self.control, Control::Review(_)) && parent.actor != self.actor {
                return Err(invalid("review successor changes original actor"));
            }
        }
        Ok(())
    }
}
/// A field's exact version commits its scope and every concurrent candidate.
/// Genesis/default values have an empty frontier, with a stable nonempty version.
pub fn property_version(
    thread: ContentHash,
    property: &Property,
    heads: &BTreeSet<ContentHash>,
) -> Result<ContentHash> {
    if heads.len() > 128 {
        return Err(invalid("Thread property frontier exceeds budget"));
    }
    Ok(ContentHash::compute_typed(
        PROPERTY_VERSION_FORMAT,
        &rmp_serde::to_vec_named(&(thread, property, heads))?,
    ))
}
fn text(value: &str, max: usize, empty: bool) -> Result<()> {
    if value.len() > max || (!empty && value.trim().is_empty()) || value.contains('\0') {
        return Err(invalid("invalid Thread control text"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ThreadGenesis, ThreadControl) {
        let spool = Uuid::from_u128(1);
        (
            ThreadGenesis {
                version: 1,
                spool: spool.to_string(),
                parent: None,
                base: StateId::from_bytes([1; 32]),
                name: "thread".into(),
                intent: "intent".into(),
                creator: [3; 32],
                owner: super::super::GenesisOwner::Account(Uuid::from_u128(2)),
                nonce: vec![],
            },
            ThreadControl {
                version: 1,
                spool,
                actor: CollaborationActor {
                    principal_id: Uuid::from_u128(2),
                    agent_id: None,
                },
                authority_digest: ContentHash::compute_typed(
                    AUTHORITY_FORMAT,
                    b"codec fixture, independently verified at admission",
                ),
                authority_envelope: b"codec fixture, independently verified at admission".to_vec(),
                client_operation_id: Uuid::from_u128(3),
                occurred_at_ms: 1000,
                control: Control::Name("new name".into()),
            },
        )
    }
    fn operation(
        genesis: &ThreadGenesis,
        value: &ThreadControl,
        parents: BTreeSet<ContentHash>,
    ) -> ThreadOperation {
        ThreadOperation {
            version: 1,
            thread: genesis.id().expect("Thread"),
            parents,
            publisher: [3; 32],
            body: ThreadOperationBody::Metadata(value.encode().expect("control")),
        }
    }
    #[test]
    fn canonical_control_retains_actor_and_value_in_original_operation_identity() {
        let (genesis, value) = fixture();
        let original = operation(&genesis, &value, BTreeSet::new());
        let encoded = original.encode().expect("canonical operation");
        assert_eq!(ThreadOperation::decode(&encoded).expect("decode"), original);
        assert_eq!(original.facet(), super::super::ThreadFacet::Metadata);
        original
            .validate_parents(&genesis, &[])
            .expect("initial property");
        let mut changed = value;
        changed.actor.principal_id = Uuid::from_u128(4);
        assert_ne!(
            original.id().expect("ID"),
            operation(&genesis, &changed, BTreeSet::new())
                .id()
                .expect("changed actor")
        );
        changed.control = Control::Name("another value".into());
        assert_ne!(
            original.id().expect("ID"),
            operation(&genesis, &changed, BTreeSet::new())
                .id()
                .expect("changed value")
        );
    }
    #[test]
    fn independent_properties_cannot_causally_erase_each_other() {
        let (genesis, mut value) = fixture();
        let name = operation(&genesis, &value, BTreeSet::new());
        let name_id = name.id().expect("name ID");
        value.control = Control::Lifecycle(Lifecycle::Active);
        let lifecycle = operation(&genesis, &value, BTreeSet::from([name_id]));
        assert!(
            lifecycle
                .validate_parents(&genesis, std::slice::from_ref(&name))
                .expect_err("cannot dominate another field")
                .to_string()
                .contains("cross fields")
        );
        value.control = Control::Name("resolved name".into());
        operation(&genesis, &value, BTreeSet::from([name_id]))
            .validate_parents(&genesis, &[name])
            .expect("same property successor");
    }
    #[test]
    fn property_version_binds_every_concurrent_candidate_and_exact_field() {
        let (genesis, value) = fixture();
        let a = operation(&genesis, &value, BTreeSet::new());
        let mut other = value;
        other.control = Control::Name("parallel".into());
        let b = operation(&genesis, &other, BTreeSet::new());
        let heads = BTreeSet::from([a.id().expect("a"), b.id().expect("b")]);
        let version = property_version(a.thread, &Property::Name, &heads).expect("version");
        assert_ne!(
            version,
            property_version(
                a.thread,
                &Property::Name,
                &BTreeSet::from([a.id().expect("a")])
            )
            .expect("incomplete frontier")
        );
        assert_ne!(
            version,
            property_version(a.thread, &Property::Intent, &heads).expect("other field")
        );
        operation(&genesis, &other, heads)
            .validate_parents(&genesis, &[b, a])
            .expect("explicit resolution retains both observed parents");
    }
    #[test]
    fn bounded_controls_cannot_claim_principal_approval_for_agent_attribution() {
        let (_, mut value) = fixture();
        value.control = Control::Intent(Intent {
            outcome: "goal".into(),
            acceptance_criteria: vec![],
            origin_urls: vec![],
            principal_approved: true,
        });
        value.actor.agent_id = Some("delegated agent".into());
        assert!(
            value
                .encode()
                .expect_err("human approval is an explicit actor assertion")
                .to_string()
                .contains("approval")
        );
        value.actor.agent_id = None;
        value.encode().expect("principal statement");
        value.control = Control::Name("n".repeat(1025));
        assert!(
            value
                .encode()
                .expect_err("name limit")
                .to_string()
                .contains("text")
        );
    }
    #[test]
    fn original_authority_envelope_is_bound_by_the_signed_control() {
        let (_, mut control) = fixture();
        control.authority_envelope.push(1);
        assert!(
            control
                .encode()
                .expect_err("substituted proof must fail before signing")
                .to_string()
                .contains("authority binding")
        );
    }
    #[test]
    fn review_successor_retains_exact_original_human_or_agent_actor() {
        let (genesis, mut value) = fixture();
        value.control = Control::Review(Review {
            id: Uuid::from_u128(5),
            source: StateId::from_bytes([7; 32]),
            target: StateId::from_bytes([8; 32]),
            policy_version: ContentHash::from_bytes([9; 32]),
            kind: ReviewKind::Approval,
            explanation: "original decision".into(),
            revokes: None,
            expires_at_unix_seconds: None,
        });
        let original = operation(&genesis, &value, BTreeSet::new());
        let parent = BTreeSet::from([original.id().expect("review ID")]);
        value.actor.agent_id = Some("delegated agent".into());
        assert!(
            operation(&genesis, &value, parent.clone())
                .validate_parents(&genesis, std::slice::from_ref(&original))
                .expect_err("agent cannot rewrite human decision")
                .to_string()
                .contains("original actor")
        );
        value.actor.agent_id = None;
        operation(&genesis, &value, parent)
            .validate_parents(&genesis, &[original])
            .expect("same accountable reviewer");
    }
}
