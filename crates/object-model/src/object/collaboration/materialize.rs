// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    CollabOpId, CollaborationAnchor, CollaborationCodecError, CollaborationOperationBodyV1,
    CollaborationResolution, DecodedCollaborationOperation, DiscussionRecordId, DiscussionTurnV1,
    LegacyDiscussionResolutionV1,
};
use crate::object::VisibilityTier;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedCollaborationSet {
    pub received: BTreeSet<CollabOpId>,
    pub accepted: BTreeSet<CollabOpId>,
    pub rejected: BTreeSet<CollabOpId>,
}

impl HostedCollaborationSet {
    pub fn validate(
        &self,
        operations: &BTreeMap<CollabOpId, DecodedCollaborationOperation>,
    ) -> Result<(), CollaborationCodecError> {
        if !self.accepted.is_subset(&self.received) || !self.rejected.is_subset(&self.received) {
            return Err(CollaborationCodecError::Invalid(
                "hosted accepted and rejected sets must be subsets of received".to_string(),
            ));
        }
        if !self.accepted.is_disjoint(&self.rejected) {
            return Err(CollaborationCodecError::Invalid(
                "hosted accepted and rejected sets must be disjoint".to_string(),
            ));
        }
        for id in &self.accepted {
            let operation = operations.get(id).ok_or_else(|| {
                CollaborationCodecError::Invalid(format!(
                    "hosted accepted operation {id} is unavailable"
                ))
            })?;
            if !operation
                .operation
                .parents
                .iter()
                .all(|parent| self.accepted.contains(parent))
            {
                return Err(CollaborationCodecError::Invalid(format!(
                    "hosted accepted set is not parent-closed at {id}"
                )));
            }
        }
        Ok(())
    }

    pub fn blocked_descendants(
        &self,
        operations: &BTreeMap<CollabOpId, DecodedCollaborationOperation>,
    ) -> BTreeSet<CollabOpId> {
        self.received
            .difference(&self.accepted)
            .filter(|id| {
                !self.rejected.contains(id)
                    && has_unaccepted_ancestor(**id, &self.accepted, operations)
            })
            .copied()
            .collect()
    }
}

