// SPDX-License-Identifier: Apache-2.0
use objects::store::ObjectStore;

use crate::{ObjectId, ObjectInfo, ObjectType, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectAvailabilityPlan {
    pub have_objects: Vec<ObjectId>,
    pub want_objects: Vec<ObjectId>,
    pub present_objects: Vec<ObjectId>,
    pub missing_objects: Vec<ObjectId>,
    pub resumable_objects: Vec<ObjectId>,
    pub lazy_objects: Vec<ObjectId>,
    pub partial_fetch_allowed: bool,
}

pub fn has_object(store: &dyn ObjectStore, info: &ObjectInfo) -> Result<bool> {
    match (&info.id, info.obj_type) {
        (ObjectId::Hash(hash), ObjectType::Blob) => Ok(store.has_blob(hash)?),
        (ObjectId::Hash(hash), ObjectType::Tree) => Ok(store.has_tree(hash)?),
        (ObjectId::ChangeId(id), ObjectType::State) => Ok(store.has_state(id)?),
        _ => Ok(false),
    }
}

pub fn plan_object_availability(
    store: &dyn ObjectStore,
    objects: &[ObjectInfo],
) -> Result<ObjectAvailabilityPlan> {
    let mut plan = ObjectAvailabilityPlan::default();

    for info in objects {
        if has_object(store, info)? {
            plan.have_objects.push(info.id.clone());
            plan.present_objects.push(info.id.clone());
        } else {
            plan.want_objects.push(info.id.clone());
            plan.missing_objects.push(info.id.clone());
        }
    }

    Ok(plan)
}

impl ObjectAvailabilityPlan {
    pub fn with_partial_fetch_allowed(mut self, allowed: bool) -> Self {
        self.partial_fetch_allowed = allowed;
        self
    }

    pub fn is_complete(&self) -> bool {
        self.want_objects.is_empty()
            && self.missing_objects.is_empty()
            && self.resumable_objects.is_empty()
            && self.lazy_objects.is_empty()
    }

    pub fn has_partial_fetch_candidates(&self) -> bool {
        !self.resumable_objects.is_empty() || !self.lazy_objects.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use objects::{
        object::{Blob, ChangeId, ContentHash, Tree},
        store::{ObjectStore, Result as StoreResult},
    };

    use super::*;

    #[derive(Default)]
    struct DummyStore {
        blob: Option<ContentHash>,
    }

    impl ObjectStore for DummyStore {
        fn get_blob(&self, _hash: &ContentHash) -> StoreResult<Option<Blob>> {
            Ok(None)
        }

        fn put_blob(&self, _blob: &Blob) -> StoreResult<ContentHash> {
            unreachable!("not used in test")
        }

        fn has_blob(&self, hash: &ContentHash) -> StoreResult<bool> {
            Ok(self.blob == Some(*hash))
        }

        fn get_tree(&self, _hash: &ContentHash) -> StoreResult<Option<Tree>> {
            Ok(None)
        }

        fn put_tree(&self, _tree: &Tree) -> StoreResult<ContentHash> {
            unreachable!("not used in test")
        }

        fn has_tree(&self, _hash: &ContentHash) -> StoreResult<bool> {
            Ok(false)
        }

        fn get_state(&self, _id: &ChangeId) -> StoreResult<Option<objects::object::State>> {
            Ok(None)
        }

        fn put_state(&self, _state: &objects::object::State) -> StoreResult<()> {
            unreachable!("not used in test")
        }

        fn has_state(&self, _id: &ChangeId) -> StoreResult<bool> {
            Ok(false)
        }

        fn list_states(&self) -> StoreResult<Vec<ChangeId>> {
            Ok(vec![])
        }

        fn get_action(
            &self,
            _id: &objects::object::ActionId,
        ) -> StoreResult<Option<objects::object::Action>> {
            Ok(None)
        }

        fn put_action(
            &self,
            _action: &mut objects::object::Action,
        ) -> StoreResult<objects::object::ActionId> {
            unreachable!("not used in test")
        }

        fn list_actions(&self) -> StoreResult<Vec<objects::object::ActionId>> {
            Ok(vec![])
        }

        fn list_blobs(&self) -> StoreResult<Vec<ContentHash>> {
            Ok(vec![])
        }

        fn list_trees(&self) -> StoreResult<Vec<ContentHash>> {
            Ok(vec![])
        }
    }

    #[test]
    fn test_plan_tracks_present_and_missing_objects() {
        let blob = Blob::new(b"hello".to_vec());
        let blob_hash = blob.hash();
        let store = DummyStore {
            blob: Some(blob_hash),
        };
        let missing_hash = ContentHash::from_bytes([7; 32]);
        let objects = vec![
            ObjectInfo {
                id: ObjectId::Hash(blob_hash),
                obj_type: ObjectType::Blob,
                size: blob.size() as u64,
                delta_base: None,
            },
            ObjectInfo {
                id: ObjectId::Hash(missing_hash),
                obj_type: ObjectType::Tree,
                size: 0,
                delta_base: None,
            },
        ];

        let plan = plan_object_availability(&store, &objects).unwrap();

        assert_eq!(plan.have_objects.len(), 1);
        assert_eq!(plan.want_objects.len(), 1);
        assert_eq!(plan.present_objects.len(), 1);
        assert_eq!(plan.missing_objects.len(), 1);
        assert!(!plan.is_complete());
    }

    #[test]
    fn test_partial_fetch_flag_helpers() {
        let plan = ObjectAvailabilityPlan::default().with_partial_fetch_allowed(true);

        assert!(plan.partial_fetch_allowed);
        assert!(!plan.has_partial_fetch_candidates());
        assert!(plan.is_complete());
    }
}