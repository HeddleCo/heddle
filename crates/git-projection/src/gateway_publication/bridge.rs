// SPDX-License-Identifier: Apache-2.0
//! Bind the immutable Git preparation to native source publication inputs.
use super::*;
use crate::gateway_write::{PushScope, SignedGitPush};

impl PreparedHistory {
    /// Prepare exact ordinary history from the externally signed Git command.
    /// The caller supplies the unchanged original creator wrapper and current
    /// disclosure verifier; an account envelope is never invented here.
    /// Receiver head CAS and hosted policy authority remain unavailable.
    #[allow(clippy::too_many_arguments)]
    pub fn for_git_push<T: RpcTransport<Error = transport::Error>>(
        remote: &Remote<T>,
        native: &repo::Repository,
        push: &SignedGitPush,
        genesis: ThreadGenesisRecord,
        scope: PublicationScope,
        scratch: &Path,
        budget: HistoryBudget,
        check_disclosure: impl Fn(&PushScope, &[StateId]) -> GitProjectionResult<()>,
    ) -> GitProjectionResult<Self> {
        use objects::lock::RepositoryLockExt;
        let _lock = native.locker().write().map_err(failure)?;
        let selected = push.scope();
        let replica = native.native_thread(&selected.thread).map_err(failure)?;
        let current = replica.projection().map_err(failure)?;
        let heads = if current.source_heads.is_empty() {
            vec![current.genesis.base]
        } else {
            current.source_heads
        };
        if replica.thread_id() != selected.thread_id
            || current.genesis.spool != selected.spool
            || current.generation != selected.expected_generation
            || heads != [selected.expected_native]
            || scope.command != push.receipt().command_id
            || scope
                .thread
                .spool
                .as_ref()
                .is_none_or(|s| s.id != selected.spool)
            || scope
                .thread
                .id
                .as_ref()
                .is_none_or(|id| id.value != selected.thread_id.as_bytes())
        {
            return Err(failure(
                "Git publication differs from exact prepared command or current sender fence",
            ));
        }
        let record = genesis
            .genesis
            .as_ref()
            .ok_or_else(|| failure("signed genesis absent"))?;
        let signed = push.signed_genesis();
        if record.canonical_record != signed.canonical
            || record.signatures.len() != 1
            || record.signatures[0].signature != signed.signature
            || replica.signed_genesis().map_err(failure)? != *signed
        {
            return Err(failure(
                "publication changed the original Git preparation genesis",
            ));
        }
        Self::prepare(
            remote,
            native.store(),
            HistorySelection {
                tip: push.receipt().native_state,
                genesis,
                originals: push.source_originals(),
            },
            scope,
            scratch,
            budget,
            |states| check_disclosure(selected, states),
        )
    }
}
