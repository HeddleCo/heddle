// SPDX-License-Identifier: Apache-2.0
use crate::{
    object::{Attribution, Blob, FileProvenance, Principal, State, StateId, Tree, TreeEntry},
    store::{InMemoryStore, ObjectStore},
};

pub(super) fn store() -> InMemoryStore {
    InMemoryStore::new()
}

pub(super) fn put_state_with_file(
    store: &InMemoryStore,
    file: &str,
    content: &[u8],
    parents: Vec<StateId>,
    principal_name: &str,
) -> State {
    let blob_hash = store.put_blob(&Blob::from_slice(content)).unwrap();
    let tree_hash = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file(file.to_string(), blob_hash, false).unwrap(),
        ]))
        .unwrap();
    let state = State::new(
        tree_hash,
        parents,
        Attribution::human(Principal::new(
            principal_name,
            format!("{principal_name}@example.com"),
        )),
    );
    store.put_state(&state).unwrap();
    state
}

pub(super) fn principals_at(provenance: &FileProvenance, line_idx: usize) -> Vec<String> {
    let line_origins = provenance.line_origin_set_indexes().unwrap();
    let set_idx = line_origins[line_idx];
    provenance.origin_sets[set_idx as usize]
        .origin_indexes
        .iter()
        .map(|index| {
            provenance.origins[*index as usize]
                .attribution
                .principal
                .name_lossy()
                .into_owned()
        })
        .collect()
}
