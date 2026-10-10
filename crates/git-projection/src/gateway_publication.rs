// SPDX-License-Identifier: Apache-2.0
//! Opt-in, transport-neutral native byte publication for a bounded Git window.
//!
//! A selected SourcePack excludes historical trees/blobs. This adapter prepares
//! one exact source closure per reachable revision, with its original causal
//! proofs. It never obtains credentials, signs originals, admits operations,
//! changes a head, or produces Git acceptance. A native upload receipt cannot
//! replace the receiver's atomic expected-old fence or current disclosure gate.
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use api::v2::client::RpcTransport;
use crypto::{
    Signer, original_boundary_acceptance::SignedBoundaryAcceptance,
    thread_operation::SignedOperation,
};
use objects::object::{ContentHash, ObjectSource, OperationId, StateId};
use thread_api::{
    Remote,
    contract::*,
    publication::{PreparedPublication, PublicationOptions, SourceBudget, SourcePack},
    transport,
};

use crate::{GitProjectionError, GitProjectionResult};
mod bridge;
mod history;
mod revision;
pub use revision::GitAcceptanceActor;

fn failure(error: impl std::fmt::Display) -> GitProjectionError {
    GitProjectionError::Git(error.to_string())
}

/// Explicit caller-selected scope; none of these fields grants authority.
pub struct PublicationScope {
    pub thread: ThreadRef,
    pub source: EndpointRef,
    pub spool_genesis: ContentHash,
    /// Required exact observed sharing-policy frontier, never an empty CAS.
    pub sharing_policy: ContentHash,
    /// Retain across retry; each revision gets a deterministic child identity.
    pub command: OperationId,
}

/// Full selected same-Thread capture ancestry. Originals are never reauthored.
pub struct HistorySelection<'a> {
    pub tip: StateId,
    pub genesis: ThreadGenesisRecord,
    pub originals: &'a [SignedOperation],
}

