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
            || self.spool.is_empty()
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "canonical", rename_all = "snake_case")]
pub enum ThreadOperationBody {
    Capture(Vec<u8>),
    Discussion(Vec<u8>),
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
            ThreadOperationBody::Discussion(_) => ThreadFacet::Discussion,
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
            ThreadOperationBody::Discussion(bytes) => {
                let decoded = CollaborationOperationEnvelope::decode(bytes).map_err(invalid)?;
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
            ThreadOperationBody::Discussion(bytes) => {
                let operation = CollaborationOperationEnvelope::decode(bytes).map_err(invalid)?;
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
