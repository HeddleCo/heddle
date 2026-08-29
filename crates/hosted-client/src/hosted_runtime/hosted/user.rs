use api::heddle::api::v1alpha1::{
    ApproveThreadRequest, BeginWebAuthnAuthenticationRequest, BootstrapOwnerRootRequest,
    BootstrapOwnerRootResponse, CheckMergeEligibilityRequest, CheckMergeEligibilityResponse,
    CreateAgentAccountRequest, CreateAgentAccountResponse, CreateGrantRequest,
    CreateInvitationRequest, CreateServiceAccountRequest, CreateSignupInviteRequest,
    CreateSignupInviteResponse, CreateSpoolRequest, DeleteGrantRequest, DeleteNamespaceRequest,
    DeleteRepositoryRequest, GetCurrentOwnerKeyringRequest, GetCurrentOwnerKeyringResponse,
    GetCurrentUserSpoolRequest, GrantSupportAccessRequest, GrantTargetRef,
    Invitation as ProtoInvitation, IssueServiceAccountCredentialRequest, IssuedCredentialResponse,
    ListGrantsRequest, ListSignupInvitesRequest, ListSignupInvitesResponse, ListSpoolsRequest,
    ListSupportAccessGrantsRequest, ListThreadApprovalsRequest, MonorepoNode,
    ResolveMonorepoRequest, RevokeApprovalRequest, RevokeSupportAccessRequest,
    ServiceAccountResponse, SpoolSummary, SupportAccessGrant, ThreadApproval, UpdateGrantRequest,
    UpdateNamespaceRequest, UpdateRepositoryRequest, Visibility,
    grant_target_ref::Target as GrantTargetKind,
};
use wire::ProtocolError;

use super::{
    HostedClient,
    helpers::{
        hosted_to_protocol_error, to_protocol_grant, to_protocol_namespace, to_protocol_repository,
        to_protocol_spool,
    },
    operation_id::ClientOperationId,
};

macro_rules! signed_call {
    ($self:ident, $client:ident, $rpc:ident, $path:expr, $msg:expr) => {{
        let request = $msg;
        $self
            .routes()
            .$rpc(&request)
            .await
            .map_err(hosted_to_protocol_error)?
    }};
}

/// Dispatch an authenticated unary call through the native hosted chokepoint.
/// The contract method path controls signing, human-verification retry, and
/// transport-neutral failure mapping.
macro_rules! authed_call {
    ($self:ident, $rpc:ident, $method:literal, $msg:expr) => {{
        signed_call!(
            $self,
            user,
            $rpc,
            concat!("/heddle.api.v1alpha1.RegistryService/", $method),
            $msg
        )
    }};
}

macro_rules! workflow_call {
    ($self:ident, $rpc:ident, $method:literal, $msg:expr) => {{
        signed_call!(
            $self,
            workflow,
            $rpc,
            concat!("/heddle.api.v1alpha1.WorkflowService/", $method),
            $msg
        )
    }};
}

