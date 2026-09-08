// SPDX-License-Identifier: Apache-2.0
//! Known-Thread replication on a directly authenticated Iroh connection.
//! The host supplies its resolved owner/account authority and admission facets.
//! Unknown Thread creation and source-object transfer have separate boundaries.
use std::{collections::BTreeSet, future::Future, sync::Arc, time::Duration};

use api::{
    framing,
    v2::client::{MessageReader, MessageWriter, RpcTransport},
};
use iroh::endpoint::Connection;
use objects::{
    object::thread_replication::{OPERATION_FORMAT, ThreadFacet},
    store::ObjectStore,
};
use prost::Message;
use repo::thread_replication::ThreadReplica;

use crate::{
    Rpc,
    authority::RootAuthority,
    contract::*,
    live_replication::{self, Feed, Side},
    replication::{self, Session, native::LocalReplica},
    rpc,
    transport::{self, Authorize, IrohTransport, Reader, Writer},
};

const FRAME_LIMIT: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

/// A resolved local Thread, with the admission scope chosen by its host.
/// Export remains subject to the Thread's current per-destination sharing policy.
#[derive(Clone)]
pub struct Peer {
    replica: ThreadReplica,
    endpoint: EndpointRef,
    facets: BTreeSet<ThreadFacet>,
}
impl Peer {
    pub fn new(
        replica: ThreadReplica,
        endpoint: EndpointRef,
        facets: BTreeSet<ThreadFacet>,
    ) -> Result<Self, transport::Error> {
        validate_endpoint(&endpoint)?;
        if facets.is_empty() {
            return Err(transport::Error::Protocol(
                "replication requires an admission facet",
            ));
        }
        Ok(Self {
            replica,
            endpoint,
            facets,
        })
    }

    fn reference(&self) -> Result<ThreadRef, transport::Error> {
        let genesis = self.replica.genesis().map_err(store_error)?;
        Ok(ThreadRef {
            spool: Some(SpoolRef { id: genesis.spool }),
            id: Some(ThreadId {
                value: self.replica.thread_id().as_bytes().to_vec(),
            }),
        })
    }

    /// Drives one RPC until cancellation, transport failure, or live authority
    /// revocation. Reopen with the same replica to repair from durable frontiers.
    pub async fn connect<S, A, G, F>(
        &self,
        connection: Connection,
        destination: EndpointKind,
        signer: A,
        store: Arc<S>,
        feed: &Feed,
        authorize: G,
    ) -> live_replication::Result<(), replication::native::Error>
    where
        S: ObjectStore + Send + Sync + 'static,
        A: Authorize,
        G: Fn() -> F + Clone + Send + Sync + 'static,
        F: Future<Output = Result<(), transport::Error>> + Send,
    {
        let remote_key = *connection.remote_id().as_bytes();
        let destination = EndpointRef {
            public_key: remote_key.to_vec(),
            kind: destination as i32,
        };
        validate_endpoint(&destination)?;
        let thread = self.reference()?;
        let (_, version) = self.replica.sharing(&remote_key).map_err(store_error)?;
        let opening = ReplicationOpen {
            thread: Some(thread.clone()),
            facets: self
                .facets
                .iter()
                .copied()
                .map(replication::wire_facet)
                .collect(),
            sharing_policy_version: version.map(|v| v.as_bytes().to_vec()).unwrap_or_default(),
            thread_genesis: None,
            budget: Some(ReadBudget {
                max_items: 64,
                max_frame_bytes: FRAME_LIMIT as u32,
                max_snapshot_bytes: 0,
            }),
            record_formats: vec![OPERATION_FORMAT.into()],
            session_nonce: uuid::Uuid::new_v4().as_bytes().to_vec(),
            source: Some(self.endpoint.clone()),
            destination: Some(destination.clone()),
        };
        let transport = IrohTransport::new(connection, signer, FRAME_LIMIT, TIMEOUT)?;
        let (writer, mut reader) = transport
            .exchange(
                rpc::SyncServiceReplicateThread::METHOD,
                ReplicateThreadRequest {
                    body: Some(replicate_thread_request::Body::Open(opening)),
                }
                .encode_to_vec(),
            )
            .await?;
        let bytes = reader.next().await?.ok_or(transport::Error::Protocol(
            "replication closed before Ready",
        ))?;
        let response =
            ReplicateThreadResponse::decode(bytes.as_slice()).map_err(transport::Error::from)?;
        let Some(replicate_thread_response::Body::Ready(ready)) = response.body else {
            return Err(transport::Error::Protocol("replication requires Ready").into());
        };
        if ready.endpoint.as_ref() != Some(&destination)
            || ready.thread.as_ref() != Some(&thread)
            || ready.record_formats != [OPERATION_FORMAT]
        {
            return Err(transport::Error::Protocol(
                "replication Ready binding differs from opening",
            )
            .into());
        }
        let facets = parse_facets(&ready.facets)?;
        if facets.is_empty() || !facets.is_subset(&self.facets) {
            return Err(
                transport::Error::Protocol("replication Ready widened admission scope").into(),
            );
        }
        let budget = ready.budget.ok_or(transport::Error::Protocol(
            "replication Ready requires budget",
        ))?;
        if budget.max_items == 0
            || budget.max_items > 64
            || budget.max_frame_bytes != FRAME_LIMIT as u32
        {
            return Err(transport::Error::Protocol("unsupported replication Ready budget").into());
        }
        let session = Session::new(
            LocalReplica::new(self.replica.clone(), store),
            remote_key,
            facets,
            budget.max_items as usize,
        )?;
        live_replication::run(session, reader, writer, Side::Initiator, feed, authorize).await
    }

