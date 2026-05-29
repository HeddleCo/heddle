// SPDX-License-Identifier: Apache-2.0
//! Reference manager: threads, markers, HEAD, and packed refs.

use std::path::{Path, PathBuf};

use objects::{
    error::{HeddleError, Result},
    fs_ops::remove_path_recursively,
    object::{ChangeId, MarkerName, ThreadName},
};

use super::{
    backend::CoreRefBackend, format_change_id_text, packed_refs::PackedRefs,
    ref_backend::RefBackend, resolve_refspec, Head, RefExpectation, RefUpdate,
};
use crate::fs_atomic::sync_directory;

/// Well-known refspec that resolves the heddle-internal pre-undo recovery
/// pointer (so `heddle goto .undo-recovery` works). It is UNSHADOWABLE by any
/// user marker or thread, in BOTH directions (heddle#305 r3):
///
/// - **Write side:** the leading `.` is rejected by [`validate_ref_name`], so a
///   user can never `marker create` / `thread` a ref with this name. The
///   recovery state therefore lives in a reserved namespace no user ref can
///   occupy.
/// - **Resolve side:** [`resolve_refspec`] routes this handle to the internal
///   recovery pointer BEFORE consulting user threads/markers, so no user ref
///   can intercept the advertised handle.
///
/// Invariant: an advertised handle for an internal ref must use a reserved form
/// that user-namespace names cannot take — never a bare user-namespace name.
///
/// [`validate_ref_name`]: super::name::validate_ref_name
pub const UNDO_RECOVERY_HANDLE: &str = ".undo-recovery";

/// Manager for references (threads, markers, HEAD).
pub struct RefManager {
    pub(crate) root: PathBuf,
    pub(crate) local_head: Option<PathBuf>,
}

impl RefManager {
    pub fn new(heddle_dir: impl AsRef<Path>) -> Self {
        Self {
            root: heddle_dir.as_ref().to_path_buf(),
            local_head: None,
        }
    }

