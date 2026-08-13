// SPDX-License-Identifier: Apache-2.0
//! Byte-exact annotated Git tag objects stored in Heddle's native CAS.

use serde::{Deserialize, Serialize};
use sley::{
    GitObjectType, ObjectFormat as GitObjectFormat, ObjectId as GitObjectId, TagObject,
    plumbing::sley_object::EncodedObject,
};
use thiserror::Error;

use super::{ContentHash, StateId};

const ANNOTATED_TAG_FORMAT_VERSION: u8 = 1;
const GIT_FORMAT_SHA1: u8 = 1;
const GIT_FORMAT_SHA256: u8 = 2;

/// The native marker that owns an outer annotated-tag object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedTagMarker {
    pub name: String,
    pub peeled_state: StateId,
}

/// A byte-exact annotated Git tag object.
///
/// `body` is the unframed body returned by `git cat-file tag`. It retains the
/// tagger identity and timezone, message, and any appended signature verbatim.
/// `target_tag` links an outer tag to the native CAS object for an inner tag;
/// the eventual commit target is represented by `marker.peeled_state` on the
/// outermost object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedTag {
    format_version: u8,
    git_format: u8,
    body: Vec<u8>,
    target_tag: Option<ContentHash>,
    marker: Option<AnnotatedTagMarker>,
}

impl AnnotatedTag {
    /// Build a validated native tag object from exact Git tag bytes.
    pub fn new(
        git_format: GitObjectFormat,
        body: Vec<u8>,
        target_tag: Option<ContentHash>,
        marker: Option<AnnotatedTagMarker>,
    ) -> Result<Self, AnnotatedTagError> {
        TagObject::parse_ref(git_format, &body)
            .map_err(|error| AnnotatedTagError::InvalidGitTag(error.to_string()))?;
        Ok(Self {
            format_version: ANNOTATED_TAG_FORMAT_VERSION,
            git_format: git_format_tag(git_format),
            body,
            target_tag,
            marker,
        })
    }

    /// Decode and validate the current durable msgpack representation.
    pub fn decode_current_msgpack(bytes: &[u8]) -> Result<Self, AnnotatedTagError> {
        let tag: Self = rmp_serde::from_slice(bytes)?;
        if tag.format_version != ANNOTATED_TAG_FORMAT_VERSION {
            return Err(AnnotatedTagError::UnsupportedVersion {
                found: tag.format_version,
                supported: ANNOTATED_TAG_FORMAT_VERSION,
            });
        }
        TagObject::parse_ref(tag.git_format()?, &tag.body)
            .map_err(|error| AnnotatedTagError::InvalidGitTag(error.to_string()))?;
        Ok(tag)
    }

    /// Encode the versioned durable representation used by loose objects and packs.
    pub fn encode_current_msgpack(&self) -> Vec<u8> {
        rmp_serde::to_vec_named(self).expect("annotated tag encoding is infallible")
    }

    /// Native content address of this complete record.
    pub fn hash(&self) -> ContentHash {
        ContentHash::compute_typed("annotated-tag", &self.encode_current_msgpack())
    }

    /// Exact unframed Git tag-object body.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Git object format used to parse and hash this tag.
    pub fn git_format(&self) -> Result<GitObjectFormat, AnnotatedTagError> {
        match self.git_format {
            GIT_FORMAT_SHA1 => Ok(GitObjectFormat::Sha1),
            GIT_FORMAT_SHA256 => Ok(GitObjectFormat::Sha256),
            other => Err(AnnotatedTagError::UnknownGitFormat(other)),
        }
    }

    /// Original Git object id, computed from `tag <len>\0<body>`.
    pub fn git_oid(&self) -> Result<GitObjectId, AnnotatedTagError> {
        EncodedObject::new(GitObjectType::Tag, self.body.clone())
            .object_id(self.git_format()?)
            .map_err(|error| AnnotatedTagError::InvalidGitTag(error.to_string()))
    }

    /// Native CAS link to an inner tag object for tag-of-tag chains.
    pub fn target_tag(&self) -> Option<ContentHash> {
        self.target_tag
    }

    /// Marker binding carried only by the outermost tag object.
    pub fn marker(&self) -> Option<&AnnotatedTagMarker> {
        self.marker.as_ref()
    }
}

fn git_format_tag(format: GitObjectFormat) -> u8 {
    match format {
        GitObjectFormat::Sha1 => GIT_FORMAT_SHA1,
        GitObjectFormat::Sha256 => GIT_FORMAT_SHA256,
    }
}

#[derive(Debug, Error)]
pub enum AnnotatedTagError {
    #[error(
        "unsupported annotated-tag format version {found}; this binary supports {supported}; upgrade heddle or run `heddle migrate`"
    )]
    UnsupportedVersion { found: u8, supported: u8 },
    #[error("unknown annotated-tag Git object format {0}")]
    UnknownGitFormat(u8),
    #[error("invalid annotated Git tag object: {0}")]
    InvalidGitTag(String),
    #[error("invalid annotated-tag encoding: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exemplar() -> AnnotatedTag {
        AnnotatedTag::new(
            GitObjectFormat::Sha1,
            b"object 1111111111111111111111111111111111111111\ntype commit\ntag v1.0\ntagger Tagger <tagger@example.com> 1700000000 -0730\n\nrelease\n-----BEGIN PGP SIGNATURE-----\nsigned bytes\n-----END PGP SIGNATURE-----\n".to_vec(),
            Some(ContentHash::from_bytes([2; 32])),
            Some(AnnotatedTagMarker {
                name: "release/v1.0".to_string(),
                peeled_state: StateId::from_bytes([3; 32]),
            }),
        )
        .expect("valid annotated tag")
    }

    #[test]
    fn msgpack_roundtrip_preserves_exact_body() {
        let tag = exemplar();
        let decoded = AnnotatedTag::decode_current_msgpack(&tag.encode_current_msgpack())
            .expect("decode annotated tag");
        assert_eq!(decoded, tag);
        assert_eq!(decoded.body(), tag.body());
        assert_eq!(decoded.git_oid().unwrap(), tag.git_oid().unwrap());
    }

    #[test]
    fn canonical_encoding_is_format_locked() {
        let encoded = exemplar().encode_current_msgpack();
        assert_eq!(
            hex::encode(encoded),
            "85ae666f726d61745f76657273696f6e01aa6769745f666f726d617401a4626f6479dc00c96f626a65637420313131313131313131313131313131313131313131313131313131313131313131313131313131310a7479706520636f6d6d69740a7461672076312e300a74616767657220546167676572203c746167676572406578616d706c652e636f6d3e2031373030303030303030202d303733300a0a72656c656173650a2d2d2d2d2d424547494e20504750205349474e41545552452d2d2d2d2d0a7369676e65642062797465730a2d2d2d2d2d454e4420504750205349474e41545552452d2d2d2d2d0aaa7461726765745f746167dc00200202020202020202020202020202020202020202020202020202020202020202a66d61726b657282a46e616d65ac72656c656173652f76312e30ac7065656c65645f7374617465dc00200303030303030303030303030303030303030303030303030303030303030303",
            "changing annotated-tag bytes requires a format-version decision and migration"
        );
    }
}