    /// Accept exactly one ReplicateThread RPC. The connection must be routed
    /// here by the host; other service methods are not advertised or emulated.
    pub async fn accept<S>(
        &self,
        connection: Connection,
        authority: Arc<RootAuthority>,
        store: Arc<S>,
        feed: &Feed,
    ) -> live_replication::Result<(), replication::native::Error>
    where
        S: ObjectStore + Send + Sync + 'static,
    {
        let remote_key = *connection.remote_id().as_bytes();
        let (send, mut recv) = tokio::time::timeout(TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| transport::Error::Timeout)?
            .map_err(io_error)?;
        let mut writer = Writer::new(send, FRAME_LIMIT, TIMEOUT);
        let opening = async {
            let mut prelude = vec![0; 6];
            recv.read_exact(&mut prelude).await.map_err(io_error)?;
            let method_size = u16::from_be_bytes([prelude[0], prelude[1]]) as usize;
            let context_size =
                u32::from_be_bytes([prelude[2], prelude[3], prelude[4], prelude[5]]) as usize;
            if method_size == 0
                || method_size > framing::MAX_METHOD_PATH
                || context_size > framing::MAX_CALL_CONTEXT
            {
                return Err(transport::Error::Protocol(
                    "opening exceeds metadata limits",
                ));
            }
            prelude.resize(6 + method_size + context_size, 0);
            recv.read_exact(&mut prelude[6..]).await.map_err(io_error)?;
            let (request, _) = framing::decode_request_prelude(&prelude)?
                .ok_or(transport::Error::Protocol("incomplete opening metadata"))?;
            if request.method != rpc::SyncServiceReplicateThread::METHOD.path {
                return Err(transport::Error::Protocol(
                    "unsupported RPC on replication boundary",
                ));
            }
            Ok(request.context)
        };
        let context = tokio::time::timeout(TIMEOUT, opening)
            .await
            .map_err(|_| transport::Error::Timeout)??;
        let mut reader = Reader::new(recv, FRAME_LIMIT, TIMEOUT);
        let bytes = reader
            .next()
            .await?
            .ok_or(transport::Error::Protocol("missing replication opening"))?;
        let peer = self.clone();
        let verifier = authority.clone();
        let checked = tokio::task::spawn_blocking(move || -> Result<_, transport::Error> {
            let request = ReplicateThreadRequest::decode(bytes.as_slice())?;
            let Some(replicate_thread_request::Body::Open(open)) = request.body else {
                return Err(transport::Error::Protocol("replication requires Open"));
            };
            peer.validate_open(&open, remote_key)?;
            let verified = verifier.verify(
                &context,
                rpc::SyncServiceReplicateThread::METHOD,
                &bytes,
                "write",
                &peer.replica,
            )?;
            let facets = parse_facets(&open.facets)?
                .intersection(&peer.facets)
                .copied()
                .collect::<BTreeSet<_>>();
            if facets.is_empty() {
                return Err(transport::Error::Protocol(
                    "no authorized replication facets",
                ));
            }
            let requested = open.budget.unwrap_or_default();
            let max_items = if requested.max_items == 0 {
                64
            } else {
                requested.max_items.min(64)
            };
            let (_, version) = peer.replica.sharing(&remote_key).map_err(store_error)?;
            let ready = ReplicationReady {
                endpoint: Some(peer.endpoint.clone()),
                thread: Some(peer.reference()?),
                facets: facets
                    .iter()
                    .copied()
                    .map(replication::wire_facet)
                    .collect(),
                sharing_policy_version: version.map(|v| v.as_bytes().to_vec()).unwrap_or_default(),
                budget: Some(ReadBudget {
                    max_items,
                    max_frame_bytes: FRAME_LIMIT as u32,
                    max_snapshot_bytes: 0,
                }),
                record_formats: vec![OPERATION_FORMAT.into()],
            };
            Ok((verified, facets, max_items, ready))
        })
        .await
        .map_err(|e| transport::Error::Io(e.to_string()))?;
        let (verified, facets, max_items, ready) = match checked {
            Ok(value) => value,
            Err(error) => {
                writer
                    .fail(&api::heddle::api::v1alpha1::CallFailure {
                        code: 7,
                        message: error.to_string(),
                        ..Default::default()
                    })
                    .await?;
                return Err(error.into());
            }
        };
        writer
            .send(
                ReplicateThreadResponse {
                    body: Some(replicate_thread_response::Body::Ready(ready)),
                }
                .encode_to_vec(),
            )
            .await?;
        let session = Session::new(
            LocalReplica::new(self.replica.clone(), store),
            remote_key,
            facets,
            max_items as usize,
        )?;
        live_replication::run(session, reader, writer, Side::Acceptor, feed, move || {
            std::future::ready(authority.recheck(&verified))
        })
        .await
    }

