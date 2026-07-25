// SPDX-License-Identifier: Apache-2.0
//! Recorded ref mutations routed through the atomic write chokepoint.

use objects::object::MarkerName;
use oplog::RecordedHead;

use super::*;

fn recorded_head(head: &Head) -> RecordedHead {
    match head {
        Head::Attached { thread } => RecordedHead::Attached {
            thread: thread.to_string(),
        },
        Head::Detached { state } => RecordedHead::Detached { state: *state },
    }
}

fn is_conflict(error: &HeddleError) -> bool {
    matches!(error, HeddleError::Conflict(_))
}

impl Repository {
    /// Build the record paired with a direct HEAD ref update.
    pub fn head_update_record(previous: &Head, new: &Head) -> OpRecord {
        OpRecord::HeadUpdate {
            previous: recorded_head(previous),
            new: recorded_head(new),
        }
    }

    /// Set a thread ref and derive its create/update record from the
    /// reconciled value. A concurrent unconditional writer is retried so this
    /// preserves the original last-writer-wins behavior without recording a
    /// stale before-image.
    pub fn set_thread_recorded(&self, name: &ThreadName, state: &StateId) -> Result<()> {
        loop {
            let expected = match self.refs.get_thread(name)? {
                Some(current) if current == *state => return Ok(()),
                Some(current) => RefExpectation::Value(current),
                None => RefExpectation::Missing,
            };
            match self.set_thread_recorded_cas(name, expected, state) {
                Err(error) if is_conflict(&error) => continue,
                result => return result,
            }
        }
    }

