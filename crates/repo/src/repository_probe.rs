// SPDX-License-Identifier: Apache-2.0
//! Read-only repository discovery.

use std::{
    fs,
    path::{Path, PathBuf},
};

use objects::object::Principal;
use sley::{GitObjectType, ObjectId, ReferenceTarget, Repository as SleyRepository};

use crate::{
    HeddleError, RepoConfig, Repository, RepositoryCapability, Result,
    repository::{
        ensure_supported_repo_format, git_config_principal, has_git_metadata,
        metadataless_managed_thread_root, parse_objectstore_pointer,
        repository_capability_for_root,
    },
};

/// The write target selected by [`Repository::probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryProbeTarget {
    /// A Heddle repository already exists at the resolved root.
    Existing,
    /// No Heddle repository exists yet, but the resolved root is a Git checkout.
    FreshGitOverlay,
    /// Neither Heddle nor Git metadata exists at or above the probe path.
    FreshNative,
}

/// Read-only discovery result for a potential Heddle repository.
#[derive(Debug, Clone)]
pub struct RepositoryProbe {
    target: RepositoryProbeTarget,
    root: PathBuf,
    heddle_dir: Option<PathBuf>,
    capability: RepositoryCapability,
    existing_config: Option<RepoConfig>,
    git_facts: RepositoryProbeGitFacts,
    repo_principal_candidate: Option<Principal>,
}

/// Git facts gathered at the resolved repository root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryProbeGitFacts {
    root: Option<PathBuf>,
    has_metadata: bool,
    has_commits: bool,
    is_shallow: bool,
    head_is_detached: bool,
    head_branch: Option<String>,
}

impl RepositoryProbe {
    /// The target class selected by read-only discovery.
    pub fn target(&self) -> RepositoryProbeTarget {
        self.target
    }

    /// The root directory that `Repository::open`/init would operate on.
    pub fn resolved_root(&self) -> &Path {
        &self.root
    }

    /// The resolved `.heddle` directory for existing repositories.
    ///
    /// For isolated checkouts this is the shared source repository's
    /// `.heddle`, not the checkout-local pointer directory.
    pub fn existing_heddle_dir(&self) -> Option<&Path> {
        self.heddle_dir.as_deref()
    }

    /// The capability the resolved repository would have when opened.
    pub fn capability(&self) -> RepositoryCapability {
        self.capability
    }

    /// Existing repository config, if discovery found an initialized repo.
    pub fn existing_config(&self) -> Option<&RepoConfig> {
        self.existing_config.as_ref()
    }

    /// Git facts at the resolved root.
    pub fn git_facts(&self) -> &RepositoryProbeGitFacts {
        &self.git_facts
    }

    /// Candidate repo-scoped principal before user-config fallback.
    ///
    /// This mirrors the repo-level part of capture attribution: repository
    /// config wins if present, then Git config for Git-overlay roots, then the
    /// shared parent Git config for isolated checkouts. Environment and user
    /// config are intentionally outside this read-only repository probe.
    pub fn repo_principal_candidate(&self) -> Option<&Principal> {
        self.repo_principal_candidate.as_ref()
    }
}

impl RepositoryProbeGitFacts {
    /// Git checkout root for the resolved repository, if any.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Whether Git metadata exists at the resolved root.
    pub fn has_metadata(&self) -> bool {
        self.has_metadata
    }

    /// Whether any local Git ref peels to a commit.
    pub fn has_commits(&self) -> bool {
        self.has_commits
    }

    /// Whether the resolved Git checkout is shallow.
    pub fn is_shallow(&self) -> bool {
        self.is_shallow
    }

    /// Whether Git HEAD is detached at the resolved root.
    pub fn head_is_detached(&self) -> bool {
        self.head_is_detached
    }

    /// Current Git HEAD branch name, when attached.
    pub fn head_branch(&self) -> Option<&str> {
        self.head_branch.as_deref()
    }
}

