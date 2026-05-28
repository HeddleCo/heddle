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
