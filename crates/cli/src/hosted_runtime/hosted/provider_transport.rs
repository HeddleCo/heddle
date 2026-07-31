use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use bytes::Bytes;
use cli_shared::ClientConfig;
use futures::{SinkExt, StreamExt, task::AtomicWaker};
use iroh::{
    EndpointAddr, EndpointId, TransportAddr,
    endpoint::transports::{CustomEndpoint, CustomSender, CustomTransport, RecvInfo, Transmit},
};
use iroh_base::CustomAddr;
use n0_watcher::Watchable;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

use super::{HostedError, Result};

const WEBSOCKET_TRANSPORT_ID: u64 = 0x6864_646c_6577_7301;
const LANE_QUEUE_DEPTH: usize = 64;

#[derive(Clone)]
pub(super) struct ProviderWebSocketTransport {
    inner: Arc<TransportState>,
}

struct TransportState {
    config: ClientConfig,
    bound: Mutex<Option<mpsc::Sender<Incoming>>>,
    lanes: Arc<Mutex<HashMap<Vec<u8>, Lane>>>,
}

struct Lane {
    outgoing: mpsc::Sender<Bytes>,
    waker: Arc<AtomicWaker>,
}

enum Incoming {
    Packet { remote: CustomAddr, data: Bytes },
    Error(io::Error),
}

impl std::fmt::Debug for ProviderWebSocketTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderWebSocketTransport")
            .field(
                "registered_provider_lanes",
                &self.inner.lanes.lock().map_or(0, |lanes| lanes.len()),
            )
            .finish()
    }
}

impl ProviderWebSocketTransport {
    pub(super) fn new(config: ClientConfig) -> Self {
        Self {
            inner: Arc::new(TransportState {
                config,
                bound: Mutex::new(None),
                lanes: Arc::new(Mutex::new(HashMap::new())),
            }),
        }
    }

    pub(super) fn register_source(
        &self,
        provider_id: &str,
        endpoint_id: &str,
        direct_url: &str,
        opaque_ticket: &str,
    ) -> Result<EndpointAddr> {
        validate_source(provider_id, direct_url, opaque_ticket)?;
        let endpoint_id: EndpointId = endpoint_id.parse().map_err(|error| {
            HostedError::InvalidDescriptor(format!("provider endpoint id: {error}"))
        })?;
        let incoming = self
            .inner
            .bound
            .lock()
            .map_err(|_| HostedError::InvalidDescriptor("provider transport lock".to_string()))?
            .clone()
            .ok_or_else(|| {
                HostedError::InvalidDescriptor("provider transport is not bound".to_string())
            })?;

        let handle = fresh_handle();
        let remote = CustomAddr::from_parts(WEBSOCKET_TRANSPORT_ID, &handle);
        let (outgoing, receiver) = mpsc::channel(LANE_QUEUE_DEPTH);
        let waker = Arc::new(AtomicWaker::new());
        self.inner
            .lanes
            .lock()
            .map_err(|_| HostedError::InvalidDescriptor("provider lane lock".to_string()))?
            .insert(
                handle.to_vec(),
                Lane {
                    outgoing,
                    waker: waker.clone(),
                },
            );
        tokio::spawn(run_lane(
            direct_url.to_string(),
            remote.clone(),
            receiver,
            incoming,
            waker,
            self.inner.config.clone(),
        ));

        Ok(EndpointAddr::from_parts(
            endpoint_id,
            [TransportAddr::Custom(remote)],
        ))
    }
}

impl CustomTransport for ProviderWebSocketTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let (incoming, receiver) = mpsc::channel(LANE_QUEUE_DEPTH * 4);
        let mut bound = self
            .inner
            .bound
            .lock()
            .map_err(|_| io::Error::other("provider transport lock poisoned"))?;
        if bound.replace(incoming).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "provider transport is already bound",
            ));
        }
        Ok(Box::new(BoundTransport {
            receiver,
            lanes: self.inner.lanes.clone(),
            local_addrs: Watchable::new(Vec::new()),
        }))
    }
}

