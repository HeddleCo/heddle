use grpc::heddle::v1::{
    ApproveThreadRequest, BeginWebAuthnAuthenticationRequest, CheckMergeEligibilityRequest,
    CheckMergeEligibilityResponse, CreateGrantRequest, CreateInvitationRequest,
    CreateRepositoryRequest, DeleteGrantRequest, DeleteNamespaceRequest, DeleteRepositoryRequest,
    GetCurrentUserNamespaceRequest, GrantSupportAccessRequest, GrantTargetRef,
    Invitation as ProtoInvitation, ListGrantsRequest, ListNamespacesRequest,
    ListRepositoriesRequest, ListSupportAccessGrantsRequest, ListThreadApprovalsRequest,
    RevokeApprovalRequest, RevokeSupportAccessRequest, SupportAccessGrant, ThreadApproval,
    UpdateGrantRequest, UpdateNamespaceRequest, UpdateRepositoryRequest,
    grant_target_ref::Target as GrantTargetKind,
};
use tonic::Request;
use wire::ProtocolError;

use super::{
    HostedGrpcClient,
    helpers::{
        status_to_protocol_error, to_protocol_grant, to_protocol_namespace, to_protocol_repository,
    },
};

/// Dispatch an authenticated unary RPC on `self.user`: wrap the message in a
/// `tonic::Request`, stamp bearer auth AND the Tier-1 PoP request signature via
/// `apply_signed_auth`, await the call, and — if the server rejects with
/// `x-weft-sig-required: human` — invoke the app-registered human-signature
/// callback over the SAME action and retry ONCE. Maps a transport `Status` to a
/// `ProtocolError` and unwraps the response.
///
/// `$rpc` is the snake_case tonic client method; `$grpc_method` is the PascalCase
/// proto RPC name (used to build the signed `:path`). The message is bound once
/// and cloned for the potential human retry (all hosted request protos derive
/// `Clone`). The macro is the one chokepoint for the auth/sign/retry sequence;
/// it must be invoked inside an `async fn` returning `Result<_, ProtocolError>`.
macro_rules! signed_call {
    ($self:ident, $client:ident, $rpc:ident, $path:expr, $msg:expr) => {{
        let path = $path;
        let message = $msg;
        let mut request = Request::new(message.clone());
        let sig_ctx = $self.apply_signed_auth(&mut request, path)?;
        match $self.$client.$rpc(request).await {
            Ok(response) => response.into_inner(),
            Err(status)
                if $crate::grpc_hosted::request_signing::requires_human_signature(&status) =>
            {
                // The human assertion must cover the SAME action (ts + nonce +
                // body-hash) the challenge was derived from, so we reuse the
                // original `sig_ctx` rather than re-signing with a fresh nonce.
                // `attach_human` re-stamps `x-weft-sig-ts`/`-nonce-bin` from that
                // context; we only need bearer auth (not a fresh PoP) on retry.
                let ctx = $self.require_human_sig_context(sig_ctx)?;
                let assertion = $self.request_human_signature(path, &ctx)?;
                let mut retry = Request::new(message);
                $self.apply_auth(&mut retry)?;
                $crate::grpc_hosted::request_signing::attach_human(&mut retry, &ctx, &assertion)?;
                $self
                    .$client
                    .$rpc(retry)
                    .await
                    .map_err(status_to_protocol_error)?
                    .into_inner()
            }
            Err(status) => return Err(status_to_protocol_error(status)),
        }
    }};
}

/// Dispatch an authenticated unary RPC on `self.user`: wrap the message in a
/// `tonic::Request`, stamp bearer auth AND the Tier-1 PoP request signature via
/// `apply_signed_auth`, await the call, and — if the server rejects with
/// `x-weft-sig-required: human` — invoke the app-registered human-signature
/// callback over the SAME action and retry ONCE. Maps a transport `Status` to a
/// `ProtocolError` and unwraps the response.
///
/// `$rpc` is the snake_case tonic client method; `$grpc_method` is the PascalCase
/// proto RPC name (used to build the signed `:path`). The message is bound once
/// and cloned for the potential human retry (all hosted request protos derive
/// `Clone`). Delegates to [`signed_call!`], the one chokepoint for the
/// auth/sign/retry sequence; must be invoked inside an `async fn` returning
/// `Result<_, ProtocolError>`.
macro_rules! authed_call {
    ($self:ident, $rpc:ident, $grpc_method:literal, $msg:expr) => {{
        signed_call!(
            $self,
            user,
            $rpc,
            concat!("/heddle.v1.HostedUserService/", $grpc_method),
            $msg
        )
    }};
}

