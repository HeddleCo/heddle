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
/// outermost object. Construct and decode reject a Git target that disagrees
/// with `target_tag`; export binds `peeled_state` to the mapped commit.
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
        bind_git_target(git_format, &body, target_tag)?;
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
        bind_git_target(tag.git_format()?, &tag.body, tag.target_tag)?;
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

    /// Git object named by this tag body (`object <oid>`).
    pub fn git_target(&self) -> Result<GitObjectId, AnnotatedTagError> {
        Ok(self.parsed_git_tag()?.object)
    }

    /// Git object type named by this tag body (`type commit` / `type tag`).
    pub fn git_target_type(&self) -> Result<GitObjectType, AnnotatedTagError> {
        Ok(self.parsed_git_tag()?.object_type)
    }

    /// Native CAS link to an inner tag object for tag-of-tag chains.
    pub fn target_tag(&self) -> Option<ContentHash> {
        self.target_tag
    }

    /// Marker binding carried only by the outermost tag object.
    pub fn marker(&self) -> Option<&AnnotatedTagMarker> {
        self.marker.as_ref()
    }

    /// Prove `inner` is the native object this tag's Git target names.
    pub fn bind_target_tag(&self, inner: &AnnotatedTag) -> Result<(), AnnotatedTagError> {
        let parsed = self.parsed_git_tag()?;
        let Some(expected_hash) = self.target_tag else {
            return Err(target_tag_disagree(&parsed, false));
        };
        if parsed.object_type != GitObjectType::Tag {
            return Err(target_tag_disagree(&parsed, true));
        }
        if inner.hash() != expected_hash {
            return Err(AnnotatedTagError::GitTargetDisagree {
                git_target: parsed.object.to_string(),
                actual: inner.hash().to_string(),
            });
        }
        let inner_oid = inner.git_oid()?;
        if inner_oid != parsed.object {
            return Err(AnnotatedTagError::GitTargetDisagree {
                git_target: parsed.object.to_string(),
                actual: inner_oid.to_string(),
            });
        }
        Ok(())
    }

    fn parsed_git_tag(&self) -> Result<TagObject, AnnotatedTagError> {
        parse_git_tag(self.git_format()?, &self.body)
    }
}

fn bind_git_target(
    git_format: GitObjectFormat,
    body: &[u8],
    target_tag: Option<ContentHash>,
) -> Result<TagObject, AnnotatedTagError> {
    let parsed = parse_git_tag(git_format, body)?;
    let has_target_tag = target_tag.is_some();
    match parsed.object_type {
        GitObjectType::Commit if !has_target_tag => Ok(parsed),
        GitObjectType::Tag if has_target_tag => Ok(parsed),
        GitObjectType::Commit | GitObjectType::Tag => {
            Err(target_tag_disagree(&parsed, has_target_tag))
        }
        GitObjectType::Blob | GitObjectType::Tree => Err(AnnotatedTagError::UnsupportedGitTarget {
            git_target: parsed.object.to_string(),
            git_type: git_object_type_name(parsed.object_type),
        }),
    }
}

fn parse_git_tag(git_format: GitObjectFormat, body: &[u8]) -> Result<TagObject, AnnotatedTagError> {
    TagObject::parse(git_format, body)
        .map_err(|error| AnnotatedTagError::InvalidGitTag(error.to_string()))
}

fn target_tag_disagree(parsed: &TagObject, has_target_tag: bool) -> AnnotatedTagError {
    AnnotatedTagError::TargetTagDisagree {
        git_target: parsed.object.to_string(),
        git_type: git_object_type_name(parsed.object_type),
        has_target_tag,
    }
}

fn git_format_tag(format: GitObjectFormat) -> u8 {
    match format {
        GitObjectFormat::Sha1 => GIT_FORMAT_SHA1,
        GitObjectFormat::Sha256 => GIT_FORMAT_SHA256,
    }
}

fn git_object_type_name(kind: GitObjectType) -> &'static str {
    kind.as_str()
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
    #[error(
        "annotated tag Git target {git_target} ({git_type}) disagrees with target_tag present={has_target_tag}"
    )]
    TargetTagDisagree {
        git_target: String,
        git_type: &'static str,
        has_target_tag: bool,
    },
    #[error("annotated tag Git target {git_target} disagrees with target_tag object {actual}")]
    GitTargetDisagree { git_target: String, actual: String },
    #[error("annotated tags cannot name a {git_type} Git target {git_target}")]
    UnsupportedGitTarget {
        git_target: String,
        git_type: &'static str,
    },
    #[error("invalid annotated-tag encoding: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

#[cfg(test)]
#[path = "annotated_tag_tests.rs"]
mod tests;
