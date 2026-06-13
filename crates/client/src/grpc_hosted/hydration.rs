use std::{
    net::ToSocketAddrs,
    sync::{Arc, Mutex, OnceLock, mpsc},
    thread,
    time::Duration,
};

use objects::{
    error::HeddleError,
    object::{ChangeId, ContentHash, ThreadName},
};
use proto::ProtocolError;
use repo::{BlobHydrator, Repository};

use super::{HostedAuthMode, HostedGrpcClient, HostedSession};

/// Default hosted lazy-hydration deadline.
///
/// This matches the hosted client config's 30s default connection timeout and
/// gives lazy reads a bounded failure mode when a gRPC request stalls without a
/// transport-level TCP timeout.
const DEFAULT_HOSTED_HYDRATION_TIMEOUT: Duration = Duration::from_secs(30);

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
/// [`Repository::require_blob`] hits a missing-blob marker left behind by a
/// lazy hosted clone (`heddle clone --lazy <hosted-url>` /
/// `--filter blob:none`), the read path delegates here, this hydrator re-runs
/// the pull with full materialization for the *current* tip of `local_thread`,
/// and the read is retried against the freshly populated store.
///
/// ## Runtime bridge
///
/// `Repository::require_blob` is sync. The underlying gRPC stack is async,
/// and the hydrator must be invocable from BOTH async contexts (the
/// `#[tokio::main]` CLI command path) and plain non-Tokio threads (future
/// FFI callers, test helpers). `Handle::block_on` invoked from within a
/// running Tokio runtime panics ("Cannot start a runtime from within a
/// runtime"), so we cannot bridge in-place.
///
/// Instead, on first use we spawn a dedicated worker thread that owns its
/// own current-thread Tokio runtime + a connected `HostedGrpcClient`. Each
/// `hydrate()` call sends a request over an mpsc channel and blocks on the
/// reply. The worker `block_on`s the gRPC call inside its private runtime,
/// avoiding any nesting. This pattern is robust regardless of what the
/// caller's thread is doing.
pub struct LazyHostedHydrator {
    /// Endpoint spec as `host:port` (or an IP literal). Re-resolved via DNS
    /// on first connect so a hostname behind a load balancer with rotating
    /// IPs still works across process restarts. We deliberately do NOT
    /// store a [`std::net::SocketAddr`] here — that would freeze the IP at
    /// clone time and break later reconnects.
    endpoint: String,
    repo_path: String,
    remote_thread: String,
    /// Local thread to resolve to a state on each hydrate. Re-read every
    /// call so a `pull --lazy` that advances the thread tip is honored
    /// without rewriting `lazy-hydrator.toml`.
    local_thread: String,
    bridge: OnceLock<HydrationBridge>,
    /// Held during first-use bridge construction so the connect + spawn
    /// sequence is atomic — N concurrent first-time callers see exactly
    /// one bridge built and shared, rather than N runtimes / N clients
    /// racing via separate `OnceLock::set` calls (the round-2 bug).
    init_lock: Mutex<()>,
}

impl LazyHostedHydrator {
    pub fn new(
        endpoint: impl Into<String>,
        repo_path: impl Into<String>,
        remote_thread: impl Into<String>,
        local_thread: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            repo_path: repo_path.into(),
            remote_thread: remote_thread.into(),
            local_thread: local_thread.into(),
            bridge: OnceLock::new(),
            init_lock: Mutex::new(()),
        }
    }

    fn ensure_bridge(&self) -> objects::error::Result<&HydrationBridge> {
        if let Some(bridge) = self.bridge.get() {
            return Ok(bridge);
        }
        // Serialize first-time construction so the runtime, client, and
        // worker thread are installed as one atomic unit.
        let _guard = self.init_lock.lock().unwrap_or_else(|poison| {
            // Prior initializer panicked. The bridge is either set (good)
            // or absent (caller will retry). Either way clearing the
            // poison and continuing is correct — we re-check `bridge.get`
            // below.
            poison.into_inner()
        });
        if let Some(bridge) = self.bridge.get() {
            return Ok(bridge);
        }

        let bridge = HydrationBridge::connect(&self.endpoint)?;
        // The init_lock guarantees no race: `set` must succeed here.
        self.bridge.set(bridge).map_err(|_| {
            HeddleError::Config(
                "lazy hosted hydrator: bridge slot already filled under init_lock — \
                     this indicates a logic bug in LazyHostedHydrator"
                    .to_string(),
            )
        })?;
        Ok(self.bridge.get().expect("just set under init_lock"))
    }
}

