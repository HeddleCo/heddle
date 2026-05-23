// SPDX-License-Identifier: Apache-2.0
//! Backend-agnostic refs trait for shared semantics.

use objects::object::ChangeId;

use super::{Head, RefExpectation, RefUpdate, resolve_refspec};

/// Shared refs backend operations usable by both local and hosted backends.
pub trait CoreRefBackend: Send + Sync {
    type Error;

    fn read_head(&self) -> Result<Head, Self::Error>;
    fn write_head(&self, head: &Head) -> Result<(), Self::Error>;
    fn write_head_cas(
        &self,
        expected: RefExpectation<Head>,
        head: &Head,
    ) -> Result<(), Self::Error>;

    fn get_thread(&self, name: &str) -> Result<Option<ChangeId>, Self::Error>;
    fn set_thread(&self, name: &str, state: &ChangeId) -> Result<(), Self::Error>;
    fn set_thread_cas(
        &self,
        name: &str,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<(), Self::Error>;
    fn delete_thread(&self, name: &str) -> Result<Option<ChangeId>, Self::Error>;
    fn delete_thread_cas(
        &self,
        name: &str,
        expected: RefExpectation<ChangeId>,
    ) -> Result<(), Self::Error>;
    fn list_threads(&self) -> Result<Vec<String>, Self::Error>;

    fn get_marker(&self, name: &str) -> Result<Option<ChangeId>, Self::Error>;
    fn create_marker(&self, name: &str, state: &ChangeId) -> Result<(), Self::Error>;
    fn set_marker_cas(
        &self,
        name: &str,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<(), Self::Error>;
    fn delete_marker(&self, name: &str) -> Result<Option<ChangeId>, Self::Error>;
    fn delete_marker_cas(
        &self,
        name: &str,
        expected: RefExpectation<ChangeId>,
    ) -> Result<(), Self::Error>;
    fn list_markers(&self) -> Result<Vec<String>, Self::Error>;

    fn update_refs(&self, updates: &[RefUpdate]) -> Result<(), Self::Error>;

    fn resolve(&self, refspec: &str) -> Result<Option<ChangeId>, Self::Error> {
        resolve_refspec(
            refspec,
            || self.read_head(),
            |name| self.get_thread(name),
            |name| self.get_marker(name),
        )
    }
}
