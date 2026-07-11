// SPDX-License-Identifier: Apache-2.0
//! Remote list/show domain assembly.
//!
//! Pure report types and default-resolution helpers for `heddle remote list`
//! and `heddle remote show`. CLI opens/probes the repo, calls these functions,
//! and renders. Mutation (add/remove/set-default) and network push/pull stay
//! outside this module.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use cli_shared::remote::{RemoteConfig, RemoteTarget};
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::{
    GitConfig, Repository as SleyRepository,
    plumbing::sley_config::{
        ConfigIncludeContext, ConfigOriginKind, ConfigScope, ConfigStack, ConfigStackEntry,
    },
};

/// Machine JSON for `heddle remote list`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteListReport {
    pub output_kind: &'static str,
    pub remotes: Vec<RemoteInfo>,
}

/// One remote entry for list/show machine output.
///
/// Field names match the existing CLI JSON contract (`name`, `url`, `source`,
/// `is_default`). `output_kind` is `Some("remote_show")` for show, omitted on
/// list rows.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_kind: Option<&'static str>,
    pub name: String,
    pub url: String,
    pub source: String,
    pub is_default: bool,
}

impl RemoteListReport {
    pub fn empty() -> Self {
        Self {
            output_kind: "remote_list",
            remotes: Vec::new(),
        }
    }
}

/// List remotes for an opened Heddle repository (merged heddle + git-overlay).
pub fn list_remotes(repo: &Repository) -> Result<RemoteListReport> {
    let items = merged_remote_items(repo)?;
    let default = resolved_default_remote_name(repo)?;
    Ok(RemoteListReport {
        output_kind: "remote_list",
        remotes: items
            .into_iter()
            .map(|(name, (url, source))| {
                let is_default = default.as_deref() == Some(name.as_str());
                RemoteInfo {
                    output_kind: None,
                    name,
                    url,
                    source,
                    is_default,
                }
            })
            .collect(),
    })
}

/// List remotes from a plain-Git worktree root (no Heddle metadata required).
pub fn list_plain_git_remotes(root: &Path) -> RemoteListReport {
    let items = plain_git_remote_items(root);
    let default = default_remote_from_items(&items);
    RemoteListReport {
        output_kind: "remote_list",
        remotes: items
            .into_iter()
            .map(|(name, url)| {
                let is_default = default.as_deref() == Some(name.as_str());
                RemoteInfo {
                    output_kind: None,
                    name,
                    url,
                    source: "git".to_string(),
                    is_default,
                }
            })
            .collect(),
    }
}

/// Show a single remote in a Heddle repository. Returns `Ok(None)` when the
/// name is not present in the merged remote set.
pub fn show_remote(repo: &Repository, name: &str) -> Result<Option<RemoteInfo>> {
    let items = merged_remote_items(repo)?;
    let default = resolved_default_remote_name(repo)?;
    let Some((url, source)) = items.get(name).cloned() else {
        return Ok(None);
    };
    Ok(Some(RemoteInfo {
        output_kind: Some("remote_show"),
        name: name.to_string(),
        url,
        source,
        is_default: default.as_deref() == Some(name),
    }))
}

/// Show a single remote from a plain-Git worktree. Returns `None` when missing.
pub fn show_plain_git_remote(root: &Path, name: &str) -> Option<RemoteInfo> {
    let items = plain_git_remote_items(root);
    let default = default_remote_from_items(&items);
    let url = items.get(name)?.clone();
    Some(RemoteInfo {
        output_kind: Some("remote_show"),
        name: name.to_string(),
        url,
        source: "git".to_string(),
        is_default: default.as_deref() == Some(name),
    })
}

/// Resolve the remote name for push/pull when the user omitted it.
///
/// Falls back to `"origin"` when no configured default exists (legacy CLI
/// contract for explicit transport resolution).
pub fn resolve_default_remote_name(repo: &Repository, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_string());
    }
    if let Some(default) = RemoteConfig::open(repo)
        .map_err(anyhow::Error::new)?
        .default_name()
    {
        return Ok(default.to_string());
    }
    if repo.capability() == RepositoryCapability::GitOverlay
        && let Some(default) = git_overlay_default_remote_name(repo)
    {
        return Ok(default);
    }
    Ok("origin".to_string())
}

/// The configured default remote name, if any (no `"origin"` fallback).
pub fn resolved_default_remote_name(repo: &Repository) -> Result<Option<String>> {
    let cfg = RemoteConfig::open(repo).map_err(anyhow::Error::new)?;
    if let Some(default) = cfg.default_name() {
        return Ok(Some(default.to_string()));
    }
    if repo.capability() == RepositoryCapability::GitOverlay {
        return Ok(git_overlay_default_remote_name(repo));
    }
    Ok(None)
}

