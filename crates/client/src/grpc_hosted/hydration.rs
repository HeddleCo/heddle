use std::sync::Arc;

use objects::{
    error::HeddleError,
    object::{ChangeId, ContentHash},
};
use proto::ProtocolError;
use repo::{BlobHydrator, Repository};
use tokio::{runtime::Handle, sync::Mutex};

use super::HostedGrpcClient;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullMaterialization {
    Full,
    Lazy,
}

impl PullMaterialization {
    pub(crate) fn allows_partial_fetch(self) -> bool {
        matches!(self, Self::Lazy)
    }
}

impl HostedGrpcClient {
    pub async fn hydrate_pulled_state(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        target_state: ChangeId,
    ) -> Result<usize, ProtocolError> {
        self.hydrate_missing_blobs_for_state(repo, repo_path, remote_thread, target_state)
            .await
    }
}

/// Read-time blob hydrator for **hosted** lazy clones (issue #50).
///
/// Plugs into [`repo::Repository::set_blob_hydrator`]: when
/// [`Repository::require_blob`] hits a missing-blob marker left behind
/// by a lazy hosted clone (`heddle clone --lazy <hosted-url>` /
/// `--filter blob:none`), the read path delegates here, this hydrator
/// re-runs the pull with full materialization for `target_state` via
/// [`HostedGrpcClient::hydrate_pulled_state`] (see
/// `crates/client/src/grpc_hosted/sync.rs:541`), and the read is
/// retried against the freshly populated store.
///
/// ## Runtime constraints
///
/// `Repository::require_blob` is sync, but the underlying gRPC stack
/// is async. The hydrator bridges the gap with `Handle::block_on`, so
/// it MUST be invoked from a thread that is not currently inside the
/// runtime represented by `handle` — otherwise tokio will panic with
/// "Cannot start a runtime from within a runtime". In practice, the
/// caller should either:
///   1. construct the hydrator on a dedicated tokio runtime spun up
///      specifically for hosted-side I/O (the recommended setup for
///      the OSS CLI's single-threaded `current_thread` runtime), or
///   2. invoke `require_blob` from a `tokio::task::spawn_blocking` /
///      `block_in_place` scope on a multi-threaded runtime.
///
/// The clone command is responsible for picking the right setup when
/// it registers the hydrator. The hydrator itself does not own a
/// runtime so that callers retain full control over lifecycle and
/// concurrency.
pub struct HostedBlobHydrator {
    client: Arc<Mutex<HostedGrpcClient>>,
    repo_path: String,
    remote_thread: String,
    target_state: ChangeId,
    handle: Handle,
}

impl HostedBlobHydrator {
    pub fn new(
        client: Arc<Mutex<HostedGrpcClient>>,
        repo_path: String,
        remote_thread: String,
        target_state: ChangeId,
        handle: Handle,
    ) -> Self {
        Self {
            client,
            repo_path,
            remote_thread,
            target_state,
            handle,
        }
    }
}

impl BlobHydrator for HostedBlobHydrator {
    fn hydrate(&self, repo: &Repository, _hash: &ContentHash) -> objects::error::Result<()> {
        // `_hash` is ignored: `hydrate_pulled_state` refetches every
        // missing blob reachable from `target_state`, not just one.
        // This matches the hosted-side strategy that already exists
        // (sync.rs:541) and is the cheapest correct behaviour given
        // the partial-fetch metadata records the blake3 only, with no
        // path / state-id reverse lookup.
        let client = Arc::clone(&self.client);
        let repo_path = self.repo_path.clone();
        let remote_thread = self.remote_thread.clone();
        let target_state = self.target_state;
        let result: Result<usize, ProtocolError> = self.handle.block_on(async move {
            let mut client = client.lock().await;
            client
                .hydrate_pulled_state(repo, &repo_path, &remote_thread, target_state)
                .await
        });
        result
            .map(|_count| ())
            .map_err(|err| HeddleError::Io(std::io::Error::other(err.to_string())))
    }
}
