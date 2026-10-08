// SPDX-License-Identifier: Apache-2.0
//! Tree entry names that alias a repository metadata directory
//! (heddle#2028).
//!
//! Checking out an entry named `.git` or `.heddle` writes into the metadata
//! directory, where hooks execute code. A filesystem can also resolve other
//! spellings to the same directory, so the check follows Git's own rules
//! (`verify_path`, `is_ntfs_dotgit`, `is_hfs_dotgit`; CVE-2014-9390,
//! CVE-2019-1353):
//!
//! - any case (`.GIT`), for case-insensitive filesystems;
//! - trailing dots and spaces (`.git.`, `.git `), which Windows strips;
//! - an NTFS alternate data stream (`.git::$INDEX_ALLOCATION`);
//! - the NTFS 8.3 short name (`GIT~1`, `HEDDLE~1`);
//! - Unicode code points HFS+ ignores (`.g\u{200c}it`).
//!
//! Where each name is reserved:
//!
//! - `.git` at every depth, as Git does: a nested `.git` is a repository.
//! - `.heddle` at the root of a tree only. A nested `.heddle` directory is
//!   ordinary content (fixtures such as `examples/calculator/.heddle/` are
//!   tracked and captured), so only the root one is the live metadata
//!   directory.
//!
//! [`reserved_tree_entry_name`] and [`reserved_path_component`] apply those
//! rules; [`reserved_metadata_name`] classifies a single name for either
//! directory regardless of depth.

use std::fmt;

/// A repository metadata directory that tree entries must not alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetadataDir {
    /// Git's `.git`, reserved at every depth.
    Git,
    /// Heddle's `.heddle`, reserved at the root of a tree.
    Heddle,
}

impl MetadataDir {
    /// The directory's name, e.g. `.git`.
    pub fn as_str(self) -> &'static str {
        match self {
            MetadataDir::Git => ".git",
            MetadataDir::Heddle => ".heddle",
        }
    }

    /// The name without its leading dot, as ASCII lowercase.
    fn stem(self) -> &'static [u8] {
        match self {
            MetadataDir::Git => b"git",
            MetadataDir::Heddle => b"heddle",
        }
    }
}

/// How a reserved name reaches the metadata directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetadataAlias {
    /// The directory's exact name.
    Exact,
    /// The name in another case, on case-insensitive filesystems.
    Case,
    /// The name followed by dots or spaces, which Windows strips.
    TrailingDotsOrSpaces,
    /// The name followed by `\`, a path separator on Windows.
    BackslashSeparator,
    /// An NTFS alternate data stream of the directory, e.g. `.git::$DATA`.
    NtfsStream,
    /// The NTFS 8.3 short name, e.g. `GIT~1`.
    NtfsShortName,
    /// The name with Unicode code points HFS+ ignores.
    HfsIgnorable,
}

/// Why a tree entry name is reserved: the directory it reaches, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReservedMetadataName {
    pub dir: MetadataDir,
    pub alias: MetadataAlias,
}

impl fmt::Display for ReservedMetadataName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dir = self.dir.as_str();
        match self.alias {
            MetadataAlias::Exact => write!(f, "is the {dir} metadata directory"),
            MetadataAlias::Case => write!(
                f,
                "names the {dir} metadata directory on case-insensitive filesystems"
            ),
            MetadataAlias::TrailingDotsOrSpaces => write!(
                f,
                "names the {dir} metadata directory on Windows, which strips trailing dots and spaces"
            ),
            MetadataAlias::BackslashSeparator => write!(
                f,
                "is a path into the {dir} metadata directory on Windows, where '\\' separates paths"
            ),
            MetadataAlias::NtfsStream => write!(
                f,
                "names an NTFS alternate data stream of the {dir} metadata directory"
            ),
            MetadataAlias::NtfsShortName => write!(
                f,
                "is the NTFS 8.3 short name of the {dir} metadata directory"
            ),
            MetadataAlias::HfsIgnorable => write!(
                f,
                "names the {dir} metadata directory on HFS+, which ignores some Unicode code points"
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

impl fmt::Display for ReservedPathComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "'{}' {}", self.component, self.reason)
    }
}

