// SPDX-License-Identifier: Apache-2.0
//! Debug-build helpers for integration tests that exercise bridge internals.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use objects::object::ChangeId;
use repo::Repository as HeddleRepository;
use sley::{ObjectId, RefPrecondition, Repository as SleyRepository};

use super::git_core::{self, GitProjection, GitProjectionResult, SyncMapping};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefNamespace {
    Branch,
    Tag,
    Note,
}

impl From<RefNamespace> for git_core::RefNamespace {
    fn from(namespace: RefNamespace) -> Self {
        match namespace {
            RefNamespace::Branch => Self::Branch,
            RefNamespace::Tag => Self::Tag,
            RefNamespace::Note => Self::Note,
        }
    }
}

impl From<git_core::RefNamespace> for RefNamespace {
    fn from(namespace: git_core::RefNamespace) -> Self {
        match namespace {
            git_core::RefNamespace::Branch => Self::Branch,
            git_core::RefNamespace::Tag => Self::Tag,
            git_core::RefNamespace::Note => Self::Note,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub name: String,
    pub target: ObjectId,
    pub namespace: RefNamespace,
}

impl From<&RefUpdate> for git_core::RefUpdate {
    fn from(update: &RefUpdate) -> Self {
        Self {
            name: update.name.clone(),
            target: update.target,
            namespace: update.namespace.into(),
        }
    }
}

impl From<git_core::RefUpdate> for RefUpdate {
    fn from(update: git_core::RefUpdate) -> Self {
        Self {
            name: update.name,
            target: update.target,
            namespace: update.namespace.into(),
        }
    }
}

#[derive(Debug)]
pub struct PlannedRefWrite {
    pub full_name: String,
    pub old: Option<ObjectId>,
    pub new: ObjectId,
    pub force: bool,
}

impl From<git_core::PlannedRefWrite> for PlannedRefWrite {
    fn from(write: git_core::PlannedRefWrite) -> Self {
        Self {
            full_name: write.full_name,
            old: write.old,
            new: write.new,
            force: write.force,
        }
    }
}

#[derive(Debug)]
pub struct PlannedRefDelete {
    pub full_name: String,
    pub old: ObjectId,
}

impl From<git_core::PlannedRefDelete> for PlannedRefDelete {
    fn from(delete: git_core::PlannedRefDelete) -> Self {
        Self {
            full_name: delete.full_name,
            old: delete.old,
        }
    }
}

#[derive(Debug)]
pub struct DestinationReconcilePlan {
    pub writes: Vec<PlannedRefWrite>,
    pub deletes: Vec<PlannedRefDelete>,
    pub new_manifest: HashMap<String, ObjectId>,
}

impl From<git_core::DestinationReconcilePlan> for DestinationReconcilePlan {
    fn from(plan: git_core::DestinationReconcilePlan) -> Self {
        Self {
            writes: plan.writes.into_iter().map(Into::into).collect(),
            deletes: plan.deletes.into_iter().map(Into::into).collect(),
            new_manifest: plan.new_manifest,
        }
    }
}

pub fn delete_reference_if_present(repo: &SleyRepository, name: &str) -> GitProjectionResult<()> {
    git_core::delete_reference_if_present(repo, name)
}

pub fn set_reference(
    repo: &SleyRepository,
    name: &str,
    target: ObjectId,
    previous: RefPrecondition,
    message: &str,
) -> GitProjectionResult<()> {
    git_core::set_reference(repo, name, target, previous, message)
}

pub fn read_exported_refs(repo: &SleyRepository) -> GitProjectionResult<HashMap<String, ObjectId>> {
    git_core::read_exported_refs(repo)
}

pub fn write_exported_refs(
    repo: &SleyRepository,
    refs: &HashMap<String, ObjectId>,
) -> GitProjectionResult<()> {
    git_core::write_exported_refs(repo, refs)
}

pub fn read_mirror_managed_refs(
    repo: &SleyRepository,
) -> GitProjectionResult<HashMap<String, ObjectId>> {
    git_core::read_mirror_managed_refs(repo)
}

pub fn write_mirror_managed_refs(
    repo: &SleyRepository,
    refs: &HashMap<String, ObjectId>,
) -> GitProjectionResult<()> {
    git_core::write_mirror_managed_refs(repo, refs)
}

pub fn collect_managed_ref_updates(
    repo: &SleyRepository,
    record: &HashMap<String, ObjectId>,
) -> GitProjectionResult<Vec<RefUpdate>> {
    git_core::collect_managed_ref_updates(repo, record)
        .map(|updates| updates.into_iter().map(Into::into).collect())
}

pub fn plan_destination_reconcile(
    mirror_repo: &SleyRepository,
    served_frontier: &[RefUpdate],
    creatable_names: Option<&HashSet<String>>,
    old_at_destination: &HashMap<String, ObjectId>,
    previously_exported: &HashMap<String, ObjectId>,
    force: bool,
) -> GitProjectionResult<DestinationReconcilePlan> {
    let served_frontier = served_frontier
        .iter()
        .map(git_core::RefUpdate::from)
        .collect::<Vec<_>>();
    git_core::plan_destination_reconcile(
        mirror_repo,
        &served_frontier,
        creatable_names,
        old_at_destination,
        previously_exported,
        force,
    )
    .map(Into::into)
}

pub fn set_git_repo_path(bridge: &mut GitProjection<'_>, path: PathBuf) {
    bridge.git_repo_path = Some(path);
}

pub fn mapping<'a>(bridge: &'a GitProjection<'_>) -> &'a SyncMapping {
    &bridge.mapping
}

pub fn mapping_mut<'a>(bridge: &'a mut GitProjection<'_>) -> &'a mut SyncMapping {
    &mut bridge.mapping
}

pub fn commit_message_overrides<'a>(
    bridge: &'a GitProjection<'_>,
) -> &'a HashMap<ChangeId, String> {
    &bridge.commit_message_overrides
}