    fn validate_open(
        &self,
        open: &ReplicationOpen,
        remote_key: [u8; 32],
    ) -> Result<(), transport::Error> {
        if open.thread.as_ref() != Some(&self.reference()?) || open.thread_genesis.is_some() {
            return Err(transport::Error::Protocol(
                "replication requires an already resolved Thread",
            ));
        }
        let source = open.source.as_ref().ok_or(transport::Error::Protocol(
            "opening requires source endpoint",
        ))?;
        validate_endpoint(source)?;
        if source.public_key != remote_key || open.destination.as_ref() != Some(&self.endpoint) {
            return Err(transport::Error::Protocol(
                "opening endpoints differ from Iroh connection",
            ));
        }
        if open.session_nonce.len() != 16
            || !open.record_formats.iter().any(|f| f == OPERATION_FORMAT)
        {
            return Err(transport::Error::Protocol(
                "unsupported replication session or format",
            ));
        }
        // Native operations are indivisible. This first implementation supports
        // one advertised frame size rather than silently exceeding a small cap.
        if open
            .budget
            .as_ref()
            .is_some_and(|b| b.max_frame_bytes != 0 && b.max_frame_bytes != FRAME_LIMIT as u32)
        {
            return Err(transport::Error::Protocol(
                "unsupported replication frame budget",
            ));
        }
        Ok(())
    }
}

fn parse_facets(values: &[i32]) -> Result<BTreeSet<ThreadFacet>, transport::Error> {
    if values.is_empty() || values.len() > 2 {
        return Err(transport::Error::Protocol(
            "replication requires one or two distinct facets",
        ));
    }
    let facets = values
        .iter()
        .map(|v| {
            replication::native_facet(*v)
                .map_err(|_| transport::Error::Protocol("unsupported replication facet"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if facets.len() != values.len() {
        return Err(transport::Error::Protocol("duplicate replication facet"));
    }
    Ok(facets)
}
fn validate_endpoint(endpoint: &EndpointRef) -> Result<(), transport::Error> {
    if endpoint.public_key.len() != 32
        || !matches!(
            EndpointKind::try_from(endpoint.kind),
            Ok(EndpointKind::Device | EndpointKind::Weft)
        )
    {
        return Err(transport::Error::Protocol("invalid replication endpoint"));
    }
    Ok(())
}
fn io_error(error: impl std::fmt::Display) -> transport::Error {
    transport::Error::Io(error.to_string())
}
fn store_error(error: repo::thread_replication::Error) -> transport::Error {
    io_error(error)
}

#[cfg(test)]
#[path = "replication_rpc_tests.rs"]
mod tests;
