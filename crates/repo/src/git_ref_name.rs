// SPDX-License-Identifier: Apache-2.0
//! Typed classification for fully-qualified Git ref names.

use objects::object::SyntheticFrontierName;

/// Sentinel remote name for refs owned by the local repository.
///
/// Local branches, tags, and notes use this owner when represented in the
/// Git projection parser. A user remote named `git` would collide with
/// that sentinel.
pub const REMOTE_NAME_FOR_LOCAL_GIT_REPO: &str = "git";

/// The content namespaces Heddle intentionally mirrors as named Git refs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitRefContentNamespace {
    /// `refs/heads/<name>`.
    Branch,
    /// `refs/tags/<name>`.
    Tag,
    /// `refs/notes/<name>`.
    Note,
    /// A managed synthetic frontier root (`refs/heddle/frontier/<thread>/<full-changeid>`).
    Heddle,
}

/// The wire-level kind used for hosted Git ref updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitRefKind {
    /// `refs/heads/<name>` or `refs/remotes/<remote>/<name>`.
    Branch,
    /// `refs/tags/<name>`.
    Tag,
    /// `refs/notes/<name>`.
    Note,
    /// Any non-local-only ref outside the known content namespaces.
    Other,
}

/// The namespace family a full ref name belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitRefNamespace {
    /// `refs/heads/<name>`.
    Branch,
    /// `refs/remotes/<remote>/<name>`.
    RemoteBranch,
    /// `refs/tags/<name>`.
    Tag,
    /// `refs/notes/<name>`.
    Note,
    /// `refs/stash`.
    Stash,
    /// `refs/original/<name>`.
    Original,
    /// `refs/replace/<name>`.
    Replace,
    /// Anything outside the named namespaces above.
    Other,
}

/// A parsed Git ref name: its kind, short name, and owning remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedGitRef<'a> {
    pub kind: GitRefKind,
    /// Short name beneath the namespace, e.g. `main` for `refs/heads/main`
    /// or `feature/x` for `refs/remotes/origin/feature/x`.
    pub name: &'a str,
    /// Owning remote. Local content refs report
    /// [`REMOTE_NAME_FOR_LOCAL_GIT_REPO`].
    pub remote: &'a str,
}

/// A fully-qualified Git ref name classified into Heddle's shared namespace
/// semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitRefName<'a> {
    full_name: &'a str,
}

impl<'a> GitRefName<'a> {
    /// Classify a fully-qualified Git ref name.
    pub fn new(full_name: &'a str) -> Self {
        Self { full_name }
    }