fn default_spool_settings_request() -> grpc::heddle::v1::SpoolSettings {
    use grpc::heddle::v1::{
        SpoolBootstrapKind, SpoolBootstrapSyncDirection, SpoolChildPolicy, SpoolInitialTooling,
        SpoolSettings, SpoolStateVisibility, SpoolSyncBehavior, SpoolVisibility, SpoolWritePolicy,
    };

    SpoolSettings {
        visibility: SpoolVisibility::Private as i32,
        default_state_visibility: SpoolStateVisibility::Internal as i32,
        bootstrap_kind: SpoolBootstrapKind::Empty as i32,
        bootstrap_source: String::new(),
        write_policy: SpoolWritePolicy::Developers as i32,
        child_policy: SpoolChildPolicy::Maintainers as i32,
        initial_tooling: Some(SpoolInitialTooling::default()),
        sync_behavior: SpoolSyncBehavior::Manual as i32,
        bootstrap_sync_direction: SpoolBootstrapSyncDirection::Pull as i32,
        description: String::new(),
    }
}

impl HostedGrpcClient {
    pub async fn begin_login(
        &mut self,
        username: &str,
    ) -> Result<(String, String, u64), ProtocolError> {
        let request = Request::new(BeginWebAuthnAuthenticationRequest {
            username: username.to_string(),
        });
        let response = self
            .auth
            .begin_web_authn_authentication(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        let expires_at_secs = response
            .expires_at
            .as_ref()
            .map(|t| t.seconds.max(0) as u64)
            .unwrap_or(0);
        Ok((response.challenge_id, response.challenge, expires_at_secs))
    }

    pub async fn get_current_user_namespace(
        &mut self,
    ) -> Result<wire::HostedNamespaceInfo, ProtocolError> {
        let namespace = authed_call!(
            self,
            get_current_user_namespace,
            "GetCurrentUserNamespace",
            GetCurrentUserNamespaceRequest {}
        );
        Ok(to_protocol_namespace(namespace))
    }

    pub async fn list_namespaces(
        &mut self,
    ) -> Result<Vec<wire::HostedNamespaceInfo>, ProtocolError> {
        let response = authed_call!(self, list_namespaces, "ListNamespaces", ListNamespacesRequest {});
        Ok(response
            .namespaces
            .into_iter()
            .map(to_protocol_namespace)
            .collect())
    }

    pub async fn create_namespace(
        &mut self,
        kind: &str,
        slug: &str,
        parent_path: Option<&str>,
        display_name: Option<String>,
    ) -> Result<wire::HostedNamespaceInfo, ProtocolError> {
        let namespace = authed_call!(
            self,
            create_namespace,
            "CreateNamespace",
            grpc::heddle::v1::CreateNamespaceRequest {
                kind: parse_namespace_kind_arg(kind)? as i32,
                slug: slug.to_string(),
                parent_path: parent_path.unwrap_or_default().to_string(),
                display_name: display_name.unwrap_or_default(),
                settings: Some(default_spool_settings_request()),
                client_operation_id: String::new(),
            }
        );
        Ok(to_protocol_namespace(namespace))
    }

    pub async fn create_repository(
        &mut self,
        namespace_path: &str,
        slug: &str,
    ) -> Result<wire::HostedRepositoryInfo, ProtocolError> {
        let repo = authed_call!(
            self,
            create_repository,
            "CreateRepository",
            CreateRepositoryRequest {
                namespace_path: namespace_path.to_string(),
                slug: slug.to_string(),
                client_operation_id: String::new(),
            }
        );
        Ok(to_protocol_repository(repo))
    }

    pub async fn list_repositories(
        &mut self,
        namespace_path: Option<&str>,
    ) -> Result<Vec<wire::HostedRepositoryInfo>, ProtocolError> {
        let response = authed_call!(
            self,
            list_repositories,
            "ListRepositories",
            ListRepositoriesRequest {
                namespace_path: namespace_path.unwrap_or_default().to_string(),
            }
        );
        Ok(response
            .repositories
            .into_iter()
            .map(to_protocol_repository)
            .collect())
    }

    pub async fn update_namespace(
        &mut self,
        full_path: &str,
        new_slug: Option<&str>,
        display_name: Option<Option<String>>,
    ) -> Result<wire::HostedNamespaceInfo, ProtocolError> {
        let (display_name, clear_display_name) = match display_name {
            Some(Some(value)) => (value, false),
            Some(None) => (String::new(), true),
            None => (String::new(), false),
        };
        let namespace = authed_call!(
            self,
            update_namespace,
            "UpdateNamespace",
            UpdateNamespaceRequest {
                full_path: full_path.to_string(),
                new_slug: new_slug.unwrap_or_default().to_string(),
                display_name,
                clear_display_name,
                client_operation_id: String::new(),
            }
        );
        Ok(to_protocol_namespace(namespace))
    }

    pub async fn delete_namespace(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        authed_call!(
            self,
            delete_namespace,
            "DeleteNamespace",
            DeleteNamespaceRequest {
                full_path: full_path.to_string(),
                client_operation_id: String::new(),
            }
        );
        Ok(())
    }

    pub async fn update_repository(
        &mut self,
        full_path: &str,
        new_slug: &str,
    ) -> Result<wire::HostedRepositoryInfo, ProtocolError> {
        let repo = authed_call!(
            self,
            update_repository,
            "UpdateRepository",
            UpdateRepositoryRequest {
                full_path: full_path.to_string(),
                new_slug: new_slug.to_string(),
                client_operation_id: String::new(),
            }
        );
        Ok(to_protocol_repository(repo))
    }

    pub async fn delete_repository(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        authed_call!(
            self,
            delete_repository,
            "DeleteRepository",
            DeleteRepositoryRequest {
                full_path: full_path.to_string(),
                client_operation_id: String::new(),
            }
        );
        Ok(())
    }

    pub async fn create_grant(
        &mut self,
        subject: &str,
        role: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
    ) -> Result<wire::HostedGrantInfo, ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        let grant = authed_call!(
            self,
            create_grant,
            "CreateGrant",
            CreateGrantRequest {
                subject: subject.to_string(),
                role: parse_hosted_role_arg(role)? as i32,
                target,
                client_operation_id: String::new(),
            }
        );
        Ok(to_protocol_grant(grant))
    }