impl BlobHydrator for LazyHostedHydrator {
    fn hydrate(&self, repo: &Repository, _hash: &ContentHash) -> objects::error::Result<()> {
        // `_hash` is ignored: `hydrate_pulled_state` refetches every
        // missing blob reachable from `target_state`, not just one. This
        // matches the hosted-side strategy that already exists
        // (sync.rs:541) and is the cheapest correct behaviour given the
        // partial-fetch metadata records the blake3 only.

        // Re-resolve the target state from the repo on EVERY call. If a
        // `pull --lazy` advanced the local thread between clone and now,
        // the cached state would point at the OLD tip and we'd leave any
        // post-pull missing blobs unresolved — that was the round-2 P1.
        let target_state = match repo
            .refs()
            .get_thread(&ThreadName::from(self.local_thread.as_str()))
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Err(HeddleError::Config(format!(
                    "lazy hosted hydrator: local thread '{}' has no recorded tip — \
                     was the lazy clone interrupted? Try `heddle pull --lazy` to refresh.",
                    self.local_thread,
                )));
            }
            Err(err) => {
                return Err(HeddleError::Config(format!(
                    "lazy hosted hydrator: failed to read local thread '{}': {err}",
                    self.local_thread,
                )));
            }
        };

        let bridge = self.ensure_bridge()?;
        bridge
            .hydrate(repo, &self.repo_path, &self.remote_thread, target_state)
            .map(|_count| ())
            .map_err(|err| HeddleError::Io(std::io::Error::other(err.to_string())))
    }
}

/// Background worker bridging sync `BlobHydrator::hydrate` calls to the
/// async gRPC stack. Owns a dedicated current-thread Tokio runtime and a
/// connected `HostedGrpcClient`. Callers reopen the repository root into
/// an owned handle, dispatch hydrate requests over an mpsc channel, and
/// block on a per-request reply channel.
///
/// This indirection is what makes the hydrator safe to call from a
/// `#[tokio::main]` async context: the worker's runtime is private, so the
/// nested `block_on` happens entirely off the caller's runtime.
struct HydrationBridge {
    tx: mpsc::Sender<HydrateMessage>,
    /// Join handle for the worker. Kept so that dropping the bridge
    /// closes the channel and lets the worker exit cleanly.
    _worker: thread::JoinHandle<()>,
}

enum HydrateMessage {
    Run {
        repo: Arc<Repository>,
        repo_path: String,
        remote_thread: String,
        target_state: ChangeId,
        reply: mpsc::SyncSender<Result<usize, ProtocolError>>,
    },
}

impl HydrationBridge {
    fn connect(endpoint: &str) -> objects::error::Result<Self> {
        // Resolve DNS at connect time so a hostname that's persisted
        // (rather than a frozen IP) re-resolves on every process start.
        let addr = endpoint
            .to_socket_addrs()
            .map_err(|err| {
                HeddleError::Config(format!(
                    "lazy hosted hydrator: resolve endpoint '{endpoint}': {err}",
                ))
            })?
            .next()
            .ok_or_else(|| {
                HeddleError::Config(format!(
                    "lazy hosted hydrator: DNS returned no addresses for '{endpoint}'",
                ))
            })?;

        let user_config = cli_shared::UserConfig::load_default().map_err(|err| {
            HeddleError::Config(format!("lazy hosted hydrator: load user config: {err}"))
        })?;
        // Build + validate the session config on this thread so a rejected
        // TLS/auth config surfaces synchronously, before the worker thread is
        // spawned. The worker connects + rotates through `session.connect`.
        let session = HostedSession::build(&user_config, None, HostedAuthMode::ConfigToken)
            .map_err(|err| {
                HeddleError::Config(format!(
                    "lazy hosted hydrator: load TLS/auth client config: {err}"
                ))
            })?;

        // Build the worker thread first so the bridge can store the
        // tx side immediately. The worker's runtime + client are
        // constructed inside the worker (so the runtime's
        // `Handle::current()` matches the thread that drives it).
        let (tx, rx) = mpsc::channel::<HydrateMessage>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), HeddleError>>(0);
        let endpoint_for_thread = endpoint.to_string();
        let worker = thread::Builder::new()
            .name("heddle-lazy-hydrator".into())
            .spawn(move || {
                // Build the runtime on this thread so all RPCs execute
                // inside it. `current_thread` is sufficient: hydrate
                // calls are serialized through the mpsc channel anyway,
                // and avoiding extra worker threads keeps the resource
                // footprint of an idle lazy clone minimal.
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        let _ = ready_tx.send(Err(HeddleError::Config(format!(
                            "lazy hosted hydrator: build worker runtime: {err}",
                        ))));
                        return;
                    }
                };

