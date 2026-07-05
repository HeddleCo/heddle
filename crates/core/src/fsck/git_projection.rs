// SPDX-License-Identifier: Apache-2.0
//! Fork note: this module mirrors the Git projection note/mapping logic from
//! `crates/cli/src/git_projection_engine/git_notes.rs` and
//! `crates/cli/src/git_projection_engine/git_mapping.rs`. Until Git projection support is fully extracted
//! into `heddle-core`, we keep the behavior aligned (notably required note
//! fields and skip-on-deserialization-failure semantics).
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use objects::{error::Result, object::ChangeId, store::ObjectStore};
use repo::Repository;
use serde::Deserialize;
use sley::{ObjectFormat, ObjectId, Repository as SleyRepository};

use super::{FsckError, invalid_fsck_config, make_error};

const NOTES_REF: &str = "refs/notes/heddle";

pub(crate) fn check_git_projection(
    repo: &Repository,
    errors: &mut Vec<FsckError>,
    warnings: &mut Vec<String>,
    objects_checked: &mut usize,
) -> Result<()> {
    if !mirror_path(repo).exists() {
        warnings.push("legacy Bridge Mirror is absent; mirror-backed Git projection checks were skipped".to_string());
        return Ok(());
    }

    let mirror = open_git_repo(&mirror_path(repo))
        .map_err(|err| invalid_fsck_config(format!("legacy Bridge Mirror open failed: {err}")))?;
    let mapping = build_existing_mapping(repo, &mirror)
        .map_err(|err| invalid_fsck_config(format!("Git Projection Mapping check failed: {err}")))?;

    for (change_id, git_oid) in mapping.iter() {
        *objects_checked += 1;
        if mirror.read_object(git_oid).is_err() {
            errors.push(make_error(
                "git-projection-mapping",
                &format!("mapped Git object {git_oid} is missing from the legacy Bridge Mirror"),
                Some(change_id.to_string()),
            ));
        }
        if repo.store().get_state(change_id)?.is_none() {
            errors.push(make_error(
                "git-projection-mapping",
                &format!("mapped Heddle state {change_id} is missing from the store"),
                Some(git_oid.to_string()),
            ));
        }
    }

    for (git_oid, note) in read_all_notes(&mirror)
        .map_err(|err| invalid_fsck_config(format!("Git projection notes check failed: {err}")))?
    {
        *objects_checked += 1;
        let Ok(change_id) = ChangeId::parse(&note.change_id) else {
            errors.push(make_error(
                "git-projection-notes",
                &format!("note for {git_oid} contains an invalid Heddle change id"),
                Some(note.change_id),
            ));
            continue;
        };
        if mapping.get_git(&change_id) != Some(git_oid) {
            errors.push(make_error(
                "git-projection-notes",
                &format!("note for {git_oid} does not round-trip through Git Projection Mapping"),
                Some(change_id.to_string()),
            ));
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
            "bridge-checkout",
            &format!(
                "checkout branch '{thread}' points at {actual_git_oid}, but Heddle maps the attached thread to {expected_git_oid}"
            ),
            Some(state_id.to_string()),
        ));
    }
    Ok(())
}

fn mirror_path(repo: &Repository) -> PathBuf {
    repo.heddle_dir().join("git")
}

fn mapping_path(repo: &Repository) -> PathBuf {
    repo.heddle_dir()
        .join("git-bridge")
        .join("bridge-mapping.json")
}

fn mapping_tmp_path(repo: &Repository) -> PathBuf {
    mapping_path(repo).with_extension("json.tmp")
}

fn open_git_repo(path: &Path) -> std::result::Result<SleyRepository, String> {
    match SleyRepository::discover(path) {
        Ok(repo) => Ok(repo),
        Err(_) => SleyRepository::open(path).map_err(|err| err.to_string()),
    }
}

fn build_existing_mapping(
    repo: &Repository,
    mirror: &SleyRepository,
) -> std::result::Result<SyncMapping, String> {
    let cache = read_mapping_cache_from_disk(repo)?;
    let mut index = GitIdentityIndex::from_notes(mirror)?;
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
        let change_id = ChangeId::parse(&entry.change_id).map_err(|err| err.to_string())?;
        let git_oid = parse_stored_git_oid(&entry.git_oid)?;
        mapping.insert_checked(change_id, git_oid)?;
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
    change_id: String,
    git_oid: String,
}

#[derive(Debug, Deserialize, Default)]
struct MappingFile {
    entries: Vec<MappingEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SyncMapping {
    heddle_to_git: HashMap<ChangeId, ObjectId>,
    git_to_heddle: HashMap<ObjectId, ChangeId>,
}

impl SyncMapping {
    fn new() -> Self {
        Self::default()
    }

    fn insert(&mut self, change_id: ChangeId, git_oid: ObjectId) {
        if let Some(previous_git) = self.heddle_to_git.remove(&change_id) {
            self.git_to_heddle.remove(&previous_git);
        }
        if let Some(previous_change) = self.git_to_heddle.remove(&git_oid) {
            self.heddle_to_git.remove(&previous_change);
        }
        self.heddle_to_git.insert(change_id, git_oid);
        self.git_to_heddle.insert(git_oid, change_id);
    }

    fn insert_checked(
        &mut self,
        change_id: ChangeId,
        git_oid: ObjectId,
    ) -> std::result::Result<(), String> {
        if let Some(existing) = self.heddle_to_git.get(&change_id)
            && *existing != git_oid
        {
            return Err(format!(
                "change id {} mapped to {} (new {})",
                change_id, existing, git_oid
            ));
        }

        if let Some(existing) = self.git_to_heddle.get(&git_oid)
            && *existing != change_id
        {
            return Err(format!(
                "git oid {} mapped to {} (new {})",
                git_oid, existing, change_id
            ));
        }

        self.insert(change_id, git_oid);
        Ok(())
    }