struct BoundTransport {
    receiver: mpsc::Receiver<Incoming>,
    lanes: Arc<Mutex<HashMap<Vec<u8>, Lane>>>,
    local_addrs: Watchable<Vec<CustomAddr>>,
}

impl std::fmt::Debug for BoundTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundProviderWebSocketTransport")
            .finish_non_exhaustive()
    }
}

impl CustomEndpoint for BoundTransport {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
        self.local_addrs.watch()
    }

    fn create_sender(&self) -> Arc<dyn CustomSender> {
        Arc::new(WebSocketSender {
            lanes: self.lanes.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        context: &mut Context<'_>,
        buffers: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        recv_infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        if buffers.len() != metas.len() || buffers.len() != recv_infos.len() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provider receive batch lengths differ",
            )));
        }
        if buffers.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let first = match self.receiver.poll_recv(context) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "provider WebSocket transport closed",
                )));
            }
            Poll::Ready(Some(incoming)) => incoming,
        };
        let mut incoming = Some(first);
        let mut count = 0;
        while count < buffers.len() {
            let message = match incoming.take() {
                Some(message) => message,
                None => match self.receiver.try_recv() {
                    Ok(message) => message,
                    Err(_) => break,
                },
            };
            match message {
                Incoming::Error(error) if count == 0 => return Poll::Ready(Err(error)),
                Incoming::Error(_) => break,
                Incoming::Packet { remote, data } => {
                    if buffers[count].len() < data.len() {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "provider WebSocket datagram exceeds receive buffer",
                        )));
                    }
                    buffers[count][..data.len()].copy_from_slice(&data);
                    metas[count].len = data.len();
                    metas[count].stride = data.len();
                    recv_infos[count] = RecvInfo::new(remote, None);
                    count += 1;
                }
            }
        }
        Poll::Ready(Ok(count))
    }
}

struct WebSocketSender {
    lanes: Arc<Mutex<HashMap<Vec<u8>, Lane>>>,
}

impl std::fmt::Debug for WebSocketSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderWebSocketSender")
            .finish_non_exhaustive()
    }
}

impl CustomSender for WebSocketSender {
    fn is_valid_send_addr(&self, address: &CustomAddr) -> bool {
        address.id() == WEBSOCKET_TRANSPORT_ID
    }

    fn poll_send(
        &self,
        context: &mut Context<'_>,
        destination: &CustomAddr,
        _source: Option<&CustomAddr>,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        if destination.id() != WEBSOCKET_TRANSPORT_ID {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid provider custom transport address",
            )));
        }
        let lanes = match self.lanes.lock() {
            Ok(lanes) => lanes,
            Err(_) => return Poll::Ready(Err(io::Error::other("provider lane lock poisoned"))),
        };
        let Some(lane) = lanes.get(destination.data()) else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::NotFound,
                "provider WebSocket lane is not registered",
            )));
        };
        lane.waker.register(context.waker());
        match lane
            .outgoing
            .try_send(Bytes::copy_from_slice(transmit.contents))
        {
            Ok(()) => Poll::Ready(Ok(())),
            Err(mpsc::error::TrySendError::Full(_)) => Poll::Pending,
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "provider WebSocket lane closed",
            ))),
        }
    }
}

