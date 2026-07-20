//! Native hosted calls over Iroh.
//!
//! The module is the single seam for bootstrap verification, connection reuse,
//! ALPN negotiation, framing, cancellation, and transport-neutral failures.

mod bootstrap;
mod call;
mod collaboration;
mod connection;
mod content;
mod context;
mod error;
mod helpers;
mod methods;
pub mod monorepo;
mod operation_id;
mod state_review;
mod sync;
mod user;

use std::sync::Arc;

use api::heddle::api::v1alpha1::CallContext;
pub use bootstrap::{DescriptorKeyring, VerifiedEndpointDescriptor, fetch_endpoint_descriptor};
pub use call::{BidirectionalRequestStream, BidirectionalStream, ServerStream};
use cli_shared::ClientConfig;
pub use collaboration::{HostedDiscussion, HostedDiscussionTurn};
use connection::HostedConnection;
pub use context::{CallContextFactory, SignedCallContext};
pub use error::HostedError;
use iroh::{Endpoint, EndpointAddr};
pub use methods::HostedRoutes;
use prost::Message;
pub use sync::{HostedRefEntry, PullObjectMix, PullProfile};

pub type Result<T> = std::result::Result<T, HostedError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullMaterialization {
    Full,
    Lazy,
}

impl PullMaterialization {
    pub(crate) fn allows_partial_fetch(self) -> bool {
        matches!(self, Self::Lazy)
    }
}

/// One reusable native connection to a terminating Weft application endpoint.
#[derive(Debug, Clone)]
pub struct HostedClient {
    connection: Arc<HostedConnection>,
    context: CallContextFactory,
    transport: helpers::HostedTransportPolicy,
}

impl HostedClient {
    pub fn routes(&self) -> HostedRoutes<'_> {
        HostedRoutes::new(self)
    }

    pub async fn connect(descriptor: &VerifiedEndpointDescriptor) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect_verified(descriptor).await?,
            context: CallContextFactory::default(),
            transport: helpers::HostedTransportPolicy::from_client_config(&ClientConfig::default()),
        })
    }

    pub async fn connect_with_config(
        descriptor: &VerifiedEndpointDescriptor,
        config: &ClientConfig,
    ) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect_verified(descriptor).await?,
            context: CallContextFactory::from_client_config(config)?,
            transport: helpers::HostedTransportPolicy::from_client_config(config),
        })
    }

    /// Direct-address constructor for conformance tests and explicit local endpoints.
    pub async fn connect_addr(endpoint: Endpoint, address: EndpointAddr) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect(endpoint, address).await?,
            context: CallContextFactory::default(),
            transport: helpers::HostedTransportPolicy::from_client_config(&ClientConfig::default()),
        })
    }

    pub async fn connect_addr_with_config(
        endpoint: Endpoint,
        address: EndpointAddr,
        config: &ClientConfig,
    ) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect(endpoint, address).await?,
            context: CallContextFactory::from_client_config(config)?,
            transport: helpers::HostedTransportPolicy::from_client_config(config),
        })
    }

    pub async fn connect_addr_with_context(
        endpoint: Endpoint,
        address: EndpointAddr,
        context: CallContextFactory,
    ) -> Result<Self> {
        Ok(Self {
            connection: HostedConnection::connect(endpoint, address).await?,
            context,
            transport: helpers::HostedTransportPolicy::from_client_config(&ClientConfig::default()),
        })
    }

    pub async fn call_unary<Request, Response>(
        &self,
        method: &str,
        request: &Request,
    ) -> Result<Response>
    where
        Request: Message,
        Response: Message + Default,
    {
        let encoded = request.encode_to_vec();
        let descriptor = api::method_descriptor(method)
            .ok_or_else(|| HostedError::Framing(format!("unknown hosted method {method}")))?;
        let client_operation_id = descriptor
            .client_operation_id(&encoded)?
            .unwrap_or_default();
        if descriptor.client_operation_id_required && client_operation_id.is_empty() {
            return Err(HostedError::MissingClientOperationId);
        }
        let signed = self.context.unary(method, &encoded, client_operation_id)?;
        call::unary_encoded(&self.connection, method, &signed.context, &encoded).await
    }

    pub async fn call_server_stream<Request, Response>(
        &self,
        method: &str,
        request: &Request,
        client_operation_id: impl Into<String>,
    ) -> Result<ServerStream<Response>>
    where
        Request: Message,
        Response: Message + Default,
    {
        let context = self.context.streaming(method, client_operation_id)?;
        call::server_stream(self.connection.clone(), method, &context, request).await
    }

    pub async fn call_bidirectional<Request, Response>(
        &self,
        method: &str,
        client_operation_id: impl Into<String>,
    ) -> Result<BidirectionalStream<Request, Response>>
    where
        Request: Message,
        Response: Message + Default,
    {
        let context = self.context.streaming(method, client_operation_id)?;
        call::bidirectional(self.connection.clone(), method, &context).await
    }

    pub async fn unary<Request, Response>(
        &self,
        method: &str,
        context: &CallContext,
        request: &Request,
    ) -> Result<Response>
    where
        Request: Message,
        Response: Message + Default,
    {
        call::unary(&self.connection, method, context, request).await
    }

    pub async fn server_stream<Request, Response>(
        &self,
        method: &str,
        context: &CallContext,
        request: &Request,
    ) -> Result<ServerStream<Response>>
    where
        Request: Message,
        Response: Message + Default,
    {
        call::server_stream(self.connection.clone(), method, context, request).await
    }

    pub async fn bidirectional<Request, Response>(
        &self,
        method: &str,
        context: &CallContext,
    ) -> Result<BidirectionalStream<Request, Response>>
    where
        Request: Message,
        Response: Message + Default,
    {
        call::bidirectional(self.connection.clone(), method, context).await
    }
}
