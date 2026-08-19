// SPDX-License-Identifier: Apache-2.0

use super::*;

const COMMIT_OID: &str = "1111111111111111111111111111111111111111";
const OTHER_OID: &str = "2222222222222222222222222222222222222222";

/// Previously accepted encoding: `type commit` with `target_tag` set.
const COMMIT_WITH_TARGET_TAG_HEX: &str = "85ae666f726d61745f76657273696f6e01aa6769745f666f726d617401a4626f6479dc00c96f626a65637420313131313131313131313131313131313131313131313131313131313131313131313131313131310a7479706520636f6d6d69740a7461672076312e300a74616767657220546167676572203c746167676572406578616d706c652e636f6d3e2031373030303030303030202d303733300a0a72656c656173650a2d2d2d2d2d424547494e20504750205349474e41545552452d2d2d2d2d0a7369676e65642062797465730a2d2d2d2d2d454e4420504750205349474e41545552452d2d2d2d2d0aaa7461726765745f746167dc00200202020202020202020202020202020202020202020202020202020202020202a66d61726b657282a46e616d65ac72656c656173652f76312e30ac7065656c65645f7374617465dc00200303030303030303030303030303030303030303030303030303030303030303";

fn tag_body(object: &str, kind: &str, name: &str) -> Vec<u8> {
    format!(
        "object {object}\ntype {kind}\ntag {name}\ntagger Tagger <tagger@example.com> 1700000000 -0730\n\nrelease\n"
    )
    .into_bytes()
}

fn signed_commit_tag_body() -> Vec<u8> {
    format!(
        "object {COMMIT_OID}\ntype commit\ntag v1.0\ntagger Tagger <tagger@example.com> 1700000000 -0730\n\nrelease\n-----BEGIN PGP SIGNATURE-----\nsigned bytes\n-----END PGP SIGNATURE-----\n"
    )
    .into_bytes()
}

fn marker() -> AnnotatedTagMarker {
    AnnotatedTagMarker {
        name: "release/v1.0".to_string(),
        peeled_state: StateId::from_bytes([3; 32]),
    }
}

fn exemplar() -> AnnotatedTag {
    AnnotatedTag::new(
        GitObjectFormat::Sha1,
        signed_commit_tag_body(),
        None,
        Some(marker()),
    )
    .expect("valid annotated tag")
}

fn unchecked(
    body: Vec<u8>,
    target_tag: Option<ContentHash>,
    marker: Option<AnnotatedTagMarker>,
) -> Vec<u8> {
    AnnotatedTag {
        format_version: ANNOTATED_TAG_FORMAT_VERSION,
        git_format: GIT_FORMAT_SHA1,
        body,
        target_tag,
        marker,
    }
    .encode_current_msgpack()
}

#[test]
fn msgpack_roundtrip_preserves_exact_body() {
    let tag = exemplar();
    let decoded = AnnotatedTag::decode_current_msgpack(&tag.encode_current_msgpack())
        .expect("decode annotated tag");
    assert_eq!(decoded, tag);
    assert_eq!(decoded.body(), tag.body());
    assert_eq!(decoded.git_oid().unwrap(), tag.git_oid().unwrap());
    assert_eq!(decoded.git_target().unwrap().to_string(), COMMIT_OID);
    assert_eq!(decoded.git_target_type().unwrap(), GitObjectType::Commit);
}

#[test]
fn canonical_encoding_is_format_locked() {
    let encoded = exemplar().encode_current_msgpack();
    assert_eq!(
        hex::encode(encoded),
        "85ae666f726d61745f76657273696f6e01aa6769745f666f726d617401a4626f6479dc00c96f626a65637420313131313131313131313131313131313131313131313131313131313131313131313131313131310a7479706520636f6d6d69740a7461672076312e300a74616767657220546167676572203c746167676572406578616d706c652e636f6d3e2031373030303030303030202d303733300a0a72656c656173650a2d2d2d2d2d424547494e20504750205349474e41545552452d2d2d2d2d0a7369676e65642062797465730a2d2d2d2d2d454e4420504750205349474e41545552452d2d2d2d2d0aaa7461726765745f746167c0a66d61726b657282a46e616d65ac72656c656173652f76312e30ac7065656c65645f7374617465dc00200303030303030303030303030303030303030303030303030303030303030303",
        "changing annotated-tag bytes requires a format-version decision and migration"
    );
}