impl Repository {
    /// Discover how Heddle would treat `path` without writing to disk.
    ///
    /// This mirrors the discovery half of [`Repository::open`]: ancestor
    /// `.heddle` lookup, isolated-checkout objectstore pointers, the nested-Git
    /// boundary special case, and the final fresh Git-overlay/native fallback.
    /// It deliberately skips all open-time writes: no Git exclude edits, no
    /// bootstrap, and no Git-to-Heddle HEAD synchronization.
    pub fn probe(path: impl AsRef<Path>) -> Result<RepositoryProbe> {
        probe_repository(path.as_ref())
    }
}

fn probe_repository(path: &Path) -> Result<RepositoryProbe> {
    let start_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if let Some(mount_root) = metadataless_managed_thread_root(&start_path) {
        return Err(HeddleError::Config(format!(
            "'{}' is a Heddle-managed virtualized thread mount with no checkout \
             metadata of its own; refusing to operate on the parent repository from \
             inside it. Run heddle from the repository root, or use a solid/materialized \
             thread checkout.",
            mount_root.display()
        )));
    }

    let mut discovered_git_root: Option<PathBuf> = None;
    let mut current: Option<&Path> = Some(start_path.as_path());
    while let Some(dir) = current {
        if discovered_git_root.is_none() && has_git_metadata(dir) {
            discovered_git_root = Some(dir.to_path_buf());
        }

        let heddle_path = dir.join(".heddle");
        if heddle_path.is_dir() {
            if let Some(git_root) = discovered_git_root.as_ref()
                && git_root != dir
                && git_root.starts_with(dir)
                && !git_root.join(".heddle").exists()
            {
                return Ok(fresh_git_overlay_probe(git_root.clone()));
            }

            if let Some(probe) = existing_probe(dir, &heddle_path)? {
                return Ok(probe);
            }
        }

        current = dir.parent();
    }

    if let Some(git_root) = discovered_git_root {
        return Ok(fresh_git_overlay_probe(git_root));
    }

    Ok(RepositoryProbe {
        target: RepositoryProbeTarget::FreshNative,
        root: start_path,
        heddle_dir: None,
        capability: RepositoryCapability::NativeHeddle,
        existing_config: None,
        git_facts: RepositoryProbeGitFacts::default(),
        repo_principal_candidate: None,
    })
}

fn existing_probe(dir: &Path, heddle_path: &Path) -> Result<Option<RepositoryProbe>> {
    let pointer_path = heddle_path.join("objectstore");
    let objects_dir = heddle_path.join("objects");

    let heddle_dir = if pointer_path.is_file() {
        resolve_objectstore_pointer(&pointer_path)?
    } else if objects_dir.is_dir() {
        heddle_path.to_path_buf()
    } else {
        return Ok(None);
    };

    let config_path = heddle_dir.join("config.toml");
    let config = RepoConfig::load(&config_path)?;
    ensure_supported_repo_format(&config_path, &config)?;
    let capability = repository_capability_for_root(dir);
    let git_facts = git_facts_for_root(dir);
    let repo_principal_candidate = repo_principal_candidate(dir, &heddle_dir, &config, capability);

    Ok(Some(RepositoryProbe {
        target: RepositoryProbeTarget::Existing,
        root: dir.to_path_buf(),
        heddle_dir: Some(heddle_dir),
        capability,
        existing_config: Some(config),
        git_facts,
        repo_principal_candidate,
    }))
}

fn fresh_git_overlay_probe(root: PathBuf) -> RepositoryProbe {
    let git_facts = git_facts_for_root(&root);
    let repo_principal_candidate =
        git_config_principal(&root).filter(|principal| !is_default_unknown_principal(principal));
    RepositoryProbe {
        target: RepositoryProbeTarget::FreshGitOverlay,
        root,
        heddle_dir: None,
        capability: RepositoryCapability::GitOverlay,
        existing_config: None,
        git_facts,
        repo_principal_candidate,
    }
}

