// SPDX-License-Identifier: Apache-2.0
//! Git Projection Mapping and residual integrity checks. Portable note parsing
//! is delegated to `heddle-git-projection`, so fsck observes exactly the same
//! note schema and malformed-note policy as ordinary projection reads.
use std::{collections::HashMap, fs, path::PathBuf};

#[cfg(test)]
use heddle_git_projection::git_notes::NOTES_REF;
use heddle_git_projection::{
    ResidualStore,
    git_export::commit_requires_residual,
    git_notes::{read_all_notes, read_identity_mappings},
};
use objects::{error::Result, object::StateId, store::ObjectStore};
use repo::Repository;
use serde::Deserialize;
use sley::{ObjectFormat, ObjectId, Repository as SleyRepository};

use super::{FsckError, invalid_fsck_config, make_error};

pub(crate) fn check_git_projection(
    repo: &Repository,
    errors: &mut Vec<FsckError>,
    _warnings: &mut Vec<String>,
    objects_checked: &mut usize,
) -> Result<()> {
    let checkout = SleyRepository::discover(repo.root()).ok();
    let mapping = build_existing_mapping(repo, checkout.as_ref()).map_err(|err| {
        invalid_fsck_config(format!("Git Projection Mapping check failed: {err}"))
    })?;
    let residuals = ResidualStore::open(repo.heddle_dir());

    for (state_id, git_oid) in mapping.iter() {
        *objects_checked += 1;
        let Some(state) = repo.store().get_state(state_id)? else {
            errors.push(make_error(
                "git-projection-mapping",
                &format!("mapped Heddle state {state_id} is missing from the store"),
                Some(git_oid.to_string()),
            ));
            continue;
        };
        if commit_requires_residual(&state) {
            match residuals.validate_commit_closure(git_oid.format(), git_oid) {
                Ok(true) => {}
                Ok(false) => errors.push(make_error(
                    "git-projection-residual",
                    &format!(
                        "mapped non-reconstructable Git object {git_oid} is missing Raw Git Object Residual bytes"
                    ),
                    Some(state_id.to_string()),
                )),
                Err(error) => errors.push(make_error(
                    "git-projection-residual",
                    &format!(
                        "mapped non-reconstructable Git object {git_oid} has an invalid Raw Git Object Residual: {error}"
                    ),
                    Some(state_id.to_string()),
                )),
            }
        }
    }

    if let Some(checkout) = &checkout {
        for (git_oid, note) in read_all_notes(checkout).map_err(|err| {
            invalid_fsck_config(format!("Git projection notes check failed: {err}"))
        })? {
            *objects_checked += 1;
            let Ok(state_id) = StateId::parse(&note.state_id) else {
                errors.push(make_error(
                    "git-projection-notes",
                    &format!("note for {git_oid} contains an invalid Heddle change id"),
                    Some(note.state_id),
                ));
                continue;
            };
            if mapping.get_git(&state_id) != Some(git_oid) {
                errors.push(make_error(
                    "git-projection-notes",
                    &format!(
                        "note for {git_oid} does not round-trip through Git Projection Mapping"
                    ),
                    Some(state_id.to_string()),
                ));
            }
        }
    }

    for thread in repo.refs().list_threads()? {
        let Some(state_id) = repo.refs().get_thread(&thread)? else {
            continue;
        };
        *objects_checked += 1;
        if repo.store().get_state(&state_id)?.is_none() {
            errors.push(make_error(
                "git-projection-thread",
                &format!("thread '{thread}' points at a missing state"),
                Some(state_id.to_string()),
            ));
        }
    }

    check_checkout_head(repo, &mapping, errors, objects_checked)?;
    Ok(())
}

fn check_checkout_head(
    repo: &Repository,
    mapping: &SyncMapping,
    errors: &mut Vec<FsckError>,
    objects_checked: &mut usize,
) -> Result<()> {
    let Ok(checkout) = SleyRepository::discover(repo.root()) else {
        return Ok(());
    };
    let refs::Head::Attached { thread } = repo.head_ref()? else {
        return Ok(());
    };
    let Some(state_id) = repo.refs().get_thread(&thread)? else {
        return Ok(());
    };
    let Some(expected_git_oid) = mapping.get_git(&state_id) else {
        return Ok(());
    };
    let branch_ref = format!("refs/heads/{thread}");
    let Ok(Some(reference)) = checkout.find_reference(&branch_ref) else {
        return Ok(());
    };
    let actual_git_oid = reference
        .peeled_oid(&checkout)
        .map_err(|err| invalid_fsck_config(format!("checkout HEAD check failed: {err}")))?
        .ok_or_else(|| invalid_fsck_config("checkout HEAD check failed: branch ref is unborn"))?;
    *objects_checked += 1;
    if actual_git_oid != expected_git_oid {
        errors.push(make_error(
            "git-projection-checkout",
            &format!(
                "checkout branch '{thread}' points at {actual_git_oid}, but Heddle maps the attached thread to {expected_git_oid}"
            ),
            Some(state_id.to_string()),
        ));
    }
    Ok(())
}

