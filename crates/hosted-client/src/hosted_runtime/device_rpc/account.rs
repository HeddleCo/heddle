//! Account-level device routes are admitted before any exact-Spool dispatcher.
use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use api::heddle::api::{
    v1alpha1::{CallContext, CallFailureCode},
    v2alpha1::*,
};
use prost::Message;

use super::{
    DeviceRpc, account_auth, account_feed::AccountFeed, failure, stream::ObservationAuthority,
};
pub(super) const METHODS: &[&str] = &[
    "/heddle.api.v2alpha1.WorkspaceService/ObserveWorkspace",
    "/heddle.api.v2alpha1.WorkspaceService/ResolveResources",
    "/heddle.api.v2alpha1.WorkspaceService/SetBookmark",
    "/heddle.api.v2alpha1.SpoolService/ObserveSpool",
    "/heddle.api.v2alpha1.SpoolService/CreateSpool",
    "/heddle.api.v2alpha1.SpoolService/ReviseSpool",
    "/heddle.api.v2alpha1.SpoolService/DeleteSpool",
    "/heddle.api.v2alpha1.SpoolService/SetSpoolMount",
    "/heddle.api.v2alpha1.SpoolService/RemoveSpoolMount",
    "/heddle.api.v2alpha1.IdentityService/ObserveIdentity",
    "/heddle.api.v2alpha1.IdentityService/IntrospectCredential",
    "/heddle.api.v2alpha1.OwnerAuthorizationService/ObserveOwnership",
    "/heddle.api.v2alpha1.ThreadService/ObserveThreads",
];
impl DeviceRpc {
    pub(super) fn account_feed(&self) -> Result<Arc<AccountFeed>> {
        let mut slot = self
            .account_feed
            .lock()
            .map_err(|_| anyhow::anyhow!("account feed guard poisoned"))?;
        if let Some(feed) = Weak::upgrade(&slot) {
            return Ok(feed);
        }
        let feed = Arc::new(AccountFeed::new(&self.home)?);
        *slot = Arc::downgrade(&feed);
        Ok(feed)
    }
    pub(super) async fn serve_account(
        &self,
        method: &str,
        context: &CallContext,
        body: &[u8],
        mut send: iroh::endpoint::SendStream,
        budget: &mut super::super::hosted::claim_protocol::CallBudget,
    ) -> Result<()> {
        let descriptor = api::v2::method_descriptor(method).context("account method")?;
        let prepared = (|| -> Result<_> {
            let mut session = account_auth::authorize(&self.home, descriptor, context, body)?;
            let catalog = repo::device_catalog::store::Catalog::read(&self.home)?;
            let paths = catalog
                .as_ref()
                .map(|c| c.registrations())
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .map(|s| s.capability_path);
            session.bind_scopes(paths)?;
            session.finish_admission(&self.home)?;
            Ok(session)
        })();
        let session = match prepared {
            Ok(session) => session,
            Err(error) => {
                let failure = failure(CallFailureCode::Unauthenticated, error);
                let bytes = if descriptor.streaming == api::StreamingShape::ServerStreaming {
                    api::framing::encode_stream_failure(&failure)?
                } else {
                    api::framing::encode_failure_response(&failure)?
                };
                send.write_all(&bytes).await?;
                send.finish()?;
                return Ok(());
            }
        };
        if descriptor.streaming == api::StreamingShape::ServerStreaming {
            budget.retain().map_err(anyhow::Error::msg)?;
            return self.observe_account(&session, method, body, send).await;
        }
        let result = if method.ends_with("/ResolveResources") {
            self.resolve_local_resources(&session, &ResolveResourcesRequest::decode(body)?)
                .map(|r| r.encode_to_vec())
        } else {
            self.account_command(&session, method, body)
        };
        let result = result.and_then(|response| {
            session.check_current(&self.home)?;
            Ok(response)
        });
        let bytes = match result {
            Ok(response) => api::framing::encode_success_response(&response)?,
            Err(error) => api::framing::encode_failure_response(&failure(
                CallFailureCode::FailedPrecondition,
                error,
            ))?,
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), send.write_all(&bytes)).await??;
        send.finish()?;
        Ok(())
    }
}
