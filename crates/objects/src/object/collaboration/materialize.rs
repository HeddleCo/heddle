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
    pub fn blocked_descendants(
        &self,
        operations: &BTreeMap<CollabOpId, DecodedCollaborationOperation>,
    ) -> BTreeSet<CollabOpId> {
        self.received
            .difference(&self.accepted)
            .filter(|id| {
                !self.rejected.contains(id)
                    && operations.get(id).is_some_and(|operation| {
                        operation
                            .operation
                            .parents
                            .iter()
                            .any(|parent| !self.accepted.contains(parent))
                    })
            })
            .copied()
            .collect()
    }
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
                !visible.contains(id)
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
    let (title, anchor, visibility, root_turns, mut resolution) = match root {
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
    let mut competing_resolutions = BTreeSet::new();
    let mut selected_conflicts = BTreeMap::new();
    for id in ids.iter().copied().filter(|id| *id != root_id) {
        match &all[&id].operation.body {
            CollaborationOperationBodyV1::AppendTurn { turn } => turns.push((id, turn.clone())),
            CollaborationOperationBodyV1::Resolve { resolution: next } => {
                competing_resolutions.insert(id);
                resolution = Some(next.clone());
            }
            CollaborationOperationBodyV1::Reopen { .. } => resolution = None,
            CollaborationOperationBodyV1::ResolveConflict {
                competing,
                selected,
            } => {
                for operation in competing {
                    selected_conflicts.insert(*operation, *selected);
                }
                let selected_body = &all
                    .get(selected)
                    .ok_or_else(|| {
                        CollaborationCodecError::Invalid(format!(
                            "conflict resolution selects missing operation {selected}"
                        ))
                    })?
                    .operation
                    .body;
                if let CollaborationOperationBodyV1::Resolve {
                    resolution: selected,
                } = selected_body
                {
                    resolution = Some(selected.clone());
                }
            }
            CollaborationOperationBodyV1::Open { .. }
            | CollaborationOperationBodyV1::LegacyImported { .. } => {
                return Err(CollaborationCodecError::Invalid(format!(
                    "discussion {discussion_id} has multiple root operations"
                )));
            }
        }
    }

    let conflicts = competing_resolutions
        .iter()
        .filter(|left| {
            competing_resolutions.iter().any(|right| {
                left != &right
                    && !precedes(**left, *right, all)
                    && !precedes(*right, **left, all)
                    && all[*left].operation.body != all[right].operation.body
                    && selected_conflicts.get(left) != Some(*right)
                    && selected_conflicts.get(right) != Some(*left)
            })
        })
        .copied()
        .collect::<BTreeSet<_>>();
    if !conflicts.is_empty() {
        resolution = None;
    }

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
        if seen.insert(id) {
            if let Some(operation) = operations.get(&id) {
                pending.extend(operation.operation.parents.iter().copied());
            }
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
        let operations = [root.clone(), rejected.clone(), child.clone()]
            .into_iter()
            .map(|operation| (operation.operation_id, operation))
            .collect();
        let hosted = HostedCollaborationSet {
            received: BTreeSet::from([
                root.operation_id,
                rejected.operation_id,
                child.operation_id,
            ]),
            accepted: BTreeSet::from([root.operation_id]),
            rejected: BTreeSet::from([rejected.operation_id]),
        };
        assert_eq!(
            hosted.blocked_descendants(&operations),
            BTreeSet::from([child.operation_id])
        );
    }
}
