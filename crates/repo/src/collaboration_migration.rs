// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

use objects::{
    error::{HeddleError, Result},
    object::{
        Attribution, CollabOpId, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, Discussion,
        DiscussionRecordId, DiscussionResolution, DiscussionTurnV1, DiscussionsBlob,
        LegacyDiscussionId, LegacyDiscussionResolutionV1, LegacySourceLocator, StateAttachmentBody,
        StateAttachmentId, StateId,
    },
    store::ObjectStore,
};
use serde::Serialize;

use crate::{CollaborationStore, CollaborationWriteOutcome, Repository};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LegacyDiscussionMigrationItem {
    pub discussion_id: DiscussionRecordId,
    pub operation_id: CollabOpId,
    pub sources: Vec<LegacySourceLocator>,
    #[serde(skip)]
    operation: CollaborationOperationEnvelope,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LegacyDiscussionMigrationBlocker {
    pub legacy_discussion_id: LegacyDiscussionId,
    pub maximal_states: Vec<StateId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LegacyDiscussionMigrationPlan {
    pub items: Vec<LegacyDiscussionMigrationItem>,
    pub blockers: Vec<LegacyDiscussionMigrationBlocker>,
}

impl LegacyDiscussionMigrationPlan {
    pub fn is_ready(&self) -> bool {
        self.blockers.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LegacyDiscussionMigrationReport {
    pub writes: Vec<CollaborationWriteOutcome>,
    pub removed_legacy_paths: Vec<String>,
}

#[derive(Clone)]
struct Candidate {
    state_id: StateId,
    attachment_id: StateAttachmentId,
    blob_hash: objects::object::ContentHash,
    discussion: Discussion,
}

pub fn plan_legacy_discussion_migration(
    repository: &Repository,
    import_actor: Attribution,
) -> Result<LegacyDiscussionMigrationPlan> {
    let candidates = collect_candidates(repository)?;
    let parents = collect_parents(repository, &candidates)?;
    let mut grouped: BTreeMap<LegacyDiscussionId, Vec<Candidate>> = BTreeMap::new();
    for candidate in candidates {
        let id = LegacyDiscussionId::new(candidate.discussion.id.clone())
            .map_err(HeddleError::InvalidObject)?;
        grouped.entry(id).or_default().push(candidate);
    }

    let mut plan = LegacyDiscussionMigrationPlan::default();
    for (legacy_id, versions) in grouped {
        let maximal = maximal_candidates(&versions, &parents);
        let agree = maximal.first().is_some_and(|first| {
            maximal
                .iter()
                .all(|candidate| candidate.discussion == first.discussion)
        });
        if !agree {
            plan.blockers.push(LegacyDiscussionMigrationBlocker {
                legacy_discussion_id: legacy_id,
                maximal_states: maximal.iter().map(|value| value.state_id).collect(),
            });
            continue;
        }
        let selected = maximal[0];
        let mut sources = versions
            .iter()
            .map(source_locator)
            .collect::<Result<Vec<_>>>()?;
        sources.sort();
        sources.dedup();
        let primary = source_locator(selected)?;
        let discussion_id = DiscussionRecordId::for_legacy_source(
            &primary,
            selected.discussion.opened_at.saturating_mul(1000),
        );
        let operation = CollaborationOperationEnvelope::new(
            discussion_id,
            Vec::new(),
            CollaborationIdempotencyKey::new(format!("legacy:{}", primary.as_str()))
                .map_err(HeddleError::InvalidObject)?,
            import_actor.clone(),
            selected.discussion.opened_at.saturating_mul(1000),
            legacy_body(selected, legacy_id, primary, &sources)?,
        )
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
        let bytes = operation
            .encode()
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
        plan.items.push(LegacyDiscussionMigrationItem {
            discussion_id,
            operation_id: CollabOpId::for_bytes(&bytes),
            sources,
            operation,
        });
    }
    Ok(plan)
}

pub fn apply_legacy_discussion_migration(
    repository: &Repository,
    store: &CollaborationStore,
    plan: &LegacyDiscussionMigrationPlan,
) -> Result<LegacyDiscussionMigrationReport> {
    if !plan.is_ready() {
        return Err(HeddleError::InvalidObject(format!(
            "legacy discussion migration has {} divergent maximal head set(s)",
            plan.blockers.len()
        )));
    }
    for item in &plan.items {
        let actual = CollabOpId::for_bytes(
            &item
                .operation
                .encode()
                .map_err(|error| HeddleError::InvalidObject(error.to_string()))?,
        );
        if actual != item.operation_id {
            return Err(HeddleError::InvalidObject(format!(
                "planned collaboration operation {} changed before apply",
                item.operation_id
            )));
        }
    }

    let mut report = LegacyDiscussionMigrationReport::default();
    for item in &plan.items {
        report.writes.push(store.write_operation(&item.operation)?);
    }
    for name in ["discussion-state-blobs", "discussions"] {
        let path = repository.heddle_dir().join(name);
        match fs::remove_dir_all(&path) {
            Ok(()) => report.removed_legacy_paths.push(path.display().to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(report)
}

fn collect_candidates(repository: &Repository) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for state_id in repository.reachable_states()? {
        let attachments = repository.list_state_attachments(&state_id)?;
        for attachment in attachments {
            let StateAttachmentBody::Discussions(blob_hash) = attachment.body else {
                continue;
            };
            let blob = repository.store().get_blob(&blob_hash)?.ok_or_else(|| {
                HeddleError::InvalidObject(format!(
                    "discussion attachment for {state_id} references missing blob {blob_hash}"
                ))
            })?;
            let discussions = DiscussionsBlob::decode(blob.content())
                .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
            candidates.extend(
                discussions
                    .discussions
                    .into_iter()
                    .map(|discussion| Candidate {
                        state_id,
                        attachment_id: attachment.id(),
                        blob_hash,
                        discussion,
                    }),
            );
        }
    }
    Ok(candidates)
}

fn collect_parents(
    repository: &Repository,
    candidates: &[Candidate],
) -> Result<BTreeMap<StateId, Vec<StateId>>> {
    let mut parents = BTreeMap::new();
    let mut pending = candidates
        .iter()
        .map(|value| value.state_id)
        .collect::<Vec<_>>();
    while let Some(state_id) = pending.pop() {
        if parents.contains_key(&state_id) {
            continue;
        }
        let state = repository.store().get_state(&state_id)?.ok_or_else(|| {
            HeddleError::InvalidObject(format!("migration source state {state_id} is missing"))
        })?;
        pending.extend(state.parents.iter().copied());
        parents.insert(state_id, state.parents);
    }
    Ok(parents)
}

fn maximal_candidates<'a>(
    candidates: &'a [Candidate],
    parents: &BTreeMap<StateId, Vec<StateId>>,
) -> Vec<&'a Candidate> {
    let candidate_ids = candidates
        .iter()
        .map(|value| value.state_id)
        .collect::<BTreeSet<_>>();
    let mut nonmaximal = BTreeSet::new();
    for id in &candidate_ids {
        let mut pending = parents.get(id).cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(parent) = pending.pop() {
            if seen.insert(parent) {
                if candidate_ids.contains(&parent) {
                    nonmaximal.insert(parent);
                }
                pending.extend(parents.get(&parent).cloned().unwrap_or_default());
            }
        }
    }
    candidates
        .iter()
        .filter(|value| !nonmaximal.contains(&value.state_id))
        .collect()
}

fn source_locator(candidate: &Candidate) -> Result<LegacySourceLocator> {
    LegacySourceLocator::new(format!(
        "state/{}/attachment/{}/blob/{}",
        candidate.state_id, candidate.attachment_id, candidate.blob_hash
    ))
    .map_err(HeddleError::InvalidObject)
}

fn legacy_body(
    candidate: &Candidate,
    legacy_discussion_id: LegacyDiscussionId,
    source: LegacySourceLocator,
    sources: &[LegacySourceLocator],
) -> Result<CollaborationOperationBodyV1> {
    let discussion = &candidate.discussion;
    let turns = discussion
        .turns
        .iter()
        .map(|turn| DiscussionTurnV1::new(turn.body.clone()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
    let resolution = match &discussion.resolution {
        DiscussionResolution::Open => LegacyDiscussionResolutionV1::Open,
        DiscussionResolution::ResolvedByEdit { state_id } => {
            LegacyDiscussionResolutionV1::AddressedByState {
                state_id: *state_id,
            }
        }
        DiscussionResolution::ResolvedIntoAnnotation { annotation_id } => {
            LegacyDiscussionResolutionV1::Annotation {
                annotation_id: annotation_id.clone(),
            }
        }
        DiscussionResolution::Dismissed { reason } => LegacyDiscussionResolutionV1::Dismissed {
            reason: reason.clone(),
        },
    };
    Ok(CollaborationOperationBodyV1::LegacyImported {
        aliases: sources
            .iter()
            .filter(|value| *value != &source)
            .cloned()
            .collect(),
        source,
        legacy_discussion_id,
        title: format!("{} in {}", discussion.anchor.symbol, discussion.anchor.file),
        anchor: CollaborationAnchor::Symbol {
            state_id: discussion.opened_against_state,
            path: discussion.anchor.file.clone(),
            symbol: discussion.anchor.symbol.clone(),
        },
        visibility: discussion.visibility.clone(),
        turns,
        resolution,
    })
}
