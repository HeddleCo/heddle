// SPDX-License-Identifier: Apache-2.0
//! Git loose-object framing and content-addressed hashing.

use sley_core::ObjectFormat;

use crate::{GitSubstrateError, Result};

/// Frame an object's content for hashing per git's loose-object format:
/// `<kind> <ascii-decimal-len>\0<content>`.
///
/// A git object's id is the SHA-1 of this buffer — never of the bare content
/// (`git cat-file` strips the framing). `<len>` is the byte length of `content`
/// with no leading zeros.
pub fn frame_git_object(kind: &str, content: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(kind.len() + 2 + 20 + content.len());
    framed.extend_from_slice(kind.as_bytes());
    framed.push(b' ');
    framed.extend_from_slice(content.len().to_string().as_bytes());
    framed.push(0);
    framed.extend_from_slice(content);
    framed
}

/// Hash framed object bytes for `object_type` + `content` using SHA-1.
pub fn object_id_for_content(object_type: &str, content: &[u8]) -> Result<sley_core::ObjectId> {
    sley_core::object_id_for_bytes(ObjectFormat::Sha1, object_type, content)
        .map_err(GitSubstrateError::from)
}

/// Render a timezone offset (seconds east of UTC) as git's `±HHMM` (§5).
pub fn format_tz_offset(offset_secs: i32) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let minutes = offset_secs.unsigned_abs() / 60;
    format!("{sign}{:02}{:02}", minutes / 60, minutes % 60)
}

/// Git commit actor suffix without the header label: `name <email> unix-seconds ±HHMM`.
pub fn actor_suffix_bytes(
    name: &[u8],
    email: &[u8],
    seconds: i64,
    tz_offset_secs: i32,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(email);
    out.extend_from_slice(b"> ");
    out.extend_from_slice(seconds.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(format_tz_offset(tz_offset_secs).as_bytes());
    out
}

/// Append one commit header actor line: `label name <email> unix-seconds ±HHMM\n`.
pub fn append_labeled_actor_line(
    out: &mut Vec<u8>,
    label: &[u8],
    name: &[u8],
    email: &[u8],
    seconds: i64,
    tz_offset_secs: i32,
) {
    out.extend_from_slice(label);
    out.push(b' ');
    out.extend_from_slice(&actor_suffix_bytes(name, email, seconds, tz_offset_secs));
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use sley_object::{EncodedObject, ObjectType};

    use super::*;
    use crate::id::{from_gix, to_gix};

    #[test]
    fn frame_prepends_kind_len_nul() {
        assert_eq!(frame_git_object("commit", b"abc"), b"commit 3\0abc");
        assert_eq!(frame_git_object("commit", b""), b"commit 0\0");
    }

    #[test]
    fn tz_offset_renders_sign_hours_minutes() {
        assert_eq!(format_tz_offset(0), "+0000");
        assert_eq!(format_tz_offset(2 * 3600), "+0200");
        assert_eq!(format_tz_offset(-8 * 3600), "-0800");
        assert_eq!(format_tz_offset(-(8 * 3600 + 30 * 60)), "-0830");
        assert_eq!(format_tz_offset(12 * 3600 + 45 * 60), "+1245");
        assert_eq!(format_tz_offset(5 * 3600 + 30 * 60), "+0530");
    }

    #[test]
    fn actor_suffix_matches_commit_header_shape() {
        assert_eq!(
            actor_suffix_bytes(b"Alice", b"alice@example.com", 1_000, 0),
            b"Alice <alice@example.com> 1000 +0000"
        );
    }

    /// Conformance: substrate framing + SHA-1 matches git's well-known ids.
    #[test]
    fn round_trip_known_object_bytes_matches_expected_sha() {
        let cases = [
            (
                "blob",
                b"".as_slice(),
                "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
            ),
            (
                "tree",
                b"".as_slice(),
                "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            ),
            (
                "blob",
                b"hello\n".as_slice(),
                "ce013625030ba8dba906f756967f9e9ca394464a",
            ),
        ];

        for (kind, content, expected_hex) in cases {
            let object_type: ObjectType = kind.parse().expect("object type");
            let framed = frame_git_object(kind, content);
            assert_eq!(
                framed,
                EncodedObject::new(object_type, content).framed_bytes()
            );

            let oid = object_id_for_content(kind, content).expect("hash object");
            assert_eq!(oid.to_hex(), expected_hex);

            let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
            hasher.update(&framed);
            let gix_hash = hasher.try_finalize().expect("sha1");
            assert_eq!(oid.to_hex(), gix_hash.to_hex().to_string());
            assert_eq!(to_gix(&oid).expect("to_gix"), gix_hash);
            assert_eq!(
                from_gix(gix_hash).expect("from_gix").to_hex(),
                expected_hex
            );
        }
    }
}