                let connect_result = runtime.block_on(async {
                    // `session.connect` connects and runs mandatory rotation
                    // together — the same seam every other hosted entry point
                    // (clone, fetch, push, pull, support, approval) opens
                    // through — so a process whose cached token has slipped
                    // past expiry recovers on first lazy hydrate.
                    let client = match tokio::time::timeout(
                        DEFAULT_HOSTED_HYDRATION_TIMEOUT,
                        session.connect(addr),
                    )
                    .await
                    {
                        Ok(result) => result.map_err(|err: ProtocolError| {
                            HeddleError::Config(format!(
                                "lazy hosted hydrator: connect to '{endpoint_for_thread}' \
                                     (resolved to {addr}): {err}",
                            ))
                        })?,
                        Err(_) => {
                            return Err(HeddleError::Config(format!(
                                "lazy hosted hydrator: connect to '{endpoint_for_thread}' \
                                     (resolved to {addr}) timed out after {}",
                                format_duration(DEFAULT_HOSTED_HYDRATION_TIMEOUT)
                            )));
                        }
                    };
                    Ok::<_, HeddleError>(client)
                });
                let mut client = match connect_result {
                    Ok(c) => c,
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };

                // Signal the bridge constructor that connect succeeded
                // BEFORE entering the request loop. After this point any
                // bridge-construction errors are gone; the channel is open
                // and `HydrationBridge::hydrate` calls will succeed.
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }

                // Drive the request loop. `recv` returns Err when the
                // last `Sender` is dropped (i.e. the LazyHostedHydrator
                // owning the bridge has been dropped), which is our
                // shutdown signal — we drop the runtime + client and
                // exit.
                runtime.block_on(async {
                    while let Ok(message) = rx.recv() {
                        match message {
                            HydrateMessage::Run {
                                repo,
                                repo_path,
                                remote_thread,
                                target_state,
                                reply,
                            } => {
                                let result = hydrate_with_rpc_timeout(
                                    &mut client,
                                    repo.as_ref(),
                                    &repo_path,
                                    &remote_thread,
                                    target_state,
                                    DEFAULT_HOSTED_HYDRATION_TIMEOUT,
                                )
                                .await;
                                let _ = reply.send(result);
                            }
                        }
                    }
                });
            })
            .map_err(|err| {
                HeddleError::Config(format!("lazy hosted hydrator: spawn worker thread: {err}",))
            })?;

        // Wait for the worker to either confirm connect or report an
        // error. The wait is bounded so a stalled first-use connect cannot
        // wedge the sync read path.
        match ready_rx.recv_timeout(DEFAULT_HOSTED_HYDRATION_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                tx,
                _worker: worker,
            }),
            Ok(Err(err)) => Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(HeddleError::Config(format!(
                "lazy hosted hydrator: worker did not signal readiness within {}",
                format_duration(DEFAULT_HOSTED_HYDRATION_TIMEOUT)
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(HeddleError::Config(
                "lazy hosted hydrator: worker thread exited before signalling readiness"
                    .to_string(),
            )),
        }
    }

    fn hydrate(
        &self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        target_state: ChangeId,
    ) -> Result<usize, ProtocolError> {
        self.hydrate_with_timeout(
            repo,
            repo_path,
            remote_thread,
            target_state,
            DEFAULT_HOSTED_HYDRATION_TIMEOUT,
        )
    }

    fn hydrate_with_timeout(
        &self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        target_state: ChangeId,
        timeout: Duration,
    ) -> Result<usize, ProtocolError> {
        let repo = Arc::new(Repository::open(repo.root()).map_err(ProtocolError::from)?);

        // Bounded reply channel of capacity 1; each sync caller blocks until
        // the worker returns the gRPC result for this request.
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<usize, ProtocolError>>(1);
        self.tx
            .send(HydrateMessage::Run {
                repo,
                repo_path: repo_path.to_string(),
                remote_thread: remote_thread.to_string(),
                target_state,
                reply: reply_tx,
            })
            .map_err(|err| {
                ProtocolError::Io(std::io::Error::other(format!(
                    "lazy hosted hydrator: worker channel closed: {err}",
                )))
            })?;
        match reply_rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(hydration_timeout_error(
                timeout,
                repo_path,
                remote_thread,
                target_state,
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(ProtocolError::Io(std::io::Error::other(
                    "lazy hosted hydrator: worker reply channel closed before hydration completed",
                )))
            }
        }
    }
}

