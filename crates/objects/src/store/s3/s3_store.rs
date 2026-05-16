// SPDX-License-Identifier: Apache-2.0
//! S3 storage implementation.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, OnceLock, mpsc},
    thread,
};

use aws_sdk_s3::{Client, config::BehaviorVersion};
use aws_smithy_async::rt::sleep::TokioSleep;

/// S3-compatible object storage backend.
///
/// Stores Heddle objects (blobs, trees, states, actions) in an S3 bucket.
/// Objects are content-addressed using their hashes as keys.
#[derive(Clone)]
pub struct S3Store {
    pub(super) client: Arc<Client>,
    pub(super) bucket: String,
    pub(super) prefix: String,
    /// Lazy worker-thread + Tokio runtime that drives async S3 calls off
    /// the caller's runtime. See [`RuntimeBridge`]. Wrapped in
    /// `Arc<OnceLock<_>>` so every clone of `S3Store` shares one bridge
    /// (one worker thread) and the spawn cost is paid on first sync use.
    pub(super) bridge: Arc<OnceLock<RuntimeBridge>>,
}

impl S3Store {
    /// Create a new S3 store with the given client, bucket, and prefix.
    pub fn new(client: Client, bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            client: Arc::new(client),
            bucket: bucket.into(),
            prefix: prefix.into(),
            bridge: Arc::new(OnceLock::new()),
        }
    }

    /// Create a builder for configuring S3 storage.
    pub fn builder() -> S3StoreBuilder {
        S3StoreBuilder::new()
    }

    /// Get the S3 key for a blob.
    pub(super) fn blob_key(&self, hash: &crate::object::ContentHash) -> String {
        format!("{}blobs/{}.bin", self.prefix, hash.to_hex())
    }

    /// Get the S3 key for a tree.
    pub(super) fn tree_key(&self, hash: &crate::object::ContentHash) -> String {
        format!("{}trees/{}.bin", self.prefix, hash.to_hex())
    }

    /// Get the S3 key for a state.
    pub(super) fn state_key(&self, id: &crate::object::ChangeId) -> String {
        format!("{}states/{}.bin", self.prefix, id.to_string_full())
    }

    /// Get the S3 key for an action.
    pub(super) fn action_key(&self, id: &crate::object::ActionId) -> String {
        format!("{}actions/{}.bin", self.prefix, id)
    }

    /// Lazily-initialized accessor for the runtime bridge.
    ///
    /// The synchronous `ObjectStore` methods route every `.send().await`
    /// call through this bridge so they can be invoked from inside a
    /// caller's Tokio runtime (`#[tokio::main]`, `#[tokio::test]`, a
    /// daemon worker, etc.) without the nested-`block_on` panic that
    /// `Handle::try_current().block_on(...)` triggers. See
    /// [`RuntimeBridge`] for the design rationale.
    pub(super) fn bridge(&self) -> crate::store::Result<&RuntimeBridge> {
        if let Some(bridge) = self.bridge.get() {
            return Ok(bridge);
        }
        let new = RuntimeBridge::new().map_err(|err| {
            crate::store::StoreError::Io(io::Error::other(format!(
                "S3 runtime bridge: spawn worker thread: {err}",
            )))
        })?;
        // If a concurrent caller already populated the slot, `set` drops
        // our worker; its tx side dies with it and the spawned thread
        // exits cleanly when `rx.recv()` returns Err. First-use only, so
        // the wasted spawn is acceptable in exchange for keeping
        // `bridge()` lock-free on the hot path.
        let _ = self.bridge.set(new);
        Ok(self
            .bridge
            .get()
            .expect("OnceLock populated above or by a concurrent caller"))
    }

    /// List objects with a given prefix.
    pub(super) async fn list_with_prefix(&self, prefix: &str) -> crate::store::Result<Vec<String>> {
        let full_prefix = format!("{}{}", self.prefix, prefix);

        let mut keys = Vec::new();
        let mut continuation_token = None;

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&full_prefix);

            if let Some(token) = continuation_token {
                request = request.continuation_token(token);
            }

            let response = request.send().await.map_err(|e| {
                crate::store::StoreError::Io(std::io::Error::other(format!(
                    "S3 list_objects_v2 failed: {}",
                    e
                )))
            })?;

            if let Some(contents) = response.contents {
                for obj in contents {
                    if let Some(key) = obj.key {
                        // Strip the prefix from the key
                        if let Some(stripped) = key.strip_prefix(&self.prefix) {
                            keys.push(stripped.to_string());
                        }
                    }
                }
            }

            if response.is_truncated.unwrap_or(false) {
                continuation_token = response.next_continuation_token;
            } else {
                break;
            }
        }

        Ok(keys)
    }
}