/// Merged remote map: name → (url, source label).
///
/// Heddle remotes from `.heddle/remotes.toml` win; git-overlay entries fill
/// gaps. Used by list/show assembly and by mutation commands that need the
/// same visibility set.
pub fn merged_remote_items(repo: &Repository) -> Result<BTreeMap<String, (String, String)>> {
    let cfg = RemoteConfig::open(repo).map_err(anyhow::Error::new)?;
    let git_overlay_remotes = if repo.capability() == RepositoryCapability::GitOverlay {
        git_overlay_config_remotes(repo)
    } else {
        BTreeMap::new()
    };
    let mut items: BTreeMap<String, (String, String)> = cfg
        .list()
        .into_iter()
        .map(|(name, remote)| {
            let source = configured_remote_source(repo, &remote.url);
            (name, (remote.url, source.to_string()))
        })
        .collect();
    if repo.capability() == RepositoryCapability::GitOverlay {
        for (name, url) in git_overlay_remotes {
            items
                .entry(name)
                .or_insert_with(|| (url, "git-overlay".to_string()));
        }
    }
    Ok(items)
}

/// Remotes visible from plain-Git config layers under `root`.
pub fn plain_git_remote_items(root: &Path) -> BTreeMap<String, String> {
    let Some(ctx) = GitConfigContext::discover(root) else {
        return BTreeMap::new();
    };
    ctx.remotes(ctx.layered_paths())
}

fn default_remote_from_items(items: &BTreeMap<String, String>) -> Option<String> {
    if items.contains_key("origin") {
        Some("origin".to_string())
    } else if items.len() == 1 {
        items.keys().next().cloned()
    } else {
        None
    }
}

fn git_overlay_default_remote_name(repo: &Repository) -> Option<String> {
    let git_remotes = git_overlay_config_remotes(repo);
    if let Some(upstream_remote) = git_upstream_remote_name(repo) {
        return Some(upstream_remote);
    }
    if git_remotes.contains_key("origin") {
        return Some("origin".to_string());
    }
    if git_remotes.len() == 1 {
        return git_remotes.keys().next().cloned();
    }
    None
}

fn git_upstream_remote_name(repo: &Repository) -> Option<String> {
    let branch = repo.git_overlay_current_branch().ok().flatten()?;
    let git = SleyRepository::discover(repo.root()).ok()?;
    git.config_snapshot()
        .ok()?
        .get("branch", Some(&branch), "remote")
        .map(str::to_string)
        .filter(|remote| !remote.is_empty())
}

fn git_overlay_config_remotes(repo: &Repository) -> BTreeMap<String, String> {
    let Some(ctx) = GitConfigContext::discover(repo.root()) else {
        return BTreeMap::new();
    };
    let mut paths = ctx.layered_paths();
    paths.push(repo.heddle_dir().join("git").join("config"));
    ctx.remotes(paths)
}

fn configured_remote_source(repo: &Repository, url: &str) -> &'static str {
    if repo.capability() == RepositoryCapability::GitOverlay
        && local_remote_path(url).is_some_and(|path| is_local_git_repository(&path))
    {
        "git-overlay"
    } else {
        "heddle"
    }
}

fn local_remote_path(url: &str) -> Option<PathBuf> {
    match RemoteTarget::parse(url).ok()? {
        RemoteTarget::Local(path) => Some(path),
        RemoteTarget::Network { .. } => None,
    }
}

fn is_local_git_repository(path: &Path) -> bool {
    if path.join(".git").exists() {
        return true;
    }
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

/// Error when a remote write would touch config outside the repo Git tree.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Remote '{name}' is defined in an included Git config that heddle won't edit: {path}")]
pub struct IncludedGitRemoteConfigError {
    pub name: String,
    pub path: PathBuf,
}

impl IncludedGitRemoteConfigError {
    fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
        }
    }
}

/// The resolved Git directory layout for a repository, used to read remote
/// definitions from `.git/config` and its layered companions.
#[derive(Debug, Clone)]
pub struct GitConfigContext {
    git_dir: PathBuf,
    common_dir: PathBuf,
    branch: Option<String>,
}

