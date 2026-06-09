// SPDX-License-Identifier: Apache-2.0
//! [`GitRepo`] — thin read-only wrapper over the local sley plumbing crates.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use sley_core::{GitError, ObjectFormat, ObjectId};
use sley_config::GitConfig;
use sley_formats::RepositoryLayout;
use sley_object::{Commit, EncodedObject, ObjectType, Tree};
use sley_odb::{repository_common_dir, FileObjectDatabase, ObjectReader};
use sley_refs::{FileRefStore, RefTarget};
use sley_rev::{peel_tags, peel_to_commit};

use crate::id::{empty_blob_sha1, empty_tree_sha1};
use crate::kind::ObjectKind;
use crate::{GitSubstrateError, Result};

/// Read-only git repository handle backed by the local sley checkout.
#[derive(Debug, Clone)]
pub struct GitRepo {
    git_dir: PathBuf,
    common_dir: PathBuf,
    format: ObjectFormat,
}

impl GitRepo {
    /// Create a new non-bare repository at `path` and return an open handle.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_with_format(path, ObjectFormat::Sha1)
    }

    /// Create a new non-bare repository at `path` using `format`.
    pub fn init_with_format(path: impl AsRef<Path>, format: ObjectFormat) -> Result<Self> {
        let path = path.as_ref();
        RepositoryLayout::init_at(path, format, false).map_err(GitSubstrateError::from)?;
        Self::discover(path)
    }

    /// Create a new bare repository at `path` and return an open handle.
    pub fn init_bare(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_bare_with_format(path, ObjectFormat::Sha1)
    }

    /// Create a new bare repository at `path` using `format` and return an open handle.
    pub fn init_bare_with_format(path: impl AsRef<Path>, format: ObjectFormat) -> Result<Self> {
        let path = path.as_ref();
        RepositoryLayout::init_at(path, format, true).map_err(GitSubstrateError::from)?;
        Self::open(path)
    }

    /// Open the repository whose git directory is exactly `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_git_dir(resolve_git_dir(path.as_ref())?)
    }

    /// Discover the repository containing `path`, mirroring git's upward search.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_git_dir(discover_git_dir(path.as_ref())?)
    }

    fn from_git_dir(git_dir: PathBuf) -> Result<Self> {
        if !is_git_dir(&git_dir) {
            return Err(GitSubstrateError::Git(GitError::repository_not_found(format!(
                "not a git repository: {}",
                git_dir.display()
            ))));
        }
        let common_dir = repository_common_dir(&git_dir);
        let format = read_object_format(&common_dir)?;
        Ok(Self {
            git_dir,
            common_dir,
            format,
        })
    }

    /// The repository's git directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The common directory shared between linked worktrees.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The repository's object format (`sha1` or `sha256`).
    pub fn object_format(&self) -> ObjectFormat {
        self.format
    }

    /// Working tree root for a non-bare repository, if known.
    pub fn workdir(&self) -> Option<PathBuf> {
        resolve_workdir(&self.git_dir)
    }

    /// Whether this repository is bare (`core.bare = true`).
    pub fn is_bare(&self) -> Result<bool> {
        if let Ok(config) = self.git_config()
            && let Some(bare) = config.get_bool("core", None, "bare")
        {
            return Ok(bare);
        }
        Ok(resolve_workdir(&self.git_dir).is_none())
    }

    pub(crate) fn object_db(&self) -> FileObjectDatabase {
        FileObjectDatabase::from_git_dir(&self.common_dir, self.format)
    }

    /// Whether `oid` is present in this repository's object database.
    pub fn has_object(&self, oid: &ObjectId) -> Result<bool> {
        self.object_db()
            .contains(oid)
            .map_err(GitSubstrateError::from)
    }

    /// Open the on-disk ref store for this repository.
    pub fn ref_store(&self) -> FileRefStore {
        FileRefStore::new(self.git_dir.clone(), self.format)
    }

    /// Read an object's kind from the object database.
    ///
    /// Returns `None` when the object is missing or unreadable.
    pub fn read_object_kind(&self, oid: &ObjectId) -> Option<ObjectKind> {
        self.object_db()
            .read_object(oid)
            .ok()
            .map(|object| ObjectKind::from(object.object_type))
    }

    /// Returns `true` when `oid` resolves to a commit object.
    ///
    /// Any read error is treated as "not a commit", matching the ingest walker.
    pub fn is_commit(&self, oid: &ObjectId) -> bool {
        matches!(self.read_object_kind(oid), Some(ObjectKind::Commit))
    }

    /// Write blob content into this repository's object database.
    pub fn write_blob(&self, content: &[u8]) -> Result<ObjectId> {
        crate::write::write_blob(self.git_dir(), self.format, content)
    }

    /// Write commit content bytes (unframed) into this repository's object database.
    pub fn write_commit_content(&self, content: &[u8]) -> Result<ObjectId> {
        crate::write::write_commit_content(self.git_dir(), self.format, content)
    }

    /// Write a tree from `entries` into this repository's object database.
    pub fn write_tree(&self, entries: &mut [crate::write::TreeEntryInput]) -> Result<ObjectId> {
        crate::write::write_tree(self.git_dir(), self.format, entries)
    }

    /// Write an annotated tag object into this repository's object database.
    pub fn write_tag(&self, tag: &sley_object::Tag) -> Result<ObjectId> {
        crate::write::write_tag(self.git_dir(), self.format, tag)
    }

    /// Read a blob object's raw bytes from the object database.
    pub fn read_blob(&self, oid: &ObjectId) -> Result<Vec<u8>> {
        let object = self.read_object(oid)?;
        if object.object_type != ObjectType::Blob {
            return Err(GitSubstrateError::Git(GitError::InvalidObject(format!(
                "expected blob {oid}, found {}",
                object.object_type.as_str()
            ))));
        }
        Ok(object.body)
    }

    /// Read any object's encoded body from the object database.
    pub fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
        if oid == &empty_tree_sha1() {
            return Ok(EncodedObject::new(ObjectType::Tree, Vec::new()));
        }
        if oid == &empty_blob_sha1() {
            return Ok(EncodedObject::new(ObjectType::Blob, Vec::new()));
        }
        self.object_db()
            .read_object(oid)
            .map(|object| (*object).clone())
            .map_err(GitSubstrateError::from)
    }

    /// Read and parse a tree object.
    pub fn read_tree(&self, oid: &ObjectId) -> Result<Tree> {
        if oid == &empty_tree_sha1() {
            return Tree::parse(self.format, b"").map_err(GitSubstrateError::from);
        }
        let object = self.read_object(oid)?;
        if object.object_type != ObjectType::Tree {
            return Err(GitSubstrateError::Git(GitError::InvalidObject(format!(
                "expected tree {oid}, found {}",
                object.object_type.as_str()
            ))));
        }
        Tree::parse(self.format, &object.body).map_err(GitSubstrateError::from)
    }

    /// Read a commit object's raw content bytes (unframed `git cat-file commit` body).
    pub fn read_commit_content(&self, oid: &ObjectId) -> Result<Vec<u8>> {
        let object = self.read_object(oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitSubstrateError::Git(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            ))));
        }
        Ok(object.body)
    }

    /// Read and parse a commit object.
    pub fn read_commit(&self, oid: &ObjectId) -> Result<Commit> {
        let body = self.read_commit_content(oid)?;
        Commit::parse(self.format, &body).map_err(GitSubstrateError::from)
    }

    /// Whether `HEAD` points directly at a commit (detached) rather than a branch ref.
    pub fn head_is_detached(&self) -> Result<bool> {
        Ok(!matches!(
            self.ref_store().read_ref("HEAD").map_err(GitSubstrateError::from)?,
            Some(RefTarget::Symbolic(_))
        ))
    }

    /// Tree oid at `HEAD`, or the canonical empty tree when unborn or unreadable.
    pub fn head_tree_oid_or_empty(&self) -> Result<ObjectId> {
        let Some(commit_oid) = self.head_commit_oid_or_none()? else {
            return Ok(empty_tree_sha1());
        };
        Ok(self
            .commit_tree_oid(&commit_oid)
            .unwrap_or_else(|_| empty_tree_sha1()))
    }

    /// Peel `HEAD` to a commit oid when present.
    pub fn head_commit_oid_or_none(&self) -> Result<Option<ObjectId>> {
        let Some(target) = self
            .ref_store()
            .read_ref("HEAD")
            .map_err(GitSubstrateError::from)?
        else {
            return Ok(None);
        };
        let Some(oid) = self.resolve_ref_target(&target)? else {
            return Ok(None);
        };
        match peel_to_commit(&self.object_db(), self.format, &oid) {
            Ok(commit) => Ok(Some(commit)),
            Err(_) => Ok(None),
        }
    }

    /// Hex-encoded peeled `HEAD` commit, if resolvable.
    pub fn head_commit_hex_or_none(&self) -> Result<Option<String>> {
        Ok(self
            .head_commit_oid_or_none()?
            .map(|oid| oid.to_hex()))
    }

    /// Short branch name when `HEAD` is a symbolic `refs/heads/*` ref.
    pub fn current_branch_name(&self) -> Option<String> {
        let head = self.ref_store().read_ref("HEAD").ok()??;
        match head {
            RefTarget::Symbolic(name) => name
                .strip_prefix("refs/heads/")
                .filter(|branch| !branch.is_empty())
                .map(str::to_string),
            RefTarget::Direct(_) => None,
        }
    }

    /// Local branch short names under `refs/heads/`.
    pub fn local_branch_names(&self) -> Result<Vec<String>> {
        Ok(sorted_short_ref_names(
            self.ref_store().list_refs().map_err(GitSubstrateError::from)?,
            "refs/heads/",
        ))
    }

    /// Local tag short names under `refs/tags/`.
    pub fn local_tag_names(&self) -> Result<Vec<String>> {
        Ok(sorted_short_ref_names(
            self.ref_store().list_refs().map_err(GitSubstrateError::from)?,
            "refs/tags/",
        ))
    }

    /// Resolve `rev` to an object id when possible.
    pub fn resolve_revision(&self, rev: &str) -> Result<Option<ObjectId>> {
        match sley_rev::resolve_revision_with_reader(
            self.git_dir(),
            self.format,
            &self.object_db(),
            rev,
        ) {
            Ok(oid) => Ok(Some(oid)),
            Err(GitError::NotFound(_)) => Ok(None),
            Err(err) => Err(GitSubstrateError::from(err)),
        }
    }

    /// Count commits reachable from `tip` but not from `hidden`.
    pub fn count_commits_since(&self, tip: &ObjectId, hidden: &ObjectId) -> Result<usize> {
        let hidden_set: HashSet<_> = sley_rev::walk_commit_metadata(
            self.git_dir(),
            self.format,
            &self.object_db(),
            [hidden.clone()],
            false,
        )
        .map_err(GitSubstrateError::from)?
        .into_iter()
        .map(|metadata| metadata.oid)
        .collect();
        Ok(
            sley_rev::walk_commit_metadata(
                self.git_dir(),
                self.format,
                &self.object_db(),
                [tip.clone()],
                false,
            )
            .map_err(GitSubstrateError::from)?
            .into_iter()
            .filter(|metadata| !hidden_set.contains(&metadata.oid))
            .count(),
        )
    }

    /// Remote-tracking ref for `branch` when `branch.<name>.remote` + `.merge` are set.
    pub fn upstream_tracking_ref(&self, branch: &str) -> Result<Option<String>> {
        let config = self.git_config()?;
        let remote = config
            .get("branch", Some(branch), "remote")
            .filter(|value| !value.is_empty());
        let merge = config
            .get("branch", Some(branch), "merge")
            .filter(|value| !value.is_empty());
        if let (Some(remote), Some(merge)) = (remote, merge)
            && let Some(short) = merge.strip_prefix("refs/heads/")
        {
            return Ok(Some(format!("refs/remotes/{remote}/{short}")));
        }
        Ok(None)
    }

    /// Configured remote names from `.git/config`.
    pub fn remote_names(&self) -> Result<Vec<String>> {
        let config = self.git_config()?;
        let mut names = config
            .sections
            .iter()
            .filter(|section| section.name.eq_ignore_ascii_case("remote"))
            .filter_map(|section| section.subsection.clone())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Returns `origin` when configured, matching git's default-remote probe.
    pub fn default_remote_name(&self) -> Option<String> {
        self.remote_names()
            .ok()
            .and_then(|names| names.into_iter().find(|name| name == "origin"))
    }

    /// Open the common git directory shared with linked worktrees.
    pub fn common_repo(&self) -> Result<Self> {
        if self.git_dir == self.common_dir {
            return Ok(self.clone());
        }
        Self::open(&self.common_dir)
    }

    /// Resolve a ref name to a direct object id when possible.
    pub fn read_ref_oid(&self, name: &str) -> Result<Option<ObjectId>> {
        let Some(target) = self
            .ref_store()
            .read_ref(name)
            .map_err(GitSubstrateError::from)?
        else {
            return Ok(None);
        };
        match target {
            RefTarget::Direct(oid) => Ok(Some(oid)),
            RefTarget::Symbolic(sym) => self.resolve_symbolic(&sym),
        }
    }

    /// Whether `name` exists in the ref database.
    pub fn has_ref(&self, name: &str) -> Result<bool> {
        Ok(self
            .ref_store()
            .read_ref(name)
            .map_err(GitSubstrateError::from)?
            .is_some())
    }

    /// Read a string value from this repository's config (`section.key`).
    pub fn config_string(&self, section: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .git_config()?
            .get(section, None, key)
            .map(str::to_string))
    }

    /// List refs visible from this repository (packed + loose).
    pub fn list_refs(&self) -> Result<Vec<sley_refs::Ref>> {
        self.ref_store()
            .list_refs()
            .map_err(GitSubstrateError::from)
    }

    /// Whether `name` (or `HEAD` when `branch` is `None`) peels to a commit.
    pub fn reference_peels_to_commit(&self, branch: Option<&str>) -> Result<bool> {
        let name = branch
            .map(|branch| format!("refs/heads/{branch}"))
            .unwrap_or_else(|| "HEAD".to_string());
        Ok(matches!(
            self.peel_reference_to_commit(&name),
            Ok(Ok(_))
        ))
    }

    fn git_config(&self) -> Result<GitConfig> {
        GitConfig::read(self.common_dir().join("config")).map_err(GitSubstrateError::from)
    }

    fn commit_tree_oid(&self, commit_oid: &ObjectId) -> Result<ObjectId> {
        let object = self
            .object_db()
            .read_object(commit_oid)
            .map_err(GitSubstrateError::from)?;
        let commit = Commit::parse(self.format, &object.body).map_err(GitSubstrateError::from)?;
        Ok(commit.tree)
    }

    /// Peel `reference` through symbolic refs and annotated tags to a commit id.
    ///
    /// Returns `Ok(Ok(commit))` when the peeled target is a commit,
    /// `Ok(Err(kind))` when it resolves but is not commit-shaped (blob/tree/tag
    /// chain ending on a non-commit), and `Err` on peel/read failures.
    pub fn peel_reference_to_commit(
        &self,
        name: &str,
    ) -> Result<std::result::Result<ObjectId, ObjectKind>> {
        let Some(target) = self
            .ref_store()
            .read_ref(name)
            .map_err(GitSubstrateError::from)?
        else {
            return Err(GitSubstrateError::Other(format!(
                "reference not found: {name}"
            )));
        };

        let Some(oid) = self.resolve_ref_target(&target)? else {
            return Err(GitSubstrateError::Other(format!(
                "reference {name} does not resolve to an object id"
            )));
        };

        let odb = self.object_db();
        match peel_to_commit(&odb, self.format, &oid) {
            Ok(commit) => Ok(Ok(commit)),
            Err(_) => {
                let peeled = peel_tags(&odb, self.format, &oid).map_err(GitSubstrateError::from)?;
                let kind = self
                    .read_object_kind(&peeled)
                    .ok_or_else(|| GitSubstrateError::Other(format!("object not found: {peeled}")))?;
                Ok(Err(kind))
            }
        }
    }

    fn resolve_ref_target(&self, target: &RefTarget) -> Result<Option<ObjectId>> {
        match target {
            RefTarget::Direct(oid) => Ok(Some(oid.clone())),
            RefTarget::Symbolic(name) => self.resolve_symbolic(name),
        }
    }

    fn resolve_symbolic(&self, name: &str) -> Result<Option<ObjectId>> {
        let refs = self.ref_store();
        let mut current = name.to_string();
        for _ in 0..5 {
            match refs.read_ref(&current).map_err(GitSubstrateError::from)? {
                None => return Ok(None),
                Some(RefTarget::Direct(oid)) => return Ok(Some(oid)),
                Some(RefTarget::Symbolic(next)) => current = next,
            }
        }
        Err(GitSubstrateError::Git(GitError::InvalidFormat(format!(
            "symbolic reference chain too deep starting at {name}"
        ))))
    }
}

