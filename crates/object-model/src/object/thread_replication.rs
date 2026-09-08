// SPDX-License-Identifier: Apache-2.0
//! Portable Thread identity and immutable replication operations. Source and
//! discussion causality have separate graphs so selective sharing is closed.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    error::{HeddleError, Result},
    object::{CollaborationOperationEnvelope, ContentHash, State, StateId},
};

pub const GENESIS_FORMAT: &str = "heddle-thread-genesis-v1";
pub const OPERATION_FORMAT: &str = "heddle-thread-operation-v1";
pub const MAX_OPERATION_BYTES: usize = 256 * 1024;

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
    pub creator: [u8; 32],
    pub nonce: Vec<u8>,
}

impl ThreadGenesis {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1
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
    Capture(Vec<u8>),
    Discussion(Vec<u8>),
    Context(Vec<u8>),
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
    pub fn facet(&self) -> ThreadFacet {
        match self.body {
            ThreadOperationBody::Capture(_) => ThreadFacet::Source,
            ThreadOperationBody::Discussion(_) | ThreadOperationBody::Context(_) => {
                ThreadFacet::Discussion
            }
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
                let state = State::decode_current_msgpack(bytes)?;
                if state.encode_current_msgpack()? != *bytes {
                    return Err(invalid("non-canonical capture"));
                }
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
        match &self.body {
            ThreadOperationBody::Capture(bytes) => {
                let state = State::decode_current_msgpack(bytes)?;
                let mut source_parents = BTreeSet::new();
                for parent in parents {
                    let ThreadOperationBody::Capture(bytes) = &parent.body else {
                        return Err(invalid("capture parent is not source"));
                    };
                    source_parents.insert(State::decode_current_msgpack(bytes)?.id());
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
