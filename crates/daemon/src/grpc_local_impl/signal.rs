// SPDX-License-Identifier: Apache-2.0
//! Local-mode `SignalService`. Reads risk signals attached to states by W1
//! (`State.risk_signals`) and reports per-signal fire rates over a rolling
//! window. The actual signal *computation* lands in R3 (`crates/state_review`);
//! this service exposes whatever's already on disk.

use objects::store::ObjectStore;
use std::{collections::HashMap, pin::Pin};

use futures::Stream;
use grpc::heddle::v1::{
    ComputeStateSignalsRequest, ComputeStateSignalsResponse, GetRepoSignalHealthRequest,
    PathSymbolRef, RepoSignalHealthReport, RiskSignal as ProtoRiskSignal,
    SignalAnchor as ProtoSignalAnchor, SignalHealthEntry, SignalUpdate,
    SubscribeSignalUpdatesRequest, signal_service_server::SignalService,
};
use objects::object::{ChangeId, RiskSignal, RiskSignalBlob, State};
use repo::Repository;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::{GrpcLocalService, to_status};

#[derive(Clone)]
pub struct LocalSignalService {
    inner: GrpcLocalService,
}

impl LocalSignalService {
    pub fn new(inner: GrpcLocalService) -> Self {
        Self { inner }
    }

    fn repo(&self) -> &Repository {
        self.inner.repo()
    }
}

#[tonic::async_trait]
impl SignalService for LocalSignalService {
    type SubscribeSignalUpdatesStream =
        Pin<Box<dyn Stream<Item = Result<SignalUpdate, Status>> + Send>>;

    async fn compute_state_signals(
        &self,
        request: Request<ComputeStateSignalsRequest>,
    ) -> Result<Response<ComputeStateSignalsResponse>, Status> {
        let req = request.into_inner();
        if req.state_id.is_empty() {
            return Err(Status::invalid_argument("state_id is required"));
        }
        let change_id = ChangeId::try_from_slice(&req.state_id)
            .map_err(|err| Status::invalid_argument(format!("invalid state_id: {err}")))?;
        let state = self
            .repo()
            .store()
            .get_state(&change_id)
            .map_err(to_status)?
            .ok_or_else(|| Status::not_found(format!("state {change_id} not found")))?;
        // Real R3 computation lives in `crates/state_review`. For W2 this
        // service surfaces whatever signals already live on the state via
        // `state.risk_signals`. When R3 wires through, replace this with
        // a call into `state_review::registry::run_all`.
        let signals = load_signals(self.repo(), &state)?;
        let proto_signals = signals
            .iter()
            .map(|s| signal_to_proto(s, "visible"))
            .collect();
        Ok(Response::new(ComputeStateSignalsResponse {
            signals: proto_signals,
            tick_budget: 3,
        }))
    }

    async fn get_repo_signal_health(
        &self,
        request: Request<GetRepoSignalHealthRequest>,
    ) -> Result<Response<RepoSignalHealthReport>, Status> {
        let req = request.into_inner();
        let window = if req.window_states == 0 {
            DEFAULT_HEALTH_WINDOW
        } else {
            req.window_states.min(MAX_HEALTH_WINDOW) as usize
        };
        // Walk recent states from HEAD's first-parent chain, up to `window`.
        // For each, collect any `RiskSignal`s on disk. Aggregate per
        // module_id; fire_rate = states-with-signals / states-considered.
        let states = walk_recent_states(self.repo(), window).map_err(to_status)?;
        let visited = states.len() as u32;
        let mut per_module: HashMap<String, u32> = HashMap::new();
        for state in &states {
            let signals = load_signals(self.repo(), state)?;
            // One state contributes at most once per module so a noisy
            // module isn't double-counted by firing many signals on the
            // same state.
            let mut seen_modules = std::collections::HashSet::new();
            for sig in &signals {
                let key = sig.producer.module.clone();
                if seen_modules.insert(key.clone()) {
                    *per_module.entry(key).or_insert(0) += 1;
                }
            }
        }
        let warn_threshold = 0.5_f32;
        let entries = per_module
            .into_iter()
            .map(|(module_id, hit_count)| {
                let fire_rate = if visited == 0 {
                    0.0
                } else {
                    hit_count as f32 / visited as f32
                };
                SignalHealthEntry {
                    module_id,
                    fire_rate,
                    warn: fire_rate > warn_threshold,
                }
            })
            .collect();
        Ok(Response::new(RepoSignalHealthReport {
            entries,
            window_states: visited,
        }))
    }

