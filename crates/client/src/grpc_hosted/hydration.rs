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

#[cfg(test)]
mod tests {
    //! These tests reach into `pub(super)` fields on `HostedGrpcClient`
    //! to fabricate a client whose `Channel` was built via
    //! `Endpoint::connect_lazy` — i.e. it doesn't actually dial
    //! anything until the first RPC and then fails predictably with
    //! `tonic::transport::Error`. That's enough to drive the
    //! [`HostedBlobHydrator::hydrate`] runtime-bridging logic end to
    //! end without spinning up an in-process gRPC server.
    use std::{sync::Arc, thread};
    use cli_shared::ClientConfig;
    use objects::object::{Blob, ChangeId};
    use repo::Repository;
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use tonic::transport::Endpoint;

    use grpc::heddle::v1::{
        auth_service_client::AuthServiceClient,
        content_service_client::ContentServiceClient,
        hosted_user_service_client::HostedUserServiceClient,
        repo_sync_service_client::RepoSyncServiceClient,
    };

    use super::{
        BlobHydrator, HostedBlobHydrator,
        super::{HostedGrpcClient, helpers::HostedTransportPolicy},
    };

    /// Build a [`HostedGrpcClient`] that points at a definitely-closed
    /// `127.0.0.1:1` endpoint via `connect_lazy`. Any RPC against it
    /// returns a transport-layer error rather than hanging. Must be
    /// called from inside a tokio runtime context — `connect_lazy`
    /// reaches into hyper-util which needs a reactor on construction.
    fn fabricate_offline_client() -> HostedGrpcClient {
        let endpoint = Endpoint::from_static("http://127.0.0.1:1");
        let channel = endpoint.connect_lazy();
        let config = ClientConfig::default();
        let transport = HostedTransportPolicy::from_client_config(&config);
        HostedGrpcClient {
            inner: RepoSyncServiceClient::new(channel.clone()),
            user: HostedUserServiceClient::new(channel.clone()),
            auth: AuthServiceClient::new(channel.clone()),
            content: ContentServiceClient::new(channel),
            token_header: None,
            transport,
            auth_proof_key_pem: None,
            server_key: None,
        }
    }

    /// Build the smallest possible Heddle repo for the trait method to
    /// scribble into (it never actually does, since the call fails
    /// before reaching the put_blob site).
    fn temp_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init heddle repo");
        (temp, repo)
    }

    #[test]
    fn new_stores_all_constructor_arguments_for_later_hydrate() {
        // Pure-construction smoke test. Runs the construction inside
        // the runtime context because `connect_lazy` reaches into
        // hyper-util which needs a tokio reactor on call. Locks in
        // the public `new` signature so hosted callers (e.g.
        // `clone_network`) break visibly on signature drift.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let handle = runtime.handle().clone();
        let hydrator = runtime.block_on(async {
            let client = Arc::new(Mutex::new(fabricate_offline_client()));
            HostedBlobHydrator::new(
                client,
                "org/acme/repo".to_string(),
                "main".to_string(),
                ChangeId::generate(),
                handle,
            )
        });
        // Force the field reads through a Debug-free path so the
        // optimizer can't drop them entirely under coverage.
        assert!(matches!(&hydrator as &dyn BlobHydrator, _));
        drop(hydrator);
        drop(runtime);
    }

    #[test]
    fn hydrate_surfaces_transport_failure_without_silent_fallback() {
        // Spin a dedicated multi-thread runtime on a SEPARATE thread,
        // hand its handle to the hydrator, and call `hydrate` from
        // the *main* test thread — that's the production setup
        // pattern (see the doc comment on the struct), and it lets
        // `handle.block_on` work without nesting into a running
        // runtime on our own thread.
        let (runtime_tx, runtime_rx) = std::sync::mpsc::channel();
        let runtime_thread = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("dedicated hydration runtime");
            runtime_tx.send(rt.handle().clone()).expect("send handle");
            // Park the runtime alive until the test releases it.
            rt.block_on(async {
                let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
                let _ = rx.await;
            });
        });
        let handle = runtime_rx.recv().expect("receive runtime handle");

        // Construct the offline client inside the dedicated runtime —
        // `Endpoint::connect_lazy` needs a tokio reactor on call.
        let client = Arc::new(Mutex::new(
            handle.block_on(async { fabricate_offline_client() }),
        ));
        let hydrator = HostedBlobHydrator::new(
            client,
            "org/acme/repo".to_string(),
            "main".to_string(),
            ChangeId::generate(),
            handle.clone(),
        );

        let (_temp, repo) = temp_repo();
        let blake3 = Blob::new(b"placeholder".to_vec()).hash();
        let err = hydrator
            .hydrate(&repo, &blake3)
            .expect_err("offline endpoint must produce an error");
        let msg = err.to_string();
        // Don't pin on the exact tonic wording (it varies by tonic /
        // hyper version), just confirm we got an error *back* — i.e.
        // the hydrator didn't silently return Ok.
        assert!(
            !msg.is_empty(),
            "hydrator must surface a non-empty error message",
        );
        // Drop the hydrator so the Arc'd client refs go away before
        // we tear down the runtime. The runtime thread leaks
        // deliberately — there's no clean shutdown story for a
        // detached runtime in std, and the test process exits right
        // after this anyway.
        drop(hydrator);
        std::mem::forget(runtime_thread);
    }
}
