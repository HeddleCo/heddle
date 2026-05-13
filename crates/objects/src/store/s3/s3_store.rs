// SPDX-License-Identifier: Apache-2.0
//! S3 storage implementation.

use std::sync::Arc;

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
}

impl S3Store {
    /// Create a new S3 store with the given client, bucket, and prefix.
    pub fn new(client: Client, bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            client: Arc::new(client),
            bucket: bucket.into(),
            prefix: prefix.into(),
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

    /// Get a handle to the current Tokio runtime.
    pub(super) fn runtime(&self) -> crate::store::Result<tokio::runtime::Handle> {
        tokio::runtime::Handle::try_current().map_err(|e| {
            crate::store::StoreError::Io(std::io::Error::other(format!(
                "No async runtime available: {}",
                e
            )))
        })
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