    /// CAS variant of [`set_thread_recorded`](Self::set_thread_recorded).
    pub fn set_thread_recorded_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<StateId>,
        state: &StateId,
    ) -> Result<()> {
        let record = match &expected {
            RefExpectation::Missing => OpRecord::ThreadCreate {
                name: name.to_string(),
                state: *state,
                manager_snapshot: None,
            },
            RefExpectation::Value(old_state) if old_state == state => {
                return match self.refs.get_thread(name)? {
                    Some(current) if current == *old_state => Ok(()),
                    current => Err(HeddleError::Conflict(format!(
                        "thread {} expected {}, found {}",
                        name,
                        old_state,
                        current
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "missing".to_string())
                    ))),
                };
            }
            RefExpectation::Value(old_state) => OpRecord::ThreadUpdate {
                name: name.to_string(),
                old_state: *old_state,
                new_state: *state,
                manager_snapshots: None,
            },
            RefExpectation::Any => {
                return self.set_thread_recorded(name, state);
            }
        };
        self.commit_and_publish(
            vec![record],
            &[RefUpdate::Thread {
                name: name.clone(),
                expected,
                new: Some(*state),
            }],
        )
    }

    /// Delete a thread ref, returning its prior value. A disappearing or
    /// concurrently-updated ref is re-read before the record is constructed.
    pub fn delete_thread_recorded(&self, name: &ThreadName) -> Result<Option<StateId>> {
        loop {
            let Some(current) = self.refs.get_thread(name)? else {
                return Ok(None);
            };
            let result = self.commit_and_publish(
                vec![OpRecord::ThreadDelete {
                    name: name.to_string(),
                    state: current,
                }],
                &[RefUpdate::Thread {
                    name: name.clone(),
                    expected: RefExpectation::Value(current),
                    new: None,
                }],
            );
            match result {
                Ok(()) => return Ok(Some(current)),
                Err(error) if is_conflict(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// Publish HEAD with an exact before/after record.
    pub fn write_head_recorded(&self, new: &Head) -> Result<()> {
        loop {
            let previous = self.refs.read_head()?;
            if previous == *new {
                return Ok(());
            }
            let result = self.commit_and_publish(
                vec![Self::head_update_record(&previous, new)],
                &[RefUpdate::Head {
                    expected: RefExpectation::Value(previous),
                    new: new.clone(),
                }],
            );
            match result {
                Err(error) if is_conflict(&error) => continue,
                result => return result,
            }
        }
    }

    /// Create a marker only when absent, recording the same CAS condition.
    pub fn create_marker_recorded(&self, name: &MarkerName, state: &StateId) -> Result<()> {
        self.commit_and_publish(
            vec![OpRecord::MarkerCreate {
                name: name.to_string(),
                state: *state,
            }],
            &[RefUpdate::Marker {
                name: name.clone(),
                expected: RefExpectation::Missing,
                new: Some(*state),
            }],
        )
    }

    /// CAS-update a marker with a reversible delete/create record pair.
    pub fn set_marker_recorded_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<StateId>,
        state: &StateId,
    ) -> Result<()> {
        let records = match &expected {
            RefExpectation::Missing => vec![OpRecord::MarkerCreate {
                name: name.to_string(),
                state: *state,
            }],
            RefExpectation::Value(old_state) if old_state == state => {
                return match self.refs.get_marker(name)? {
                    Some(current) if current == *old_state => Ok(()),
                    current => Err(HeddleError::Conflict(format!(
                        "marker {} expected {}, found {}",
                        name,
                        old_state,
                        current
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "missing".to_string())
                    ))),
                };
            }
            RefExpectation::Value(old_state) => vec![
                OpRecord::MarkerDelete {
                    name: name.to_string(),
                    state: *old_state,
                },
                OpRecord::MarkerCreate {
                    name: name.to_string(),
                    state: *state,
                },
            ],
            RefExpectation::Any => loop {
                let exact = match self.refs.get_marker(name)? {
                    Some(current) if current == *state => return Ok(()),
                    Some(current) => RefExpectation::Value(current),
                    None => RefExpectation::Missing,
                };
                match self.set_marker_recorded_cas(name, exact, state) {
                    Err(error) if is_conflict(&error) => continue,
                    result => return result,
                }
            },
        };
        self.commit_and_publish(
            records,
            &[RefUpdate::Marker {
                name: name.clone(),
                expected,
                new: Some(*state),
            }],
        )
    }

    /// Delete a marker, returning its prior value.
    pub fn delete_marker_recorded(&self, name: &MarkerName) -> Result<Option<StateId>> {
        loop {
            let Some(current) = self.refs.get_marker(name)? else {
                return Ok(None);
            };
            let result = self.commit_and_publish(
                vec![OpRecord::MarkerDelete {
                    name: name.to_string(),
                    state: current,
                }],
                &[RefUpdate::Marker {
                    name: name.clone(),
                    expected: RefExpectation::Value(current),
                    new: None,
                }],
            );
            match result {
                Ok(()) => return Ok(Some(current)),
                Err(error) if is_conflict(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// Publish a remote-tracking thread with its reconciliation record.
    pub fn set_remote_thread_recorded(
        &self,
        remote: &str,
        thread: &ThreadName,
        state: &StateId,
    ) -> Result<()> {
        loop {
            let expected = match self.refs.get_remote_thread(remote, thread)? {
                Some(current) if current == *state => return Ok(()),
                Some(current) => RefExpectation::Value(current),
                None => RefExpectation::Missing,
            };
            let result = self.commit_and_publish(
                vec![OpRecord::RemoteThreadUpdate {
                    remote: remote.to_string(),
                    thread: thread.to_string(),
                    state: *state,
                }],
                &[RefUpdate::RemoteThread {
                    remote: remote.to_string(),
                    thread: thread.clone(),
                    expected,
                    new: Some(*state),
                }],
            );
            match result {
                Err(error) if is_conflict(&error) => continue,
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn test_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn recorded_ref_helpers_publish_with_exact_records() {
        let (_temp, repo) = test_repo();
        let thread = ThreadName::new("feature");
        let first = crate::test_state_id();
        let second = crate::test_state_id();

        repo.set_thread_recorded(&thread, &first).unwrap();
        repo.set_thread_recorded(&thread, &second).unwrap();

        assert_eq!(repo.refs().get_thread(&thread).unwrap(), Some(second));
        let recent = repo.oplog().recent(8).unwrap();
        assert!(recent.iter().any(|entry| matches!(
            &entry.operation,
            OpRecord::ThreadCreate { name, state, .. }
                if name == "feature" && *state == first
        )));
        assert!(recent.iter().any(|entry| matches!(
            &entry.operation,
            OpRecord::ThreadUpdate {
                name,
                old_state,
                new_state,
                ..
            } if name == "feature" && *old_state == first && *new_state == second
        )));
    }

    #[test]
    fn head_and_remote_updates_are_replayable_records() {
        let (_temp, repo) = test_repo();
        let previous = repo.refs().read_head().unwrap();
        let detached = crate::test_state_id();
        repo.write_head_recorded(&Head::Detached { state: detached })
            .unwrap();

        let remote_state = crate::test_state_id();
        let remote_thread = ThreadName::new("main");
        repo.set_remote_thread_recorded("origin", &remote_thread, &remote_state)
            .unwrap();

        assert_eq!(
            repo.refs().read_head().unwrap(),
            Head::Detached { state: detached }
        );
        assert_eq!(
            repo.refs()
                .get_remote_thread("origin", &remote_thread)
                .unwrap(),
            Some(remote_state)
        );
        let recent = repo.oplog().recent(8).unwrap();
        assert!(recent.iter().any(|entry| matches!(
            &entry.operation,
            OpRecord::HeadUpdate {
                previous: recorded_previous,
                new: RecordedHead::Detached { state },
            } if recorded_previous == &recorded_head(&previous) && *state == detached
        )));
        assert!(recent.iter().any(|entry| matches!(
            &entry.operation,
            OpRecord::RemoteThreadUpdate {
                remote,
                thread,
                state,
            } if remote == "origin" && thread == "main" && *state == remote_state
        )));
    }
}
