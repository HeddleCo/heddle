//! Hosted client wrappers for the read-side tree-edit RPCs (heddle#409).
//!
//! These expose `TreeEditService`'s `StatusForThread`, `DiffForThread`, and
//! `LogForThread` over the hosted gRPC surface so a filesystem-less client can
//! inspect a hosted thread's tree state without a local `.heddle/` or worktree.
//! The contract is committed-tree-by-default; dirty overlays only appear when
//! the caller names a `compare_tree`.

use grpc::heddle::v1::{
    DiffForThreadRequest, DiffForThreadResponse, LogForThreadRequest, LogForThreadResponse,
    StatusForThreadRequest, StatusForThreadResponse, Treeish,
};
use tonic::Request;
use wire::ProtocolError;

use super::{HostedGrpcClient, helpers::status_to_protocol_error};

impl HostedGrpcClient {
    /// Inspect a hosted thread's committed status, optionally comparing against
    /// a caller-supplied overlay tree (`compare_tree`). With no overlay this
    /// reports committed-thread facts only and makes no claim about a client
    /// filesystem.
    pub async fn status_for_thread(
        &mut self,
        repo_path: &str,
        thread: &str,
        compare_tree: Option<Treeish>,
    ) -> Result<StatusForThreadResponse, ProtocolError> {
        let mut request = Request::new(StatusForThreadRequest {
            repo_path: repo_path.to_string(),
            thread: thread.to_string(),
            compare_tree,
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.TreeEditService/StatusForThread")?;
        self.tree_edit
            .status_for_thread(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
    }

    /// Diff a hosted thread between two explicitly-named treeish points. Both
    /// sides are required — a diff never implies an invisible worktree.
    pub async fn diff_for_thread(
        &mut self,
        repo_path: &str,
        thread: &str,
        from: Treeish,
        to: Treeish,
        include_semantic: bool,
    ) -> Result<DiffForThreadResponse, ProtocolError> {
        let mut request = Request::new(DiffForThreadRequest {
            repo_path: repo_path.to_string(),
            thread: thread.to_string(),
            from: Some(from),
            to: Some(to),
            include_semantic,
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.TreeEditService/DiffForThread")?;
        self.tree_edit
            .diff_for_thread(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
    }

    /// Walk a hosted thread's first-parent history, reusing the
    /// `ContentService` `StateSummary` shape. `since_state` is an exclusive
    /// lower bound; `paths` and `agent_model_substring` are optional filters.
    pub async fn log_for_thread(
        &mut self,
        repo_path: &str,
        thread: &str,
        limit: u32,
        since_state: Option<&str>,
        paths: Vec<String>,
        agent_model_substring: Option<&str>,
    ) -> Result<LogForThreadResponse, ProtocolError> {
        let mut request = Request::new(LogForThreadRequest {
            repo_path: repo_path.to_string(),
            thread: thread.to_string(),
            limit,
            since_state: since_state.map(str::to_string),
            paths,
            agent_model_substring: agent_model_substring.map(str::to_string),
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.TreeEditService/LogForThread")?;
        self.tree_edit
            .log_for_thread(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use cli_shared::ClientConfig;
    use grpc::heddle::v1::{
        CompareSummary, DiffForThreadRequest, DiffForThreadResponse, FileDiff, LogForThreadRequest,
        LogForThreadResponse, StateSummary, StatusForThreadRequest, StatusForThreadResponse,
        ThreadPathSet, Treeish,
        tree_edit_service_server::{TreeEditService, TreeEditServiceServer},
        treeish,
    };
    use tonic::{Request, Response, Status, transport::Server};

    use super::HostedGrpcClient;

    /// A mock hosted `TreeEditService` that echoes the request inputs back into
    /// the response shapes. It stands in for the weft-side handler so the
    /// heddle client can be exercised end-to-end over a real gRPC channel.
    #[derive(Default)]
    struct EchoTreeEditService;

    #[tonic::async_trait]
    impl TreeEditService for EchoTreeEditService {
        async fn status_for_thread(
            &self,
            request: Request<StatusForThreadRequest>,
        ) -> Result<Response<StatusForThreadResponse>, Status> {
            let req = request.into_inner();
            let compared_to_supplied_tree = req.compare_tree.is_some();
            Ok(Response::new(StatusForThreadResponse {
                thread: req.thread,
                head_state: "hs-head".into(),
                base_state: "hs-base".into(),
                target_thread: "main".into(),
                coordination_status: "ahead".into(),
                changes: Some(ThreadPathSet {
                    modified: vec!["src/lib.rs".into()],
                    added: vec![],
                    deleted: vec![],
                }),
                compared_to_supplied_tree,
            }))
        }

        async fn diff_for_thread(
            &self,
            request: Request<DiffForThreadRequest>,
        ) -> Result<Response<DiffForThreadResponse>, Status> {
            let req = request.into_inner();
            // Reject a diff with a missing side — the contract requires both.
            if req.from.is_none() || req.to.is_none() {
                return Err(Status::invalid_argument("from and to are both required"));
            }
            Ok(Response::new(DiffForThreadResponse {
                from_state: "hs-from".into(),
                to_state: "hs-to".into(),
                files: vec![FileDiff {
                    path: "src/lib.rs".into(),
                    kind: "modified".into(),
                    hunks: vec![],
                    classification: "Logic".into(),
                    importance: "High".into(),
                }],
                summary: Some(CompareSummary {
                    added: 0,
                    modified: 1,
                    deleted: 0,
                    renamed: 0,
                    total: 1,
                }),
            }))
        }

        async fn log_for_thread(
            &self,
            request: Request<LogForThreadRequest>,
        ) -> Result<Response<LogForThreadResponse>, Status> {
            let req = request.into_inner();
            // Echo the requested limit as the number of returned states so the
            // client test can assert the request was wired through.
            let states = (0..req.limit)
                .map(|_| StateSummary::default())
                .collect::<Vec<_>>();
            Ok(Response::new(LogForThreadResponse { states }))
        }
    }

    async fn connect_echo_service() -> Option<(HostedGrpcClient, tokio::task::JoinHandle<()>)> {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping tree-edit client test: TCP bind denied: {err}");
                return None;
            }
            Err(err) => panic!("bind test server: {err}"),
        };
        let addr = listener.local_addr().expect("local addr");
        let incoming = futures::stream::unfold(listener, |listener| async {
            match listener.accept().await {
                Ok((stream, _addr)) => Some((Ok::<_, std::io::Error>(stream), listener)),
                Err(err) => Some((Err(err), listener)),
            }
        });

        let handle = tokio::spawn(async move {
            Server::builder()
                .add_service(TreeEditServiceServer::new(EchoTreeEditService))
                .serve_with_incoming(incoming)
                .await
                .expect("serve tree-edit test service");
        });

        let client = HostedGrpcClient::connect(addr, &ClientConfig::default())
            .await
            .expect("connect client");
        Some((client, handle))
    }

    #[tokio::test]
    async fn status_for_thread_without_overlay_reports_committed_only() {
        let Some((mut client, server)) = connect_echo_service().await else {
            return;
        };
        let resp = client
            .status_for_thread("owner/repo", "feat/x", None)
            .await
            .expect("status_for_thread");
        server.abort();

        assert_eq!(resp.thread, "feat/x");
        assert_eq!(resp.head_state, "hs-head");
        assert!(
            !resp.compared_to_supplied_tree,
            "no compare_tree supplied = committed-only status"
        );
    }

    #[tokio::test]
    async fn status_for_thread_with_overlay_sets_compared_flag() {
        let Some((mut client, server)) = connect_echo_service().await else {
            return;
        };
        let overlay = Treeish {
            value: Some(treeish::Value::CaptureId("cap-123".into())),
        };
        let resp = client
            .status_for_thread("owner/repo", "feat/x", Some(overlay))
            .await
            .expect("status_for_thread");
        server.abort();

        assert!(
            resp.compared_to_supplied_tree,
            "compare_tree supplied = overlay comparison"
        );
    }

    #[tokio::test]
    async fn diff_for_thread_round_trips_file_diffs() {
        let Some((mut client, server)) = connect_echo_service().await else {
            return;
        };
        let from = Treeish {
            value: Some(treeish::Value::StateId("hs-from".into())),
        };
        let to = Treeish {
            value: Some(treeish::Value::Ref("feat/x".into())),
        };
        let resp = client
            .diff_for_thread("owner/repo", "feat/x", from, to, true)
            .await
            .expect("diff_for_thread");
        server.abort();

        assert_eq!(resp.files.len(), 1);
        assert_eq!(resp.files[0].path, "src/lib.rs");
        assert_eq!(resp.summary.expect("summary").modified, 1);
    }

    #[tokio::test]
    async fn log_for_thread_returns_requested_number_of_states() {
        let Some((mut client, server)) = connect_echo_service().await else {
            return;
        };
        let resp = client
            .log_for_thread("owner/repo", "feat/x", 3, Some("hs-since"), vec![], None)
            .await
            .expect("log_for_thread");
        server.abort();

        assert_eq!(resp.states.len(), 3);
    }
}
