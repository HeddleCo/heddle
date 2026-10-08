// SPDX-License-Identifier: Apache-2.0
//! Tree entry names that alias a repository metadata name (heddle#2028).
//!
//! Checking out an entry named `.git` or `.heddle` writes into the metadata
//! directory, where hooks execute code, and a `.gitmodules` symlink lets a
//! tree point Git's submodule configuration anywhere. A filesystem can also
//! resolve other spellings to the same name, so the checks follow Git's own
//! rules (`verify_path`, `is_ntfs_dotgit`, `is_ntfs_dotgitmodules`,
//! `is_hfs_dotgit`; CVE-2014-9390, CVE-2019-1353, CVE-2018-11235):
//!
//! - any case (`.GIT`), for case-insensitive filesystems;
//! - trailing dots and spaces (`.git.`, `.git `), which Windows strips;
//! - an NTFS alternate data stream (`.git::$INDEX_ALLOCATION`);
//! - the NTFS 8.3 short name: `GIT~1` as Git checks it, `HEDDLE~1` to `~4`
//!   and `GITMOD~1` to `~4` (Git's rule for names longer than six
//!   characters), plus the hashed fallback `GI7EBA~N` for `.gitmodules`;
//! - Unicode code points HFS+ ignores (`.g\u{200c}it`).
//!
//! Where each name is reserved:
//!
//! - `.git` at every depth, as Git does: a nested `.git` is a repository.
//! - `.heddle` at the root of a tree only. A nested `.heddle` directory is
//!   ordinary content (fixtures such as `examples/calculator/.heddle/` are
//!   tracked and captured), so only the root one is the live metadata
//!   directory.
//! - `.gitmodules` at every depth, but only as a symlink.
//!
//! These are checked where a tree meets the outside world: Git import
//! refuses them, and checkout never writes them. Decoding a stored tree does
//! not check them, because repositories captured before heddle#2028 can hold
//! a nested `.git` (a vendored clone or a submodule's gitfile) and must stay
//! readable.

use std::fmt;

/// A name that tree entries must not alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetadataName {
    /// Git's `.git` directory, reserved at every depth.
    Git,
    /// Heddle's `.heddle` directory, reserved at the root of a tree.
    Heddle,
    /// Git's `.gitmodules` file, reserved as a symlink at every depth.
    GitModules,
}

impl MetadataName {
    /// The reserved name, e.g. `.git`.
    pub fn as_str(self) -> &'static str {
        match self {
            MetadataName::Git => ".git",
            MetadataName::Heddle => ".heddle",
            MetadataName::GitModules => ".gitmodules",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            MetadataName::Git => ".git metadata directory",
            MetadataName::Heddle => ".heddle metadata directory",
            MetadataName::GitModules => ".gitmodules file, which must not be a symlink",
        }
    }

    /// The name without its leading dot, as ASCII lowercase.
    fn stem(self) -> &'static [u8] {
        match self {
            MetadataName::Git => b"git",
            MetadataName::Heddle => b"heddle",
            MetadataName::GitModules => b"gitmodules",
        }
    }
}

/// How a reserved name reaches the metadata name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetadataAlias {
    /// The exact name.
    Exact,
    /// The name in another case, on case-insensitive filesystems.
    Case,
    /// The name followed by dots or spaces, which Windows strips.
    TrailingDotsOrSpaces,
    /// The name followed by `\`, a path separator on Windows.
    BackslashSeparator,
    /// An NTFS alternate data stream of the name, e.g. `.git::$DATA`.
    NtfsStream,
    /// An NTFS 8.3 short name, e.g. `GIT~1`.
    NtfsShortName,
    /// The name with Unicode code points HFS+ ignores.
    HfsIgnorable,
}

/// Why a tree entry name is reserved: the name it reaches, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReservedMetadataName {
    pub name: MetadataName,
    pub alias: MetadataAlias,
}

impl fmt::Display for ReservedMetadataName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = self.name.describe();
        match self.alias {
            MetadataAlias::Exact => write!(f, "is the {target}"),
            MetadataAlias::Case => {
                write!(f, "names the {target} on case-insensitive filesystems")
            }
            MetadataAlias::TrailingDotsOrSpaces => write!(
                f,
                "names the {target} on Windows, which strips trailing dots and spaces"
            ),
            MetadataAlias::BackslashSeparator => write!(
                f,
                "is a path into the {target} on Windows, where '\\' separates paths"
            ),
            MetadataAlias::NtfsStream => {
                write!(f, "names an NTFS alternate data stream of the {target}")
            }
            MetadataAlias::NtfsShortName => {
                write!(f, "is an NTFS 8.3 short name of the {target}")
            }
            MetadataAlias::HfsIgnorable => write!(
                f,
                "names the {target} on HFS+, which ignores some Unicode code points"
            ),
        }
    }
}