/// Builder for configuring S3 storage.
pub struct S3StoreBuilder {
    pub(super) bucket: Option<String>,
    pub(super) region: Option<String>,
    pub(super) prefix: String,
    pub(super) endpoint_url: Option<String>,
    pub(super) access_key_id: Option<String>,
    pub(super) secret_access_key: Option<String>,
    pub(super) session_token: Option<String>,
    /// Use path-style bucket addressing (`endpoint/bucket/key`) instead of
    /// virtual-hosted style (`bucket.endpoint/key`). Required for MinIO and
    /// most non-AWS S3-compatible services.
    pub(super) force_path_style: bool,
}

impl S3StoreBuilder {
    /// Create a new S3 store builder.
    pub fn new() -> Self {
        Self {
            bucket: None,
            region: None,
            prefix: String::new(),
            endpoint_url: None,
            access_key_id: None,
            secret_access_key: None,
            session_token: None,
            force_path_style: false,
        }
    }

    /// Set the S3 bucket name.
    pub fn bucket(mut self, bucket: impl Into<String>) -> Self {
        self.bucket = Some(bucket.into());
        self
    }

    /// Set the AWS region.
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Set a prefix for all objects in the bucket.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        // Ensure prefix ends with a slash
        self.prefix = if prefix.is_empty() || prefix.ends_with('/') {
            prefix
        } else {
            format!("{}/", prefix)
        };
        self
    }

    /// Set a custom endpoint URL (for MinIO, etc.).
    pub fn endpoint_url(mut self, url: impl Into<String>) -> Self {
        self.endpoint_url = Some(url.into());
        self
    }

    /// Set the AWS access key ID.
    pub fn access_key_id(mut self, key: impl Into<String>) -> Self {
        self.access_key_id = Some(key.into());
        self
    }

    /// Set the AWS secret access key.
    pub fn secret_access_key(mut self, key: impl Into<String>) -> Self {
        self.secret_access_key = Some(key.into());
        self
    }

    /// Set the AWS session token (for temporary credentials).
    pub fn session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }

    /// Use path-style bucket addressing (`endpoint/bucket/key`).
    ///
    /// Required for MinIO, Ceph RGW, and other non-AWS S3-compatible services
    /// that do not support virtual-hosted–style addressing. Has no effect when
    /// no `endpoint_url` is set.
    pub fn force_path_style(mut self, enable: bool) -> Self {
        self.force_path_style = enable;
        self
    }

    /// Build the S3 store synchronously.
    ///
    /// Equivalent to [`Self::build`] but callable from a sync context — even
    /// from inside a caller's Tokio runtime, where `Handle::block_on(self.build())`
    /// would panic with "Cannot start a runtime from within a runtime".
    ///
    /// Routes the async `build()` future through a short-lived
    /// [`RuntimeBridge`] worker thread (its own private current-thread
    /// runtime), so the caller's runtime is never re-entered. The bridge
    /// is dropped on return; the resulting [`S3Store`] carries its own
    /// lazy `OnceLock<RuntimeBridge>` for subsequent sync `ObjectStore`
    /// calls, so no worker thread lingers beyond the build phase.
    ///
    /// `Repository::open` uses this entry point to construct an S3-backed
    /// store from sync code that may itself be running on a Tokio
    /// runtime (`#[tokio::main]`, `#[tokio::test]`, a daemon worker).
    pub fn build_blocking(self) -> crate::store::Result<S3Store> {
        let bridge = RuntimeBridge::new().map_err(|err| {
            crate::store::StoreError::Config(format!(
                "S3 store: spawn worker thread for builder: {err}"
            ))
        })?;
        bridge.block_on(self.build())
    }

    /// Build the S3 store.
    pub async fn build(self) -> crate::store::Result<S3Store> {
        let bucket = self.bucket.ok_or_else(|| {
            crate::store::StoreError::Config("S3 bucket name is required".to_string())
        })?;

        let (Some(access_key_id), Some(secret_access_key)) =
            (self.access_key_id, self.secret_access_key)
        else {
            return Err(crate::store::StoreError::Config(
                "S3 access_key_id and secret_access_key are required (set them in the \
                 server config file or via HEDDLE_SERVER_S3_ACCESS_KEY_ID / \
                 HEDDLE_SERVER_S3_SECRET_ACCESS_KEY, or AWS_ACCESS_KEY_ID / \
                 AWS_SECRET_ACCESS_KEY env vars)"
                    .to_string(),
            ));
        };

        let credentials = aws_sdk_s3::config::Credentials::new(
            access_key_id,
            secret_access_key,
            self.session_token,
            None,
            "heddle-s3-store",
        );
        let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(credentials)
            .sleep_impl(TokioSleep::new());
        if let Some(region) = self.region {
            s3_config_builder = s3_config_builder.region(aws_sdk_s3::config::Region::new(region));
        }
        if let Some(url) = self.endpoint_url {
            s3_config_builder = s3_config_builder.endpoint_url(url);
        }

        if self.force_path_style {
            s3_config_builder = s3_config_builder.force_path_style(true);
        }

        let client = Client::from_conf(s3_config_builder.build());

        // Verify the bucket is accessible before handing out the store.
        client
            .head_bucket()
            .bucket(&bucket)
            .send()
            .await
            .map_err(|e| {
                crate::store::StoreError::Config(format!(
                    "Failed to access S3 bucket '{}': {}",
                    bucket, e
                ))
            })?;

        Ok(S3Store::new(client, bucket, self.prefix))
    }
}