fn has_unaccepted_ancestor(
    id: CollabOpId,
    accepted: &BTreeSet<CollabOpId>,
    operations: &BTreeMap<CollabOpId, DecodedCollaborationOperation>,
) -> bool {
    let Some(operation) = operations.get(&id) else {
        return true;
    };
    let mut pending = operation.operation.parents.clone();
    let mut seen = BTreeSet::new();
    while let Some(parent) = pending.pop() {
        if !accepted.contains(&parent) {
            return true;
        }
        if seen.insert(parent)
            && let Some(operation) = operations.get(&parent)
        {
            pending.extend(operation.operation.parents.iter().copied());
        }
    }
    false
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedDiscussion {
    pub discussion_id: DiscussionRecordId,
    pub title: String,
    pub anchor: CollaborationAnchor,
    pub visibility: VisibilityTier,
    pub turns: Vec<(CollabOpId, DiscussionTurnV1)>,
    pub resolution: Option<CollaborationResolution>,
    pub conflict_operations: BTreeSet<CollabOpId>,
    pub heads: BTreeSet<CollabOpId>,
    pub display_head: CollabOpId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaterializedRepositoryCollaboration {
    pub discussions: BTreeMap<DiscussionRecordId, MaterializedDiscussion>,
    pub pending: BTreeSet<CollabOpId>,
}

pub fn materialize_repository_collaboration(
    operations: impl IntoIterator<Item = DecodedCollaborationOperation>,
) -> Result<MaterializedRepositoryCollaboration, CollaborationCodecError> {
    let mut by_id = BTreeMap::new();
    for operation in operations {
        if by_id.insert(operation.operation_id, operation).is_some() {
            return Err(CollaborationCodecError::Invalid(
                "duplicate collaboration operation id".to_string(),
            ));
        }
    }

    let mut visible = BTreeSet::new();
    let mut ordered = Vec::new();
    loop {
        let next = by_id
            .iter()
            .filter(|(id, operation)| {
                !visible.contains(*id)
                    && operation
                        .operation
                        .parents
                        .iter()
                        .all(|parent| visible.contains(parent))
            })
            .map(|(id, operation)| (operation.operation.occurred_at_ms, *id))
            .min();
        let Some((_, id)) = next else { break };
        visible.insert(id);
        ordered.push(id);
    }

    let mut grouped: BTreeMap<DiscussionRecordId, Vec<CollabOpId>> = BTreeMap::new();
    for id in &ordered {
        let operation = &by_id[id].operation;
        for parent in &operation.parents {
            if by_id[parent].operation.discussion_id != operation.discussion_id {
                return Err(CollaborationCodecError::Invalid(format!(
                    "operation {id} has a parent from another discussion"
                )));
            }
        }
        grouped
            .entry(operation.discussion_id)
            .or_default()
            .push(*id);
    }

    let mut result = MaterializedRepositoryCollaboration {
        discussions: BTreeMap::new(),
        pending: by_id
            .keys()
            .filter(|id| !visible.contains(id))
            .copied()
            .collect(),
    };
    for (discussion_id, ids) in grouped {
        let discussion = materialize_discussion(discussion_id, &ids, &by_id)?;
        result.discussions.insert(discussion_id, discussion);
    }
    Ok(result)
}

fn materialize_discussion(
    discussion_id: DiscussionRecordId,
    ids: &[CollabOpId],
    all: &BTreeMap<CollabOpId, DecodedCollaborationOperation>,
) -> Result<MaterializedDiscussion, CollaborationCodecError> {
    let roots = ids
        .iter()
        .filter(|id| all[id].operation.parents.is_empty())
        .copied()
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        return Err(CollaborationCodecError::Invalid(format!(
            "discussion {discussion_id} has {} roots",
            roots.len()
        )));
    }
    let root_id = roots[0];
    let root = &all[&root_id].operation.body;
    let (title, anchor, visibility, root_turns, base_resolution) = match root {
        CollaborationOperationBodyV1::Open {
            title,
            anchor,
            visibility,
            turn,
        } => (
            title.clone(),
            anchor.clone(),
            visibility.clone(),
            vec![turn.clone()],
            None,
        ),
        CollaborationOperationBodyV1::LegacyImported {
            title,
            anchor,
            visibility,
            turns,
            resolution,
            ..
        } => (
            title.clone(),
            anchor.clone(),
            visibility.clone(),
            turns.clone(),
            legacy_resolution(resolution),
        ),
        _ => {
            return Err(CollaborationCodecError::Invalid(format!(
                "discussion {discussion_id} root is not an open or legacy import"
            )));
        }
    };

    let mut turns = root_turns
        .into_iter()
        .map(|turn| (root_id, turn))
        .collect::<Vec<_>>();
    let mut state_operations = BTreeSet::new();
    for id in ids.iter().copied().filter(|id| *id != root_id) {
        match &all[&id].operation.body {
            CollaborationOperationBodyV1::AppendTurn { turn } => turns.push((id, turn.clone())),
            CollaborationOperationBodyV1::Resolve { .. }
            | CollaborationOperationBodyV1::Reopen { .. }
            | CollaborationOperationBodyV1::ResolveConflict { .. } => {
                state_operations.insert(id);
            }
            CollaborationOperationBodyV1::Open { .. }
            | CollaborationOperationBodyV1::LegacyImported { .. } => {
                return Err(CollaborationCodecError::Invalid(format!(
                    "discussion {discussion_id} has multiple root operations"
                )));
            }
        }
    }

    let maximal_state = state_operations
        .iter()
        .filter(|candidate| {
            !state_operations
                .iter()
                .any(|other| candidate != &other && precedes(**candidate, *other, all))
        })
        .copied()
        .collect::<BTreeSet<_>>();
    let mut outcomes = BTreeMap::new();
    for id in &maximal_state {
        outcomes.insert(*id, resolution_outcome(*id, all, &mut BTreeSet::new())?);
    }
    let first_outcome = outcomes.values().next().cloned();
    let conflicts = if outcomes
        .values()
        .all(|outcome| Some(outcome) == first_outcome.as_ref())
    {
        BTreeSet::new()
    } else {
        outcomes.keys().copied().collect()
    };
    let resolution = if conflicts.is_empty() {
        first_outcome.unwrap_or(base_resolution)
    } else {
        None
    };

    let ids_set = ids.iter().copied().collect::<BTreeSet<_>>();
    let heads = ids_set
        .iter()
        .filter(|candidate| {
            !ids_set
                .iter()
                .any(|other| candidate != &other && precedes(**candidate, *other, all))
        })
        .copied()
        .collect::<BTreeSet<_>>();
    let display_head = *heads.iter().next().expect("root guarantees a head");
    Ok(MaterializedDiscussion {
        discussion_id,
        title,
        anchor,
        visibility,
        turns,
        resolution,
        conflict_operations: conflicts,
        heads,
        display_head,
    })
}

fn resolution_outcome(
    id: CollabOpId,
    operations: &BTreeMap<CollabOpId, DecodedCollaborationOperation>,
    visiting: &mut BTreeSet<CollabOpId>,
) -> Result<Option<CollaborationResolution>, CollaborationCodecError> {
    if !visiting.insert(id) {
        return Err(CollaborationCodecError::Invalid(format!(
            "collaboration conflict resolution cycle at {id}"
        )));
    }
    let body = &operations
        .get(&id)
        .ok_or_else(|| CollaborationCodecError::Invalid(format!("missing operation {id}")))?
        .operation
        .body;
    let result = match body {
        CollaborationOperationBodyV1::Resolve { resolution } => Some(resolution.clone()),
        CollaborationOperationBodyV1::Reopen { .. } => None,
        CollaborationOperationBodyV1::ResolveConflict { selected, .. } => {
            resolution_outcome(*selected, operations, visiting)?
        }
        _ => {
            return Err(CollaborationCodecError::Invalid(format!(
                "operation {id} does not select a resolution outcome"
            )));
        }
    };
    visiting.remove(&id);
    Ok(result)
}

fn precedes(
    ancestor: CollabOpId,
    descendant: CollabOpId,
    operations: &BTreeMap<CollabOpId, DecodedCollaborationOperation>,
) -> bool {
    let mut pending = operations[&descendant].operation.parents.clone();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if id == ancestor {
            return true;
        }
        if seen.insert(id)
            && let Some(operation) = operations.get(&id)
        {
            pending.extend(operation.operation.parents.iter().copied());
        }
    }
    false
}

