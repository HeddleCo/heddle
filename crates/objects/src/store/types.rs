// SPDX-License-Identifier: Apache-2.0
//! Typed keys and pagination primitives for object storage.

use bytes::Bytes;

use crate::{
    error::{HeddleError, Result, StorageErrorKind},
    object::{ActionId, ChangeId, ContentHash},
};

use super::pack::PackObjectId;

/// Opaque continuation token returned by paginated storage listings.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PageToken(String);

impl PageToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Page request for store listings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageRequest {
    pub limit: Option<usize>,
    pub token: Option<PageToken>,
}

impl PageRequest {
    pub fn first(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            token: None,
        }
    }
}

/// One page of storage listing results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_token: Option<PageToken>,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, next_token: Option<PageToken>) -> Self {
        Self { items, next_token }
    }

    pub fn from_local_items(items: Vec<T>, request: PageRequest) -> Result<Self> {
        let offset = match request.token.as_ref() {
            Some(token) => token.as_str().parse::<usize>().map_err(|_| {
                HeddleError::storage(
                    StorageErrorKind::Invalid,
                    format!("invalid local pagination token '{}'", token.as_str()),
                )
            })?,
            None => 0,
        }
        .min(items.len());
        let limit = request.limit.unwrap_or(items.len().saturating_sub(offset));
        let end = offset.saturating_add(limit).min(items.len());
        let next_token = (end < items.len()).then(|| PageToken::new(end.to_string()));
        Ok(Self {
            items: items.into_iter().skip(offset).take(end - offset).collect(),
            next_token,
        })
    }
}

/// Storage collection to enumerate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectCollection {
    Blobs,
    Trees,
    States,
    Actions,
    Redactions,
    StateVisibility,
}

/// Typed object or sidecar key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKey {
    Blob(ContentHash),
    Tree(ContentHash),
    State(ChangeId),
    Action(ActionId),
    Redactions(ContentHash),
    StateVisibility(ChangeId),
    PackObject(PackObjectId),
}

/// Byte payload paired with a typed storage key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectBytes {
    pub key: ObjectKey,
    pub bytes: Bytes,
}

impl ObjectBytes {
    pub fn new(key: ObjectKey, bytes: impl Into<Bytes>) -> Self {
        Self {
            key,
            bytes: bytes.into(),
        }
    }
}

/// Result of a batch existence probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectPresence {
    pub key: ObjectKey,
    pub present: bool,
}

/// Result of one item in a batch write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectPutOutcome {
    pub key: ObjectKey,
    pub written: bool,
}