/// Bounds apply to the complete history, including repeated historical content.
#[derive(Clone, Copy)]
pub struct HistoryBudget {
    pub states: usize,
    /// Conservative cumulative decoded object reads, across every source pack.
    pub decoded_bytes: u64,
    pub artifact_bytes: u64,
    /// Complete retained proposals and later acceptance signatures combined.
    pub metadata_bytes: usize,
}
impl Default for HistoryBudget {
    fn default() -> Self {
        Self {
            states: 128,
            decoded_bytes: 64 * 1024 * 1024,
            artifact_bytes: 64 * 1024 * 1024,
            metadata_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Exact source artifacts and publication proposal for one historical revision.
/// Structural validation and a successful upload do not admit a Git update.
pub struct PreparedRevision {
    source: SourcePack,
    publication: PreparedPublication,
    metadata_remaining: Arc<AtomicUsize>,
}

/// All historical packs prepared locally, with exact revision-scoped originals.
/// Dropping this value removes every temporary artifact, including on failure.
pub struct PreparedHistory {
    tip: StateId,
    revisions: Vec<PreparedRevision>,
}
impl PreparedHistory {
    pub fn tip(&self) -> StateId {
        self.tip
    }
    pub fn revisions(&self) -> &[PreparedRevision] {
        &self.revisions
    }
    pub fn revisions_mut(&mut self) -> &mut [PreparedRevision] {
        &mut self.revisions
    }
    /// Pure local preparation: the Remote supplies a pinned endpoint description
    /// but no RPC is made. `check_disclosure` must check all original and current
    /// restrictions for these exact States under a caller-held fence; it runs
    /// before hydration and again afterward. Its success is never persisted as
    /// future permission. Recheck before serving history or acknowledging Git.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare<T: RpcTransport<Error = transport::Error>>(
        remote: &Remote<T>,
        source: &impl ObjectSource,
        selection: HistorySelection<'_>,
        scope: PublicationScope,
        scratch: &Path,
        budget: HistoryBudget,
        check_disclosure: impl Fn(&[StateId]) -> GitProjectionResult<()>,
    ) -> GitProjectionResult<Self> {
        if budget.states == 0
            || budget.states > 128
            || budget.decoded_bytes == 0
            || budget.decoded_bytes > 64 * 1024 * 1024
            || budget.artifact_bytes == 0
            || budget.artifact_bytes > 64 * 1024 * 1024
            || scope.command.as_uuid().is_nil()
            || budget.metadata_bytes == 0
            || budget.metadata_bytes > 16 * 1024 * 1024
        {
            return Err(failure("bounded history publication required"));
        }
        let history = history::Selection::new(
            &selection,
            &scope.thread,
            budget.states,
            budget.metadata_bytes,
        )?;
        let states = history.states()?;
        check_disclosure(&states)?;
        let counted = history::CountedSource::new(source, budget.decoded_bytes);
        let destination = remote
            .description
            .endpoint
            .as_ref()
            .ok_or_else(|| failure("independently selected endpoint required"))?;
        thread_api::replication::opening::validate_endpoint(&scope.source).map_err(failure)?;
        thread_api::replication::opening::validate_endpoint(destination).map_err(failure)?;
        let mut scope_identity = scope.command.as_bytes().to_vec();
        for part in [
            scope.spool_genesis.as_bytes().as_slice(),
            scope.sharing_policy.as_bytes().as_slice(),
            scope.source.public_key.as_slice(),
            destination.public_key.as_slice(),
            scope
                .thread
                .spool
                .as_ref()
                .ok_or_else(|| failure("Spool absent"))?
                .id
                .as_bytes(),
            scope
                .thread
                .id
                .as_ref()
                .ok_or_else(|| failure("Thread absent"))?
                .value
                .as_slice(),
        ] {
            scope_identity.extend_from_slice(&(part.len() as u64).to_le_bytes());
            scope_identity.extend_from_slice(part);
        }
        scope_identity.extend_from_slice(&scope.source.kind.to_le_bytes());
        scope_identity.extend_from_slice(&destination.kind.to_le_bytes());
        let thread = remote.thread(scope.thread);
        let mut revisions = Vec::new();
        let mut artifact_bytes = 0u64;
        let metadata_remaining = Arc::new(AtomicUsize::new(budget.metadata_bytes));
        let mut metadata_bytes = budget.metadata_bytes;
        for state_id in &states {
            let (state, originals, references) =
                history.revision(*state_id, &counted, &mut metadata_bytes)?;
            let pack = SourcePack::prepare_with_references(
                &counted,
                &state,
                &references,
                scratch,
                SourceBudget {
                    max_decoded_bytes: counted.remaining(),
                },
            )
            .map_err(failure)?;
            for extent in pack.artifacts() {
                artifact_bytes = artifact_bytes
                    .checked_add(extent.length)
                    .ok_or_else(|| failure("history artifact byte overflow"))?;
            }
            if artifact_bytes > budget.artifact_bytes {
                return Err(failure("complete history artifact byte limit"));
            }
            let mut identity = scope_identity.clone();
            identity.extend_from_slice(state_id.as_bytes());
            let hash = ContentHash::compute_typed("git-source-revision-publication-v1", &identity);
            let id: OperationId = hash.to_hex()[..32].parse().map_err(failure)?;
            let publication = thread
                .prepare_publication(
                    &pack,
                    originals,
                    PublicationOptions {
                        client_operation_id: id.to_string(),
                        source: scope.source.clone(),
                        sharing_policy_version: scope.sharing_policy.as_bytes().to_vec(),
                        checkpoint: None,
                    },
                    scope.spool_genesis,
                )
                .map_err(failure)?;
            revisions.push(PreparedRevision {
                source: pack,
                publication,
                metadata_remaining: metadata_remaining.clone(),
            });
        }
        metadata_remaining.store(metadata_bytes, Ordering::Relaxed);
        check_disclosure(&states)?;
        Ok(Self {
            tip: selection.tip,
            revisions,
        })
    }
}
