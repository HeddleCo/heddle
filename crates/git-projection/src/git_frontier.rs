// SPDX-License-Identifier: Apache-2.0
//! Git publish path for synthetic frontier roots.
//!
//! User threads publish to `refs/heads/{thread}`. Synthetic sibling-line
//! roots publish only under `refs/heddle/frontier/<thread>/<full-changeid>`.
//! The two namespaces are disjoint, so a user thread such as `main@hd-…`
//! cannot overwrite a frontier root.

use std::collections::{HashMap, HashSet};

use objects::object::SyntheticFrontierName;
use repo::Repository as HeddleRepository;
use sley::{
    ObjectId as SleyObjectId, RefPrecondition, ReferenceTarget, Repository as SleyRepository,
};

use crate::git_core::{
    delete_reference_if_present, git_err, GitProjectionError, GitProjectionResult, SyncMapping,
};

use super::git_sync::set_ref;

/// Publish a synthetic frontier root into Heddle's reserved Git namespace.
///
/// Replacing an existing ref uses the observed raw target as a CAS
/// precondition so a racing writer is reported instead of overwritten.
pub fn sync_synthetic_frontier_to_git(
    repo: &SleyRepository,
    name: &SyntheticFrontierName,
    git_oid: SleyObjectId,
) -> GitProjectionResult<()> {
    let git_ref = name.git_ref();
    if let Some(existing) = repo.find_reference(&git_ref).map_err(git_err)? {
        let current = match existing.target {
            ReferenceTarget::Direct(oid) => oid,
            ReferenceTarget::Symbolic(_) => {
                return Err(GitProjectionError::Git(format!(
                    "synthetic frontier {git_ref} is symbolic; refuse to overwrite"
                )));
            }
        };
        if current == git_oid {
            return Ok(());
        }
        return set_ref(
            repo,
            &git_ref,
            git_oid,
            RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(current)),
            "heddle: sync synthetic frontier",
        );
    }
    set_ref(
        repo,
        &git_ref,
        git_oid,
        RefPrecondition::MustNotExist,
        "heddle: sync synthetic frontier",
    )
}

/// Materialize advertised synthetic frontier refs and drop stale managed ones.
pub fn reconcile_synthetic_frontier_refs(
    repo: &SleyRepository,
    heddle_repo: &HeddleRepository,
    mapping: &SyncMapping,
    managed_record: &mut HashMap<String, SleyObjectId>,
) -> GitProjectionResult<()> {
    let advertised = heddle_repo.refs().list_synthetic_frontiers()?;
    let mut desired: HashSet<String> = HashSet::new();
    for (name, state) in advertised {
        let git_ref = name.git_ref();
        let Some(git_oid) = mapping.get_git(&state) else {
            continue;
        };
        desired.insert(git_ref.clone());
        sync_synthetic_frontier_to_git(repo, &name, git_oid)?;
        managed_record.insert(git_ref, git_oid);
    }
    let stale: Vec<String> = managed_record
        .keys()
        .filter(|name| name.starts_with("refs/heddle/") && !desired.contains(*name))
        .cloned()
        .collect();
    for git_ref in stale {
        delete_reference_if_present(repo, &git_ref)?;
        managed_record.remove(&git_ref);
    }
    Ok(())
}

/// True when a Git ref name is in the reserved `refs/heddle/` namespace.
pub fn is_reserved_heddle_git_ref(name: &str) -> bool {
    let stripped = name.strip_prefix("refs/").unwrap_or(name);
    objects::object::is_reserved_heddle_namespace(stripped)
        || objects::object::is_reserved_heddle_namespace(name)
}