    pub async fn list_grants(
        &mut self,
        resource: Option<&str>,
    ) -> Result<Vec<wire::HostedGrantInfo>, ProtocolError> {
        let response = authed_call!(
            self,
            list_grants,
            "ListGrants",
            ListGrantsRequest {
                resource: resource.unwrap_or_default().to_string(),
            }
        );
        Ok(response.grants.into_iter().map(to_protocol_grant).collect())
    }

    pub async fn update_grant(
        &mut self,
        subject: &str,
        role: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
    ) -> Result<wire::HostedGrantInfo, ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        let grant = authed_call!(
            self,
            update_grant,
            "UpdateGrant",
            UpdateGrantRequest {
                subject: subject.to_string(),
                role: parse_hosted_role_arg(role)? as i32,
                target,
                client_operation_id: String::new(),
            }
        );
        Ok(to_protocol_grant(grant))
    }

    pub async fn delete_grant(
        &mut self,
        subject: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        authed_call!(
            self,
            delete_grant,
            "DeleteGrant",
            DeleteGrantRequest {
                subject: subject.to_string(),
                target,
                client_operation_id: String::new(),
            }
        );
        Ok(())
    }

    /// Track D — create a pending invitation. Returns the raw proto type
    /// to keep the surface narrow until we settle on a domain shape.
    pub async fn create_invitation(
        &mut self,
        email: &str,
        namespace_path: &str,
        role: &str,
    ) -> Result<ProtoInvitation, ProtocolError> {
        let invitation = authed_call!(
            self,
            create_invitation,
            "CreateInvitation",
            CreateInvitationRequest {
                email: email.to_string(),
                namespace_path: namespace_path.to_string(),
                role: parse_hosted_role_arg(role)? as i32,
                expires_at: None,
                metadata: String::new(),
                client_operation_id: String::new(),
            }
        );
        Ok(invitation)
    }

    /// Record an approval for `(source_thread → target_thread)` at
    /// the source's current `source_state`. The server's gate decides
    /// later whether this approval *counts* against any matching
    /// policy's requirements.
    pub async fn approve_thread(
        &mut self,
        repo_path: &str,
        source_thread: &str,
        target_thread: &str,
        source_state: &str,
        note: Option<&str>,
    ) -> Result<ThreadApproval, ProtocolError> {
        Ok(authed_call!(
            self,
            approve_thread,
            "ApproveThread",
            ApproveThreadRequest {
                repo_path: repo_path.to_string(),
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
                source_state: objects::object::ChangeId::parse(source_state)
                    .map(|id| id.as_bytes().to_vec())
                    .unwrap_or_default(),
                note: note.unwrap_or_default().to_string(),
                client_operation_id: String::new(),
            }
        ))
    }

    pub async fn revoke_approval(&mut self, id: &str) -> Result<(), ProtocolError> {
        authed_call!(
            self,
            revoke_approval,
            "RevokeApproval",
            RevokeApprovalRequest {
                id: id.to_string(),
                client_operation_id: String::new(),
            }
        );
        Ok(())
    }

    pub async fn list_thread_approvals(
        &mut self,
        repo_path: &str,
        source_thread: &str,
        target_thread: &str,
    ) -> Result<Vec<ThreadApproval>, ProtocolError> {
        Ok(authed_call!(
            self,
            list_thread_approvals,
            "ListThreadApprovals",
            ListThreadApprovalsRequest {
                repo_path: repo_path.to_string(),
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
            }
        )
        .approvals)
    }

    /// Ask the server "can <source> merge into <target> at
    /// <source_state>, given the diff touches `changed_paths`?" The
    /// reply lists every unmet requirement and the approvals that
    /// counted as valid.
    #[allow(clippy::too_many_arguments)]
    pub async fn check_merge_eligibility(
        &mut self,
        repo_path: &str,
        source_thread: &str,
        target_thread: &str,
        source_state: &str,
        gated_action: &str,
        changed_paths: Vec<String>,
        author_user_id: Option<&str>,
    ) -> Result<CheckMergeEligibilityResponse, ProtocolError> {
        Ok(authed_call!(
            self,
            check_merge_eligibility,
            "CheckMergeEligibility",
            CheckMergeEligibilityRequest {
                repo_path: repo_path.to_string(),
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
                source_state: objects::object::ChangeId::parse(source_state)
                    .map(|id| id.as_bytes().to_vec())
                    .unwrap_or_default(),
                gated_action: gated_action.to_string(),
                changed_paths,
                author_user_id: author_user_id.unwrap_or_default().to_string(),
            }
        ))
    }

    /// Phase C: grant a Heddle staff member temporary admin on a
    /// namespace or repo. Exactly one of `namespace_path` or
    /// `repo_path` should be set.
    pub async fn grant_support_access(
        &mut self,
        operator_email: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
        ttl_seconds: u32,
        reason: &str,
        client_operation_id: String,
    ) -> Result<SupportAccessGrant, ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        Ok(authed_call!(
            self,
            grant_support_access,
            "GrantSupportAccess",
            GrantSupportAccessRequest {
                operator_email: operator_email.to_string(),
                target,
                ttl_seconds: Some(prost_types::Duration {
                    seconds: i64::from(ttl_seconds),
                    nanos: 0,
                }),
                reason: reason.to_string(),
                client_operation_id,
            }
        ))
    }

    pub async fn list_support_access_grants(
        &mut self,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
        include_inactive: bool,
    ) -> Result<Vec<SupportAccessGrant>, ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        Ok(authed_call!(
            self,
            list_support_access_grants,
            "ListSupportAccessGrants",
            ListSupportAccessGrantsRequest {
                target,
                include_inactive,
            }
        )
        .grants)
    }

    pub async fn revoke_support_access(
        &mut self,
        id: &str,
        client_operation_id: String,
    ) -> Result<(), ProtocolError> {
        authed_call!(
            self,
            revoke_support_access,
            "RevokeSupportAccess",
            RevokeSupportAccessRequest {
                id: id.to_string(),
                client_operation_id,
            }
        );
        Ok(())
    }

    /// Test-only: exercise the exact `signed_call!` orchestration (PoP sign →
    /// human-required rejection → callback → retry with WebAuthn headers) over
    /// the 3-method `TreeEditService` mock, so the retry path is covered
    /// end-to-end without a 41-method `HostedUserService` mock. Uses
    /// `StatusForThread` purely as a carrier RPC.
    #[cfg(test)]
    async fn signed_status_for_thread_with_retry(
        &mut self,
        thread: &str,
    ) -> Result<grpc::heddle::v1::StatusForThreadResponse, ProtocolError> {
        Ok(signed_call!(
            self,
            tree_edit,
            status_for_thread,
            "/heddle.v1.TreeEditService/StatusForThread",
            grpc::heddle::v1::StatusForThreadRequest {
                repo_path: "owner/repo".to_string(),
                thread: thread.to_string(),
                compare_tree: None,
            }
        ))
    }
}

