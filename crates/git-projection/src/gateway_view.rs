// SPDX-License-Identifier: Apache-2.0
//! Bounded public projection of admitted native source into an EMPTY Git sink.
//! No retained packs, credentials, refs, or source projection caches are copied.
use crate::{
    GitProjectionError, GitProjectionResult, SyncMapping,
    git_export::{ExportStateOptions, commit_is_byte_faithful, export_state},
};
use crypto::thread_operation::SignedOperation;
use objects::{
    object::{
        AudienceTier, ContentHash, StateId, TreeEntryTarget,
        thread_replication::{
            Admission, GenesisOwner, ThreadGenesis, ThreadOperation, ThreadOperationBody,
        },
        visible,
    },
    store::ObjectStore,
};
use repo::{Repository, thread_replication::ThreadReplica};
use sley::{ObjectId, Repository as GitRepository};
use std::collections::{HashMap, HashSet};

fn refuse(message: &str) -> GitProjectionError {
    GitProjectionError::Git(message.into())
}

/// Limits apply to the complete selected closure, before any objects are written.
#[derive(Clone, Copy)]
pub struct ViewLimits {
    pub states: usize,
    pub entries: usize,
    pub bytes: usize,
    pub blob_bytes: usize,
}
impl Default for ViewLimits {
    fn default() -> Self {
        Self {
            states: 128,
            entries: 10_000,
            bytes: 64 * 1024 * 1024,
            blob_bytes: 16 * 1024 * 1024,
        }
    }
}

fn tree_budget(
    repo: &Repository,
    hash: ContentHash,
    depth: usize,
    remaining: &mut ViewLimits,
    preserve_git_identity: bool,
) -> GitProjectionResult<()> {
    if depth > 64 {
        return Err(refuse("tree depth limit"));
    }
    let tree = repo
        .store()
        .get_tree(&hash)?
        .ok_or_else(|| refuse("missing tree"))?;
    for entry in tree.entries() {
        remaining.entries = remaining
            .entries
            .checked_sub(1)
            .ok_or_else(|| refuse("entry limit"))?;
        match entry.target() {
            TreeEntryTarget::Tree { hash } => {
                tree_budget(repo, *hash, depth + 1, remaining, preserve_git_identity)?
            }
            TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => {
                // Match the exporter's redaction gate, never read redacted bytes.
                let len = if let Some(stub) = repo
                    .redaction_stub_for_blob(hash)
                    .map_err(|e| refuse(&e.to_string()))?
                {
                    if preserve_git_identity {
                        return Err(refuse("redacted history cannot preserve Git identity"));
                    }
                    stub.len()
                } else {
                    repo.store()
                        .get_blob(hash)?
                        .ok_or_else(|| refuse("missing blob"))?
                        .content()
                        .len()
                };
                if len > remaining.blob_bytes {
                    return Err(refuse("blob limit"));
                }
                remaining.bytes = remaining
                    .bytes
                    .checked_sub(len)
                    .ok_or_else(|| refuse("byte limit"))?;
            }
            // A complete snapshot must not silently drop native child spools.
            TreeEntryTarget::Spoollink { .. } | TreeEntryTarget::Gitlink { .. } => {
                return Err(refuse("nested repository edges unsupported"));
            }
        }
    }
    Ok(())
}

/// One explicitly selected source tip. Git ref names remain the caller's concern.
#[derive(Clone, Copy, Debug)]
pub struct HistoryTip<'a> {
    pub thread: &'a str,
    pub state: StateId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Snapshot,
    NativeHistory,
    GitHistory,
}

type AuthorizedThreads = HashMap<ContentHash, (ThreadReplica, ThreadGenesis)>;
// Candidate-only overlay for precommit checks. Public export always supplies an
// empty overlay and continues to require persisted accepted originals.
type PreparedOriginals = HashMap<(ContentHash, StateId), Vec<SignedOperation>>;

#[derive(Default)]
struct AdmissionBudget {
    work: usize,
    bytes: usize,
}

impl AdmissionBudget {
    fn charge(&mut self, bytes: usize) -> GitProjectionResult<()> {
        self.work += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        if self.work > 4096 || self.bytes > 16 * 1024 * 1024 {
            return Err(refuse("native original limit"));
        }
        Ok(())
    }
}

fn require_local_dependency(
    governing: ContentHash,
    dependency: ContentHash,
    authorized: &AuthorizedThreads,
) -> GitProjectionResult<()> {
    let (root, root_genesis) = authorized
        .get(&governing)
        .ok_or_else(|| refuse("governing Thread not explicitly authorized"))?;
    let (replica, genesis) = authorized
        .get(&dependency)
        .ok_or_else(|| refuse("dependency Thread not explicitly authorized"))?;
    let GenesisOwner::LocalKey(key) = root_genesis.owner else {
        return Err(refuse("cross-Thread account authority unsupported"));
    };
    if genesis.spool != root_genesis.spool
        || genesis.owner != GenesisOwner::LocalKey(key)
        || root.effective_owner().map_err(|e| refuse(&e.to_string()))?
            != GenesisOwner::LocalKey(key)
        || replica
            .effective_owner()
            .map_err(|e| refuse(&e.to_string()))?
            != GenesisOwner::LocalKey(key)
        || !root
            .ownership_claims()
            .map_err(|e| refuse(&e.to_string()))?
            .is_empty()
        || !replica
            .ownership_claims()
            .map_err(|e| refuse(&e.to_string()))?
            .is_empty()
    {
        return Err(refuse("cross-Thread local ownership or Spool differs"));
    }
    Ok(())
}