fn read_object_format(common_dir: &Path) -> Result<ObjectFormat> {
    let config_path = common_dir.join("config");
    match GitConfig::read(&config_path) {
        Ok(config) => config
            .repository_object_format()
            .map_err(GitSubstrateError::from),
        Err(GitError::Io(_)) | Err(GitError::NotFound(_)) => Ok(ObjectFormat::Sha1),
        Err(err) => Err(GitSubstrateError::from(err)),
    }
}

fn resolve_git_dir(path: &Path) -> std::result::Result<PathBuf, GitError> {
    if path.is_file()
        && let Some(target) = read_gitdir_link(path)?
    {
        return Ok(target);
    }
    Ok(path.to_path_buf())
}

fn is_git_dir(path: &Path) -> bool {
    path.join("HEAD").is_file()
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

fn read_gitdir_link(path: &Path) -> std::result::Result<Option<PathBuf>, GitError> {
    let contents = std::fs::read_to_string(path)?;
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let target = PathBuf::from(target.trim());
    if target.is_absolute() {
        Ok(Some(target))
    } else {
        let base = path.parent().unwrap_or_else(|| Path::new(""));
        Ok(Some(base.join(target)))
    }
}

fn sorted_short_ref_names(refs: Vec<sley_refs::Ref>, prefix: &str) -> Vec<String> {
    let mut names = refs
        .into_iter()
        .filter_map(|reference| {
            reference
                .name
                .strip_prefix(prefix)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn resolve_workdir(git_dir: &Path) -> Option<PathBuf> {
    let gitdir_file = git_dir.join("gitdir");
    if gitdir_file.is_file() {
        let contents = fs::read_to_string(&gitdir_file).ok()?;
        let path = contents.trim().strip_prefix("gitdir:")?.trim();
        let dot_git = PathBuf::from(path);
        return dot_git.parent().map(|parent| parent.to_path_buf());
    }
    if git_dir.file_name() == Some(OsStr::new(".git")) {
        return git_dir.parent().map(|parent| parent.to_path_buf());
    }
    None
}

fn discover_git_dir(start: &Path) -> std::result::Result<PathBuf, GitError> {
    let start = if start.as_os_str().is_empty() {
        Path::new(".")
    } else {
        start
    };
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };
    for candidate in absolute.ancestors() {
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() {
            return Ok(dot_git);
        }
        if dot_git.is_file()
            && let Some(git_dir) = read_gitdir_link(&dot_git)?
            && is_git_dir(&git_dir)
        {
            return Ok(git_dir);
        }
        if is_git_dir(candidate) {
            return Ok(candidate.to_path_buf());
        }
    }
    Err(GitError::repository_not_found(format!(
        "not a git repository (or any parent up to {}): {}",
        absolute.display(),
        start.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::{set_reference, RefConstraint};
    use crate::{empty_tree_sha1, write_simple_commit};
    use tempfile::TempDir;

    fn init_repo(temp: &TempDir) -> GitRepo {
        let root = temp.path();
        let git_dir = root.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        GitRepo::discover(root).expect("discover")
    }

    fn seed_commit(git_dir: &Path, format: ObjectFormat) -> ObjectId {
        let tree = empty_tree_sha1();
        let actor = crate::refs::bridge_reflog_committer();
        write_simple_commit(git_dir, format, &tree, &[], &actor, &actor, b"seed")
            .expect("seed commit")
    }

    #[test]
    fn current_branch_name_reads_symbolic_head() {
        let temp = TempDir::new().expect("tempdir");
        let repo = init_repo(&temp);
        assert_eq!(repo.current_branch_name().as_deref(), Some("main"));
    }

    #[test]
    fn local_branch_and_tag_names_list_refs() {
        let temp = TempDir::new().expect("tempdir");
        let repo = init_repo(&temp);
        let format = repo.object_format();
        let oid = seed_commit(repo.git_dir(), format);
        set_reference(
            repo.git_dir(),
            format,
            "refs/heads/feature",
            &oid,
            RefConstraint::MustNotExist,
            "test",
        )
        .expect("branch");
        set_reference(
            repo.git_dir(),
            format,
            "refs/tags/v1",
            &oid,
            RefConstraint::MustNotExist,
            "test",
        )
        .expect("tag");
        assert_eq!(
            repo.local_branch_names().expect("branches"),
            vec!["feature".to_string()]
        );
        assert_eq!(
            repo.local_tag_names().expect("tags"),
            vec!["v1".to_string()]
        );
    }

    #[test]
    fn reference_peels_to_commit_is_false_for_unborn_head() {
        let temp = TempDir::new().expect("tempdir");
        let repo = init_repo(&temp);
        assert!(!repo.reference_peels_to_commit(None).expect("head"));
        assert!(!repo.reference_peels_to_commit(Some("main")).expect("branch"));
    }

    #[test]
    fn reference_peels_to_commit_is_true_after_branch_update() {
        let temp = TempDir::new().expect("tempdir");
        let repo = init_repo(&temp);
        let format = repo.object_format();
        let oid = seed_commit(repo.git_dir(), format);
        set_reference(
            repo.git_dir(),
            format,
            "refs/heads/main",
            &oid,
            RefConstraint::Any,
            "test",
        )
        .expect("update main");
        assert!(repo.reference_peels_to_commit(None).expect("head"));
        assert!(repo.reference_peels_to_commit(Some("main")).expect("branch"));
    }

    #[test]
    fn init_bare_creates_openable_repository() {
        let temp = TempDir::new().expect("tempdir");
        let git_dir = temp.path().join("bare.git");
        let repo = GitRepo::init_bare(&git_dir).expect("init bare");
        assert_eq!(repo.git_dir(), git_dir);
        assert_eq!(repo.object_format(), ObjectFormat::Sha1);
        assert!(repo.has_ref("HEAD").expect("HEAD"));
    }

    #[test]
    fn is_bare_false_for_linked_worktree_gitdir() {
        let temp = TempDir::new().expect("tempdir");
        let main = temp.path().join("main");
        let linked = temp.path().join("linked");
        let main_git = main.join(".git");
        let wt_admin = main_git.join("worktrees").join("wt1");
        let linked_dot_git = linked.join(".git");
        std::fs::create_dir_all(wt_admin.join("objects")).expect("wt objects");
        std::fs::create_dir_all(wt_admin.join("refs/heads")).expect("wt refs");
        std::fs::write(wt_admin.join("HEAD"), "ref: refs/heads/main\n").expect("wt HEAD");
        std::fs::write(
            wt_admin.join("gitdir"),
            format!("gitdir: {}\n", linked_dot_git.display()),
        )
        .expect("wt gitdir");
        std::fs::create_dir_all(&linked).expect("linked worktree dir");
        std::fs::write(
            &linked_dot_git,
            format!("gitdir: {}\n", wt_admin.display()),
        )
        .expect("linked .git file");

        let repo = GitRepo::discover(&linked).expect("open linked worktree");
        assert!(!repo.is_bare().expect("is_bare"));
        assert_eq!(
            repo.workdir().as_deref(),
            Some(linked.as_path()),
            "linked worktree must resolve a workdir"
        );
    }

    #[test]
    fn default_remote_name_prefers_origin() {
        let temp = TempDir::new().expect("tempdir");
        let repo = init_repo(&temp);
        std::fs::write(
            repo.common_dir().join("config"),
            "[remote \"upstream\"]\n\turl = /tmp/up\n[remote \"origin\"]\n\turl = /tmp/o\n",
        )
        .expect("config");
        assert_eq!(repo.default_remote_name().as_deref(), Some("origin"));
    }
}