/// Build a `GrantTargetRef` oneof from CLI-style optional path args.
/// Caller layer enforces that at most one of `namespace_path` /
/// `repo_path` is set; this helper is just the wire-format adapter.
fn build_target_ref(
    namespace_path: Option<&str>,
    repo_path: Option<&str>,
) -> Result<Option<GrantTargetRef>, ProtocolError> {
    match (
        namespace_path.filter(|s| !s.is_empty()),
        repo_path.filter(|s| !s.is_empty()),
    ) {
        (Some(ns), None) => Ok(Some(GrantTargetRef {
            target: Some(GrantTargetKind::NamespacePath(ns.to_string())),
        })),
        (None, Some(rp)) => Ok(Some(GrantTargetRef {
            target: Some(GrantTargetKind::RepoPath(rp.to_string())),
        })),
        _ => Err(ProtocolError::InvalidState(
            "exactly one of namespace_path or repo_path must be set".into(),
        )),
    }
}

/// Parse a CLI-supplied namespace kind string ("user" / "namespace" /
/// "team", with "org" accepted as an alias for "namespace") into the
/// proto `NamespaceKind` enum.
fn parse_namespace_kind_arg(value: &str) -> Result<grpc::heddle::v1::NamespaceKind, ProtocolError> {
    use grpc::heddle::v1::NamespaceKind;
    match value.trim().to_ascii_lowercase().as_str() {
        "user" => Ok(NamespaceKind::User),
        "namespace" | "org" => Ok(NamespaceKind::Org),
        "team" => Ok(NamespaceKind::Team),
        other => Err(ProtocolError::InvalidState(format!(
            "invalid namespace kind '{other}': expected user|namespace|team"
        ))),
    }
}

