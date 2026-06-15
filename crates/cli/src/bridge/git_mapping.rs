// SPDX-License-Identifier: Apache-2.0
//! Persistence and discovery for Git bridge mappings.

use std::{
    collections::HashSet,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use objects::object::ChangeId;
use serde::{Deserialize, Serialize};
use sley::{ObjectFormat, ObjectId as SleyObjectId, ReferenceTarget, Repository as SleyRepository};

use super::git_core::{GitBridge, GitBridgeError, GitResult, SyncMapping, git_err};

#[derive(Debug, Serialize, Deserialize)]
struct MappingEntry {
    change_id: String,
    git_oid: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct MappingFile {
    entries: Vec<MappingEntry>,
}

#[derive(Debug, Default)]
struct GitIdentityIndex {
    mapping: SyncMapping,
}

impl GitIdentityIndex {
    fn from_notes(repo: &SleyRepository) -> GitResult<Self> {
        let mut index = Self::default();
        for (change_id, git_oid) in super::git_notes::read_identity_mappings(repo)? {
            index.mapping.insert_checked(change_id, git_oid)?;
        }
        Ok(index)
    }

    fn fill_gaps_from_cache(&mut self, cache: &SyncMapping) {
        for (change_id, git_oid) in cache.iter() {
            if self.mapping.get_git(change_id) == Some(*git_oid) {
                continue;
            }
            if self.mapping.has_heddle(change_id) || self.mapping.has_git(*git_oid) {
                continue;
            }
            self.mapping.insert(*change_id, *git_oid);
        }
    }

    fn into_mapping(self) -> SyncMapping {
        self.mapping
    }
}

impl<'a> GitBridge<'a> {
    pub(crate) fn mapping_path(&self) -> PathBuf {
        self.heddle_repo
            .heddle_dir()
            .join("git-bridge")
            .join("bridge-mapping.json")
    }

    pub(crate) fn mapping_tmp_path(&self) -> PathBuf {
        self.mapping_path().with_extension("json.tmp")
    }

    fn read_mapping_cache_from_disk(&self) -> GitResult<SyncMapping> {
        self.recover_mapping_tmp()?;
        let path = self.mapping_path();
        if !path.exists() {
            return Ok(SyncMapping::new());
        }

        let data = fs::read_to_string(&path)?;
        let file: MappingFile = serde_json::from_str(&data)
            .map_err(|err| GitBridgeError::InvalidMapping(err.to_string()))?;

        let mut mapping = SyncMapping::new();
        for entry in file.entries {
            let change_id = ChangeId::parse(&entry.change_id)?;
            let git_oid = parse_stored_git_oid(&entry.git_oid)?;
            mapping.insert_checked(change_id, git_oid)?;
        }

        Ok(mapping)
    }

    fn recover_mapping_tmp(&self) -> GitResult<()> {
        let path = self.mapping_path();
        let tmp_path = self.mapping_tmp_path();
        if !tmp_path.exists() {
            return Ok(());
        }
        if !path.exists() {
            fs::rename(&tmp_path, &path)?;
        } else {
            fs::remove_file(&tmp_path)?;
        }
        Ok(())
    }

    fn mapping_bytes(mapping: &SyncMapping) -> GitResult<Vec<u8>> {
        let entries = mapping
            .iter()
            .map(|(change_id, git_oid)| MappingEntry {
                change_id: change_id.to_string_full(),
                git_oid: git_oid.to_string(),
            })
            .collect();

        let file = MappingFile { entries };
        serde_json::to_vec_pretty(&file)
            .map_err(|err| GitBridgeError::InvalidMapping(err.to_string()))
    }

    pub(crate) fn write_mapping_tmp_to_disk(&self) -> GitResult<PathBuf> {
        let path = self.mapping_path();
        let tmp_path = self.mapping_tmp_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            let parent_file = File::open(parent)?;
            parent_file.sync_all()?;
        }

        let data = Self::mapping_bytes(&self.mapping)?;
        let mut file = File::create(&tmp_path)?;
        file.write_all(&data)?;
        file.sync_all()?;
        Ok(tmp_path)
    }

    pub(crate) fn commit_mapping_tmp_to_disk(&self) -> GitResult<()> {
        let path = self.mapping_path();
        let tmp_path = self.mapping_tmp_path();
        if !tmp_path.exists() {
            return Err(GitBridgeError::InvalidMapping(format!(
                "mapping temp file is missing: {}",
                tmp_path.display()
            )));
        }
        fs::rename(&tmp_path, &path)?;
        if let Some(parent) = path.parent() {
            let parent_file = File::open(parent)?;
            parent_file.sync_all()?;
        }
        Ok(())
    }

    pub(crate) fn save_mapping_to_disk(&self) -> GitResult<()> {
        self.write_mapping_tmp_to_disk()?;
        // Fault-injection checkpoint: a crash here leaves the
        // sidecar in tmp form (`bridge-mapping.json.tmp`) without a
        // committed `bridge-mapping.json`. The next mapping-cache read
        // atomically renames the tmp into place. Tested by
        // `bridge_recovers_from_crash_after_tmp_before_commit`.
        objects::fault_inject::maybe_panic_at("mapping_after_tmp_before_commit");
        self.commit_mapping_tmp_to_disk()
    }

    /// Build the identity mapping from portable metadata and local cache.
    /// `refs/notes/heddle` is authoritative because it travels with Git history;
    /// `bridge-mapping.json` is a rebuildable cache that fills entries with no
    /// note yet (for example, before a commit has been exported with notes).
    pub(crate) fn build_existing_mapping(&mut self, git_repo_path: Option<&Path>) -> GitResult<()> {
        let repo = match git_repo_path {
            Some(path) => super::git_core::open_repo(path)?,
            None => self.open_git_repo()?,
        };

        let cache = self.read_mapping_cache_from_disk()?;
        let live_cache = self.mapping.clone();
        let mut index = GitIdentityIndex::from_notes(&repo)?;
        index.fill_gaps_from_cache(&cache);
        index.fill_gaps_from_cache(&live_cache);
        self.mapping = index.into_mapping();

        self.save_mapping_to_disk()?;
        Ok(())
    }

    #[cfg_attr(not(feature = "git-overlay"), allow(dead_code))]
    pub(crate) fn prune_unreachable_mapping_entries(&mut self) -> GitResult<usize> {
        let repo = self.open_git_repo()?;
        self.mapping = self.read_mapping_cache_from_disk()?;
        let reachable: HashSet<_> = collect_commit_oids(&repo)?.into_iter().collect();
        let removed = self.mapping.retain_git_object_set(&reachable);
        if removed > 0 {
            self.save_mapping_to_disk()?;
        }
        Ok(removed)
    }
}

/// Walk all branch- and tag-tipped commit ancestry. Skips refs that peel
/// to non-commit objects (annotated-tag-points-at-blob/tree), matching the
/// marker model's current commit-target-only constraint.
fn collect_commit_oids(repo: &SleyRepository) -> GitResult<Vec<SleyObjectId>> {
    let mut tips = Vec::new();

    for reference in repo.references().list_refs().map_err(git_err)? {
        if !(reference.name.starts_with("refs/heads/") || reference.name.starts_with("refs/tags/"))
        {
            continue;
        }
        let oid = match reference.target {
            ReferenceTarget::Direct(oid) => oid,
            ReferenceTarget::Symbolic(_) => {
                let Some(reference) = repo.find_reference(&reference.name).map_err(git_err)? else {
                    continue;
                };
                let Some(oid) = reference.peeled_oid(repo).map_err(git_err)? else {
                    continue;
                };
                oid
            }
        };
        if let Ok(commit_oid) = sley::plumbing::sley_rev::peel_to_commit(
            repo.objects().as_ref(),
            repo.object_format(),
            &oid,
        ) {
            tips.push(commit_oid);
        }
    }

    let mut seen = HashSet::new();
    let mut stack = tips;
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let commit = repo.read_commit(&oid).map_err(git_err)?;
        stack.extend(commit.parents);
    }

    Ok(seen.into_iter().collect())
}

fn parse_stored_git_oid(value: &str) -> GitResult<SleyObjectId> {
    let format = match value.len() {
        40 => ObjectFormat::Sha1,
        64 => ObjectFormat::Sha256,
        _ => {
            return Err(GitBridgeError::InvalidMapping(format!(
                "invalid git oid length for {value}"
            )));
        }
    };
    SleyObjectId::from_hex(format, value)
        .map_err(|err| GitBridgeError::InvalidMapping(err.to_string()))
}