impl Default for S3StoreBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Worker thread + private current-thread Tokio runtime used to drive the
/// async `aws_sdk_s3` client off the caller's runtime.
///
/// ## Why
///
/// Every method on the [`crate::ObjectStore`] impl for [`S3Store`] is
/// synchronous, but the AWS SDK is async. The previous design called
/// `tokio::runtime::Handle::try_current().block_on(...)` from inside each
/// method. When the caller was itself running on a Tokio runtime
/// (`#[tokio::main]`, `#[tokio::test]`, a daemon worker task), that
/// `block_on` panicked with `"Cannot start a runtime from within a
/// runtime"` because nesting `block_on` on a runtime you are already
/// inside is disallowed.
///
/// This bridge owns its own current-thread Tokio runtime on a dedicated
/// worker thread. Synchronous calls hand each request to the worker over
/// a channel and block on a reply channel; the worker drives the future
/// inside its private runtime. The caller's runtime (if any) is never
/// re-entered, so no nesting occurs.
///
/// The pattern mirrors
/// [`client::grpc_hosted::hydration::LazyHostedHydrator`] (issue #50),
/// which solved the same shape for the sync `BlobHydrator` trait.
///
/// ## Shutdown
///
/// Dropping the bridge drops the `Sender`; the worker's `Receiver::recv`
/// then returns `Err` and the worker exits and the runtime is dropped.
/// The `JoinHandle` is retained as `_worker` so the thread is tied to
/// the bridge's lifetime and isn't reaped before its requests drain.
pub(super) struct RuntimeBridge {
    tx: mpsc::Sender<BridgedTask>,
    _worker: thread::JoinHandle<()>,
}

type BridgedTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

impl RuntimeBridge {
    fn new() -> io::Result<Self> {
        let (tx, rx) = mpsc::channel::<BridgedTask>();
        let worker = thread::Builder::new()
            .name("heddle-s3-bridge".into())
            .spawn(move || {
                // current_thread is sufficient: every request is serialized
                // through the mpsc channel anyway, and an idle store costs
                // exactly one thread + one runtime instead of one per
                // worker-pool slot.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build heddle-s3-bridge worker runtime");
                runtime.block_on(async move {
                    // Blocking `rx.recv()` parks the runtime between
                    // requests. Acceptable: nothing else lives on this
                    // runtime, so there are no other tasks to starve.
                    while let Ok(task) = rx.recv() {
                        task.await;
                    }
                });
            })?;
        Ok(Self {
            tx,
            _worker: worker,
        })
    }

    /// Run `future` on the worker's runtime and block the caller until it
    /// completes, returning the future's output.
    ///
    /// `future` must be `Send + 'static`; call sites compose it from
    /// `Arc<Client>` clones plus owned `String` keys / serialized bodies,
    /// never borrows of `&self`.
    pub(super) fn block_on<F, T>(&self, future: F) -> T
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // Capacity 1: each call sends exactly one value and then drops.
        let (reply_tx, reply_rx) = mpsc::sync_channel::<T>(1);
        let task: BridgedTask = Box::pin(async move {
            let value = future.await;
            // The receiver is held on the caller's stack until `recv`
            // returns, so `send` is infallible in practice.
            let _ = reply_tx.send(value);
        });
        self.tx
            .send(task)
            .expect("heddle-s3-bridge worker thread terminated unexpectedly");
        reply_rx
            .recv()
            .expect("heddle-s3-bridge worker dropped reply channel without sending")
    }
}
