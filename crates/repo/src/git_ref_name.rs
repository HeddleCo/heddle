// SPDX-License-Identifier: Apache-2.0
//! Typed classification for fully-qualified Git ref names.

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

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
