// SPDX-License-Identifier: Apache-2.0
//! Transfer planning for Heddle object lanes.
//!
//! One vocabulary for local sync, hosted sync, and the wire protocol:
//! content-addressed objects that can ride the native pack, and signed
//! sidecars that must use the out-of-pack verification paths.

pub mod availability;
pub mod graph;
pub mod plan;

pub use availability::{ObjectAvailabilityPlan, has_object, plan_object_availability};
pub use graph::{
    ObjectId, ObjectInfo, ObjectType, ObjectTypeBucket, PlannedObject, StateClosureOptions,
    StateClosureTransferObjects, enumerate_state_closure, enumerate_state_closure_plan,
    enumerate_state_closure_plan_with_options, enumerate_state_closure_transfer_from_boundaries,
    enumerate_state_closure_transfer_with_options, enumerate_state_closure_with_options,
    is_ancestor, missing_blobs_in_tree,
};
pub use plan::{
    GitLaneTransferIntent, RepositoryTransferPlan, TransferPartitions, TransferPlanStats,
};

#[cfg(test)]
mod tests {
    use super::graph::ObjectType;

    /// The push/pull packability split routes `StateAttachment` off the push
    /// pack (weft#549 forgery seal) while leaving pull carriage and every other
    /// type untouched.
    #[test]
    fn packable_predicates_split_state_attachment_by_direction() {
        // Sidecar records are never packable in either direction.
        for sidecar in [
            ObjectType::Redaction,
            ObjectType::StateVisibility,
            ObjectType::KeyBinding,
        ] {
            assert!(!sidecar.packable_for_push(), "{sidecar:?} push");
            assert!(!sidecar.packable_for_pull(), "{sidecar:?} pull");
        }
        // Content-addressed objects ride the pack in both directions.
        for packable in [
            ObjectType::Blob,
            ObjectType::Tree,
            ObjectType::State,
            ObjectType::Action,
        ] {
            assert!(packable.packable_for_push(), "{packable:?} push");
            assert!(packable.packable_for_pull(), "{packable:?} pull");
        }
        // The attachment record: excluded from the push pack. The pull
        // predicate stays true so planners still list it; weft delivers the
        // record on Frame::StateAttachment, not in the native pack.
        assert!(!ObjectType::StateAttachment.packable_for_push());
        assert!(ObjectType::StateAttachment.packable_for_pull());
    }
}