pub fn set_commit_message_override(
    bridge: &mut GitProjection<'_>,
    state_id: ChangeId,
    message: String,
) {
    bridge.set_commit_message_override(state_id, message);
}

pub fn mapping_path(bridge: &GitProjection<'_>) -> PathBuf {
    bridge.mapping_path()
}

pub fn save_mapping_to_disk(bridge: &GitProjection<'_>) -> GitProjectionResult<()> {
    bridge.save_mapping_to_disk()
}

pub fn build_existing_mapping(
    bridge: &mut GitProjection<'_>,
    git_repo_path: Option<&Path>,
) -> GitProjectionResult<()> {
    bridge.build_existing_mapping(git_repo_path)
}

pub fn stage_ingest_source_in_mirror(
    bridge: &mut GitProjection<'_>,
    source: &Path,
    refs: &[String],
) -> GitProjectionResult<()> {
    bridge.stage_ingest_source_in_mirror(source, refs)
}

pub fn seed_ingest_identity_mappings_from_mirror(
    bridge: &mut GitProjection<'_>,
    repo: &SleyRepository,
) -> GitProjectionResult<()> {
    bridge.seed_ingest_identity_mappings_from_mirror(repo)
}

pub fn open_git_repo(bridge: &GitProjection<'_>) -> GitProjectionResult<SleyRepository> {
    bridge.open_git_repo()
}

pub fn consolidate_mirror(bridge: &GitProjection<'_>) -> GitProjectionResult<usize> {
    bridge.consolidate_mirror()
}

pub fn heddle_repo<'a>(bridge: &'a GitProjection<'a>) -> &'a HeddleRepository {
    bridge.heddle_repo
}

pub fn open_repo(path: &Path) -> GitProjectionResult<SleyRepository> {
    git_core::open_repo(path)
}

/// Drive the #568 P1 checkout-materialization closure walk directly: reconstruct
/// faithful commits from heddle state into `object_repo`, mirror-backstop the
/// lossy residual. Used by the bridge integration tests to prove a faithful
/// commit materializes WITHOUT the mirror holding its objects (and that the OID
/// safety gate fires on a divergence).
pub fn materialize_checkout_closure_from_state(
    bridge: &GitProjection<'_>,
    mirror_repo: &SleyRepository,
    object_repo: &SleyRepository,
    tip_state_id: &ChangeId,
    tip_oid: ObjectId,
    excluded: &HashSet<ObjectId>,
) -> GitProjectionResult<()> {
    git_core::materialize_checkout_closure_from_state(
        bridge.heddle_repo,
        &bridge.mapping,
        mirror_repo,
        object_repo,
        tip_state_id,
        tip_oid,
        excluded,
    )
}
