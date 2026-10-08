// SPDX-License-Identifier: Apache-2.0
//! Exact source closure validation, shared by device and hosted publication.
use std::{
    fs::File,
    io::{Read, Seek, Write},
};

use super::{ObjectType, PackObjectId, PackReader, PackStats, StreamingPackBuilder, SyncData};
use crate::{
    object::{
        ContentHash, EntryRedactions, ObjectSource, PartialTree, State, Tree, TreeEntryTarget,
    },
    store::{Result, StoreError},
};

pub(super) fn validate(
    reader: &PackReader<'_>,
    selected: &State,
    max_decoded_bytes: u64,
    references: &[crate::object::source_target::capture::ReferenceProof],
    visibility: Option<&crate::object::thread_replication::CaptureVisibility>,
) -> Result<()> {
    validate_disclosure(
        reader,
        selected,
        max_decoded_bytes,
        references,
        visibility,
        false,
    )
    .map(|_| ())
}

/// Verified disclosure proofs, retained on disk until installed in the partial
/// tree cache. Object identities remain in the pack's mapped index.
pub struct VisibleSourceClosure {
    proofs: tempfile::NamedTempFile,
    partial_tree_count: usize,
    _scratch: super::ScratchDir,
}
impl VisibleSourceClosure {
    pub fn partial_tree_count(&self) -> usize {
        self.partial_tree_count
    }
    /// Decode and visit one verified partial tree at a time.
    pub fn visit_partial_trees(
        &self,
        mut visitor: impl FnMut(&PartialTree) -> Result<()>,
    ) -> Result<()> {
        let mut input = std::io::BufReader::new(File::open(self.proofs.path())?);
        for _ in 0..self.partial_tree_count {
            let mut length = [0; 8];
            input.read_exact(&mut length)?;
            let length = usize::try_from(u64::from_be_bytes(length))
                .map_err(|_| invalid("partial tree size exceeds platform"))?;
            let mut bytes = vec![0; length];
            input.read_exact(&mut bytes)?;
            visitor(&crate::object::decode_redacted_projection(&bytes)?)?;
        }
        Ok(())
    }
}

pub(super) fn validate_disclosure(
    reader: &PackReader<'_>,
    selected: &State,
    max_decoded_bytes: u64,
    references: &[crate::object::source_target::capture::ReferenceProof],
    visibility: Option<&crate::object::thread_replication::CaptureVisibility>,
    allow_partial: bool,
) -> Result<VisibleSourceClosure> {
    let canonical = selected.encode_current_msgpack()?;
    let scratch = super::ScratchDir::new(reader.scratch_root()?, "source-closure-")?;
    let mut verified = VisibleSourceClosure {
        proofs: tempfile::NamedTempFile::new_in(scratch.path())?,
        partial_tree_count: 0,
        _scratch: scratch,
    };
    let mut decoded = 0;
    reader.visit_objects(|id, kind, data| {
        charge_bytes(&mut decoded, data.len() as u64, max_decoded_bytes)?;
        match (id, kind) {
            (PackObjectId::StateId(id), ObjectType::State)
                if id == selected.id() && data == canonical => {}
            (PackObjectId::Hash(hash), ObjectType::Blob)
                if ContentHash::compute_typed("blob", data) == hash => {}
            (PackObjectId::Hash(hash), ObjectType::Tree) => {
                decode_tree(hash, data, allow_partial)?;
                if crate::object::is_redacted_tree(data) {
                    verified
                        .proofs
                        .write_all(&(data.len() as u64).to_be_bytes())?;
                    verified.proofs.write_all(data)?;
                    verified.partial_tree_count += 1;
                }
            }
            _ => {
                return Err(invalid(
                    "source pack contains an unselected or incorrectly addressed object",
                ));
            }
        }
        Ok(())
    })?;
    let mut visited = super::disk_graph::DiskGraph::new(verified._scratch.path())?;
    if let Some(visibility) = visibility {
        visibility.validate(selected)?;
        for entry in &visibility.entries {
            visited.insert_file(entry.tree_id)?;
        }
    }
    visited.insert(PackObjectId::StateId(selected.id()), ObjectType::State)?;
    visited.insert(PackObjectId::Hash(selected.tree), ObjectType::Tree)?;
    while let Some((id, kind)) = visited.pop()? {
        let (actual, bytes) = reader
            .get_object(&id)?
            .ok_or_else(|| invalid("source closure is incomplete"))?;
        if actual != kind {
            return Err(invalid("source closure object type mismatch"));
        }
        if kind == ObjectType::Tree {
            let PackObjectId::Hash(hash) = id else {
                return Err(invalid("tree address is not a content hash"));
            };
            let tree = decode_tree(hash, &bytes, allow_partial)?;
            if visited.contains_file(hash)? {
                for index in 0..tree.entries().len() {
                    if let Some(leaf) = tree.v4_leaf_hash_at(index) {
                        visited.insert_leaf(hash, leaf)?;
                    }
                }
            }
            for (hash, kind) in children(&tree) {
                if visited.insert(PackObjectId::Hash(hash), kind)?
                    && kind == ObjectType::Blob
                    && reader.get_hashed_object_type(&hash)? != Some(ObjectType::Blob)
                {
                    return Err(invalid(
                        "source closure is incomplete or has a type mismatch",
                    ));
                }
            }
        }
    }
    if let Some(visibility) = visibility {
        for entry in &visibility.entries {
            if !visited.contains(PackObjectId::Hash(entry.tree_id))? {
                return Err(invalid("entry visibility tree is outside selected source"));
            }
            if !visited.contains_leaf(entry.tree_id, entry.leaf_hash)? {
                return Err(invalid(
                    "entry visibility leaf is absent from selected salted tree",
                ));
            }
        }
    }
    for reference in references {
        super::reference_pack::visit(
            &ReferenceReader(reader),
            reference,
            verified._scratch.path(),
            |hash, _| {
                visited.insert(PackObjectId::Hash(hash), ObjectType::Blob)?;
                Ok(())
            },
        )?;
    }
    if visited.len() != reader.object_count() {
        return Err(invalid(
            "source pack contains objects outside the selected revision",
        ));
    }
    verified.proofs.flush()?;
    Ok(verified)
}

