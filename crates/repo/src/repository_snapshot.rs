// SPDX-License-Identifier: Apache-2.0
//! Snapshot operations for Repository.

use std::collections::BTreeSet;

use objects::{
    lock::RepositoryLockExt,
    object::{
        Attribution, Blob, ChangeLineage, ContentHash, State, StateAttachment, StateAttachmentBody,
        StateId, Tree, TreeEntry,
    },
    store::ObjectStore,
};
use oplog::{IsolationKey, OpRecord};
use refs::Head;
use tracing::{debug, instrument};

use super::{HeddleError, Repository, Result, repository_tree::TreeBuildProfile};
use crate::{
    atomic::{AtomicMutation, RewindLedger, StagedCommit, Tx, execute},
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
}

struct SnapshotMutation<'a> {
    repo: &'a Repository,
    source: SnapshotSource,
    intent: Option<String>,
    confidence: Option<f32>,
    attribution: Attribution,
    lineage: Vec<ChangeLineage>,
    prev_head: Option<StateId>,
    head: Head,
    transaction_id: String,
    /// Set by `apply` when the automatic capture-time default-visibility binding
    /// folds a `StateVisibilitySet` into this snapshot's batch (heddle#317 / PR
    /// #529 P1): `(state, sidecar-before-the-binding)`. `rewind` restores the
    /// sidecar to that before-image if the batch fails to commit, so a rewound
    /// snapshot never leaves its auto-applied tier behind.
    staged_visibility_rewind: Option<(StateId, Option<Vec<u8>>)>,
}

