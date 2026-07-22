// SPDX-License-Identifier: Apache-2.0
//! Snapshot operations for Repository.

use std::collections::BTreeSet;

use objects::{
    lock::RepositoryLockExt,
    object::{
        Attribution, Blob, ChangeLineage, ContentHash, State, StateAttachment, StateAttachmentBody,
        StateId, Tree, TreeEntry,
    },
    store::{ObjectStore, SnapshotCommitArtifact, SnapshotCommitDescriptor},
};
use oplog::{IsolationKey, OpRecord};
use refs::Head;
use tracing::{debug, instrument};

use super::{HeddleError, Repository, Result, repository_tree::TreeBuildProfile};
use crate::{
    atomic::{AtomicMutation, RewindLedger, StagedCommit, Tx, execute, execute_reconstructible},
    worktree_ignore::WorktreeIgnoreMatcher,
    worktree_walk::{
        WalkDirectory, WalkEntry, WorktreeWalkPolicy, read_file_hash, validate_symlink_target,
        walk_worktree,
    },
};

#[derive(Debug, Clone, Default)]
pub struct SnapshotProfile {
    pub tree_walk_ms: u128,
    pub blob_prep_ms: u128,
    pub blob_write_ms: u128,
    pub tree_write_ms: u128,
    pub state_ref_oplog_ms: u128,
    /// Whole atomic executor, including the staged object phases above and the
    /// durable oplog commit. Subtract the staged fields to isolate commit
    /// overhead without perturbing the generic transaction executor.
    pub atomic_execute_ms: u128,
    pub ref_publish_ms: u128,
}

#[derive(Debug, Clone)]
pub struct SnapshotExecution {
    pub state: State,
    pub tree: Tree,
    pub profile: SnapshotProfile,
}

enum SnapshotSource {
    Worktree { fingerprint: ContentHash },
    SuppliedTree(Tree),
    SuppliedTreeWithBlobs { tree: Tree, blobs: Vec<Blob> },
}

struct SnapshotDetails {
    intent: Option<String>,
    confidence: Option<f32>,
    attribution: Attribution,
    lineage: Vec<ChangeLineage>,
}

struct PreparedSnapshotArtifact {
    blobs: Vec<(ContentHash, Vec<u8>)>,
    tree: Tree,
    state: State,
    attachments: Vec<StateAttachment>,
}

struct SnapshotMutation<'a> {
    repo: &'a Repository,
    source: SnapshotSource,
    details: SnapshotDetails,
    prev_head: Option<StateId>,
    head: Head,
    transaction_id: String,
    /// Set by `apply` when the automatic capture-time default-visibility binding
    /// folds a `StateVisibilitySet` into this snapshot's batch (heddle#317 / PR
    /// #529 P1): `(state, sidecar-before-the-binding)`. `rewind` restores the
    /// sidecar to that before-image if the batch fails to commit, so a rewound
    /// snapshot never leaves its auto-applied tier behind.
    staged_visibility_rewind: Option<(StateId, Option<Vec<u8>>)>,
    prepared_artifact: Option<PreparedSnapshotArtifact>,
}

impl<'a> SnapshotMutation<'a> {
    fn new(
        repo: &'a Repository,
        source: SnapshotSource,
        details: SnapshotDetails,
        prev_head: Option<StateId>,
        head: Head,
    ) -> Self {
        let transaction_id = snapshot_transaction_id(repo, &source, &details, &head);
        Self {
            repo,
            source,
            details,
            prev_head,
            head,
            transaction_id,
            staged_visibility_rewind: None,
            prepared_artifact: None,
        }
    }

    fn thread(&self) -> Option<String> {
        match &self.head {
            Head::Attached { thread } => Some(thread.to_string()),
            Head::Detached { .. } => None,
        }
    }
}