impl GitConfigContext {
    pub fn discover(root: &Path) -> Option<Self> {
        let git = SleyRepository::discover(root).ok()?;
        Some(Self {
            git_dir: git.git_dir().to_path_buf(),
            common_dir: git.common_dir().to_path_buf(),
            branch: git
                .head()
                .ok()
                .and_then(|head| head.symbolic_target.map(|name| name.to_string()))
                .and_then(|name| name.strip_prefix("refs/heads/").map(str::to_string)),
        })
    }

    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The standard repository config files, ordered highest-precedence first:
    /// the per-worktree `config.worktree` (only when `extensions.worktreeConfig`
    /// is enabled), then the git-dir `config`, then the shared common-dir
    /// `config` for linked worktrees.
    pub fn layered_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if self.worktree_config_enabled() {
            paths.push(self.git_dir.join("config.worktree"));
        }
        paths.push(self.git_dir.join("config"));
        if self.common_dir != self.git_dir {
            paths.push(self.common_dir.join("config"));
        }
        paths
    }

    fn worktree_config_enabled(&self) -> bool {
        let mut paths = vec![self.git_dir.join("config")];
        if self.common_dir != self.git_dir {
            paths.push(self.common_dir.join("config"));
        }
        self.load(paths)
            .and_then(|config| config.get_bool("extensions", None, "worktreeConfig"))
            .unwrap_or(false)
    }

    /// The file a write to remote `name` must target so the next
    /// `remote list` read resolves the value we just wrote.
    pub fn write_file_for(
        &self,
        name: &str,
    ) -> std::result::Result<PathBuf, IncludedGitRemoteConfigError> {
        match self.defining_files_for(name).into_iter().next() {
            Some(path) => {
                if !self.owns_config_file(&path) {
                    return Err(IncludedGitRemoteConfigError::new(name, path));
                }
                Ok(path)
            }
            None => Ok(self.common_dir.join("config")),
        }
    }

    /// Every file that currently defines remote `name`, resolved through
    /// includes. A remove must clear all of them.
    pub fn remove_files_for(
        &self,
        name: &str,
    ) -> std::result::Result<Vec<PathBuf>, IncludedGitRemoteConfigError> {
        let files = self.defining_files_for(name);
        for path in &files {
            if !self.owns_config_file(path) {
                return Err(IncludedGitRemoteConfigError::new(name, path.clone()));
            }
        }
        Ok(files)
    }

    /// The file(s) whose `[remote "<name>"]` section the reader resolves,
    /// following `include.path`/`includeIf`. Returned highest-precedence first.
    pub fn defining_files_for(&self, name: &str) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let Some(stack) = self.config_stack() else {
            return files;
        };
        for entry in stack.entries.iter().rev() {
            if entry.section.eq_ignore_ascii_case("remote")
                && entry.subsection.as_deref() == Some(name)
                && let Some(path) = config_entry_origin_path(entry)
                && !files.contains(&path)
            {
                files.push(path);
            }
        }
        files
    }

    /// Whether heddle may rewrite `path`: only config files within the
    /// repository's own Git directory tree (git-dir / common-dir).
    pub fn owns_config_file(&self, path: &Path) -> bool {
        let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        [&self.git_dir, &self.common_dir].into_iter().any(|root| {
            let root = root.canonicalize().unwrap_or_else(|_| root.clone());
            target.starts_with(&root)
        })
    }

    pub fn remotes(&self, paths: Vec<PathBuf>) -> BTreeMap<String, String> {
        let mut remotes = BTreeMap::new();
        for path in paths {
            let Some(config) = self.load_one(&path, true) else {
                continue;
            };
            for section in &config.sections {
                if !section.name.eq_ignore_ascii_case("remote") {
                    continue;
                }
                let Some(name) = section.subsection.as_deref() else {
                    continue;
                };
                let Some(url) = config_section_value(section, "url") else {
                    continue;
                };
                remotes
                    .entry(name.to_string())
                    .or_insert_with(|| url.to_string());
            }
        }
        remotes
    }

    fn load(&self, paths: Vec<PathBuf>) -> Option<GitConfig> {
        let mut merged = GitConfig::default();
        for path in paths.into_iter().rev() {
            let Some(config) = self.load_one(&path, true) else {
                continue;
            };
            merged.sections.extend(config.sections);
        }
        Some(merged)
    }

    fn config_stack(&self) -> Option<ConfigStack> {
        let context = ConfigIncludeContext {
            git_dir: Some(self.git_dir.clone()),
            current_branch: self.branch.clone(),
        };
        let mut stack = ConfigStack::new();
        for path in self.layered_paths().into_iter().rev() {
            let scope = if path
                .file_name()
                .is_some_and(|name| name == "config.worktree")
            {
                ConfigScope::Worktree
            } else {
                ConfigScope::Local
            };
            stack.push_file(&path, scope, true, &context).ok()?;
        }
        Some(stack)
    }

    fn load_one(&self, path: &Path, follow_includes: bool) -> Option<GitConfig> {
        let bytes = fs::read(path).ok()?;
        let config = GitConfig::parse(&bytes).ok()?;
        if !follow_includes {
            return Some(config);
        }
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        config
            .resolve_includes(
                base,
                &ConfigIncludeContext {
                    git_dir: Some(self.git_dir.clone()),
                    current_branch: self.branch.clone(),
                },
            )
            .ok()
    }
}