/// A path component that is a reserved metadata name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedPathComponent {
    /// Zero-based index of the component among the path's non-empty,
    /// non-`.` components.
    pub index: usize,
    /// The component, lossily decoded for display.
    pub component: String,
    pub reason: ReservedMetadataName,
}

impl ReservedPathComponent {
    /// Whether this is the exact root `.git` or `.heddle`: the repository's
    /// own metadata, which every walk skips silently. Anything else skipped
    /// for this reason deserves a warning naming it.
    pub fn is_own_metadata(&self) -> bool {
        self.index == 0
            && self.reason.alias == MetadataAlias::Exact
            && matches!(self.reason.name, MetadataName::Git | MetadataName::Heddle)
    }
}

impl fmt::Display for ReservedPathComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "'{}' {}", self.component, self.reason)
    }
}

/// Classify `name` as an alias of `.git` or `.heddle`, regardless of depth.
///
/// This is the name-level predicate. Most callers want
/// [`reserved_tree_entry_name`] or [`reserved_path_component`], which also
/// apply the depth rule (`.heddle` is reserved only at the root) and the
/// `.gitmodules` symlink rule.
///
/// `name` is raw bytes so Git tree names that are not UTF-8 can be checked.
pub fn reserved_metadata_name(name: &[u8]) -> Option<ReservedMetadataName> {
    [MetadataName::Git, MetadataName::Heddle]
        .into_iter()
        .find_map(|target| reserved_as(name, target))
}

/// Whether `name` aliases `.git` or `.heddle`, regardless of depth.
pub fn is_reserved_metadata_name(name: impl AsRef<[u8]>) -> bool {
    reserved_metadata_name(name.as_ref()).is_some()
}

/// Classify the tree entry `name`:
///
/// - `.git` aliases at any depth;
/// - `.heddle` aliases only when `at_root` (the entry is a direct child of a
///   commit's or State's root tree, or of a checkout's destination);
/// - `.gitmodules` aliases at any depth when `symlink`.
pub fn reserved_tree_entry_name(
    name: &[u8],
    at_root: bool,
    symlink: bool,
) -> Option<ReservedMetadataName> {
    reserved_as(name, MetadataName::Git)
        .or_else(|| {
            at_root
                .then(|| reserved_as(name, MetadataName::Heddle))
                .flatten()
        })
        .or_else(|| {
            symlink
                .then(|| reserved_as(name, MetadataName::GitModules))
                .flatten()
        })
}

/// The first component of the repository-relative `path` that is reserved
/// by [`reserved_tree_entry_name`], if any. `leaf_symlink` says the last
/// component is a symlink.
///
/// Components are split on both `/` and `\`. Empty and `.` components are
/// skipped, so `./.heddle/x` is a root `.heddle`.
pub fn reserved_path_component(path: &[u8], leaf_symlink: bool) -> Option<ReservedPathComponent> {
    let mut components = path
        .split(|&b| b == b'/' || b == b'\\')
        .filter(|component| !component.is_empty() && *component != b".")
        .enumerate()
        .peekable();
    while let Some((index, component)) = components.next() {
        let symlink = leaf_symlink && components.peek().is_none();
        if let Some(reason) = reserved_tree_entry_name(component, index == 0, symlink) {
            return Some(ReservedPathComponent {
                index,
                component: String::from_utf8_lossy(component).into_owned(),
                reason,
            });
        }
    }
    None
}

fn reserved_as(name: &[u8], target: MetadataName) -> Option<ReservedMetadataName> {
    ntfs_alias(name, target)
        .or_else(|| hfs_alias(name, target))
        .map(|alias| ReservedMetadataName {
            name: target,
            alias,
        })
}

