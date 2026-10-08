// SPDX-License-Identifier: Apache-2.0
//! Git layout of a tree imported from a non-canonical Git tree (heddle#2018).
//!
//! Git accepts trees that Git itself never writes: zero-padded or odd modes
//! (`040000`, `100664`) and entries out of Git's canonical order. Heddle keeps
//! the canonical meaning of such a tree (a `100664` entry is a regular file) and
//! records the source layout next to it, so export rebuilds the source tree
//! byte-for-byte and the commit id and every descendant id survive.
//!
//! Both pieces are optional and present only when the source differs from what
//! Git would write. Trees that do not use them hash and encode exactly as they
//! did before the extension existed.
//!
//! On the wire the extension is a trailer after an entry's target, announced by
//! two otherwise-unused bits of the entry's mode byte. The same trailer and the
//! same flag bits go into the tree hash preimage, so both fields are part of
//! the tree's identity and a fetch verifies them with no new transport.

use std::{cmp::Ordering, num::NonZeroU32};

use sley_core::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{EntryType, TreeEntry, TreeError};

/// Mode-byte flag: a raw Git mode trailer follows the entry's target.
pub(crate) const ENTRY_FLAG_RAW_GIT_MODE: u8 = 0x80;
/// Mode-byte flag: a source-position trailer follows the entry's target.
pub(crate) const ENTRY_FLAG_SOURCE_POSITION: u8 = 0x40;
/// Every flag bit. The remaining bits hold the mode byte (always `< 0x40`).
pub(crate) const ENTRY_LAYOUT_FLAGS: u8 = ENTRY_FLAG_RAW_GIT_MODE | ENTRY_FLAG_SOURCE_POSITION;
const RAW_GIT_MODE_TRAILER_LEN: usize = 5;
const SOURCE_POSITION_TRAILER_LEN: usize = 4;
/// Git's file-type bits (`S_IFMT`).
const GIT_TYPE_MASK: u32 = 0o170000;

/// A Git tree-entry mode exactly as written in a source tree.
///
/// Git writes a mode as octal digits without leading zeros. Git also *reads*
/// zero-padded digits and permission bits it would never write. This keeps
/// both: the numeric value plus the count of leading zeros reproduce the
/// source digits exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RawGitMode {
    // Never zero: a mode needs a significant digit. The niche keeps
    // `Option<RawGitMode>` the size of `RawGitMode` on every tree entry.
    value: NonZeroU32,
    leading_zeros: u8,
}

impl RawGitMode {
    /// Parse the octal digits of a tree-entry mode, as they appear before the
    /// space in a raw Git tree.
    pub fn parse(digits: &[u8]) -> Result<Self, TreeError> {
        let invalid = || {
            TreeError::InvalidStructure(format!(
                "invalid git tree mode {:?}",
                String::from_utf8_lossy(digits)
            ))
        };
        let leading = digits.iter().take_while(|digit| **digit == b'0').count();
        let significant = &digits[leading..];
        if significant.is_empty() || !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
            return Err(invalid());
        }
        let mut value = 0u32;
        for digit in significant {
            value = value
                .checked_mul(8)
                .and_then(|value| value.checked_add(u32::from(digit - b'0')))
                .ok_or_else(invalid)?;
        }
        Ok(Self {
            value: NonZeroU32::new(value).ok_or_else(invalid)?,
            leading_zeros: u8::try_from(leading).map_err(|_| invalid())?,
        })
    }

    /// The canonical mode Git writes for an entry of `entry_type`. `None` for
    /// a spoollink, which is not a Git object.
    pub fn canonical(entry_type: EntryType, executable: bool) -> Option<Self> {
        let value = match entry_type {
            EntryType::Tree => 0o040000,
            EntryType::Blob if executable => 0o100755,
            EntryType::Blob => 0o100644,
            EntryType::Symlink => 0o120000,
            EntryType::Gitlink => 0o160000,
            EntryType::Spoollink => return None,
        };
        Some(Self {
            value: NonZeroU32::new(value)?,
            leading_zeros: 0,
        })
    }

    /// The numeric mode.
    pub fn value(self) -> u32 {
        self.value.get()
    }

    /// The entry kind Git reads this mode as, or `None` for a mode Heddle
    /// cannot represent. Follows Git's own reading: only the file-type bits
    /// pick the kind.
    pub fn entry_type(self) -> Option<EntryType> {
        let value = self.value();
        if value & !0o177777 != 0 {
            return None;
        }
        match value & GIT_TYPE_MASK {
            0o040000 => Some(EntryType::Tree),
            0o100000 => Some(EntryType::Blob),
            0o120000 => Some(EntryType::Symlink),
            0o160000 => Some(EntryType::Gitlink),
            _ => None,
        }
    }

    /// Whether Git checks a regular file with this mode out as executable:
    /// the owner execute bit, as in Git's `canon_mode`.
    pub fn is_executable(self) -> bool {
        self.entry_type() == Some(EntryType::Blob) && self.value() & 0o100 != 0
    }

    /// Append the exact source digits.
    pub fn write_digits(self, out: &mut Vec<u8>) {
        out.extend(std::iter::repeat_n(b'0', usize::from(self.leading_zeros)));
        out.extend_from_slice(format!("{:o}", self.value()).as_bytes());
    }

    fn is_canonical_for(self, entry_type: EntryType, executable: bool) -> bool {
        Self::canonical(entry_type, executable) == Some(self)
    }
}

