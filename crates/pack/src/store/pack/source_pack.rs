// SPDX-License-Identifier: Apache-2.0
//! Exact source closure validation, shared by device and hosted publication.
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, Write},
};

use super::{ObjectType, PackObjectId, PackReader, PackStats, StreamingPackBuilder, SyncData};
use crate::{
    object::{ContentHash, ObjectSource, State, Tree, TreeEntryTarget},
    store::{Result, StoreError},
};

pub(super) fn validate(
    reader: &PackReader<'_>,
    selected: &State,
    max_decoded_bytes: u64,
    references: &[crate::object::source_target::capture::ReferenceProof],
) -> Result<Vec<PackObjectId>> {
    let canonical = selected.encode_current_msgpack()?;
    let mut available = BTreeMap::new();
    let mut trees = BTreeMap::new();
    let mut decoded = 0_u64;
    reader.visit_objects(|id, kind, data| {
        decoded = decoded
            .checked_add(data.len() as u64)
            .ok_or_else(|| invalid("source pack size overflow"))?;
        if decoded > max_decoded_bytes {
            return Err(invalid("source pack decoded byte budget exceeded"));
        }
        if available.insert(id, kind).is_some() {
            return Err(invalid("duplicate source pack object"));
        }
        match (id, kind) {
            (PackObjectId::StateId(id), ObjectType::State)
                if id == selected.id() && data == canonical => {}
            (PackObjectId::Hash(hash), ObjectType::Blob)
                if ContentHash::compute_typed("blob", data) == hash => {}
            (PackObjectId::Hash(hash), ObjectType::Tree) => {
                // Full tree anchors avoid pulling a private historical delta
                // base into a selected revision's disclosure closure.
                let tree = Tree::decode_canonical(data)
                    .map_err(|_| invalid("source pack requires complete canonical tree anchors"))?;
                if tree.hash() != hash {
                    return Err(invalid("source tree hash differs from its address"));
                }
                trees.insert(hash, tree);
            }
            _ => {
                return Err(invalid(
                    "source pack contains an unselected or incorrectly addressed object",
                ));
            }
        }
        Ok(())
    })?;
    let mut visited = BTreeSet::new();
    let mut pending = vec![
        (PackObjectId::StateId(selected.id()), ObjectType::State),
        (PackObjectId::Hash(selected.tree), ObjectType::Tree),
    ];
    while let Some((id, kind)) = pending.pop() {
        if available.get(&id) != Some(&kind) {
            return Err(invalid("source closure is incomplete"));
        }
        if !visited.insert(id) {
            continue;
        }
        if kind == ObjectType::Tree {
            let PackObjectId::Hash(hash) = id else {
                return Err(invalid("tree address is not a content hash"));
            };
            let tree = trees
                .get(&hash)
                .ok_or_else(|| invalid("source tree unavailable"))?;
            for (hash, kind) in children(tree) {
                pending.push((PackObjectId::Hash(hash), kind));
            }
        }
    }
    for reference in references {
        let closure = crate::object::source_target::capture::closure(
            &ReferenceReader(reader),
            reference.descriptor,
            &reference.scope,
            reference.state,
        )?;
        for hash in closure.blobs.keys() {
            let id = PackObjectId::Hash(*hash);
            if available.get(&id) != Some(&ObjectType::Blob) {
                return Err(invalid("reference closure is incomplete"));
            }
            visited.insert(id);
        }
    }
    if visited.len() != available.len() {
        return Err(invalid(
            "source pack contains objects outside the selected revision",
        ));
    }
    Ok(visited.into_iter().collect())
}
fn invalid(message: &str) -> StoreError {
    StoreError::InvalidObject(message.into())
}

/// Build an exact selected-source pack without consulting State history,
/// provenance, attachments, or linked spools. Full canonical trees keep private
/// delta bases out of the pack. The caller owns revision authorization and the
/// temporary output directory; discard that directory on failure.
///
/// Object bytes stream into the builder one object at a time. Blob header
/// lengths are checked before reading where the source supports that probe.
/// No object repository, CLI, network, or async runtime dependency is required.
pub fn build_source_pack<W: Write + Read + Seek + SyncData>(
    builder: StreamingPackBuilder<W>,
    source: &impl ObjectSource,
    selected: &State,
    max_objects: usize,
    max_decoded_bytes: u64,
) -> Result<(W, PackStats)> {
    build_source_pack_with_references(
        builder,
        source,
        selected,
        &[],
        max_objects,
        max_decoded_bytes,
    )
}

