// SPDX-License-Identifier: Apache-2.0
//! Tests for the S3 storage backend.
//!
//! Unit tests (builder fields, key generation) always run with no external
//! dependencies.
//!
//! The compliance test starts an embedded [`s3s_fs`]-backed S3 server on a
//! random local port — no external MinIO or AWS account required.

#[cfg(test)]
mod tests {
    use aws_sdk_s3::{config::BehaviorVersion, primitives::ByteStream};
    use aws_smithy_async::rt::sleep::TokioSleep;
    use chrono::{TimeZone, Utc};

    use crate::{
        object::{Action, Attribution, ChangeId, ContentHash, Operation, Principal, State},
        store::{ObjectStore, StoreError, s3::s3_store::S3Store},
    };

    // ── Unit tests ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_builder_fields() {
        let builder = S3Store::builder()
            .bucket("test-bucket")
            .region("us-east-1")
            .prefix("test-prefix/")
            .force_path_style(true);

        assert_eq!(builder.bucket, Some("test-bucket".to_string()));
        assert_eq!(builder.region, Some("us-east-1".to_string()));
        assert_eq!(builder.prefix, "test-prefix/".to_string());
        assert!(builder.force_path_style);
    }

    #[tokio::test]
    async fn test_prefix_slash_normalization() {
        let b = S3Store::builder().prefix("my-prefix");
        assert_eq!(b.prefix, "my-prefix/");

        let b = S3Store::builder().prefix("already/");
        assert_eq!(b.prefix, "already/");

        let b = S3Store::builder().prefix("");
        assert_eq!(b.prefix, "");
    }

    #[tokio::test]
    async fn test_key_generation() {
        let client = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .sleep_impl(TokioSleep::new())
                .build(),
        );
        let store = S3Store::new(client, "test-bucket", "prefix/");

        let hash = ContentHash::compute(b"key generation test");
        let hex = hash.to_hex();

        assert_eq!(store.blob_key(&hash), format!("prefix/blobs/{hex}.bin"));
        assert_eq!(store.tree_key(&hash), format!("prefix/trees/{hex}.bin"));

        let id = ChangeId::generate();
        let state_key = store.state_key(&id);
        assert!(
            state_key.starts_with("prefix/states/"),
            "state key prefix wrong: {state_key}"
        );
        assert!(
            state_key.ends_with(".bin"),
            "state key suffix wrong: {state_key}"
        );
    }

    // ── Compliance test ───────────────────────────────────────────────────────

    /// Starts an embedded S3 server, builds an S3Store against it, and runs
    /// the full ObjectStore compliance suite — no external service required.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_s3_store_compliance() {
        let (endpoint, bucket, _tmp) = start_local_s3().await;

        let (_client, store) = build_test_store(&endpoint, &bucket).await;

        // S3Store::ObjectStore methods use Handle::block_on internally and
        // must run from a blocking thread, not directly inside an async task.
        tokio::task::spawn_blocking(move || {
            crate::store::store_compliance::run_compliance_tests(&store);
        })
        .await
        .expect("S3 compliance tests panicked");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_state_rejects_wrong_object_swap() {
        let (endpoint, bucket, _tmp) = start_local_s3().await;
        let (client, store) = build_test_store(&endpoint, &bucket).await;

        let tree_hash = ContentHash::compute(b"tree");
        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        let state1 = State::new(tree_hash, vec![], attribution.clone());
        let state2 = State::new(tree_hash, vec![], attribution);

        tokio::task::spawn_blocking({
            let store = store.clone();
            let state1 = state1.clone();
            let state2 = state2.clone();
            move || {
                store.put_state(&state1).unwrap();
                store.put_state(&state2).unwrap();
            }
        })
        .await
        .unwrap();

        client
            .put_object()
            .bucket(&bucket)
            .key(store.state_key(&state1.change_id))
            .body(ByteStream::from(rmp_serde::to_vec(&state2).unwrap()))
            .send()
            .await
            .unwrap();

        let error =
            tokio::task::spawn_blocking(move || store.get_state(&state1.change_id).unwrap_err())
                .await
                .unwrap();
        assert!(
            matches!(error, StoreError::InvalidObject(message) if message.contains("state change_id mismatch"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_action_rejects_wrong_object_swap() {
        let (endpoint, bucket, _tmp) = start_local_s3().await;
        let (client, store) = build_test_store(&endpoint, &bucket).await;

        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        let action1 = Action::new(
            None,
            ChangeId::generate(),
            Operation::Snapshot,
            "first action",
            attribution.clone(),
        )
        .with_timestamp(Utc.timestamp_opt(1_700_000_000, 0).unwrap());
        let action2 = Action::new(
            None,
            ChangeId::generate(),
            Operation::Snapshot,
            "second action",
            attribution,
        )
        .with_timestamp(Utc.timestamp_opt(1_700_000_001, 0).unwrap());

        let action1_id = tokio::task::spawn_blocking({
            let store = store.clone();
            let mut action1 = action1.clone();
            let mut action2 = action2.clone();
            move || {
                let action1_id = store.put_action(&mut action1).unwrap();
                store.put_action(&mut action2).unwrap();
                action1_id
            }
        })
        .await
        .unwrap();

        client
            .put_object()
            .bucket(&bucket)
            .key(store.action_key(&action1_id))
            .body(ByteStream::from(rmp_serde::to_vec(&action2).unwrap()))
            .send()
            .await
            .unwrap();

        let error = tokio::task::spawn_blocking(move || store.get_action(&action1_id).unwrap_err())
            .await
            .unwrap();
        assert!(
            matches!(error, StoreError::InvalidObject(message) if message.contains("action id mismatch"))
        );
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Start an embedded [`s3s_fs`] S3 server on a random local port.
    ///
    /// Returns `(endpoint_url, bucket_name, tmp_dir)`. The server lives as
    /// long as the tokio task does; the temp dir owns the on-disk storage and
    /// must be kept alive for the duration of the test.
    async fn start_local_s3() -> (String, String, tempfile::TempDir) {
        use s3s::{auth::SimpleAuth, service::S3ServiceBuilder};
        use s3s_fs::FileSystem;

        let tmp = tempfile::TempDir::new().expect("create tmp dir");
        let fs = FileSystem::new(tmp.path()).expect("create s3s FileSystem");

        // S3ServiceBuilder uses a mutable builder pattern (set_* take &mut self).
        let mut builder = S3ServiceBuilder::new(fs);
        builder.set_auth(SimpleAuth::from_single("minioadmin", "minioadmin"));
        let service = builder.build();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind to random port");
        let addr = listener.local_addr().expect("get local addr");

        // S3Service implements hyper::service::Service<Request<Incoming>> directly —
        // no TowerToHyperService adapter needed.
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let service = service.clone();
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(io, service)
                    .await
                    .ok();
                });
            }
        });

        let bucket = "heddle-test".to_string();
        let endpoint = format!("http://{addr}");

        // s3s-fs maps each bucket to a subdirectory under the root.
        // Create it directly — simpler than an HTTP CreateBucket round-trip.
        std::fs::create_dir_all(tmp.path().join(&bucket))
            .expect("create bucket directory for s3s-fs");

        (endpoint, bucket, tmp)
    }

    async fn build_test_store(endpoint: &str, bucket: &str) -> (aws_sdk_s3::Client, S3Store) {
        let credentials = aws_sdk_s3::config::Credentials::new(
            "minioadmin",
            "minioadmin",
            None,
            None,
            "heddle-s3-tests",
        );
        let config = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(credentials)
            .sleep_impl(TokioSleep::new())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .build();
        let client = aws_sdk_s3::Client::from_conf(config);
        let store = S3Store::new(client.clone(), bucket, "");
        (client, store)
    }
}