impl TreeEntry {
    /// Check that `mode` describes this entry and is worth recording: it must
    /// read as this entry's kind (and executable bit), and differ from the
    /// mode Git would write for it.
    pub(crate) fn check_raw_git_mode(&self, mode: RawGitMode) -> Result<(), TreeError> {
        let entry_type = self.entry_type();
        let executable = self.is_executable();
        if mode.entry_type() != Some(entry_type)
            || (entry_type == EntryType::Blob && mode.is_executable() != executable)
        {
            return Err(TreeError::InvalidStructure(format!(
                "git mode {:o} does not describe {:?} entry '{}'",
                mode.value(),
                entry_type,
                self.name()
            )));
        }
        if mode.is_canonical_for(entry_type, executable) {
            return Err(TreeError::InvalidStructure(format!(
                "entry '{}' records its canonical git mode as a raw mode",
                self.name()
            )));
        }
        Ok(())
    }

    /// The mode-byte flags announcing this entry's layout trailer.
    pub(crate) fn layout_flags(&self, source_position: Option<u32>) -> u8 {
        let mut flags = 0;
        if self.raw_git_mode().is_some() {
            flags |= ENTRY_FLAG_RAW_GIT_MODE;
        }
        if source_position.is_some() {
            flags |= ENTRY_FLAG_SOURCE_POSITION;
        }
        flags
    }

    /// Emit this entry's layout trailer: raw mode (`u32` LE value, `u8`
    /// leading zeros) then source position (`u32` LE), each only when present.
    pub(crate) fn write_layout_trailer(
        &self,
        source_position: Option<u32>,
        mut emit: impl FnMut(&[u8]),
    ) {
        if let Some(mode) = self.raw_git_mode() {
            emit(&mode.value().to_le_bytes());
            emit(&[mode.leading_zeros]);
        }
        if let Some(position) = source_position {
            emit(&position.to_le_bytes());
        }
    }
}

/// One entry of a raw Git tree object, with its mode exactly as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitTreeEntryRef<'a> {
    pub mode: RawGitMode,
    pub name: &'a [u8],
    pub oid: GitObjectId,
}

/// Parse a raw Git tree body (`<mode> SP <name> NUL <oid>`...) in source
/// order, keeping each mode's digits. Git porcelain parsers turn the digits
/// into a number, which loses `040000` versus `40000`; importers that must
/// reproduce the source tree read it through this instead.
pub fn parse_git_tree(
    format: GitObjectFormat,
    body: &[u8],
) -> Result<Vec<GitTreeEntryRef<'_>>, TreeError> {
    let malformed = |what: &str| TreeError::InvalidStructure(format!("malformed git tree: {what}"));
    let mut entries = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| malformed("unterminated mode"))?;
        let mode = RawGitMode::parse(&rest[..space])?;
        rest = &rest[space + 1..];
        let nul = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| malformed("unterminated name"))?;
        if nul == 0 {
            return Err(malformed("empty name"));
        }
        let name = &rest[..nul];
        rest = &rest[nul + 1..];
        let oid_len = format.raw_len();
        if rest.len() < oid_len {
            return Err(malformed("truncated object id"));
        }
        let oid = GitObjectId::from_raw(format, &rest[..oid_len])
            .map_err(|error| malformed(&error.to_string()))?;
        rest = &rest[oid_len..];
        entries.push(GitTreeEntryRef { mode, name, oid });
    }
    Ok(entries)
}

/// Byte length of the trailer announced by `flags`.
pub(crate) fn layout_trailer_len(flags: u8) -> usize {
    let mut len = 0;
    if flags & ENTRY_FLAG_RAW_GIT_MODE != 0 {
        len += RAW_GIT_MODE_TRAILER_LEN;
    }
    if flags & ENTRY_FLAG_SOURCE_POSITION != 0 {
        len += SOURCE_POSITION_TRAILER_LEN;
    }
    len
}

