// SPDX-License-Identifier: Apache-2.0
//! Public native client for hosted repository events.
//!
//! Weft persists repository events and serves every event after
//! [`SubscribeRepoEventsRequest::after_event_id`] before switching to live
//! delivery. This client deliberately leaves reconnect timing to its caller:
//! [`RepoEventSubscription::next`] never turns a dead stream into an indefinite
//! wait. It returns [`RepoEventError::Disconnected`] or
//! [`RepoEventError::Ended`], and [`RepoEventSubscription::resume_request`]
//! carries the last received event ID into a new subscription.

use api::heddle::api::v1alpha1::CallFailureCode;
pub use api::heddle::api::v1alpha1::{RepoEvent, SubscribeRepoEventsRequest};
use config::UserConfig;
use repo::remote::RemoteTarget;

pub use crate::hosted_runtime::hosted::HostedError;
use crate::hosted_runtime::{HostedAuthMode, HostedClient, HostedSession, ServerStream};

/// Failure to connect or receive hosted repository events.
#[derive(Debug, thiserror::Error)]
pub enum RepoEventError {
    /// The supplied server is not a supported native hosted remote.
    #[error("invalid hosted server URL: {0}")]
    InvalidServer(String),
    /// Event subscriptions are only available from hosted network servers.
    #[error("repo-event subscriptions require a hosted network server")]
    LocalServer,
    /// Standard Heddle credential or transport configuration could not load.
    #[error("failed to configure the hosted repo-event client: {0}")]
    Configuration(String),
    /// Descriptor discovery or the native Iroh connection failed.
    #[error("failed to connect the hosted repo-event client: {0}")]
    Connection(String),
    /// The server refused the subscription, including authorization failures.
    #[error("repo-event subscription was refused: {source}")]
    Refused {
        /// Typed hosted failure returned by the server.
        #[source]
        source: HostedError,
    },
    /// The underlying connection failed after the reported durable cursor.
    #[error(
        "repo-event stream disconnected after event {last_event_id}: {source}; reconnect and resume after event {last_event_id}"
    )]
    Disconnected {
        /// Last event ID successfully returned to the caller.
        last_event_id: i64,
        /// Typed transport, framing, or remote failure.
        #[source]
        source: HostedError,
    },
    /// The server ended a stream that is defined to be long-lived.
    #[error(
        "repo-event stream ended after event {last_event_id}; reconnect and resume after event {last_event_id}"
    )]
    Ended {
        /// Last event ID successfully returned to the caller.
        last_event_id: i64,
    },
}

impl RepoEventError {
    /// Cursor that may be placed in `after_event_id` after a disconnect.
    pub fn resume_after_event_id(&self) -> Option<i64> {
        match self {
            Self::Disconnected { last_event_id, .. } | Self::Ended { last_event_id } => {
                Some(*last_event_id)
            }
            _ => None,
        }
    }
}

/// Authenticated native client for hosted repository-event subscriptions.
pub struct RepoEventClient {
    client: HostedClient,
}

impl RepoEventClient {
    /// Connect using Heddle's standard credential resolution and descriptor trust.
    ///
    /// `server` accepts the same native network forms as a Heddle remote, such
    /// as `heddle://host:8421/owner/repo` or
    /// `https://host/owner/repo`. Credentials are resolved from the active
    /// environment credential or Heddle's credential store.
    pub async fn connect(server: &str) -> Result<Self, RepoEventError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let target = RemoteTarget::parse_native(server).map_err(RepoEventError::InvalidServer)?;
        let RemoteTarget::Network { authority, .. } = target else {
            return Err(RepoEventError::LocalServer);
        };
        let server_key = repo::remote::credential_key_from_remote_url(server);
        let user_config = UserConfig::load_default()
            .map_err(|error| RepoEventError::Configuration(error.to_string()))?;
        let session =
            HostedSession::build(&user_config, server_key, HostedAuthMode::CredentialFallback)
                .map_err(|error| RepoEventError::Configuration(error.to_string()))?;
        let client = session
            .connect(&authority)
            .await
            .map_err(|error| RepoEventError::Connection(error.to_string()))?;
        Ok(Self { client })
    }

    /// Subscribe with the API-owned request shape used on the wire.
    ///
    /// A positive `after_event_id` first replays persisted matching events
    /// after that ID, then continues with live events. A non-positive cursor
    /// replays the available matching history before live delivery.
    pub async fn subscribe(
        &self,
        request: SubscribeRepoEventsRequest,
    ) -> Result<RepoEventSubscription, RepoEventError> {
        let last_event_id = request.after_event_id.max(0);
        let stream = self
            .client
            .routes()
            .subscribe_repo_events(&request)
            .await
            .map_err(|source| RepoEventError::Disconnected {
                last_event_id,
                source,
            })?;
        Ok(RepoEventSubscription {
            request,
            stream,
            last_event_id,
        })
    }

    /// Gracefully close the native connection.
    pub async fn close(self) {
        self.client.close().await;
    }

    /// Wrap an already-connected hosted client.
    ///
    /// Live discussion delivery shares this connection with `ListByState` /
    /// `GetDiscussion` so bootstrap and the event tail use one session.
    pub fn from_hosted_client(client: HostedClient) -> Self {
        Self { client }
    }
}

/// One long-lived repository-event stream and its durable resume cursor.
pub struct RepoEventSubscription {
    request: SubscribeRepoEventsRequest,
    stream: ServerStream<RepoEvent>,
    last_event_id: i64,
}

impl RepoEventSubscription {
    /// Receive the next wire event.
    ///
    /// Unlike a generic optional stream, clean EOF is an error because hosted
    /// repository-event subscriptions are long-lived. A connection failure is
    /// also returned immediately; neither case silently leaves a consumer
    /// waiting on a dead channel.
    pub async fn next(&mut self) -> Result<RepoEvent, RepoEventError> {
        match self.stream.next().await {
            Ok(Some(event)) => {
                self.last_event_id = self.last_event_id.max(event.event_id);
                Ok(event)
            }
            Ok(None) => Err(RepoEventError::Ended {
                last_event_id: self.last_event_id,
            }),
            Err(source) if is_refusal(&source) => Err(RepoEventError::Refused { source }),
            Err(source) => Err(RepoEventError::Disconnected {
                last_event_id: self.last_event_id,
                source,
            }),
        }
    }

    /// Last event ID successfully returned by [`Self::next`].
    pub fn last_event_id(&self) -> i64 {
        self.last_event_id
    }

    /// Clone the original wire request with its durable cursor advanced.
    pub fn resume_request(&self) -> SubscribeRepoEventsRequest {
        let mut request = self.request.clone();
        request.after_event_id = self.last_event_id;
        request
    }
}

fn is_refusal(error: &HostedError) -> bool {
    matches!(
        error,
        HostedError::Call {
            code: CallFailureCode::Unauthenticated
                | CallFailureCode::PermissionDenied
                | CallFailureCode::NotFound,
            ..
        }
    )
}

#[cfg(test)]
#[path = "repo_events_tests.rs"]
mod tests;
