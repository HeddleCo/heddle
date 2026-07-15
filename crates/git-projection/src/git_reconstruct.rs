// SPDX-License-Identifier: Apache-2.0
#![deny(clippy::cast_possible_truncation)]

//! Byte-exact git commit object serialization from a Heddle [`State`] (#566).
//!
//! Reconstructs the exact bytes `git cat-file commit <sha>` prints from the
//! de-lossy fidelity fields #565 captured, so that re-framing (§0) and
//! SHA-1-hashing the result reproduces the *original* commit's object id. This
//! is the consumer that makes #565's fields load-bearing and the step that lets
//! the git mirror be eliminated (#568): a commit can be rebuilt from Heddle
//! state alone — no stored git object.
//!
//! The wire format is specced byte-for-byte in
//! `.heddleco-orchestrator/briefs/spike-566-serializer-format.md`; the `§N`
//! references below point into it. Tag-object reconstruction
//! (`reconstruct_tag_bytes`) is deferred to #575, where annotated tags become
//! first-class content-addressed objects; lightweight tags need no object (just
//! a ref at the commit).

use objects::{
    object::{Principal, State},
    store::{ObjectStore, StoreError},
};
use repo::Repository as HeddleRepository;
use sley::{
    GitObjectType, ObjectFormat, ObjectId, Repository as SleyRepository,
    plumbing::sley_object::EncodedObject,
};

use crate::{
    git_core::{GitProjection, GitProjectionError, GitProjectionResult, SyncMapping, git_err},
    git_export::export_tree,
};

/// Frame an object's content for hashing per spike §0:
/// `<kind> <ascii-decimal-len>\0<content>`. A git object's id is the SHA-1 of
/// THIS buffer — never of the bare content (`git cat-file` strips the framing).
/// `<len>` is the byte length of `content` (after all folding/newlines), with no
/// leading zeros.
pub fn frame_git_object(kind: &str, content: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(kind.len() + 2 + 20 + content.len());
    framed.extend_from_slice(kind.as_bytes());
    framed.push(b' ');
    framed.extend_from_slice(content.len().to_string().as_bytes());
    framed.push(0);
    framed.extend_from_slice(content);
    framed
}

/// The git object id (SHA-1) of a commit whose reconstructed content bytes are
/// `content`: frame per §0, then hash. Equals the original commit SHA exactly
/// when `content` is byte-identical to the original object.
pub fn commit_object_id(content: &[u8]) -> ObjectId {
    sley::plumbing::sley_core::object_id_for_bytes(ObjectFormat::Sha1, "commit", content)
        .expect("SHA-1 commit object id over in-memory bytes cannot fail")
}

/// Reconstruct the byte-exact git commit object **content** (the bytes
/// `git cat-file commit` prints, WITHOUT the §0 framing) for `state`.
///
/// `repo` is any writable sley repo: the git tree OID is resolved by re-exporting
/// `state.tree` through [`export_tree`] (git trees are content-addressed, so the
/// resulting OID is independent of which repo it is written into — the round-trip
/// fidelity gate proves this path reproduces the original tree SHA). Parent OIDs
/// come from the import `mapping` (`StateId` → original git OID), in
/// `state.parents` order — order is part of a commit's identity (§1.2).
pub fn reconstruct_commit_bytes(
    heddle_repo: &HeddleRepository,
    repo: &SleyRepository,
    mapping: &SyncMapping,
    state: &State,
) -> GitProjectionResult<Vec<u8>> {
    let tree_oid = export_tree(heddle_repo, repo, &state.tree)?;
    let parent_oids = state
        .parents
        .iter()
        .map(|parent| {
            mapping
                .get_git(parent)
                .ok_or(GitProjectionError::StateNotFound(*parent))
        })
        .collect::<GitProjectionResult<Vec<_>>>()?;
    build_commit_content(state, &tree_oid, &parent_oids)
}