#[test]
fn new_rejects_commit_target_with_target_tag() {
    let error = AnnotatedTag::new(
        GitObjectFormat::Sha1,
        tag_body(COMMIT_OID, "commit", "v1.0"),
        Some(ContentHash::from_bytes([2; 32])),
        Some(marker()),
    )
    .expect_err("commit tag with target_tag must be rejected");
    assert!(
        matches!(
            error,
            AnnotatedTagError::TargetTagDisagree {
                has_target_tag: true,
                ..
            }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn new_rejects_tag_target_without_target_tag() {
    let error = AnnotatedTag::new(
        GitObjectFormat::Sha1,
        tag_body(OTHER_OID, "tag", "outer"),
        None,
        Some(marker()),
    )
    .expect_err("tag-of-tag without target_tag must be rejected");
    assert!(
        matches!(
            error,
            AnnotatedTagError::TargetTagDisagree {
                has_target_tag: false,
                ..
            }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn new_rejects_blob_git_target() {
    let error = AnnotatedTag::new(
        GitObjectFormat::Sha1,
        tag_body(COMMIT_OID, "blob", "gpg-pub"),
        None,
        None,
    )
    .expect_err("blob-pointing tag must be rejected");
    assert!(
        matches!(error, AnnotatedTagError::UnsupportedGitTarget { .. }),
        "unexpected error: {error}"
    );
}

#[test]
fn decode_rejects_commit_target_disagreeing_with_target_tag() {
    let bytes = hex::decode(COMMIT_WITH_TARGET_TAG_HEX).expect("fixture hex");
    let error = AnnotatedTag::decode_current_msgpack(&bytes)
        .expect_err("legacy disagreeing encoding must fail closed");
    assert!(
        matches!(
            error,
            AnnotatedTagError::TargetTagDisagree {
                has_target_tag: true,
                ..
            }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn decode_rejects_tag_target_without_target_tag() {
    let bytes = unchecked(tag_body(OTHER_OID, "tag", "outer"), None, Some(marker()));
    let error = AnnotatedTag::decode_current_msgpack(&bytes)
        .expect_err("tag-of-tag without target_tag must fail closed");
    assert!(
        matches!(
            error,
            AnnotatedTagError::TargetTagDisagree {
                has_target_tag: false,
                ..
            }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn bind_target_tag_rejects_git_target_disagreeing_with_inner_oid() {
    let inner = AnnotatedTag::new(
        GitObjectFormat::Sha1,
        tag_body(COMMIT_OID, "commit", "inner"),
        None,
        None,
    )
    .expect("inner tag");
    let outer = AnnotatedTag::new(
        GitObjectFormat::Sha1,
        tag_body(OTHER_OID, "tag", "outer"),
        Some(inner.hash()),
        Some(marker()),
    )
    .expect("outer tag shape is valid");
    let error = outer
        .bind_target_tag(&inner)
        .expect_err("Git target must match inner git_oid");
    assert!(
        matches!(error, AnnotatedTagError::GitTargetDisagree { .. }),
        "unexpected error: {error}"
    );
}

#[test]
fn bind_target_tag_accepts_matching_inner_tag() {
    let inner = AnnotatedTag::new(
        GitObjectFormat::Sha1,
        tag_body(COMMIT_OID, "commit", "inner"),
        None,
        None,
    )
    .expect("inner tag");
    let inner_oid = inner.git_oid().expect("inner git oid");
    let outer = AnnotatedTag::new(
        GitObjectFormat::Sha1,
        tag_body(&inner_oid.to_string(), "tag", "outer"),
        Some(inner.hash()),
        Some(marker()),
    )
    .expect("outer tag");
    outer
        .bind_target_tag(&inner)
        .expect("Git target and target_tag agree");
}
