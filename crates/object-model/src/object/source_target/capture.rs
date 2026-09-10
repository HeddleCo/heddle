//! Per-Thread derived source-target snapshot, committed by the signed capture.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{SourceFileCore, SourceSelector, SourceTargetCore};
use crate::{
    error::{HeddleError, Result},
    object::{
        CollaborationScope, ContentHash, ObjectSource, StateId,
        source_target_map::{MapBudget, SourceTargetMap, SourceTargetMapStore},
    },
};

pub const MAX_REFERENCE_OBJECTS: usize = 65_536;
pub const MAX_REFERENCE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_REFERENCE_OBJECT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTargetSnapshot {
    pub version: u16,
    pub scope: CollaborationScope,
    pub state: StateId,
    /// Commitment to the exact sorted collaboration heads at capture. Keeping
    /// individual private operation IDs out of source-only publication avoids
    /// revealing withheld discussion membership.
    pub collaboration_frontier: ContentHash,
    pub files: Option<ContentHash>,
    pub targets: Option<ContentHash>,
}
/// Expected descriptor address/scope derived from an already verified signed
/// capture. Supplying this tuple alone does not establish author authority.
pub struct ReferenceProof {
    pub descriptor: ContentHash,
    pub scope: CollaborationScope,
    pub state: StateId,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    Resolved,
    Ambiguous,
    Deleted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileResolution {
    pub core: SourceFileCore,
    pub path: String,
    pub blob: Option<ContentHash>,
    pub status: ResolutionStatus,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetResolution {
    pub core: SourceTargetCore,
    pub selector: SourceSelector,
    pub status: ResolutionStatus,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSourceTarget {
    pub scope: CollaborationScope,
    pub state: StateId,
    pub file: FileResolution,
    pub target: TargetResolution,
}
pub fn invalid(message: impl std::fmt::Display) -> HeddleError {
    HeddleError::InvalidObject(message.to_string())
}
pub fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    Ok(rmp_serde::to_vec_named(value)?)
}
pub fn decode<T: serde::de::DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_REFERENCE_OBJECT_BYTES {
        return Err(invalid("reference object exceeds budget"));
    }
    let value: T = rmp_serde::from_slice(bytes)?;
    if encode(&value)? != bytes {
        return Err(invalid("noncanonical reference object"));
    }
    Ok(value)
}
impl SourceTargetSnapshot {
    pub fn validate(&self, scope: &CollaborationScope, state: StateId) -> Result<()> {
        if self.version != 1
            || &self.scope != scope
            || self.scope.spool.is_nil()
            || self.scope.thread.is_none()
            || self.state != state
        {
            return Err(invalid(
                "reference snapshot scope, revision, or frontier mismatch",
            ));
        }
        Ok(())
    }
}
/// Verified closure includes map nodes and resolution records, but does not
/// follow original source revisions or named references into private history.
pub struct ReferenceClosure {
    pub snapshot: SourceTargetSnapshot,
    pub blobs: BTreeMap<ContentHash, Vec<u8>>,
    pub files: BTreeMap<ContentHash, FileResolution>,
    pub targets: BTreeMap<ContentHash, TargetResolution>,
}
struct Reader<'a, S> {
    source: &'a S,
    blobs: BTreeMap<ContentHash, Vec<u8>>,
    bytes: usize,
}
impl<S: ObjectSource> Reader<'_, S> {
    fn blob(&mut self, hash: ContentHash, limit: usize) -> Result<Vec<u8>> {
        if let Some(bytes) = self.blobs.get(&hash) {
            if bytes.len() > limit {
                return Err(invalid("reference read budget"));
            }
            return Ok(bytes.clone());
        }
        if self.blobs.len() >= MAX_REFERENCE_OBJECTS {
            return Err(invalid("reference object count budget"));
        }
        let allowed = limit
            .min(MAX_REFERENCE_OBJECT_BYTES)
            .min(MAX_REFERENCE_BYTES.saturating_sub(self.bytes));
        if self
            .source
            .decoded_blob_len(&hash)?
            .is_none_or(|len| len > allowed as u64)
        {
            return Err(invalid("reference blob missing or exceeds budget"));
        }
        let blob = self
            .source
            .get_blob(&hash)?
            .ok_or_else(|| invalid("reference blob missing"))?;
        let bytes = blob.into_content();
        if bytes.len() > allowed || ContentHash::compute_typed("blob", &bytes) != hash {
            return Err(invalid("reference blob identity or budget mismatch"));
        }
        self.bytes += bytes.len();
        self.blobs.insert(hash, bytes.clone());
        Ok(bytes)
    }
}
impl<S: ObjectSource> SourceTargetMapStore for Reader<'_, S> {
    type Error = HeddleError;
    fn read(&mut self, hash: ContentHash, max: usize) -> Result<Option<Vec<u8>>> {
        self.blob(hash, max).map(Some)
    }
    fn write(&mut self, _: ContentHash, _: Vec<u8>) -> Result<()> {
        Err(invalid("read-only reference closure"))
    }
}
pub fn closure(
    source: &impl ObjectSource,
    descriptor: ContentHash,
    scope: &CollaborationScope,
    state: StateId,
) -> Result<ReferenceClosure> {
    let mut reader = Reader {
        source,
        blobs: BTreeMap::new(),
        bytes: 0,
    };
    let snapshot: SourceTargetSnapshot =
        decode(&reader.blob(descriptor, MAX_REFERENCE_OBJECT_BYTES)?)?;
    snapshot.validate(scope, state)?;
    let mut budget = MapBudget::new(MAX_REFERENCE_OBJECTS, MAX_REFERENCE_BYTES, 0, 0);
    let file_entries = SourceTargetMap::entries(
        &mut reader,
        snapshot.files,
        MAX_REFERENCE_OBJECTS,
        &mut budget,
    )
    .map_err(invalid)?;
    let target_entries = SourceTargetMap::entries(
        &mut reader,
        snapshot.targets,
        MAX_REFERENCE_OBJECTS,
        &mut budget,
    )
    .map_err(invalid)?;
    let mut files = BTreeMap::new();
    for (id, hash) in file_entries {
        let file: FileResolution = decode(&reader.blob(hash, MAX_REFERENCE_OBJECT_BYTES)?)?;
        if file.core.id().map_err(invalid)? != id || file.core.scope.spool != scope.spool {
            return Err(invalid("source file core identity or Spool mismatch"));
        }
        // Reuse the canonical path validator without claiming new coordinates
        // change the original core's identity.
        let mut current = file.core.clone();
        current.path = file.path.clone();
        current.id().map_err(invalid)?;
        files.insert(id, file);
    }
    let mut targets = BTreeMap::new();
    for (id, hash) in target_entries {
        let target: TargetResolution = decode(&reader.blob(hash, MAX_REFERENCE_OBJECT_BYTES)?)?;
        if target.core.id().map_err(invalid)? != id || !files.contains_key(&target.core.file) {
            return Err(invalid("source target identity or file closure mismatch"));
        }
        let mut current = target.core.clone();
        current.selector = target.selector.clone();
        current.id().map_err(invalid)?;
        targets.insert(id, target);
    }
    Ok(ReferenceClosure {
        snapshot,
        blobs: reader.blobs,
        files,
        targets,
    })
}