/// Frame + write a reconstructed commit object's `content` bytes into `repo`'s
/// object database, returning its git OID — the SHA-1 of the framed object (§0),
/// equal to the original commit's id exactly when `content` is byte-identical to
/// the original.
///
/// This is the write side of export-from-state (#567): export regenerates each
/// commit object from Heddle state and writes it here, rather than relying on the
/// git mirror still holding the verbatim imported bytes — the dependency #568
/// removes. Idempotent: sley's object writer hashes first and no-ops when the
/// object already exists, so re-writing a commit the mirror already carries (the
/// common case today) costs nothing.
pub fn write_commit_object(repo: &SleyRepository, content: &[u8]) -> GitProjectionResult<ObjectId> {
    repo.write_object(EncodedObject::new(GitObjectType::Commit, content.to_vec()))
        .map_err(git_err)
}

/// Assemble the commit content bytes from already-resolved OIDs. Pure (no repo,
/// no mapping) so the byte layout — header order, actor lines, header folding,
/// verbatim message — is unit-testable in isolation (§1/§2/§5/§6).
fn build_commit_content(
    state: &State,
    tree_oid: &ObjectId,
    parent_oids: &[ObjectId],
) -> GitProjectionResult<Vec<u8>> {
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

    // `author` then `committer` (§1.3/§5). Author time/tz come from the #565
    // `authored_at` + `authored_tz_offset` (with `created_at` as the native-commit
    // fallback); committer identity/time/tz from the distinct `committer`
    // Principal (author fallback when absent) + `created_at` + `committer_tz_offset`
    // — NOT a hardcoded `+0000`.
    let author_seconds = state.authored_at.unwrap_or(state.created_at).timestamp();
    write_actor_line(
        &mut out,
        b"author",
        &state.attribution.principal,
        author_seconds,
        state.authored_tz_offset,
    )?;
    let committer = state
        .committer
        .as_ref()
        .unwrap_or(&state.attribution.principal);
    write_actor_line(
        &mut out,
        b"committer",
        committer,
        state.created_at.timestamp(),
        state.committer_tz_offset,
    )?;

    // Extension headers (`encoding`/`gpgsig`/`mergetag`/unknown) at their captured
    // ordinal, multi-line values re-folded (§1.4/§2). The ordered `Vec` is the
    // source of truth — gpgsig and mergetag are just entries here, never
    // special-cased; when both are present git emits mergetag before gpgsig and
    // the captured order already encodes that.
    for (name, value) in &state.extra_headers {
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
    if let Some(message) = &state.raw_message {
        out.extend_from_slice(message);
    }

    Ok(out)
}

/// `<label> <name> <<email>> <unix-seconds> <±HHMM>\n` (§5).
fn write_actor_line(
    out: &mut Vec<u8>,
    label: &[u8],
    who: &Principal,
    seconds: i64,
    tz_offset_secs: i32,
) -> GitProjectionResult<()> {
    let seconds = checked_actor_timestamp(label, seconds, tz_offset_secs)?;
    out.extend_from_slice(label);
    out.push(b' ');
    out.extend_from_slice(who.name.as_bytes());
    out.extend_from_slice(b" <");
    out.extend_from_slice(who.email.as_bytes());
    out.extend_from_slice(b"> ");
    out.extend_from_slice(seconds.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(format_tz_offset(tz_offset_secs).as_bytes());
    out.push(b'\n');
    Ok(())
}

fn checked_actor_timestamp(
    label: &[u8],
    seconds: i64,
    tz_offset_secs: i32,
) -> GitProjectionResult<i64> {
    // Git serializes UTC seconds plus a timezone offset. Validate the local
    // seconds implied by that pair so malformed fidelity data cannot overflow
    // reconstruct-time timestamp arithmetic.
    seconds
        .checked_add(i64::from(tz_offset_secs))
        .map(|_| seconds)
        .ok_or_else(|| {
            let label = String::from_utf8_lossy(label);
            GitProjectionError::Store(StoreError::InvalidObject(format!(
                "{label} timestamp {seconds} with timezone offset {tz_offset_secs} overflows i64"
            )))
        })
}

/// Render a timezone offset — stored as **seconds** east of UTC (#565's `i32`
/// unit) — as git's `±HHMM` (§5). The sign is
/// always present; zero is `+0000` (git never emits `-0000` for a real commit);
/// odd offsets like `-0830` / `+1245` survive verbatim.
fn format_tz_offset(offset_secs: i32) -> String {
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

impl GitProjection<'_> {
    /// Open (initializing if necessary) a writable sley repo suitable for
    /// reconstruction's tree-OID resolution. Any writable odb works — git trees
    /// are content-addressed — so the bridge's own mirror is reused.
    pub fn reconstruction_repo(&mut self) -> GitProjectionResult<SleyRepository> {
        self.init_mirror()?;
        self.open_git_repo()
    }

    /// Reconstruct the byte-exact commit content for `state` against `repo` (see
    /// [`reconstruct_commit_bytes`]), using the bridge's import-built mapping for
    /// parent OIDs.
    pub fn reconstruct_commit_bytes(
        &self,
        repo: &SleyRepository,
        state: &State,
    ) -> GitProjectionResult<Vec<u8>> {
        reconstruct_commit_bytes(self.heddle_repo, repo, &self.mapping, state)
    }

    /// Reconstruct `state`'s commit object from Heddle state and WRITE it into
    /// `repo`'s object database, returning its git OID (see [`write_commit_object`]).
    /// The export's commit-minting step (#567): the object is regenerated from
    /// state, so it lands at the original SHA without the mirror needing to hold
    /// the verbatim bytes.
    pub fn reconstruct_and_write_commit(
        &self,
        repo: &SleyRepository,
        state: &State,
    ) -> GitProjectionResult<ObjectId> {
        let content = self.reconstruct_commit_bytes(repo, state)?;
        write_commit_object(repo, &content)
    }

    /// Reconstruct the commit currently mapped to the git object `sha` (40-hex),
    /// or `None` if no Heddle state maps to it. Convenience for callers keyed by
    /// the original git OID — e.g. the #566 conformance gate, which compares the
    /// reconstruction of each original commit against its captured golden bytes.
    pub fn reconstruct_commit_for_git_sha(
        &self,
        repo: &SleyRepository,
        sha: &str,
    ) -> GitProjectionResult<Option<Vec<u8>>> {
        let oid = ObjectId::from_hex(ObjectFormat::Sha1, sha).map_err(git_err)?;
        let Some(state_id) = self.mapping.get_heddle(oid) else {
            return Ok(None);
        };
        let Some(state) = self.heddle_repo.store().get_state(&state_id)? else {
            return Ok(None);
        };
        Ok(Some(reconstruct_commit_bytes(
            self.heddle_repo,
            repo,
            &self.mapping,
            &state,
        )?))
    }

    /// Reconstruct the commit mapped to git object `sha` and WRITE it into `repo`,
    /// returning the written OID (or `None` if no Heddle state maps to `sha`).
    /// Combines [`Self::reconstruct_commit_for_git_sha`] with the odb write so the
    /// #567 export-from-state path is exercisable against an arbitrary repo —
    /// notably a FRESH one that never received the verbatim imported bytes, which
    /// is how the export gate proves the object is regenerated from state, not
    /// copied from the mirror.
    pub fn reconstruct_and_write_commit_for_git_sha(
        &self,
        repo: &SleyRepository,
        sha: &str,
    ) -> GitProjectionResult<Option<ObjectId>> {
        let oid = ObjectId::from_hex(ObjectFormat::Sha1, sha).map_err(git_err)?;
        let Some(state_id) = self.mapping.get_heddle(oid) else {
            return Ok(None);
        };
        let Some(state) = self.heddle_repo.store().get_state(&state_id)? else {
            return Ok(None);
        };
        Ok(Some(self.reconstruct_and_write_commit(repo, &state)?))
    }
}

#[cfg(test)]
mod tests {
    use objects::object::parse_commit_extension_headers;

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
    }

    #[test]
    fn frame_prepends_kind_len_nul() {
        assert_eq!(frame_git_object("commit", b"abc"), b"commit 3\0abc");
        assert_eq!(frame_git_object("commit", b""), b"commit 0\0");
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
            matches!(&error, GitProjectionError::Store(StoreError::InvalidObject(message)) if message.contains("overflows i64")),
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
