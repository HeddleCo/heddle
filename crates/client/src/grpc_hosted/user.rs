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
use proto::ProtocolError;
use tonic::Request;

use super::{
    HostedGrpcClient,
    helpers::{
        status_to_protocol_error, to_protocol_grant, to_protocol_namespace, to_protocol_repository,
    },
};

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
    ) -> Result<proto::HostedNamespaceInfo, ProtocolError> {
        let mut request = Request::new(GetCurrentUserNamespaceRequest {});
        self.apply_auth(&mut request)?;
        let namespace = self
            .user
            .get_current_user_namespace(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(to_protocol_namespace(namespace))
    }

    pub async fn list_namespaces(
        &mut self,
    ) -> Result<Vec<proto::HostedNamespaceInfo>, ProtocolError> {
        let mut request = Request::new(ListNamespacesRequest {});
        self.apply_auth(&mut request)?;
        let response = self
            .user
            .list_namespaces(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
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
    ) -> Result<proto::HostedNamespaceInfo, ProtocolError> {
        let mut request = Request::new(grpc::heddle::v1::CreateNamespaceRequest {
            kind: parse_namespace_kind_arg(kind)? as i32,
            slug: slug.to_string(),
            parent_path: parent_path.unwrap_or_default().to_string(),
            display_name: display_name.unwrap_or_default(),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        let namespace = self
            .user
            .create_namespace(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(to_protocol_namespace(namespace))
    }

    pub async fn create_repository(
        &mut self,
        namespace_path: &str,
        slug: &str,
    ) -> Result<proto::HostedRepositoryInfo, ProtocolError> {
        let mut request = Request::new(CreateRepositoryRequest {
            namespace_path: namespace_path.to_string(),
            slug: slug.to_string(),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        let repo = self
            .user
            .create_repository(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(to_protocol_repository(repo))
    }

    pub async fn list_repositories(
        &mut self,
        namespace_path: Option<&str>,
    ) -> Result<Vec<proto::HostedRepositoryInfo>, ProtocolError> {
        let mut request = Request::new(ListRepositoriesRequest {
            namespace_path: namespace_path.unwrap_or_default().to_string(),
        });
        self.apply_auth(&mut request)?;
        let response = self
            .user
            .list_repositories(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
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
    ) -> Result<proto::HostedNamespaceInfo, ProtocolError> {
        let (display_name, clear_display_name) = match display_name {
            Some(Some(value)) => (value, false),
            Some(None) => (String::new(), true),
            None => (String::new(), false),
        };
        let mut request = Request::new(UpdateNamespaceRequest {
            full_path: full_path.to_string(),
            new_slug: new_slug.unwrap_or_default().to_string(),
            display_name,
            clear_display_name,
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        let namespace = self
            .user
            .update_namespace(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(to_protocol_namespace(namespace))
    }

    pub async fn delete_namespace(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        let mut request = Request::new(DeleteNamespaceRequest {
            full_path: full_path.to_string(),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        self.user
            .delete_namespace(request)
            .await
            .map_err(status_to_protocol_error)?;
        Ok(())
    }

    pub async fn update_repository(
        &mut self,
        full_path: &str,
        new_slug: &str,
    ) -> Result<proto::HostedRepositoryInfo, ProtocolError> {
        let mut request = Request::new(UpdateRepositoryRequest {
            full_path: full_path.to_string(),
            new_slug: new_slug.to_string(),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        let repo = self
            .user
            .update_repository(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(to_protocol_repository(repo))
    }

    pub async fn delete_repository(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        let mut request = Request::new(DeleteRepositoryRequest {
            full_path: full_path.to_string(),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        self.user
            .delete_repository(request)
            .await
            .map_err(status_to_protocol_error)?;
        Ok(())
    }

    pub async fn create_grant(
        &mut self,
        subject: &str,
        role: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
    ) -> Result<proto::HostedGrantInfo, ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        let mut request = Request::new(CreateGrantRequest {
            subject: subject.to_string(),
            role: parse_hosted_role_arg(role)? as i32,
            target,
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        let grant = self
            .user
            .create_grant(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(to_protocol_grant(grant))
    }

    pub async fn list_grants(
        &mut self,
        resource: Option<&str>,
    ) -> Result<Vec<proto::HostedGrantInfo>, ProtocolError> {
        let mut request = Request::new(ListGrantsRequest {
            resource: resource.unwrap_or_default().to_string(),
        });
        self.apply_auth(&mut request)?;
        let response = self
            .user
            .list_grants(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(response.grants.into_iter().map(to_protocol_grant).collect())
    }

    pub async fn update_grant(
        &mut self,
        subject: &str,
        role: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
    ) -> Result<proto::HostedGrantInfo, ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        let mut request = Request::new(UpdateGrantRequest {
            subject: subject.to_string(),
            role: parse_hosted_role_arg(role)? as i32,
            target,
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        let grant = self
            .user
            .update_grant(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(to_protocol_grant(grant))
    }

    pub async fn delete_grant(
        &mut self,
        subject: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        let mut request = Request::new(DeleteGrantRequest {
            subject: subject.to_string(),
            target,
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        self.user
            .delete_grant(request)
            .await
            .map_err(status_to_protocol_error)?;
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
        let mut request = Request::new(CreateInvitationRequest {
            email: email.to_string(),
            namespace_path: namespace_path.to_string(),
            role: parse_hosted_role_arg(role)? as i32,
            expires_at: None,
            metadata: String::new(),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        let invitation = self
            .user
            .create_invitation(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
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
        let mut request = Request::new(ApproveThreadRequest {
            repo_path: repo_path.to_string(),
            source_thread: source_thread.to_string(),
            target_thread: target_thread.to_string(),
            source_state: objects::object::ChangeId::parse(source_state)
                .map(|id| id.as_bytes().to_vec())
                .unwrap_or_default(),
            note: note.unwrap_or_default().to_string(),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        Ok(self
            .user
            .approve_thread(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner())
    }

    pub async fn revoke_approval(&mut self, id: &str) -> Result<(), ProtocolError> {
        let mut request = Request::new(RevokeApprovalRequest {
            id: id.to_string(),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        self.user
            .revoke_approval(request)
            .await
            .map_err(status_to_protocol_error)?;
        Ok(())
    }

    pub async fn list_thread_approvals(
        &mut self,
        repo_path: &str,
        source_thread: &str,
        target_thread: &str,
    ) -> Result<Vec<ThreadApproval>, ProtocolError> {
        let mut request = Request::new(ListThreadApprovalsRequest {
            repo_path: repo_path.to_string(),
            source_thread: source_thread.to_string(),
            target_thread: target_thread.to_string(),
        });
        self.apply_auth(&mut request)?;
        Ok(self
            .user
            .list_thread_approvals(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner()
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
        let mut request = Request::new(CheckMergeEligibilityRequest {
            repo_path: repo_path.to_string(),
            source_thread: source_thread.to_string(),
            target_thread: target_thread.to_string(),
            source_state: objects::object::ChangeId::parse(source_state)
                .map(|id| id.as_bytes().to_vec())
                .unwrap_or_default(),
            gated_action: gated_action.to_string(),
            changed_paths,
            author_user_id: author_user_id.unwrap_or_default().to_string(),
        });
        self.apply_auth(&mut request)?;
        Ok(self
            .user
            .check_merge_eligibility(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner())
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
        let mut request = Request::new(GrantSupportAccessRequest {
            operator_email: operator_email.to_string(),
            target,
            ttl_seconds: Some(prost_types::Duration {
                seconds: i64::from(ttl_seconds),
                nanos: 0,
            }),
            reason: reason.to_string(),
            client_operation_id,
        });
        self.apply_auth(&mut request)?;
        Ok(self
            .user
            .grant_support_access(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner())
    }

    pub async fn list_support_access_grants(
        &mut self,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
        include_inactive: bool,
    ) -> Result<Vec<SupportAccessGrant>, ProtocolError> {
        let target = build_target_ref(namespace_path, repo_path)?;
        let mut request = Request::new(ListSupportAccessGrantsRequest {
            target,
            include_inactive,
        });
        self.apply_auth(&mut request)?;
        Ok(self
            .user
            .list_support_access_grants(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner()
            .grants)
    }

    pub async fn revoke_support_access(
        &mut self,
        id: &str,
        client_operation_id: String,
    ) -> Result<(), ProtocolError> {
        let mut request = Request::new(RevokeSupportAccessRequest {
            id: id.to_string(),
            client_operation_id,
        });
        self.apply_auth(&mut request)?;
        self.user
            .revoke_support_access(request)
            .await
            .map_err(status_to_protocol_error)?;
        Ok(())
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
