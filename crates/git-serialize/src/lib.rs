// SPDX-License-Identifier: Apache-2.0
//! Engine-free Git object content serialization from Heddle native objects.
//!
//! Bytes are unframed, as returned by `git cat-file`. Import fidelity metadata
//! (raw messages, ordered extension headers, raw tree modes and source order)
//! is authoritative. This crate does not open repositories or retain Git packs.
//! See [`reconstruct_closure`] for bounded, asynchronous reconstruction.

mod closure;
mod commit;
mod tree;

pub use closure::{
    NativeObjectId, ObjectReader, ObjectSink, ReconstructionError, ReconstructionLimits,
    reconstruct_closure,
};
pub use commit::{CommitMetadata, serialize_commit};
use heddle_object_model::object::Blob;
pub use sley_core::{ObjectFormat, ObjectId};
pub use tree::serialize_tree;

/// Invalid or incomplete native fidelity metadata.
#[derive(Debug, thiserror::Error)]
pub enum SerializeError {
    #[error("invalid Git reconstruction metadata: {0}")]
    InvalidObject(String),
}

pub type Result<T> = std::result::Result<T, SerializeError>;

/// A blob's Git content is its native content, without a copy or a header.
pub fn serialize_blob(blob: &Blob) -> &[u8] {
    blob.content()
}

/// Git object types supported by native reconstruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitObjectKind {
    Blob,
    Tree,
    Commit,
}

impl GitObjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
        }
    }
}

impl std::fmt::Display for GitObjectKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Hash unframed content with Git's `<kind> <len>\0` header.
pub fn object_id(format: ObjectFormat, kind: GitObjectKind, content: &[u8]) -> Result<ObjectId> {
    let mut digest = sley_core::StreamingDigest::new(format);
    digest.update(kind.as_str().as_bytes());
    digest.update(b" ");
    digest.update(content.len().to_string().as_bytes());
    digest.update(&[0]);
    digest.update(content);
    digest
        .finalize()
        .map_err(|error| SerializeError::InvalidObject(error.to_string()))
}