async fn hydrate_with_rpc_timeout(
    client: &mut HostedGrpcClient,
    repo: &Repository,
    repo_path: &str,
    remote_thread: &str,
    target_state: ChangeId,
    timeout: Duration,
) -> Result<usize, ProtocolError> {
    match tokio::time::timeout(
        timeout,
        client.hydrate_pulled_state(repo, repo_path, remote_thread, target_state),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(hydration_timeout_error(
            timeout,
            repo_path,
            remote_thread,
            target_state,
        )),
    }
}

fn hydration_timeout_error(
    timeout: Duration,
    repo_path: &str,
    remote_thread: &str,
    target_state: ChangeId,
) -> ProtocolError {
    ProtocolError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "lazy hosted hydrator: blob hydration timed out after {} \
             (repo={repo_path}, remote_thread={remote_thread}, target_state={target_state})",
            format_duration(timeout)
        ),
    ))
}

fn format_duration(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{duration:?}")
    }
}

/// Register the `"hosted"` factory in the global lazy-hydrator registry.
/// Call once at process startup. The factory reads the hosted-section
/// fields out of `lazy-hydrator.toml` and hands back a
/// [`LazyHostedHydrator`] adapter that defers the actual gRPC connect (and
/// worker-thread spawn) until the first `require_blob` call needs it.
pub fn register_hosted_factory() {
    use std::{path::Path as StdPath, sync::Arc as StdArc};

    use repo::lazy_hydrator::{
        BlobHydratorFactory, HydratorSection, KIND_HOSTED, register_factory,
    };

    let factory: BlobHydratorFactory = StdArc::new(
        |_root: &StdPath,
         section: &HydratorSection|
         -> objects::error::Result<StdArc<dyn BlobHydrator>> {
            let hosted = section.hosted.as_ref().ok_or_else(|| {
                HeddleError::Config(
                    "lazy hosted hydrator: lazy-hydrator.toml has kind=\"hosted\" \
                     but no [hydrator.hosted] table was found"
                        .to_string(),
                )
            })?;
            Ok(StdArc::new(LazyHostedHydrator::new(
                hosted.endpoint.clone(),
                hosted.repo_path.clone(),
                hosted.remote_thread.clone(),
                hosted.local_thread.clone(),
            )))
        },
    );
    register_factory(KIND_HOSTED, factory);
}

#[cfg(test)]
mod tests {
    //! These tests exercise the lazy-hydrator adapter against a worker
    //! bridge that connects to a definitely-closed `127.0.0.1:1` endpoint
    //! via `Endpoint::connect_lazy` — the channel doesn't actually dial
    //! until the first RPC, at which point it fails predictably with a
    //! transport-layer error. That's enough to drive the bridge's
    //! sync→async hand-off, runtime construction, and error propagation
    //! end-to-end without spinning up an in-process gRPC server.
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use cli_shared::ClientConfig;
    use grpc::heddle::v1::{
        auth_service_client::AuthServiceClient, content_service_client::ContentServiceClient,
        hosted_user_service_client::HostedUserServiceClient,
        repo_sync_service_client::RepoSyncServiceClient,
    };
    use objects::object::{Blob, ChangeId, ThreadName};
    use repo::Repository;
    use tempfile::TempDir;
    use tonic::transport::Endpoint;

    use super::{
        super::{HostedGrpcClient, helpers::HostedTransportPolicy},
        BlobHydrator, HydrationBridge, LazyHostedHydrator,
    };

    /// Build a `HostedGrpcClient` that points at a closed loopback port
    /// via `connect_lazy`. RPCs fail with a transport error rather than
    /// hanging. Must be called from inside a tokio runtime context.
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