/// Establish every signed original before reading payload trees or writing the sink.
/// Revisit exact operation edges before deduplication: a valid alternate path must
/// never hide an invalid integration edge. Privacy intersects ALL accepted originals.
#[allow(clippy::too_many_arguments)]
fn validate_native_admission(
    repo: &Repository,
    tip: StateId,
    governing: ContentHash,
    authorized: &AuthorizedThreads,
    state_limit: usize,
    budget: &mut AdmissionBudget,
    prepared: &PreparedOriginals,
    verify: &impl Fn(&ThreadReplica, &ThreadOperation) -> GitProjectionResult<()>,
) -> GitProjectionResult<()> {
    let mut pending = vec![(governing, tip, None::<ContentHash>)];
    let mut seen = HashSet::new();
    let mut states = HashSet::new();
    while let Some((owner, id, exact)) = pending.pop() {
        if pending.len() > 4096 || seen.len() > 4096 {
            return Err(refuse("native dependency limit"));
        }
        let (replica, genesis) = authorized
            .get(&owner)
            .ok_or_else(|| refuse("dependency Thread not explicitly authorized"))?;
        if owner != governing {
            require_local_dependency(governing, owner, authorized)?;
        }
        if let Some(exact) = exact {
            let (signed, status) = replica
                .operation(&exact)
                .map_err(|e| refuse(&e.to_string()))?
                .ok_or_else(|| refuse("missing exact admitted source operation"))?;
            budget.charge(signed.canonical.len() + signed.signature.len())?;
            let operation = signed.verify().map_err(|e| refuse(&e.to_string()))?;
            if status != Admission::Accepted
                || operation.thread != owner
                || operation.id().map_err(|e| refuse(&e.to_string()))? != exact
                || operation
                    .source_state()
                    .map_err(|e| refuse(&e.to_string()))?
                    .is_none_or(|state| state.id() != id)
            {
                return Err(refuse("invalid exact admitted source operation"));
            }
            verify(replica, &operation)?;
        }
        if !seen.insert((owner, id)) {
            continue;
        }
        states.insert(id);
        if states.len() > state_limit {
            return Err(refuse("state limit"));
        }
        let state = repo
            .store()
            .get_state(&id)?
            .ok_or(GitProjectionError::StateNotFound(id))?;
        let mut originals = replica
            .accepted_source_originals_for_revisions(&[id])
            .map_err(|e| refuse(&e.to_string()))?;
        if let Some(candidates) = prepared.get(&(owner, id)) {
            for signed in candidates {
                // An exact replay is one eventual indexed original, not two.
                // Every distinct existing original remains in the intersection.
                if !originals
                    .iter()
                    .any(|(_, existing)| existing.canonical == signed.canonical)
                {
                    originals.push((id, signed.clone()));
                }
            }
        }
        if originals.is_empty() {
            if genesis.base != id {
                return Err(refuse("no admitted native original"));
            }
            if let Some(parent) = genesis.parent {
                require_local_dependency(governing, owner, authorized)?;
                require_local_dependency(governing, parent, authorized)?;
                pending.push((parent, id, None));
                continue;
            }
            objects::object::thread_replication::initial_base::initial_base_state(
                genesis,
                &state.encode_current_msgpack()?,
            )
            .map_err(|_| refuse("no admitted native original or canonical initialization"))?;
            continue;
        }
        let mut foreign_parents = HashSet::new();
        for (indexed, signed) in originals {
            budget.charge(signed.canonical.len() + signed.signature.len())?;
            let operation = signed.verify().map_err(|e| refuse(&e.to_string()))?;
            let original = operation
                .source_state()
                .map_err(|e| refuse(&e.to_string()))?
                .ok_or_else(|| refuse("missing signed state"))?;
            if indexed != id
                || operation.thread != owner
                || original.id() != id
                // git_lossy is deliberately outside StateId. Compare the entire
                // signed State, not just the hashed identity, before fidelity use.
                || original.encode_current_msgpack()? != state.encode_current_msgpack()?
            {
                return Err(refuse("signed source differs from native state"));
            }
            // Historical admission and signature validity never replace a fresh
            // author/disclosure decision for every retained original.
            verify(replica, &operation)?;
            if owner != governing {
                replica
                    .verify_local_source_owner(&operation)
                    .map_err(|e| refuse(&e.to_string()))?;
            }
            if let Some(integration) = operation
                .local_integration()
                .map_err(|e| refuse(&e.to_string()))?
            {
                require_local_dependency(governing, owner, authorized)?;
                require_local_dependency(governing, integration.source_thread, authorized)?;
                replica
                    .verify_local_source_owner(&operation)
                    .map_err(|e| refuse(&e.to_string()))?;
                if integration.source_thread == owner
                    || integration.spool.to_string() != genesis.spool
                    || !state.parents.contains(&integration.source_revision)
                    || !visible(&integration.result_visibility, &AudienceTier::Public)
                {
                    return Err(refuse("local integration ancestry or visibility invalid"));
                }
                let (source, _) = authorized
                    .get(&integration.source_thread)
                    .ok_or_else(|| refuse("dependency Thread not explicitly authorized"))?;
                let (source_signed, status) = source
                    .operation(&integration.source_operation)
                    .map_err(|e| refuse(&e.to_string()))?
                    .ok_or_else(|| refuse("missing exact admitted source operation"))?;
                budget.charge(source_signed.canonical.len() + source_signed.signature.len())?;
                let source_operation =
                    source_signed.verify().map_err(|e| refuse(&e.to_string()))?;
                if status != Admission::Accepted {
                    return Err(refuse("source operation not admitted"));
                }
                integration
                    .validate_source(&source_operation)
                    .map_err(|e| refuse(&e.to_string()))?;
                foreign_parents.insert(integration.source_revision);
                pending.push((
                    integration.source_thread,
                    integration.source_revision,
                    Some(integration.source_operation),
                ));
            } else if owner != governing
                && !matches!(operation.body, ThreadOperationBody::Capture(_))
            {
                return Err(refuse("cross-Thread hosted source unsupported"));
            }
            let result = operation
                .source_result()
                .map_err(|e| refuse(&e.to_string()))?
                .ok_or_else(|| refuse("not a source operation"))?;
            let tier = result
                .visibility
                .as_ref()
                .and_then(|privacy| privacy.state.as_ref())
                .cloned()
                .unwrap_or_else(|| repo.resolve_capture_default_visibility());
            if !visible(&tier, &AudienceTier::Public) {
                return Err(refuse("original captured visibility withholds state"));
            }
            if let Some(privacy) = &result.visibility
                && let Some(entries) = privacy
                    .entry_sidecar(&state)
                    .map_err(|e| refuse(&e.to_string()))?
                && entries
                    .entries
                    .iter()
                    .any(|entry| !visible(&entry.tier, &AudienceTier::Public))
            {
                return Err(refuse(
                    "original captured entry visibility withholds content",
                ));
            }
        }
        for parent in &state.parents {
            if !foreign_parents.contains(parent) {
                pending.push((owner, *parent, None));
            }
        }
    }
    Ok(())
}

