//! Opt-in, command-scoped stable identities. Only
//! [`HostedClient::with_command_resolution_cache`](super::HostedClient::with_command_resolution_cache)
//! creates a cache, and it lives as long as that client and its clones, so only
//! short-lived commands (clone) may opt in. Long-lived clients (lazy hydration,
//! discussion live) resolve afresh every time. Only explicit resource/authority
//! failures of resolution calls, or of calls that carried a cached identity,
//! discard successful resolutions; transport failures never expire them.
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use api::{
    heddle::api::{
        common::CallFailureCode,
        v1alpha2::{SpoolRef, ThreadRef},
    },
    v2::{
        MethodDescriptor,
        client::{MessageReader, Rpc, RpcTransport},
    },
};
use thread_api::{rpc, transport::Error};

#[derive(Default)]
pub(super) struct ResolutionCache {
    generation: AtomicU64,
    entries: tokio::sync::Mutex<Entries>,
}

#[derive(Default)]
pub(super) struct Entries {
    generation: u64,
    pub spools: HashMap<String, SpoolRef>,
    pub threads: HashMap<(String, String), ThreadRef>,
}

impl ResolutionCache {
    pub async fn entries(&self) -> tokio::sync::MutexGuard<'_, Entries> {
        let mut entries = self.entries.lock().await;
        let generation = self.generation.load(Ordering::Acquire);
        if entries.generation != generation {
            entries.spools.clear();
            entries.threads.clear();
            entries.generation = generation;
        }
        entries
    }

    pub fn invalidate_protocol_error(&self, error: &wire::ProtocolError) {
        if matches!(
            error,
            wire::ProtocolError::ObjectNotFound(_)
                | wire::ProtocolError::AuthorizationFailed(_)
                | wire::ProtocolError::AuthenticationFailed(_)
        ) {
            self.invalidate();
        }
    }

    fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether a call resolves names or carries a cached identity: a cached
    /// address, or a cached Spool id (Thread refs always embed their Spool
    /// id). Judged when the call is sent, because that is when the cache was
    /// consulted. The lock is held for the whole of a first resolution,
    /// including endpoint discovery on the same task, so a contended lock
    /// cannot be inspected: report it as used, which only costs one extra
    /// resolution.
    fn call_uses_resolutions(&self, method: &'static MethodDescriptor, request: &[u8]) -> bool {
        if method.path == rpc::WorkspaceServiceResolveResources::METHOD.path {
            return true;
        }
        let Ok(entries) = self.entries.try_lock() else {
            return true;
        };
        entries.spools.iter().any(|(address, spool)| {
            contains(request, address.as_bytes()) || contains(request, spool.id.as_bytes())
        }) || entries.threads.keys().any(|(spool, name)| {
            contains(request, spool.as_bytes()) && contains(request, name.as_bytes())
        })
    }

    fn observe_error<T>(&self, used_resolutions: bool, result: &Result<T, Error>) {
        if used_resolutions
            && let Err(Error::Remote(failure)) = result
            && matches!(
                CallFailureCode::try_from(failure.code),
                Ok(CallFailureCode::NotFound
                    | CallFailureCode::PermissionDenied
                    | CallFailureCode::Unauthenticated)
            )
        {
            self.invalidate();
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Keep invalidation at the native RPC boundary, including terminal stream
/// failures, so callers cannot accidentally retain revoked name resolutions.
/// Without a cache this is a pass-through.
pub(super) struct ResolutionTransport<T> {
    inner: T,
    cache: Option<Arc<ResolutionCache>>,
}

impl<T> ResolutionTransport<T> {
    pub fn new(inner: T, cache: Option<Arc<ResolutionCache>>) -> Self {
        Self { inner, cache }
    }

    fn scope(
        &self,
        method: &'static MethodDescriptor,
        request: &[u8],
    ) -> Option<(Arc<ResolutionCache>, bool)> {
        let cache = self.cache.clone()?;
        let used = cache.call_uses_resolutions(method, request);
        Some((cache, used))
    }
}

fn observe_error<T>(scope: &Option<(Arc<ResolutionCache>, bool)>, result: &Result<T, Error>) {
    if let Some((cache, used)) = scope {
        cache.observe_error(*used, result);
    }
}

pub(super) struct ResolutionReader<R> {
    inner: R,
    scope: Option<(Arc<ResolutionCache>, bool)>,
}
impl<R: MessageReader<Error = Error>> MessageReader for ResolutionReader<R> {
    type Error = Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let result = self.inner.next().await;
        observe_error(&self.scope, &result);
        result
    }
    fn cancel(&mut self) {
        self.inner.cancel();
    }
}

impl<T: RpcTransport<Error = Error>> RpcTransport for ResolutionTransport<T> {
    type Error = Error;
    type Reader = ResolutionReader<T::Reader>;
    type Writer = T::Writer;
    async fn unary(
        &self,
        method: &'static MethodDescriptor,
        request: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        let scope = self.scope(method, &request);
        let result = self.inner.unary(method, request).await;
        observe_error(&scope, &result);
        result
    }
    async fn observe(
        &self,
        method: &'static MethodDescriptor,
        request: Vec<u8>,
    ) -> Result<Self::Reader, Error> {
        let scope = self.scope(method, &request);
        let result = self.inner.observe(method, request).await;
        observe_error(&scope, &result);
        Ok(ResolutionReader {
            inner: result?,
            scope,
        })
    }
    async fn exchange(
        &self,
        method: &'static MethodDescriptor,
        opening: Vec<u8>,
    ) -> Result<(Self::Writer, Self::Reader), Error> {
        let scope = self.scope(method, &opening);
        let result = self.inner.exchange(method, opening).await;
        observe_error(&scope, &result);
        let (writer, reader) = result?;
        Ok((
            writer,
            ResolutionReader {
                inner: reader,
                scope,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use api::heddle::api::common::CallFailure;

    use super::*;

    struct FailureReader(Option<Error>);
    impl MessageReader for FailureReader {
        type Error = Error;
        async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
            match self.0.take() {
                Some(error) => Err(error),
                None => Ok(None),
            }
        }
        fn cancel(&mut self) {}
    }

    #[tokio::test]
    async fn terminal_resource_and_authority_errors_invalidate_resolutions() {
        let _process_env_guard = crate::test_process_env::shared().await;
        for code in [
            CallFailureCode::NotFound,
            CallFailureCode::PermissionDenied,
            CallFailureCode::Unauthenticated,
        ] {
            let cache = Arc::new(ResolutionCache::default());
            cache.entries().await.spools.insert(
                "acme/widgets".into(),
                SpoolRef {
                    id: uuid::Uuid::now_v7().to_string(),
                },
            );
            let mut reader = ResolutionReader {
                inner: FailureReader(Some(Error::Remote(
                    CallFailure {
                        code: code as i32,
                        message: "resource revoked".into(),
                        error: None,
                    }
                    .into(),
                ))),
                scope: Some((cache.clone(), true)),
            };
            assert!(reader.next().await.is_err());
            assert!(cache.entries().await.spools.is_empty(), "{code:?}");
        }
    }

    #[tokio::test]
    async fn transient_failures_do_not_expire_resolutions() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let cache = ResolutionCache::default();
        let spool = SpoolRef {
            id: uuid::Uuid::now_v7().to_string(),
        };
        cache
            .entries()
            .await
            .spools
            .insert("acme/widgets".into(), spool.clone());
        for error in [
            Error::Timeout,
            Error::Io("connection reset".into()),
            Error::Remote(
                CallFailure {
                    code: CallFailureCode::Unavailable as i32,
                    message: "overload".into(),
                    error: None,
                }
                .into(),
            ),
        ] {
            cache.observe_error::<()>(true, &Err(error));
            assert_eq!(
                cache.entries().await.spools.get("acme/widgets"),
                Some(&spool)
            );
        }
    }

    #[tokio::test]
    async fn concurrent_handles_resolve_thread_names_once_per_session() {
        use super::super::test_server::{ThreadListingFixture, start_with_thread_listing};
        let _process_env_guard = crate::test_process_env::shared().await;
        let fixture = ThreadListingFixture {
            overviews: vec![api::heddle::api::v1alpha2::ThreadOverview {
                name: "main".into(),
                r#ref: Some(ThreadRef {
                    spool: Some(SpoolRef {
                        id: uuid::Uuid::from_bytes([2; 16]).to_string(),
                    }),
                    id: Some(api::heddle::api::v1alpha2::ThreadId { value: vec![4; 32] }),
                }),
                ..Default::default()
            }],
            page_size: 64,
            requests: Arc::default(),
            resolution_failure: None,
            resolution_requests: Arc::default(),
            resolved_thread_byte: Arc::default(),
        };
        let (client, server, captured) = start_with_thread_listing(fixture).await;
        let client = client.with_command_resolution_cache();
        let other = client.clone();
        let (first, second) = tokio::join!(
            client.resolve_thread_ref("acme/widgets", "main"),
            other.resolve_thread_ref("acme/widgets", "main"),
        );
        assert_eq!(
            first.expect("first resolution"),
            second.expect("shared resolution")
        );
        client
            .resolve_thread_ref("acme/widgets", "main")
            .await
            .expect("reuse");
        assert_eq!(
            captured.resolution_requests.lock().expect("requests").len(),
            1
        );
        client.close().await;
        server.await.expect("server");
    }
    #[tokio::test]
    async fn unary_resolution_failure_invalidates_only_explicit_resource_errors() {
        use super::super::test_server::{ThreadListingFixture, start_with_thread_listing};
        let _process_env_guard = crate::test_process_env::shared().await;
        for code in [
            CallFailureCode::NotFound,
            CallFailureCode::PermissionDenied,
            CallFailureCode::Unauthenticated,
            CallFailureCode::Unavailable,
        ] {
            let fixture = ThreadListingFixture {
                overviews: Vec::new(),
                page_size: 64,
                requests: Arc::default(),
                resolution_failure: Some(code),
                resolution_requests: Arc::default(),
                resolved_thread_byte: Arc::default(),
            };
            let (client, server, _) = start_with_thread_listing(fixture).await;
            let client = client.with_command_resolution_cache();
            let cache = client.resolutions.clone().expect("command cache");
            client
                .resolve_spool_ref("acme/widgets")
                .await
                .expect("spool");
            assert!(!cache.entries().await.spools.is_empty());
            assert!(
                client
                    .resolve_thread_ref("acme/widgets", "main")
                    .await
                    .is_err()
            );
            assert_eq!(
                cache.entries().await.spools.is_empty(),
                code != CallFailureCode::Unavailable,
                "{code:?}"
            );
            client.close().await;
            server.await.expect("server");
        }
    }

    struct NoWriter;
    impl api::v2::client::MessageWriter for NoWriter {
        type Error = Error;
        async fn send(&mut self, _: Vec<u8>) -> Result<(), Error> {
            Ok(())
        }
        async fn finish(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn abort(&mut self) {}
    }

    /// Every call fails with the same remote code.
    struct Failing(CallFailureCode);
    impl Failing {
        fn error(&self) -> Error {
            Error::Remote(
                CallFailure {
                    code: self.0 as i32,
                    message: "gone".into(),
                    error: None,
                }
                .into(),
            )
        }
    }
    impl RpcTransport for Failing {
        type Error = Error;
        type Reader = FailureReader;
        type Writer = NoWriter;
        async fn unary(&self, _: &'static MethodDescriptor, _: Vec<u8>) -> Result<Vec<u8>, Error> {
            Err(self.error())
        }
        async fn observe(
            &self,
            _: &'static MethodDescriptor,
            _: Vec<u8>,
        ) -> Result<Self::Reader, Error> {
            Ok(FailureReader(Some(self.error())))
        }
        async fn exchange(
            &self,
            _: &'static MethodDescriptor,
            _: Vec<u8>,
        ) -> Result<(Self::Writer, Self::Reader), Error> {
            Err(self.error())
        }
    }

    async fn seeded(spool: &SpoolRef) -> Arc<ResolutionCache> {
        let cache = Arc::new(ResolutionCache::default());
        cache
            .entries()
            .await
            .spools
            .insert("acme/widgets".into(), spool.clone());
        cache
    }

    #[tokio::test]
    async fn only_resolution_scoped_failures_clear_resolutions() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let spool = SpoolRef {
            id: uuid::Uuid::now_v7().to_string(),
        };
        let unrelated = rpc::ThreadServiceReviseIntent::METHOD;
        let resolve = rpc::WorkspaceServiceResolveResources::METHOD;
        let other_spool = uuid::Uuid::now_v7().to_string();
        for code in [
            CallFailureCode::NotFound,
            CallFailureCode::PermissionDenied,
            CallFailureCode::Unauthenticated,
        ] {
            let cache = seeded(&spool).await;
            let transport = ResolutionTransport::new(Failing(code), Some(cache.clone()));

            // A NotFound for another Spool is unrelated to anything cached,
            // whether it ends a unary call or a stream.
            assert!(
                transport
                    .unary(unrelated, other_spool.clone().into_bytes())
                    .await
                    .is_err()
            );
            let mut stream = transport
                .observe(unrelated, other_spool.clone().into_bytes())
                .await
                .expect("stream opens");
            assert!(stream.next().await.is_err());
            assert!(
                transport
                    .exchange(unrelated, other_spool.clone().into_bytes())
                    .await
                    .is_err()
            );
            assert_eq!(
                cache.entries().await.spools.get("acme/widgets"),
                Some(&spool),
                "unrelated {code:?} must keep the cache"
            );

            // A call that carried the cached Spool id clears it, as does a
            // terminal stream failure of such a call.
            assert!(
                transport
                    .unary(unrelated, spool.id.clone().into_bytes())
                    .await
                    .is_err()
            );
            assert!(cache.entries().await.spools.is_empty(), "{code:?} used");
            let cache = seeded(&spool).await;
            let transport = ResolutionTransport::new(Failing(code), Some(cache.clone()));
            let mut stream = transport
                .observe(unrelated, spool.id.clone().into_bytes())
                .await
                .expect("stream opens");
            assert!(stream.next().await.is_err());
            assert!(cache.entries().await.spools.is_empty(), "{code:?} stream");

            // A failed resolution always clears.
            let cache = seeded(&spool).await;
            let transport = ResolutionTransport::new(Failing(code), Some(cache.clone()));
            assert!(transport.unary(resolve, Vec::new()).await.is_err());
            assert!(cache.entries().await.spools.is_empty(), "{code:?} resolve");
        }
    }

    #[tokio::test]
    async fn clients_resolve_afresh_unless_the_command_opts_in() {
        use super::super::test_server::{ThreadListingFixture, start_with_thread_listing};
        let _process_env_guard = crate::test_process_env::shared().await;
        let fixture = ThreadListingFixture {
            overviews: Vec::new(),
            page_size: 64,
            requests: Arc::default(),
            resolution_failure: None,
            resolution_requests: Arc::default(),
            resolved_thread_byte: Arc::default(),
        };
        let (client, server, captured) = start_with_thread_listing(fixture).await;
        assert!(client.resolutions.is_none());
        for byte in [3_u8, 9] {
            *captured.resolved_thread_byte.lock().expect("resolved byte") = Some(byte);
            let thread = client
                .resolve_thread_ref("acme/widgets", "main")
                .await
                .expect("thread");
            assert_eq!(thread.id.expect("id").value, vec![byte; 32]);
        }
        client.close().await;
        server.await.expect("server");
    }
}
