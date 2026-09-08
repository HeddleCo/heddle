// SPDX-License-Identifier: Apache-2.0
//! Deliver bounded, committed changes; never expose half a snapshot as a view.
use api::v2::{
    ObservationAction, ObservationState, StreamProtocolError,
    client::{ClientError, MessageReader, Messages},
};

use crate::{contract::*, transport};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Client(#[from] ClientError<transport::Error>),
    #[error(transparent)]
    Stream(#[from] StreamProtocolError),
    #[error("invalid observation: {0}")]
    Invalid(&'static str),
    #[error("observation interrupted; resume only from the last committed batch")]
    Interrupted,
    #[error("observation reset ({0}); start a replacement snapshot")]
    Reset(i32),
}

/// Store this atomically with the view changed by its committed batch. A resume
/// token belongs to one authenticated endpoint and one exact request projection.
#[derive(Clone, Debug)]
pub struct Resume {
    pub(crate) cursor: Vec<u8>,
    binding: [u8; 32],
    source: EndpointRef,
    query: Vec<u8>,
}

// Local bookmark format, not a server cursor or a signed authority record.
#[derive(prost::Message)]
struct StoredResume {
    #[prost(uint32, tag = "1")]
    version: u32,
    #[prost(message, optional, tag = "2")]
    source: Option<EndpointRef>,
    #[prost(bytes = "vec", tag = "3")]
    binding: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    query: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    cursor: Vec<u8>,
}

impl Resume {
    pub fn encode(&self) -> Vec<u8> {
        prost::Message::encode_to_vec(&StoredResume {
            version: 1,
            source: Some(self.source.clone()),
            binding: self.binding.to_vec(),
            query: self.query.clone(),
            cursor: self.cursor.clone(),
        })
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 512 * 1024 {
            return Err(Error::Invalid("oversized observation bookmark"));
        }
        let stored: StoredResume = prost::Message::decode(bytes)
            .map_err(|_| Error::Invalid("malformed observation bookmark"))?;
        let source = stored
            .source
            .ok_or(Error::Invalid("missing bookmark source"))?;
        if stored.version != 1
            || source.public_key.len() != 32
            || !matches!(
                EndpointKind::try_from(source.kind),
                Ok(EndpointKind::Weft | EndpointKind::Device)
            )
            || stored.cursor.is_empty()
            || stored.cursor.len() > api::v2::MAX_CURSOR_BYTES
            || stored.query.is_empty()
        {
            return Err(Error::Invalid("invalid observation bookmark"));
        }
        let binding = stored
            .binding
            .try_into()
            .map_err(|_| Error::Invalid("invalid bookmark binding"))?;
        Ok(Self {
            cursor: stored.cursor,
            binding,
            source,
            query: stored.query,
        })
    }
}

pub type CommittedThreadBatch = CommittedBatch<thread_event::Payload>;
pub type CommittedAnalysisBatch = CommittedBatch<analysis_event::Payload>;

pub struct CommittedBatch<P> {
    pub replace: bool,
    pub changes: Vec<P>,
    pub page: Option<PageInfo>,
    pub resume: Resume,
}

pub(crate) fn validate_resume(
    resume: &Option<Resume>,
    description: &DescribeEndpointResponse,
    query: &[u8],
) -> Result<(), Error> {
    if resume
        .as_ref()
        .is_some_and(|r| Some(&r.source) != description.endpoint.as_ref() || r.query != query)
    {
        return Err(Error::Invalid(
            "resume belongs to a different source or projection",
        ));
    }
    Ok(())
}

pub fn budget(description: &DescribeEndpointResponse) -> Result<ReadBudget, Error> {
    let budget = description
        .default_read_budget
        .ok_or(Error::Invalid("missing default read budget"))?;
    if budget.max_items == 0
        || budget.max_frame_bytes == 0
        || budget.max_snapshot_bytes == 0
        || description.max_pending_batch_bytes == 0
    {
        return Err(Error::Invalid("unbounded endpoint budget"));
    }
    // Local ceilings apply even when a peer advertises excessive defaults.
    Ok(ReadBudget {
        max_items: budget.max_items.min(1000),
        max_frame_bytes: budget.max_frame_bytes.min(256 * 1024),
        max_snapshot_bytes: budget.max_snapshot_bytes.min(4 * 1024 * 1024),
    })
}

pub type ThreadObservation<R> = Observation<R, ThreadEvent>;
pub type AnalysisObservation<R> = Observation<R, AnalysisEvent>;

/// Typed payload access; the checkpoint/budget state machine is shared.
pub trait ObservedEvent: prost::Message + Default {
    type Payload;
    fn frame(&self) -> Option<&StreamFrame>;
    fn has_payload(&self) -> bool;
    fn take_payload(&mut self) -> Option<Self::Payload>;
    fn is_removal(&self) -> bool;
}

impl ObservedEvent for ThreadEvent {
    type Payload = thread_event::Payload;
    fn frame(&self) -> Option<&StreamFrame> {
        self.frame.as_ref()
    }
    fn has_payload(&self) -> bool {
        self.payload.is_some()
    }
    fn take_payload(&mut self) -> Option<Self::Payload> {
        self.payload.take()
    }
    fn is_removal(&self) -> bool {
        matches!(self.payload, Some(thread_event::Payload::Removal(_)))
    }
}
impl ObservedEvent for AnalysisEvent {
    type Payload = analysis_event::Payload;
    fn frame(&self) -> Option<&StreamFrame> {
        self.frame.as_ref()
    }
    fn has_payload(&self) -> bool {
        self.payload.is_some()
    }
    fn take_payload(&mut self) -> Option<Self::Payload> {
        self.payload.take()
    }
    fn is_removal(&self) -> bool {
        matches!(
            self.payload,
            Some(analysis_event::Payload::Removal(_) | analysis_event::Payload::BehaviorRemoval(_))
        )
    }
}

/// Request shapes with the common observation controls. Typed RPC selection still
/// comes from the contract; this trait never guesses a method from a payload.
pub trait ObservationRequest: prost::Message {
    fn options_mut(&mut self) -> &mut ObserveOptions;
}
macro_rules! observation_requests {
    ($($request:ty),+ $(,)?) => { $(
        impl ObservationRequest for $request {
            fn options_mut(&mut self) -> &mut ObserveOptions {
                self.observe.get_or_insert_default()
            }
        }
    )+ };
}
observation_requests!(
    ObserveThreadRequest,
    ObserveThreadsRequest,
    ObserveAnalysisRequest,
    ObserveIdentityRequest,
    ObservePairingRequest,
    ObserveOwnershipRequest,
    ObserveWorkspaceRequest,
    ObserveSpoolRequest,
    ObserveCollaborationRequest,
    ObserveCheckoutsRequest,
    ObserveRunsRequest,
    ObserveAttentionRequest,
    ObserveNotificationsRequest,
    ObserveOperationsRequest,
    ObserveIntegrationsRequest,
);

macro_rules! observed_events {
    ($($event:ty => $module:ident [$($removal:ident),*]),+ $(,)?) => { $(
        impl ObservedEvent for $event {
            type Payload = $module::Payload;
            fn frame(&self) -> Option<&StreamFrame> { self.frame.as_ref() }
            fn has_payload(&self) -> bool { self.payload.is_some() }
            fn take_payload(&mut self) -> Option<Self::Payload> { self.payload.take() }
            fn is_removal(&self) -> bool {
                match &self.payload {
                    $(Some($module::Payload::$removal(_)) => true,)*
                    _ => false,
                }
            }
        }
    )+ };
}
observed_events!(
    IdentityEvent => identity_event [Removal],
    PairingEvent => pairing_event [],
    OwnershipEvent => ownership_event [],
    WorkspaceEvent => workspace_event [Removal],
    SpoolEvent => spool_event [Removal],
    ThreadListEvent => thread_list_event [Removal],
    CollaborationEvent => collaboration_event [Removal],
    CheckoutEvent => checkout_event [Removal],
    RunEvent => run_event [Removal],
    AttentionEvent => attention_event [Removal],
    NotificationEvent => notification_event [Removal],
    OperationEvent => operation_event [Removal],
    IntegrationEvent => integration_event [Removal],
);

pub struct Observation<R: MessageReader<Error = transport::Error>, E: ObservedEvent> {
    messages: Messages<R, E>,
    state: Option<ObservationState>,
    binding: Option<[u8; 32]>,
    source: EndpointRef,
    requested: ReadBudget,
    accepted: ReadBudget,
    max_batch_bytes: u64,
    resume: Option<Resume>,
    query: Vec<u8>,
    pending: Vec<E::Payload>,
    pending_bytes: u64,
    snapshot: bool,
    done: bool,
}

impl<R: MessageReader<Error = transport::Error>, E: ObservedEvent> Observation<R, E> {
    pub(crate) fn new(
        messages: Messages<R, E>,
        description: &DescribeEndpointResponse,
        requested: ReadBudget,
        resume: Option<Resume>,
        query: Vec<u8>,
    ) -> Result<Self, Error> {
        let source = description
            .endpoint
            .clone()
            .ok_or(Error::Invalid("missing endpoint"))?;
        if resume
            .as_ref()
            .is_some_and(|r| r.source != source || r.query != query)
        {
            return Err(Error::Invalid(
                "resume belongs to a different source or projection",
            ));
        }
        Ok(Self {
            messages,
            state: None,
            binding: None,
            source,
            accepted: requested,
            requested,
            max_batch_bytes: u64::from(description.max_pending_batch_bytes).min(4 * 1024 * 1024),
            resume,
            query,
            pending: vec![],
            pending_bytes: 0,
            snapshot: true,
            done: false,
        })
    }

    pub fn cancel(&mut self) {
        self.messages.cancel();
        self.pending.clear();
        self.done = true;
    }

    pub async fn next_commit(&mut self) -> Result<Option<CommittedBatch<E::Payload>>, Error> {
        let result = self.next_inner().await;
        if result.is_err() {
            self.cancel();
        }
        result
    }

    async fn next_inner(&mut self) -> Result<Option<CommittedBatch<E::Payload>>, Error> {
        if self.done {
            return Ok(None);
        }
        loop {
            let mut event = self.messages.next().await?.ok_or(Error::Interrupted)?;
            let size = prost::Message::encoded_len(&event) as u64;
            if size > u64::from(self.accepted.max_frame_bytes) {
                return Err(Error::Invalid("frame budget exceeded"));
            }
            let frame = event.frame().ok_or(Error::Invalid("missing frame"))?;
            if self.state.is_none() {
                if let Some(stream_frame::Body::Reset(reset)) = &frame.body {
                    if frame.sequence != 1 || event.has_payload() {
                        return Err(Error::Invalid("malformed initial reset"));
                    }
                    return Err(Error::Reset(reset.reason));
                }
                let Some(stream_frame::Body::Open(open)) = &frame.body else {
                    return Err(Error::Invalid("missing Open"));
                };
                if open.source.as_ref() != Some(&self.source) {
                    return Err(Error::Invalid("stream source mismatch"));
                }
                let binding: [u8; 32] = open
                    .binding_digest
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invalid("invalid binding"))?;
                let accepted = open
                    .accepted_budget
                    .ok_or(Error::Invalid("missing accepted budget"))?;
                if accepted.max_items == 0
                    || accepted.max_items > self.requested.max_items
                    || accepted.max_frame_bytes == 0
                    || accepted.max_frame_bytes > self.requested.max_frame_bytes
                    || accepted.max_snapshot_bytes == 0
                    || accepted.max_snapshot_bytes > self.requested.max_snapshot_bytes
                {
                    return Err(Error::Invalid("server widened or omitted the read budget"));
                }
                self.accepted = accepted;
                self.binding = Some(binding);
                self.state = Some(ObservationState::new(
                    self.resume.as_ref().map(|r| r.binding).unwrap_or(binding),
                    self.resume
                        .as_ref()
                        .map(|r| r.cursor.clone())
                        .unwrap_or_default(),
                ));
            }
            let state = self
                .state
                .as_mut()
                .ok_or(Error::Invalid("missing observation state"))?;
            match state.accept(frame, event.has_payload())? {
                ObservationAction::BeginSnapshot => self.snapshot = true,
                ObservationAction::Resumed => self.snapshot = false,
                ObservationAction::Stage(kind) => {
                    if event.is_removal() != (kind == StreamDataKind::Remove) {
                        return Err(Error::Invalid("removal payload/kind mismatch"));
                    }
                    let ceiling = if self.snapshot {
                        self.accepted.max_snapshot_bytes
                    } else {
                        self.max_batch_bytes
                    };
                    if self.pending.len() >= self.accepted.max_items as usize
                        || size > ceiling.saturating_sub(self.pending_bytes)
                    {
                        return Err(Error::Invalid("uncommitted batch budget exceeded"));
                    }
                    self.pending_bytes += size;
                    self.pending.push(
                        event
                            .take_payload()
                            .ok_or(Error::Invalid("missing data payload"))?,
                    );
                }
                ObservationAction::Commit => {
                    let binding = self
                        .binding
                        .ok_or(Error::Invalid("missing opening binding"))?;
                    let resume = Resume {
                        cursor: state.cursor().to_vec(),
                        binding,
                        source: self.source.clone(),
                        query: self.query.clone(),
                    };
                    let batch = CommittedBatch {
                        page: match &frame.body {
                            Some(stream_frame::Body::Checkpoint(c)) => c.page.clone(),
                            _ => None,
                        },
                        replace: self.snapshot,
                        changes: std::mem::take(&mut self.pending),
                        resume: resume.clone(),
                    };
                    self.resume = Some(resume);
                    self.pending_bytes = 0;
                    self.snapshot = false;
                    return Ok(Some(batch));
                }
                ObservationAction::Complete => {
                    self.done = true;
                    self.messages.cancel();
                    return Ok(None);
                }
                ObservationAction::Reset => {
                    let Some(stream_frame::Body::Reset(reset)) = &frame.body else {
                        return Err(Error::Invalid("missing Reset"));
                    };
                    return Err(Error::Reset(reset.reason));
                }
                ObservationAction::Heartbeat => {}
            }
        }
    }
}
