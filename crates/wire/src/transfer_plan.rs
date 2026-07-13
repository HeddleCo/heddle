// SPDX-License-Identifier: Apache-2.0
//! Shared repository transfer planning primitives.
//!
//! The wire protocol still carries the existing push/pull messages. This
//! module gives local and hosted sync paths one Rust-native vocabulary for the
//! Heddle object lane: content-addressed objects that can ride the native pack,
//! and signed sidecars that must use the out-of-pack verification paths.

use objects::{object::StateId, store::ObjectStore};

use crate::{
    ObjectInfo, ObjectType, ObjectTypeBucket, PlannedObject, Result, StateClosureOptions,
    enumerate_state_closure_plan_with_options,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GitLaneTransferIntent {
    /// This transfer contains only Heddle content-addressed objects and
    /// sidecars; no Git-lane work is expected.
    #[default]
    HeddleObjectsOnly,
    /// Hosted Git-lane pack streaming remains on the current implementation.
    /// The shared transfer plan records that fact without taking ownership of
    /// reachable Git pack construction.
    ExistingImplementation,
    /// Placeholder for the future Sley facade boundary. Heddle should not grow
    /// a second reachable-pack planner locally; once Sley exposes the needed
    /// facade, this intent can become an executable Git-lane plan.
    BlockedOnSleyReachablePackPlanning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryTransferPlan<T = PlannedObject> {
    pub partitions: TransferPartitions<T>,
    pub stats: TransferPlanStats,
    pub git_lane: GitLaneTransferIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPartitions<T = PlannedObject> {
    pub packable_objects: Vec<T>,
    pub sidecar_objects: Vec<T>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferPlanStats {
    pub total_objects: usize,
    pub packable_objects: usize,
    pub sidecar_objects: usize,
    pub blobs: usize,
    pub trees: usize,
    pub states: usize,
    pub actions: usize,
    pub redactions: usize,
    pub state_visibilities: usize,
}

impl RepositoryTransferPlan<PlannedObject> {
    pub fn from_state_closure_plan(
        store: &impl ObjectStore,
        root: StateId,
        options: StateClosureOptions,
        git_lane: GitLaneTransferIntent,
    ) -> Result<Self> {
        let objects = enumerate_state_closure_plan_with_options(store, root, options)?;
        Ok(Self::from_planned_objects(objects, git_lane))
    }

    pub fn from_planned_objects(
        objects: impl IntoIterator<Item = PlannedObject>,
        git_lane: GitLaneTransferIntent,
    ) -> Self {
        build_plan(objects, planned_object_type, git_lane)
    }
}

impl RepositoryTransferPlan<ObjectInfo> {
    pub fn from_object_infos(
        objects: impl IntoIterator<Item = ObjectInfo>,
        git_lane: GitLaneTransferIntent,
    ) -> Self {
        build_plan(objects, object_info_type, git_lane)
    }
}

impl<T> RepositoryTransferPlan<T> {
    pub fn requires_native_pack(&self, include_full_closure: bool) -> bool {
        include_full_closure || self.stats.packable_objects > 0
    }

    pub fn is_heddle_only(&self) -> bool {
        self.git_lane == GitLaneTransferIntent::HeddleObjectsOnly
    }
}

impl<T> TransferPartitions<T> {
    pub fn is_empty(&self) -> bool {
        self.packable_objects.is_empty() && self.sidecar_objects.is_empty()
    }

    pub fn len(&self) -> usize {
        self.packable_objects.len() + self.sidecar_objects.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.packable_objects
            .iter()
            .chain(self.sidecar_objects.iter())
    }

    pub fn is_sidecar_object_type(obj_type: ObjectType) -> bool {
        !obj_type.packable()
    }
}

impl<T> Default for TransferPartitions<T> {
    fn default() -> Self {
        Self {
            packable_objects: Vec::new(),
            sidecar_objects: Vec::new(),
        }
    }
}

impl TransferPlanStats {
    fn record(&mut self, obj_type: ObjectType) {
        self.total_objects += 1;
        if TransferPartitions::<()>::is_sidecar_object_type(obj_type) {
            self.sidecar_objects += 1;
        } else {
            self.packable_objects += 1;
        }
        match obj_type.bucket() {
            ObjectTypeBucket::Blob => self.blobs += 1,
            ObjectTypeBucket::Tree => self.trees += 1,
            ObjectTypeBucket::State => self.states += 1,
            ObjectTypeBucket::Action => self.actions += 1,
            ObjectTypeBucket::Redaction => self.redactions += 1,
            ObjectTypeBucket::StateVisibility => self.state_visibilities += 1,
        }
    }
}

fn build_plan<T>(
    objects: impl IntoIterator<Item = T>,
    object_type: fn(&T) -> ObjectType,
    git_lane: GitLaneTransferIntent,
) -> RepositoryTransferPlan<T> {
    let mut partitions = TransferPartitions::default();
    let mut stats = TransferPlanStats::default();

    for object in objects {
        let obj_type = object_type(&object);
        stats.record(obj_type);
        if TransferPartitions::<T>::is_sidecar_object_type(obj_type) {
            partitions.sidecar_objects.push(object);
        } else {
            partitions.packable_objects.push(object);
        }
    }

    RepositoryTransferPlan {
        partitions,
        stats,
        git_lane,
    }
}

fn planned_object_type(object: &PlannedObject) -> ObjectType {
    object.obj_type
}

fn object_info_type(object: &ObjectInfo) -> ObjectType {
    object.obj_type
}

#[cfg(test)]
mod tests {
    use objects::object::{ContentHash, StateId};

    use super::*;
    use crate::ObjectId;

    fn hash(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    #[test]
    fn partitions_split_native_pack_objects_from_sidecars() {
        let state = StateId::from_bytes([9; 32]);
        let plan = RepositoryTransferPlan::from_planned_objects(
            vec![
                PlannedObject {
                    id: ObjectId::Hash(hash(1)),
                    obj_type: ObjectType::Blob,
                },
                PlannedObject {
                    id: ObjectId::Hash(hash(2)),
                    obj_type: ObjectType::Tree,
                },
                PlannedObject {
                    id: ObjectId::Hash(hash(1)),
                    obj_type: ObjectType::Redaction,
                },
                PlannedObject {
                    id: ObjectId::StateId(state),
                    obj_type: ObjectType::StateVisibility,
                },
            ],
            GitLaneTransferIntent::HeddleObjectsOnly,
        );

        assert_eq!(plan.partitions.packable_objects.len(), 2);
        assert_eq!(plan.partitions.sidecar_objects.len(), 2);
        assert_eq!(plan.stats.total_objects, 4);
        assert_eq!(plan.stats.packable_objects, 2);
        assert_eq!(plan.stats.sidecar_objects, 2);
        assert_eq!(plan.stats.blobs, 1);
        assert_eq!(plan.stats.trees, 1);
        assert_eq!(plan.stats.redactions, 1);
        assert_eq!(plan.stats.state_visibilities, 1);
        assert!(plan.requires_native_pack(false));
    }

    #[test]
    fn sidecar_only_plan_does_not_require_native_pack() {
        let state = StateId::from_bytes([3; 32]);
        let plan = RepositoryTransferPlan::from_object_infos(
            vec![ObjectInfo {
                id: ObjectId::StateId(state),
                obj_type: ObjectType::StateVisibility,
                size: 128,
                delta_base: None,
            }],
            GitLaneTransferIntent::HeddleObjectsOnly,
        );

        assert!(!plan.requires_native_pack(false));
        assert!(plan.requires_native_pack(true));
        assert_eq!(plan.stats.packable_objects, 0);
        assert_eq!(plan.stats.sidecar_objects, 1);
    }

    #[test]
    fn git_lane_intents_name_current_and_sley_gated_paths() {
        let hosted = RepositoryTransferPlan::from_planned_objects(
            Vec::<PlannedObject>::new(),
            GitLaneTransferIntent::ExistingImplementation,
        );
        let sley_blocked = RepositoryTransferPlan::from_planned_objects(
            Vec::<PlannedObject>::new(),
            GitLaneTransferIntent::BlockedOnSleyReachablePackPlanning,
        );

        assert_eq!(
            hosted.git_lane,
            GitLaneTransferIntent::ExistingImplementation
        );
        assert_eq!(
            sley_blocked.git_lane,
            GitLaneTransferIntent::BlockedOnSleyReachablePackPlanning
        );
        assert!(!hosted.is_heddle_only());
        assert!(!sley_blocked.is_heddle_only());
    }
}