impl<'a> SnapshotMutation<'a> {
    fn new(
        repo: &'a Repository,
        source: SnapshotSource,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
        lineage: Vec<ChangeLineage>,
        prev_head: Option<StateId>,
        head: Head,
    ) -> Self {
        let transaction_id = snapshot_transaction_id(
            repo,
            &source,
            intent.as_deref(),
            confidence,
            &attribution,
            &lineage,
            prev_head,
            &head,
        );
        Self {
            repo,
            source,
            intent,
            confidence,
            attribution,
            lineage,
            prev_head,
            head,
            transaction_id,
            staged_visibility_rewind: None,
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
    fn stage_snapshot_objects(&self) -> Result<SnapshotExecution> {
        debug!("Building tree from worktree");
        let (tree, tree_profile) = match &self.source {
            SnapshotSource::Worktree { .. } => self.build_worktree_tree()?,
            SnapshotSource::SuppliedTree(tree) => (tree.clone(), TreeBuildProfile::default()),
        };
        debug!(duration_ms = tree_profile.tree_walk_ms, "Tree built");

        debug!("Storing tree");
        let root_tree_write_start = std::time::Instant::now();
        let tree_hash = self.repo.store.put_tree(&tree)?;
        let root_tree_write_ms = root_tree_write_start.elapsed().as_millis();

        let parents = match self.prev_head {
            Some(id) => vec![id],
            None => vec![],
        };

        let mut state = State::new_snapshot(tree_hash, parents, self.attribution.clone());

        if let Some(intent) = self.intent.clone() {
            state = state.with_intent(intent);
        }

        if let Some(confidence) = self.confidence {
            state = state.with_confidence(confidence);
        }

        if !self.lineage.is_empty() {
            state = state.with_lineage(self.lineage.clone());
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
        {
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

            if let Some(parent_state) = prior_state.as_ref() {
                match self
                    .repo
                    .compute_and_persist_discussion_anchor_travel(parent_state, &tree)
                {
                    Ok(Some(hash)) => discussions = Some(hash),
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(error = %err, "discussion anchor travel failed; continuing without discussions");
                    }
                }
            }
        }

        // Persist the immutable state before its independently-addressed metadata.
        let state_ref_oplog_start = std::time::Instant::now();
        self.repo.put_authored_state(&state)?;
        if let Some(context) = inherited_context {
            self.repo.put_state_attachment(&StateAttachment {
                state_id: state.id(),
                body: StateAttachmentBody::Context(context),
                attribution: state.attribution.clone(),
                created_at: chrono::Utc::now(),
                supersedes: None,
            })?;
        }
        #[cfg(feature = "tree-sitter-symbols")]
        for body in [
            risk_signals.map(StateAttachmentBody::RiskSignals),
            discussions.map(StateAttachmentBody::Discussions),
        ]
        .into_iter()
        .flatten()
        {
            self.repo.put_state_attachment(&StateAttachment {
                state_id: state.id(),
                body,
                attribution: state.attribution.clone(),
                created_at: chrono::Utc::now(),
                supersedes: None,
            })?;
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
    intent: Option<&str>,
    confidence: Option<f32>,
    attribution: &Attribution,
    lineage: &[ChangeLineage],
    _prev_head: Option<StateId>,
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
        SnapshotSource::SuppliedTree(tree) => {
            hasher.update(b"tree\0");
            hasher.update(tree.hash().as_bytes());
        }
    };
    hasher.update(b"\0");
    hasher.update(head.to_text().as_bytes());
    hasher.update(b"\0");
    hasher.update(intent.unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    if let Some(confidence) = confidence {
        hasher.update(&confidence.to_bits().to_le_bytes());
    }
    hasher.update(b"\0");
    hasher.update(attribution.principal.name.as_bytes());
    hasher.update(b"\0");
    hasher.update(attribution.principal.email.as_bytes());
    if let Some(agent) = &attribution.agent {
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
    hasher.update(&rmp_serde::to_vec_named(lineage).expect("lineage encoding is infallible"));
    format!("snapshot-{}", hasher.finalize().to_hex())
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotFault {
    AfterStageBeforeAtomicCommit,
    AfterAtomicCommitBeforeRefPublish,
}

#[cfg(test)]
thread_local! {
    static SNAPSHOT_FAULT: std::cell::Cell<Option<SnapshotFault>> = const { std::cell::Cell::new(None) };
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
                intent,
                confidence,
                attribution,
                base,
                // We hold the snapshot write lock here; fold the default-
                // visibility binding into the merge's batch (heddle#317).
                true,
                lineage,
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
        let execution = execute(
            self,
            SnapshotMutation::new(
                self,
                SnapshotSource::Worktree { fingerprint },
                intent,
                confidence,
                attribution,
                lineage,
                prev_head,
                head.clone(),
            ),
        )?;

        objects::fault_inject::maybe_panic_at("snapshot_after_atomic_commit_before_ref_publish");
        #[cfg(test)]
        maybe_snapshot_fault(SnapshotFault::AfterAtomicCommitBeforeRefPublish);

        // Phase 5 is a materialized view, not the commit point: force the
        // success-path ref publish through the same per-read reconciliation that
        // recovers a crash after the atomic oplog append.
        let _ = self.head()?;
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
        self.snapshot_tree_with_attribution_profiled_locked(tree, intent, confidence, attribution)
    }

    #[instrument(skip(self, tree, attribution), fields(intent = ?intent, confidence))]
    fn snapshot_tree_with_attribution_profiled_locked(
        &self,
        tree: Tree,
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

        let head = self.head_ref()?;
        let prev_head = self.head()?;
        let execution = execute(
            self,
            SnapshotMutation::new(
                self,
                SnapshotSource::SuppliedTree(tree),
                intent,
                confidence,
                attribution,
                Vec::new(),
                prev_head,
                head,
            ),
        )?;

        objects::fault_inject::maybe_panic_at("snapshot_after_atomic_commit_before_ref_publish");
        #[cfg(test)]
        maybe_snapshot_fault(SnapshotFault::AfterAtomicCommitBeforeRefPublish);

        let _ = self.head()?;
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
        self.snapshot_merge_with_attribution_and_lineage(
            merge_parent,
            intent,
            confidence,
            attribution,
            merge_base,
            fold_default_visibility,
            Vec::new(),
        )
    }

    fn snapshot_merge_with_attribution_and_lineage(
        &self,
        merge_parent: &StateId,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
        merge_base: Option<StateId>,
        fold_default_visibility: bool,
        lineage: Vec<ChangeLineage>,
    ) -> Result<State> {
        let tree = self.build_tree(&self.root)?;
        let tree_hash = self.store.put_tree(&tree)?;

        let first_parent = self
            .head()?
            .ok_or_else(|| HeddleError::NotFound("No current state".to_string()))?;
        let parents = vec![first_parent, *merge_parent];

        let mut state = State::new_merge(tree_hash, parents, attribution);

        if let Some(intent) = intent {
            state = state.with_intent(intent);
        }

        if let Some(confidence) = confidence {
            state = state.with_confidence(confidence);
        }

        if !lineage.is_empty() {
            state = state.with_lineage(lineage);
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
            self.commit_snapshot_atomic_with_capture_visibility(
                &state.id(),
                Some(first_parent),
                thread.as_ref(),
                true,
            )?;
        } else {
            self.commit_snapshot_atomic_with_records(
                &state.id(),
                Some(first_parent),
                thread.as_ref(),
                Vec::new(),
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
    }
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