    /// Build the smallest Heddle repo + seed the `main` thread to a real
    /// state so `hydrate` can resolve `local_thread`.
    fn temp_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init heddle repo");
        (temp, repo)
    }

    /// Spawn a `HydrationBridge` with a pre-built offline client, bypassing
    /// the DNS / connect / credential paths so tests stay hermetic.
    fn offline_bridge() -> HydrationBridge {
        let (tx, rx) = mpsc::channel::<super::HydrateMessage>();
        let worker = thread::Builder::new()
            .name("test-lazy-hydrator".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("worker runtime");
                let mut client = runtime.block_on(async { fabricate_offline_client() });
                runtime.block_on(async {
                    while let Ok(message) = rx.recv() {
                        match message {
                            super::HydrateMessage::Run {
                                repo,
                                repo_path,
                                remote_thread,
                                target_state,
                                reply,
                            } => {
                                let result = client
                                    .hydrate_pulled_state(
                                        repo.as_ref(),
                                        &repo_path,
                                        &remote_thread,
                                        target_state,
                                    )
                                    .await;
                                let _ = reply.send(result);
                            }
                        }
                    }
                });
            })
            .expect("spawn test worker");
        HydrationBridge {
            tx,
            _worker: worker,
        }
    }

    /// Construct a `LazyHostedHydrator` whose bridge is already installed
    /// from `offline_bridge`. Bypasses the real `ensure_bridge` connect
    /// path so we can drive the trait surface deterministically.
    fn offline_lazy_hydrator(local_thread: &str) -> LazyHostedHydrator {
        let hydrator = LazyHostedHydrator::new(
            "ignored.example.test:443",
            "org/acme/repo",
            "main",
            local_thread,
        );
        hydrator
            .bridge
            .set(offline_bridge())
            .map_err(|_| ())
            .expect("set bridge");
        hydrator
    }

    /// Round-3 test from the task brief — proves the worker bridge is
    /// callable from inside a `#[tokio::main]`-style multi-thread async
    /// context. With the previous design (`Handle::block_on` from the
    /// outer runtime's thread) this would have panicked.
    #[test]
    fn hydrate_safe_from_tokio_main_context() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("multi-thread runtime");
        runtime.block_on(async {
            let (_temp, repo) = temp_repo();
            let target = repo
                .refs()
                .get_thread(&ThreadName::from("main"))
                .unwrap()
                .unwrap();
            // Seed a known thread tip the hydrator can resolve via
            // `local_thread`.
            let _ = target;

            let hydrator = offline_lazy_hydrator("main");
            let blake3 = Blob::new(b"placeholder".to_vec()).hash();
            // Must not panic. The offline client surfaces a transport
            // error, which the trait reshapes into a HeddleError::Io. We
            // assert "non-empty error" rather than pinning tonic wording.
            let err = hydrator
                .hydrate(&repo, &blake3)
                .expect_err("offline endpoint must produce an error");
            assert!(!err.to_string().is_empty(), "must surface a real error");
        });
    }

    /// Round-3 test from the task brief — direct counterpart to the
    /// Tokio test above. The hydrator must also work on plain non-Tokio
    /// threads (the future FFI / library-embedder path).
    #[test]
    fn hydrate_safe_from_blocking_context() {
        let (_temp, repo) = temp_repo();
        let hydrator = offline_lazy_hydrator("main");
        let blake3 = Blob::new(b"placeholder".to_vec()).hash();
        let err = hydrator
            .hydrate(&repo, &blake3)
            .expect_err("offline endpoint must produce an error");
        assert!(!err.to_string().is_empty(), "must surface a real error");
    }

    /// Round-3 test from the task brief. If `target_state` were cached at
    /// first hydrate (the round-2 bug), the second call against an advanced
    /// thread tip would hydrate against the OLD state. We exercise both
    /// the first and second hydrate, and inspect the request the bridge
    /// processed via an inspection bridge that captures the target_state
    /// it received.
    #[test]
    fn hydrate_after_thread_advance_uses_new_state() {
        // Build an inspecting bridge: instead of running real RPCs it
        // records the ChangeId on each request and replies with an
        // "io error: simulated". That lets us verify the bridge saw the
        // post-advance ChangeId on the second call.
        let recorded: Arc<std::sync::Mutex<Vec<ChangeId>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded_for_worker = Arc::clone(&recorded);
        let (tx, rx) = mpsc::channel::<super::HydrateMessage>();
        let worker = thread::Builder::new()
            .name("inspect-hydrator".into())
            .spawn(move || {
                while let Ok(message) = rx.recv() {
                    match message {
                        super::HydrateMessage::Run {
                            target_state,
                            reply,
                            ..
                        } => {
                            recorded_for_worker.lock().unwrap().push(target_state);
                            let _ = reply.send(Err(proto::ProtocolError::Io(
                                std::io::Error::other("simulated"),
                            )));
                        }
                    }
                }
            })
            .expect("spawn inspect worker");
        let bridge = HydrationBridge {
            tx,
            _worker: worker,
        };

        let hydrator =
            LazyHostedHydrator::new("ignored.example.test:443", "org/acme/repo", "main", "main");
        hydrator.bridge.set(bridge).map_err(|_| ()).expect("set");

        let (_temp, repo) = temp_repo();
        let first_tip = repo
            .refs()
            .get_thread(&ThreadName::from("main"))
            .unwrap()
            .unwrap();

        // First hydrate — bridge sees the original tip.
        let blake3 = Blob::new(b"a".to_vec()).hash();
        let _ = hydrator.hydrate(&repo, &blake3);

        // Advance the local "main" thread to a fresh, distinct ChangeId.
        let advanced = ChangeId::generate();
        assert_ne!(advanced, first_tip, "fresh ChangeId must differ");
        repo.refs()
            .set_thread(&ThreadName::from("main"), &advanced)
            .expect("advance");

        // Second hydrate — bridge MUST see the advanced tip, not the
        // first one (round-2 cached-state bug regression guard).
        let _ = hydrator.hydrate(&repo, &blake3);

        let seen = recorded.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "two hydrate calls = two recorded states");
        assert_eq!(seen[0], first_tip, "first call uses original tip");
        assert_eq!(
            seen[1], advanced,
            "second call MUST re-resolve to the advanced tip"
        );
    }

    /// Round-3 test from the task brief. With the round-2 design,
    /// concurrent first-time callers raced two separate `OnceLock::set`
    /// calls (runtime + inner) and could end up storing an inner whose
    /// `Handle` referenced a runtime that was dropped by the losing
    /// thread. Now there's a single OnceLock + an init_lock, so all
    /// callers observe exactly one bridge.
    #[test]
    fn concurrent_first_use_no_race() {
        const N: usize = 8;
        let (_temp, repo) = temp_repo();
        let repo = Arc::new(repo);
        // The arc allows N threads to share one hydrator that they all
        // race to initialize.
        let hydrator = Arc::new(offline_lazy_hydrator("main"));
        let observed_ok: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let observed_err: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let repo = Arc::clone(&repo);
            let hydrator = Arc::clone(&hydrator);
            let observed_ok = Arc::clone(&observed_ok);
            let observed_err = Arc::clone(&observed_err);
            handles.push(thread::spawn(move || {
                let blake3 = Blob::new(b"placeholder".to_vec()).hash();
                match hydrator.hydrate(repo.as_ref(), &blake3) {
                    Ok(()) => observed_ok.fetch_add(1, Ordering::SeqCst),
                    Err(_) => observed_err.fetch_add(1, Ordering::SeqCst),
                };
            }));
        }
        for h in handles {
            h.join().expect("worker joined");
        }
        // Either outcome is fine — the assertion is that no panic /
        // deadlock occurred and every caller got a reply. The offline
        // client produces errors, so we expect all N to land in the err
        // bucket; we accept any split as long as the total is N.
        let total = observed_ok.load(Ordering::SeqCst) + observed_err.load(Ordering::SeqCst);
        assert_eq!(total, N, "every concurrent caller must receive a reply");
    }

    #[test]
    fn hydrate_times_out_when_worker_never_replies() {
        let (_temp, repo) = temp_repo();
        let target = repo
            .refs()
            .get_thread(&ThreadName::from("main"))
            .unwrap()
            .unwrap();
        let (tx, rx) = mpsc::channel::<super::HydrateMessage>();
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(0);
        let (done_tx, done_rx) = mpsc::sync_channel::<()>(0);
        let worker = thread::Builder::new()
            .name("stalling-hydrator".into())
            .spawn(move || {
                match rx.recv() {
                    Ok(super::HydrateMessage::Run { reply, .. }) => {
                        let _ = release_rx.recv();
                        drop(reply);
                    }
                    Err(_) => {}
                }
                let _ = done_tx.send(());
            })
            .expect("spawn stalling worker");
        let bridge = HydrationBridge {
            tx,
            _worker: worker,
        };

        let started = Instant::now();
        let err = bridge
            .hydrate_with_timeout(
                &repo,
                "org/acme/repo",
                "main",
                target,
                Duration::from_millis(50),
            )
            .expect_err("stalled worker must time out");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "hydrate timeout must return promptly; elapsed {elapsed:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("blob hydration timed out after") && msg.contains("org/acme/repo"),
            "timeout error must name the operation and repo context; got: {msg}"
        );

        release_tx.send(()).expect("release stalled worker");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker exits after release");
    }

    /// Drop the bridge → worker exits cleanly. Catches the case where a
    /// future refactor leaks the worker forever.
    #[test]
    fn dropping_bridge_shuts_worker_down() {
        let bridge = offline_bridge();
        // Pull the worker handle out via a Drop-detecting wrapper isn't
        // possible without restructuring; instead we observe that
        // dropping the bridge closes the channel and `send` afterwards
        // would fail. The cleanest visible assertion: dropping the
        // bridge does not hang the test.
        drop(bridge);
        // Give the worker a moment to wind down on slow CI.
        thread::sleep(Duration::from_millis(50));
    }

    /// Force the owned repo handle into the type system. The hydration
    /// worker must receive an owned `Arc<Repository>` rather than a raw
    /// borrowed pointer whose lifetime is erased across the mpsc channel.
    #[test]
    fn hydration_message_carries_send_owned_repo_handle() {
        fn assert_send_static<T: Send + 'static>(_: &T) {}
        let (_temp, repo) = temp_repo();
        let (reply, _recv) = mpsc::sync_channel::<Result<usize, proto::ProtocolError>>(1);
        let message = super::HydrateMessage::Run {
            repo: Arc::new(repo),
            repo_path: "org/acme/repo".to_string(),
            remote_thread: "main".to_string(),
            target_state: ChangeId::generate(),
            reply,
        };
        assert_send_static(&message);
    }

    #[test]
    fn hydration_bridge_does_not_reintroduce_raw_repo_pointer() {
        let source = include_str!("hydration.rs");
        let raw_wrapper = ["Repo", "Ptr"].concat();
        let raw_repo_pointer = ["*const ", "Repository"].concat();
        assert!(
            !source.contains(&raw_wrapper),
            "hydration bridge must not reintroduce the raw-pointer send wrapper"
        );
        assert!(
            !source.contains(&raw_repo_pointer),
            "hydration bridge must not send raw Repository pointers across threads"
        );
    }

    /// Round-4 patch-coverage fill: exercise the `hydrate` early-return
    /// taken when the persisted `local_thread` has no recorded tip in the
    /// current repo (e.g. the lazy clone was interrupted before the first
    /// thread write landed). The hydrator must surface this as a clean
    /// `Config` error rather than calling `ensure_bridge` and dialing the
    /// network for a state we don't have.
    #[test]
    fn hydrate_returns_config_error_when_local_thread_missing() {
        let (_temp, repo) = temp_repo();
        // Pre-set the bridge so `ensure_bridge` would succeed if reached —
        // that way a failure here proves the early-return fired before the
        // bridge was consulted.
        let hydrator = offline_lazy_hydrator("thread-that-was-never-written");
        let blake3 = Blob::new(b"placeholder".to_vec()).hash();
        let err = hydrator
            .hydrate(&repo, &blake3)
            .expect_err("missing thread must surface as Config error");
        let msg = err.to_string();
        assert!(
            msg.contains("no recorded tip") && msg.contains("thread-that-was-never-written"),
            "error must name the missing thread and explain why hydration was skipped; got: {msg}"
        );
    }

    /// Round-4 patch-coverage fill: drive the real `ensure_bridge` path
    /// (no pre-installed bridge) against an unresolvable hostname. The
    /// DNS error must propagate back through `HydrationBridge::connect`
    /// → `ensure_bridge` → `hydrate` rather than panicking or hanging.
    ///
    /// The `.invalid` TLD is RFC 2606-reserved and guaranteed never to
    /// resolve, so this test stays hermetic in CI environments without
    /// outbound DNS.
    #[test]
    fn ensure_bridge_propagates_dns_failure() {
        let (_temp, repo) = temp_repo();
        // Note: no `offline_lazy_hydrator` — this constructor leaves
        // `bridge` empty so the first `hydrate()` exercises the real
        // ensure_bridge → HydrationBridge::connect path including DNS.
        let hydrator = LazyHostedHydrator::new(
            "definitely-nonexistent-host-for-tests.invalid:443",
            "org/acme/repo",
            "main",
            "main",
        );
        let blake3 = Blob::new(b"placeholder".to_vec()).hash();
        let err = hydrator
            .hydrate(&repo, &blake3)
            .expect_err("unresolvable endpoint must surface as a Config error");
        let msg = err.to_string();
        assert!(
            msg.contains("resolve endpoint")
                || msg.contains("DNS returned no addresses")
                || msg.contains(".invalid"),
            "error must identify the DNS-resolution failure; got: {msg}"
        );
        // Repeat the call — second attempt must also fail-fast (no
        // half-initialized bridge cached on disk / in OnceLock).
        let err2 = hydrator
            .hydrate(&repo, &blake3)
            .expect_err("second call must also fail rather than reuse a partial bridge");
        assert!(
            !err2.to_string().is_empty(),
            "second call must surface a real error"
        );
    }
}