    pub fn with_local_head(mut self, path: PathBuf) -> Self {
        self.local_head = Some(path);
        self
    }

    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(self.threads_dir())?;
        std::fs::create_dir_all(self.markers_dir())?;
        std::fs::create_dir_all(self.remotes_dir())?;
        Ok(())
    }

    pub fn migrate_legacy_tracks(&self) -> Result<()> {
        let legacy_dir = self.legacy_tracks_dir();
        if !legacy_dir.exists() {
            return Ok(());
        }

        let threads_dir = self.threads_dir();
        if !threads_dir.exists() {
            std::fs::create_dir_all(self.refs_dir())?;
            std::fs::rename(&legacy_dir, &threads_dir)?;
            return Ok(());
        }

        let legacy_threads = self.list_refs_recursive(&legacy_dir, "")?;
        for name in legacy_threads {
            let legacy_path = self.legacy_track_path(&name)?;
            let thread_path = self.thread_path(&name)?;
            if thread_path.exists() {
                continue;
            }

            let parent = thread_path
                .parent()
                .ok_or_else(|| HeddleError::Config(format!("invalid thread path for {}", name)))?;
            std::fs::create_dir_all(parent)?;
            std::fs::rename(&legacy_path, &thread_path)?;
        }

        if legacy_dir.exists() {
            remove_path_recursively(&legacy_dir)?;
        }

        Ok(())
    }

    pub fn cleanup_stale_temps(&self) {
        let refs_dir = self.refs_dir();
        if let Ok(entries) = std::fs::read_dir(&refs_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.starts_with("tmp-"))
                    .unwrap_or(false)
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    pub fn read_head(&self) -> Result<Head> {
        self.read_head_state().map(|state| state.head)
    }

    pub fn write_head(&self, head: &Head) -> Result<()> {
        self.write_head_cas(RefExpectation::Any, head)
    }

    pub fn write_head_cas(&self, expected: RefExpectation<Head>, head: &Head) -> Result<()> {
        self.update_refs(&[RefUpdate::Head {
            expected,
            new: head.clone(),
        }])
    }

    pub fn get_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        let path = self.thread_path(name)?;
        if let Some(id) = self.read_change_id_at(&path, "thread", name)? {
            return Ok(Some(id));
        }
        Ok(PackedRefs::load(&self.packed_refs_path())?.get_thread(name))
    }

    pub fn set_thread(&self, name: &ThreadName, state: &ChangeId) -> Result<()> {
        self.set_thread_cas(name, RefExpectation::Any, state)
    }

    pub fn set_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        self.update_refs(&[RefUpdate::Thread {
            name: name.clone(),
            expected,
            new: Some(*state),
        }])
    }

    pub fn delete_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        let state = self.get_thread(name)?;
        if state.is_some() {
            self.update_refs(&[RefUpdate::Thread {
                name: name.clone(),
                expected: RefExpectation::Any,
                new: None,
            }])?;
        }
        Ok(state)
    }

    pub fn delete_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<()> {
        self.update_refs(&[RefUpdate::Thread {
            name: name.clone(),
            expected,
            new: None,
        }])
    }

    pub fn list_threads(&self) -> Result<Vec<ThreadName>> {
        if let Some(summary) = self.try_read_ref_summary_index() {
            return Ok(summary.thread_names());
        }
        self.list_threads_from_storage()
    }

    pub fn get_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        let path = self.marker_path(name)?;
        if let Some(id) = self.read_change_id_at(&path, "marker", name)? {
            return Ok(Some(id));
        }
        Ok(PackedRefs::load(&self.packed_refs_path())?.get_marker(name))
    }

    pub fn create_marker(&self, name: &MarkerName, state: &ChangeId) -> Result<()> {
        self.set_marker_cas(name, RefExpectation::Missing, state)
    }

    pub fn set_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        self.update_refs(&[RefUpdate::Marker {
            name: name.clone(),
            expected,
            new: Some(*state),
        }])
    }

    pub fn delete_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        let state = self.get_marker(name)?;
        if state.is_some() {
            self.delete_marker_cas(name, RefExpectation::Any)?;
        }
        Ok(state)
    }

    pub fn delete_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<()> {
        self.update_refs(&[RefUpdate::Marker {
            name: name.clone(),
            expected,
            new: None,
        }])
    }

    pub fn list_markers(&self) -> Result<Vec<MarkerName>> {
        if let Some(summary) = self.try_read_ref_summary_index() {
            return Ok(summary.marker_names());
        }
        self.list_markers_from_storage()
    }

    /// Record the heddle-internal pre-undo recovery pointer (ORIG_HEAD-style:
    /// a single rolling ref each undo overwrites). Stored OUTSIDE the
    /// user-writable marker namespace so `marker create/delete` — and their
    /// undo inverses — can never collide with it. See
    /// [`UNDO_RECOVERY_HANDLE`] for the resolution handle.
    pub fn set_undo_recovery(&self, state: &ChangeId) -> Result<()> {
        let _lock = self.lock_refs()?;
        self.write_string(
            &self.undo_recovery_path(),
            &super::format_change_id_text(state),
        )
    }

    /// Read the heddle-internal pre-undo recovery pointer, if one has been
    /// recorded. Returns `None` when no undo has run in this repo.
    pub fn get_undo_recovery(&self) -> Result<Option<ChangeId>> {
        self.read_change_id_at(&self.undo_recovery_path(), "undo recovery", UNDO_RECOVERY_HANDLE)
    }

    pub fn get_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>> {
        let path = self.remote_thread_path(remote, thread)?;
        self.read_change_id_at(&path, "remote thread", &format!("{}/{}", remote, thread))
    }

    pub fn set_remote_thread(
        &self,
        remote: &str,
        thread: &ThreadName,
        state: &ChangeId,
    ) -> Result<()> {
        let _lock = self.lock_refs()?;
        let path = self.remote_thread_path(remote, thread)?;
        let content = format_change_id_text(state);
        let parent = path.parent().ok_or_else(|| {
            HeddleError::Config(format!(
                "invalid remote thread path for {}/{}",
                remote, thread
            ))
        })?;
        std::fs::create_dir_all(parent)?;
        self.write_string(&path, &content)?;
        if self.rebuild_ref_summary_index_with_lock(&_lock).is_err() {
            self.invalidate_ref_summary_index();
        }
        Ok(())
    }

    pub fn delete_remote_thread(
        &self,
        remote: &str,
        thread: &ThreadName,
    ) -> Result<Option<ChangeId>> {
        let _lock = self.lock_refs()?;
        let state = self.get_remote_thread(remote, thread)?;
        if state.is_some() {
            let path = self.remote_thread_path(remote, thread)?;
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(HeddleError::from(e)),
            }
        }
        if self.rebuild_ref_summary_index_with_lock(&_lock).is_err() {
            self.invalidate_ref_summary_index();
        }
        Ok(state)
    }

    pub fn list_remotes(&self) -> Result<Vec<String>> {
        if let Some(summary) = self.try_read_ref_summary_index() {
            return Ok(summary.remote_names());
        }
        self.list_remotes_from_storage()
    }

    pub fn list_remote_threads(&self, remote: &str) -> Result<Vec<ThreadName>> {
        if let Some(summary) = self.try_read_ref_summary_index() {
            return Ok(summary.remote_thread_names(remote));
        }
        self.list_remote_threads_from_storage(remote)
    }

    pub fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let lock = self.lock_refs()?;
        self.update_refs_with_lock(updates, &lock)
    }

    pub fn resolve(&self, refspec: &str) -> Result<Option<ChangeId>> {
        resolve_refspec(
            refspec,
            || self.read_head(),
            |name| self.get_thread(&ThreadName::new(name)),
            |name| self.get_marker(&MarkerName::new(name)),
            || self.get_undo_recovery(),
        )
    }

    pub fn pack_refs(&self) -> Result<()> {
        let lock = self.lock_refs()?;
        let packed_path = self.packed_refs_path();
        let mut packed = PackedRefs::load(&packed_path)?;

        let threads = self.list_threads_from_storage()?;
        for name in &threads {
            let path = self.thread_path(name)?;
            if let Some(id) = self.read_change_id_at(&path, "thread", name)? {
                packed.set_thread(name, id);
            }
        }
        let markers = self.list_markers_from_storage()?;
        for name in &markers {
            let path = self.marker_path(name)?;
            if let Some(id) = self.read_change_id_at(&path, "marker", name)? {
                packed.set_marker(name, id);
            }
        }
        if !packed.is_empty() {
            packed.save(&packed_path)?;
            let packed_parent = packed_path
                .parent()
                .ok_or_else(|| HeddleError::Config("invalid packed-refs path".to_string()))?;
            sync_directory(packed_parent)?;
            for name in &threads {
                let path = self.thread_path(name)?;
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
            for name in &markers {
                let path = self.marker_path(name)?;
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        if self.rebuild_ref_summary_index_with_lock(&lock).is_err() {
            self.invalidate_ref_summary_index();
        }
        drop(lock);
        Ok(())
    }
}

impl CoreRefBackend for RefManager {
    type Error = HeddleError;

    fn read_head(&self) -> Result<Head> {
        RefManager::read_head(self)
    }
    fn write_head(&self, head: &Head) -> Result<()> {
        RefManager::write_head(self, head)
    }
    fn write_head_cas(&self, expected: RefExpectation<Head>, head: &Head) -> Result<()> {
        RefManager::write_head_cas(self, expected, head)
    }
    async fn get_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        RefManager::get_thread(self, name)
    }
    fn set_thread(&self, name: &ThreadName, state: &ChangeId) -> Result<()> {
        RefManager::set_thread(self, name, state)
    }
    fn set_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        RefManager::set_thread_cas(self, name, expected, state)
    }
    fn delete_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        RefManager::delete_thread(self, name)
    }
    fn delete_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<()> {
        RefManager::delete_thread_cas(self, name, expected)
    }
    fn list_threads(&self) -> Result<Vec<ThreadName>> {
        RefManager::list_threads(self)
    }
    async fn get_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        RefManager::get_marker(self, name)
    }
    async fn create_marker(&self, name: &MarkerName, state: &ChangeId) -> Result<()> {
        RefManager::create_marker(self, name, state)
    }
    fn set_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        RefManager::set_marker_cas(self, name, expected, state)
    }
    fn delete_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        RefManager::delete_marker(self, name)
    }
    fn delete_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<()> {
        RefManager::delete_marker_cas(self, name, expected)
    }
    fn list_markers(&self) -> Result<Vec<MarkerName>> {
        RefManager::list_markers(self)
    }
    fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
        RefManager::update_refs(self, updates)
    }
    async fn resolve(&self, refspec: &str) -> Result<Option<ChangeId>> {
        RefManager::resolve(self, refspec)
    }
}

impl RefBackend for RefManager {
    fn get_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>> {
        RefManager::get_remote_thread(self, remote, thread)
    }
    fn set_remote_thread(&self, remote: &str, thread: &ThreadName, state: &ChangeId) -> Result<()> {
        RefManager::set_remote_thread(self, remote, thread, state)
    }
    fn delete_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>> {
        RefManager::delete_remote_thread(self, remote, thread)
    }
    fn list_remotes(&self) -> Result<Vec<String>> {
        RefManager::list_remotes(self)
    }
    fn list_remote_threads(&self, remote: &str) -> Result<Vec<ThreadName>> {
        RefManager::list_remote_threads(self, remote)
    }
    fn inspect_ref_summary_index(&self) -> Result<super::RefSummaryIndexInspection> {
        RefManager::inspect_ref_summary_index(self)
    }
    fn rebuild_ref_summary_index(&self) -> Result<super::RefSummaryIndexInspection> {
        RefManager::rebuild_ref_summary_index(self)
    }
    fn pack_refs(&self) -> Result<()> {
        RefManager::pack_refs(self)
    }
    fn cleanup_stale_temps(&self) {
        RefManager::cleanup_stale_temps(self)
    }
}
