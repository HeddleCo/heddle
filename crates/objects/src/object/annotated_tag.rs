// SPDX-License-Identifier: Apache-2.0
//! Annotated git tag objects (#564 de-lossy step 1, #565).
//!
//! Heddle's marker model stores only a tag's *peeled commit* `ChangeId`.
//! That is enough to point a tag at the right state, but it throws away
//! everything that makes an annotated tag a distinct git object: the
//! tagger, the tag message, the gpg signature, and the original tag name.
//! Once the git mirror is eliminated (#568) that metadata would be gone for
//! good and the tag could no longer be byte-reconstructed (#566/#567).
//!
//! [`AnnotatedTag`] captures that metadata so it can be stored alongside a
//! marker (see the object store's annotated-tag sidecar) and reconstructed
//! later. Lightweight tags have no annotated-tag object — they are just a
//! marker pointing at a commit, and that is preserved unchanged.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ChangeId, Principal};

/// The metadata of an annotated git tag object, captured on import so the
/// tag can be byte-reconstructed after the git mirror is dropped.
///
/// Field order mirrors a git tag object's header order so #566's serializer
/// can reconstruct it directly. `extra_headers` is an ordered `Vec` —
/// order is load-bearing for byte-exactness, never a map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedTag {
    /// The Heddle [`ChangeId`] of the object the tag points at. For the
    /// common case (`target_kind == "commit"`) this is the state the
    /// marker also resolves to.
    pub object: ChangeId,
    /// The git type of the tagged object, verbatim (`"commit"`, and in
    /// rare tag-of-tag / tag-of-blob cases `"tag"` / `"blob"` / `"tree"`).
    pub target_kind: String,
    /// The tag's own name, verbatim (the `tag <name>` header).
    pub tag_name: String,
    /// The tagger identity, when present. Some imported tags (and all
    /// lightweight tags, which never reach here) have no tagger.
    #[serde(default)]
    pub tagger: Option<Principal>,
    /// The tagger timestamp, when present.
    #[serde(default)]
    pub tagger_at: Option<DateTime<Utc>>,
    /// Timezone offset (seconds east of UTC) of the tagger timestamp.
    #[serde(default)]
    pub tagger_tz_offset: i32,
    /// The tag message body, verbatim.
    #[serde(default)]
    pub message: Option<String>,
    /// The tag's `gpgsig`-equivalent (the embedded PGP signature block),
    /// verbatim, when the tag is signed.
    #[serde(default)]
    pub git_gpgsig: Option<String>,
    /// Any remaining tag headers in original order. ORDER IS LOAD-BEARING
    /// (#566). Empty for ordinary annotated tags.
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
}

impl AnnotatedTag {
    /// Create an annotated tag pointing at `object` of git type
    /// `target_kind` (usually `"commit"`), with the given name.
    pub fn new(
        object: ChangeId,
        target_kind: impl Into<String>,
        tag_name: impl Into<String>,
    ) -> Self {
        Self {
            object,
            target_kind: target_kind.into(),
            tag_name: tag_name.into(),
            tagger: None,
            tagger_at: None,
            tagger_tz_offset: 0,
            message: None,
            git_gpgsig: None,
            extra_headers: Vec::new(),
        }
    }

    pub fn with_tagger(mut self, tagger: Principal, at: DateTime<Utc>, tz_offset: i32) -> Self {
        self.tagger = Some(tagger);
        self.tagger_at = Some(at);
        self.tagger_tz_offset = tz_offset;
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_git_gpgsig(mut self, gpgsig: impl Into<String>) -> Self {
        self.git_gpgsig = Some(gpgsig.into());
        self
    }

    pub fn with_extra_headers(mut self, extra_headers: Vec<(String, String)>) -> Self {
        self.extra_headers = extra_headers;
        self
    }

    /// Serialize to the rmp bytes stored in the annotated-tag sidecar.
    pub fn to_bytes(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }

    /// Deserialize from the rmp bytes stored in the annotated-tag sidecar.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
        rmp_serde::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AnnotatedTag {
        AnnotatedTag::new(ChangeId::from_bytes([5; 16]), "commit", "v1.0")
            .with_tagger(
                Principal::new("Tagger", "tagger@example.com"),
                Utc::now(),
                -7200,
            )
            .with_message("release\n")
            .with_git_gpgsig("-----BEGIN PGP SIGNATURE-----\n")
            .with_extra_headers(vec![("custom".into(), "x".into())])
    }

    #[test]
    fn round_trips_through_bytes() {
        let tag = sample();
        let bytes = tag.to_bytes().unwrap();
        let back = AnnotatedTag::from_bytes(&bytes).unwrap();
        assert_eq!(tag, back);
    }

    #[test]
    fn lightweight_constructor_has_no_metadata() {
        let tag = AnnotatedTag::new(ChangeId::from_bytes([1; 16]), "commit", "v0.1");
        assert!(tag.tagger.is_none());
        assert!(tag.message.is_none());
        assert!(tag.git_gpgsig.is_none());
        assert!(tag.extra_headers.is_empty());
    }
}