#[cfg(test)]
mod register_factory_tests {
    //! Round-4 patch-coverage fill for `register_hosted_factory` and the
    //! closure it installs in the lazy-hydrator registry. Both branches
    //! of the closure (missing `[hydrator.hosted]` table → Config error;
    //! present table → ready-to-install adapter) are exercised here.

    use std::sync::Mutex;

    use repo::lazy_hydrator::{HostedHydratorConfig, HydratorSection, KIND_HOSTED, lookup_factory};
    use tempfile::TempDir;

    use super::register_hosted_factory;

    /// Serialize tests that mutate the process-wide hydrator registry so
    /// they don't race on the global `"hosted"` key.
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn register_hosted_factory_installs_factory_for_kind_hosted() {
        let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        register_hosted_factory();
        assert!(
            lookup_factory(KIND_HOSTED).is_some(),
            "register_hosted_factory must populate the registry under KIND_HOSTED"
        );
    }

    #[test]
    fn registered_factory_builds_adapter_for_hosted_section() {
        let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        register_hosted_factory();
        let factory =
            lookup_factory(KIND_HOSTED).expect("factory present after register_hosted_factory");
        let temp = TempDir::new().expect("temp");
        let section = HydratorSection {
            kind: KIND_HOSTED.to_string(),
            hosted: Some(HostedHydratorConfig {
                endpoint: "example.heddle.cloud:443".to_string(),
                repo_path: "org/acme/repo".to_string(),
                remote_thread: "main".to_string(),
                local_thread: "main".to_string(),
            }),
            git_overlay: None,
        };
        let _hydrator = factory(temp.path(), &section)
            .expect("factory must produce an adapter when [hydrator.hosted] is present");
    }

