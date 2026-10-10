// SPDX-License-Identifier: Apache-2.0
//! Exact Git commit content from native fidelity metadata.
use heddle_object_model::object::{Principal, State, TZ_OFFSET_NEGATIVE_UTC};
use sley_core::ObjectId;

use crate::{Result, SerializeError};

/// Borrowed Git commit metadata. Timestamps are UTC Unix seconds; timezone
/// offsets are seconds east of UTC (including [`TZ_OFFSET_NEGATIVE_UTC`]).
/// This view lets native projection supply its message/identity policy without
/// copying a State or using another Git serializer.
#[derive(Clone, Copy, Debug)]
pub struct CommitMetadata<'a> {
    pub author: &'a Principal,
    pub author_seconds: i64,
    pub author_tz_offset: i32,
    pub committer: &'a Principal,
    pub committer_seconds: i64,
    pub committer_tz_offset: i32,
    pub extra_headers: &'a [(Vec<u8>, Vec<u8>)],
    pub message: &'a [u8],
}

impl<'a> From<&'a State> for CommitMetadata<'a> {
    fn from(state: &'a State) -> Self {
        let author = &state.attribution.principal;
        Self {
            author,
            author_seconds: state.authored_at.unwrap_or(state.created_at).timestamp(),
            author_tz_offset: state.authored_tz_offset,
            committer: state.committer.as_ref().unwrap_or(author),
            committer_seconds: state.created_at.timestamp(),
            committer_tz_offset: state.committer_tz_offset,
            extra_headers: &state.extra_headers,
            message: state.raw_message.as_deref().unwrap_or_default(),
        }
    }
}

/// Assemble the commit content bytes from already-resolved OIDs. Pure (no repo,
/// no mapping) so the byte layout — header order, actor lines, header folding,
/// verbatim message — is unit-testable in isolation (§1/§2/§5/§6).
/// Accepts a native `&State` or a borrowed [`CommitMetadata`] view.
pub fn serialize_commit<'a>(
    metadata: impl Into<CommitMetadata<'a>>,
    tree_oid: &ObjectId,
    parent_oids: &[ObjectId],
) -> Result<Vec<u8>> {
    let metadata = metadata.into();
    if parent_oids
        .iter()
        .any(|id| id.format() != tree_oid.format())
    {
        return Err(SerializeError::InvalidObject(
            "commit IDs use different Git object formats".into(),
        ));
    }
    let mut out = Vec::new();

    // `tree` is always first, exactly once (§1.1).
    out.extend_from_slice(b"tree ");
    out.extend_from_slice(tree_oid.to_string().as_bytes());
    out.push(b'\n');

    // `parent` lines follow, zero or more, in recorded order (§1.2).
    for parent in parent_oids {
        out.extend_from_slice(b"parent ");
        out.extend_from_slice(parent.to_string().as_bytes());
        out.push(b'\n');
    }

    // `author` then `committer` (§1.3/§5), with independently recorded times
    // and offsets. State fallbacks are resolved by the borrowed metadata view.
    write_actor_line(
        &mut out,
        b"author",
        metadata.author,
        metadata.author_seconds,
        metadata.author_tz_offset,
    )?;
    write_actor_line(
        &mut out,
        b"committer",
        metadata.committer,
        metadata.committer_seconds,
        metadata.committer_tz_offset,
    )?;

    // Extension headers (`encoding`/`gpgsig`/`mergetag`/unknown) at their captured
    // ordinal, multi-line values re-folded (§1.4/§2). The ordered `Vec` is the
    // source of truth — gpgsig and mergetag are just entries here, never
    // special-cased; when both are present git emits mergetag before gpgsig and
    // the captured order already encodes that.
    for (name, value) in metadata.extra_headers {
        out.extend_from_slice(name);
        out.push(b' ');
        append_folded(&mut out, value);
        out.push(b'\n');
    }

    // Exactly one blank line separates headers from the body (§1.5) — always
    // present, even for an empty message.
    out.push(b'\n');

    // Message bytes verbatim: no trim, no appended newline (§6). An empty message
    // contributes zero bytes; a message without a trailing newline ends mid-line.
    out.extend_from_slice(metadata.message);

    Ok(out)
}

