// SPDX-License-Identifier: Apache-2.0
//! Packed refs file for storing cold (infrequently updated) references.

use std::path::Path;

use objects::{
    error::{HeddleError, Result},
    object::ChangeId,
};

use super::PackedRefsModel as CorePackedRefs;
use crate::fs_atomic::write_file_atomic;

#[derive(Clone)]
pub(super) struct PackedRefs {
    inner: CorePackedRefs,
}

impl PackedRefs {
    pub fn new() -> Self {
        Self {
            inner: CorePackedRefs::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }

        let contents = std::fs::read_to_string(path)?;
        Ok(Self {
            inner: CorePackedRefs::parse(&contents),
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| HeddleError::Config("invalid packed-refs path".to_string()))?;
        std::fs::create_dir_all(parent)?;
        let content = self.inner.to_text();
        Ok(write_file_atomic(path, content.as_bytes())?)
    }

    pub fn get_thread(&self, name: &str) -> Option<ChangeId> {
        self.inner.get_thread(name)
    }

    pub fn get_marker(&self, name: &str) -> Option<ChangeId> {
        self.inner.get_marker(name)
    }

    pub fn set_thread(&mut self, name: &str, id: ChangeId) {
        self.inner.set_thread(name, id);
    }

    pub fn set_marker(&mut self, name: &str, id: ChangeId) {
        self.inner.set_marker(name, id);
    }

    pub fn remove_track(&mut self, name: &str) {
        self.inner.remove_track(name);
    }

    pub fn remove_marker(&mut self, name: &str) {
        self.inner.remove_marker(name);
    }

    pub fn list_threads(&self) -> Vec<String> {
        self.inner.list_threads()
    }

    pub fn list_markers(&self) -> Vec<String> {
        self.inner.list_markers()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for PackedRefs {
    fn default() -> Self {
        Self::new()
    }
}
