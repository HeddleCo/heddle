//! Command-scoped stable identities. Only explicit resource/authority failures
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
        client::{MessageReader, RpcTransport},
    },
};
use thread_api::transport::Error;

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
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn observe_error<T>(&self, result: &Result<T, Error>) {
        if let Err(Error::Remote(failure)) = result
            && matches!(
                CallFailureCode::try_from(failure.code),
                Ok(CallFailureCode::NotFound
                    | CallFailureCode::PermissionDenied
                    | CallFailureCode::Unauthenticated)
            )
        {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Keep invalidation at the native RPC boundary, including terminal stream
/// failures, so callers cannot accidentally retain revoked name resolutions.
pub(super) struct ResolutionTransport<T> {
    inner: T,
    cache: Arc<ResolutionCache>,
}

impl<T> ResolutionTransport<T> {
    pub fn new(inner: T, cache: Arc<ResolutionCache>) -> Self {
        Self { inner, cache }
    }
}

pub(super) struct ResolutionReader<R> {
    inner: R,
    cache: Arc<ResolutionCache>,
}
impl<R: MessageReader<Error = Error>> MessageReader for ResolutionReader<R> {
    type Error = Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let result = self.inner.next().await;
        self.cache.observe_error(&result);
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
        let result = self.inner.unary(method, request).await;
        self.cache.observe_error(&result);
        result
    }
    async fn observe(
        &self,
        method: &'static MethodDescriptor,
        request: Vec<u8>,
    ) -> Result<Self::Reader, Error> {
        let result = self.inner.observe(method, request).await;
        self.cache.observe_error(&result);
        Ok(ResolutionReader {
            inner: result?,
            cache: self.cache.clone(),
        })
    }
    async fn exchange(
        &self,
        method: &'static MethodDescriptor,
        opening: Vec<u8>,
    ) -> Result<(Self::Writer, Self::Reader), Error> {
        let result = self.inner.exchange(method, opening).await;
        self.cache.observe_error(&result);
        let (writer, reader) = result?;
        Ok((
            writer,
            ResolutionReader {
                inner: reader,
                cache: self.cache.clone(),
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
                cache: cache.clone(),
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
            cache.observe_error::<()>(&Err(error));
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
        };
        let (client, server, captured) = start_with_thread_listing(fixture).await;
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
            };
            let (client, server, _) = start_with_thread_listing(fixture).await;
            client
                .resolve_spool_ref("acme/widgets")
                .await
                .expect("spool");
            assert!(!client.resolutions.entries().await.spools.is_empty());
            assert!(
                client
                    .resolve_thread_ref("acme/widgets", "main")
                    .await
                    .is_err()
            );
            assert_eq!(
                client.resolutions.entries().await.spools.is_empty(),
                code != CallFailureCode::Unavailable,
                "{code:?}"
            );
            client.close().await;
            server.await.expect("server");
        }
    }
}