/// `<label> <name> <<email>> <unix-seconds> <±HHMM>\n` (§5).
fn write_actor_line(
    out: &mut Vec<u8>,
    label: &[u8],
    who: &Principal,
    seconds: i64,
    tz_offset_secs: i32,
) -> Result<()> {
    let seconds = checked_actor_timestamp(label, seconds, tz_offset_secs)?;
    out.extend_from_slice(label);
    out.push(b' ');
    // An empty name is written as `author <email>`: the separator belongs to the
    // name, so there is no name and no space.
    if !who.name.is_empty() {
        out.extend_from_slice(&who.name);
        out.push(b' ');
    }
    out.push(b'<');
    out.extend_from_slice(&who.email);
    out.extend_from_slice(b"> ");
    out.extend_from_slice(seconds.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(format_tz_offset(tz_offset_secs).as_bytes());
    out.push(b'\n');
    Ok(())
}

fn checked_actor_timestamp(label: &[u8], seconds: i64, tz_offset_secs: i32) -> Result<i64> {
    // Git serializes UTC seconds plus a timezone offset. Validate the local
    // seconds implied by that pair so malformed fidelity data cannot overflow
    // reconstruct-time timestamp arithmetic.
    seconds
        .checked_add(i64::from(tz_offset_secs))
        .map(|_| seconds)
        .ok_or_else(|| {
            let label = String::from_utf8_lossy(label);
            SerializeError::InvalidObject(format!(
                "{label} timestamp {seconds} with timezone offset {tz_offset_secs} overflows i64"
            ))
        })
}

/// Render a timezone offset — stored as **seconds** east of UTC (#565's `i32`
/// unit) — as git's `±HHMM` (§5). The sign is always present; zero is `+0000`
/// unless it is the [`TZ_OFFSET_NEGATIVE_UTC`] sentinel, which restores git's
/// `-0000` ("unknown zone", present in old histories); odd offsets like `-0830`
/// / `+1245` survive verbatim.
fn format_tz_offset(offset_secs: i32) -> String {
    if offset_secs == TZ_OFFSET_NEGATIVE_UTC {
        return "-0000".to_string();
    }
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let minutes = offset_secs.unsigned_abs() / 60;
    format!("{sign}{:02}{:02}", minutes / 60, minutes % 60)
}

/// Fold a stored (unfolded) extension-header value onto the wire (§2): each
/// internal `\n` becomes `\n ` (newline + one continuation space). A value with
/// an internal blank line folds to a line containing exactly one space — never a
/// truly empty line, which git would read as the header/body separator. Exact
/// inverse of `objects::object::parse_commit_extension_headers`'s unfold.
fn append_folded(out: &mut Vec<u8>, value: &[u8]) {
    let mut first = true;
    for segment in value.split(|&b| b == b'\n') {
        if first {
            first = false;
        } else {
            out.push(b'\n');
            out.push(b' ');
        }
        out.extend_from_slice(segment);
    }
}

#[cfg(test)]
mod tests {
    use heddle_object_model::object::parse_commit_extension_headers;

    use super::*;

    #[test]
    fn tz_offset_renders_sign_hours_minutes() {
        assert_eq!(format_tz_offset(0), "+0000");
        assert_eq!(format_tz_offset(2 * 3600), "+0200");
        assert_eq!(format_tz_offset(-8 * 3600), "-0800");
        // Odd, sub-hour offsets survive verbatim (§5).
        assert_eq!(format_tz_offset(-(8 * 3600 + 30 * 60)), "-0830");
        assert_eq!(format_tz_offset(12 * 3600 + 45 * 60), "+1245");
        assert_eq!(format_tz_offset(5 * 3600 + 30 * 60), "+0530");
        assert_eq!(format_tz_offset(TZ_OFFSET_NEGATIVE_UTC), "-0000");
    }

    #[test]
    fn write_actor_line_omits_separator_for_empty_name() {
        let principal = Principal::new("", "only@email.example");
        let mut out = Vec::new();

        write_actor_line(&mut out, b"author", &principal, 1_700_000_000, 0)
            .expect("empty name should serialize");

        assert_eq!(out, b"author <only@email.example> 1700000000 +0000\n");
    }

    #[test]
    fn fold_then_unfold_round_trips() {
        // A gpgsig-shaped value: a leading line, an internal blank line (the
        // armor's empty line), then body lines and the END marker — stored
        // unfolded, with no trailing newline (§2).
        let value: &[u8] =
            b"-----BEGIN PGP SIGNATURE-----\n\niHUEsigbytes\nmoresig\n-----END PGP SIGNATURE-----";

        // Fold the way the serializer writes the wire.
        let mut folded = Vec::new();
        folded.extend_from_slice(b"gpgsig ");
        append_folded(&mut folded, value);
        folded.push(b'\n');

        // The internal blank line must fold to a line that is exactly one space,
        // never an empty line (which would terminate the header block).
        assert!(folded.windows(3).any(|w| w == b"\n \n"));

        // Re-parsing a minimal commit header block carrying this folded header
        // must recover the original unfolded value byte-for-byte.
        let mut content = Vec::new();
        content.extend_from_slice(b"tree ");
        content.extend_from_slice(&[b'0'; 40]);
        content.push(b'\n');
        content.extend_from_slice(b"author A <a@x> 1 +0000\n");
        content.extend_from_slice(b"committer A <a@x> 1 +0000\n");
        content.extend_from_slice(&folded);
        content.extend_from_slice(b"\nbody\n");

        let headers = parse_commit_extension_headers(&content);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, b"gpgsig");
        assert_eq!(headers[0].1, value);
    }

    #[test]
    fn write_actor_line_rejects_overflowing_timestamp_offset_arithmetic() {
        let principal = Principal::new("A", "a@example.com");
        let mut out = Vec::new();

        let error = write_actor_line(&mut out, b"author", &principal, i64::MAX, 1)
            .expect_err("timestamp plus timezone offset must not overflow");

        assert!(
            matches!(&error, SerializeError::InvalidObject(message) if message.contains("overflows i64")),
            "expected InvalidObject overflow error, got: {error:?}",
        );
        assert!(
            out.is_empty(),
            "failed actor line must not emit partial bytes"
        );
    }

    #[test]
    fn write_actor_line_valid_timestamp_is_unchanged() {
        let principal = Principal::new("A", "a@example.com");
        let mut out = Vec::new();

        write_actor_line(&mut out, b"author", &principal, 1_700_000_000, -8 * 3600)
            .expect("valid timestamp should serialize");

        assert_eq!(out, b"author A <a@example.com> 1700000000 -0800\n");
    }
}