    fn get_git(&self, change_id: &ChangeId) -> Option<ObjectId> {
        self.heddle_to_git.get(change_id).copied()
    }

    fn has_heddle(&self, change_id: &ChangeId) -> bool {
        self.heddle_to_git.contains_key(change_id)
    }

    fn has_git(&self, git_oid: ObjectId) -> bool {
        self.git_to_heddle.contains_key(&git_oid)
    }

    fn iter(&self) -> impl Iterator<Item = (&ChangeId, &ObjectId)> {
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
        for (change_id, git_oid) in read_identity_mappings(repo)? {
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

fn read_identity_mappings(
    repo: &SleyRepository,
) -> std::result::Result<Vec<(ChangeId, ObjectId)>, String> {
    read_all_notes(repo)?
        .into_iter()
        .map(|(oid, note)| {
            let change_id = ChangeId::parse(&note.change_id).map_err(|err| err.to_string())?;
            Ok((change_id, oid))
        })
        .collect()
}

fn read_all_notes(
    repo: &SleyRepository,
) -> std::result::Result<HashMap<ObjectId, HeddleNote>, String> {
    let mut out = HashMap::new();
    for note_entry in repo
        .list_notes(&notes_ref())
        .map_err(|err| err.to_string())?
    {
        let object = repo
            .read_object(&note_entry.blob)
            .map_err(|err| err.to_string())?;
        if object.object_type != sley::GitObjectType::Blob {
            continue;
        }
        if let Ok(note) = serde_json::from_slice(&object.body) {
            out.insert(note_entry.annotated, note);
        }
    }
    Ok(out)
}

fn notes_ref() -> sley::notes::NotesRef {
    sley::notes::NotesRef::expand(NOTES_REF)
}

#[derive(Debug, Clone, Deserialize)]
struct HeddleNote {
    change_id: String,
    #[allow(dead_code)]
    status: String,
}

#[cfg(test)]
mod tests {
    use objects::{
        object::{Attribution, Principal, State, Tree},
        store::ObjectStore,
    };
    use tempfile::TempDir;

    use super::*;

    fn write_projection_mapping(repo: &Repository, change_id: &str, git_oid: &sley::ObjectId) {
        let mapping_path = repo
            .heddle_dir()
            .join("git-bridge")
            .join("bridge-mapping.json");
        let mapping_parent = mapping_path.parent().expect("mapping path has parent");
        fs::create_dir_all(mapping_parent).expect("create Git projection mapping directory");
        let contents =
            format!(r#"{{"entries":[{{"change_id":"{change_id}","git_oid":"{git_oid}"}}]}}"#);
        std::fs::write(&mapping_path, contents).expect("write Git projection mapping");
    }

    fn write_bridge_note(mirror: &SleyRepository, target: sley::ObjectId, body: &str) {
        let refs = mirror.references();
        let notes_ref = notes_ref();
        let expected_ref =
            sley::notes::notes_ref_expected(&refs, &notes_ref).expect("get notes ref expected");
        let identity = sley::notes::NotesCommitIdentity {
            author: b"heddle test <test@localhost> 0 +0000".to_vec(),
            committer: b"heddle test <test@localhost> 0 +0000".to_vec(),
        };
        sley::notes::upsert_note_bytes_for(
            mirror.git_dir(),
            mirror.object_format(),
            &refs,
            &notes_ref,
            &target,
            body.as_bytes(),
            "heddle: test note",
            &identity,
            expected_ref,
        )
        .expect("write bridge note");
    }

    #[test]
    fn test_git_projection_skips_foreign_note_without_status() {
        let temp = TempDir::new().expect("create temp dir");
        let repo = Repository::init_default(temp.path()).expect("init repo");

        let state_change_id = objects::object::ChangeId::generate();
        let tree = repo.store().put_tree(&Tree::new()).expect("write tree");
        let state = State::new(
            tree,
            Vec::new(),
            Attribution::human(Principal::new("Test User", "test@example.com")),
        )
        .with_change_id(state_change_id);
        let state_change_id = state.change_id.to_string_full();
        repo.store().put_state(&state).expect("store state");

        let mirror_path = repo.heddle_dir().join("git");
        let mirror = SleyRepository::init_bare(&mirror_path).expect("init mirror");
        let foreign_note_target = mirror
            .write_blob("foreign-note")
            .expect("write foreign note blob");
        let valid_note_target = mirror
            .write_blob("valid-note")
            .expect("write valid note blob");
        let foreign_note = format!(
            r#"{{"change_id":"{}"}}"#,
            objects::object::ChangeId::generate()
        );
        let valid_note = format!(
            r#"{{"change_id":"{}","status":"published"}}"#,
            state_change_id
        );
        write_projection_mapping(&repo, &state_change_id, &valid_note_target);
        write_bridge_note(&mirror, foreign_note_target, &foreign_note);
        write_bridge_note(&mirror, valid_note_target, &valid_note);

        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut objects_checked = 0;
        check_git_projection(&repo, &mut errors, &mut warnings, &mut objects_checked)
            .expect("run Git projection check");

        assert!(warnings.is_empty());
        let expected_objects_checked = repo.refs().list_threads().expect("list threads").len() + 2;
        assert!(
            !errors.iter().any(|error| error.kind == "git-projection-notes"),
            "unexpected git-projection-notes errors: {errors:?}",
        );
        assert!(
            !errors.iter().any(|error| error.kind == "git-projection-mapping"),
            "unexpected git-projection-mapping errors: {errors:?}",
        );
        assert_eq!(
            objects_checked, expected_objects_checked,
            "expected mapping + valid note + one check per thread to be counted, got {objects_checked}",
        );
    }
}
