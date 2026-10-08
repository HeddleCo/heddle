// SPDX-License-Identifier: Apache-2.0
//! Stream signed descriptor closures, keeping only the current trie route and
//! resolution. Both deduplication and file-binding membership live on disk.
use std::path::Path;

use super::{ObjectType, PackObjectId, disk_graph::DiskGraph};
use crate::{
    object::{
        ContentHash, ObjectSource,
        source_target::capture::{
            self, FileResolution, ReferenceProof, SourceTargetSnapshot, TargetResolution,
        },
        source_target_map::{MapBudget, SourceTargetMap, SourceTargetMapStore},
    },
    store::{Result, StoreError},
};

pub(super) fn visit(
    source: &impl ObjectSource,
    proof: &ReferenceProof,
    root: &Path,
    visitor: impl FnMut(ContentHash, &[u8]) -> Result<()>,
) -> Result<()> {
    let mut reader = Reader {
        source,
        visited: DiskGraph::new(root)?,
        bytes: 0,
        visitor,
    };
    let snapshot: SourceTargetSnapshot =
        capture::decode(&reader.blob(proof.descriptor, capture::MAX_REFERENCE_OBJECT_BYTES)?)?;
    snapshot.validate(&proof.scope, proof.state)?;
    // Canonical trie depth bounds the frontier. Bytes, rather than the number
    // of nodes or resolution records, bound the reference disclosure.
    let mut budget = MapBudget::new(usize::MAX, capture::MAX_REFERENCE_BYTES, 0, 0);
    SourceTargetMap::visit_entries(
        &mut reader,
        snapshot.files,
        &mut budget,
        |reader, id, hash| {
            let file: FileResolution =
                capture::decode(&reader.blob(hash, capture::MAX_REFERENCE_OBJECT_BYTES)?)?;
            if file.core.id().map_err(capture::invalid)? != id
                || file.core.scope.spool != proof.scope.spool
            {
                return Err(capture::invalid(
                    "source file core identity or Spool mismatch",
                ));
            }
            let mut current = file.core;
            current.path = file.path;
            current.id().map_err(capture::invalid)?;
            reader.visited.insert_file(id)
        },
    )
    .map_err(capture::invalid)?;
    SourceTargetMap::visit_entries(
        &mut reader,
        snapshot.targets,
        &mut budget,
        |reader, id, hash| {
            let target: TargetResolution =
                capture::decode(&reader.blob(hash, capture::MAX_REFERENCE_OBJECT_BYTES)?)?;
            if target.core.id().map_err(capture::invalid)? != id
                || !reader.visited.contains_file(target.core.file)?
            {
                return Err(capture::invalid(
                    "source target identity or file closure mismatch",
                ));
            }
            let mut current = target.core;
            current.selector = target.selector;
            current.id().map_err(capture::invalid)?;
            Ok(())
        },
    )
    .map_err(capture::invalid)?;
    Ok(())
}
struct Reader<'a, S, V> {
    source: &'a S,
    visited: DiskGraph,
    bytes: usize,
    visitor: V,
}
impl<S: ObjectSource, V: FnMut(ContentHash, &[u8]) -> Result<()>> Reader<'_, S, V> {
    fn blob(&mut self, hash: ContentHash, limit: usize) -> Result<Vec<u8>> {
        let seen = self.visited.contains(PackObjectId::Hash(hash))?;
        let allowed = limit.min(capture::MAX_REFERENCE_OBJECT_BYTES).min(if seen {
            usize::MAX
        } else {
            capture::MAX_REFERENCE_BYTES.saturating_sub(self.bytes)
        });
        if self
            .source
            .decoded_blob_len(&hash)?
            .is_none_or(|len| len > allowed as u64)
        {
            return Err(capture::invalid("reference blob missing or exceeds budget"));
        }
        let bytes = self
            .source
            .get_blob_bytes(&hash)?
            .ok_or_else(|| capture::invalid("reference blob missing"))?;
        if bytes.len() > allowed || ContentHash::compute_typed("blob", &bytes) != hash {
            return Err(capture::invalid(
                "reference blob identity or budget mismatch",
            ));
        }
        if self
            .visited
            .insert(PackObjectId::Hash(hash), ObjectType::Blob)?
        {
            self.bytes += bytes.len();
            (self.visitor)(hash, &bytes)?;
        }
        Ok(bytes.to_vec())
    }
}
impl<S: ObjectSource, V: FnMut(ContentHash, &[u8]) -> Result<()>> SourceTargetMapStore
    for Reader<'_, S, V>
{
    type Error = StoreError;
    fn read(&mut self, hash: ContentHash, max: usize) -> Result<Option<Vec<u8>>> {
        self.blob(hash, max).map(Some)
    }
    fn write(&mut self, _: ContentHash, _: Vec<u8>) -> Result<()> {
        Err(capture::invalid("read-only reference closure"))
    }
}