/// Git's `is_ntfs_dotgit` and `is_ntfs_dot_generic`, for `target`: the long
/// name `.<stem>` or a short name in any case, then any run of dots and
/// spaces, then the end of the name, a path separator, or `:` (an alternate
/// data stream).
fn ntfs_alias(name: &[u8], target: MetadataName) -> Option<MetadataAlias> {
    let stem = target.stem();
    let (rest, short_name) = match name
        .strip_prefix(b".")
        .and_then(|rest| strip_prefix_ignore_ascii_case(rest, stem))
    {
        Some(rest) => (rest, false),
        None => (short_name_rest(name, target)?, true),
    };
    let trimmed_len = rest.iter().take_while(|&&b| b == b'.' || b == b' ').count();
    let trailing = &rest[..trimmed_len];
    let alias = match rest.get(trimmed_len).copied() {
        None | Some(b'/' | b'\\' | b':') if short_name => MetadataAlias::NtfsShortName,
        None | Some(b'/') if !trailing.is_empty() => MetadataAlias::TrailingDotsOrSpaces,
        // `name` is `.` + the stem in some case + the end or `/`.
        None | Some(b'/') if name[1..=stem.len()] == *stem => MetadataAlias::Exact,
        None | Some(b'/') => MetadataAlias::Case,
        Some(b':') => MetadataAlias::NtfsStream,
        Some(b'\\') => MetadataAlias::BackslashSeparator,
        Some(_) => return None,
    };
    Some(alias)
}

/// What follows an NTFS 8.3 short name of `target` at the start of `name`.
fn short_name_rest(name: &[u8], target: MetadataName) -> Option<&[u8]> {
    match target {
        // Git checks only `git~1` for `.git` (`is_ntfs_dotgit`).
        MetadataName::Git => strip_prefix_ignore_ascii_case(name, b"git~1"),
        // Longer names: the first six characters, then `~1` to `~4`.
        MetadataName::Heddle | MetadataName::GitModules => {
            numbered_short_name_rest(name, &target.stem()[..6]).or_else(|| {
                (target == MetadataName::GitModules)
                    .then(|| hashed_short_name_rest(name, b"gi7eba"))
                    .flatten()
            })
        }
    }
}

/// `<prefix>~1` to `<prefix>~4`, the regular short names Windows gives the
/// first four long names sharing a six-character prefix.
fn numbered_short_name_rest<'a>(name: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let rest = strip_prefix_ignore_ascii_case(name, prefix)?.strip_prefix(b"~")?;
    match rest.first() {
        Some(b'1'..=b'4') => Some(&rest[1..]),
        _ => None,
    }
}

/// The fall-back short name Windows derives from a hash once the numbered
/// ones run out: the first eight characters are a leading part of `prefix`,
/// `~`, a digit 1-9, then digits (Git's `is_ntfs_dot_generic`).
fn hashed_short_name_rest<'a>(name: &'a [u8], prefix: &[u8; 6]) -> Option<&'a [u8]> {
    let mut saw_tilde = false;
    let mut index = 0;
    while index < 8 {
        let byte = *name.get(index)?;
        if saw_tilde {
            if !byte.is_ascii_digit() {
                return None;
            }
        } else if byte == b'~' {
            index += 1;
            if !matches!(name.get(index), Some(b'1'..=b'9')) {
                return None;
            }
            saw_tilde = true;
        } else if index >= 6 || !byte.is_ascii() || byte.to_ascii_lowercase() != prefix[index] {
            return None;
        }
        index += 1;
    }
    Some(&name[8..])
}

fn strip_prefix_ignore_ascii_case<'a>(name: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    (name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &name[prefix.len()..])
}

/// Git's `is_hfs_dotgit`, for `target`: after dropping the code points HFS+
/// ignores, `.<stem>` in any case, then the end of the name or `/`.
/// Malformed UTF-8 ends the name, as it does in Git.
fn hfs_alias(name: &[u8], target: MetadataName) -> Option<MetadataAlias> {
    let valid = match std::str::from_utf8(name) {
        Ok(valid) => valid,
        Err(error) => std::str::from_utf8(&name[..error.valid_up_to()]).ok()?,
    };
    let mut chars = valid
        .chars()
        .take_while(|&c| c != '/')
        .filter(|&c| !is_hfs_ignorable(c));
    if chars.next() != Some('.') {
        return None;
    }
    for &expected in target.stem() {
        let c = chars.next()?;
        if !c.is_ascii() || c.to_ascii_lowercase() as u32 != u32::from(expected) {
            return None;
        }
    }
    chars
        .next()
        .is_none()
        .then_some(MetadataAlias::HfsIgnorable)
}

/// The code points HFS+ drops when it compares names (Git's
/// `next_hfs_char`).
fn is_hfs_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{200c}'
            | '\u{200d}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'
            ..='\u{202e}'
            | '\u{206a}'
            ..='\u{206f}'
            | '\u{feff}'
    )
}

#[cfg(test)]
#[path = "reserved_name_tests.rs"]
mod tests;