    async fn subscribe_signal_updates(
        &self,
        _request: Request<SubscribeSignalUpdatesRequest>,
    ) -> Result<Response<Self::SubscribeSignalUpdatesStream>, Status> {
        // W2 lands the contract; live event broadcasting wires up in R3 once
        // the signal registry can recompute on capture. For now we open a
        // channel that closes immediately — clients see EOF and reconnect
        // when the producer becomes available.
        let (_tx, rx) = tokio::sync::mpsc::channel::<Result<SignalUpdate, Status>>(1);
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

const DEFAULT_HEALTH_WINDOW: usize = 200;
const MAX_HEALTH_WINDOW: u32 = 5_000;

fn load_signals(repo: &Repository, state: &State) -> Result<Vec<RiskSignal>, Status> {
    let Some(hash) = state.risk_signals else {
        return Ok(Vec::new());
    };
    let blob = repo
        .store()
        .get_blob(&hash)
        .map_err(to_status)?
        .ok_or_else(|| {
            Status::data_loss(format!(
                "risk_signals blob {hash} referenced by state {} is missing",
                state.change_id
            ))
        })?;
    let parsed = RiskSignalBlob::decode(blob.content()).map_err(|err| {
        Status::internal(format!(
            "failed to decode risk signals on state {}: {err}",
            state.change_id
        ))
    })?;
    Ok(parsed.signals)
}

fn walk_recent_states(repo: &Repository, window: usize) -> objects::error::Result<Vec<State>> {
    let mut out = Vec::new();
    let mut cursor = repo.head()?;
    while let Some(id) = cursor {
        if out.len() >= window {
            break;
        }
        let Some(state) = repo.store().get_state(&id)? else {
            break;
        };
        let parent = state.parents.first().copied();
        out.push(state);
        cursor = parent;
    }
    Ok(out)
}

fn signal_to_proto(sig: &RiskSignal, visibility: &str) -> ProtoRiskSignal {
    let (start_line, end_line) = sig.anchor.line_range.unwrap_or((0, 0));
    ProtoRiskSignal {
        kind: sig.kind.as_str().to_string(),
        anchor: Some(ProtoSignalAnchor {
            file: sig.anchor.file.clone(),
            symbol: sig.anchor.symbol.clone().unwrap_or_default(),
            start_line,
            end_line,
        }),
        reason: sig.reason.clone(),
        producer_module: sig.producer.module.clone(),
        producer_version: sig.producer.version,
        computed_at: Some(prost_types::Timestamp {
            seconds: sig.computed_at,
            nanos: 0,
        }),
        visibility: visibility.to_string(),
    }
}

// Small helper kept private; exported via PathSymbolRef wherever needed.
#[allow(dead_code)]
fn make_path_symbol(file: &str, symbol: &str) -> PathSymbolRef {
    PathSymbolRef {
        file: file.to_string(),
        symbol: symbol.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{Attribution, Blob, Principal, ProducerId, RiskSignalKind, SignalAnchor};
    use tempfile::TempDir;

    use super::*;

    fn fresh_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    fn local_service(repo: Repository) -> LocalSignalService {
        let dedup = std::sync::Arc::new(
            repo::operation_dedup::OperationDedupStore::open(repo.heddle_dir()).unwrap(),
        );
        LocalSignalService::new(GrpcLocalService::new(std::sync::Arc::new(repo), dedup))
    }

    fn snapshot_with_signals(repo: &Repository, signals: Vec<RiskSignal>) -> ChangeId {
        let attribution = Attribution::human(Principal::new("Alice", "alice@example.com"));
        let snapshot = repo
            .snapshot_with_attribution(Some("test snapshot".to_string()), None, attribution)
            .unwrap();
        let blob = RiskSignalBlob::new(signals).encode().unwrap();
        let hash = repo.store().put_blob(&Blob::new(blob)).unwrap();
        let state = repo
            .store()
            .get_state(&snapshot.change_id)
            .unwrap()
            .unwrap();
        let updated = state.with_risk_signals(hash);
        repo.store().put_state(&updated).unwrap();
        snapshot.change_id
    }

    fn sample_signal(kind: RiskSignalKind, reason: &str) -> RiskSignal {
        RiskSignal {
            kind,
            anchor: SignalAnchor::symbol("src/lib.rs", "foo"),
            reason: reason.to_string(),
            producer: ProducerId::new("novelty.tree_sitter", 1),
            computed_at: 1_700_000_000,
            computed_against: None,
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn compute_state_signals_returns_persisted_signals() {
        let (_t, repo) = fresh_repo();
        let signal = sample_signal(RiskSignalKind::Novelty, "novel control flow shape");
        let state_id = snapshot_with_signals(&repo, vec![signal]);
        let svc = local_service(repo);
        let resp = svc
            .compute_state_signals(Request::new(ComputeStateSignalsRequest {
                repo_path: String::new(),
                state_id: state_id.as_bytes().to_vec(),
                prior_state_id: Vec::new(),
            }))
            .await
            .unwrap();
        let signals = resp.into_inner().signals;
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, "novelty");
        assert_eq!(signals[0].reason, "novel control flow shape");
        assert_eq!(signals[0].visibility, "visible");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn compute_state_signals_returns_empty_when_state_has_no_signals() {
        let (_t, repo) = fresh_repo();
        let attribution = Attribution::human(Principal::new("Alice", "alice@example.com"));
        let snap = repo
            .snapshot_with_attribution(Some("plain".to_string()), None, attribution)
            .unwrap();
        let svc = local_service(repo);
        let resp = svc
            .compute_state_signals(Request::new(ComputeStateSignalsRequest {
                repo_path: String::new(),
                state_id: snap.change_id.as_bytes().to_vec(),
                prior_state_id: Vec::new(),
            }))
            .await
            .unwrap();
        assert!(resp.into_inner().signals.is_empty());
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn invalid_state_id_returns_invalid_argument() {
        let (_t, repo) = fresh_repo();
        let svc = local_service(repo);
        let err = svc
            .compute_state_signals(Request::new(ComputeStateSignalsRequest {
                repo_path: String::new(),
                state_id: "not-a-change-id".into(),
                prior_state_id: Vec::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn signal_health_groups_by_module_id() {
        let (_t, repo) = fresh_repo();
        let novelty = sample_signal(RiskSignalKind::Novelty, "novel");
        snapshot_with_signals(&repo, vec![novelty]);
        let svc = local_service(repo);
        let resp = svc
            .get_repo_signal_health(Request::new(GetRepoSignalHealthRequest {
                repo_path: String::new(),
                window_states: 50,
            }))
            .await
            .unwrap();
        let report = resp.into_inner();
        assert!(report.window_states >= 1);
        let entry = report
            .entries
            .iter()
            .find(|e| e.module_id == "novelty.tree_sitter")
            .expect("novelty entry present");
        assert!(entry.fire_rate > 0.0 && entry.fire_rate <= 1.0);
    }
}