impl HostedClient {
    pub async fn create_agent_account(
        &mut self,
        request: CreateAgentAccountRequest,
    ) -> Result<CreateAgentAccountResponse, ProtocolError> {
        self.routes()
            .create_agent_account(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    /// Resolve the acting identity for the bound bearer (subject, staff/service
    /// markers, session, server-side scope, and directly-held resource roles).
    /// Read-only; drives `heddle whoami`.
    pub async fn who_am_i(
        &mut self,
    ) -> Result<api::heddle::api::v1alpha1::WhoAmIResponse, ProtocolError> {
        Ok(signed_call!(
            self,
            auth,
            who_am_i,
            "/heddle.api.v1alpha1.IdentityService/WhoAmI",
            api::heddle::api::v1alpha1::WhoAmIRequest {}
        ))
    }

    pub async fn create_service_account(
        &mut self,
        request: CreateServiceAccountRequest,
    ) -> Result<ServiceAccountResponse, ProtocolError> {
        Ok(signed_call!(
            self,
            auth,
            create_service_account,
            "/heddle.api.v1alpha1.IdentityService/CreateServiceAccount",
            request
        ))
    }

    pub async fn create_signup_invite(
        &mut self,
        request: CreateSignupInviteRequest,
    ) -> Result<CreateSignupInviteResponse, ProtocolError> {
        Ok(signed_call!(
            self,
            auth,
            create_signup_invite,
            "/heddle.api.v1alpha1.IdentityService/CreateSignupInvite",
            request
        ))
    }

    pub async fn issue_service_account_credential(
        &mut self,
        request: IssueServiceAccountCredentialRequest,
    ) -> Result<IssuedCredentialResponse, ProtocolError> {
        self.routes()
            .issue_service_account_credential(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    pub async fn list_signup_invites(
        &mut self,
        request: ListSignupInvitesRequest,
    ) -> Result<ListSignupInvitesResponse, ProtocolError> {
        Ok(signed_call!(
            self,
            auth,
            list_signup_invites,
            "/heddle.api.v1alpha1.IdentityService/ListSignupInvites",
            request
        ))
    }

    pub async fn begin_login(
        &mut self,
        username: &str,
    ) -> Result<(String, String, u64), ProtocolError> {
        let request = BeginWebAuthnAuthenticationRequest {
            username: username.to_string(),
        };
        let response = self
            .routes()
            .begin_web_authn_authentication(&request)
            .await
            .map_err(hosted_to_protocol_error)?;
        let expires_at_secs = response
            .expires_at
            .as_ref()
            .map(|t| t.seconds.max(0) as u64)
            .unwrap_or(0);
        Ok((response.challenge_id, response.challenge, expires_at_secs))
    }

    pub async fn get_current_user_spool(&mut self) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        let spool = authed_call!(
            self,
            get_current_user_spool,
            "GetCurrentUserSpool",
            GetCurrentUserSpoolRequest {}
        );
        Ok(to_protocol_spool(spool))
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

    pub(crate) async fn ensure_claimable_owner_root(&mut self) -> anyhow::Result<()> {
        let Some(mut state) = crate::hosted_runtime::identity_state::load()? else {
            return Ok(());
        };
        if state.is_claimed() {
            return Ok(());
        }
        if let Some(server) = self.server_key.as_deref()
            && !crate::hosted_runtime::hosted::server_keys_match(&state.server, server)
        {
            return Ok(());
        }
        let (signed, signer_pem) = {
            let Some(signer) = self.context.proof_signer() else {
                return Ok(());
            };
            let signed = crate::hosted_runtime::owner_root::mint_and_record_claimable_root(
                &mut state,
                signer,
                chrono::Utc::now().timestamp(),
            )?;
            (signed, signer.to_pem()?)
        };
        crate::hosted_runtime::owner_root::persist_claimable_root(&state)?;
        let signer = crypto::Ed25519Signer::from_pem(&signer_pem)?;
        crate::hosted_runtime::owner_root::upload_claimable_root(self, &signer, signed).await
    }

    pub async fn bootstrap_owner_root(
        &mut self,
        request: BootstrapOwnerRootRequest,
    ) -> Result<BootstrapOwnerRootResponse, ProtocolError> {
        Ok(signed_call!(
            self,
            auth,
            bootstrap_owner_root,
            "/heddle.api.v1alpha1.OwnerAuthorizationService/BootstrapOwnerRoot",
            request
        ))
    }

    pub async fn get_current_owner_keyring(
        &mut self,
        request: GetCurrentOwnerKeyringRequest,
    ) -> Result<GetCurrentOwnerKeyringResponse, ProtocolError> {
        Ok(signed_call!(
            self,
            auth,
            get_current_owner_keyring,
            "/heddle.api.v1alpha1.OwnerAuthorizationService/GetCurrentOwnerKeyring",
            request
        ))
    }

    pub async fn create_spool(
        &mut self,
        parent_path: &str,
        slug: &str,
        kind: wire::HostedSpoolKind,
        display_name: Option<String>,
    ) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        self.ensure_claimable_owner_root()
            .await
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/CreateSpool");
        let owner_genesis = Some(
            self.context
                .mint_spool_owner_genesis()
                .map_err(hosted_to_protocol_error)?,
        );
        let spool = authed_call!(
            self,
            create_spool,
            "CreateSpool",
            CreateSpoolRequest {
                parent_path: parent_path.to_string(),
                slug: slug.to_string(),
                is_repo: kind.is_repo(),
                display_name,
                visibility: Visibility::Private as i32,
                client_operation_id: operation_id.to_wire(),
                settings: None,
                owner_genesis,
            }
        );
        Ok(to_protocol_spool(spool))
    }

    pub async fn create_invitation(
        &mut self,
        email: &str,
        namespace_path: &str,
        role: &str,
    ) -> Result<ProtoInvitation, ProtocolError> {
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/CreateInvitation");
        Ok(authed_call!(
            self,
            create_invitation,
            "CreateInvitation",
            CreateInvitationRequest {
                email: email.to_string(),
                namespace_path: namespace_path.to_string(),
                role: parse_hosted_role_arg(role)? as i32,
                expires_at: None,
                metadata: String::new(),
                client_operation_id: operation_id.to_wire(),
            }
        ))
    }

    pub async fn update_namespace(
        &mut self,
        full_path: &str,
        new_slug: Option<&str>,
        display_name: Option<Option<String>>,
    ) -> Result<wire::HostedNamespaceInfo, ProtocolError> {
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/UpdateNamespace");
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
                client_operation_id: operation_id.to_wire(),
            }
        );
        Ok(to_protocol_namespace(namespace))
    }

    pub async fn delete_namespace(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/DeleteNamespace");
        authed_call!(
            self,
            delete_namespace,
            "DeleteNamespace",
            DeleteNamespaceRequest {
                full_path: full_path.to_string(),
                client_operation_id: operation_id.to_wire(),
            }
        );
        Ok(())
    }

    pub async fn update_repository(
        &mut self,
        full_path: &str,
        new_slug: &str,
    ) -> Result<wire::HostedRepositoryInfo, ProtocolError> {
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/UpdateRepository");
        let repo = authed_call!(
            self,
            update_repository,
            "UpdateRepository",
            UpdateRepositoryRequest {
                full_path: full_path.to_string(),
                new_slug: new_slug.to_string(),
                client_operation_id: operation_id.to_wire(),
            }
        );
        Ok(to_protocol_repository(repo))
    }

    pub async fn delete_repository(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/DeleteRepository");
        authed_call!(
            self,
            delete_repository,
            "DeleteRepository",
            DeleteRepositoryRequest {
                full_path: full_path.to_string(),
                client_operation_id: operation_id.to_wire(),
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
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/CreateGrant");
        let target = build_target_ref(namespace_path, repo_path)?;
        let grant = authed_call!(
            self,
            create_grant,
            "CreateGrant",
            CreateGrantRequest {
                subject: subject.to_string(),
                role: parse_hosted_role_arg(role)? as i32,
                target,
                client_operation_id: operation_id.to_wire(),
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
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/UpdateGrant");
        let target = build_target_ref(namespace_path, repo_path)?;
        let grant = authed_call!(
            self,
            update_grant,
            "UpdateGrant",
            UpdateGrantRequest {
                subject: subject.to_string(),
                role: parse_hosted_role_arg(role)? as i32,
                target,
                client_operation_id: operation_id.to_wire(),
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
        let operation_id =
            ClientOperationId::fresh("heddle.api.v1alpha1.RegistryService/DeleteGrant");
        let target = build_target_ref(namespace_path, repo_path)?;
        authed_call!(
            self,
            delete_grant,
            "DeleteGrant",
            DeleteGrantRequest {
                subject: subject.to_string(),
                target,
                client_operation_id: operation_id.to_wire(),
            }
        );
        Ok(())
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
        client_operation_id: String,
    ) -> Result<ThreadApproval, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(
            "heddle.api.v1alpha1.WorkflowService/ApproveThread",
            client_operation_id,
        );
        let source_thread_id = self.require_thread_id(repo_path, source_thread).await?;
        let target_thread_id = self.require_thread_id(repo_path, target_thread).await?;
        Ok(workflow_call!(
            self,
            approve_thread,
            "ApproveThread",
            ApproveThreadRequest {
                repo_path: super::helpers::repository_ref(repo_path),
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
                source_state: objects::object::StateId::parse(source_state)
                    .ok()
                    .and_then(super::helpers::proto_state_id),
                note: note.unwrap_or_default().to_string(),
                client_operation_id: operation_id.to_wire(),
                source_thread_id,
                target_thread_id,
            }
        ))
    }

    pub async fn revoke_approval(
        &mut self,
        id: &str,
        client_operation_id: String,
    ) -> Result<(), ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(
            "heddle.api.v1alpha1.WorkflowService/RevokeApproval",
            client_operation_id,
        );
        workflow_call!(
            self,
            revoke_approval,
            "RevokeApproval",
            RevokeApprovalRequest {
                id: id.to_string(),
                client_operation_id: operation_id.to_wire(),
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
        let source_thread_id = self.require_thread_id(repo_path, source_thread).await?;
        let target_thread_id = self.require_thread_id(repo_path, target_thread).await?;
        Ok(workflow_call!(
            self,
            list_thread_approvals,
            "ListThreadApprovals",
            ListThreadApprovalsRequest {
                repo_path: super::helpers::repository_ref(repo_path),
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
                source_thread_id,
                target_thread_id,
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
        let source_thread_id = self.require_thread_id(repo_path, source_thread).await?;
        let target_thread_id = self.require_thread_id(repo_path, target_thread).await?;
        Ok(workflow_call!(
            self,
            check_merge_eligibility,
            "CheckMergeEligibility",
            CheckMergeEligibilityRequest {
                repo_path: super::helpers::repository_ref(repo_path),
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
                source_state: objects::object::StateId::parse(source_state)
                    .ok()
                    .and_then(super::helpers::proto_state_id),
                gated_action: gated_action.to_string(),
                changed_paths,
                author_user_id: author_user_id.unwrap_or_default().to_string(),
                source_thread_id,
                target_thread_id,
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
        let operation_id = ClientOperationId::caller_or_fresh(
            "heddle.api.v1alpha1.RegistryService/GrantSupportAccess",
            client_operation_id,
        );
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
                client_operation_id: operation_id.to_wire(),
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
        let operation_id = ClientOperationId::caller_or_fresh(
            "heddle.api.v1alpha1.RegistryService/RevokeSupportAccess",
            client_operation_id,
        );
        authed_call!(
            self,
            revoke_support_access,
            "RevokeSupportAccess",
            RevokeSupportAccessRequest {
                id: id.to_string(),
                client_operation_id: operation_id.to_wire(),
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
            target: Some(GrantTargetKind::RepoPath(
                super::helpers::repository_ref(rp).expect("non-empty repository path"),
            )),
        })),
        _ => Err(ProtocolError::InvalidState(
            "exactly one of namespace_path or repo_path must be set".into(),
        )),
    }
}

/// Parse a CLI-supplied role name into the proto `HostedRole` enum.
fn parse_hosted_role_arg(
    value: &str,
) -> Result<api::heddle::api::v1alpha1::HostedRole, ProtocolError> {
    use api::heddle::api::v1alpha1::HostedRole;
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
mod tests {
    use api::heddle::api::v1alpha1::{HostedRole, grant_target_ref::Target};

    use super::*;

    #[tokio::test]
    async fn administration_facade_builds_and_dispatches_every_native_request() {
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;

        client.who_am_i().await.unwrap();
        let _ = client
            .create_service_account(CreateServiceAccountRequest::default())
            .await;
        let _ = client
            .create_signup_invite(CreateSignupInviteRequest {
                recipient_email: Some("alice@example.com".to_string()),
                client_operation_id: "signup-invite-op".to_string(),
            })
            .await;
        let _ = client
            .issue_service_account_credential(IssueServiceAccountCredentialRequest::default())
            .await;
        let _ = client
            .list_signup_invites(ListSignupInvitesRequest {
                page_size: 200,
                page_token: String::new(),
            })
            .await;
        client.begin_login("alice@example.com").await.unwrap();
        let _ = client.get_current_user_spool().await;
        assert!(client.list_spools(true).await.unwrap().is_empty());
        let _ = client
            .create_spool(
                "acme",
                "widgets",
                wire::HostedSpoolKind::Project,
                Some("Widgets".to_string()),
            )
            .await;
        client
            .create_invitation("alice@example.com", "acme", "developer")
            .await
            .unwrap();
        let _ = client
            .update_namespace("acme", Some("acme-new"), Some(None))
            .await;
        client.delete_namespace("acme-new").await.unwrap();
        let _ = client
            .update_repository("acme/widgets", "widgets-new")
            .await;
        client.delete_repository("acme/widgets-new").await.unwrap();
        let _ = client
            .create_grant("principal:alice", "reader", None, Some("acme/widgets"))
            .await;
        assert!(
            client
                .list_grants(Some("repo:acme/widgets"))
                .await
                .unwrap()
                .is_empty()
        );
        let _ = client
            .update_grant("principal:alice", "maintainer", Some("acme"), None)
            .await;
        client
            .delete_grant("principal:alice", Some("acme"), None)
            .await
            .unwrap();
        client
            .revoke_approval("approval-1", "revoke-approval-op".to_string())
            .await
            .unwrap();
        client
            .grant_support_access(
                "operator@example.com",
                None,
                Some("acme/widgets"),
                300,
                "investigation",
                "support-op".to_string(),
            )
            .await
            .unwrap();
        assert!(
            client
                .list_support_access_grants(Some("acme"), None, true)
                .await
                .unwrap()
                .is_empty()
        );
        client
            .revoke_support_access("support-1", "support-revoke-op".to_string())
            .await
            .unwrap();
        client.resolve_monorepo("acme", Some(4)).await.unwrap();

        let invalid_state = objects::object::StateId::from_bytes([3; 32]).to_string_full();
        assert!(
            client
                .approve_thread(
                    "acme/widgets",
                    "feature",
                    "main",
                    &invalid_state,
                    Some("looks good"),
                    "approval-op".to_string(),
                )
                .await
                .is_err()
        );
        assert!(
            client
                .list_thread_approvals("acme/widgets", "feature", "main")
                .await
                .is_err()
        );
        assert!(
            client
                .check_merge_eligibility(
                    "acme/widgets",
                    "feature",
                    "main",
                    &invalid_state,
                    "land",
                    vec!["src/lib.rs".to_string()],
                    Some("principal:alice"),
                )
                .await
                .is_err()
        );

        client.close().await;
        server.await.unwrap();
    }

    #[test]
    fn parse_hosted_role_arg_accepts_every_role_and_rejects_unknown() {
        assert_eq!(parse_hosted_role_arg("reader").unwrap(), HostedRole::Reader);
        assert_eq!(
            parse_hosted_role_arg(" Developer ").unwrap(),
            HostedRole::Developer
        );
        assert_eq!(
            parse_hosted_role_arg("MAINTAINER").unwrap(),
            HostedRole::Maintainer
        );
        assert_eq!(parse_hosted_role_arg("admin").unwrap(), HostedRole::Admin);
        assert_eq!(parse_hosted_role_arg("owner").unwrap(), HostedRole::Owner);
        let err = parse_hosted_role_arg("root").unwrap_err();
        assert!(err.to_string().contains("invalid role"));
    }

    #[test]
    fn build_target_ref_requires_exactly_one_path() {
        let ns = build_target_ref(Some("acme"), None).unwrap().unwrap();
        assert!(matches!(ns.target, Some(Target::NamespacePath(p)) if p == "acme"));

        let repo = build_target_ref(None, Some("acme/widgets"))
            .unwrap()
            .unwrap();
        assert!(matches!(repo.target, Some(Target::RepoPath(_))));

        // Exactly one required: neither, both, or empty-only → error.
        assert!(build_target_ref(None, None).is_err());
        assert!(build_target_ref(Some("acme"), Some("acme/widgets")).is_err());
        assert!(build_target_ref(Some(""), None).is_err());
        assert!(build_target_ref(None, Some("")).is_err());
        assert!(build_target_ref(Some(""), Some("")).is_err());
    }
}
