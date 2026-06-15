// SPDX-License-Identifier: Apache-2.0
//! Persistence and discovery for Git bridge mappings.

use std::{
    collections::HashSet,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use ingest::LossyImportEntry;
use objects::object::ChangeId;
use serde::{Deserialize, Serialize};
use sley::{ObjectFormat, ObjectId as SleyObjectId, ReferenceTarget, Repository as SleyRepository};

use super::git_core::{GitBridge, GitBridgeError, GitResult, git_err};

#[derive(Debug, Serialize, Deserialize)]
struct MappingEntry {
    change_id: String,
    git_oid: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lossy_entries: Vec<LossyImportEntry>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct MappingFile {
    entries: Vec<MappingEntry>,
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

    fn legacy_mapping_path(&self) -> PathBuf {
        self.heddle_repo
            .heddle_dir()
            .join("git")
            .join("bridge-mapping.json")
    }

    fn remove_legacy_mapping_file(&self) -> GitResult<()> {
        let legacy_path = self.legacy_mapping_path();
        if !legacy_path.exists() {
            return Ok(());
        }

        fs::remove_file(&legacy_path)?;
        Ok(())
    }

    fn migrate_legacy_mapping_if_needed(&self) -> GitResult<PathBuf> {
        let path = self.mapping_path();
        let legacy_path = self.legacy_mapping_path();

        if path.exists() {
            self.remove_legacy_mapping_file()?;
            return Ok(path);
        }

        if !legacy_path.exists() {
            return Ok(path);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::rename(&legacy_path, &path)?;
        Ok(path)
    }

    pub(crate) fn load_mapping_from_disk(&mut self) -> GitResult<()> {
        self.recover_mapping_tmp()?;
        let path = self.migrate_legacy_mapping_if_needed()?;
        if !path.exists() {
            return Ok(());
        }

        let data = fs::read_to_string(&path)?;
        let file: MappingFile = serde_json::from_str(&data)
            .map_err(|err| GitBridgeError::InvalidMapping(err.to_string()))?;

        for entry in file.entries {
            let change_id = ChangeId::parse(&entry.change_id)?;
            let git_oid = parse_stored_git_oid(&entry.git_oid)?;
            self.mapping.insert_checked(change_id, git_oid)?;
            self.mapping
                .set_git_lossy_entries(git_oid, entry.lossy_entries);
        }

        Ok(())
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

    fn mapping_bytes(&self) -> GitResult<Vec<u8>> {
        let entries = self
            .mapping
            .iter()
            .map(|(change_id, git_oid)| MappingEntry {
                change_id: change_id.to_string_full(),
                git_oid: git_oid.to_string(),
                lossy_entries: self
                    .mapping
                    .get_git_lossy_entries(*git_oid)
                    .map(|entries| entries.to_vec())
                    .unwrap_or_default(),
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

        let data = self.mapping_bytes()?;
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
        self.remove_legacy_mapping_file()?;
        Ok(())
    }

    pub(crate) fn save_mapping_to_disk(&self) -> GitResult<()> {
        self.write_mapping_tmp_to_disk()?;
        // Fault-injection checkpoint: a crash here leaves the
        // sidecar in tmp form (`bridge-mapping.json.tmp`) without a
        // committed `bridge-mapping.json`. The next `save_mapping_to_disk`
        // call invokes `recover_mapping_tmp` (in `load_mapping_from_disk`)
        // which atomically renames the tmp into place. Tested by
        // `bridge_recovers_from_crash_after_tmp_before_commit`.
        objects::fault_inject::maybe_panic_at("mapping_after_tmp_before_commit");
        self.commit_mapping_tmp_to_disk()
    }

    /// Build the mapping from existing commits and persisted data. Sources,
    /// in order:
    ///   1. The on-disk sidecar (`bridge-mapping.json`).
    ///   2. The git notes ref (`refs/notes/heddle`) — Phase B and later.
    ///   3. Legacy `Heddle-Change-Id:` trailers in commit messages.
    ///
    /// Sources 2 and 3 must agree with anything already in the sidecar (via
    /// `insert_checked`) or the build aborts — this is what catches a
    /// corrupted sidecar that disagrees with the git side of the bridge.
    pub(crate) fn build_existing_mapping(&mut self, git_repo_path: Option<&Path>) -> GitResult<()> {
        self.load_mapping_from_disk()?;

        let repo = match git_repo_path {
            Some(path) => super::git_core::open_repo(path)?,
            None => self.open_git_repo()?,
        };

        // Phase B: scan refs/notes/heddle first. Notes carry change_ids
        // without altering commit SHAs, so they're our preferred fallback
        // source after the sidecar.
        let notes = super::git_notes::read_all_notes(&repo)?;
        for (oid, note) in &notes {
            let change_id = ChangeId::parse(&note.change_id)?;
            self.mapping.insert_checked(change_id, *oid)?;
        }

        // Legacy: scan commit-message trailers for any commits not already
        // covered by the sidecar or notes. Future-proofing for repos that
        // were exported by pre-Phase-B builds.
        let commit_oids = collect_commit_oids(&repo)?;
        for oid in commit_oids {
            if self.mapping.has_git(oid) {
                continue;
            }
            let commit = repo.read_commit(&oid).map_err(git_err)?;
            let message = String::from_utf8_lossy(&commit.message);
            let trailers = GitBridge::parse_trailers(&message);
            if let Some(change_id) = trailers.get(GitBridge::TRAILER_CHANGE_ID) {
                let change_id = ChangeId::parse(change_id)?;
                self.mapping.insert_checked(change_id, oid)?;
            }
        }

        self.save_mapping_to_disk()?;
        Ok(())
    }

    #[cfg_attr(not(feature = "git-overlay"), allow(dead_code))]
    pub(crate) fn prune_unreachable_mapping_entries(&mut self) -> GitResult<usize> {
        let repo = self.open_git_repo()?;
        self.load_mapping_from_disk()?;
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
