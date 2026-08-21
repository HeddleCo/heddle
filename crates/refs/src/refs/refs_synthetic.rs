// SPDX-License-Identifier: Apache-2.0
//! Dedicated store path for synthetic frontier roots.
//!
//! User threads and markers cannot occupy `heddle/` (the reservation in
//! [`super::name::validate_ref_name`]). Synthetic roots therefore live in
//! their own directory and are addressed by [`SyntheticFrontierName`], never
//! by [`ThreadName`] or [`MarkerName`].

use objects::{
    error::{HeddleError, Result},
    object::{StateId, SyntheticFrontierName},
};

use super::{
    RefManager, format_state_id_text, parse_state_id_text,
    refs_storage::{decode_flat_thread_name, encode_flat_thread_name},
};
use crate::fs_atomic::create_dir_all_durable;

impl RefManager {
    fn synthetic_dir(&self) -> std::path::PathBuf {
        self.refs_dir().join("synthetic")
    }

    fn synthetic_frontier_path(&self, name: &SyntheticFrontierName) -> std::path::PathBuf {
        self.synthetic_dir()
            .join(encode_flat_thread_name(&name.as_name()))
    }

    /// Persist a synthetic frontier root. Does not construct a [`ThreadName`].
    pub fn set_synthetic_frontier(
        &self,
        name: &SyntheticFrontierName,
        state: &StateId,
    ) -> Result<()> {
        self.write_chokepoint(|_lock| {
            let path = self.synthetic_frontier_path(name);
            let parent = path.parent().ok_or_else(|| {
                HeddleError::Config("invalid synthetic frontier path".to_string())
            })?;
            create_dir_all_durable(parent)?;
            self.write_string(&path, &format_state_id_text(state))?;
            Ok(())
        })
    }

    /// Fetch a synthetic frontier root by its type-distinct name.
    pub fn get_synthetic_frontier(&self, name: &SyntheticFrontierName) -> Result<Option<StateId>> {
        let path = self.synthetic_frontier_path(name);
        match self.read_optional_string(&path)? {
            Some(contents) => parse_state_id_text(contents.trim())
                .map(Some)
                .map_err(|error| HeddleError::InvalidObject(error.to_string())),
            None => Ok(None),
        }
    }

    /// List every stored synthetic frontier root.
    pub fn list_synthetic_frontiers(&self) -> Result<Vec<(SyntheticFrontierName, StateId)>> {
        let dir = self.synthetic_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_name = match entry.file_name().to_str() {
                Some(name) => name.to_string(),
                None => continue,
            };
            let Some(decoded) = decode_flat_thread_name(&file_name) else {
                continue;
            };
            let Ok(name) = SyntheticFrontierName::parse(&decoded) else {
                continue;
            };
            let Some(state) = self.get_synthetic_frontier(&name)? else {
                continue;
            };
            out.push((name, state));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{ChangeId, ThreadName};
    use tempfile::TempDir;

    use super::*;
    use crate::refs::fresh_state_id;

    fn cid(last: u8) -> ChangeId {
        let mut bytes = [0u8; 16];
        bytes[15] = last;
        ChangeId::from_bytes(bytes)
    }

    #[test]
    fn prefix_sharing_siblings_are_distinct_and_individually_fetchable() {
        let temp = TempDir::new().unwrap();
        let refs = RefManager::new(temp.path());
        let mut left_bytes = [0xaa; 16];
        left_bytes[15] = 1;
        let mut right_bytes = [0xaa; 16];
        right_bytes[15] = 2;
        let left = SyntheticFrontierName::new("main", ChangeId::from_bytes(left_bytes)).unwrap();
        let right = SyntheticFrontierName::new("main", ChangeId::from_bytes(right_bytes)).unwrap();
        let left_state = fresh_state_id();
        let right_state = fresh_state_id();

        refs.set_synthetic_frontier(&left, &left_state).unwrap();
        refs.set_synthetic_frontier(&right, &right_state).unwrap();

        assert_ne!(left.as_name(), right.as_name());
        assert_eq!(
            refs.get_synthetic_frontier(&left).unwrap(),
            Some(left_state)
        );
        assert_eq!(
            refs.get_synthetic_frontier(&right).unwrap(),
            Some(right_state)
        );
        assert_eq!(refs.list_synthetic_frontiers().unwrap().len(), 2);
    }

    #[test]
    fn user_thread_at_change_id_does_not_overwrite_synthetic_root() {
        let temp = TempDir::new().unwrap();
        let refs = RefManager::new(temp.path());
        let change = cid(7);
        let synthetic = SyntheticFrontierName::new("main", change).unwrap();
        let synthetic_state = fresh_state_id();
        let user_state = fresh_state_id();
        let user = ThreadName::try_new(format!("main@{}", change.to_string_full())).unwrap();

        refs.set_synthetic_frontier(&synthetic, &synthetic_state)
            .unwrap();
        refs.set_thread(&user, &user_state).unwrap();

        assert_eq!(
            refs.get_synthetic_frontier(&synthetic).unwrap(),
            Some(synthetic_state)
        );
        assert_eq!(refs.get_thread(&user).unwrap(), Some(user_state));
        assert!(
            refs.set_thread(&ThreadName::new(synthetic.as_name()), &user_state)
                .is_err()
        );
    }
}
