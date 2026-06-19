// SPDX-License-Identifier: Apache-2.0
use objects::{
    object::ActionId,
    store::{LocalObjectStore, ObjectKey, ObjectStore},
};

use crate::{ObjectId, ObjectInfo, ObjectType, ProtocolError, Result};

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

pub fn has_object(store: &impl LocalObjectStore, info: &ObjectInfo) -> Result<bool> {
    match (&info.id, info.obj_type) {
        (ObjectId::Hash(hash), ObjectType::Blob) => Ok(store.has_blob(hash)?),
        (ObjectId::Hash(hash), ObjectType::Tree) => Ok(store.has_tree(hash)?),
        (ObjectId::ChangeId(id), ObjectType::State) => Ok(store.has_state(id)?),
        // Redactions are keyed by the redacted blob's hash. Two senders
        // can declare different redactions on the same blob (different
        // reason / signature / timestamp), so we conservatively report
        // "do not have" and always re-fetch — `accept_wire_redactions`
        // deduplicates via the content-addressed `put_redaction`
        // idempotency rule. Cheap to refetch; correct under merge.
        (ObjectId::Hash(_), ObjectType::Redaction) => Ok(false),
        // StateVisibility is a per-state sidecar with append/merge
        // semantics. Like Redaction, conservatively refetch and let the
        // repository boundary validate + dedupe.
        (ObjectId::ChangeId(_), ObjectType::StateVisibility) => Ok(false),
        _ => Ok(false),
    }
}

pub async fn has_object_async<S>(store: &S, info: &ObjectInfo) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    let Some(key) = object_key_for_availability(info) else {
        return Ok(false);
    };
    Ok(store.has_object(&key).await?)
}

pub fn plan_object_availability(
    store: &impl LocalObjectStore,
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

pub async fn plan_object_availability_async<S>(
    store: &S,
    objects: &[ObjectInfo],
) -> Result<ObjectAvailabilityPlan>
where
    S: ObjectStore + ?Sized,
{
    let mut plan = ObjectAvailabilityPlan::default();
    let keyed: Vec<_> = objects
        .iter()
        .enumerate()
        .filter_map(|(index, info)| object_key_for_availability(info).map(|key| (index, key)))
        .collect();
    let keys: Vec<_> = keyed.iter().map(|(_, key)| key.clone()).collect();
    let presence = store.has_many(&keys).await?;
    if presence.len() != keyed.len() {
        return Err(ProtocolError::InvalidState(format!(
            "object store returned {} availability results for {} requested objects",
            presence.len(),
            keyed.len()
        )));
    }

    let mut present = vec![false; objects.len()];
    for ((index, _key), result) in keyed.into_iter().zip(presence) {
        present[index] = result.present;
    }

    for (info, is_present) in objects.iter().zip(present) {
        if is_present {
            plan.have_objects.push(info.id.clone());
            plan.present_objects.push(info.id.clone());
        } else {
            plan.want_objects.push(info.id.clone());
            plan.missing_objects.push(info.id.clone());
        }
    }

    Ok(plan)
}

fn object_key_for_availability(info: &ObjectInfo) -> Option<ObjectKey> {
    match (&info.id, info.obj_type) {
        (ObjectId::Hash(hash), ObjectType::Blob) => Some(ObjectKey::Blob(*hash)),
        (ObjectId::Hash(hash), ObjectType::Tree) => Some(ObjectKey::Tree(*hash)),
        (ObjectId::Hash(hash), ObjectType::Action) => {
            Some(ObjectKey::Action(ActionId::from_hash(*hash)))
        }
        (ObjectId::ChangeId(id), ObjectType::State) => Some(ObjectKey::State(*id)),
        // Sidecars are merge/dedupe records. Keep the existing conservative
        // sync semantics: fetch them and validate at the repository boundary.
        (ObjectId::Hash(_), ObjectType::Redaction)
        | (ObjectId::ChangeId(_), ObjectType::StateVisibility) => None,
        _ => None,
    }
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
        store::{LocalObjectStore, Result as StoreResult},
    };

    use super::*;

    #[derive(Default)]
    struct DummyStore {
        blob: Option<ContentHash>,
    }

    impl LocalObjectStore for DummyStore {
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