    /// Return the original fully-qualified name.
    pub fn as_str(&self) -> &'a str {
        self.full_name
    }

    /// Return the ref namespace family.
    pub fn namespace(&self) -> GitRefNamespace {
        if self.branch_name().is_some() {
            GitRefNamespace::Branch
        } else if self.remote_name().is_some() {
            GitRefNamespace::RemoteBranch
        } else if self.tag_name().is_some() {
            GitRefNamespace::Tag
        } else if self.note_name().is_some() {
            GitRefNamespace::Note
        } else if self.full_name == "refs/stash" {
            GitRefNamespace::Stash
        } else if self.full_name.starts_with("refs/original/") {
            GitRefNamespace::Original
        } else if self.full_name.starts_with("refs/replace/") {
            GitRefNamespace::Replace
        } else {
            GitRefNamespace::Other
        }
    }

    /// Whether this ref is local Git bookkeeping and must not be shipped by
    /// the hosted mirror push path.
    pub fn is_local_only(&self) -> bool {
        matches!(
            self.namespace(),
            GitRefNamespace::RemoteBranch
                | GitRefNamespace::Stash
                | GitRefNamespace::Original
                | GitRefNamespace::Replace
        )
    }

    /// Whether this ref is content for the hosted mirror push path.
    ///
    /// This is intentionally denylist-based: future non-local namespaces are
    /// mirrored as `Other` until product policy says otherwise.
    pub fn is_hosted_mirror_content(&self) -> bool {
        !self.is_local_only()
    }

    /// Return the named content namespace Heddle surfaces in local Git projection
    /// operations.
    pub fn content_namespace(&self) -> Option<GitRefContentNamespace> {
        if self.heddle_name().is_some() {
            return Some(GitRefContentNamespace::Heddle);
        }
        match self.namespace() {
            GitRefNamespace::Branch => Some(GitRefContentNamespace::Branch),
            GitRefNamespace::Tag => Some(GitRefContentNamespace::Tag),
            GitRefNamespace::Note => Some(GitRefContentNamespace::Note),
            _ => None,
        }
    }

    /// Return the hosted Git ref update kind for this ref.
    pub fn wire_kind(&self) -> GitRefKind {
        match self.namespace() {
            GitRefNamespace::Branch | GitRefNamespace::RemoteBranch => GitRefKind::Branch,
            GitRefNamespace::Tag => GitRefKind::Tag,
            GitRefNamespace::Note => GitRefKind::Note,
            _ => GitRefKind::Other,
        }
    }

    /// Return the remote owner for `refs/remotes/<remote>/<name>`.
    pub fn remote_name(&self) -> Option<&'a str> {
        let remote_and_name = self.full_name.strip_prefix("refs/remotes/")?;
        let remote = remote_and_name
            .split_once('/')
            .map_or(remote_and_name, |(remote, _)| remote);
        (!remote.is_empty()).then_some(remote)
    }

    /// Return the short name for a branch, remote branch, tag, or note.
    pub fn short_name(&self) -> Option<&'a str> {
        self.branch_name()
            .or_else(|| self.remote_branch_parts().map(|(_, name)| name))
            .or_else(|| self.tag_name())
            .or_else(|| self.note_name())
            .or_else(|| self.heddle_name())
    }

    /// Parse a Git-projection-visible ref. Notes are content refs in Heddle and are
    /// accepted here to match hosted mirror behavior.
    pub fn git_projection_ref(&self) -> Option<ParsedGitRef<'a>> {
        match self.namespace() {
            GitRefNamespace::Branch => {
                let name = self.branch_name()?;
                (name != "HEAD").then_some(ParsedGitRef {
                    kind: GitRefKind::Branch,
                    name,
                    remote: REMOTE_NAME_FOR_LOCAL_GIT_REPO,
                })
            }
            GitRefNamespace::RemoteBranch => {
                let (remote, name) = self.remote_branch_parts()?;
                (name != "HEAD" && !is_reserved_git_remote_name(remote)).then_some(ParsedGitRef {
                    kind: GitRefKind::Branch,
                    name,
                    remote,
                })
            }
            GitRefNamespace::Tag => self.tag_name().map(|name| ParsedGitRef {
                kind: GitRefKind::Tag,
                name,
                remote: REMOTE_NAME_FOR_LOCAL_GIT_REPO,
            }),
            GitRefNamespace::Note => self.note_name().map(|name| ParsedGitRef {
                kind: GitRefKind::Note,
                name,
                remote: REMOTE_NAME_FOR_LOCAL_GIT_REPO,
            }),
            _ => None,
        }
    }

    /// Format `refs/heads/<name>`.
    pub fn branch_full_name(name: &str) -> String {
        format!("refs/heads/{name}")
    }

    /// Format `refs/remotes/<remote>/<name>`.
    pub fn remote_branch_full_name(remote: &str, name: &str) -> String {
        format!("refs/remotes/{remote}/{name}")
    }

    /// Normalize either `refs/remotes/<remote>/<name>` or `<remote>/<name>`
    /// into a full remote-tracking ref name.
    pub fn remote_tracking_full_name(name: &str) -> String {
        if GitRefName::new(name).remote_name().is_some() {
            name.to_string()
        } else {
            format!("refs/remotes/{name}")
        }
    }

    /// Format `refs/tags/<name>`.
    pub fn tag_full_name(name: &str) -> String {
        format!("refs/tags/{name}")
    }

    /// Format `refs/notes/<name>`.
    pub fn note_full_name(name: &str) -> String {
        format!("refs/notes/{name}")
    }

    /// Format a named content ref.
    pub fn content_full_name(namespace: GitRefContentNamespace, name: &str) -> String {
        match namespace {
            GitRefContentNamespace::Branch => Self::branch_full_name(name),
            GitRefContentNamespace::Tag => Self::tag_full_name(name),
            GitRefContentNamespace::Note => Self::note_full_name(name),
            GitRefContentNamespace::Heddle => format!("refs/heddle/{name}"),
        }
    }

    fn branch_name(&self) -> Option<&'a str> {
        self.full_name.strip_prefix("refs/heads/")
    }

    fn tag_name(&self) -> Option<&'a str> {
        self.full_name.strip_prefix("refs/tags/")
    }

    fn note_name(&self) -> Option<&'a str> {
        self.full_name.strip_prefix("refs/notes/")
    }

    fn heddle_name(&self) -> Option<&'a str> {
        SyntheticFrontierName::parse(self.full_name).ok()?;
        self.full_name
            .strip_prefix("refs/heddle/")
            .filter(|name| !name.is_empty())
    }

    fn remote_branch_parts(&self) -> Option<(&'a str, &'a str)> {
        let remote_and_name = self.full_name.strip_prefix("refs/remotes/")?;
        let (remote, name) = remote_and_name.split_once('/')?;
        (!remote.is_empty() && !name.is_empty()).then_some((remote, name))
    }
}