/// Snapshot removes parents and is a derived root commit, not original history.
/// History mode requires EVERY ancestor to be present and publicly visible.
/// Imported/fidelity states are rejected until residual and history policy is implemented.
/// The trusted caller must authorize repository + state + governing Thread.
/// No other Thread is implicitly authorized by this entrypoint.
pub fn export_public_native_view(
    repo: &Repository,
    sink: &GitRepository,
    tip: StateId,
    thread: &str,
    snapshot: bool,
    limits: ViewLimits,
) -> GitProjectionResult<ObjectId> {
    export_public_native_view_with_authorized_threads(
        repo,
        sink,
        tip,
        thread,
        &[thread],
        snapshot,
        limits,
    )
}

/// Export an explicitly authorized local source closure. The caller independently
/// authorizes the repository, selected State, EVERY named Thread, and their current
/// disclosure policies. This static local-fixture grant is not hosted audience or
/// capability verification. A signature or matching owner key is not a reader grant.
///
/// `authorized_threads` is the complete allowlist (including `thread`), bounded to
/// 128 names / 32 KiB. Entries resolve to verified immutable Thread identities before
/// traversal. Only signed fork-base and exact admitted LocalIntegration edges may
/// cross Threads; matching objects, refs, or Spool membership never create an edge.
/// Cross-Thread support is restricted to unchanged, identical local-key ownership.
#[allow(clippy::too_many_arguments)]
pub fn export_public_native_view_with_authorized_threads(
    repo: &Repository,
    sink: &GitRepository,
    tip: StateId,
    thread: &str,
    authorized_threads: &[&str],
    snapshot: bool,
    limits: ViewLimits,
) -> GitProjectionResult<ObjectId> {
    let mapping = export_public_views(
        repo,
        sink,
        &[HistoryTip { thread, state: tip }],
        authorized_threads,
        if snapshot {
            ViewMode::Snapshot
        } else {
            ViewMode::NativeHistory
        },
        limits,
        &|_, _| Ok(()),
    )?;
    mapping
        .get_git(&tip)
        .ok_or_else(|| refuse("tip not projected"))
}

