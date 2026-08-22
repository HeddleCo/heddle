// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, bail};
use objects::{
    object::{Blob, ContentHash, State, StateId, Tree},
    store::{FsStore, ObjectStore},
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ObjectRef {
    State(StateId),
    Tree(ContentHash),
    Blob(ContentHash),
}

impl ObjectRef {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::State(_) => "state",
            Self::Tree(_) => "tree",
            Self::Blob(_) => "blob",
        }
    }

    pub fn id(&self) -> String {
        match self {
            Self::State(id) => id.to_string_full(),
            Self::Tree(hash) | Self::Blob(hash) => hash.to_hex(),
        }
    }

    pub fn load(&self, store: &FsStore) -> Result<Vec<u8>> {
        let bytes = match self {
            Self::State(id) => rmp_serde::to_vec_named(
                &store
                    .get_state(id)?
                    .with_context(|| format!("missing native state {}", self.id()))?,
            )?,
            Self::Tree(hash) => rmp_serde::to_vec_named(
                &store
                    .get_tree(hash)?
                    .with_context(|| format!("missing native tree {}", self.id()))?,
            )?,
            Self::Blob(hash) => store
                .get_blob(hash)?
                .with_context(|| format!("missing native blob {}", self.id()))?
                .content()
                .to_vec(),
        };
        self.validate(&bytes)?;
        Ok(bytes)
    }

    fn validate(&self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::State(expected) => {
                let state: State = rmp_serde::from_slice(bytes)?;
                let found = state.id();
                if found != *expected {
                    bail!("state payload hash mismatch: expected {expected}, found {found}");
                }
            }
            Self::Tree(expected) => {
                let tree: Tree = rmp_serde::from_slice(bytes)?;
                tree.validate()?;
                let found = tree.hash();
                if found != *expected {
                    bail!("tree payload hash mismatch: expected {expected}, found {found}");
                }
            }
            Self::Blob(expected) => {
                let found = Blob::from_slice(bytes).hash();
                if found != *expected {
                    bail!("blob payload hash mismatch: expected {expected}, found {found}");
                }
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub struct ObjectCounts {
    pub states: usize,
    pub trees: usize,
    pub blobs: usize,
    pub total: usize,
}

pub struct ObjectSet {
    pub states: Vec<StateId>,
    pub trees: Vec<ContentHash>,
    pub blobs: Vec<ContentHash>,
}

impl ObjectSet {
    pub fn counts(&self) -> ObjectCounts {
        ObjectCounts {
            states: self.states.len(),
            trees: self.trees.len(),
            blobs: self.blobs.len(),
            total: self.states.len() + self.trees.len() + self.blobs.len(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = ObjectRef> + '_ {
        self.states
            .iter()
            .copied()
            .map(ObjectRef::State)
            .chain(self.trees.iter().copied().map(ObjectRef::Tree))
            .chain(self.blobs.iter().copied().map(ObjectRef::Blob))
    }
}

pub fn load_object_set(store: &FsStore) -> Result<ObjectSet> {
    let mut states = store.list_states()?;
    let mut trees = store.list_trees()?;
    let mut blobs = store.list_blobs()?;
    states.sort();
    states.dedup();
    trees.sort();
    trees.dedup();
    blobs.sort();
    blobs.dedup();
    Ok(ObjectSet {
        states,
        trees,
        blobs,
    })
}