/// Attach the trailer announced by `flags` (exactly [`layout_trailer_len`]
/// bytes) to a decoded entry and return its source position. Rejects a raw
/// mode that is canonical or does not describe the entry, so every tree has
/// exactly one encoding.
pub(crate) fn apply_layout_trailer(
    entry: TreeEntry,
    flags: u8,
    trailer: &[u8],
) -> Result<(TreeEntry, Option<u32>), TreeError> {
    let malformed = || TreeError::InvalidStructure("malformed tree entry layout trailer".into());
    if trailer.len() != layout_trailer_len(flags) {
        return Err(malformed());
    }
    let mut rest = trailer;
    let mut entry = entry;
    if flags & ENTRY_FLAG_RAW_GIT_MODE != 0 {
        let (mode, tail) = rest.split_at(RAW_GIT_MODE_TRAILER_LEN);
        let value = u32::from_le_bytes(mode[..4].try_into().map_err(|_| malformed())?);
        let mode = RawGitMode {
            value: NonZeroU32::new(value).ok_or_else(malformed)?,
            leading_zeros: mode[4],
        };
        entry.check_raw_git_mode(mode)?;
        entry = entry.with_checked_raw_git_mode(mode);
        rest = tail;
    }
    let position = if flags & ENTRY_FLAG_SOURCE_POSITION != 0 {
        Some(u32::from_le_bytes(
            rest.try_into().map_err(|_| malformed())?,
        ))
    } else {
        None
    };
    Ok((entry, position))
}

/// Split a stored mode byte into its layout flags and the plain mode byte.
pub(crate) fn split_layout_flags(byte: u8) -> (u8, u8) {
    (byte & ENTRY_LAYOUT_FLAGS, byte & !ENTRY_LAYOUT_FLAGS)
}

/// Git's canonical tree order: byte order of the names, where a tree's name
/// compares as if it ended in `/` (Git's `base_name_compare`).
pub(crate) fn git_canonical_order(left: &TreeEntry, right: &TreeEntry) -> Ordering {
    let left_name = left.name().as_bytes();
    let right_name = right.name().as_bytes();
    let shared = left_name.len().min(right_name.len());
    left_name[..shared]
        .cmp(&right_name[..shared])
        .then_with(|| {
            let terminator = |entry: &TreeEntry| if entry.is_tree() { b'/' } else { 0 };
            let left_next = left_name.get(shared).copied().unwrap_or(terminator(left));
            let right_next = right_name.get(shared).copied().unwrap_or(terminator(right));
            left_next.cmp(&right_next)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ContentHash;

    fn mode(digits: &str) -> RawGitMode {
        RawGitMode::parse(digits.as_bytes()).expect("valid mode")
    }

    fn digits(mode: RawGitMode) -> String {
        let mut out = Vec::new();
        mode.write_digits(&mut out);
        String::from_utf8(out).expect("ascii")
    }

    #[test]
    fn an_absent_raw_mode_costs_no_tag() {
        assert_eq!(
            std::mem::size_of::<Option<RawGitMode>>(),
            std::mem::size_of::<RawGitMode>()
        );
    }

    #[test]
    fn raw_modes_reproduce_their_source_digits() {
        for source in ["040000", "100664", "0100644", "40000", "120777", "160000"] {
            assert_eq!(digits(mode(source)), source);
        }
    }

    #[test]
    fn raw_modes_read_as_git_reads_them() {
        assert_eq!(mode("040000").entry_type(), Some(EntryType::Tree));
        assert_eq!(mode("100664").entry_type(), Some(EntryType::Blob));
        assert!(!mode("100664").is_executable());
        assert!(mode("100744").is_executable());
        // Git reads only the owner execute bit.
        assert!(!mode("100645").is_executable());
        assert_eq!(mode("120777").entry_type(), Some(EntryType::Symlink));
        assert_eq!(mode("160000").entry_type(), Some(EntryType::Gitlink));
        assert_eq!(mode("644").entry_type(), None);
        assert_eq!(mode("1100644").entry_type(), None);
    }

    #[test]
    fn malformed_mode_digits_are_rejected() {
        for source in ["", "000", "1006a4", "100 644", "77777777777777"] {
            assert!(
                RawGitMode::parse(source.as_bytes()).is_err(),
                "{source:?} must be rejected"
            );
        }
    }

    #[test]
    fn git_tree_parser_keeps_mode_digits_and_source_order() {
        let mut body = Vec::new();
        for (mode, name, fill) in [("100664", "b", 1u8), ("040000", "a", 2u8)] {
            body.extend_from_slice(mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(&[fill; 20]);
        }
        let entries = parse_git_tree(GitObjectFormat::Sha1, &body).unwrap();
        let names: Vec<&[u8]> = entries.iter().map(|entry| entry.name).collect();
        assert_eq!(names, [b"b".as_slice(), b"a".as_slice()]);
        assert_eq!(digits(entries[1].mode), "040000");
        assert!(parse_git_tree(GitObjectFormat::Sha1, &body[..body.len() - 1]).is_err());
    }

    #[test]
    fn git_order_sorts_trees_as_if_they_end_in_a_slash() {
        let hash = ContentHash::compute(b"x");
        let dir = TreeEntry::directory("lib", hash).unwrap();
        let file = TreeEntry::file("lib.rs", hash, false).unwrap();
        let plain = TreeEntry::file("lib", hash, false).unwrap();
        // Native byte order puts `lib` first; Git puts `lib.rs` before `lib/`.
        assert_eq!(git_canonical_order(&file, &dir), Ordering::Less);
        assert_eq!(git_canonical_order(&plain, &file), Ordering::Less);
    }
}

#[cfg(test)]
#[path = "tree_git_layout_tests.rs"]
mod layout_tests;