/// Project the complete union of explicit source tips with stable ordered ancestry.
/// Returns the existing bidirectional mapping for ref publication and Git write
/// reconciliation. No refs, notes, source mapping caches, or retained packs are copied.
/// Shared ancestors are projected once; all admission/visibility/budget checks finish
/// before the first sink write. An over-budget or incomplete graph fails, never truncates.
///
/// Every State must have signed native admission. Byte-faithful Git captures may be
/// reconstructed through the existing exporter; arbitrary imports, lossy/residual
/// states, partial visibility, and redacted closures are refused. A write adapter must
/// separately verify its received Git bytes/OID before admitting such a capture.
/// The synthetic Heddle initialization seed is verified but has no Git commit.
///
/// The caller supplies a fresh empty sink and immutable native input, independently
/// authorizes every Thread/current disclosure policy, pins each expected output tip,
/// and atomically publishes only those refs. Across advancing tips, old OIDs remain
/// stable under the same projection policy (including configured hosted URL/footer).
/// Changing that context is a new projection generation, not a fast-forward promise.
pub fn export_public_git_history(
    repo: &Repository,
    sink: &GitRepository,
    tips: &[HistoryTip<'_>],
    authorized_threads: &[&str],
    limits: ViewLimits,
) -> GitProjectionResult<SyncMapping> {
    export_public_views(
        repo,
        sink,
        tips,
        authorized_threads,
        ViewMode::GitHistory,
        limits,
        &|_, _| Ok(()),
    )
}

/// Export under an explicit current-authority verifier for every signature-verified
/// source original, including retained ancestors and exact dependency edges.
/// The caller must hold the corresponding authority/mutation fence through use of
/// the output; this callback is not a persisted grant or revocation cache.
pub fn export_public_git_history_with_authority(
    repo: &Repository,
    sink: &GitRepository,
    tips: &[HistoryTip<'_>],
    authorized_threads: &[&str],
    limits: ViewLimits,
    verify: impl Fn(&ThreadReplica, &ThreadOperation) -> GitProjectionResult<()>,
) -> GitProjectionResult<SyncMapping> {
    export_public_views(
        repo,
        sink,
        tips,
        authorized_threads,
        ViewMode::GitHistory,
        limits,
        &verify,
    )
}

/// Check exactly the eventual history view before durably admitting a local push.
/// Prepared originals are only an inspection overlay: they are signature/owner
/// verified, retain all existing signed restrictions, and consume the same combined
/// originals/content budgets as export. This function neither admits operations nor
/// writes a Git object. The caller must hold native mutation/generation guards until
/// these exact originals are committed through ordinary source admission.
// Retain the narrow local-only entrypoint even when the prepared adapter routes
// its local verifier through the shared authority hook. It stays regression-tested.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn preflight_prepared_git_history(
    repo: &Repository,
    tip: StateId,
    thread: &str,
    originals: &[SignedOperation],
    limits: ViewLimits,
) -> GitProjectionResult<()> {
    preflight_prepared_git_history_with_authority(
        repo,
        tip,
        thread,
        originals,
        limits,
        |replica, operation| {
            replica
                .verify_local_source_owner(operation)
                .map_err(|e| refuse(&e.to_string()))
        },
    )
}

/// Shared inspection for an externally authorized prepared source publisher.
/// `verify` is a trusted CURRENT-authority policy hook over a signature-verified,
/// source original across the complete closure, including retained ancestors.
/// The caller must bind the exact independently chosen actor,
/// publisher and SourceAuthor (including its authority envelope) before preparation,
/// and reject any mismatch here. This hook grants neither persisted admission nor
/// a receiver generation/CAS proof. Those remain the caller's guarded commit work.
/// Public export never invokes this helper or accepts pending originals.
pub(crate) fn preflight_prepared_git_history_with_authority(
    repo: &Repository,
    tip: StateId,
    thread: &str,
    originals: &[SignedOperation],
    limits: ViewLimits,
    verify: impl Fn(&ThreadReplica, &ThreadOperation) -> GitProjectionResult<()>,
) -> GitProjectionResult<()> {
    if originals.is_empty() || originals.len() > 4096 {
        return Err(refuse("native original limit"));
    }
    let replica = repo
        .native_thread(thread)
        .map_err(|e| refuse(&e.to_string()))?;
    // Only deduplicate an exact original within this held-fence inspection. No
    // authority decision survives this invocation or is stored with the plan.
    let checked = std::cell::RefCell::new(HashSet::new());
    let check = |replica: &ThreadReplica, operation: &ThreadOperation| {
        let id = operation.id().map_err(|e| refuse(&e.to_string()))?;
        if !checked.borrow().contains(&id) {
            verify(replica, operation)?;
            checked.borrow_mut().insert(id);
        }
        Ok(())
    };
    let mut prepared = PreparedOriginals::new();
    let mut preparation_budget = AdmissionBudget::default();
    for signed in originals {
        // Bound verification/allocation even for unreachable prepared originals.
        preparation_budget.charge(signed.canonical.len() + signed.signature.len())?;
        let operation = signed.verify().map_err(|e| refuse(&e.to_string()))?;
        if operation.thread != replica.thread_id()
            || !matches!(operation.body, ThreadOperationBody::Capture(_))
        {
            return Err(refuse("prepared history requires same-Thread captures"));
        }
        check(&replica, &operation)?;
        let state = operation
            .source_state()
            .map_err(|e| refuse(&e.to_string()))?
            .ok_or_else(|| refuse("prepared source State missing"))?;
        prepared
            .entry((operation.thread, state.id()))
            .or_default()
            .push(signed.clone());
    }
    let order = preflight_public_views(
        repo,
        &[HistoryTip { thread, state: tip }],
        &[thread],
        ViewMode::GitHistory,
        limits,
        &prepared,
        &check,
    )?;
    let reachable: HashSet<_> = order.into_iter().collect();
    if prepared.keys().any(|(_, state)| !reachable.contains(state)) {
        return Err(refuse("prepared original outside selected history"));
    }
    Ok(())
}

fn export_public_views(
    repo: &Repository,
    sink: &GitRepository,
    tips: &[HistoryTip<'_>],
    authorized_threads: &[&str],
    mode: ViewMode,
    limits: ViewLimits,
    verify: &impl Fn(&ThreadReplica, &ThreadOperation) -> GitProjectionResult<()>,
) -> GitProjectionResult<SyncMapping> {
    let order = preflight_public_views(
        repo,
        tips,
        authorized_threads,
        mode,
        limits,
        &PreparedOriginals::new(),
        verify,
    )?;
    let seed = objects::object::thread_replication::initial_base::synthetic_initial_base()?.id();
    let mut mapping = SyncMapping::new();
    for id in order {
        if (mode == ViewMode::Snapshot && !tips.iter().any(|tip| tip.state == id))
            || (mode == ViewMode::GitHistory && id == seed)
        {
            continue;
        }
        let oid = export_state(
            &mut mapping,
            repo,
            sink,
            &id,
            ExportStateOptions {
                identity: None,
                message_override: None,
                parent_override: if mode == ViewMode::Snapshot {
                    Some(&[])
                } else {
                    None
                },
                audience: &AudienceTier::Public,
            },
        )?
        .ok_or_else(|| refuse("visibility changed during projection"))?;
        mapping.insert_checked(id, oid)?;
    }
    Ok(mapping)
}

fn preflight_public_views(
    repo: &Repository,
    tips: &[HistoryTip<'_>],
    authorized_threads: &[&str],
    mode: ViewMode,
    mut limits: ViewLimits,
    prepared: &PreparedOriginals,
    verify: &impl Fn(&ThreadReplica, &ThreadOperation) -> GitProjectionResult<()>,
) -> GitProjectionResult<Vec<StateId>> {
    if tips.is_empty()
        || tips.len() > 128
        || tips
            .iter()
            .any(|tip| tip.thread.is_empty() || tip.thread.len() > 1024)
        || tips.iter().map(|tip| tip.thread.len()).sum::<usize>() > 32 * 1024
    {
        return Err(refuse("history tip limit"));
    }
    if authorized_threads.is_empty()
        || authorized_threads.len() > 128
        || authorized_threads
            .iter()
            .any(|name| name.is_empty() || name.len() > 1024)
        || authorized_threads
            .iter()
            .map(|name| name.len())
            .sum::<usize>()
            > 32 * 1024
    {
        return Err(refuse("authorized Thread limit"));
    }
    let mut authorized = HashMap::new();
    for name in authorized_threads {
        let replica = repo
            .native_thread(name)
            .map_err(|e| refuse(&e.to_string()))?;
        let genesis = replica
            .signed_genesis()
            .map_err(|e| refuse(&e.to_string()))?
            .verify()
            .map_err(|e| refuse(&e.to_string()))?;
        authorized.insert(replica.thread_id(), (replica, genesis));
    }
    let seed = objects::object::thread_replication::initial_base::synthetic_initial_base()?.id();
    let mut budget = AdmissionBudget::default();
    for tip in tips {
        let governing = repo
            .native_thread(tip.thread)
            .map_err(|e| refuse(&e.to_string()))?
            .thread_id();
        if !authorized.contains_key(&governing) {
            return Err(refuse("governing Thread not explicitly authorized"));
        }
        if mode == ViewMode::GitHistory && tip.state == seed {
            return Err(refuse("synthetic initialization has no Git history"));
        }
        validate_native_admission(
            repo,
            tip.state,
            governing,
            &authorized,
            limits.states,
            &mut budget,
            prepared,
            verify,
        )?;
    }
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    let mut pending: Vec<_> = tips.iter().rev().map(|tip| (tip.state, false)).collect();
    while let Some((id, finish)) = pending.pop() {
        if finish {
            order.push(id);
            continue;
        }
        if !seen.insert(id) {
            continue;
        }
        limits.states = limits
            .states
            .checked_sub(1)
            .ok_or_else(|| refuse("state limit"))?;
        let state = repo
            .store()
            .get_state(&id)?
            .ok_or(GitProjectionError::StateNotFound(id))?;
        if state.raw_message.is_some()
            && (mode != ViewMode::GitHistory || !commit_is_byte_faithful(&state))
        {
            return Err(refuse("imported or lossy history unsupported in this view"));
        }
        let tier = repo
            .effective_visibility_tier(&id)
            .map_err(|e| refuse(&e.to_string()))?;
        if !visible(&tier, &AudienceTier::Public) {
            return Err(refuse("state not publicly visible"));
        }
        let restrictions = repo
            .content_visibility_for_audience(&id, &AudienceTier::Public)
            .map_err(|e| refuse(&e.to_string()))?
            .ok_or_else(|| refuse("unresolved or withheld ancestry"))?;
        if !restrictions.is_empty() {
            return Err(refuse(
                "entry visibility requires a partial view; unsupported",
            ));
        }
        tree_budget(
            repo,
            state.tree,
            0,
            &mut limits,
            mode == ViewMode::GitHistory,
        )?;
        pending.push((id, true));
        for parent in state.parents.iter().rev() {
            pending.push((*parent, false));
        }
    }
    // Establish topological order before writing any unrelated valid tip.
    let mut complete = HashSet::new();
    for id in &order {
        let state = repo
            .store()
            .get_state(id)?
            .ok_or(GitProjectionError::StateNotFound(*id))?;
        if state
            .parents
            .iter()
            .any(|parent| !complete.contains(parent))
        {
            return Err(refuse("incomplete or cyclic native history"));
        }
        complete.insert(*id);
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objects::object::{Attribution, Principal, VisibilityTier};

    fn fixture() -> (tempfile::TempDir, Repository, StateId) {
        let temp = tempfile::tempdir().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("native repo");
        std::fs::write(temp.path().join("public.txt"), b"public\n").expect("file");
        let state = repo
            .snapshot_with_attribution(
                Some("test".into()),
                None,
                Attribution::human(Principal::new("Synthetic", "fixture@example.invalid")),
            )
            .expect("signed capture");
        (temp, repo, state.id())
    }

    #[test]
    fn prepared_authority_hook_cannot_admit_or_export_pending_source() {
        use crypto::Signer;
        use objects::object::{State, thread_replication::AuthoredCapture};
        use std::{cell::Cell, collections::BTreeSet};

        let (_temp, repo, base) = fixture();
        let replica = repo.native_thread("main").expect("main");
        let before = replica.projection().expect("projection").source_heads;
        let parent = repo.store().get_state(&base).expect("state").expect("base");
        let next = State::new_snapshot(parent.tree, vec![base], parent.attribution)
            .with_intent("not yet admitted");
        repo.store()
            .put_state(&next)
            .expect("stage immutable state");
        let signer = repo
            .native_thread_signer(&replica)
            .expect("existing signer");
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: replica
                .source_operation_page(base, None, 128)
                .expect("parent originals")
                .into_iter()
                .collect::<BTreeSet<_>>(),
            publisher: signer.public_key().try_into().expect("publisher"),
            body: ThreadOperationBody::Capture(AuthoredCapture::local(
                replica
                    .prepare_capture(&repo, &next)
                    .expect("prepared capture"),
            )),
        };
        let signed = SignedOperation::sign(&operation, &signer).expect("prepared signature");
        let checks = Cell::new(0);
        preflight_prepared_git_history_with_authority(
            &repo,
            next.id(),
            "main",
            std::slice::from_ref(&signed),
            ViewLimits::default(),
            |candidate, op| {
                checks.set(checks.get() + 1);
                assert_eq!(candidate.thread_id(), operation.thread);
                assert!([base, next.id()].contains(&op.source_state()?.expect("source").id()));
                Ok(())
            },
        )
        .expect("external policy may inspect a signed candidate");
        assert_eq!(
            checks.get(),
            2,
            "new and retained originals require current authority"
        );
        // Original wrapper preserves its explicit existing-local-owner gate.
        preflight_prepared_git_history(
            &repo,
            next.id(),
            "main",
            std::slice::from_ref(&signed),
            ViewLimits::default(),
        )
        .expect("same local policy as before");
        let error = preflight_prepared_git_history_with_authority(
            &repo,
            next.id(),
            "main",
            std::slice::from_ref(&signed),
            ViewLimits::default(),
            |_, _| Err(refuse("current external authority denied")),
        )
        .expect_err("authority denial is binding");
        assert!(
            error
                .to_string()
                .contains("current external authority denied")
        );
        assert_eq!(
            replica.projection().expect("projection").source_heads,
            before
        );
        assert!(
            replica
                .accepted_source_originals_for_revisions(&[next.id()])
                .expect("admission")
                .is_empty()
        );
        let out = tempfile::tempdir().expect("empty sink");
        let sink = GitRepository::init_bare(out.path()).expect("Git");
        let error = export_public_git_history(
            &repo,
            &sink,
            &[HistoryTip {
                thread: "main",
                state: next.id(),
            }],
            &["main"],
            ViewLimits::default(),
        )
        .expect_err("inspection cannot grant export admission");
        assert!(error.to_string().contains("no admitted native original"));

        let mut bad_signature = signed;
        bad_signature.signature[0] ^= 1;
        preflight_prepared_git_history_with_authority(
            &repo,
            next.id(),
            "main",
            &[bad_signature],
            ViewLimits::default(),
            |_, _| panic!("invalid signature must fail before authority callback"),
        )
        .expect_err("invalid signature");
        let (_other_temp, other, other_base) = fixture();
        let foreign = other
            .native_thread("main")
            .expect("foreign Thread")
            .accepted_source_originals_for_revisions(&[other_base])
            .expect("foreign originals")
            .remove(0)
            .1;
        preflight_prepared_git_history_with_authority(
            &repo,
            next.id(),
            "main",
            &[foreign],
            ViewLimits::default(),
            |_, _| panic!("wrong Thread must fail before authority callback"),
        )
        .expect_err("wrong governing Thread");
    }

    #[test]
    fn limits_fail_before_writing_objects() {
        let (_temp, repo, state) = fixture();
        for limits in [
            ViewLimits {
                states: 0,
                ..ViewLimits::default()
            },
            ViewLimits {
                entries: 0,
                ..ViewLimits::default()
            },
            ViewLimits {
                bytes: 0,
                ..ViewLimits::default()
            },
            ViewLimits {
                blob_bytes: 0,
                ..ViewLimits::default()
            },
        ] {
            let temp = tempfile::tempdir().expect("sink temp");
            let sink = GitRepository::init_bare(temp.path().join("view.git")).expect("sink");
            let error = export_public_native_view(&repo, &sink, state, "main", true, limits)
                .expect_err("bounded");
            assert!(error.to_string().contains("limit"), "{error}");
        }
    }

    #[test]
    fn captured_entry_privacy_survives_sidecar_removal() {
        let (temp, repo, _) = fixture();
        std::fs::write(temp.path().join("restricted.txt"), b"restricted\n").expect("file");
        repo.mark_entry_visibility("restricted.txt", VisibilityTier::Internal)
            .expect("mark");
        let state = repo
            .snapshot_with_attribution(
                Some("restricted".into()),
                None,
                Attribution::human(Principal::new("Synthetic", "fixture@example.invalid")),
            )
            .expect("signed capture");
        // Remove only the mutable copy; the real signed original remains authoritative.
        repo.restore_entry_visibility_sidecar(&state.change_id, None)
            .expect("remove sidecar");
        let out = tempfile::tempdir().expect("sink temp");
        let sink = GitRepository::init_bare(out.path().join("view.git")).expect("sink");
        let error = export_public_native_view(
            &repo,
            &sink,
            state.id(),
            "main",
            true,
            ViewLimits::default(),
        )
        .expect_err("original privacy");
        assert!(
            error.to_string().contains("original captured entry"),
            "{error}"
        );
    }

    /// Genuine authored fork captures and an admitted LocalIntegration. No
    /// operation index rows, arbitrary store lookups, or replacement captures
    /// are used as authorization evidence.
    fn integration_fixture() -> (tempfile::TempDir, Repository, StateId) {
        integration_fixture_with_private_floor(false)
    }

    fn integration_fixture_with_private_floor(
        private_floor: bool,
    ) -> (tempfile::TempDir, Repository, StateId) {
        use objects::object::{State, StateVisibility};
        let (temp, repo, base) = fixture();
        let source = repo
            .create_native_thread("source", base, Some("main"), "source")
            .expect("source fork");
        repo.create_native_thread("target", base, Some("main"), "target")
            .expect("target fork");
        let tree = repo
            .store()
            .get_state(&base)
            .expect("state")
            .expect("base")
            .tree;
        let attribution =
            Attribution::human(Principal::new("Synthetic", "fixture@example.invalid"));
        let left = State::new_snapshot(tree, vec![base], attribution.clone()).with_intent("left");
        let right = State::new_snapshot(tree, vec![base], attribution.clone()).with_intent("right");
        repo.put_authored_state(&left).expect("left");
        repo.put_authored_state(&right).expect("right");
        let source_operation = repo
            .record_native_capture("source", left.id())
            .expect("source capture");
        repo.record_native_capture("target", right.id())
            .expect("target capture");
        let merged = State::new_merge(tree, vec![right.id(), left.id()], attribution)
            .with_intent("integrated");
        repo.put_authored_state(&merged).expect("merge State");
        if private_floor {
            repo.put_state_visibility_if_absent(StateVisibility {
                state: left.id(),
                tier: VisibilityTier::Internal,
                embargo_until: None,
                declarer: left.attribution.principal.clone(),
                declared_at: left.created_at,
                signature: None,
                supersedes: None,
            })
            .expect("source current privacy");
        }
        repo.record_native_local_integration(
            "target",
            merged.id(),
            source.thread_id(),
            source_operation,
            left.id(),
        )
        .expect("admitted local integration");
        if private_floor {
            repo.restore_state_visibility_sidecar(&left.id(), None)
                .expect("remove source mutable floor");
            repo.restore_state_visibility_sidecar(&merged.id(), None)
                .expect("remove result mutable floor");
        }
        (temp, repo, merged.id())
    }

    #[test]
    fn admitted_local_integration_and_fork_base_require_explicit_threads() {
        let (_temp, repo, tip) = integration_fixture();
        for snapshot in [true, false] {
            let out = tempfile::tempdir().expect("sink temp");
            let sink = GitRepository::init_bare(out.path().join("view.git")).expect("sink");
            let oid = export_public_native_view_with_authorized_threads(
                &repo,
                &sink,
                tip,
                "target",
                &["target", "source", "main"],
                snapshot,
                ViewLimits::default(),
            )
            .expect("explicitly authorized signed closure");
            assert!(!oid.to_string().is_empty());
        }
        // The original entrypoint cannot silently expand its caller's grant.
        let out = tempfile::tempdir().expect("sink temp");
        let sink = GitRepository::init_bare(out.path().join("view.git")).expect("sink");
        let error =
            export_public_native_view(&repo, &sink, tip, "target", true, ViewLimits::default())
                .expect_err("no implicit dependency grant");
        assert!(
            error.to_string().contains("not explicitly authorized"),
            "{error}"
        );
    }

    #[test]
    fn signed_integration_visibility_survives_mutable_sidecar_removal() {
        let (_temp, repo, tip) = integration_fixture_with_private_floor(true);
        assert_eq!(
            repo.effective_visibility_tier(&tip).expect("local tier"),
            VisibilityTier::Public
        );
        let replica = repo.native_thread("target").expect("target");
        let originals = replica
            .accepted_source_originals_for_revisions(&[tip])
            .expect("originals");
        let operation = originals[0].1.verify().expect("signed operation");
        let receipt = operation
            .local_integration()
            .expect("receipt")
            .expect("local integration");
        assert!(
            receipt.result.visibility.is_none(),
            "floor is in the integration receipt"
        );
        assert_eq!(receipt.result_visibility, VisibilityTier::Internal);
        let out = tempfile::tempdir().expect("sink temp");
        let sink = GitRepository::init_bare(out.path().join("view.git")).expect("sink");
        let error = export_public_native_view_with_authorized_threads(
            &repo,
            &sink,
            tip,
            "target",
            &["target", "source", "main"],
            true,
            ViewLimits::default(),
        )
        .expect_err("signed receipt floor is permanent");
        assert!(
            error
                .to_string()
                .contains("integration ancestry or visibility"),
            "{error}"
        );
    }

    #[test]
    fn missing_integration_or_fork_dependency_is_denied() {
        let (_temp, repo, tip) = integration_fixture();
        for allowed in [&["target", "main"][..], &["target", "source"][..]] {
            let out = tempfile::tempdir().expect("sink temp");
            let sink = GitRepository::init_bare(out.path().join("view.git")).expect("sink");
            let error = export_public_native_view_with_authorized_threads(
                &repo,
                &sink,
                tip,
                "target",
                allowed,
                true,
                ViewLimits::default(),
            )
            .expect_err("every dependency explicitly authorized");
            assert!(
                error.to_string().contains("not explicitly authorized"),
                "{error}"
            );
        }
    }

    #[test]
    fn wrong_governing_thread_and_unadmitted_store_state_stay_denied() {
        let (_temp, repo, tip) = integration_fixture();
        let stored = repo.store().get_state(&tip).expect("state").expect("tip");
        let unadmitted = objects::object::State::new_snapshot(
            stored.tree,
            vec![tip],
            stored.attribution.clone(),
        );
        repo.store()
            .put_state(&unadmitted)
            .expect("store-only fixture");
        for (state, governing) in [(tip, "main"), (unadmitted.id(), "target")] {
            let out = tempfile::tempdir().expect("sink temp");
            let sink = GitRepository::init_bare(out.path().join("view.git")).expect("sink");
            let error = export_public_native_view_with_authorized_threads(
                &repo,
                &sink,
                state,
                governing,
                &["target", "source", "main"],
                true,
                ViewLimits::default(),
            )
            .expect_err("explicit list does not invent source membership");
            assert!(
                error.to_string().contains("no admitted native original"),
                "{error}"
            );
        }
    }

    #[test]
    fn deterministic_native_snapshot() {
        let (_temp, repo, state) = fixture();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let out = tempfile::tempdir().expect("sink temp");
            let sink = GitRepository::init_bare(out.path().join("view.git")).expect("sink");
            ids.push(
                export_public_native_view(&repo, &sink, state, "main", true, ViewLimits::default())
                    .expect("projection"),
            );
        }
        assert_eq!(ids[0], ids[1]);
    }
}