fn mapping_path(repo: &Repository) -> PathBuf {
    repo.heddle_dir()
        .join("git-projection")
        .join("git-projection-mapping.json")
}

fn mapping_tmp_path(repo: &Repository) -> PathBuf {
    mapping_path(repo).with_extension("json.tmp")
}

fn build_existing_mapping(
    repo: &Repository,
    checkout: Option<&SleyRepository>,
) -> std::result::Result<SyncMapping, String> {
    let cache = read_mapping_cache_from_disk(repo)?;
    let mut index = match checkout {
        Some(checkout) => GitIdentityIndex::from_notes(checkout)?,
        None => GitIdentityIndex::default(),
    };
    index.fill_gaps_from_cache(&cache);
    Ok(index.into_mapping())
}

fn read_mapping_cache_from_disk(repo: &Repository) -> std::result::Result<SyncMapping, String> {
    recover_mapping_tmp(repo)?;
    let path = mapping_path(repo);
    if !path.exists() {
        return Ok(SyncMapping::new());
    }

    let data = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let file: MappingFile = serde_json::from_str(&data).map_err(|err| err.to_string())?;

    let mut mapping = SyncMapping::new();
    for entry in file.entries {
        let state_id = StateId::parse(&entry.state_id).map_err(|err| err.to_string())?;
        let git_oid = parse_stored_git_oid(&entry.git_oid)?;
        mapping.insert_checked(state_id, git_oid)?;
    }

    Ok(mapping)
}