fn decode_tree(hash: ContentHash, data: &[u8], allow_partial: bool) -> Result<Tree> {
    if allow_partial && crate::object::is_redacted_tree(data) {
        let partial = crate::object::decode_redacted_projection(data)?;
        partial.verify()?;
        if partial.declared_root() != hash || !partial.has_redactions() {
            return Err(invalid(
                "partial source tree differs from its address or is complete",
            ));
        }
        return Ok(partial.visible_tree()?);
    }
    let tree = Tree::decode_canonical(data)
        .map_err(|_| invalid("source pack requires complete canonical tree anchors"))?;
    if tree.hash() != hash {
        return Err(invalid("source tree hash differs from its address"));
    }
    Ok(tree)
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
    max_decoded_bytes: u64,
) -> Result<(W, PackStats)> {
    build_source_pack_with_references(builder, source, selected, &[], max_decoded_bytes)
}

pub fn build_source_pack_with_references<W: Write + Read + Seek + SyncData>(
    builder: StreamingPackBuilder<W>,
    source: &impl ObjectSource,
    selected: &State,
    references: &[crate::object::source_target::capture::ReferenceProof],
    max_decoded_bytes: u64,
) -> Result<(W, PackStats)> {
    build_disclosure(
        builder,
        source,
        selected,
        references,
        None,
        max_decoded_bytes,
    )
    .map(|(output, stats, _)| (output, stats))
}

/// Build only source bytes visible under an already-admitted entry projection.
/// Hidden directories are not traversed; their salted commitments prove the
/// original tree root without names or targets. Reference descriptor closures
/// are included only when no selected entry is hidden. The final boolean is
/// true when the selected source and reference closure is complete.
pub fn build_visible_source_pack<W: Write + Read + Seek + SyncData>(
    builder: StreamingPackBuilder<W>,
    source: &impl ObjectSource,
    selected: &State,
    references: &[crate::object::source_target::capture::ReferenceProof],
    redactions: &EntryRedactions,
    max_decoded_bytes: u64,
) -> Result<(W, PackStats, bool)> {
    build_disclosure(
        builder,
        source,
        selected,
        references,
        Some(redactions),
        max_decoded_bytes,
    )
}