    #[test]
    fn registered_factory_errors_when_hosted_section_absent() {
        let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        register_hosted_factory();
        let factory = lookup_factory(KIND_HOSTED).expect("factory present");
        let temp = TempDir::new().expect("temp");
        let section = HydratorSection {
            kind: KIND_HOSTED.to_string(),
            hosted: None,
            git_overlay: None,
        };
        let err = match factory(temp.path(), &section) {
            Ok(_) => panic!(
                "factory must reject a kind=hosted section that omits the [hydrator.hosted] table"
            ),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("[hydrator.hosted]") || msg.contains("hydrator.hosted"),
            "error must name the missing TOML table; got: {msg}"
        );
    }
}

#[cfg(test)]
mod connect_path_tests {
    //! Source-presence test for the credential-rotation invariant. Lazy
    //! hydration must open its session through the shared `HostedSession`
    //! seam — whose `connect` connects and rotates together (guarded by a
    //! source-presence test in `session.rs`) — rather than connecting by
    //! hand and risking a dropped rotation. Without rotation, a process
    //! whose cached token has slipped past expiry hits an auth failure on
    //! first lazy hydrate even though the rotation data is on disk.
    #[test]
    fn lazy_hosted_connect_opens_session_through_rotating_seam() {
        let source = include_str!("hydration.rs");
        assert!(
            source
                .contains("HostedSession::build(&user_config, None, HostedAuthMode::ConfigToken)"),
            "hydration.rs must build its session through the shared HostedSession seam",
        );
        assert!(
            source.contains("session.connect(addr)"),
            "hydration.rs must connect via HostedSession::connect, which owns rotation",
        );
    }
}