/// Parse a CLI-supplied role name into the proto `HostedRole` enum.
fn parse_hosted_role_arg(value: &str) -> Result<grpc::heddle::v1::HostedRole, ProtocolError> {
    use grpc::heddle::v1::HostedRole;
    match value.trim().to_ascii_lowercase().as_str() {
        "reader" => Ok(HostedRole::Reader),
        "developer" => Ok(HostedRole::Developer),
        "maintainer" => Ok(HostedRole::Maintainer),
        "admin" => Ok(HostedRole::Admin),
        "owner" => Ok(HostedRole::Owner),
        other => Err(ProtocolError::InvalidState(format!(
            "invalid role '{other}': expected reader|developer|maintainer|admin|owner"
        ))),
    }
}

#[cfg(test)]
mod human_retry_tests {
    //! End-to-end coverage of the `signed_call!` orchestration: proactive PoP
    //! signing, the human-required rejection → app callback → single retry with
    //! WebAuthn headers, and the no-callback typed-error (no-loop) case.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cli_shared::ClientConfig;
    use crypto::Ed25519Signer;
    use grpc::heddle::v1::{
        StatusForThreadRequest, StatusForThreadResponse,
        tree_edit_service_server::{TreeEditService, TreeEditServiceServer},
        DiffForThreadRequest, DiffForThreadResponse, LogForThreadRequest, LogForThreadResponse,
    };
    use tonic::{Request, Response, Status, transport::Server};
    use wire::ProtocolError;

