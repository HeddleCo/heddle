// SPDX-License-Identifier: Apache-2.0
//! Deterministic functions of immutable state input and ordered parent values.
//!
//! Implementations are compile-time Rust types. Changing an algorithm, codec or
//! immutable configuration requires a new descriptor version or id. Stores retain
//! every node reachable from a binding; successful bindings are insert-once.
//! Scheduling, authorization and mutable repository policy are outside this contract.

use super::{ContentHash, ObjectSource, StateId};

pub type DerivedResult<T> = Result<T, DerivedError>;

#[derive(Debug, thiserror::Error)]
pub enum DerivedError {
    #[error("missing derivation input for {0}")]
    MissingState(StateId),
    #[error("missing derived node {0}")]
    MissingValue(ContentHash),
    #[error("cycle in derivation inputs at {0}")]
    Cycle(StateId),
    #[error("derivation budget exhausted: {0}")]
    Limit(&'static str),
    #[error("corrupt derived data: {0}")]
    Corrupt(&'static str),
    #[error("immutable derived-data conflict")]
    Conflict,
    #[error("derived-data storage: {0}")]
    Storage(String),
    #[error("derivation input: {0}")]
    Input(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub id: ContentHash,
    pub version: u32,
}

impl Descriptor {
    pub fn new(name: &str, version: u32) -> Self {
        Self {
            id: ContentHash::compute_typed("derivation-id", name.as_bytes()),
            version,
        }
    }

    pub fn key(self, state: StateId) -> DerivedKey {
        DerivedKey {
            state,
            id: self.id,
            version: self.version,
        }
    }
}

/// Full immutable identity. Parameterized derivations must include their
/// canonical immutable configuration in `id`, or use a separate version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DerivedKey {
    pub state: StateId,
    pub id: ContentHash,
    pub version: u32,
}

impl DerivedKey {
    pub fn encode(self) -> [u8; 68] {
        let mut bytes = [0; 68];
        bytes[..32].copy_from_slice(self.state.as_bytes());
        bytes[32..64].copy_from_slice(self.id.as_bytes());
        bytes[64..].copy_from_slice(&self.version.to_be_bytes());
        bytes
    }

    fn decode(cursor: &mut Cursor<'_>) -> DerivedResult<Self> {
        Ok(Self {
            state: StateId::from_bytes(cursor.array()?),
            id: ContentHash::from_bytes(cursor.array()?),
            version: u32::from_be_bytes(cursor.array()?),
        })
    }
}

/// Exact local change plus recorded parent order. `change` is interpreted by
/// the derivation, never by the executor; roots use an empty parent sequence.
#[derive(Debug)]
pub struct DerivedInput {
    pub state: StateId,
    pub parents: Vec<StateId>,
    pub change: Vec<u8>,
}

impl DerivedInput {
    pub fn check_limits(&self, max_bytes: usize, max_parents: usize) -> DerivedResult<()> {
        if self.parents.len() > max_parents {
            return Err(DerivedError::Limit("parents"));
        }
        if self.encoded_len() > max_bytes {
            return Err(DerivedError::Limit("input bytes"));
        }
        Ok(())
    }

    pub fn encoded_len(&self) -> usize {
        36usize
            .saturating_add(self.parents.len().saturating_mul(32))
            .saturating_add(self.change.len())
    }

    pub fn encode(&self) -> DerivedResult<Vec<u8>> {
        let count =
            u32::try_from(self.parents.len()).map_err(|_| DerivedError::Limit("parents"))?;
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(self.state.as_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        for parent in &self.parents {
            out.extend_from_slice(parent.as_bytes());
        }
        out.extend_from_slice(&self.change);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> DerivedResult<Self> {
        let mut cursor = Cursor(bytes);
        let state = StateId::from_bytes(cursor.array()?);
        let count = u32::from_be_bytes(cursor.array()?) as usize;
        if count > cursor.0.len() / 32 {
            return Err(DerivedError::Corrupt("input parent count"));
        }
        let mut parents = Vec::with_capacity(count);
        for _ in 0..count {
            parents.push(StateId::from_bytes(cursor.array()?));
        }
        Ok(Self {
            state,
            parents,
            change: cursor.0.to_vec(),
        })
    }
}

/// Sources enforce the supplied limits before allocating their returned input.
/// Source-internal decoding/caches are owned and bounded by the source itself.
pub trait DerivedSource {
    fn input(
        &self,
        state: StateId,
        max_bytes: usize,
        max_parents: usize,
    ) -> DerivedResult<Option<DerivedInput>>;
}

/// Project a verified native state to its tree identity and ordered parents.
/// The underlying ObjectSource owns the memory used to decode the full state.
pub struct StateGraphSource<'a, S: ?Sized>(pub &'a S);

impl<S: ObjectSource + ?Sized> DerivedSource for StateGraphSource<'_, S> {
    fn input(
        &self,
        id: StateId,
        max_bytes: usize,
        max_parents: usize,
    ) -> DerivedResult<Option<DerivedInput>> {
        let Some(state) = self
            .0
            .get_state(&id)
            .map_err(|e| DerivedError::Input(e.to_string()))?
        else {
            return Ok(None);
        };
        if !state.accepts_stored_id(&id) {
            return Err(DerivedError::Corrupt("source state identity"));
        }
        let input = DerivedInput {
            state: id,
            parents: state.parents,
            change: state.tree.as_bytes().to_vec(),
        };
        input.check_limits(max_bytes, max_parents)?;
        Ok(Some(input))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParentValue {
    pub key: DerivedKey,
    pub root: ContentHash,
}

/// A canonical result node, sharing parent subgraphs by hash. Its envelope
/// includes the full key and dependency keys/roots, so a codec/version mix is
/// rejected before composition. Value bytes are canonical per derivation.
#[derive(Debug, PartialEq, Eq)]
pub struct DerivedNode {
    pub key: DerivedKey,
    pub parents: Vec<ParentValue>,
    pub value: Vec<u8>,
}

impl DerivedNode {
    pub fn encoded_len(&self) -> usize {
        77usize
            .saturating_add(self.parents.len().saturating_mul(100))
            .saturating_add(self.value.len())
    }

    pub fn encode(&self) -> DerivedResult<Vec<u8>> {
        let count =
            u32::try_from(self.parents.len()).map_err(|_| DerivedError::Limit("parents"))?;
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(b"HDDN\x01");
        out.extend_from_slice(&self.key.encode());
        out.extend_from_slice(&count.to_be_bytes());
        for parent in &self.parents {
            out.extend_from_slice(&parent.key.encode());
            out.extend_from_slice(parent.root.as_bytes());
        }
        out.extend_from_slice(&self.value);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> DerivedResult<Self> {
        let mut cursor = Cursor(bytes);
        if cursor.array::<5>()? != *b"HDDN\x01" {
            return Err(DerivedError::Corrupt("node format"));
        }
        let key = DerivedKey::decode(&mut cursor)?;
        let count = u32::from_be_bytes(cursor.array()?) as usize;
        if count > cursor.0.len() / 100 {
            return Err(DerivedError::Corrupt("node parent count"));
        }
        let mut parents = Vec::with_capacity(count);
        for _ in 0..count {
            let parent_key = DerivedKey::decode(&mut cursor)?;
            if parent_key.id != key.id || parent_key.version != key.version {
                return Err(DerivedError::Corrupt("parent descriptor"));
            }
            parents.push(ParentValue {
                key: parent_key,
                root: ContentHash::from_bytes(cursor.array()?),
            });
        }
        Ok(Self {
            key,
            parents,
            value: cursor.0.to_vec(),
        })
    }

    pub fn hash(&self) -> DerivedResult<ContentHash> {
        Ok(node_hash(&self.encode()?))
    }

    pub fn check_key(&self, key: DerivedKey) -> DerivedResult<()> {
        if self.key != key {
            return Err(DerivedError::Corrupt("result identity/version"));
        }
        Ok(())
    }
}

pub fn node_hash(bytes: &[u8]) -> ContentHash {
    ContentHash::compute_typed("derived-node", bytes)
}

/// Hosts implement durable, immutable nodes and insert-once bindings. A
/// binding may be published only after its complete node DAG is durable.
/// Retain that DAG while the binding is live. Reads must enforce `max_bytes`
/// before allocating. Conflicting roots/bytes are errors, never overwrites.
/// Calls are synchronous; async hosts should run execution on blocking workers.
pub trait DerivedStore {
    fn lookup(&self, key: DerivedKey) -> DerivedResult<Option<ContentHash>>;
    fn get_node(&self, hash: ContentHash, max_bytes: usize) -> DerivedResult<Option<Vec<u8>>>;
    fn put_node_if_absent(&self, hash: ContentHash, bytes: &[u8]) -> DerivedResult<()>;
    fn bind_if_absent(&self, key: DerivedKey, root: ContentHash) -> DerivedResult<()>;
}

pub trait Derivation {
    fn descriptor(&self) -> Descriptor;
    /// Deterministic composition with parents in recorded order. Implementors
    /// enforce the output limit before allocation and bound their own scratch.
    fn compose(
        &self,
        input: &DerivedInput,
        parents: &[DerivedNode],
        max_bytes: usize,
    ) -> DerivedResult<Vec<u8>>;
}

/// State reachability membership. This is a shared graph of state identities,
/// not a dense ancestor set or a proof of tree/blob availability.
pub struct StateClosure;

impl Derivation for StateClosure {
    fn descriptor(&self) -> Descriptor {
        Descriptor::new("state-closure", 1)
    }

    fn compose(
        &self,
        input: &DerivedInput,
        _parents: &[DerivedNode],
        max_bytes: usize,
    ) -> DerivedResult<Vec<u8>> {
        if input.change.len() > max_bytes {
            return Err(DerivedError::Limit("result bytes"));
        }
        Ok(input.change.clone())
    }
}

struct Cursor<'a>(&'a [u8]);
impl Cursor<'_> {
    fn array<const N: usize>(&mut self) -> DerivedResult<[u8; N]> {
        let bytes = self
            .0
            .get(..N)
            .ok_or(DerivedError::Corrupt("truncated bytes"))?;
        let mut array = [0; N];
        array.copy_from_slice(bytes);
        self.0 = &self.0[N..];
        Ok(array)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_envelopes_round_trip_and_reject_truncation_and_versions() -> DerivedResult<()> {
        let descriptor = Descriptor::new("codec-test", 7);
        let parent = DerivedNode {
            key: descriptor.key(StateId::from_bytes([1; 32])),
            parents: vec![],
            value: b"root value".to_vec(),
        };
        let child = DerivedNode {
            key: descriptor.key(StateId::from_bytes([2; 32])),
            parents: vec![ParentValue {
                key: parent.key,
                root: parent.hash()?,
            }],
            value: b"child value".to_vec(),
        };
        let bytes = child.encode()?;
        assert_eq!(bytes.len(), child.encoded_len());
        assert_eq!(DerivedNode::decode(&bytes)?, child);
        for n in 0..177 {
            assert!(DerivedNode::decode(&bytes[..n]).is_err());
        }
        let mut wrong = bytes.clone();
        // Parent's version is part of its full key, not just its node hash.
        wrong[73 + 4 + 64..73 + 4 + 68].copy_from_slice(&8u32.to_be_bytes());
        assert!(matches!(
            DerivedNode::decode(&wrong),
            Err(DerivedError::Corrupt(_))
        ));
        assert_ne!(node_hash(&bytes), node_hash(&wrong));
        assert_ne!(
            descriptor.key(parent.key.state),
            Descriptor::new("codec-test", 8).key(parent.key.state)
        );
        let input = DerivedInput {
            state: parent.key.state,
            parents: vec![child.key.state],
            change: b"delta".to_vec(),
        };
        let decoded = DerivedInput::decode(&input.encode()?)?;
        assert_eq!(decoded.state, input.state);
        assert_eq!(decoded.parents, input.parents);
        assert_eq!(decoded.change, input.change);
        for n in 0..68 {
            assert!(DerivedInput::decode(&input.encode()?[..n]).is_err());
        }
        Ok(())
    }
}