fn resolve_objectstore_pointer(pointer_path: &Path) -> Result<PathBuf> {
    let content = fs::read_to_string(pointer_path)?;
    let raw_shared = parse_objectstore_pointer(&content).ok_or_else(|| {
        HeddleError::Config(format!(
            "invalid .heddle/objectstore pointer at {}: expected 'objectstore: <path>'",
            pointer_path.display()
        ))
    })?;

    if raw_shared.is_relative() {
        return Err(HeddleError::Config(format!(
            ".heddle/objectstore pointer at {} contains a relative path '{}'; \
             objectstore path must be absolute",
            pointer_path.display(),
            raw_shared.display()
        )));
    }

    let shared = raw_shared.canonicalize().map_err(|e| {
        HeddleError::Config(format!(
            ".heddle/objectstore pointer at {} points to non-existent path '{}': {}",
            pointer_path.display(),
            raw_shared.display(),
            e
        ))
    })?;

    if !shared.join("objects").is_dir() {
        return Err(HeddleError::Config(format!(
            ".heddle/objectstore pointer at {} resolves to '{}' which does not \
             contain an 'objects/' directory; not a valid Heddle store",
            pointer_path.display(),
            shared.display()
        )));
    }

    Ok(shared)
}

fn repo_principal_candidate(
    root: &Path,
    heddle_dir: &Path,
    config: &RepoConfig,
    capability: RepositoryCapability,
) -> Option<Principal> {
    if let Some(config) = &config.principal {
        return Some(Principal::new(&config.name, &config.email));
    }
    if capability == RepositoryCapability::GitOverlay
        && let Some(principal) = git_config_principal(root)
        && !is_default_unknown_principal(&principal)
    {
        return Some(principal);
    }
    shared_checkout_parent_git_principal(root, heddle_dir)
        .filter(|principal| !is_default_unknown_principal(principal))
}

fn shared_checkout_parent_git_principal(root: &Path, heddle_dir: &Path) -> Option<Principal> {
    let local_heddle_dir = root.join(".heddle");
    if local_heddle_dir == heddle_dir || !local_heddle_dir.join("objectstore").is_file() {
        return None;
    }
    let parent_root = heddle_dir.parent()?;
    if parent_root == root {
        return None;
    }
    git_config_principal(parent_root)
}

fn git_facts_for_root(root: &Path) -> RepositoryProbeGitFacts {
    if !has_git_metadata(root) {
        return RepositoryProbeGitFacts::default();
    }
    let Ok(repo) = SleyRepository::discover(root) else {
        return RepositoryProbeGitFacts::default();
    };
    let head = repo.head().ok();
    RepositoryProbeGitFacts {
        root: Some(root.to_path_buf()),
        has_metadata: true,
        has_commits: git_has_commits(&repo),
        is_shallow: repo.git_dir().join("shallow").is_file(),
        head_is_detached: head.as_ref().is_some_and(|head| head.is_detached()),
        head_branch: head
            .as_ref()
            .and_then(|head| head.branch_name())
            .map(str::to_string),
    }
}

fn git_has_commits(repo: &SleyRepository) -> bool {
    if repo.head().ok().and_then(|head| head.oid).is_some() {
        return true;
    }
    let Ok(refs) = repo.references().list_refs() else {
        return false;
    };
    for reference in refs {
        let ReferenceTarget::Direct(oid) = reference.target else {
            continue;
        };
        if object_peels_to_commit(repo, oid) {
            return true;
        }
    }
    false
}

fn object_peels_to_commit(repo: &SleyRepository, mut oid: ObjectId) -> bool {
    loop {
        let Ok(object) = repo.read_object(&oid) else {
            return false;
        };
        match object.object_type {
            GitObjectType::Commit => return true,
            GitObjectType::Tag => {
                let Ok(tag) = repo.read_tag(&oid) else {
                    return false;
                };
                oid = tag.object;
            }
            _ => return false,
        }
    }
}

fn is_default_unknown_principal(principal: &Principal) -> bool {
    principal.name.trim().is_empty()
        || principal.email.trim().is_empty()
        || (principal.name.trim() == "Unknown" && principal.email.trim() == "unknown@example.com")
}
