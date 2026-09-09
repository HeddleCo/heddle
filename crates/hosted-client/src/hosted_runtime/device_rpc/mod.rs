//! Native direct browser/device RPCs. Device authority is locally admitted;
//! no handler creates a hosted client or consults Weft to permit local work.
mod account;
mod account_auth;
mod account_feed;
mod account_identity;
mod account_observe;
mod account_resolve;
mod account_spool;
#[cfg(test)]
mod account_tests;
mod account_threads;
mod artifact;
#[cfg(test)]
mod artifact_tests;
mod auth;
#[cfg(test)]
mod capacity_tests;
mod checkout;
#[cfg(test)]
mod inventory_tests;
mod land;
mod observe;
#[cfg(test)]
mod receipt_tests;
mod replication;
mod stream;
#[cfg(test)]
mod tests;
mod thread;
mod thread_observe;
#[cfg(test)]
mod thread_tests;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

use anyhow::{Context, Result, bail};
use api::heddle::api::{
    v1alpha1::{CallContext, CallFailure, CallFailureCode},
    v2alpha1::*,
};
use iroh::endpoint::SendStream;
use prost::Message;

pub(crate) const STREAM_METHODS: &[&str] = &["/heddle.api.v2alpha1.SyncService/ReplicateThread"];
pub(crate) const METHODS: &[&str] = &[
    "/heddle.api.v2alpha1.ContentService/ReadArtifact",
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
    "/heddle.api.v2alpha1.ThreadService/ObserveThread",
    "/heddle.api.v2alpha1.ThreadService/StartThread",
    "/heddle.api.v2alpha1.ThreadService/RenameThread",
    "/heddle.api.v2alpha1.ThreadService/ReviseIntent",
    "/heddle.api.v2alpha1.ThreadService/ChangeLifecycle",
    "/heddle.api.v2alpha1.ThreadService/SetSharingPolicy",
    "/heddle.api.v2alpha1.ThreadService/RecordReview",
    "/heddle.api.v2alpha1.SyncService/ReplicateThread",
    "/heddle.api.v2alpha1.CheckoutService/ObserveCheckouts",
    "/heddle.api.v2alpha1.CheckoutService/Materialize",
    "/heddle.api.v2alpha1.CheckoutService/ClaimCheckoutWriter",
    "/heddle.api.v2alpha1.CheckoutService/ReleaseCheckoutWriter",
    "/heddle.api.v2alpha1.CheckoutService/Capture",
    "/heddle.api.v2alpha1.CheckoutService/Refresh",
    "/heddle.api.v2alpha1.CheckoutService/Resolve",
    "/heddle.api.v2alpha1.CheckoutService/Recover",
    "/heddle.api.v2alpha1.CheckoutService/LandCheckout",
    "/heddle.api.v2alpha1.RunService/ObserveRuns",
    "/heddle.api.v2alpha1.RunService/ControlRun",
    "/heddle.api.v2alpha1.RunService/DecidePermission",
    "/heddle.api.v2alpha1.RunService/PutRunPolicy",
];
#[derive(Clone, Debug)]
pub(crate) struct DeviceRpc {
    home: PathBuf,
    endpoint: [u8; 32],
    feeds: Arc<Mutex<BTreeMap<uuid::Uuid, Weak<observe::Feed>>>>,
    account_feed: Arc<Mutex<Weak<account_feed::AccountFeed>>>,
    content_work: Arc<tokio::sync::Semaphore>,
}
impl DeviceRpc {
    pub fn new(home: PathBuf, endpoint: [u8; 32]) -> Self {
        Self {
            home,
            endpoint,
            feeds: Arc::new(Mutex::new(BTreeMap::new())),
            account_feed: Arc::new(Mutex::new(Weak::new())),
            content_work: Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }
    pub fn endpoint(&self) -> EndpointRef {
        EndpointRef {
            public_key: self.endpoint.to_vec(),
            kind: EndpointKind::Device as i32,
        }
    }
    pub async fn serve(
        &self,
        method: &str,
        context: &CallContext,
        body: &[u8],
        mut send: SendStream,
        budget: &mut super::hosted::claim_protocol::CallBudget,
    ) -> Result<()> {
        if account::METHODS.contains(&method) {
            return self
                .serve_account(method, context, body, send, budget)
                .await;
        }
        let descriptor = api::v2::method_descriptor(method).context("unknown device RPC")?;
        let prepared = (|| {
            let id = request_spool(method, body)?;
            let spool = repo::device_catalog::load(&self.home, id)?;
            auth::authorize(&self.home, descriptor, context, body, spool)
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
        }
        if method.ends_with("/ReadArtifact") {
            return self.read_artifact(session, body, send).await;
        }
        if method.ends_with("/ObserveThread") {
            return self.observe_thread(&session, body, send).await;
        }
        if method.ends_with("/ObserveCheckouts") || method.ends_with("/ObserveRuns") {
            return self.observe(&session, method, body, send).await;
        }
        let this = self.clone();
        let body = body.to_vec();
        let (session, response) = tokio::task::spawn_blocking(move || {
            let response = session
                .check_current(&this.home)
                .and_then(|_| this.execute(&session, descriptor.path, &body));
            (session, response)
        })
        .await?;
        let response = response.and_then(|body| {
            session.check_current(&self.home)?;
            Ok(body)
        });
        let bytes = match response {
            Ok(body) => api::framing::encode_success_response(&body)?,
            Err(error) => api::framing::encode_failure_response(&failure(
                CallFailureCode::FailedPrecondition,
                error,
            ))?,
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), send.write_all(&bytes)).await??;
        send.finish()?;
        Ok(())
    }
    fn execute(&self, session: &auth::Session, method: &str, body: &[u8]) -> Result<Vec<u8>> {
        if method.contains(".ThreadService/") {
            return self.thread_command(session, method, body);
        }
        if method.contains(".CheckoutService/") {
            return self.checkout_command(session, method, body);
        }
        let store = repo::device_runs::RunStore::open(&session.spool.heddle_dir)?;
        let operation = match method.rsplit('/').next().context("method missing")? {
            "ControlRun" => {
                let request = ControlRunRequest::decode(body)?;
                store.enqueue_control(&request, &session.actor)?;
                request.client_operation_id
            }
            "DecidePermission" => {
                let request = DecideRunPermissionRequest::decode(body)?;
                store.decide_permission(&request, &session.actor)?;
                request.client_operation_id
            }
            "PutRunPolicy" => {
                let request = PutRunPolicyRequest::decode(body)?;
                store.put_policy(&request, &session.actor)?;
                request.client_operation_id
            }
            _ => bail!("unknown run method"),
        };
        Ok(MutationResponse {
            receipt: Some(self.receipt(&operation)),
        }
        .encode_to_vec())
    }
    fn receipt(&self, operation: &str) -> MutationReceipt {
        let now = chrono::Utc::now();
        MutationReceipt {
            client_operation_id: operation.into(),
            endpoint: Some(self.endpoint()),
            outcome: Some(mutation_receipt::Outcome::Applied(Applied::default())),
            observed_at: Some(prost_types::Timestamp {
                seconds: now.timestamp(),
                nanos: now.timestamp_subsec_nanos() as i32,
            }),
        }
    }
}
fn request_spool(method: &str, body: &[u8]) -> Result<uuid::Uuid> {
    macro_rules! scope {
        ($ty:ty,$field:expr) => {{
            let request = <$ty>::decode(body)?;
            $field(request)
        }};
    }
    let spool = match method.rsplit('/').next().context("method missing")? {
        "ReadArtifact" => scope!(ReadArtifactRequest, |r: ReadArtifactRequest| r
            .artifact
            .and_then(|r| r.spool)),
        "ObserveThread" => scope!(ObserveThreadRequest, |r: ObserveThreadRequest| r
            .thread
            .and_then(|t| t.spool)),
        "StartThread" => scope!(StartThreadRequest, |r: StartThreadRequest| r.spool),
        "RenameThread" => scope!(RenameThreadRequest, |r: RenameThreadRequest| r
            .thread
            .and_then(|t| t.spool)),
        "ReviseIntent" => scope!(ReviseIntentRequest, |r: ReviseIntentRequest| r
            .thread
            .and_then(|t| t.spool)),
        "ChangeLifecycle" => scope!(
            ChangeThreadLifecycleRequest,
            |r: ChangeThreadLifecycleRequest| r.thread.and_then(|t| t.spool)
        ),
        "SetSharingPolicy" => scope!(SetThreadSharingRequest, |r: SetThreadSharingRequest| r
            .policy
            .and_then(|p| p.thread)
            .and_then(|t| t.spool)),
        "RecordReview" => scope!(RecordReviewRequest, |r: RecordReviewRequest| r
            .decision
            .and_then(|p| p.thread)
            .and_then(|t| t.spool)),
        "ObserveCheckouts" => scope!(ObserveCheckoutsRequest, |r: ObserveCheckoutsRequest| r
            .spool),
        "Materialize" => scope!(
            MaterializeCheckoutRequest,
            |r: MaterializeCheckoutRequest| r.thread.and_then(|r| r.spool)
        ),
        "ClaimCheckoutWriter" => scope!(
            ClaimCheckoutWriterRequest,
            |r: ClaimCheckoutWriterRequest| r.checkout.and_then(|r| r.spool)
        ),
        "ReleaseCheckoutWriter" => scope!(
            ReleaseCheckoutWriterRequest,
            |r: ReleaseCheckoutWriterRequest| r.checkout.and_then(|r| r.spool)
        ),
        "Capture" => scope!(CaptureCheckoutRequest, |r: CaptureCheckoutRequest| r
            .checkout
            .and_then(|r| r.spool)),
        "Refresh" => scope!(RefreshCheckoutRequest, |r: RefreshCheckoutRequest| r
            .checkout
            .and_then(|r| r.spool)),
        "Resolve" => scope!(ResolveCheckoutRequest, |r: ResolveCheckoutRequest| r
            .checkout
            .and_then(|r| r.spool)),
        "Recover" => scope!(RecoverCheckoutRequest, |r: RecoverCheckoutRequest| r
            .checkout
            .and_then(|r| r.spool)),
        "LandCheckout" => scope!(LandCheckoutRequest, |r: LandCheckoutRequest| r
            .checkout
            .and_then(|r| r.spool)),
        "ObserveRuns" => scope!(ObserveRunsRequest, |r: ObserveRunsRequest| r.spool),
        "ControlRun" => scope!(ControlRunRequest, |r: ControlRunRequest| r
            .run
            .and_then(|r| r.spool)),
        "DecidePermission" => scope!(
            DecideRunPermissionRequest,
            |r: DecideRunPermissionRequest| r.run.and_then(|r| r.spool)
        ),
        "PutRunPolicy" => scope!(PutRunPolicyRequest, |r: PutRunPolicyRequest| r
            .policy
            .and_then(|r| r.spool)),
        _ => bail!("unknown device RPC"),
    }
    .context("spool scope required")?;
    Ok(uuid::Uuid::parse_str(&spool.id)?)
}
fn failure(code: CallFailureCode, error: impl std::fmt::Display) -> CallFailure {
    CallFailure {
        code: code as i32,
        message: error.to_string(),
        error: None,
    }
}
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("device record exceeds bound");
    }
    Ok(bytes)
}
