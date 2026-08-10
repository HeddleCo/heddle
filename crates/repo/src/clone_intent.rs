// SPDX-License-Identifier: Apache-2.0
//! Durable recovery record for an in-progress hosted clone.

use std::{
    fs,
    path::{Path, PathBuf},
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::{create_private_dir_all, write_file_atomic},
};
use serde::{Deserialize, Serialize};

pub const CLONE_INTENT_FILE: &str = "clone-intent.toml";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CloneIntent {
    pub origin: String,
    pub endpoint: String,
    pub repository: String,
    pub thread: Option<String>,
    pub depth: Option<u32>,
    pub lazy: bool,
}

impl CloneIntent {
    pub fn path(root: &Path) -> PathBuf {
        root.join(".heddle").join(CLONE_INTENT_FILE)
    }

    /// Persist the recovery authority before any reconstructible clone data.
    pub fn create(&self, root: &Path) -> Result<()> {
        let heddle_dir = root.join(".heddle");
        create_private_dir_all(&heddle_dir)?;
        let bytes = toml::to_string_pretty(self)?;
        write_file_atomic(&Self::path(root), bytes.as_bytes())?;
        Ok(())
    }

    pub fn load(root: &Path) -> Result<Option<Self>> {
        let path = Self::path(root);
        match fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents)
                .map(Some)
                .map_err(|source| HeddleError::ConfigParse { path, source }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Remove the publication gate after verification, the data commit, and
    /// ref/HEAD publication. A crash may conservatively resurrect the marker;
    /// that only causes another idempotent verification/repair pass.
    pub fn clear(root: &Path) -> Result<()> {
        match fs::remove_file(Self::path(root)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub fn find_clone_intent_root(start: &Path) -> Option<PathBuf> {
    let start = start.canonicalize().ok()?;
    start
        .ancestors()
        .find(|root| CloneIntent::path(root).is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use objects::{
        fs_atomic::CloneDurabilityBatch,
        object::{Attribution, Principal, State, ThreadName, Tree},
        store::ObjectStore,
    };
    use refs::Head;

    use super::*;
    use crate::{Repository, RepositorySourceAuthority};

    fn intent() -> CloneIntent {
        CloneIntent {
            origin: "heddle://127.0.0.1:8443/acme/demo".to_string(),
            endpoint: "127.0.0.1:8443".to_string(),
            repository: "acme/demo".to_string(),
            thread: Some("main".to_string()),
            depth: None,
            lazy: false,
        }
    }

    #[test]
    fn clone_commit_is_one_barrier_and_marker_gates_publication() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("clone");
        intent().create(&root).unwrap();

        let durability = CloneDurabilityBatch::begin(&root);
        let repo = Repository::init_clone(&root, RepositorySourceAuthority::Native).unwrap();
        assert!(!repo.heddle_dir().join("HEAD").exists());
        let canonical_root = root.canonicalize().expect("canonical clone root");
        assert!(matches!(
            Repository::open(&root),
            Err(HeddleError::IncompleteClone(path)) if path == canonical_root
        ));

        let tree = Tree::new();
        let tree_hash = repo.store().put_tree(&tree).unwrap();
        let state = State::new(
            tree_hash,
            Vec::new(),
            Attribution::human(Principal::new("Clone Test", "clone@example.com")),
        );
        repo.store().put_state(&state).unwrap();
        wire::enumerate_state_closure(repo.store(), state.id()).unwrap();

        durability.commit().unwrap();
        assert_eq!(durability.barrier_count(), 1);
        assert!(durability.skipped_barrier_count() > 10);

        repo.set_thread_recorded(&ThreadName::from("main"), &state.id())
            .unwrap();
        repo.refs()
            .write_head(&Head::Attached {
                thread: ThreadName::from("main"),
            })
            .unwrap();
        assert!(matches!(
            Repository::open(&root),
            Err(HeddleError::IncompleteClone(path)) if path == canonical_root
        ));

        CloneIntent::clear(&root).unwrap();
        drop(durability);
        let opened = Repository::open(&root).unwrap();
        assert_eq!(opened.head().unwrap(), Some(state.id()));
    }
}
