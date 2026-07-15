use grpc::heddle::v1::{
    ApproveThreadRequest, BeginWebAuthnAuthenticationRequest, CheckMergeEligibilityRequest,
    CheckMergeEligibilityResponse, CreateGrantRequest, CreateInvitationRequest,
    CreateRepositoryRequest, DeleteGrantRequest, DeleteNamespaceRequest, DeleteRepositoryRequest,
    GetCurrentUserNamespaceRequest, GrantSupportAccessRequest, GrantTargetRef,
    Invitation as ProtoInvitation, ListGrantsRequest, ListSpoolsRequest,
    ListSupportAccessGrantsRequest, ListThreadApprovalsRequest, MonorepoNode,
    ResolveMonorepoRequest, RevokeApprovalRequest, RevokeSupportAccessRequest, SpoolSummary,
    SupportAccessGrant, ThreadApproval, UpdateGrantRequest, UpdateNamespaceRequest,
    UpdateRepositoryRequest, grant_target_ref::Target as GrantTargetKind,
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
                // The server may include a deep-link (weft#338) on the rejection pointing at a
                // surface that can complete the WebAuthn ceremony; forward it to the callback.
                let action_url =
                    $crate::grpc_hosted::request_signing::action_url_from_status(&status);
                let assertion = $self.request_human_signature(path, &ctx, action_url)?;
                let mut retry = Request::new(message);
                $self.apply_auth(&mut retry, path)?;
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
        SpoolBootstrapKind, SpoolBootstrapSyncDirection, SpoolChildPolicy, SpoolHoldLifecycle,
        SpoolInitialTooling, SpoolSettings, SpoolStateVisibility, SpoolSyncBehavior,
        SpoolVisibility, SpoolWritePolicy,
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
        // UNSPECIFIED = inherit; effective root default is EXPLICIT_SUPERSESSION.
        hold_lifecycle: SpoolHoldLifecycle::Unspecified as i32,
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

    pub async fn list_spools(
        &mut self,
        repos_only: bool,
    ) -> Result<Vec<SpoolSummary>, ProtocolError> {
        let response = authed_call!(
            self,
            list_spools,
            "ListSpools",
            ListSpoolsRequest { repos_only }
        );
        Ok(response.spools)
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
                source_state: objects::object::StateId::parse(source_state)
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
                source_state: objects::object::StateId::parse(source_state)
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

    /// Recursively resolve the monorepo rooted at `root_path` into the caller's
    /// coherent visible slice (per-child visibility, cycle guard, depth bound).
    /// `max_depth` is an optional recursion bound (server clamps to
    /// `MONOREPO_MAX_DEPTH`). Returns the root `MonorepoNode` — the whole tree
    /// the monorepo-clone planner walks.
    pub async fn resolve_monorepo(
        &mut self,
        root_path: &str,
        max_depth: Option<u32>,
    ) -> Result<MonorepoNode, ProtocolError> {
        Ok(authed_call!(
            self,
            resolve_monorepo,
            "ResolveMonorepo",
            ResolveMonorepoRequest {
                root_path: root_path.to_string(),
                max_depth,
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
mod spool_request_shape_tests {
    use grpc::heddle::v1::ResolveMonorepoRequest;

    #[test]
    fn resolve_monorepo_request_threads_optional_max_depth() {
        let bounded = ResolveMonorepoRequest {
            root_path: "acme/root".to_string(),
            max_depth: Some(3),
        };
        assert_eq!(bounded.root_path, "acme/root");
        assert_eq!(bounded.max_depth, Some(3));

        let unbounded = ResolveMonorepoRequest {
            root_path: "acme/root".to_string(),
            max_depth: None,
        };
        assert_eq!(unbounded.max_depth, None);
    }
}