pub fn build_source_pack_with_references<W: Write + Read + Seek + SyncData>(
    mut builder: StreamingPackBuilder<W>,
    source: &impl ObjectSource,
    selected: &State,
    references: &[crate::object::source_target::capture::ReferenceProof],
    max_objects: usize,
    max_decoded_bytes: u64,
) -> Result<(W, PackStats)> {
    if max_objects < 2 {
        return Err(invalid("source pack object budget exceeded"));
    }
    let canonical = selected.encode_current_msgpack()?;
    let mut decoded = 0_u64;
    charge_bytes(&mut decoded, canonical.len() as u64, max_decoded_bytes)?;
    builder.add_id(
        PackObjectId::StateId(selected.id()),
        ObjectType::State,
        &canonical,
    )?;
    let mut discovered = BTreeMap::from([(selected.tree, ObjectType::Tree)]);
    for reference in references {
        let closure = crate::object::source_target::capture::closure(
            source,
            reference.descriptor,
            &reference.scope,
            reference.state,
        )?;
        for hash in closure.blobs.keys() {
            discovered.entry(*hash).or_insert(ObjectType::Blob);
        }
        if discovered.len().saturating_add(1) > max_objects {
            return Err(invalid("reference pack object budget exceeded"));
        }
    }
    let mut pending = discovered.clone();
    while let Some((hash, kind)) = pending.pop_first() {
        match kind {
            ObjectType::Tree => {
                let tree = source
                    .get_tree(&hash)?
                    .ok_or_else(|| invalid("selected source tree is missing"))?;
                if tree.hash() != hash {
                    return Err(invalid("source tree differs from its address"));
                }
                let canonical = tree.encode_canonical()?;
                charge_bytes(&mut decoded, canonical.len() as u64, max_decoded_bytes)?;
                for (hash, kind) in children(&tree) {
                    if let Some(expected) = discovered.get(&hash) {
                        if *expected != kind {
                            return Err(invalid(
                                "source object is referenced with conflicting types",
                            ));
                        }
                        continue;
                    }
                    // The selected State occupies one slot beyond this set.
                    // Check before queuing or reading the excess object.
                    if discovered.len().saturating_add(1) >= max_objects {
                        return Err(invalid("source pack object budget exceeded"));
                    }
                    discovered.insert(hash, kind);
                    pending.insert(hash, kind);
                }
                builder.add_id(PackObjectId::Hash(hash), ObjectType::Tree, &canonical)?;
            }
            ObjectType::Blob => {
                let length = source
                    .decoded_blob_len(&hash)?
                    .ok_or_else(|| invalid("selected source blob is missing"))?;
                if length > max_decoded_bytes.saturating_sub(decoded) {
                    return Err(invalid("source pack decoded byte budget exceeded"));
                }
                let bytes = source
                    .get_blob_bytes(&hash)?
                    .ok_or_else(|| invalid("selected source blob is missing"))?;
                if bytes.len() as u64 != length
                    || ContentHash::compute_typed("blob", &bytes) != hash
                {
                    return Err(invalid(
                        "source blob differs from its address or declared size",
                    ));
                }
                charge_bytes(&mut decoded, length, max_decoded_bytes)?;
                builder.add_id(PackObjectId::Hash(hash), ObjectType::Blob, bytes)?;
            }
            _ => return Err(invalid("unexpected source object type")),
        }
    }
    builder.finalize()
}

fn charge_bytes(decoded: &mut u64, length: u64, limit: u64) -> Result<()> {
    *decoded = decoded
        .checked_add(length)
        .ok_or_else(|| invalid("source pack size overflow"))?;
    if *decoded > limit {
        return Err(invalid("source pack decoded byte budget exceeded"));
    }
    Ok(())
}

fn children(tree: &Tree) -> impl Iterator<Item = (ContentHash, ObjectType)> + '_ {
    tree.entries()
        .iter()
        .filter_map(|entry| match entry.target() {
            TreeEntryTarget::Tree { hash } => Some((*hash, ObjectType::Tree)),
            TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => {
                Some((*hash, ObjectType::Blob))
            }
            TreeEntryTarget::Gitlink { .. } | TreeEntryTarget::Spoollink { .. } => None,
        })
}

struct ReferenceReader<'a, 'b>(&'a PackReader<'b>);
impl ObjectSource for ReferenceReader<'_, '_> {
    fn get_tree(&self, _: &ContentHash) -> Result<Option<Tree>> {
        Err(invalid("reference closure must not read source trees"))
    }
    fn get_state(&self, _: &crate::object::StateId) -> Result<Option<State>> {
        Err(invalid("reference closure must not read source history"))
    }
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<crate::object::Blob>> {
        match self.0.get_hashed_object(hash)? {
            Some((ObjectType::Blob, bytes)) => Ok(Some(crate::object::Blob::new(bytes))),
            None => Ok(None),
            _ => Err(invalid("reference object must be a blob")),
        }
    }
    fn decoded_blob_len(&self, hash: &ContentHash) -> Result<Option<u64>> {
        self.0.get_hashed_object_size(hash)
    }
}