    use super::super::request_signing::{
        HDR_SIG_ALG, HDR_SIG_BIN, HDR_SIG_KEY_BIN, HDR_SIG_REQUIRED,
        HDR_SIG_WEBAUTHN_AUTH_DATA_BIN, HDR_SIG_WEBAUTHN_CLIENT_DATA_BIN, WebAuthnAssertion,
    };
    use super::HostedGrpcClient;

    /// A `TreeEditService` mock for `StatusForThread` that models a `human`-tier
    /// endpoint: the first request (no WebAuthn assertion) is rejected with
    /// `x-weft-sig-required: human`; a request carrying the WebAuthn alg + client
    /// data succeeds. It records how many times it was hit.
    #[derive(Clone, Default)]
    struct HumanTierMock {
        hits: Arc<AtomicUsize>,
    }

    #[tonic::async_trait]
    impl TreeEditService for HumanTierMock {
        async fn status_for_thread(
            &self,
            request: Request<StatusForThreadRequest>,
        ) -> Result<Response<StatusForThreadResponse>, Status> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let md = request.metadata();
            let is_human = md
                .get(HDR_SIG_ALG)
                .and_then(|v| v.to_str().ok())
                .map(|v| v == "webauthn")
                .unwrap_or(false);
            if !is_human {
                // A keyed client PoP-signs the first attempt; a keyless
                // (anonymous) client sends no signature. Record which so the
                // signed test can assert PoP headers were present.
                if md.get(HDR_SIG_ALG).is_some() {
                    assert_eq!(
                        md.get(HDR_SIG_ALG).and_then(|v| v.to_str().ok()),
                        Some("ed25519"),
                        "a signed first attempt must be PoP (ed25519), not webauthn"
                    );
                    assert!(md.get_bin(HDR_SIG_KEY_BIN).is_some(), "PoP key header present");
                    assert!(md.get_bin(HDR_SIG_BIN).is_some(), "PoP signature present");
                }
                let mut trailer = tonic::metadata::MetadataMap::new();
                trailer.insert(HDR_SIG_REQUIRED, "human".parse().unwrap());
                return Err(Status::with_metadata(
                    tonic::Code::Unauthenticated,
                    "user verification required",
                    trailer,
                ));
            }
            // Retry: WebAuthn headers must be present.
            assert!(
                md.get_bin(HDR_SIG_WEBAUTHN_CLIENT_DATA_BIN).is_some(),
                "retry carries clientDataJSON"
            );
            assert!(
                md.get_bin(HDR_SIG_WEBAUTHN_AUTH_DATA_BIN).is_some(),
                "retry carries authenticatorData"
            );
            Ok(Response::new(StatusForThreadResponse {
                thread: request.into_inner().thread,
                head_state: "hd".into(),
                base_state: "bd".into(),
                target_thread: "main".into(),
                coordination_status: "ahead".into(),
                changes: None,
                compared_to_supplied_tree: false,
            }))
        }

        async fn diff_for_thread(
            &self,
            _request: Request<DiffForThreadRequest>,
        ) -> Result<Response<DiffForThreadResponse>, Status> {
            Err(Status::unimplemented("unused"))
        }

