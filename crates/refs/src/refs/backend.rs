// SPDX-License-Identifier: Apache-2.0
//! Backend-agnostic refs trait for shared semantics.

use std::future::Future;

use objects::object::{ChangeId, MarkerName, ThreadName};

use super::{Head, RefExpectation, RefUpdate};

/// Shared refs backend operations usable by both local and hosted backends.
///
/// The database-hitting reads (`get_thread`, `get_marker`, `create_marker`,
/// `resolve`) are `async` so the Postgres backend can `.await` `sqlx`
/// directly instead of bridging through a worker-thread runtime. They're
/// spelled as `-> impl Future + Send` rather than `async fn` so the
/// returned future carries an explicit `Send` bound (required by the
/// hosted server's Tower/tonic stack) and the trait stays clean under
/// `-D warnings` (the `async_fn_in_trait` lint). This is a sealed
/// interface — heddle is the sole implementer — so the lint's
/// downstream-bound concern does not apply.
pub trait CoreRefBackend: Send + Sync {
    type Error;

    fn read_head(&self) -> Result<Head, Self::Error>;
    fn write_head(&self, head: &Head) -> Result<(), Self::Error>;
    fn write_head_cas(
        &self,
        expected: RefExpectation<Head>,
        head: &Head,
    ) -> Result<(), Self::Error>;

    fn get_thread(
        &self,
        name: &ThreadName,
    ) -> impl Future<Output = Result<Option<ChangeId>, Self::Error>> + Send;
    fn set_thread(&self, name: &ThreadName, state: &ChangeId) -> Result<(), Self::Error>;
    fn set_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<(), Self::Error>;
    fn delete_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>, Self::Error>;
    fn delete_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<(), Self::Error>;
    fn list_threads(&self) -> Result<Vec<ThreadName>, Self::Error>;