fn build_disclosure<W: Write + Read + Seek + SyncData>(
    mut builder: StreamingPackBuilder<W>,
    source: &impl ObjectSource,
    selected: &State,
    references: &[crate::object::source_target::capture::ReferenceProof],
    redactions: Option<&EntryRedactions>,
    max_decoded_bytes: u64,
) -> Result<(W, PackStats, bool)> {
    let canonical = selected.encode_current_msgpack()?;
    let mut decoded = 0_u64;
    charge_bytes(&mut decoded, canonical.len() as u64, max_decoded_bytes)?;
    builder.add_id(
        PackObjectId::StateId(selected.id()),
        ObjectType::State,
        &canonical,
    )?;
    let scratch_root = builder.scratch_root().to_path_buf();
    let mut discovered = super::disk_graph::DiskGraph::new(&scratch_root)?;
    discovered.insert(PackObjectId::Hash(selected.tree), ObjectType::Tree)?;
    let mut partial = false;
    while let Some((id, kind)) = discovered.pop()? {
        let PackObjectId::Hash(hash) = id else {
            return Err(invalid("source tree requires a content hash"));
        };
        match kind {
            ObjectType::Tree => {
                let tree = source
                    .get_tree(&hash)?
                    .ok_or_else(|| invalid("selected source tree is missing"))?;
                if tree.hash() != hash {
                    return Err(invalid("source tree differs from its address"));
                }
                let full = tree.encode_canonical()?;
                // Charge examined bytes, including hidden entries, so redaction
                // cannot turn a bounded disclosure into an unbounded tree walk.
                charge_bytes(&mut decoded, full.len() as u64, max_decoded_bytes)?;
                let (canonical, visible_tree) = match redactions {
                    Some(redactions)
                        if (0..tree.entries().len())
                            .any(|index| !redactions.entry_visible(&tree, index)) =>
                    {
                        partial = true;
                        let partial = PartialTree::project(&tree, redactions.leaves())?;
                        (
                            crate::object::encode_redacted_projection(&partial)?,
                            partial.visible_tree()?,
                        )
                    }
                    _ => (full, tree),
                };
                for (hash, kind) in children(&visible_tree) {
                    if discovered.insert(PackObjectId::Hash(hash), kind)?
                        && kind == ObjectType::Blob
                    {
                        add_blob(&mut builder, source, hash, &mut decoded, max_decoded_bytes)?;
                    }
                }
                builder.add_id(PackObjectId::Hash(hash), ObjectType::Tree, &canonical)?;
            }
            _ => return Err(invalid("unexpected source object type")),
        }
    }
    if !partial {
        for reference in references {
            super::reference_pack::visit(source, reference, &scratch_root, |hash, bytes| {
                if discovered.insert(PackObjectId::Hash(hash), ObjectType::Blob)? {
                    charge_bytes(&mut decoded, bytes.len() as u64, max_decoded_bytes)?;
                    builder.add_id(PackObjectId::Hash(hash), ObjectType::Blob, bytes)?;
                }
                Ok(())
            })?;
        }
    }
    drop(discovered);
    let (output, stats) = builder.finalize()?;
    Ok((output, stats, !partial))
}

fn add_blob<W: Write + Read + Seek + SyncData>(
    builder: &mut StreamingPackBuilder<W>,
    source: &impl ObjectSource,
    hash: ContentHash,
    decoded: &mut u64,
    max_decoded_bytes: u64,
) -> Result<()> {
    let length = source
        .decoded_blob_len(&hash)?
        .ok_or_else(|| invalid("selected source blob is missing"))?;
    if length > max_decoded_bytes.saturating_sub(*decoded) {
        return Err(invalid("source pack decoded byte budget exceeded"));
    }
    let bytes = source
        .get_blob_bytes(&hash)?
        .ok_or_else(|| invalid("selected source blob is missing"))?;
    if bytes.len() as u64 != length || ContentHash::compute_typed("blob", &bytes) != hash {
        return Err(invalid(
            "source blob differs from its address or declared size",
        ));
    }
    charge_bytes(decoded, length, max_decoded_bytes)?;
    builder.add_id(PackObjectId::Hash(hash), ObjectType::Blob, bytes)?;
    Ok(())
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