/// Classify `name` as an alias of `.git` or `.heddle`, regardless of depth.
///
/// This is the name-level predicate. Most callers want
/// [`reserved_tree_entry_name`] or [`reserved_path_component`], which also
/// apply the depth rule (`.heddle` is reserved only at the root).
///
/// `name` is raw bytes so Git tree names that are not UTF-8 can be checked.
pub fn reserved_metadata_name(name: &[u8]) -> Option<ReservedMetadataName> {
    [MetadataDir::Git, MetadataDir::Heddle]
        .into_iter()
        .find_map(|dir| alias_of(name, dir).map(|alias| ReservedMetadataName { dir, alias }))
}

/// Whether `name` aliases `.git` or `.heddle`, regardless of depth.
pub fn is_reserved_metadata_name(name: impl AsRef<[u8]>) -> bool {
    reserved_metadata_name(name.as_ref()).is_some()
}

/// Classify the tree entry `name`: `.git` aliases at any depth, `.heddle`
/// aliases only when `at_root` (the entry is a direct child of a commit's or
/// State's root tree, or of a checkout's destination).
pub fn reserved_tree_entry_name(name: &[u8], at_root: bool) -> Option<ReservedMetadataName> {
    if let Some(alias) = alias_of(name, MetadataDir::Git) {
        return Some(ReservedMetadataName {
            dir: MetadataDir::Git,
            alias,
        });
    }
    if at_root && let Some(alias) = alias_of(name, MetadataDir::Heddle) {
        return Some(ReservedMetadataName {
            dir: MetadataDir::Heddle,
            alias,
        });
    }
    None
}

/// The first component of the repository-relative `path` that is reserved
/// by [`reserved_tree_entry_name`], if any.
///
/// Components are split on both `/` and `\`. Empty and `.` components are
/// skipped, so `./.heddle/x` is a root `.heddle`.
pub fn reserved_path_component(path: &[u8]) -> Option<ReservedPathComponent> {
    path.split(|&b| b == b'/' || b == b'\\')
        .filter(|component| !component.is_empty() && *component != b".")
        .enumerate()
        .find_map(|(index, component)| {
            reserved_tree_entry_name(component, index == 0).map(|reason| ReservedPathComponent {
                index,
                component: String::from_utf8_lossy(component).into_owned(),
                reason,
            })
        })
}

fn alias_of(name: &[u8], dir: MetadataDir) -> Option<MetadataAlias> {
    ntfs_alias(name, dir).or_else(|| hfs_alias(name, dir))
}

/// Git's `is_ntfs_dotgit`, generalised to `dir`: `.<stem>` or `<stem>~1` in
/// any case, then any run of dots and spaces, then the end of the name, a
/// path separator, or `:` (an alternate data stream).
fn ntfs_alias(name: &[u8], dir: MetadataDir) -> Option<MetadataAlias> {
    let stem = dir.stem();
    let (rest, short_name) = if let Some(rest) = name
        .strip_prefix(b".")
        .and_then(|rest| strip_prefix_ignore_ascii_case(rest, stem))
    {
        (rest, false)
    } else {
        let rest = strip_prefix_ignore_ascii_case(name, stem)?.strip_prefix(b"~1")?;
        (rest, true)
    };
    let trimmed_len = rest.iter().take_while(|&&b| b == b'.' || b == b' ').count();
    let trailing = &rest[..trimmed_len];
    let terminator = rest.get(trimmed_len).copied();
    let alias = match terminator {
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

fn strip_prefix_ignore_ascii_case<'a>(name: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    (name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &name[prefix.len()..])
}

/// Git's `is_hfs_dotgit`, generalised to `dir`: after dropping the code
/// points HFS+ ignores, `.<stem>` in any case, then the end of the name or
/// `/`. Malformed UTF-8 ends the name, as it does in Git.
fn hfs_alias(name: &[u8], dir: MetadataDir) -> Option<MetadataAlias> {
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
    for &expected in dir.stem() {
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