fn config_entry_origin_path(entry: &ConfigStackEntry) -> Option<PathBuf> {
    (entry.origin.kind == ConfigOriginKind::File).then(|| PathBuf::from(&entry.origin.name))
}

fn config_section_value<'a>(
    section: &'a sley::plumbing::sley_config::ConfigSection,
    key: &str,
) -> Option<&'a str> {
    section
        .entries
        .iter()
        .rev()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))
        .and_then(|entry| entry.value.as_deref())
}

/// Map a core included-config error into a plain `anyhow` so CLI call sites
/// can attach recovery advice without depending on render types here.
pub fn included_config_error(err: IncludedGitRemoteConfigError) -> anyhow::Error {
    anyhow!(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git(root: &Path) {
        SleyRepository::init(root).expect("init git repo");
    }

    #[test]
    fn parses_quoted_url_with_equals_and_strips_quotes() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        fs::write(
            tmp.path().join(".git").join("config"),
            "[remote \"origin\"]\n\turl = \"https://example.com/repo?ref=main&a=b\"\n",
        )
        .unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("origin").map(String::as_str),
            Some("https://example.com/repo?ref=main&a=b"),
        );
    }

    #[test]
    fn strips_inline_comments_from_url() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        fs::write(
            tmp.path().join(".git").join("config"),
            "[remote \"origin\"]\n\turl = https://example.com/repo ; trailing comment\n",
        )
        .unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("origin").map(String::as_str),
            Some("https://example.com/repo"),
        );
    }

    #[test]
    fn follows_include_directives() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("extra.config"),
            "[remote \"upstream\"]\n\turl = https://example.com/upstream\n",
        )
        .unwrap();
        fs::write(git_dir.join("config"), "[include]\n\tpath = extra.config\n").unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("upstream").map(String::as_str),
            Some("https://example.com/upstream"),
        );
    }

    #[test]
    fn worktree_config_overrides_local_when_extension_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[extensions]\n\tworktreeConfig = true\n\
             [remote \"origin\"]\n\turl = https://example.com/local\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("config.worktree"),
            "[remote \"origin\"]\n\turl = https://example.com/worktree\n",
        )
        .unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("origin").map(String::as_str),
            Some("https://example.com/worktree"),
        );
    }

    #[test]
    fn ignores_worktree_config_when_extension_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://example.com/local\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("config.worktree"),
            "[remote \"origin\"]\n\turl = https://example.com/worktree\n",
        )
        .unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("origin").map(String::as_str),
            Some("https://example.com/local"),
        );
    }

    #[test]
    fn list_plain_git_marks_origin_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        fs::write(
            tmp.path().join(".git").join("config"),
            "[remote \"origin\"]\n\turl = https://example.com/repo\n\
             [remote \"upstream\"]\n\turl = https://example.com/up\n",
        )
        .unwrap();

        let report = list_plain_git_remotes(tmp.path());
        assert_eq!(report.output_kind, "remote_list");
        assert_eq!(report.remotes.len(), 2);
        let origin = report.remotes.iter().find(|r| r.name == "origin").unwrap();
        assert!(origin.is_default);
        assert_eq!(origin.source, "git");
        let upstream = report
            .remotes
            .iter()
            .find(|r| r.name == "upstream")
            .unwrap();
        assert!(!upstream.is_default);
    }

    #[test]
    fn write_file_for_rejects_external_include() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        let external = tmp.path().join("external.config");
        fs::write(
            &external,
            "[remote \"origin\"]\n\turl = https://example.com/external\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("config"),
            format!("[include]\n\tpath = {}\n", external.display()),
        )
        .unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        assert!(ctx.write_file_for("origin").is_err());
        assert!(ctx.remove_files_for("origin").is_err());
    }

    #[test]
    fn defining_files_follow_include_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("extra.config"),
            "[remote \"origin\"]\n\turl = https://example.com/old\n",
        )
        .unwrap();
        fs::write(git_dir.join("config"), "[include]\n\tpath = extra.config\n").unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        let target = ctx.write_file_for("origin").unwrap();
        assert_eq!(target, git_dir.join("extra.config"));
    }
}
