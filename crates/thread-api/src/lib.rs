// SPDX-License-Identifier: Apache-2.0
//! Native v2 client design. Endpoint ownership, credentials and connection
//! discovery belong to the application; this crate never obtains a Weft token.
#[cfg(feature = "native")]
pub mod authority;
#[cfg(feature = "replication")]
pub mod authority_admission;
#[cfg(feature = "replication")]
pub mod boundary_acceptance;
#[cfg(feature = "replication")]
pub mod thread_ownership;
#[cfg(feature = "semantic-analysis")]
pub mod behavior;
#[cfg(feature = "replication")]
pub mod collaboration;
pub mod content;
#[cfg(feature = "replication")]
pub mod creation;
#[cfg(feature = "signing")]
pub mod credentials;
#[cfg(feature = "replication")]
pub mod evidence;
#[cfg(feature = "source-transfer")]
pub mod fetch;
#[cfg(feature = "replication")]
pub mod live_replication;
pub mod observation;
#[cfg(feature = "root-attachment")]
pub mod pairing;
#[cfg(any(feature = "native", feature = "replication", feature = "iroh"))]
pub mod publication;
#[cfg(feature = "replication")]
pub mod replication;
#[cfg(all(feature = "native", feature = "iroh"))]
pub mod replication_rpc;
#[cfg(feature = "signing")]
pub mod request_proof;
#[cfg(feature = "root-attachment")]
pub mod root_attachment;
#[cfg(feature = "replication")]
pub mod thread_control;
pub mod transport;

use api::v2::client::{Client, ClientError, RpcTransport};
pub use api::{
    heddle::api::v2alpha1 as contract,
    v2::{client::Rpc, rpc},
};
use contract::{DescribeEndpointRequest, DescribeEndpointResponse, EndpointKind, ThreadRef};
use transport::Error;

/// One authenticated source. A combined Thread retains one source per endpoint;
/// a hosted action is sent directly to the Weft source, never via a device.
pub struct Remote<T: RpcTransport<Error = Error>> {
    pub api: Client<T>,
    pub description: DescribeEndpointResponse,
}

impl<T: RpcTransport<Error = Error>> Remote<T> {
    /// The caller supplies the Iroh-authenticated endpoint key and intended kind.
    /// Describe is the only bootstrap exception to implemented-method discovery.
    pub async fn discover(
        transport: T,
        endpoint_key: [u8; 32],
        kind: EndpointKind,
    ) -> Result<Self, ClientError<Error>> {
        let bytes = transport
            .unary(
                rpc::EndpointServiceDescribeEndpoint::METHOD,
                prost::Message::encode_to_vec(&DescribeEndpointRequest {
                    understood_packages: vec!["heddle.api.v2alpha1".into()],
                }),
            )
            .await
            .map_err(ClientError::Transport)?;
        let description: DescribeEndpointResponse = prost::Message::decode(bytes.as_slice())?;
        if description
            .endpoint
            .as_ref()
            .is_none_or(|source| source.public_key != endpoint_key || source.kind != kind as i32)
            || !description
                .supported_packages
                .iter()
                .any(|p| p == "heddle.api.v2alpha1")
        {
            return Err(ClientError::Transport(Error::Protocol(
                "endpoint identity/package mismatch",
            )));
        }
        let api = Client::new(transport, description.implemented_methods.clone());
        Ok(Self { api, description })
    }

    /// Observe any contract view as atomic bounded batches. The bookmark binds
    /// the exact method and projection as well as the authenticated endpoint.
    pub async fn observe<M>(
        &self,
        mut request: M::Request,
        resume: Option<observation::Resume>,
    ) -> Result<observation::Observation<T::Reader, M::Response>, observation::Error>
    where
        M: api::v2::client::ServerStreamingRpc,
        M::Request: observation::ObservationRequest,
        M::Response: observation::ObservedEvent,
    {
        use observation::ObservationRequest as _;
        let budget = observation::budget(&self.description)?;
        let options = request.options_mut();
        options.budget = Some(budget);
        options.after_cursor.clear();
        let mut query = b"heddle-observation-query-v2\0".to_vec();
        query.extend_from_slice(M::METHOD.path.as_bytes());
        query.push(0);
        query.extend_from_slice(&prost::Message::encode_to_vec(&request));
        observation::validate_resume(&resume, &self.description, &query)?;
        if let Some(resume) = &resume {
            request.options_mut().after_cursor = resume.cursor.clone();
        }
        let messages = self.api.observe::<M>(&request).await?;
        observation::Observation::new(messages, &self.description, budget, resume, query)
    }

    /// Source-backed analysis uses the same committed view protocol as identity,
    /// collaboration, checkouts and Thread observations.
    pub async fn observe_analysis(
        &self,
        request: contract::ObserveAnalysisRequest,
        resume: Option<observation::Resume>,
    ) -> Result<observation::AnalysisObservation<T::Reader>, observation::Error> {
        self.observe::<rpc::AnalysisServiceObserveAnalysis>(request, resume)
            .await
    }

    /// Binding a Thread is local and costs no RPC. Persist this stable reference
    /// when discovering/creating the Thread; its display name is never its key.
    pub fn thread(&self, thread: ThreadRef) -> Thread<'_, T> {
        Thread {
            remote: self,
            reference: thread,
        }
    }
}

pub struct Thread<'a, T: RpcTransport<Error = Error>> {
    remote: &'a Remote<T>,
    pub reference: ThreadRef,
}

impl<T: RpcTransport<Error = Error>> Thread<'_, T> {
    /// Send a locally prepared original signed intent. Preparation consumes the
    /// existing overview's field frontier and performs no extra RPC.
    #[cfg(feature = "replication")]
    pub async fn revise_intent(
        &self,
        command: &thread_control::PreparedControl,
    ) -> Result<contract::ThreadMutationResponse, ClientError<Error>> {
        let request = command.revise_intent().map_err(ClientError::Transport)?;
        if request.thread.as_ref() != Some(&self.reference) {
            return Err(ClientError::Transport(Error::Protocol(
                "prepared command belongs to another Thread",
            )));
        }
        self.remote
            .api
            .call::<rpc::ThreadServiceReviseIntent>(&request)
            .await
    }

    pub async fn observe(
        &self,
        sections: &[contract::ThreadSection],
        mode: contract::ObservationMode,
        resume: Option<observation::Resume>,
    ) -> Result<observation::ThreadObservation<T::Reader>, observation::Error> {
        self.remote
            .observe::<rpc::ThreadServiceObserveThread>(
                contract::ObserveThreadRequest {
                    thread: Some(self.reference.clone()),
                    sections: sections.iter().map(|section| *section as i32).collect(),
                    observe: Some(contract::ObserveOptions {
                        mode: mode as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                resume,
            )
            .await
    }
}