fn legacy_resolution(value: &LegacyDiscussionResolutionV1) -> Option<CollaborationResolution> {
    match value {
        LegacyDiscussionResolutionV1::Open => None,
        LegacyDiscussionResolutionV1::AddressedByState { state_id } => {
            Some(CollaborationResolution::AddressedByState {
                state_id: *state_id,
            })
        }
        LegacyDiscussionResolutionV1::Dismissed { reason } => {
            Some(CollaborationResolution::Dismissed {
                reason: reason.clone(),
            })
        }
        LegacyDiscussionResolutionV1::Annotation { annotation_id } => {
            Some(CollaborationResolution::Annotation {
                annotation_id: annotation_id.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{
        Attribution, CollaborationIdempotencyKey, CollaborationOperationEnvelope, Principal,
    };

    fn discussion_id() -> DiscussionRecordId {
        "disc-018f47ea-4a54-7c89-b012-3456789abcde".parse().unwrap()
    }

    fn author() -> Attribution {
        Attribution::human(Principal::new("Ada", "ada@example.com"))
    }

    fn decoded(
        parents: Vec<CollabOpId>,
        key: &str,
        at: i64,
        body: CollaborationOperationBodyV1,
    ) -> DecodedCollaborationOperation {
        let operation = CollaborationOperationEnvelope::new(
            discussion_id(),
            parents,
            CollaborationIdempotencyKey::new(key).unwrap(),
            author(),
            at,
            body,
        )
        .unwrap();
        let bytes = operation.encode().unwrap();
        CollaborationOperationEnvelope::decode(&bytes).unwrap()
    }

    fn root() -> DecodedCollaborationOperation {
        decoded(
            vec![],
            "root",
            1,
            CollaborationOperationBodyV1::Open {
                title: "Review".to_string(),
                anchor: CollaborationAnchor::Repository,
                visibility: VisibilityTier::default(),
                turn: DiscussionTurnV1::new("first").unwrap(),
            },
        )
    }

    #[test]
    fn op_set_union_converges_independent_of_arrival_order() {
        let root = root();
        let a = decoded(
            vec![root.operation_id],
            "a",
            2,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("a").unwrap(),
            },
        );
        let b = decoded(
            vec![root.operation_id],
            "b",
            3,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("b").unwrap(),
            },
        );
        let forward =
            materialize_repository_collaboration(vec![root.clone(), a.clone(), b.clone()]).unwrap();
        let reverse = materialize_repository_collaboration(vec![b, a, root]).unwrap();
        assert_eq!(forward, reverse);
        let discussion = &forward.discussions[&discussion_id()];
        assert_eq!(discussion.turns.len(), 3);
        assert_eq!(discussion.heads.len(), 2);
        assert_eq!(
            discussion.display_head,
            *discussion.heads.iter().next().unwrap()
        );
    }

    #[test]
    fn missing_parent_blocks_descendant_until_causal_closure_arrives() {
        let root = root();
        let missing = CollabOpId::from_bytes([9; 32]);
        let child = decoded(
            vec![missing],
            "child",
            2,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("waiting").unwrap(),
            },
        );
        let materialized = materialize_repository_collaboration(vec![root, child.clone()]).unwrap();
        assert_eq!(materialized.pending, BTreeSet::from([child.operation_id]));
    }

    #[test]
    fn competing_resolutions_conflict_and_causal_reopen_clears_resolution() {
        let root = root();
        let left = decoded(
            vec![root.operation_id],
            "left",
            2,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::Dismissed {
                    reason: "obsolete".to_string(),
                },
            },
        );
        let right = decoded(
            vec![root.operation_id],
            "right",
            3,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::Annotation {
                    annotation_id: "ann-1".to_string(),
                },
            },
        );
        let conflicted =
            materialize_repository_collaboration(vec![root.clone(), left.clone(), right.clone()])
                .unwrap();
        assert_eq!(
            conflicted.discussions[&discussion_id()]
                .conflict_operations
                .len(),
            2
        );
        assert_eq!(conflicted.discussions[&discussion_id()].resolution, None);

        let mut competing = vec![left.operation_id, right.operation_id];
        competing.sort();
        let selected = competing[0];
        let resolved = decoded(
            competing.clone(),
            "resolve-conflict",
            4,
            CollaborationOperationBodyV1::ResolveConflict {
                competing,
                selected,
            },
        );
        let reopened = decoded(
            vec![resolved.operation_id],
            "reopen",
            5,
            CollaborationOperationBodyV1::Reopen {
                reason: "new evidence".to_string(),
            },
        );
        let view =
            materialize_repository_collaboration(vec![root, left, right, resolved, reopened])
                .unwrap();
        assert_eq!(view.discussions[&discussion_id()].resolution, None);
    }

    #[test]
    fn concurrent_reopen_and_resolve_surface_conflict() {
        let root = root();
        let resolved = decoded(
            vec![root.operation_id],
            "resolve",
            2,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::Dismissed {
                    reason: "done".to_string(),
                },
            },
        );
        let reopened = decoded(
            vec![root.operation_id],
            "reopen",
            3,
            CollaborationOperationBodyV1::Reopen {
                reason: "new evidence".to_string(),
            },
        );
        let view =
            materialize_repository_collaboration(vec![root, resolved.clone(), reopened.clone()])
                .unwrap();
        assert_eq!(
            view.discussions[&discussion_id()].conflict_operations,
            BTreeSet::from([resolved.operation_id, reopened.operation_id])
        );
    }

    #[test]
    fn competing_conflict_resolutions_form_a_recursive_conflict() {
        let root = root();
        let left = decoded(
            vec![root.operation_id],
            "left",
            2,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::Dismissed {
                    reason: "left".to_string(),
                },
            },
        );
        let right = decoded(
            vec![root.operation_id],
            "right",
            3,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::Dismissed {
                    reason: "right".to_string(),
                },
            },
        );
        let mut competing = vec![left.operation_id, right.operation_id];
        competing.sort();
        let choose_left = decoded(
            competing.clone(),
            "choose-left",
            4,
            CollaborationOperationBodyV1::ResolveConflict {
                competing: competing.clone(),
                selected: left.operation_id,
            },
        );
        let choose_right = decoded(
            competing.clone(),
            "choose-right",
            5,
            CollaborationOperationBodyV1::ResolveConflict {
                competing,
                selected: right.operation_id,
            },
        );
        let view = materialize_repository_collaboration(vec![
            root,
            left,
            right,
            choose_left.clone(),
            choose_right.clone(),
        ])
        .unwrap();
        assert_eq!(
            view.discussions[&discussion_id()].conflict_operations,
            BTreeSet::from([choose_left.operation_id, choose_right.operation_id])
        );
    }

    #[test]
    fn hosted_sets_separate_rejected_and_blocked_descendants() {
        let root = root();
        let rejected = decoded(
            vec![root.operation_id],
            "rejected",
            2,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("rejected").unwrap(),
            },
        );
        let child = decoded(
            vec![rejected.operation_id],
            "child",
            3,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("blocked").unwrap(),
            },
        );
        let grandchild = decoded(
            vec![child.operation_id],
            "grandchild",
            4,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("also blocked").unwrap(),
            },
        );
        let operations = [
            root.clone(),
            rejected.clone(),
            child.clone(),
            grandchild.clone(),
        ]
        .into_iter()
        .map(|operation| (operation.operation_id, operation))
        .collect();
        let hosted = HostedCollaborationSet {
            received: BTreeSet::from([
                root.operation_id,
                rejected.operation_id,
                child.operation_id,
                grandchild.operation_id,
            ]),
            accepted: BTreeSet::from([root.operation_id]),
            rejected: BTreeSet::from([rejected.operation_id]),
        };
        hosted.validate(&operations).unwrap();
        assert_eq!(
            hosted.blocked_descendants(&operations),
            BTreeSet::from([child.operation_id, grandchild.operation_id])
        );
        let invalid = HostedCollaborationSet {
            received: hosted.received.clone(),
            accepted: BTreeSet::from([root.operation_id, child.operation_id]),
            rejected: BTreeSet::new(),
        };
        assert!(invalid.validate(&operations).is_err());
    }
}
