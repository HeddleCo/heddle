// SPDX-License-Identifier: Apache-2.0
use std::time::{Duration, Instant};

use api::heddle::api::v1alpha1::{
    ListRefsRequest, ListRefsResponse, PullComplete, PullReady, PullRequest, RepositoryRef,
    repository_ref::Reference,
};
use iroh::{Endpoint, EndpointAddr, endpoint::presets};
use objects::store::{FsStore, PackObjectId};

use crate::{
    Result, TransportError,
    codec::{
        ALPN, METHOD_LIST_REFS, METHOD_PULL, read_file_body, read_pull_prelude, read_response,
        transport_config, write_request, write_wire_benchmark_request,
    },
};

/// Result of draining a generated response directly from an Iroh QUIC stream.
#[derive(Debug, Clone, Copy)]
pub struct WireBenchmarkOutcome {
    pub bytes: u64,
    pub transfer_latency: Duration,
}

/// Result of one experimental native-pack pull.
#[derive(Debug)]
pub struct NativePullOutcome {
    pub ready: PullReady,
    pub complete: PullComplete,
    pub installed: Vec<PackObjectId>,
    pub pack_bytes: usize,
    pub index_bytes: usize,
    pub ready_latency: Duration,
    pub transfer_latency: Duration,
    pub install_latency: Duration,
    pub completion_latency: Duration,
}

/// Connected client using one reusable Iroh QUIC connection.
#[derive(Debug)]
pub struct IrohClient {
    endpoint: Endpoint,
    connection: iroh::endpoint::Connection,
}

impl IrohClient {
    /// Create a loopback-only endpoint and connect to the experiment server.
    pub async fn connect_loopback(addr: EndpointAddr) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::Minimal)
            .transport_config(transport_config())
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .map_err(|error| TransportError::Iroh(error.to_string()))?
            .bind()
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
        let connection = endpoint
            .connect(addr, ALPN)
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
        Ok(Self {
            endpoint,
            connection,
        })
    }

    /// List repository refs over a fresh bidirectional QUIC stream.
    pub async fn list_refs(&self, repo_path: impl Into<String>) -> Result<ListRefsResponse> {
        let (mut send, mut recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
        let request = ListRefsRequest {
            repo_path: Some(RepositoryRef {
                reference: Some(Reference::CanonicalPath(repo_path.into())),
            }),
        };
        write_request(&mut send, METHOD_LIST_REFS, &request).await?;
        read_response(&mut recv).await
    }

    /// Measure the established-connection transport ceiling without storage or codecs.
    pub async fn benchmark_wire(&self, expected_bytes: u64) -> Result<WireBenchmarkOutcome> {
        let (mut send, mut recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
        write_wire_benchmark_request(&mut send, expected_bytes).await?;

        let transfer_started = Instant::now();
        let mut received = 0u64;
        let mut chunks: [bytes::Bytes; 32] = std::array::from_fn(|_| bytes::Bytes::new());
        while let Some(count) = recv
            .read_many_chunks(&mut chunks)
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?
        {
            for chunk in chunks.iter().take(count) {
                received = received.checked_add(chunk.len() as u64).ok_or_else(|| {
                    TransportError::InvalidFrame(
                        "wire benchmark received byte count overflow".to_string(),
                    )
                })?;
            }
        }
        let transfer_latency = transfer_started.elapsed();
        if received != expected_bytes {
            return Err(TransportError::InvalidFrame(format!(
                "wire benchmark expected {expected_bytes} bytes, received {received}"
            )));
        }
        Ok(WireBenchmarkOutcome {
            bytes: received,
            transfer_latency,
        })
    }

    /// Pull the experiment's native object pack and install it into `target`.
    pub async fn pull_native(
        &self,
        target: &FsStore,
        request: PullRequest,
    ) -> Result<NativePullOutcome> {
        let request_started = Instant::now();
        let (mut send, mut recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
        write_request(&mut send, METHOD_PULL, &request).await?;
        let (ready, pack_len, index_len): (PullReady, _, _) = read_pull_prelude(&mut recv).await?;
        let ready_latency = request_started.elapsed();

        let transfer_started = Instant::now();
        let mut spool = wire::PackChunkSpool::new_in(target.root())?;
        let pack_bytes = read_file_body(
            &mut recv,
            pack_len,
            wire::MAX_RECEIVED_PACK_SIZE,
            &mut spool,
            false,
        )
        .await?;
        let index_bytes = read_file_body(
            &mut recv,
            index_len,
            wire::MAX_RECEIVED_PACK_INDEX_SIZE,
            &mut spool,
            true,
        )
        .await?;
        let transfer_latency = transfer_started.elapsed();
        let install_started = Instant::now();
        let installed = spool.install_into(target)?;
        let install_latency = install_started.elapsed();
        let completion_started = Instant::now();
        let complete: PullComplete = read_response(&mut recv).await?;
        let completion_latency = completion_started.elapsed();
        if !complete.success {
            return Err(TransportError::Remote(complete.error));
        }
        Ok(NativePullOutcome {
            ready,
            complete,
            installed,
            pack_bytes: usize::try_from(pack_bytes).map_err(|_| {
                TransportError::InvalidFrame("pack length exceeds usize".to_string())
            })?,
            index_bytes: usize::try_from(index_bytes).map_err(|_| {
                TransportError::InvalidFrame("index length exceeds usize".to_string())
            })?,
            ready_latency,
            transfer_latency,
            install_latency,
            completion_latency,
        })
    }

    /// Close the connection and endpoint cleanly.
    pub async fn close(self) {
        self.connection.close(0u32.into(), b"experiment complete");
        self.endpoint.close().await;
    }
}
