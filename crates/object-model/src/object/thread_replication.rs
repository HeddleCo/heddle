// SPDX-License-Identifier: Apache-2.0
//! Portable Thread identity and immutable replication operations. Source and
//! discussion causality have separate graphs so selective sharing is closed.
pub mod hosted_import;
pub mod integration;
pub mod local_integration;
pub mod metadata;
pub mod ownership_claim;
pub mod source_author;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
pub use source_author::{AuthoredCapture, SOURCE_AUTHORIZATION_METHOD, SourceAuthor};

use crate::{
    error::{HeddleError, Result},
    object::{CollaborationOperationEnvelope, ContentHash, State, StateId},
};

pub const GENESIS_FORMAT: &str = "heddle-thread-genesis-v1";
pub const OPERATION_FORMAT: &str = "heddle-thread-operation-v1";
pub const MAX_OPERATION_BYTES: usize = 256 * 1024;

/// Ownership fixed by the original signed genesis. Hosting a key-owned Thread
/// requires a separately verified, explicit ownership claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenesisOwner {
    LocalKey([u8; 32]),
    Account(uuid::Uuid),
}

impl GenesisOwner {
    fn is_valid(&self) -> bool {
        match self {
            Self::LocalKey(key) => *key != [0; 32],
            Self::Account(account) => !account.is_nil(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadGenesis {
    pub version: u16,
    /// Canonical non-nil UUID shared by local and hosted replicas. Mutable
    /// namespace/name addresses and native spool-link locators are not identity.
    pub spool: String,
    pub parent: Option<ContentHash>,
    pub base: StateId,
    pub name: String,
    pub intent: String,
    /// Immutable original ownership; uploading does not transfer it.
    pub owner: GenesisOwner,
    pub creator: [u8; 32],
    pub nonce: Vec<u8>,
}

impl ThreadGenesis {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1
            || !self.owner.is_valid()
            || matches!(&self.owner, GenesisOwner::LocalKey(key) if *key != self.creator)
            || !uuid::Uuid::parse_str(&self.spool)
                .is_ok_and(|id| !id.is_nil() && id.to_string() == self.spool)
            || self.name.is_empty()
            || self.nonce.len() > 64
        {
            return Err(invalid("invalid Thread genesis"));
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        bounded(&bytes)?;
        Ok(bytes)
    }

    pub fn id(&self) -> Result<ContentHash> {
        Ok(ContentHash::compute_typed(GENESIS_FORMAT, &self.encode()?))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bounded(bytes)?;
        let genesis: Self = rmp_serde::from_slice(bytes)?;
        if genesis.encode()? != bytes {
            return Err(invalid("non-canonical Thread genesis"));
        }
        Ok(genesis)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadFacet {
    Source,
    Discussion,
    Metadata,
}
impl ThreadFacet {
    pub const ALL: [Self; 3] = [Self::Source, Self::Discussion, Self::Metadata];
}

/// Durable causal admission. Receiving bytes alone does not accept an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    Pending,
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "canonical", rename_all = "snake_case")]
pub enum ThreadOperationBody {
    Capture(AuthoredCapture),
    Integration(Vec<u8>),
    HostedImport(Vec<u8>),
    LocalIntegration(Vec<u8>),
    Discussion(Vec<u8>),
    Context(Vec<u8>),
    Metadata(Vec<u8>),
}

/// Source result shared by authored captures and executor-derived integrations.
/// Per-Thread reference metadata never changes State identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub state: Vec<u8>,
    pub source_targets: Option<ContentHash>,
}
impl From<Vec<u8>> for Capture {
    fn from(state: Vec<u8>) -> Self {
        Self {
            state,
            source_targets: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadOperation {
    pub version: u16,
    pub thread: ContentHash,
    pub parents: BTreeSet<ContentHash>,
    /// Signing key of the publisher of this immutable operation. Attribution
    /// inside a source/discussion object is retained, not promoted to authority.
    pub publisher: [u8; 32],
    pub body: ThreadOperationBody,
}

impl ThreadOperation {
    pub fn reference_proof(
        &self,
        genesis: &ThreadGenesis,
    ) -> Result<Option<crate::object::source_target::capture::ReferenceProof>> {
        let Some(capture) = self.source_result()? else {
            return Ok(None);
        };
        let Some(descriptor) = capture.source_targets else {
            return Ok(None);
        };
        if self.thread != genesis.id()? {
            return Err(invalid("capture reference Thread mismatch"));
        }
        Ok(Some(
            crate::object::source_target::capture::ReferenceProof {
                descriptor,
                scope: crate::object::CollaborationScope {
                    spool: genesis.spool.parse().map_err(invalid)?,
                    thread: Some(self.thread),
                },
                state: State::decode_current_msgpack(&capture.state)?.id(),
            },
        ))
    }
    pub fn facet(&self) -> ThreadFacet {
        match self.body {
            ThreadOperationBody::Capture(_)
            | ThreadOperationBody::Integration(_)
            | ThreadOperationBody::HostedImport(_)
            | ThreadOperationBody::LocalIntegration(_) => ThreadFacet::Source,
            ThreadOperationBody::Metadata(_) => ThreadFacet::Metadata,
            ThreadOperationBody::Discussion(_) | ThreadOperationBody::Context(_) => {
                ThreadFacet::Discussion
            }
        }
    }

    /// Uniform source result for capture and both integration authorities.
    /// Original authored source identity. Hosted execution uses its executor proof.
    pub fn source_author(&self) -> Result<Option<SourceAuthor>> {
        if let ThreadOperationBody::Capture(capture) = &self.body {
            return Ok(Some(capture.author.clone()));
        }
        Ok(self
            .local_integration()?
            .map(|integration| integration.author))
    }

    pub fn source_result(&self) -> Result<Option<Capture>> {
        match &self.body {
            ThreadOperationBody::Capture(capture) => Ok(Some(capture.result.clone())),
            ThreadOperationBody::Integration(bytes) => {
                Ok(Some(integration::HostedIntegration::decode(bytes)?.result))
            }
            ThreadOperationBody::HostedImport(bytes) => {
                Ok(Some(hosted_import::HostedImport::decode(bytes)?.result))
            }
            ThreadOperationBody::LocalIntegration(bytes) => Ok(Some(
                local_integration::LocalIntegration::decode(bytes)?.result,
            )),
            _ => Ok(None),
        }
    }

    /// The exact source revision represented by a capture or hosted integration.
    pub fn source_state(&self) -> Result<Option<State>> {
        match &self.body {
            ThreadOperationBody::Capture(bytes) => {
                State::decode_current_msgpack(&bytes.result.state).map(Some)
            }
            ThreadOperationBody::Integration(bytes) => {
                integration::HostedIntegration::decode(bytes)?
                    .resulting_state()
                    .map(Some)
            }
            ThreadOperationBody::LocalIntegration(bytes) => {
                local_integration::LocalIntegration::decode(bytes)?
                    .resulting_state()
                    .map(Some)
            }
            ThreadOperationBody::HostedImport(bytes) => hosted_import::HostedImport::decode(bytes)?
                .resulting_state()
                .map(Some),
            _ => Ok(None),
        }
    }
    pub fn local_integration(&self) -> Result<Option<local_integration::LocalIntegration>> {
        match &self.body {
            ThreadOperationBody::LocalIntegration(bytes) => {
                local_integration::LocalIntegration::decode(bytes).map(Some)
            }
            _ => Ok(None),
        }
    }
    pub fn integration(&self) -> Result<Option<integration::HostedIntegration>> {
        match &self.body {
            ThreadOperationBody::Integration(bytes) => {
                integration::HostedIntegration::decode(bytes).map(Some)
            }
            _ => Ok(None),
        }
    }

    pub fn hosted_import(&self) -> Result<Option<hosted_import::HostedImport>> {
        match &self.body {
            ThreadOperationBody::HostedImport(bytes) => {
                hosted_import::HostedImport::decode(bytes).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// A context root may be extracted atomically by a discussion resolution.
    /// Later context revisions retain that original signed resolution as parent.
    pub fn context_revision(&self) -> Result<Option<crate::object::ContextRevision>> {
        use crate::object::{CollaborationOperationBodyV1 as Body, CollaborationResolution};
        match &self.body {
            ThreadOperationBody::Context(bytes) => crate::object::ContextRevision::decode(bytes)
                .map(Some)
                .map_err(invalid),
            ThreadOperationBody::Discussion(bytes) => {
                let record = CollaborationOperationEnvelope::decode(bytes)
                    .map_err(invalid)?
                    .operation;
                match record.body {
                    Body::Resolve {
                        resolution: CollaborationResolution::IntoContext { context },
                    } => Ok(Some(context)),
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1 {
            return Err(invalid("unsupported Thread operation version"));
        }
        match &self.body {
            ThreadOperationBody::Capture(bytes) => {
                bytes.author.validate()?;
                let state = State::decode_current_msgpack(&bytes.result.state)?;
                if state.encode_current_msgpack()? != bytes.result.state {
                    return Err(invalid("non-canonical capture"));
                }
            }
            ThreadOperationBody::Integration(bytes) => {
                integration::HostedIntegration::decode(bytes)?.validate_operation(self)?;
            }
            ThreadOperationBody::HostedImport(bytes) => {
                hosted_import::HostedImport::decode(bytes)?.validate_operation(self)?
            }
            ThreadOperationBody::LocalIntegration(bytes) => {
                local_integration::LocalIntegration::decode(bytes)?.validate_operation(self)?;
            }
            ThreadOperationBody::Metadata(bytes) => {
                metadata::ThreadControl::decode(bytes)?.validate_operation(self)?;
            }
            ThreadOperationBody::Context(bytes) => {
                let context = crate::object::ContextRevision::decode(bytes).map_err(invalid)?;
                if context.metadata.scope.thread != Some(self.thread)
                    || context.parents.iter().copied().collect::<BTreeSet<_>>() != self.parents
                {
                    return Err(invalid(
                        "context scope or causal parents differ from Thread operation",
                    ));
                }
            }
            ThreadOperationBody::Discussion(bytes) => {
                let decoded = CollaborationOperationEnvelope::decode(bytes).map_err(invalid)?;
                if decoded
                    .operation
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m.scope.thread != Some(self.thread))
                {
                    return Err(invalid("collaboration metadata belongs to another Thread"));
                }
                if let Some(context) = self.context_revision()? {
                    if Some(&context.metadata) != decoded.operation.metadata.as_ref()
                        || context.extracted_from != Some(decoded.operation.discussion_id)
                        || context.parents.iter().copied().collect::<BTreeSet<_>>() != self.parents
                    {
                        return Err(invalid(
                            "extracted context differs from signed discussion actor, scope or parents",
                        ));
                    }
                }
                if decoded.operation.encode().map_err(invalid)? != *bytes {
                    return Err(invalid("non-canonical discussion operation"));
                }
            }
        }
        let bytes = rmp_serde::to_vec_named(self)?;
        bounded(&bytes)?;
        Ok(bytes)
    }

    pub fn id(&self) -> Result<ContentHash> {
        Ok(ContentHash::compute_typed(
            OPERATION_FORMAT,
            &self.encode()?,
        ))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bounded(bytes)?;
        let operation: Self = rmp_serde::from_slice(bytes)?;
        if operation.encode()? != bytes {
            return Err(invalid("non-canonical Thread operation"));
        }
        Ok(operation)
    }

    /// Called after every parent is present. A peer cannot relabel a private
    /// dependency as public or attach an operation to a different Thread.
    pub fn validate_parents(&self, genesis: &ThreadGenesis, parents: &[Self]) -> Result<()> {
        if self.thread != genesis.id()? || parents.len() != self.parents.len() {
            return Err(invalid("Thread or causal parent set mismatch"));
        }
        let ids = parents
            .iter()
            .map(Self::id)
            .collect::<Result<BTreeSet<_>>>()?;
        if ids != self.parents
            || parents
                .iter()
                .any(|p| p.thread != self.thread || p.facet() != self.facet())
        {
            return Err(invalid("causal parents cross Thread or disclosure facet"));
        }
        if self
            .source_result()?
            .is_some_and(|result| result.source_targets.is_none())
        {
            for parent in parents {
                if parent
                    .source_result()?
                    .is_some_and(|result| result.source_targets.is_some())
                {
                    return Err(invalid(
                        "source evolution drops inherited reference closure",
                    ));
                }
            }
        }
        match &self.body {
            ThreadOperationBody::Capture(bytes) => {
                bytes.author.validate()?;
                if let SourceAuthor::Account { spool, .. } = &bytes.author {
                    if spool.to_string() != genesis.spool {
                        return Err(invalid("original source author crosses Spool scope"));
                    }
                }
                let state = State::decode_current_msgpack(&bytes.result.state)?;
                let mut source_parents = BTreeSet::new();
                for parent in parents {
                    source_parents.insert(
                        parent
                            .source_state()?
                            .ok_or_else(|| invalid("capture parent is not source"))?
                            .id(),
                    );
                }
                let declared: BTreeSet<_> = state
                    .parents
                    .iter()
                    .copied()
                    .filter(|id| *id != genesis.base)
                    .collect();
                if declared != source_parents
                    || state.parents.is_empty()
                    || state.parents.len() != state.parents.iter().collect::<BTreeSet<_>>().len()
                {
                    return Err(invalid(
                        "capture source ancestry differs from causal parents",
                    ));
                }
            }
            ThreadOperationBody::Integration(bytes) => {
                let receipt = integration::HostedIntegration::decode(bytes)?;
                receipt.validate_operation(self)?;
                receipt.validate_parents(genesis, parents)?;
            }
            ThreadOperationBody::HostedImport(bytes) => {
                let receipt = hosted_import::HostedImport::decode(bytes)?;
                receipt.validate_operation(self)?;
                receipt.validate_parents(genesis, parents)?;
            }
            ThreadOperationBody::LocalIntegration(bytes) => {
                let receipt = local_integration::LocalIntegration::decode(bytes)?;
                receipt.validate_operation(self)?;
                receipt.validate_parents(genesis, parents)?;
            }
            ThreadOperationBody::Metadata(bytes) => {
                metadata::ThreadControl::decode(bytes)?.validate_parents(genesis, parents)?;
            }
            ThreadOperationBody::Context(bytes) => {
                let context = crate::object::ContextRevision::decode(bytes).map_err(invalid)?;
                if context.metadata.scope.spool.to_string() != genesis.spool {
                    return Err(invalid("context belongs to another spool"));
                }
                if parents.is_empty() && context.extracted_from.is_some() {
                    return Err(invalid(
                        "context extraction requires a signed discussion resolution",
                    ));
                }
                for parent in parents {
                    let parent = parent.context_revision()?.ok_or_else(|| {
                        invalid("context parent is not a context revision or extraction")
                    })?;
                    if parent.id != context.id
                        || parent.metadata.scope != context.metadata.scope
                        || parent.extracted_from != context.extracted_from
                    {
                        return Err(invalid("context parents belong to another record or scope"));
                    }
                }
            }
            ThreadOperationBody::Discussion(bytes) => {
                let operation = CollaborationOperationEnvelope::decode(bytes).map_err(invalid)?;
                if operation
                    .operation
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m.scope.spool.to_string() != genesis.spool)
                {
                    return Err(invalid("collaboration metadata belongs to another spool"));
                }
                let mut discussion_parents = BTreeSet::new();
                for parent in parents {
                    let ThreadOperationBody::Discussion(bytes) = &parent.body else {
                        return Err(invalid("discussion parent is not discussion"));
                    };
                    let parent = CollaborationOperationEnvelope::decode(bytes).map_err(invalid)?;
                    if parent.operation.discussion_id != operation.operation.discussion_id {
                        return Err(invalid("parents cross discussions"));
                    }
                    discussion_parents.insert(parent.operation_id);
                }
                if discussion_parents != operation.operation.parents.iter().copied().collect() {
                    return Err(invalid(
                        "discussion causality differs from its canonical operation",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn bounded(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_OPERATION_BYTES {
        return Err(invalid("Thread record exceeds the durable record bound"));
    }
    Ok(())
}

fn invalid(message: impl std::fmt::Display) -> HeddleError {
    HeddleError::InvalidObject(message.to_string())
}

#[cfg(test)]
mod capture_shape_tests {
    use super::*;
    use crate::object::{Attribution, Principal, Tree};
    #[test]
    fn byte_only_capture_shape_is_rejected_in_clean_cutover() {
        #[derive(serde::Serialize)]
        struct OldBody {
            kind: &'static str,
            canonical: Vec<u8>,
        }
        #[derive(serde::Serialize)]
        struct OldOperation {
            version: u16,
            thread: ContentHash,
            parents: BTreeSet<ContentHash>,
            publisher: [u8; 32],
            body: OldBody,
        }
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![],
            Attribution::human(Principal::new("author", "")),
        );
        let bytes = rmp_serde::to_vec_named(&OldOperation {
            version: 1,
            thread: ContentHash::from_bytes([1; 32]),
            parents: BTreeSet::new(),
            publisher: [41; 32],
            body: OldBody {
                kind: "capture",
                canonical: state.encode_current_msgpack().expect("state"),
            },
        })
        .expect("old wire bytes");
        assert!(
            ThreadOperation::decode(&bytes).is_err(),
            "capture has one typed shape, without a legacy byte decoder"
        );
    }
}
