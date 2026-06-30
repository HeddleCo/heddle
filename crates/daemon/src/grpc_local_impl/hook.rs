// SPDX-License-Identifier: Apache-2.0
//! Local-mode `HookService`. Manages hook registrations on disk and
//! exposes the event-schema catalog so hook authors can scaffold
//! payloads. Live event delivery (subscribe + respond) lands when the
//! capture/merge code paths emit events.

use std::{
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use grpc::heddle::v1::{
    DeleteResponse, DeregisterHookRequest, GetHookEventSchemaRequest, GetHookEventSchemaResponse,
    Hook as ProtoHook, HookEvent as ProtoHookEvent, HookEventSchema, ListHooksRequest,
    ListHooksResponse, RegisterHookRequest, RespondToHookRequest, RespondToHookResponse,
    SubscribeHookEventsRequest, hook_service_server::HookService,
};
use objects::{error::HeddleError, fs_atomic::write_file_atomic};
use prost::Message;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::{GrpcLocalService, HookResponse, to_status, with_idempotency};

#[derive(Clone)]
pub struct LocalHookService {
    inner: GrpcLocalService,
}

impl LocalHookService {
    pub fn new(inner: GrpcLocalService) -> Self {
        Self { inner }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HookRegistry {
    #[serde(default)]
    hooks: Vec<HookConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HookConfig {
    name: String,
    command: String,
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    timeout_ms: u32,
}

impl HookConfig {
    fn to_proto(&self) -> ProtoHook {
        ProtoHook {
            name: self.name.clone(),
            command: self.command.clone(),
            events: self.events.clone(),
            timeout_ms: self.timeout_ms,
        }
    }
}

fn registry_path(heddle_dir: &std::path::Path) -> PathBuf {
    heddle_dir.join("hooks").join("registry.toml")
}

fn load_registry(heddle_dir: &std::path::Path) -> Result<HookRegistry, Status> {
    let path = registry_path(heddle_dir);
    if !path.exists() {
        return Ok(HookRegistry::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| to_status(HeddleError::from(e)))?;
    toml::from_str(&raw).map_err(|e| {
        Status::internal(format!(
            "hook registry at {} is malformed: {e}",
            path.display()
        ))
    })
}

fn save_registry(heddle_dir: &std::path::Path, registry: &HookRegistry) -> Result<(), Status> {
    let path = registry_path(heddle_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| to_status(HeddleError::from(e)))?;
    }
    let raw = toml::to_string_pretty(registry)
        .map_err(|e| Status::internal(format!("failed to encode hook registry: {e}")))?;
    write_file_atomic(&path, raw.as_bytes()).map_err(|e| to_status(HeddleError::from(e)))
}

/// The hook event catalog. Each entry documents what the payload and
/// response look like in JSON Schema. Exposing the catalog from
/// `GetHookEventSchema` lets hook authors scaffold against the contract
/// even before live event delivery is wired up.
fn event_catalog() -> Vec<HookEventSchema> {
    let v1 = 1;
    vec![
        HookEventSchema {
            event_name: "pre_capture".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"thread":{"type":"string"},"intent":{"type":"string"}},"required":[]}"#.to_string(),
            response_schema_json: r#"{"type":"object","properties":{"extra_signals":{"type":"array"},"abort":{"type":"string"}}}"#.to_string(),
        },
        HookEventSchema {
            event_name: "post_capture".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"state_id":{"type":"string"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object"}"#.to_string(),
        },
        HookEventSchema {
            event_name: "pre_merge".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"source":{"type":"string"},"target":{"type":"string"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object","properties":{"abort":{"type":"string"}}}"#.to_string(),
        },
        HookEventSchema {
            event_name: "post_merge".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"state_id":{"type":"string"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object"}"#.to_string(),
        },
        HookEventSchema {
            event_name: "on_conflict".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"conflicts":{"type":"array"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object","properties":{"veto":{"type":"object","properties":{"reason":{"type":"string"},"discussion_id":{"type":"string"}}}}}"#.to_string(),
        },
        HookEventSchema {
            event_name: "pre_thread_create".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"name":{"type":"string"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object","properties":{"abort":{"type":"string"}}}"#.to_string(),
        },
        HookEventSchema {
            event_name: "post_thread_create".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"name":{"type":"string"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object"}"#.to_string(),
        },
        HookEventSchema {
            event_name: "pre_push".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"remote":{"type":"string"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object","properties":{"abort":{"type":"string"}}}"#.to_string(),
        },
        HookEventSchema {
            event_name: "post_push".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"remote":{"type":"string"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object"}"#.to_string(),
        },
        HookEventSchema {
            event_name: "on_signal".to_string(),
            schema_version: v1,
            payload_schema_json: r#"{"type":"object","properties":{"state_id":{"type":"string"},"signal_kind":{"type":"string"}}}"#.to_string(),
            response_schema_json: r#"{"type":"object"}"#.to_string(),
        },
    ]
}

/// Concrete stream type for `SubscribeHookEvents`.
pub struct SubscribeHookEventsStream {
    receiver: ReceiverStream<ProtoHookEvent>,
    filter: std::collections::HashSet<String>,
}

impl SubscribeHookEventsStream {
    fn new(
        receiver: tokio::sync::mpsc::Receiver<ProtoHookEvent>,
        filter: std::collections::HashSet<String>,
    ) -> Self {
        Self {
            receiver: ReceiverStream::new(receiver),
            filter,
        }
    }
}

impl Stream for SubscribeHookEventsStream {
    type Item = Result<ProtoHookEvent, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match Pin::new(&mut this.receiver).poll_next(cx) {
                Poll::Ready(Some(event))
                    if this.filter.is_empty() || this.filter.contains(&event.event_name) =>
                {
                    return Poll::Ready(Some(Ok(event)));
                }
                Poll::Ready(Some(_)) => continue,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[tonic::async_trait]
impl HookService for LocalHookService {
    type SubscribeHookEventsStream = SubscribeHookEventsStream;

    async fn register_hook(
        &self,
        request: Request<RegisterHookRequest>,
    ) -> Result<Response<ProtoHook>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let heddle_dir = self.inner.repo().heddle_dir().to_path_buf();
        let client_op = req.client_operation_id.clone();

        let result = with_idempotency(
            &self.inner,
            &client_op,
            "hook.register_hook",
            &body,
            || async move {
                if req.name.trim().is_empty() {
                    return Err(Status::invalid_argument("hook name must not be empty"));
                }
                if req.command.trim().is_empty() {
                    return Err(Status::invalid_argument("hook command must not be empty"));
                }
                let catalog: std::collections::HashSet<String> =
                    event_catalog().into_iter().map(|s| s.event_name).collect();
                for event in &req.events {
                    if !catalog.contains(event) {
                        return Err(Status::invalid_argument(format!(
                            "unknown hook event '{event}' — see GetHookEventSchema for the catalog"
                        )));
                    }
                }
                let mut registry = load_registry(&heddle_dir)?;
                registry.hooks.retain(|h| h.name != req.name);
                let cfg = HookConfig {
                    name: req.name.clone(),
                    command: req.command.clone(),
                    events: req.events.clone(),
                    timeout_ms: req.timeout_ms,
                };
                registry.hooks.push(cfg.clone());
                save_registry(&heddle_dir, &registry)?;
                Ok(cfg.to_proto())
            },
        )
        .await?;
        Ok(Response::new(result))
    }

    async fn deregister_hook(
        &self,
        request: Request<DeregisterHookRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let heddle_dir = self.inner.repo().heddle_dir().to_path_buf();
        let client_op = req.client_operation_id.clone();
        let result = with_idempotency(
            &self.inner,
            &client_op,
            "hook.deregister_hook",
            &body,
            || async move {
                let mut registry = load_registry(&heddle_dir)?;
                let before = registry.hooks.len();
                registry.hooks.retain(|h| h.name != req.name);
                let deleted = registry.hooks.len() < before;
                if deleted {
                    save_registry(&heddle_dir, &registry)?;
                }
                Ok(DeleteResponse { deleted })
            },
        )
        .await?;
        Ok(Response::new(result))
    }

    async fn list_hooks(
        &self,
        _request: Request<ListHooksRequest>,
    ) -> Result<Response<ListHooksResponse>, Status> {
        let registry = load_registry(self.inner.repo().heddle_dir())?;
        let hooks = registry.hooks.iter().map(HookConfig::to_proto).collect();
        Ok(Response::new(ListHooksResponse { hooks }))
    }

    async fn get_hook_event_schema(
        &self,
        request: Request<GetHookEventSchemaRequest>,
    ) -> Result<Response<GetHookEventSchemaResponse>, Status> {
        let req = request.into_inner();
        let mut catalog = event_catalog();
        if !req.event_name.is_empty() {
            catalog.retain(|s| s.event_name == req.event_name);
            if catalog.is_empty() {
                return Err(Status::not_found(format!(
                    "unknown hook event '{}'",
                    req.event_name
                )));
            }
        }
        Ok(Response::new(GetHookEventSchemaResponse {
            schemas: catalog,
        }))
    }

    async fn subscribe_hook_events(
        &self,
        request: Request<SubscribeHookEventsRequest>,
    ) -> Result<Response<Self::SubscribeHookEventsStream>, Status> {
        let req = request.into_inner();
        // Optional event-name filter. Empty = subscribe to every
        // event in the catalog. Validate up front so a typo is a
        // synchronous `InvalidArgument` rather than a silently-empty
        // stream.
        let catalog: std::collections::HashSet<String> =
            event_catalog().into_iter().map(|s| s.event_name).collect();
        for event in &req.events {
            if !catalog.contains(event) {
                return Err(Status::invalid_argument(format!(
                    "unknown hook event '{event}' — see GetHookEventSchema for the catalog"
                )));
            }
        }
        let filter: std::collections::HashSet<String> = req.events.into_iter().collect();
        let receiver = self.inner.hook_events.subscribe();
        // Adapt the broker's `mpsc::Receiver<ProtoHookEvent>` into a
        // `tonic::Stream<Result<ProtoHookEvent, Status>>`. Apply the
        // event-name filter on the read side so subscribers don't pay
        // for events they don't care about.
        Ok(Response::new(SubscribeHookEventsStream::new(
            receiver, filter,
        )))
    }

    async fn respond_to_hook(
        &self,
        request: Request<RespondToHookRequest>,
    ) -> Result<Response<RespondToHookResponse>, Status> {
        let req = request.into_inner();
        let body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let broker = self.inner.hook_events.clone();
        let result = with_idempotency(
            &self.inner,
            &client_op,
            "hook.respond_to_hook",
            &body,
            move || async move {
                if req.hook_event_id.trim().is_empty() {
                    return Err(Status::invalid_argument("hook_event_id must not be empty"));
                }
                // Decode `extra_signals_json` lazily — empty string =
                // no extra. Anything else must parse as JSON; a
                // malformed payload surfaces as `InvalidArgument`
                // rather than getting silently dropped on the
                // emit-side.
                let extra = if req.extra_signals_json.trim().is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_str::<serde_json::Value>(&req.extra_signals_json).map_err(
                        |err| {
                            Status::invalid_argument(format!(
                                "extra_signals_json is not valid JSON: {err}"
                            ))
                        },
                    )?
                };
                let response = HookResponse {
                    abort: req.abort,
                    extra,
                };
                let accepted = broker.deliver_response(&req.hook_event_id, response);
                Ok(RespondToHookResponse { accepted })
            },
        )
        .await?;
        Ok(Response::new(result))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use repo::Repository;
    use tempfile::TempDir;

    use super::*;

    fn fresh_service() -> (TempDir, LocalHookService) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let dedup =
            Arc::new(repo::operation_dedup::OperationDedupStore::open(repo.heddle_dir()).unwrap());
        let inner = GrpcLocalService::new(Arc::new(repo), dedup);
        let svc = LocalHookService::new(inner);
        (temp, svc)
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn register_then_list_returns_hook() {
        let (_t, svc) = fresh_service();
        svc.register_hook(Request::new(RegisterHookRequest {
            repo_path: String::new(),
            name: "log-capture".into(),
            command: "/usr/local/bin/heddle-log".into(),
            events: vec!["post_capture".into()],
            timeout_ms: 5000,
            client_operation_id: String::new(),
        }))
        .await
        .unwrap();
        let resp = svc
            .list_hooks(Request::new(ListHooksRequest {
                repo_path: String::new(),
            }))
            .await
            .unwrap();
        let hooks = resp.into_inner().hooks;
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].name, "log-capture");
        assert_eq!(hooks[0].events, vec!["post_capture".to_string()]);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn register_unknown_event_is_invalid_argument() {
        let (_t, svc) = fresh_service();
        let err = svc
            .register_hook(Request::new(RegisterHookRequest {
                repo_path: String::new(),
                name: "x".into(),
                command: "true".into(),
                events: vec!["definitely_not_an_event".into()],
                timeout_ms: 0,
                client_operation_id: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn deregister_removes_hook() {
        let (_t, svc) = fresh_service();
        svc.register_hook(Request::new(RegisterHookRequest {
            repo_path: String::new(),
            name: "x".into(),
            command: "true".into(),
            events: vec!["pre_capture".into()],
            timeout_ms: 0,
            client_operation_id: String::new(),
        }))
        .await
        .unwrap();
        let resp = svc
            .deregister_hook(Request::new(DeregisterHookRequest {
                repo_path: String::new(),
                name: "x".into(),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap();
        assert!(resp.into_inner().deleted);
        let listed = svc
            .list_hooks(Request::new(ListHooksRequest {
                repo_path: String::new(),
            }))
            .await
            .unwrap();
        assert!(listed.into_inner().hooks.is_empty());
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn get_hook_event_schema_returns_full_catalog() {
        let (_t, svc) = fresh_service();
        let resp = svc
            .get_hook_event_schema(Request::new(GetHookEventSchemaRequest {
                event_name: String::new(),
            }))
            .await
            .unwrap();
        let catalog = resp.into_inner().schemas;
        assert!(catalog.iter().any(|s| s.event_name == "pre_capture"));
        assert!(catalog.iter().any(|s| s.event_name == "on_conflict"));
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn get_hook_event_schema_unknown_returns_not_found() {
        let (_t, svc) = fresh_service();
        let err = svc
            .get_hook_event_schema(Request::new(GetHookEventSchemaRequest {
                event_name: "pretend".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn subscribe_then_emit_round_trips() {
        let (_t, svc) = fresh_service();
        let stream = svc
            .subscribe_hook_events(Request::new(SubscribeHookEventsRequest {
                repo_path: String::new(),
                events: vec![],
            }))
            .await
            .unwrap()
            .into_inner();
        tokio::pin!(stream);
        // Yield so the subscriber's forwarding task is wired up
        // before the broker emit fires.
        tokio::task::yield_now().await;
        let id = svc.inner.hook_events.emit("post_capture", "{}");
        let event = futures::StreamExt::next(&mut stream)
            .await
            .expect("event")
            .expect("ok");
        assert_eq!(event.hook_event_id, id);
        assert_eq!(event.event_name, "post_capture");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn subscribe_filters_non_matching_events() {
        let (_t, svc) = fresh_service();
        let stream = svc
            .subscribe_hook_events(Request::new(SubscribeHookEventsRequest {
                repo_path: String::new(),
                events: vec!["post_capture".into()],
            }))
            .await
            .unwrap()
            .into_inner();
        tokio::pin!(stream);
        tokio::task::yield_now().await;
        svc.inner.hook_events.emit("pre_capture", "{}");
        let id = svc.inner.hook_events.emit("post_capture", "{}");

        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            futures::StreamExt::next(&mut stream),
        )
        .await
        .expect("filtered event")
        .expect("event")
        .expect("ok");
        assert_eq!(event.hook_event_id, id);
        assert_eq!(event.event_name, "post_capture");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn subscribe_unknown_event_is_invalid_argument() {
        let (_t, svc) = fresh_service();
        let result = svc
            .subscribe_hook_events(Request::new(SubscribeHookEventsRequest {
                repo_path: String::new(),
                events: vec!["definitely_not_an_event".into()],
            }))
            .await;
        // `Response<Stream>` doesn't implement Debug so we can't
        // `unwrap_err` here. Match on the result instead.
        match result {
            Err(status) => assert_eq!(status.code(), tonic::Code::InvalidArgument),
            Ok(_) => panic!("expected InvalidArgument, got Ok"),
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn respond_to_hook_delivers_to_emit_waiter() {
        use std::time::Duration;
        let (_t, svc) = fresh_service();
        let _stream = svc
            .subscribe_hook_events(Request::new(SubscribeHookEventsRequest {
                repo_path: String::new(),
                events: vec![],
            }))
            .await
            .unwrap()
            .into_inner();
        tokio::task::yield_now().await;
        let (id, waiter) =
            svc.inner
                .hook_events
                .emit_and_wait("pre_capture", "{}", Duration::from_secs(1));
        let resp = svc
            .respond_to_hook(Request::new(RespondToHookRequest {
                repo_path: String::new(),
                hook_event_id: id,
                abort: "veto".into(),
                extra_signals_json: String::new(),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap();
        assert!(resp.into_inner().accepted);
        let response = waiter.wait().await.expect("response");
        assert_eq!(response.abort, "veto");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn respond_to_hook_rejects_empty_id() {
        let (_t, svc) = fresh_service();
        let err = svc
            .respond_to_hook(Request::new(RespondToHookRequest {
                repo_path: String::new(),
                hook_event_id: String::new(),
                abort: String::new(),
                extra_signals_json: String::new(),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn respond_to_hook_unknown_id_returns_not_accepted() {
        let (_t, svc) = fresh_service();
        let resp = svc
            .respond_to_hook(Request::new(RespondToHookRequest {
                repo_path: String::new(),
                hook_event_id: "made-up".into(),
                abort: String::new(),
                extra_signals_json: String::new(),
                client_operation_id: String::new(),
            }))
            .await
            .unwrap();
        assert!(!resp.into_inner().accepted);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn register_idempotent_returns_same_hook() {
        let (_t, svc) = fresh_service();
        let op_id = objects::object::OperationId::new().to_string();
        let req = RegisterHookRequest {
            repo_path: String::new(),
            name: "foo".into(),
            command: "true".into(),
            events: vec!["pre_capture".into()],
            timeout_ms: 1000,
            client_operation_id: op_id.clone(),
        };
        let first = svc
            .register_hook(Request::new(req.clone()))
            .await
            .unwrap()
            .into_inner();
        let second = svc
            .register_hook(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(first, second);
        let listed = svc
            .list_hooks(Request::new(ListHooksRequest {
                repo_path: String::new(),
            }))
            .await
            .unwrap();
        assert_eq!(listed.into_inner().hooks.len(), 1);
    }
}