async fn run_lane(
    direct_url: String,
    remote: CustomAddr,
    mut outgoing: mpsc::Receiver<Bytes>,
    incoming: mpsc::Sender<Incoming>,
    waker: Arc<AtomicWaker>,
    config: ClientConfig,
) {
    while let Some(first) = outgoing.recv().await {
        waker.wake();
        let request = match direct_url.as_str().into_client_request() {
            Ok(request) => request,
            Err(error) => {
                let _ = incoming
                    .send(Incoming::Error(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        error.to_string(),
                    )))
                    .await;
                continue;
            }
        };
        let websocket = match crate::hosted_runtime::connect_websocket(request, &config).await {
            Ok((websocket, _)) => websocket,
            Err(error) => {
                let _ = incoming
                    .send(Incoming::Error(io::Error::other(error.to_string())))
                    .await;
                continue;
            }
        };
        let (mut sink, mut stream) = websocket.split();
        if sink.send(Message::Binary(first)).await.is_err() {
            let _ = incoming
                .send(Incoming::Error(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "provider WebSocket send failed",
                )))
                .await;
            continue;
        }

        loop {
            tokio::select! {
                packet = outgoing.recv() => {
                    let Some(packet) = packet else {
                        let _ = sink.close().await;
                        return;
                    };
                    waker.wake();
                    if sink.send(Message::Binary(packet)).await.is_err() {
                        let _ = incoming.send(Incoming::Error(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "provider WebSocket send failed",
                        ))).await;
                        break;
                    }
                }
                message = stream.next() => {
                    match message {
                        Some(Ok(Message::Binary(data))) => {
                            if incoming.send(Incoming::Packet {
                                remote: remote.clone(),
                                data,
                            }).await.is_err() {
                                return;
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            if sink.send(Message::Pong(data)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) | None => {
                            let _ = incoming.send(Incoming::Error(io::Error::new(
                                io::ErrorKind::ConnectionReset,
                                "provider WebSocket closed",
                            ))).await;
                            break;
                        }
                        Some(Ok(_)) => {
                            let _ = incoming.send(Incoming::Error(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "provider WebSocket sent a non-binary data frame",
                            ))).await;
                            break;
                        }
                        Some(Err(_)) => {
                            let _ = incoming.send(Incoming::Error(io::Error::new(
                                io::ErrorKind::ConnectionReset,
                                "provider WebSocket receive failed",
                            ))).await;
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn validate_source(provider_id: &str, direct_url: &str, opaque_ticket: &str) -> Result<()> {
    if provider_id.is_empty() || opaque_ticket.is_empty() {
        return Err(HostedError::InvalidDescriptor(
            "provider source identity is empty".to_string(),
        ));
    }
    let url = reqwest::Url::parse(direct_url)
        .map_err(|error| HostedError::InvalidDescriptor(format!("provider direct URL: {error}")))?;
    if url.scheme() != "wss"
        || url.path() != "/direct"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(HostedError::InvalidDescriptor(
            "provider direct URL is not a bare authenticated WSS route".to_string(),
        ));
    }
    let mut provider_matches = false;
    let mut ticket_matches = false;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "provider" if !provider_matches && value == provider_id => provider_matches = true,
            "ticket" if !ticket_matches && value == opaque_ticket => ticket_matches = true,
            _ => {
                return Err(HostedError::InvalidDescriptor(
                    "provider direct URL has ambiguous query data".to_string(),
                ));
            }
        }
    }
    if !provider_matches || !ticket_matches {
        return Err(HostedError::InvalidDescriptor(
            "provider direct URL does not match its source".to_string(),
        ));
    }
    Ok(())
}

fn fresh_handle() -> [u8; 16] {
    let mut handle = [0; 16];
    rand::fill(&mut handle);
    handle
}

#[cfg(test)]
mod tests {
    use super::validate_source;

    #[test]
    fn provider_url_requires_exact_provider_and_ticket_query() {
        validate_source(
            "provider-a",
            "wss://iroh.example/direct?provider=provider-a&ticket=opaque",
            "opaque",
        )
        .unwrap();

        for invalid in [
            "ws://iroh.example/direct?provider=provider-a&ticket=opaque",
            "wss://iroh.example/other?provider=provider-a&ticket=opaque",
            "wss://iroh.example/direct?provider=provider-a&ticket=opaque&ticket=other",
            "wss://attacker@iroh.example/direct?provider=provider-a&ticket=opaque",
        ] {
            assert!(validate_source("provider-a", invalid, "opaque").is_err());
        }
    }
}