#[cfg(test)]
mod tests {
    use objects::object::ChangeId;
    use repo::Repository as HeddleRepository;
    use sley::{
        plumbing::{sley_core::ByteString, sley_object::EncodedObject},
        CommitObject, GitObjectType, GitTime, Signature, TreeEditor,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::GitProjection;

    fn test_signature() -> Signature {
        let time = GitTime::new(0, 0);
        let raw = format!("Heddle Test <heddle@test> 0 {}", time.offset_token()).into_bytes();
        Signature {
            name: ByteString::new(b"Heddle Test".to_vec()),
            email: ByteString::new(b"heddle@test".to_vec()),
            time,
            raw,
        }
    }

    fn bare_repo_with_commit() -> (TempDir, SleyRepository, SleyObjectId) {
        let temp = TempDir::new().unwrap();
        let repo = SleyRepository::init_bare(temp.path()).unwrap();
        let oid = write_commit(&repo, "frontier");
        (temp, repo, oid)
    }

    fn write_commit(repo: &SleyRepository, message: &str) -> SleyObjectId {
        let tree = repo.write_tree(TreeEditor::new()).unwrap();
        let sig = test_signature();
        let object = CommitObject {
            tree,
            parents: Vec::new(),
            author: sig.to_ident_bytes(),
            committer: sig.to_ident_bytes(),
            encoding: None,
            message: format!("{message}\n").into_bytes(),
        };
        repo.write_object(EncodedObject::new(GitObjectType::Commit, object.write()))
            .unwrap()
    }

    fn frontier_name() -> SyntheticFrontierName {
        let mut bytes = [0u8; 16];
        bytes[15] = 9;
        SyntheticFrontierName::new("main", ChangeId::from_bytes(bytes)).unwrap()
    }

    #[test]
    fn synthetic_git_ref_does_not_collide_with_user_branch_at_hd_suffix() {
        let (_temp, repo, oid) = bare_repo_with_commit();
        let synthetic = frontier_name();
        let user_branch = format!("refs/heads/main@{}", synthetic.change_id().to_string_full());

        sync_synthetic_frontier_to_git(&repo, &synthetic, oid).unwrap();
        set_ref(
            &repo,
            &user_branch,
            oid,
            RefPrecondition::MustNotExist,
            "test: user thread at hd suffix",
        )
        .unwrap();

        assert_ne!(synthetic.git_ref(), user_branch);
        assert!(synthetic.git_ref().starts_with("refs/heddle/frontier/"));
        assert!(repo.find_reference(&synthetic.git_ref()).unwrap().is_some());
        assert!(repo.find_reference(&user_branch).unwrap().is_some());
    }

    #[test]
    fn replacing_synthetic_git_ref_uses_observed_raw_target() {
        let (_temp, repo, first) = bare_repo_with_commit();
        let second = write_commit(&repo, "retarget");
        let synthetic = frontier_name();

        sync_synthetic_frontier_to_git(&repo, &synthetic, first).unwrap();
        sync_synthetic_frontier_to_git(&repo, &synthetic, second).unwrap();

        let written = repo
            .find_reference(&synthetic.git_ref())
            .unwrap()
            .expect("synthetic ref");
        assert_eq!(written.target, ReferenceTarget::Direct(second));
    }

    #[test]
    fn export_writes_synthetic_frontier_git_ref() {
        let temp = TempDir::new().unwrap();
        let repo = HeddleRepository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("README"), "frontier\n").unwrap();
        let snapshot = repo
            .snapshot_with_attribution(
                Some("seed".to_string()),
                None,
                objects::object::Attribution::human(objects::object::Principal::new(
                    "Test",
                    "test@example.com",
                )),
            )
            .expect("snapshot");
        let synthetic = frontier_name();
        repo.refs()
            .set_synthetic_frontier(&synthetic, &snapshot.state_id)
            .unwrap();

        let mut bridge = GitProjection::new(&repo);
        let dest = temp.path().join("export.git");
        bridge.export_to_path(&dest).expect("export");

        let exported = SleyRepository::open(&dest).expect("open export");
        assert!(
            exported
                .find_reference(&synthetic.git_ref())
                .unwrap()
                .is_some(),
            "export must publish {}",
            synthetic.git_ref()
        );
    }

    #[test]
    fn reconcile_deletes_managed_synthetic_ref_when_desired_is_absent() {
        let (_temp, repo, oid) = bare_repo_with_commit();
        let synthetic = frontier_name();
        sync_synthetic_frontier_to_git(&repo, &synthetic, oid).unwrap();
        let mut managed = HashMap::new();
        managed.insert(synthetic.git_ref(), oid);

        let heddle_temp = TempDir::new().unwrap();
        let heddle = HeddleRepository::init_default(heddle_temp.path()).unwrap();
        reconcile_synthetic_frontier_refs(&repo, &heddle, &SyncMapping::new(), &mut managed)
            .unwrap();

        assert!(repo.find_reference(&synthetic.git_ref()).unwrap().is_none());
        assert!(!managed.contains_key(&synthetic.git_ref()));
    }

    #[test]
    fn reserved_git_ref_classifier_covers_heads_and_heddle_namespaces() {
        assert!(is_reserved_heddle_git_ref(
            "refs/heddle/frontier/main/hc-abc"
        ));
        assert!(is_reserved_heddle_git_ref("heddle/frontier/main/hc-abc"));
        assert!(!is_reserved_heddle_git_ref("refs/heads/main@hd-abc"));
        assert!(!is_reserved_heddle_git_ref("refs/heads/heddle"));
    }
}
