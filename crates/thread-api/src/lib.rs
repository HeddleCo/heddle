// SPDX-License-Identifier: Apache-2.0
//! Native v2 client design. Endpoint ownership, credentials and connection
//! discovery belong to the application; this crate never obtains a Weft token.
#[cfg(feature = "native")]
pub mod authority;
pub mod content;
#[cfg(feature = "replication")]
pub mod live_replication;
pub mod observation;
#[cfg(any(feature = "native", feature = "replication", feature = "iroh"))]
pub mod publication;
#[cfg(feature = "replication")]
pub mod replication;
#[cfg(all(feature = "native", feature = "iroh"))]
pub mod replication_rpc;
#[cfg(feature = "signing")]
pub mod request_proof;
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
    /// A stable operation ID and the observed intent version make retry and
    /// concurrent edits explicit. A blocked receipt remains a typed outcome.
    pub async fn revise_intent(
        &self,
        operation_id: impl Into<String>,
        observed: &contract::ThreadIntent,
        proposed: contract::ThreadIntent,
    ) -> Result<contract::ThreadMutationResponse, ClientError<Error>> {
        if observed.version.is_empty() {
            return Err(ClientError::Transport(Error::Protocol(
                "observe the intent version before editing",
            )));
        }
        self.remote
            .api
            .call::<rpc::ThreadServiceReviseIntent>(&contract::ReviseIntentRequest {
                client_operation_id: operation_id.into(),
                thread: Some(self.reference.clone()),
                expected_intent_version: observed.version.clone(),
                proposed_intent: Some(proposed),
            })
            .await
    }

    pub async fn observe(
        &self,
        sections: &[contract::ThreadSection],
        mode: contract::ObservationMode,
        resume: Option<observation::Resume>,
    ) -> Result<observation::ThreadObservation<T::Reader>, observation::Error> {
        let budget = observation::budget(&self.remote.description)?;
        let mut request = contract::ObserveThreadRequest {
            thread: Some(self.reference.clone()),
            sections: sections.iter().map(|s| *s as i32).collect(),
            observe: Some(contract::ObserveOptions {
                mode: mode as i32,
                after_cursor: resume
                    .as_ref()
                    .map(|r| r.cursor.clone())
                    .unwrap_or_default(),
                budget: Some(budget),
            }),
            ..Default::default()
        };
        let cursor = request
            .observe
            .as_mut()
            .map(|o| std::mem::take(&mut o.after_cursor));
        let query = prost::Message::encode_to_vec(&request);
        observation::validate_resume(&resume, &self.remote.description, &query)?;
        if let Some(options) = request.observe.as_mut() {
            options.after_cursor = cursor.unwrap_or_default();
        }
        let messages = self
            .remote
            .api
            .observe::<rpc::ThreadServiceObserveThread>(&request)
            .await?;
        observation::ThreadObservation::new(
            messages,
            &self.remote.description,
            budget,
            resume,
            query,
        )
    }
}
