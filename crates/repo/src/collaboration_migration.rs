// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
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
            CollaborationIdempotencyKey::new(format!(
                "legacy:{}:{}:{}",
                primary.state_id.to_string_full(),
                primary.attachment_id.as_hash().to_hex(),
                primary.blob_hash.to_hex()
            ))
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

fn legacy_discussion_migration_marker(repository: &Repository) -> std::path::PathBuf {
    repository
        .heddle_dir()
        .join("collaboration/migrations/legacy-discussions-v1")
}

/// Claim the one-shot legacy-discussion migration marker without running the
/// migration. Idempotent.
///
/// Hosted repositories carry their discussions as server-minted `Discussions`
/// state-attachments that ride the normal pull. Those attachments are the
/// *transport* form of the hosted `CollaborationService` discussions, which the
/// hosted-sync bridge materializes into the op-log directly (RPC read path). If
/// the one-shot legacy blob→op-log migration were also allowed to run over the
/// same pulled attachments it would (a) duplicate every hosted discussion and
/// (b) diverge on multi-turn discussions, whose `AppendTurn` supersede history
/// leaves several differing blobs on one state. A fresh clone has no genuine
/// *local* legacy discussions to migrate, so claiming the marker at hosted-sync
/// time is safe and leaves the RPC-materialized op-log authoritative.
pub fn mark_legacy_discussions_migrated(repository: &Repository) -> Result<()> {
    let marker = legacy_discussion_migration_marker(repository);
    if marker.exists() {
        return Ok(());
    }
    fs::create_dir_all(marker.parent().expect("migration marker has parent"))?;
    write_file_atomic(&marker, b"1\n")?;
    Ok(())
}

pub fn migrate_legacy_discussions_once(
    repository: &Repository,
    store: &CollaborationStore,
    import_actor: Attribution,
) -> Result<Option<LegacyDiscussionMigrationReport>> {
    let marker = legacy_discussion_migration_marker(repository);
    if marker.exists() {
        return Ok(None);
    }
    let plan = plan_legacy_discussion_migration(repository, import_actor)?;
    let report = apply_legacy_discussion_migration(repository, store, &plan)?;
    fs::create_dir_all(marker.parent().expect("migration marker has parent"))?;
    write_file_atomic(&marker, b"1\n")?;
    Ok(Some(report))
}

fn collect_candidates(repository: &Repository) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for state_id in repository.reachable_states().map_err(|error| {
        HeddleError::InvalidObject(format!(
            "walk reachable states for discussion migration: {error}"
        ))
    })? {
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
    Ok(LegacySourceLocator::new(
        candidate.state_id,
        candidate.attachment_id,
        candidate.blob_hash,
    ))
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::object::{
        Blob, DiscussionTurn, Principal, StateAttachment, SymbolAnchor, VisibilityTier,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::CollaborationWriteDisposition;

    fn actor() -> Attribution {
        Attribution::human(Principal::new("Importer", "importer@example.com"))
    }

    fn discussion(id: &str, body: &str, state_id: StateId) -> Discussion {
        Discussion {
            id: id.to_string(),
            anchor: SymbolAnchor::new("src/lib.rs", "run"),
            opened_against_state: state_id,
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: Principal::new("Ada", "ada@example.com"),
                body: body.to_string(),
                posted_at: 1_700_000_000,
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            orphaned: false,
            visibility: VisibilityTier::default(),
            resolved_annotation_id: None,
        }
    }

    fn candidate(state_id: StateId, body: &str) -> Candidate {
        Candidate {
            state_id,
            attachment_id: StateAttachmentId::from_hash(objects::object::ContentHash::from_bytes(
                [4; 32],
            )),
            blob_hash: objects::object::ContentHash::from_bytes([5; 32]),
            discussion: discussion("legacy-1", body, state_id),
        }
    }

    #[test]
    fn maximal_head_divergence_is_a_blocker() {
        let left = StateId::from_bytes([1; 32]);
        let right = StateId::from_bytes([2; 32]);
        let candidates = vec![candidate(left, "left"), candidate(right, "right")];
        let parents = BTreeMap::from([(left, Vec::new()), (right, Vec::new())]);
        let maximal = maximal_candidates(&candidates, &parents);
        assert_eq!(maximal.len(), 2);
        assert_ne!(maximal[0].discussion, maximal[1].discussion);
    }

    #[test]
    fn migration_reads_detached_attachment_and_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let repository = Repository::init_default(temp.path()).unwrap();
        let state_id = repository.head().unwrap().unwrap();
        let bytes = DiscussionsBlob::new(vec![discussion("legacy-1", "why?", state_id)])
            .encode()
            .unwrap();
        let blob_hash = repository.store().put_blob(&Blob::new(bytes)).unwrap();
        repository
            .put_state_attachment(&StateAttachment {
                state_id,
                body: StateAttachmentBody::Discussions(blob_hash),
                attribution: actor(),
                created_at: Utc::now(),
                supersedes: None,
            })
            .unwrap();
        let obsolete = repository.heddle_dir().join("discussions");
        fs::create_dir_all(&obsolete).unwrap();
        fs::write(obsolete.join("legacy.msgpack"), b"legacy").unwrap();

        let store = CollaborationStore::open(repository.heddle_dir()).unwrap();
        let plan = plan_legacy_discussion_migration(&repository, actor()).unwrap();
        assert!(plan.is_ready());
        assert_eq!(plan.items.len(), 1);
        let first = apply_legacy_discussion_migration(&repository, &store, &plan).unwrap();
        let second = apply_legacy_discussion_migration(&repository, &store, &plan).unwrap();
        assert_eq!(
            first.writes[0].disposition,
            CollaborationWriteDisposition::Created
        );
        assert_eq!(
            second.writes[0].disposition,
            CollaborationWriteDisposition::IdempotentReplay
        );
        assert!(!obsolete.exists());
        assert_eq!(
            repository
                .latest_state_attachment(&state_id, crate::StateAttachmentKind::Discussions)
                .unwrap()
                .unwrap()
                .body,
            StateAttachmentBody::Discussions(blob_hash)
        );
    }
}
