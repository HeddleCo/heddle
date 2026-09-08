// SPDX-License-Identifier: Apache-2.0
//! Exact source closure validation, shared by device and hosted publication.
use std::collections::{BTreeMap, BTreeSet};

use super::{ObjectType, PackObjectId, PackReader};
use crate::{
    object::{ContentHash, State, Tree, TreeEntryTarget},
    store::{Result, StoreError},
};

pub(super) fn validate(
    reader: &PackReader<'_>,
    selected: &State,
    max_decoded_bytes: u64,
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
            for entry in tree.entries() {
                let child = match entry.target() {
                    TreeEntryTarget::Tree { hash } => Some((*hash, ObjectType::Tree)),
                    TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => {
                        Some((*hash, ObjectType::Blob))
                    }
                    TreeEntryTarget::Gitlink { .. } | TreeEntryTarget::Spoollink { .. } => None,
                };
                if let Some((hash, kind)) = child {
                    pending.push((PackObjectId::Hash(hash), kind));
                }
            }
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
