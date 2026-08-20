// SPDX-License-Identifier: Apache-2.0
//! Git publish path for synthetic frontier roots.
//!
//! User threads publish to `refs/heads/{thread}`. Synthetic sibling-line
//! roots publish only under `refs/heddle/frontier/<thread>/<full-changeid>`.
//! The two namespaces are disjoint, so a user thread such as `main@hd-…`
//! cannot overwrite a frontier root.

use objects::object::SyntheticFrontierName;
use sley::{ObjectId as SleyObjectId, RefPrecondition, Repository as SleyRepository};

use crate::git_core::{GitProjectionResult, git_err};

use super::git_sync::set_ref;

/// Publish a synthetic frontier root into Heddle's reserved Git namespace.
pub fn sync_synthetic_frontier_to_git(
    repo: &SleyRepository,
    name: &SyntheticFrontierName,
    git_oid: SleyObjectId,
) -> GitProjectionResult<()> {
    let git_ref = name.git_ref();
    if let Some(existing) = repo.find_reference(&git_ref).map_err(git_err)? {
        let current = existing.peeled_oid(repo).map_err(git_err)?;
        if current == Some(git_oid) {
            return Ok(());
        }
        return set_ref(
            repo,
            &git_ref,
            git_oid,
            RefPrecondition::Any,
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

/// True when a Git ref name is in the reserved `refs/heddle/` namespace.
pub fn is_reserved_heddle_git_ref(name: &str) -> bool {
    let stripped = name.strip_prefix("refs/").unwrap_or(name);
    objects::object::is_reserved_heddle_namespace(stripped)
        || objects::object::is_reserved_heddle_namespace(name)
}

#[cfg(test)]
mod tests {
    use objects::object::ChangeId;
    use sley::{
        CommitObject, GitObjectType, GitTime, Signature, TreeEditor,
        plumbing::{sley_core::ByteString, sley_object::EncodedObject},
    };
    use tempfile::TempDir;

    use super::*;

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
        let tree = repo.write_tree(TreeEditor::new()).unwrap();
        let sig = test_signature();
        let object = CommitObject {
            tree,
            parents: Vec::new(),
            author: sig.to_ident_bytes(),
            committer: sig.to_ident_bytes(),
            encoding: None,
            message: b"frontier".to_vec(),
        };
        let oid = repo
            .write_object(EncodedObject::new(GitObjectType::Commit, object.write()))
            .unwrap();
        (temp, repo, oid)
    }

    #[test]
    fn synthetic_git_ref_does_not_collide_with_user_branch_at_hd_suffix() {
        let (_temp, repo, oid) = bare_repo_with_commit();
        let mut bytes = [0u8; 16];
        bytes[15] = 9;
        let change = ChangeId::from_bytes(bytes);
        let synthetic = SyntheticFrontierName::new("main", change).unwrap();
        let user_branch = format!("refs/heads/main@{}", change.to_string_full());

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
    fn reserved_git_ref_classifier_covers_heads_and_heddle_namespaces() {
        assert!(is_reserved_heddle_git_ref(
            "refs/heddle/frontier/main/hc-abc"
        ));
        assert!(is_reserved_heddle_git_ref("heddle/frontier/main/hc-abc"));
        assert!(!is_reserved_heddle_git_ref("refs/heads/main@hd-abc"));
        assert!(!is_reserved_heddle_git_ref("refs/heads/heddle"));
    }
}