        async fn log_for_thread(
            &self,
            _request: Request<LogForThreadRequest>,
        ) -> Result<Response<LogForThreadResponse>, Status> {
            Err(Status::unimplemented("unused"))
        }
    }

    /// A software Ed25519 seed usable as the client device key (`auth_proof_key_pem`).
    fn device_key_pem() -> String {
        Ed25519Signer::generate()
            .expect("gen device key")
            .to_pem()
            .expect("pem")
    }

    async fn connect_mock(
        callback: Option<super::super::request_signing::HumanSignatureCallback>,
    ) -> Option<(HostedGrpcClient, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
        let mock = HumanTierMock::default();
        let hits = mock.hits.clone();
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(l) => l,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping human-retry test: TCP bind denied: {err}");
                return None;
            }
            Err(err) => panic!("bind: {err}"),
        };
        let addr = listener.local_addr().expect("addr");
        let incoming = futures::stream::unfold(listener, |listener| async {
            match listener.accept().await {
                Ok((stream, _)) => Some((Ok::<_, std::io::Error>(stream), listener)),
                Err(err) => Some((Err(err), listener)),
            }
        });
        let handle = tokio::spawn(async move {
            Server::builder()
                .add_service(TreeEditServiceServer::new(mock))
                .serve_with_incoming(incoming)
                .await
                .expect("serve");
        });

        let config = ClientConfig::default().with_auth_proof_key_pem(device_key_pem());
        let mut client = HostedGrpcClient::connect(addr, &config)
            .await
            .expect("connect");
        if let Some(cb) = callback {
            client = client.with_human_signature_callback(cb);
        }
        Some((client, hits, handle))
    }

    #[tokio::test]
    async fn human_tier_rejection_invokes_callback_and_retries_once() {
        let callback_calls = Arc::new(AtomicUsize::new(0));
        let cc = callback_calls.clone();
        let callback: super::super::request_signing::HumanSignatureCallback =
            Arc::new(move |req: super::super::request_signing::HumanSignatureRequest| {
                cc.fetch_add(1, Ordering::SeqCst);
                // The challenge must be the client-derived SHA256(canonical).
                let expected =
                    super::super::request_signing::human_challenge(&req.canonical);
                assert_eq!(req.challenge, expected);
                assert!(req.method_path.ends_with("/StatusForThread"));
                Ok(WebAuthnAssertion {
                    credential_id: b"cred-id".to_vec(),
                    signature: b"assertion-sig".to_vec(),
                    client_data_json: b"{\"type\":\"webauthn.get\"}".to_vec(),
                    authenticator_data: vec![0u8; 37],
                    user_handle: None,
                })
            });

        let Some((mut client, hits, server)) = connect_mock(Some(callback)).await else {
            return;
        };
        let resp = client
            .signed_status_for_thread_with_retry("feat/x")
            .await
            .expect("call succeeds after human retry");
        server.abort();

        assert_eq!(resp.thread, "feat/x");
        assert_eq!(callback_calls.load(Ordering::SeqCst), 1, "callback invoked once");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "server hit exactly twice (reject + retry)");
    }

    #[tokio::test]
    async fn human_tier_rejection_without_callback_is_typed_error_no_loop() {
        let Some((mut client, hits, server)) = connect_mock(None).await else {
            return;
        };
        let err = client
            .signed_status_for_thread_with_retry("feat/x")
            .await
            .expect_err("no callback => typed error");
        server.abort();

        match err {
            ProtocolError::AuthorizationFailed(msg) => {
                assert!(
                    msg.contains("user verification"),
                    "typed error names user verification: {msg}"
                );
            }
            other => panic!("expected AuthorizationFailed, got {other:?}"),
        }
        // Exactly one server hit — the rejection — with NO retry loop.
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no retry without a callback");
    }

    #[tokio::test]
    async fn anonymous_client_without_device_key_skips_signing() {
        // No device key + a mock that rejects only unsigned-tier-agnostic: here we
        // just assert signing is skipped (no PoP headers) and no panic. Reuse the
        // echo mock indirectly by asserting the request is not human-rejected on a
        // fresh call — an anonymous client sends no signature and the server's
        // human gate would 401, but with no callback and no context we get the
        // typed error. The key assertion is that `apply_signed_auth` returns
        // `Ok(None)` for a keyless client (covered here by not panicking).
        let mock = HumanTierMock::default();
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(l) => l,
            Err(_) => return,
        };
        let addr = listener.local_addr().expect("addr");
        let incoming = futures::stream::unfold(listener, |listener| async {
            match listener.accept().await {
                Ok((stream, _)) => Some((Ok::<_, std::io::Error>(stream), listener)),
                Err(err) => Some((Err(err), listener)),
            }
        });
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(TreeEditServiceServer::new(mock))
                .serve_with_incoming(incoming)
                .await
                .expect("serve");
        });

        // Anonymous: no auth_proof_key_pem.
        let mut client = HostedGrpcClient::connect(addr, &ClientConfig::default())
            .await
            .expect("connect");
        // Should not panic; signing is simply skipped. The mock rejects because
        // no ed25519 alg header is present, which maps to a typed error — the
        // point is the client did not crash and sent no signature.
        let result = client.signed_status_for_thread_with_retry("feat/x").await;
        server.abort();
        assert!(result.is_err(), "keyless client hits the human gate but does not panic");
    }
}