/// Whether a remote name collides with Heddle's local-ref sentinel.
pub fn is_reserved_git_remote_name(remote: &str) -> bool {
    remote == REMOTE_NAME_FOR_LOCAL_GIT_REPO
}

/// Import reads source objects by their real OIDs; replacement refs are
/// excluded by import policy. Disable Sley's eager replacement-ref scan, which
/// otherwise decodes unrelated packed names before namespace admission.
pub fn open_git_import_source(path: &std::path::Path) -> objects::error::Result<sley::Repository> {
    let options = sley::OpenOptions::new().replace_objects(false);
    sley::Repository::open_with(path, options)
        .or_else(|_| sley::Repository::open_with(path, options.exact_path(true)))
        .map_err(|error| objects::error::HeddleError::InvalidObject(error.to_string()))
}

/// Raw identities at the Git boundary. Sley owns target/OID parsing; names
/// remain bytes until each consumer has applied its namespace policy.
#[derive(Debug, Clone)]
pub struct RawGitRef {
    pub name: Vec<u8>,
    pub target: RawGitRefTarget,
}

#[derive(Debug, Clone)]
pub enum RawGitRefTarget {
    Direct(sley::ObjectId),
    Symbolic(Vec<u8>),
}

/// Read each files-backend ref once, preserving non-UTF-8 identities and loose
/// precedence over packed refs. The reftable backend remains owned by Sley.
pub fn read_raw_git_refs(repository: &sley::Repository) -> objects::error::Result<Vec<RawGitRef>> {
    use std::{collections::BTreeMap, fs, io, path::Path};

    use objects::error::HeddleError;

    fn parse_target(
        format: sley::ObjectFormat,
        name: &[u8],
        bytes: &[u8],
    ) -> Option<RawGitRefTarget> {
        let value = bytes
            .strip_suffix(b"\r\n")
            .or_else(|| bytes.strip_suffix(b"\n"))
            .unwrap_or(bytes);
        if let Some(target) = value.strip_prefix(b"ref: ") {
            return Some(RawGitRefTarget::Symbolic(target.to_vec()));
        }
        let name = std::str::from_utf8(name).unwrap_or("refs/heads/non-utf8");
        let parsed = sley_refs::parse_loose_ref(format, name, bytes).ok()?;
        match parsed.target {
            sley_refs::RefTarget::Direct(oid) if !oid.as_bytes().iter().all(|byte| *byte == 0) => {
                Some(RawGitRefTarget::Direct(oid))
            }
            _ => None,
        }
    }

    fn loose(
        format: sley::ObjectFormat,
        dir: &Path,
        prefix: &[u8],
        out: &mut BTreeMap<Vec<u8>, RawGitRefTarget>,
    ) -> io::Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = [prefix, b"/", entry.file_name().as_encoded_bytes()].concat();
            if entry.file_type()?.is_dir() {
                loose(format, &entry.path(), &name, out)?;
            } else if !name.ends_with(b".lock") {
                // A broken loose ref still shadows the packed value.
                out.remove(&name);
                let target = if entry.file_type()?.is_symlink() {
                    Some(RawGitRefTarget::Symbolic(
                        fs::read_link(entry.path())?
                            .as_os_str()
                            .as_encoded_bytes()
                            .to_vec(),
                    ))
                } else {
                    parse_target(format, &name, &fs::read(entry.path())?)
                };
                if let Some(target) = target {
                    out.insert(name, target);
                }
            }
        }
        Ok(())
    }

    let refs = repository.references();
    if refs
        .uses_reftable()
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?
    {
        return refs
            .list_all_refs()
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))
            .map(|refs| {
                refs.into_iter()
                    .map(|reference| RawGitRef {
                        name: reference.name.into_bytes(),
                        target: match reference.target {
                            sley::ReferenceTarget::Direct(oid) => RawGitRefTarget::Direct(oid),
                            sley::ReferenceTarget::Symbolic(name) => {
                                RawGitRefTarget::Symbolic(name.into_bytes())
                            }
                        },
                    })
                    .collect()
            });
    }
    let mut out = BTreeMap::new();
    let packed_path = repository.common_dir().join("packed-refs");
    let packed = match fs::read(&packed_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut utf8_packed = Vec::with_capacity(packed.len());
    let mut excluded_last = false;
    for line in packed.split_inclusive(|byte| *byte == b'\n') {
        if line.starts_with(b"^") && excluded_last {
            continue;
        }
        if !line.starts_with(b"#")
            && !line.starts_with(b"^")
            && let Some(space) = line.iter().position(|byte| *byte == b' ')
        {
            let name = line[space + 1..]
                .strip_suffix(b"\n")
                .unwrap_or(&line[space + 1..]);
            excluded_last = std::str::from_utf8(name).is_err();
            if excluded_last {
                let oid = std::str::from_utf8(&line[..space])
                    .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
                let oid = sley::ObjectId::from_hex(repository.object_format(), oid)
                    .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
                out.insert(name.to_vec(), RawGitRefTarget::Direct(oid));
                continue;
            }
        }
        utf8_packed.extend_from_slice(line);
    }
    for packed in sley_refs::parse_packed_refs(repository.object_format(), &utf8_packed)
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?
    {
        if let sley_refs::RefTarget::Direct(oid) = packed.reference.target {
            out.insert(
                packed.reference.name.into_bytes(),
                RawGitRefTarget::Direct(oid),
            );
        }
    }
    loose(
        repository.object_format(),
        &repository.common_dir().join("refs"),
        b"refs",
        &mut out,
    )?;
    let head = repository.git_dir().join("HEAD");
    match fs::read(&head) {
        Ok(bytes) => {
            if let Some(target) = parse_target(repository.object_format(), b"HEAD", &bytes) {
                out.insert(b"HEAD".to_vec(), target);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(out
        .into_iter()
        .map(|(name, target)| RawGitRef { name, target })
        .collect())
}

/// Refuse malformed UTF-8 before Sley's String enumeration can turn it into
/// an identity. Literal U+FFFD is valid; no normalization or lossy decode occurs.
pub fn require_exact_git_ref_encoding(repository: &sley::Repository) -> objects::error::Result<()> {
    fn check_tree(path: &std::path::Path) -> objects::error::Result<()> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(path)?;
            if target.to_str().is_none() {
                return Err(objects::error::HeddleError::InvalidRefName(
                    "Git symbolic ref is not UTF-8; refusing lossy identity".into(),
                ));
            }
        } else if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                if entry.file_name().to_str().is_none() {
                    return Err(objects::error::HeddleError::InvalidRefName(
                        "Git ref name is not UTF-8; refusing lossy identity".into(),
                    ));
                }
                check_tree(&entry.path())?;
            }
        } else {
            let bytes = std::fs::read(path)?;
            std::str::from_utf8(&bytes).map_err(|_| {
                objects::error::HeddleError::InvalidRefName(
                    "Git ref framing is not UTF-8; refusing lossy identity".into(),
                )
            })?;
        }
        Ok(())
    }
    check_tree(&repository.common_dir().join("refs"))?;
    check_tree(&repository.common_dir().join("packed-refs"))?;
    check_tree(&repository.git_dir().join("HEAD"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn dangling_git_symbolic_ref_refuses_non_utf8_target_before_enumeration() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let temp = tempfile::TempDir::new().expect("Git source");
        let git = sley::Repository::init(temp.path()).expect("Git repository");
        let refs = git.common_dir().join("refs/heads");
        std::fs::create_dir_all(&refs).expect("heads");
        std::os::unix::fs::symlink(
            OsString::from_vec(b"missing-\xff".to_vec()),
            refs.join("symbolic"),
        )
        .expect("dangling symbolic ref");
        let error = require_exact_git_ref_encoding(&git).expect_err("no lossy target identity");
        assert!(error.to_string().contains("symbolic ref is not UTF-8"));
    }

    #[test]
    fn classifies_every_git_namespace_used_by_sync_and_projection() {
        let cases = [
            (
                "refs/heads/main",
                GitRefNamespace::Branch,
                Some(GitRefContentNamespace::Branch),
                GitRefKind::Branch,
                false,
                true,
            ),
            (
                "refs/remotes/origin/main",
                GitRefNamespace::RemoteBranch,
                None,
                GitRefKind::Branch,
                true,
                false,
            ),
            (
                "refs/remotes/origin",
                GitRefNamespace::RemoteBranch,
                None,
                GitRefKind::Branch,
                true,
                false,
            ),
            (
                "refs/tags/v1.0",
                GitRefNamespace::Tag,
                Some(GitRefContentNamespace::Tag),
                GitRefKind::Tag,
                false,
                true,
            ),
            (
                "refs/notes/heddle",
                GitRefNamespace::Note,
                Some(GitRefContentNamespace::Note),
                GitRefKind::Note,
                false,
                true,
            ),
            (
                "refs/stash",
                GitRefNamespace::Stash,
                None,
                GitRefKind::Other,
                true,
                false,
            ),
            (
                "refs/original/refs/heads/main",
                GitRefNamespace::Original,
                None,
                GitRefKind::Other,
                true,
                false,
            ),
            (
                "refs/replace/deadbeef",
                GitRefNamespace::Replace,
                None,
                GitRefKind::Other,
                true,
                false,
            ),
            (
                "refs/heddle/internal",
                GitRefNamespace::Other,
                None,
                GitRefKind::Other,
                false,
                true,
            ),
            (
                "refs/heddle/frontier/main/hc-abc",
                GitRefNamespace::Other,
                None,
                GitRefKind::Other,
                false,
                true,
            ),
        ];

        for (name, namespace, content_namespace, wire_kind, local_only, mirror_content) in cases {
            let ref_name = GitRefName::new(name);
            assert_eq!(ref_name.namespace(), namespace, "{name}");
            assert_eq!(ref_name.content_namespace(), content_namespace, "{name}");
            assert_eq!(ref_name.wire_kind(), wire_kind, "{name}");
            assert_eq!(ref_name.is_local_only(), local_only, "{name}");
            assert_eq!(
                ref_name.is_hosted_mirror_content(),
                mirror_content,
                "{name}"
            );
        }
    }

    #[test]
    fn parses_git_projection_visible_refs() {
        assert_eq!(
            GitRefName::new("refs/heads/main").git_projection_ref(),
            Some(ParsedGitRef {
                kind: GitRefKind::Branch,
                name: "main",
                remote: REMOTE_NAME_FOR_LOCAL_GIT_REPO,
            })
        );
        assert_eq!(
            GitRefName::new("refs/remotes/origin/feature/x").git_projection_ref(),
            Some(ParsedGitRef {
                kind: GitRefKind::Branch,
                name: "feature/x",
                remote: "origin",
            })
        );
        assert_eq!(
            GitRefName::new("refs/tags/v1.0").git_projection_ref(),
            Some(ParsedGitRef {
                kind: GitRefKind::Tag,
                name: "v1.0",
                remote: REMOTE_NAME_FOR_LOCAL_GIT_REPO,
            })
        );
        assert_eq!(
            GitRefName::new("refs/notes/heddle").git_projection_ref(),
            Some(ParsedGitRef {
                kind: GitRefKind::Note,
                name: "heddle",
                remote: REMOTE_NAME_FOR_LOCAL_GIT_REPO,
            })
        );
    }

    #[test]
    fn rejects_symbolic_head_and_reserved_remote_from_git_projection_parse() {
        assert_eq!(
            GitRefName::new("refs/heads/HEAD").git_projection_ref(),
            None
        );
        assert_eq!(
            GitRefName::new("refs/remotes/origin/HEAD").git_projection_ref(),
            None
        );
        assert_eq!(
            GitRefName::new("refs/remotes/git/main").git_projection_ref(),
            None
        );
    }

    #[test]
    fn classifies_only_parseable_synthetic_frontier_as_heddle_namespace() {
        let change = objects::object::ChangeId::from_bytes([9; 16]);
        let name = objects::object::SyntheticFrontierName::new("main", change)
            .expect("fixture synthetic name")
            .git_ref();
        let ref_name = GitRefName::new(&name);
        assert_eq!(
            ref_name.content_namespace(),
            Some(GitRefContentNamespace::Heddle)
        );
        assert_eq!(
            ref_name.short_name().expect("heddle short name"),
            name.strip_prefix("refs/heddle/").expect("heddle prefix")
        );
    }

    #[test]
    fn rejects_internal_and_forged_heddle_refs_as_publishable_namespace() {
        for name in [
            "refs/heddle/internal",
            "refs/heddle/frontier/main/hc-abc",
            "refs/heddle",
            "refs/heddle/",
        ] {
            assert_eq!(
                GitRefName::new(name).content_namespace(),
                None,
                "{name} must not be a publishable Heddle content ref"
            );
        }
    }

    #[test]
    fn formats_full_ref_names() {
        assert_eq!(GitRefName::branch_full_name("main"), "refs/heads/main");
        assert_eq!(
            GitRefName::remote_branch_full_name("origin", "feature/x"),
            "refs/remotes/origin/feature/x"
        );
        assert_eq!(
            GitRefName::remote_tracking_full_name("origin/feature/x"),
            "refs/remotes/origin/feature/x"
        );
        assert_eq!(
            GitRefName::remote_tracking_full_name("refs/remotes/origin/feature/x"),
            "refs/remotes/origin/feature/x"
        );
        assert_eq!(GitRefName::tag_full_name("v1.0"), "refs/tags/v1.0");
        assert_eq!(GitRefName::note_full_name("heddle"), "refs/notes/heddle");
        assert_eq!(
            GitRefName::content_full_name(GitRefContentNamespace::Heddle, "frontier/main/hc-abc"),
            "refs/heddle/frontier/main/hc-abc"
        );
    }
}