fn recover_mapping_tmp(repo: &Repository) -> std::result::Result<(), String> {
    let path = mapping_path(repo);
    let tmp_path = mapping_tmp_path(repo);
    if !tmp_path.exists() {
        return Ok(());
    }
    if !path.exists() {
        fs::rename(&tmp_path, &path).map_err(|err| err.to_string())?;
    } else {
        fs::remove_file(&tmp_path).map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn parse_stored_git_oid(value: &str) -> std::result::Result<ObjectId, String> {
    let format = match value.len() {
        40 => ObjectFormat::Sha1,
        64 => ObjectFormat::Sha256,
        _ => return Err(format!("invalid git oid length for {value}")),
    };
    ObjectId::from_hex(format, value).map_err(|err| err.to_string())
}

#[derive(Debug, Deserialize)]
struct MappingEntry {
    state_id: String,
    git_oid: String,
}

#[derive(Debug, Deserialize, Default)]
struct MappingFile {
    entries: Vec<MappingEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SyncMapping {
    heddle_to_git: HashMap<StateId, ObjectId>,
    git_to_heddle: HashMap<ObjectId, StateId>,
}

impl SyncMapping {
    fn new() -> Self {
        Self::default()
    }

    fn insert(&mut self, state_id: StateId, git_oid: ObjectId) {
        if let Some(previous_git) = self.heddle_to_git.remove(&state_id) {
            self.git_to_heddle.remove(&previous_git);
        }
        if let Some(previous_change) = self.git_to_heddle.remove(&git_oid) {
            self.heddle_to_git.remove(&previous_change);
        }
        self.heddle_to_git.insert(state_id, git_oid);
        self.git_to_heddle.insert(git_oid, state_id);
    }

    fn insert_checked(
        &mut self,
        state_id: StateId,
        git_oid: ObjectId,
    ) -> std::result::Result<(), String> {
        if let Some(existing) = self.heddle_to_git.get(&state_id)
            && *existing != git_oid
        {
            return Err(format!(
                "change id {} mapped to {} (new {})",
                state_id, existing, git_oid
            ));
        }

        if let Some(existing) = self.git_to_heddle.get(&git_oid)
            && *existing != state_id
        {
            return Err(format!(
                "git oid {} mapped to {} (new {})",
                git_oid, existing, state_id
            ));
        }

        self.insert(state_id, git_oid);
        Ok(())
    }

    fn get_git(&self, state_id: &StateId) -> Option<ObjectId> {
        self.heddle_to_git.get(state_id).copied()
    }

    fn has_heddle(&self, state_id: &StateId) -> bool {
        self.heddle_to_git.contains_key(state_id)
    }

    fn has_git(&self, git_oid: ObjectId) -> bool {
        self.git_to_heddle.contains_key(&git_oid)
    }

    fn iter(&self) -> impl Iterator<Item = (&StateId, &ObjectId)> {
        self.heddle_to_git.iter()
    }
}

#[derive(Debug, Default)]
struct GitIdentityIndex {
    mapping: SyncMapping,
}

impl GitIdentityIndex {
    fn from_notes(repo: &SleyRepository) -> std::result::Result<Self, String> {
        let mut index = Self::default();
        for (state_id, git_oid) in
            read_identity_mappings(repo).map_err(|error| error.to_string())?
        {
            index.mapping.insert_checked(state_id, git_oid)?;
        }
        Ok(index)
    }

    fn fill_gaps_from_cache(&mut self, cache: &SyncMapping) {
        for (state_id, git_oid) in cache.iter() {
            if self.mapping.get_git(state_id) == Some(*git_oid) {
                continue;
            }
            if self.mapping.has_heddle(state_id) || self.mapping.has_git(*git_oid) {
                continue;
            }
            self.mapping.insert(*state_id, *git_oid);
        }
    }

    fn into_mapping(self) -> SyncMapping {
        self.mapping
    }
}

#[cfg(test)]
fn notes_ref() -> sley::notes::NotesRef {
    sley::notes::NotesRef::expand(NOTES_REF)
}

#[cfg(test)]
mod tests {
    use objects::{
        object::{Attribution, Principal, State, Tree},
        store::ObjectStore,
    };
    use tempfile::TempDir;

    use super::*;

    fn write_projection_mapping(repo: &Repository, state_id: &str, git_oid: &sley::ObjectId) {
        let mapping_path = repo
            .heddle_dir()
            .join("git-projection")
            .join("git-projection-mapping.json");
        let mapping_parent = mapping_path.parent().expect("mapping path has parent");
        fs::create_dir_all(mapping_parent).expect("create Git projection mapping directory");
        let contents =
            format!(r#"{{"entries":[{{"state_id":"{state_id}","git_oid":"{git_oid}"}}]}}"#);
        std::fs::write(&mapping_path, contents).expect("write Git projection mapping");
    }

    fn write_git_note(git: &SleyRepository, target: sley::ObjectId, body: &str) {
        let refs = git.references();
        let notes_ref = notes_ref();
        let expected_ref =
            sley::notes::notes_ref_expected(&refs, &notes_ref).expect("get notes ref expected");
        let identity = sley::notes::NotesCommitIdentity {
            author: b"heddle test <test@localhost> 0 +0000".to_vec(),
            committer: b"heddle test <test@localhost> 0 +0000".to_vec(),
        };
        sley::notes::upsert_note_bytes_for(
            git.git_dir(),
            git.object_format(),
            &refs,
            &notes_ref,
            &target,
            body.as_bytes(),
            "heddle: test note",
            &identity,
            expected_ref,
        )
        .expect("write Git note");
    }

    #[test]
    fn test_git_projection_skips_foreign_note_without_status() {
        let temp = TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");

        let tree = repo.store().put_tree(&Tree::new()).expect("write tree");
        let state = State::new(
            tree,
            Vec::new(),
            Attribution::human(Principal::new("Test User", "test@example.com")),
        );
        let state_state_id = state.state_id.to_string_full();
        let state_change_id = state.change_id.to_string_full();
        repo.store().put_state(&state).expect("store state");

        let git = SleyRepository::init(repo.root()).expect("init Git checkout");
        let foreign_note_target = git
            .write_blob("foreign-note")
            .expect("write foreign note blob");
        let valid_note_target = git.write_blob("valid-note").expect("write valid note blob");
        let foreign_note = format!(
            r#"{{"state_id":"{}"}}"#,
            objects::object::StateId::from_bytes([0x44; 32])
        );
        let valid_note = format!(
            r#"{{"state_id":"{}","change_id":"{}","status":"published"}}"#,
            state_state_id, state_change_id
        );
        write_projection_mapping(&repo, &state_state_id, &valid_note_target);
        write_git_note(&git, foreign_note_target, &foreign_note);
        write_git_note(&git, valid_note_target, &valid_note);

        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut objects_checked = 0;
        check_git_projection(&repo, &mut errors, &mut warnings, &mut objects_checked)
            .expect("run Git projection check");

        assert!(warnings.is_empty());
        let expected_objects_checked = repo.refs().list_threads().expect("list threads").len() + 2;
        assert!(
            !errors
                .iter()
                .any(|error| error.kind == "git-projection-notes"),
            "unexpected git-projection-notes errors: {errors:?}",
        );
        assert!(
            !errors
                .iter()
                .any(|error| error.kind == "git-projection-mapping"),
            "unexpected git-projection-mapping errors: {errors:?}",
        );
        assert_eq!(
            objects_checked, expected_objects_checked,
            "expected mapping + valid note + one check per thread to be counted, got {objects_checked}",
        );
    }

    /// Negative control for #1279: removing residual validation must make this
    /// test fail, because the mapped lossy commit is intentionally left without
    /// its HR01 bytes.
    #[test]
    fn mapped_non_reconstructable_commit_without_residual_fails_fsck() {
        let temp = TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        let tree = repo.store().put_tree(&Tree::new()).expect("write tree");
        let state = State::new(
            tree,
            Vec::new(),
            Attribution::human(Principal::new("Test User", "test@example.com")),
        )
        .with_raw_message("lossy import\n")
        .with_git_lossy(true);
        let state_id = state.state_id.to_string_full();
        repo.store().put_state(&state).expect("store state");
        let git_oid: sley::ObjectId = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            .parse()
            .expect("parse mapped oid");
        write_projection_mapping(&repo, &state_id, &git_oid);

        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut objects_checked = 0;
        check_git_projection(&repo, &mut errors, &mut warnings, &mut objects_checked)
            .expect("run Git projection check");

        assert!(
            errors.iter().any(|error| {
                error.kind == "git-projection-residual"
                    && error
                        .message
                        .contains("missing Raw Git Object Residual bytes")
            }),
            "fsck must fail the mapped non-reconstructable oid when residual bytes are missing: {errors:?}",
        );
    }
}