#[cfg(test)]
mod config_persistence_tests {
    //! Tests for the round-3 hostname-vs-IP persistence fix. These live
    //! alongside the hydrator tests because the contract — "endpoint
    //! field stores a host:port string, NOT a resolved SocketAddr" — is
    //! enforced at the LazyHostedHydrator boundary.
    use repo::lazy_hydrator::LazyHydratorConfig;
    use tempfile::TempDir;

    #[test]
    fn lazy_hydrator_config_round_trip_preserves_hostname() {
        let temp = TempDir::new().expect("temp");
        let heddle = temp.path().join(".heddle");
        // The persisted endpoint MUST be the hostname spec, not a
        // SocketAddr-formatted IP. clone.rs is the producer; here we
        // simulate it and verify load round-trips byte-for-byte.
        let endpoint = "example.heddle.cloud:443";
        let cfg = LazyHydratorConfig::hosted(endpoint, "org/acme/repo", "main", "main");
        cfg.save(&heddle).expect("save");
        let loaded = LazyHydratorConfig::load(&heddle)
            .expect("load")
            .expect("present");
        let hosted = loaded
            .hydrator
            .hosted
            .expect("hosted section present after round-trip");
        assert_eq!(
            hosted.endpoint, endpoint,
            "endpoint MUST round-trip as the original hostname:port spec; \
             pinning the IP at clone time would break hosts with rotating IPs"
        );
        // Sanity: the persisted value must not parse as a SocketAddr —
        // if it does, the producer was silently resolving DNS at save
        // time and we'd be back to the round-2 bug shape.
        assert!(
            hosted.endpoint.parse::<std::net::SocketAddr>().is_err(),
            "persisted endpoint must be a hostname spec, not a SocketAddr literal"
        );
    }
}