impl AtomicMutation for SnapshotMutation<'_> {
    type Output = SnapshotExecution;

    fn transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        match &self.head {
            Head::Attached { thread } => {
                keys.insert(IsolationKey::Thread(thread.to_string()));
            }
            Head::Detached { .. } => {
                keys.insert(IsolationKey::LocalHead {
                    scope: self.repo.op_scope(),
                });
            }
        }
        Ok(keys)
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<Self::Output>> {
        self.repo.store.begin_snapshot_write_batch()?;
        let execution = self.stage_snapshot_objects()?;

        objects::fault_inject::maybe_panic_at("snapshot_after_stage_before_atomic_commit");
        #[cfg(test)]
        maybe_snapshot_fault(SnapshotFault::AfterStageBeforeAtomicCommit);

        let mut records = vec![OpRecord::Snapshot {
            new_state: execution.state.id(),
            prev_head: self.prev_head,
            head: self.thread().is_none().then_some(execution.state.id()),
            thread: self.thread(),
        }];

        // heddle#317 / PR #529 P1: fold the automatic capture-time
        // default-visibility binding into THIS snapshot's batch so one `heddle
        // undo` reverts the snapshot AND its auto-applied default tier together
        // (the old separate trailing batch made the first undo restore only the
        // sidecar). `apply` runs under the snapshot write lock, so the sidecar
        // write must not re-enter the non-reentrant repo lock (`lock_held =
        // true`); `rewind` restores the sidecar if this batch fails to commit.
        if let Some(binding) = self
            .repo
            .stage_default_visibility_binding(&execution.state.id(), true)
            .map_err(|e| HeddleError::Io(std::io::Error::other(format!("{e:#}"))))?
        {
            self.staged_visibility_rewind = Some((execution.state.id(), binding.prior_sidecar));
            records.push(binding.record);
        }

        Ok(StagedCommit::new(execution, records))
    }

    fn rewind(&mut self, _ledger: &RewindLedger) -> Result<()> {
        self.repo.store.abort_snapshot_write_batch();
        // Roll the folded default-visibility binding back to its before-image so
        // a rewound snapshot leaves no orphaned auto-applied tier (heddle#317).
        // Idempotent: `take` makes a second rewind a no-op, and
        // `restore_state_visibility_sidecar` is an absolute write-or-delete.
        if let Some((state, prior)) = self.staged_visibility_rewind.take() {
            self.repo
                .restore_state_visibility_sidecar(&state, prior)
                .map_err(|e| HeddleError::Io(std::io::Error::other(format!("{e:#}"))))?;
        }
        Ok(())
    }

    fn reconstruct_committed_output(
        &self,
        committed_records: &[OpRecord],
        this_run: Self::Output,
    ) -> Result<Self::Output> {
        let Some(committed_state) = committed_records.iter().find_map(|record| match record {
            OpRecord::Snapshot { new_state, .. } => Some(*new_state),
            OpRecord::Goto { .. }
            | OpRecord::ThreadCreate { .. }
            | OpRecord::ThreadDelete { .. }
            | OpRecord::ThreadUpdate { .. }
            | OpRecord::Fork { .. }
            | OpRecord::Collapse { .. }
            | OpRecord::MarkerCreate { .. }
            | OpRecord::MarkerDelete { .. }
            | OpRecord::Checkpoint { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::EphemeralThreadCollapse { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::Redact { .. }
            | OpRecord::Purge { .. }
            | OpRecord::FastForward { .. }
            | OpRecord::GitCheckpoint { .. }
            | OpRecord::RemoteThreadUpdate { .. }
            | OpRecord::RemoteThreadDelete { .. }
            | OpRecord::UndoRecoveryUpdate { .. }
            | OpRecord::StateVisibilitySet { .. }
            | OpRecord::StateVisibilityPromote { .. } => None,
        }) else {
            return Ok(this_run);
        };
        let Some(state) = self.repo.store.get_state(&committed_state)? else {
            return Ok(this_run);
        };
        let Some(tree) = self.repo.store.get_tree(&state.tree)? else {
            return Ok(this_run);
        };
        Ok(SnapshotExecution {
            state,
            tree,
            profile: SnapshotProfile::default(),
        })
    }
}

impl SnapshotMutation<'_> {
    fn stage_snapshot_objects(&mut self) -> Result<SnapshotExecution> {
        debug!("Building tree from worktree");
        let (tree, tree_profile, supplied_blobs) = match &self.source {
            SnapshotSource::Worktree { .. } => {
                let (tree, profile) = self.build_worktree_tree()?;
                (tree, profile, None)
            }
            SnapshotSource::SuppliedTree(tree) => (tree.clone(), TreeBuildProfile::default(), None),
            SnapshotSource::SuppliedTreeWithBlobs { tree, blobs } => (
                tree.clone(),
                TreeBuildProfile::default(),
                Some(
                    blobs
                        .iter()
                        .map(|blob| (blob.hash(), blob.content().to_vec()))
                        .collect(),
                ),
            ),
        };
        debug!(duration_ms = tree_profile.tree_walk_ms, "Tree built");

        debug!("Storing tree");
        let root_tree_write_start = std::time::Instant::now();
        let tree_hash = if supplied_blobs.is_some() {
            tree.hash()
        } else {
            self.repo.store.put_tree(&tree)?
        };
        let root_tree_write_ms = root_tree_write_start.elapsed().as_millis();

        let parents = match self.prev_head {
            Some(id) => vec![id],
            None => vec![],
        };

        let mut state = State::new_snapshot(tree_hash, parents, self.details.attribution.clone());

        if let Some(intent) = self.details.intent.clone() {
            state = state.with_intent(intent);
        }

        if let Some(confidence) = self.details.confidence {
            state = state.with_confidence(confidence);
        }

        if !self.details.lineage.is_empty() {
            state = state.with_lineage(self.details.lineage.clone());
        }

        let inherited_context = if let Some(parent_id) = self.prev_head
            && let Some(parent_state) = self.repo.store.get_state(&parent_id)?
        {
            self.repo.inherit_parent_context(&parent_state)?
        } else {
            None
        };

        #[cfg(feature = "tree-sitter-symbols")]
        let mut risk_signals = None;
        #[cfg(feature = "tree-sitter-symbols")]
        let mut discussions = None;
        #[cfg(feature = "tree-sitter-symbols")]
        let mut semantic_index = None;

        #[cfg(feature = "tree-sitter-symbols")]
        {
            let source_blobs =
                supplied_blobs
                    .as_ref()
                    .map(|blobs: &Vec<(ContentHash, Vec<u8>)>| {
                        blobs
                            .iter()
                            .map(|(hash, bytes)| (*hash, bytes.as_slice()))
                            .collect::<std::collections::HashMap<_, _>>()
                    });
            let prior_state = match self.prev_head {
                Some(id) => self.repo.store.get_state(&id).ok().flatten(),
                None => None,
            };
            match self
                .repo
                .compute_and_persist_signals(prior_state.as_ref(), &state)
            {
                Ok(Some(hash)) => risk_signals = Some(hash),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "risk signal computation failed; continuing without signals");
                }
            }

            // Eager semantic index: parse only changed blobs, reusing the
            // parent index for unchanged subtrees. Never fails the capture.
            match self.repo.compute_and_persist_semantic_index_for_tree(
                prior_state.as_ref(),
                &tree,
                source_blobs.as_ref(),
            ) {
                Ok(Some(hash)) => semantic_index = Some(hash),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "semantic index computation failed; continuing without index");
                }
            }

            if let Some(parent_state) = prior_state.as_ref() {
                match self.repo.compute_and_persist_discussion_anchor_travel(
                    parent_state,
                    &tree,
                    source_blobs.as_ref(),
                ) {
                    Ok(Some(hash)) => discussions = Some(hash),
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(error = %err, "discussion anchor travel failed; continuing without discussions");
                    }
                }
            }
        }

        // Structured native authoring owns the entire newly-authored immutable
        // closure already. Keep it in memory until exact-once and isolation
        // validation succeed under the oplog lock; its marked pack then becomes
        // the single authoritative durable commit barrier.
        let packed_snapshot = supplied_blobs.is_some();
        let mut attachments = if packed_snapshot {
            self.repo
                .authored_state_signature_attachment(&state)
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        // Persist the immutable state before its independently-addressed metadata.
        let state_ref_oplog_start = std::time::Instant::now();
        if !packed_snapshot {
            self.repo.put_authored_state(&state)?;
        }
        if let Some(context) = inherited_context {
            let attachment = StateAttachment {
                state_id: state.id(),
                body: StateAttachmentBody::Context(context),
                attribution: state.attribution.clone(),
                created_at: chrono::Utc::now(),
                supersedes: None,
            };
            if packed_snapshot {
                attachments.push(attachment);
            } else {
                self.repo.put_state_attachment(&attachment)?;
            }
        }
        #[cfg(feature = "tree-sitter-symbols")]
        for body in [
            risk_signals.map(StateAttachmentBody::RiskSignals),
            discussions.map(StateAttachmentBody::Discussions),
            semantic_index.map(StateAttachmentBody::SemanticIndex),
        ]
        .into_iter()
        .flatten()
        {
            let attachment = StateAttachment {
                state_id: state.id(),
                body,
                attribution: state.attribution.clone(),
                created_at: chrono::Utc::now(),
                supersedes: None,
            };
            if packed_snapshot {
                attachments.push(attachment);
            } else {
                self.repo.put_state_attachment(&attachment)?;
            }
        }
        if let Some(blobs) = supplied_blobs {
            self.prepared_artifact = Some(PreparedSnapshotArtifact {
                blobs,
                tree: tree.clone(),
                state: state.clone(),
                attachments,
            });
        }
        self.repo.store.flush_snapshot_write_batch()?;

        Ok(SnapshotExecution {
            state,
            tree,
            profile: snapshot_profile_from_tree(
                tree_profile,
                root_tree_write_ms,
                state_ref_oplog_start.elapsed().as_millis(),
            ),
        })
    }

    fn install_prepared_artifact(
        &mut self,
        base_oplog_head_id: u64,
        records: &[OpRecord],
    ) -> Result<(SnapshotCommitDescriptor, u128)> {
        let prepared = self.prepared_artifact.take().ok_or_else(|| {
            HeddleError::Conflict("structured snapshot artifact was not staged".to_string())
        })?;
        let artifact = SnapshotCommitArtifact {
            schema: objects::store::SNAPSHOT_COMMIT_ARTIFACT_SCHEMA,
            transaction_id: self.transaction_id.clone(),
            scope: self.repo.op_scope(),
            base_oplog_head_id,
            state: prepared.state.id(),
            encoded_records: records
                .iter()
                .map(rmp_serde::to_vec_named)
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        let started = std::time::Instant::now();
        let descriptor = self.repo.store.put_committed_snapshot_objects_packed(
            prepared.blobs,
            &prepared.tree,
            &prepared.state,
            prepared.attachments,
            artifact,
        )?;
        let elapsed_ms = started.elapsed().as_millis();
        objects::fault_inject::maybe_panic_at("snapshot_after_artifact_commit_before_oplog_view");
        #[cfg(test)]
        maybe_snapshot_fault(SnapshotFault::AfterArtifactCommitBeforeOplogView);
        Ok((descriptor, elapsed_ms))
    }

    fn build_worktree_tree(&self) -> Result<(Tree, TreeBuildProfile)> {
        let baseline_tree = match self.prev_head {
            Some(prev_head) => {
                let state = self
                    .repo
                    .store
                    .get_state(&prev_head)?
                    .ok_or(HeddleError::StateNotFound(prev_head))?;
                Some(self.repo.store.get_tree(&state.tree)?.ok_or_else(|| {
                    HeddleError::NotFound(format!("tree {} (for state {})", state.tree, prev_head))
                })?)
            }
            None => None,
        };

        let manifest_context: Option<(String, crate::thread_manifest::ThreadManifest)> =
            match &self.head {
                Head::Attached { thread } => {
                    match crate::thread_manifest::read_manifest(self.repo.heddle_dir(), thread) {
                        Ok(Some(m)) => {
                            let self_root_canonical =
                                super::repository_thread_materialize::canonical_worktree_path(
                                    &self.repo.root,
                                );
                            if m.worktree_path == self_root_canonical {
                                Some((thread.to_string(), m))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
                Head::Detached { .. } => None,
            };

        match manifest_context.as_ref() {
            Some((_, manifest)) => self.repo.build_tree_profiled_with_stat_cache_against(
                &self.repo.root,
                baseline_tree.as_ref(),
                manifest,
            ),
            None => self
                .repo
                .build_tree_profiled_against(&self.repo.root, baseline_tree.as_ref()),
        }
    }
}

#[derive(Default)]
struct SnapshotFingerprintState {
    entries: Vec<TreeEntry>,
}

struct SnapshotFingerprintOutput {
    tree: Tree,
}

struct SnapshotFingerprintPolicy<'a> {
    walk_root: &'a std::path::Path,
}

impl<'a> SnapshotFingerprintPolicy<'a> {
    fn new(walk_root: &'a std::path::Path) -> Self {
        Self { walk_root }
    }
}

impl WorktreeWalkPolicy for SnapshotFingerprintPolicy<'_> {
    type DirectoryState = SnapshotFingerprintState;
    type Output = SnapshotFingerprintOutput;

    fn enter_directory(
        &mut self,
        _directory: &WalkDirectory<'_>,
        _tree: Option<&Tree>,
    ) -> Result<Self::DirectoryState> {
        Ok(SnapshotFingerprintState::default())
    }

    fn visit_file(
        &mut self,
        entry: WalkEntry<'_>,
        _tree_entry: Option<&TreeEntry>,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        let hash = read_file_hash(entry.path, entry.metadata.len())?;
        state.entries.push(TreeEntry::file(
            entry.name.to_string(),
            hash,
            entry.executable,
        )?);
        Ok(())
    }

    fn visit_symlink(
        &mut self,
        entry: WalkEntry<'_>,
        _tree_entry: Option<&TreeEntry>,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        let target = std::fs::read_link(entry.path)?;
        let symlink_dir = entry.path.parent().unwrap_or(self.walk_root);
        if !validate_symlink_target(self.walk_root, symlink_dir, &target) {
            return Err(HeddleError::InvalidSymlinkTarget(target));
        }

        let blob = Blob::new(objects::util::symlink_target_bytes(&target));
        state
            .entries
            .push(TreeEntry::symlink(entry.name.to_string(), blob.hash())?);
        Ok(())
    }

    fn visit_directory_output(
        &mut self,
        entry: WalkEntry<'_>,
        _tree_entry: Option<&TreeEntry>,
        subtree: Self::Output,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        state.entries.push(TreeEntry::directory(
            entry.name.to_string(),
            subtree.tree.hash(),
        )?);
        Ok(())
    }

    fn visit_missing(
        &mut self,
        _rel_path: &std::path::Path,
        _tree_entry: &TreeEntry,
        _state: &mut Self::DirectoryState,
    ) -> Result<()> {
        Ok(())
    }

    fn leave_directory(
        &mut self,
        _directory: &WalkDirectory<'_>,
        _tree: Option<&Tree>,
        state: Self::DirectoryState,
    ) -> Result<Self::Output> {
        Ok(SnapshotFingerprintOutput {
            tree: Tree::from_entries(state.entries),
        })
    }
}

fn snapshot_transaction_id(
    repo: &Repository,
    source: &SnapshotSource,
    details: &SnapshotDetails,
    head: &Head,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"snapshot-v1\0");
    hasher.update(repo.op_scope().as_bytes());
    hasher.update(b"\0");
    hasher.update(repo.root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    match source {
        SnapshotSource::Worktree { fingerprint } => {
            hasher.update(b"worktree\0");
            hasher.update(fingerprint.as_bytes());
        }
        SnapshotSource::SuppliedTree(tree) | SnapshotSource::SuppliedTreeWithBlobs { tree, .. } => {
            hasher.update(b"tree\0");
            hasher.update(tree.hash().as_bytes());
        }
    };
    hasher.update(b"\0");
    hasher.update(head.to_text().as_bytes());
    hasher.update(b"\0");
    hasher.update(details.intent.as_deref().unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    if let Some(confidence) = details.confidence {
        hasher.update(&confidence.to_bits().to_le_bytes());
    }
    hasher.update(b"\0");
    hasher.update(details.attribution.principal.name.as_bytes());
    hasher.update(b"\0");
    hasher.update(details.attribution.principal.email.as_bytes());
    if let Some(agent) = &details.attribution.agent {
        hasher.update(b"\0agent\0");
        hasher.update(agent.provider.as_bytes());
        hasher.update(b"\0");
        hasher.update(agent.model.as_bytes());
        hasher.update(b"\0");
        hasher.update(agent.session_id.as_deref().unwrap_or_default().as_bytes());
        hasher.update(b"\0");
        hasher.update(agent.segment_id.as_deref().unwrap_or_default().as_bytes());
        hasher.update(b"\0");
        hasher.update(agent.policy_id.as_deref().unwrap_or_default().as_bytes());
    }
    hasher.update(b"\0lineage\0");
    hasher.update(
        &rmp_serde::to_vec_named(&details.lineage).expect("lineage encoding is infallible"),
    );
    format!("snapshot-{}", hasher.finalize().to_hex())
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotFault {
    AfterStageBeforeAtomicCommit,
    AfterArtifactCommitBeforeOplogView,
    AfterAtomicCommitBeforeRefPublish,
}

#[cfg(test)]
thread_local! {
    static SNAPSHOT_FAULT: std::cell::Cell<Option<SnapshotFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
static SNAPSHOT_PREPARE_PROBE_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static SNAPSHOT_PREPARE_PROBE_ACTIVE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SNAPSHOT_PREPARE_PROBE_MAX: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn with_snapshot_prepare_probe<T>(
    delay: std::time::Duration,
    body: impl FnOnce() -> T,
) -> (T, usize) {
    use std::sync::atomic::Ordering;

    SNAPSHOT_PREPARE_PROBE_ACTIVE.store(0, Ordering::SeqCst);
    SNAPSHOT_PREPARE_PROBE_MAX.store(0, Ordering::SeqCst);
    SNAPSHOT_PREPARE_PROBE_DELAY_MS.store(delay.as_millis() as u64, Ordering::SeqCst);
    let output = body();
    SNAPSHOT_PREPARE_PROBE_DELAY_MS.store(0, Ordering::SeqCst);
    (output, SNAPSHOT_PREPARE_PROBE_MAX.load(Ordering::SeqCst))
}

#[cfg(test)]
fn snapshot_prepare_probe() {
    use std::sync::atomic::Ordering;

    let delay = SNAPSHOT_PREPARE_PROBE_DELAY_MS.load(Ordering::SeqCst);
    if delay == 0 {
        return;
    }
    let active = SNAPSHOT_PREPARE_PROBE_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
    SNAPSHOT_PREPARE_PROBE_MAX.fetch_max(active, Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(delay));
    SNAPSHOT_PREPARE_PROBE_ACTIVE.fetch_sub(1, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn with_snapshot_fault<T>(fault: SnapshotFault, body: impl FnOnce() -> T) -> T {
    SNAPSHOT_FAULT.with(|f| f.set(Some(fault)));
    let out = body();
    SNAPSHOT_FAULT.with(|f| f.set(None));
    out
}

#[cfg(test)]
fn maybe_snapshot_fault(fault: SnapshotFault) {
    SNAPSHOT_FAULT.with(|f| {
        if f.get() == Some(fault) {
            f.set(None);
            panic!("snapshot fault checkpoint");
        }
    });
}

impl Repository {
    fn snapshot_worktree_fingerprint(&self) -> Result<ContentHash> {
        let patterns = self.ignore_patterns()?;
        let nested_exclusions = self.nested_thread_worktree_exclusions(&self.root)?;
        let ignore_matcher = WorktreeIgnoreMatcher::new(&patterns)
            .with_nested_worktree_exclusions(nested_exclusions);
        let mut policy = SnapshotFingerprintPolicy::new(&self.root);
        Ok(
            walk_worktree(self, &self.root, &ignore_matcher, None, &mut policy)?
                .tree
                .hash(),
        )
    }

    /// Create a snapshot of the current worktree.
    #[instrument(skip(self), fields(intent = ?intent))]
    pub fn snapshot(&self, intent: Option<String>, confidence: Option<f32>) -> Result<State> {
        let attribution = self.get_attribution()?;
        self.snapshot_with_attribution(intent, confidence, attribution)
    }

    /// Create a snapshot with explicit attribution.
    #[instrument(skip(self, attribution), fields(intent = ?intent, confidence))]
    pub fn snapshot_with_attribution(
        &self,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
    ) -> Result<State> {
        self.snapshot_with_attribution_profiled(intent, confidence, attribution)
            .map(|execution| execution.state)
    }

    /// Create a snapshot with profiling details for the hot path.
    ///
    /// Snapshot chokepoint (heddle#317, PR #529): EVERY worktree-snapshot
    /// creator — capture, cherry-pick, revert, retro — funnels through here, so
    /// the configured default visibility tier is bound to the freshly created
    /// state at this single site. The binding is folded into the snapshot's own
    /// oplog batch inside [`SnapshotMutation::apply`] (via
    /// [`stage_default_visibility_binding`](Self::stage_default_visibility_binding)),
    /// so one `heddle undo` reverts the snapshot and its auto-applied default
    /// together — never a separate trailing batch (PR #529 P1). The in-progress
    /// merge branch commits through `commit_snapshot_atomic` and folds the
    /// binding there for the same reason.
    pub fn snapshot_with_attribution_profiled(
        &self,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
    ) -> Result<SnapshotExecution> {
        self.snapshot_with_attribution_profiled_locked(intent, confidence, attribution, Vec::new())
    }

    pub fn snapshot_with_attribution_and_lineage(
        &self,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
        lineage: Vec<ChangeLineage>,
    ) -> Result<State> {
        self.snapshot_with_attribution_profiled_locked(intent, confidence, attribution, lineage)
            .map(|execution| execution.state)
    }

    #[instrument(skip(self, attribution), fields(intent = ?intent, confidence))]
    fn snapshot_with_attribution_profiled_locked(
        &self,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
        lineage: Vec<ChangeLineage>,
    ) -> Result<SnapshotExecution> {
        let _lock = self
            .locker()
            .write()
            .map_err(|e| HeddleError::Io(std::io::Error::other(e.to_string())))?;

        if let Some(merge_state) = self.merge_state_manager().load()? {
            let unresolved: Vec<_> = merge_state
                .conflicts
                .iter()
                .filter(|path| !merge_state.resolved.contains(*path))
                .collect();
            if !unresolved.is_empty() {
                return Err(HeddleError::Conflict(format!(
                    "Unresolved conflicts: {}",
                    unresolved
                        .into_iter()
                        .map(|path| path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            let theirs = merge_state.theirs;
            let base = merge_state.base;
            let intent = intent.or_else(|| Some(format!("Merge {}", theirs.short())));
            let state = self.snapshot_merge_with_attribution_and_lineage(
                &theirs,
                SnapshotDetails {
                    intent,
                    confidence,
                    attribution,
                    lineage,
                },
                base,
                // We hold the snapshot write lock here; fold the default-
                // visibility binding into the merge's batch (heddle#317).
                true,
                None,
            )?;
            self.merge_state_manager().finish()?;
            let tree = self
                .store
                .get_tree(&state.tree)?
                .ok_or_else(|| HeddleError::NotFound("merge snapshot tree missing".to_string()))?;
            return Ok(SnapshotExecution {
                state,
                tree,
                profile: SnapshotProfile::default(),
            });
        }

        let head = self.head_ref()?;
        let prev_head = self.head()?;
        let fingerprint = self.snapshot_worktree_fingerprint()?;
        #[cfg(test)]
        snapshot_prepare_probe();
        let atomic_execute_started = std::time::Instant::now();
        let mut execution = execute(
            self,
            SnapshotMutation::new(
                self,
                SnapshotSource::Worktree { fingerprint },
                SnapshotDetails {
                    intent,
                    confidence,
                    attribution,
                    lineage,
                },
                prev_head,
                head.clone(),
            ),
        )?;
        execution.profile.atomic_execute_ms = atomic_execute_started.elapsed().as_millis();
        let committed_tip = self.oplog().head_id()?;

        objects::fault_inject::maybe_panic_at("snapshot_after_atomic_commit_before_ref_publish");
        #[cfg(test)]
        maybe_snapshot_fault(SnapshotFault::AfterAtomicCommitBeforeRefPublish);

        // Phase 5 is a materialized view, not the commit point: force the
        // success-path ref publish through the same per-read reconciliation that
        // recovers a crash after the atomic oplog append.
        let ref_publish_started = std::time::Instant::now();
        reconcile_snapshot_ref(self, &head, &execution.state, committed_tip)?;
        execution.profile.ref_publish_ms = ref_publish_started.elapsed().as_millis();
        refresh_materialized_thread_manifest(self, &head, &execution.state, &execution.tree);
        Ok(execution)
    }

    /// Create a snapshot from a caller-supplied tree instead of walking
    /// the worktree. Used by Git-overlay staged-index commits, where
    /// the desired snapshot is the Git index boundary and the worktree
    /// may intentionally still contain unstaged files.
    ///
    /// Routes through the same default-visibility chokepoint as
    /// [`snapshot_with_attribution_profiled`](Self::snapshot_with_attribution_profiled):
    /// a Git-overlay capture must inherit the configured default tier too. The
    /// binding is folded into the snapshot's batch inside
    /// [`SnapshotMutation::apply`] (PR #529 P1).
    pub fn snapshot_tree_with_attribution_profiled(
        &self,
        tree: Tree,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
    ) -> Result<SnapshotExecution> {
        self.snapshot_tree_source_with_attribution_profiled_locked(
            SnapshotSource::SuppliedTree(tree),
            intent,
            confidence,
            attribution,
        )
    }

    /// Create a caller-supplied tree and its newly-authored blobs in one durable
    /// snapshot batch. This is the structured-authoring counterpart to a
    /// worktree snapshot: blob files, trees, and state become durable before
    /// the oplog commit, without one directory fsync per blob.
    pub fn snapshot_tree_with_blobs_with_attribution_profiled(
        &self,
        tree: Tree,
        blobs: Vec<Blob>,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
    ) -> Result<SnapshotExecution> {
        self.snapshot_tree_source_with_attribution_profiled_locked(
            SnapshotSource::SuppliedTreeWithBlobs { tree, blobs },
            intent,
            confidence,
            attribution,
        )
    }

    #[instrument(skip(self, source, attribution), fields(intent = ?intent, confidence))]
    fn snapshot_tree_source_with_attribution_profiled_locked(
        &self,
        source: SnapshotSource,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
    ) -> Result<SnapshotExecution> {
        let _lock = self
            .locker()
            .write()
            .map_err(|e| HeddleError::Io(std::io::Error::other(e.to_string())))?;

        if let Some(merge_state) = self.merge_state_manager().load()? {
            let unresolved: Vec<_> = merge_state
                .conflicts
                .iter()
                .filter(|path| !merge_state.resolved.contains(*path))
                .collect();
            if !unresolved.is_empty() {
                return Err(HeddleError::Conflict(format!(
                    "Unresolved conflicts: {}",
                    unresolved
                        .into_iter()
                        .map(|path| path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }

        let authoritative_artifact =
            matches!(&source, SnapshotSource::SuppliedTreeWithBlobs { .. });
        let head = self.head_ref()?;
        let prev_head = self.head()?;
        let atomic_execute_started = std::time::Instant::now();
        let mutation = SnapshotMutation::new(
            self,
            source,
            SnapshotDetails {
                intent,
                confidence,
                attribution,
                lineage: Vec::new(),
            },
            prev_head,
            head.clone(),
        );
        let (mut execution, committed_tip) = if authoritative_artifact {
            let committed =
                execute_reconstructible(self, mutation, |mutation, base_head_id, records| {
                    mutation.install_prepared_artifact(base_head_id, records)
                })?;
            let committed_tip = committed.committed_tip;
            let mut output = committed.output;
            if let Some((descriptor, artifact_write_ms)) = committed.artifact {
                output.profile.blob_write_ms = artifact_write_ms;
                debug!(
                    pack = %descriptor.pack_name,
                    path = %descriptor.pack_path.display(),
                    objects = descriptor.object_ids.len(),
                    state = %descriptor.artifact.state,
                    "structured snapshot committed through authoritative pack artifact"
                );
            }
            (output, committed_tip)
        } else {
            let output = execute(self, mutation)?;
            let committed_tip = self.oplog().head_id()?;
            (output, committed_tip)
        };
        execution.profile.atomic_execute_ms = atomic_execute_started.elapsed().as_millis();

        objects::fault_inject::maybe_panic_at("snapshot_after_atomic_commit_before_ref_publish");
        #[cfg(test)]
        maybe_snapshot_fault(SnapshotFault::AfterAtomicCommitBeforeRefPublish);

        let ref_publish_started = std::time::Instant::now();
        reconcile_snapshot_ref(self, &head, &execution.state, committed_tip)?;
        execution.profile.ref_publish_ms = ref_publish_started.elapsed().as_millis();
        Ok(execution)
    }

    /// Create a merge state with two parents.
    ///
    /// `fold_default_visibility` binds the configured capture-time default
    /// visibility tier to the new merge state and folds the resulting
    /// `OpRecord::StateVisibilitySet` into the merge's own commit batch (so one
    /// `heddle undo` reverts both — heddle#317 / PR #529 P1). It is `true` ONLY
    /// for the in-progress-merge capture branch of
    /// [`snapshot_with_attribution_profiled`](Self::snapshot_with_attribution_profiled),
    /// which calls this while already holding the snapshot write lock — so the
    /// sidecar write runs lock-held. The direct merge callers (the `merge` verb,
    /// thread refresh, signing tests) pass `false`, preserving their prior
    /// no-auto-binding behavior.
    pub fn snapshot_merge_with_attribution(
        &self,
        merge_parent: &StateId,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
        merge_base: Option<StateId>,
        fold_default_visibility: bool,
    ) -> Result<State> {
        self.snapshot_merge_with_attribution_transaction(
            merge_parent,
            intent,
            confidence,
            attribution,
            merge_base,
            fold_default_visibility,
            None,
        )
    }

    /// Transaction-bound merge snapshot. When `transaction_id` is present its
    /// commit sentinel is folded into the same record-first batch as the
    /// snapshot ref update, allowing recovery to identify the exact integration
    /// batch even if the caller crashes before persisting the resulting state.
    #[allow(clippy::too_many_arguments)]
    pub fn snapshot_merge_with_attribution_transaction(
        &self,
        merge_parent: &StateId,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
        merge_base: Option<StateId>,
        fold_default_visibility: bool,
        transaction_id: Option<&str>,
    ) -> Result<State> {
        self.snapshot_merge_with_attribution_and_lineage(
            merge_parent,
            SnapshotDetails {
                intent,
                confidence,
                attribution,
                lineage: Vec::new(),
            },
            merge_base,
            fold_default_visibility,
            transaction_id,
        )
    }

    fn snapshot_merge_with_attribution_and_lineage(
        &self,
        merge_parent: &StateId,
        details: SnapshotDetails,
        merge_base: Option<StateId>,
        fold_default_visibility: bool,
        transaction_id: Option<&str>,
    ) -> Result<State> {
        let tree = self.build_tree(&self.root)?;
        let tree_hash = self.store.put_tree(&tree)?;

        let first_parent = self
            .head()?
            .ok_or_else(|| HeddleError::NotFound("No current state".to_string()))?;
        let parents = vec![first_parent, *merge_parent];

        let mut state = State::new_merge(tree_hash, parents, details.attribution);

        if let Some(intent) = details.intent {
            state = state.with_intent(intent);
        }

        if let Some(confidence) = details.confidence {
            state = state.with_confidence(confidence);
        }

        if !details.lineage.is_empty() {
            state = state.with_lineage(details.lineage);
        }

        let ours_state = self
            .store
            .get_state(&first_parent)?
            .ok_or(HeddleError::StateNotFound(first_parent))?;
        let theirs_state = self
            .store
            .get_state(merge_parent)?
            .ok_or(HeddleError::StateNotFound(*merge_parent))?;
        let base_state = match merge_base {
            Some(base_id) => self.store.get_state(&base_id)?,
            None => None,
        };
        if let Some(provenance) = self.build_merge_provenance_root(
            &state,
            &ours_state,
            &theirs_state,
            base_state.as_ref(),
        )? {
            state = state.with_provenance(provenance);
        }

        // Union the parents' context trees so annotations from either side
        // ride forward. Same-id collisions resolve to the newest revision;
        // see `union_parent_contexts` for the merge rules.
        let merged_context = self.union_parent_contexts(&[&ours_state, &theirs_state])?;

        // Persist the immutable merge before its independently-addressed context.
        self.put_authored_state(&state)?;
        if let Some(context) = merged_context {
            self.put_state_attachment(&StateAttachment {
                state_id: state.id(),
                body: StateAttachmentBody::Context(context),
                attribution: state.attribution.clone(),
                created_at: chrono::Utc::now(),
                supersedes: None,
            })?;
        }

        let head = self.head_ref()?;
        let thread = match &head {
            Head::Attached { thread } => Some(thread.clone()),
            Head::Detached { .. } => None,
        };

        // Fold the automatic capture-time default-visibility binding into the
        // merge's own batch (heddle#317 / PR #529 P1) when the in-progress-merge
        // capture branch asked for it, routing through the fold-and-rewind
        // chokepoint so the staged sidecar is rewound if the commit fails
        // (invariant 2). That caller holds the snapshot write lock, so the
        // sidecar write runs lock-held (`lock_held = true`). Direct merge callers
        // (`merge` verb, thread refresh, signing tests) pass
        // `fold_default_visibility = false` and keep their no-auto-binding
        // behavior — a plain record-first commit (heddle#354 r8).
        if fold_default_visibility {
            if transaction_id.is_some() {
                return Err(HeddleError::Config(
                    "transaction-bound merge snapshots cannot fold capture visibility".to_string(),
                ));
            }
            self.commit_snapshot_atomic_with_capture_visibility(
                &state.id(),
                Some(first_parent),
                thread.as_ref(),
                true,
            )?;
        } else {
            let extra = transaction_id
                .map(|transaction_id| OpRecord::TransactionCommit {
                    transaction_id: transaction_id.to_string(),
                    op_count: 1,
                })
                .into_iter()
                .collect();
            self.commit_snapshot_atomic_with_records(
                &state.id(),
                Some(first_parent),
                thread.as_ref(),
                extra,
            )?;
        }

        Ok(state)
    }
}

fn snapshot_profile_from_tree(
    tree_profile: TreeBuildProfile,
    root_tree_write_ms: u128,
    state_ref_oplog_ms: u128,
) -> SnapshotProfile {
    SnapshotProfile {
        tree_walk_ms: tree_profile.tree_walk_ms,
        blob_prep_ms: tree_profile.blob_prep_ms,
        blob_write_ms: tree_profile.blob_write_ms,
        tree_write_ms: tree_profile.tree_write_ms + root_tree_write_ms,
        state_ref_oplog_ms,
        atomic_execute_ms: 0,
        ref_publish_ms: 0,
    }
}

/// Atomically materialize the committed snapshot ref without adding another
/// durability barrier. The oplog is authoritative; the persisted ref watermark
/// deliberately remains at its prior floor so a crash that loses this
/// reconstructible view is recovered by a fresh process.
fn reconcile_snapshot_ref(
    repo: &Repository,
    head: &Head,
    state: &State,
    committed_tip: u64,
) -> Result<()> {
    match head {
        Head::Attached { thread } => {
            repo.refs.materialize_snapshot_thread_after_commit(
                thread,
                state.id(),
                committed_tip,
            )?;
        }
        Head::Detached { .. } => repo
            .refs
            .materialize_snapshot_head_after_commit(state.id(), committed_tip)?,
    }
    Ok(())
}

fn refresh_materialized_thread_manifest(
    repo: &Repository,
    head: &Head,
    state: &State,
    tree: &Tree,
) {
    let Head::Attached { thread } = head else {
        return;
    };
    let Ok(Some(original)) = crate::thread_manifest::read_manifest(repo.heddle_dir(), thread)
    else {
        return;
    };
    let self_root_canonical =
        super::repository_thread_materialize::canonical_worktree_path(&repo.root);
    if original.worktree_path != self_root_canonical {
        return;
    }

    let mut refreshed =
        crate::thread_manifest::ThreadManifest::new(state.id(), state.tree, original.worktree_path);
    if let Err(err) = super::repository_thread_materialize::populate_manifest_from_tree(
        repo,
        tree,
        &repo.root,
        "",
        &mut refreshed.files,
    ) {
        tracing::warn!(
            error = %err,
            thread = %thread,
            "manifest refresh post-capture failed; next capture will rebuild"
        );
    } else if let Err(err) =
        crate::thread_manifest::write_manifest(repo.heddle_dir(), thread, &refreshed)
    {
        tracing::warn!(
            error = %err,
            thread = %thread,
            "manifest write post-capture failed; next capture will rebuild"
        );
    }
}