    fn get_marker(
        &self,
        name: &MarkerName,
    ) -> impl Future<Output = Result<Option<ChangeId>, Self::Error>> + Send;
    fn create_marker(
        &self,
        name: &MarkerName,
        state: &ChangeId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn set_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<(), Self::Error>;
    fn delete_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>, Self::Error>;
    fn delete_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<(), Self::Error>;
    fn list_markers(&self) -> Result<Vec<MarkerName>, Self::Error>;

    fn update_refs(&self, updates: &[RefUpdate]) -> Result<(), Self::Error>;

    fn resolve(
        &self,
        refspec: &str,
    ) -> impl Future<Output = Result<Option<ChangeId>, Self::Error>> + Send {
        async move {
            if refspec == "@" || refspec == "HEAD" {
                // Bind the `read_head()?` result before the `.await` below so
                // the `?`-residual (which carries `Self::Error`) is not held
                // across an await point — keeps the returned future `Send`
                // without bounding `Self::Error: Send`.
                let head = self.read_head()?;
                return match head {
                    Head::Attached { thread } => self.get_thread(&thread).await,
                    Head::Detached { state } => Ok(Some(state)),
                };
            }
            if let Some(id) = self.get_thread(&ThreadName::new(refspec)).await? {
                return Ok(Some(id));
            }
            if let Some(id) = self.get_marker(&MarkerName::new(refspec)).await? {
                return Ok(Some(id));
            }
            if let Ok(id) = ChangeId::parse(refspec) {
                return Ok(Some(id));
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use objects::{
        error::HeddleError,
        object::{ChangeId, MarkerName, ThreadName},
        sync::LockExt,
    };

    use super::CoreRefBackend;
    use crate::refs::{Head, RefBackend, RefExpectation, RefUpdate};

    /// In-memory backend that does **not** override `resolve`, so the
    /// trait's default `resolve` (the only non-Pg, non-`RefManager`
    /// exerciser of that code) gets coverage.
    #[derive(Default)]
    struct MemRefBackend {
        head: Mutex<Option<Head>>,
        threads: Mutex<HashMap<String, ChangeId>>,
        markers: Mutex<HashMap<String, ChangeId>>,
    }

    impl CoreRefBackend for MemRefBackend {
        type Error = HeddleError;

        fn read_head(&self) -> Result<Head, HeddleError> {
            self.head
                .lock_or_poisoned()
                .clone()
                .ok_or_else(|| HeddleError::Config("no head".to_string()))
        }
        fn write_head(&self, head: &Head) -> Result<(), HeddleError> {
            *self.head.lock_or_poisoned() = Some(head.clone());
            Ok(())
        }
        fn write_head_cas(
            &self,
            _expected: RefExpectation<Head>,
            head: &Head,
        ) -> Result<(), HeddleError> {
            self.write_head(head)
        }
        async fn get_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>, HeddleError> {
            Ok(self
                .threads
                .lock_or_poisoned()
                .get(name.as_str())
                .copied())
        }
        fn set_thread(&self, name: &ThreadName, state: &ChangeId) -> Result<(), HeddleError> {
            self.threads
                .lock_or_poisoned()
                .insert(name.as_str().to_string(), *state);
            Ok(())
        }
        fn set_thread_cas(
            &self,
            name: &ThreadName,
            _expected: RefExpectation<ChangeId>,
            state: &ChangeId,
        ) -> Result<(), HeddleError> {
            self.set_thread(name, state)
        }
        fn delete_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>, HeddleError> {
            Ok(self.threads.lock_or_poisoned().remove(name.as_str()))
        }
        fn delete_thread_cas(
            &self,
            _name: &ThreadName,
            _expected: RefExpectation<ChangeId>,
        ) -> Result<(), HeddleError> {
            Ok(())
        }
        fn list_threads(&self) -> Result<Vec<ThreadName>, HeddleError> {
            Ok(self
                .threads
                .lock_or_poisoned()
                .keys()
                .map(|k| ThreadName::new(k.as_str()))
                .collect())
        }
        async fn get_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>, HeddleError> {
            Ok(self
                .markers
                .lock_or_poisoned()
                .get(name.as_str())
                .copied())
        }
        async fn create_marker(
            &self,
            name: &MarkerName,
            state: &ChangeId,
        ) -> Result<(), HeddleError> {
            self.markers
                .lock_or_poisoned()
                .insert(name.as_str().to_string(), *state);
            Ok(())
        }
        fn set_marker_cas(
            &self,
            name: &MarkerName,
            _expected: RefExpectation<ChangeId>,
            state: &ChangeId,
        ) -> Result<(), HeddleError> {
            self.markers
                .lock_or_poisoned()
                .insert(name.as_str().to_string(), *state);
            Ok(())
        }
        fn delete_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>, HeddleError> {
            Ok(self.markers.lock_or_poisoned().remove(name.as_str()))
        }
        fn delete_marker_cas(
            &self,
            _name: &MarkerName,
            _expected: RefExpectation<ChangeId>,
        ) -> Result<(), HeddleError> {
            Ok(())
        }
        fn list_markers(&self) -> Result<Vec<MarkerName>, HeddleError> {
            Ok(self
                .markers
                .lock_or_poisoned()
                .keys()
                .map(|k| MarkerName::new(k.as_str()))
                .collect())
        }
        fn update_refs(&self, _updates: &[RefUpdate]) -> Result<(), HeddleError> {
            Ok(())
        }
        // `resolve` deliberately left unimplemented — exercises the default.
    }

    /// Implements only the required remote methods, leaving
    /// `commit_and_publish` (and the other maintenance methods) on the trait
    /// default — the exerciser for the default `commit_and_publish` fail-closed
    /// path.
    impl RefBackend for MemRefBackend {
        fn get_remote_thread(
            &self,
            _remote: &str,
            _thread: &ThreadName,
        ) -> Result<Option<ChangeId>, HeddleError> {
            Ok(None)
        }
        fn set_remote_thread(
            &self,
            _remote: &str,
            _thread: &ThreadName,
            _state: &ChangeId,
        ) -> Result<(), HeddleError> {
            Ok(())
        }
        fn delete_remote_thread(
            &self,
            _remote: &str,
            _thread: &ThreadName,
        ) -> Result<Option<ChangeId>, HeddleError> {
            Ok(None)
        }
        fn list_remotes(&self) -> Result<Vec<String>, HeddleError> {
            Ok(Vec::new())
        }
        fn list_remote_threads(&self, _remote: &str) -> Result<Vec<ThreadName>, HeddleError> {
            Ok(Vec::new())
        }
    }

    /// The default `commit_and_publish` (no record committer) must fail closed
    /// when handed records (heddle#354 r9, cid 3330304656): committed data can
    /// never be silently dropped while the ref batch is published anyway. A
    /// record-free publish still succeeds.
    #[test]
    fn default_commit_and_publish_fails_closed_on_records() {
        let backend = MemRefBackend::default();
        assert!(
            RefBackend::commit_and_publish(&backend, &[vec![1u8, 2, 3]], &[], None).is_err(),
            "records with no committer must fail closed, not be silently dropped"
        );
        RefBackend::commit_and_publish(&backend, &[], &[], None)
            .expect("a record-free publish has nothing to lose and must succeed");
    }

    #[test]
    fn default_resolve_covers_every_branch() {
        let backend = MemRefBackend::default();
        let thread_id = ChangeId::generate();
        let marker_id = ChangeId::generate();
        let detached_id = ChangeId::generate();
        let parseable_id = ChangeId::generate();

        backend
            .set_thread(&ThreadName::new("main"), &thread_id)
            .unwrap();
        backend
            .set_marker_cas(&MarkerName::new("v1"), RefExpectation::Any, &marker_id)
            .unwrap();

        // "@" / "HEAD" with an attached HEAD resolves through get_thread.
        backend
            .write_head(&Head::Attached {
                thread: ThreadName::new("main"),
            })
            .unwrap();
        assert_eq!(
            pollster::block_on(backend.resolve("@")).unwrap(),
            Some(thread_id)
        );
        assert_eq!(
            pollster::block_on(backend.resolve("HEAD")).unwrap(),
            Some(thread_id)
        );

        // "@" with a detached HEAD returns the pinned state directly.
        backend
            .write_head(&Head::Detached { state: detached_id })
            .unwrap();
        assert_eq!(
            pollster::block_on(backend.resolve("@")).unwrap(),
            Some(detached_id)
        );

        // Bare thread name, then bare marker name, then a raw ChangeId.
        assert_eq!(
            pollster::block_on(backend.resolve("main")).unwrap(),
            Some(thread_id)
        );
        assert_eq!(
            pollster::block_on(backend.resolve("v1")).unwrap(),
            Some(marker_id)
        );
        assert_eq!(
            pollster::block_on(backend.resolve(&parseable_id.to_string_full())).unwrap(),
            Some(parseable_id)
        );

        // Unknown refspec that is neither a thread, marker, nor ChangeId.
        assert_eq!(
            pollster::block_on(backend.resolve("no-such-ref")).unwrap(),
            None
        );
    }

    #[test]
    fn mem_backend_exercises_full_trait_surface() {
        let backend = MemRefBackend::default();
        let thread_id = ChangeId::generate();
        let marker_id = ChangeId::generate();
        let head_id = ChangeId::generate();

        // write_head_cas writes through to the head slot.
        let head = Head::Detached { state: head_id };
        backend.write_head_cas(RefExpectation::Any, &head).unwrap();
        assert_eq!(backend.read_head().unwrap(), head);

        // set_thread_cas inserts; list_threads + get_thread observe it.
        let main = ThreadName::new("main");
        backend
            .set_thread_cas(&main, RefExpectation::Missing, &thread_id)
            .unwrap();
        assert_eq!(
            backend.list_threads().unwrap(),
            vec![ThreadName::new("main")]
        );
        assert_eq!(
            pollster::block_on(backend.get_thread(&main)).unwrap(),
            Some(thread_id)
        );

        // delete_thread returns the removed value; delete_thread_cas is a
        // no-op that still reports success.
        assert_eq!(backend.delete_thread(&main).unwrap(), Some(thread_id));
        assert_eq!(backend.delete_thread(&main).unwrap(), None);
        backend
            .delete_thread_cas(&main, RefExpectation::Any)
            .unwrap();
        assert!(backend.list_threads().unwrap().is_empty());

        // create_marker (async) inserts; list_markers + get_marker observe it.
        let tag = MarkerName::new("v1");
        pollster::block_on(backend.create_marker(&tag, &marker_id)).unwrap();
        assert_eq!(backend.list_markers().unwrap(), vec![MarkerName::new("v1")]);
        assert_eq!(
            pollster::block_on(backend.get_marker(&tag)).unwrap(),
            Some(marker_id)
        );

        // delete_marker returns the removed value; delete_marker_cas is a
        // no-op that still reports success.
        assert_eq!(backend.delete_marker(&tag).unwrap(), Some(marker_id));
        assert_eq!(backend.delete_marker(&tag).unwrap(), None);
        backend
            .delete_marker_cas(&tag, RefExpectation::Any)
            .unwrap();
        assert!(backend.list_markers().unwrap().is_empty());

        // update_refs accepts the batch shape without error.
        backend
            .update_refs(&[RefUpdate::Thread {
                name: ThreadName::new("main"),
                expected: RefExpectation::Any,
                new: Some(thread_id),
            }])
            .unwrap();
    }
}
