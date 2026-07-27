// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use api::heddle::api::v1alpha1::{
    ListRefsRequest, ListRefsResponse, PullComplete, PullReady, PullRequest, RepositoryRef,
    StateId as ProtoStateId, repository_ref::Reference,
};
use iroh::{
    Endpoint, EndpointAddr,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use objects::{object::StateId, store::FsStore};
use prost::Message;
use wire::{NativePackFileBundle, NativePackStreamingWriter, ObjectInfo};

use crate::{
    Result, TransportError,
    codec::{
        ALPN, METHOD_LIST_REFS, METHOD_PULL, METHOD_WIRE_BENCHMARK, read_request, transport_config,
        write_file_body, write_pull_prelude, write_response, write_synthetic_body,
    },
};

/// Minimal repository view needed to exercise refs and native pack transfer.
pub struct ExperimentRepository {
    store: FsStore,
    refs: ListRefsResponse,
    state: StateId,
    objects: Vec<ObjectInfo>,
}

impl std::fmt::Debug for ExperimentRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExperimentRepository")
            .field("state", &self.state)
            .field("object_count", &self.objects.len())
            .finish_non_exhaustive()
    }
}

impl ExperimentRepository {
    pub fn new(
        store: FsStore,
        refs: ListRefsResponse,
        state: StateId,
        objects: Vec<ObjectInfo>,
    ) -> Self {
        Self {
            store,
            refs,
            state,
            objects,
        }
    }
}

/// Running direct-only Iroh server for the transport experiment.
#[derive(Debug, Clone)]
pub struct IrohServer {
    router: Router,
}

impl IrohServer {
    /// Bind a loopback-only endpoint and serve one experiment repository.
    pub async fn spawn_loopback(repo: Arc<ExperimentRepository>) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::Minimal)
            .transport_config(transport_config())
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .map_err(|error| TransportError::Iroh(error.to_string()))?
            .bind()
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
        let router = Router::builder(endpoint)
            .accept(ALPN, HeddleProtocol { repo })
            .spawn();
        Ok(Self { router })
    }

    /// Current dialable endpoint address, including direct socket addresses.
    pub fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    /// Shut down the router and endpoint.
    pub async fn shutdown(&self) -> Result<()> {
        self.router
            .shutdown()
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))
    }
}

#[derive(Debug, Clone)]
struct HeddleProtocol {
    repo: Arc<ExperimentRepository>,
}

impl ProtocolHandler for HeddleProtocol {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        let mut requests = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                incoming = connection.accept_bi() => {
                    let Ok((mut send, mut recv)) = incoming else {
                        break;
                    };
                    let repo = self.repo.clone();
                    requests.spawn(async move { handle_request(&repo, &mut send, &mut recv).await });
                }
                request = requests.join_next(), if !requests.is_empty() => {
                    match request {
                        Some(Ok(Ok(()))) => {}
                        Some(Ok(Err(error))) => return Err(AcceptError::from_err(error)),
                        Some(Err(error)) => return Err(AcceptError::from_err(error)),
                        None => {}
                    }
                }
            }
        }

        while let Some(request) = requests.join_next().await {
            match request {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(AcceptError::from_err(error)),
                Err(error) => return Err(AcceptError::from_err(error)),
            }
        }
        Ok(())
    }
}

async fn handle_request(
    repo: &ExperimentRepository,
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
) -> Result<()> {
    let (method, payload) = read_request(recv).await?;
    match method.as_ref() {
        path if path == METHOD_LIST_REFS.as_bytes() => {
            let request = ListRefsRequest::decode(payload)?;
            validate_repo_path(&request.repo_path)?;
            write_response(send, &repo.refs).await
        }
        path if path == METHOD_PULL.as_bytes() => {
            let request = PullRequest::decode(payload)?;
            serve_pull(repo, send, request).await
        }
        path if path == METHOD_WIRE_BENCHMARK.as_bytes() => {
            let length: [u8; 8] = payload.as_ref().try_into().map_err(|_| {
                TransportError::InvalidFrame(format!(
                    "wire benchmark request must contain 8 bytes, found {}",
                    payload.len()
                ))
            })?;
            write_synthetic_body(send, u64::from_be_bytes(length)).await
        }
        other => Err(TransportError::InvalidFrame(format!(
            "unknown operation stream method {}",
            String::from_utf8_lossy(other)
        ))),
    }
}

async fn serve_pull(
    repo: &ExperimentRepository,
    response: &mut iroh::endpoint::SendStream,
    request: PullRequest,
) -> Result<()> {
    let (state, pack, ready) = prepare_pull(repo, request)?;
    write_pull_prelude(response, &ready, pack.pack_len, pack.index_len).await?;
    write_file_body(response, &pack.pack_path, pack.pack_len).await?;
    write_file_body(response, &pack.index_path, pack.index_len).await?;
    write_response(
        response,
        &PullComplete {
            success: true,
            new_state: Some(state),
            new_revision_address: format!("heddle:{}", repo.state.to_string_full()),
            ..PullComplete::default()
        },
    )
    .await
}

fn prepare_pull(
    repo: &ExperimentRepository,
    request: PullRequest,
) -> Result<(ProtoStateId, NativePackFileBundle, PullReady)> {
    validate_repo_path(&request.repo_path)?;
    let state = ProtoStateId {
        value: repo.state.as_bytes().to_vec(),
    };
    if request
        .target_state
        .as_ref()
        .is_some_and(|target| target != &state)
    {
        return Err(TransportError::Remote(
            "experiment only serves its advertised state".to_string(),
        ));
    }
    let pack = build_native_pack_files(repo)?;
    let ready = PullReady {
        remote_state: Some(state.clone()),
        object_count: repo.objects.len() as u32,
        full_closure_available: true,
        remote_revision_address: format!("heddle:{}", repo.state.to_string_full()),
        ..PullReady::default()
    };
    Ok((state, pack, ready))
}

fn build_native_pack_files(repo: &ExperimentRepository) -> Result<NativePackFileBundle> {
    let packable_count = repo
        .objects
        .iter()
        .filter(|object| wire::is_native_packable_object_type(object.obj_type))
        .count();
    let object_count = u64::try_from(packable_count).map_err(|_| {
        TransportError::InvalidFrame("native pack object count exceeds u64".to_string())
    })?;
    let mut writer = NativePackStreamingWriter::new_in(repo.store.root(), object_count)?;
    for object in &repo.objects {
        if wire::is_native_packable_object_type(object.obj_type) {
            writer.add_object_data(wire::load_object_data(
                &repo.store,
                &object.id,
                object.obj_type,
            )?)?;
        }
    }
    writer.finish().map_err(TransportError::from)
}

fn validate_repo_path(repo_path: &Option<RepositoryRef>) -> Result<()> {
    let path = repo_path
        .as_ref()
        .and_then(|repo| match repo.reference.as_ref() {
            Some(Reference::CanonicalPath(path) | Reference::HostedId(path)) => Some(path.as_str()),
            None => None,
        });
    if path.is_none_or(|path| path.is_empty() || path == "/") {
        Ok(())
    } else {
        Err(TransportError::Remote(format!(
            "single-repository experiment cannot route repo path '{}'",
            path.unwrap_or_default()
        )))
    }
}
