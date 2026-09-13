use api::heddle::api::v1alpha1::{
    BeginWebAuthnAuthenticationRequest, BootstrapOwnerRootRequest, BootstrapOwnerRootResponse,
    CreateAgentAccountRequest, CreateAgentAccountResponse, CreateInvitationRequest,
    CreateServiceAccountRequest, GetCurrentOwnerKeyringRequest, GetCurrentOwnerKeyringResponse,
    GrantSupportAccessRequest, GrantTargetRef, Invitation as ProtoInvitation,
    IssueServiceAccountCredentialRequest, IssuedCredentialResponse, ListSupportAccessGrantsRequest,
    MonorepoNode, ResolveMonorepoRequest, RevokeSupportAccessRequest, ServiceAccountResponse,
    SupportAccessGrant, grant_target_ref::Target as GrantTargetKind,
};
use wire::ProtocolError;

use super::{HostedClient, helpers::hosted_to_protocol_error, operation_id::ClientOperationId};

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
    pub async fn observe_current_identity(
        &mut self,
    ) -> Result<
        (
            api::heddle::api::v2alpha1::PrincipalRecord,
            api::heddle::api::v2alpha1::CurrentCredentialRecord,
        ),
        ProtocolError,
    > {
        use api::heddle::api::v2alpha1 as contract;
        let remote = self.native().await.map_err(native_protocol_error)?;
        let mut observation = remote
            .observe::<thread_api::rpc::IdentityServiceObserveIdentity>(
                contract::ObserveIdentityRequest {
                    include_current_credential: true,
                    observe: Some(contract::ObserveOptions {
                        mode: contract::ObservationMode::Once as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_protocol_error)?;
        let batch = observation
            .next_commit()
            .await
            .map_err(native_protocol_error)?
            .ok_or_else(|| {
                ProtocolError::InvalidState("identity view ended without a checkpoint".into())
            })?;
        let mut principal = None;
        let mut credential = None;
        for change in batch.changes {
            match change {
                contract::identity_event::Payload::Identity(value) => {
                    if principal.replace(value).is_some() {
                        return Err(ProtocolError::InvalidState(
                            "identity view duplicated principal".into(),
                        ));
                    }
                }
                contract::identity_event::Payload::CurrentCredential(value) => {
                    if credential.replace(value).is_some() {
                        return Err(ProtocolError::InvalidState(
                            "identity view duplicated current credential".into(),
                        ));
                    }
                }
                _ => {}
            }
        }
        let principal = principal
            .ok_or_else(|| ProtocolError::InvalidState("identity view omitted principal".into()))?;
        let credential = credential.ok_or_else(|| {
            ProtocolError::InvalidState("identity view omitted current credential".into())
        })?;
        if principal.account_id.is_empty()
            || principal.id.is_empty()
            || credential.subject.is_empty()
            || contract::CredentialKind::try_from(credential.kind).is_err()
            || credential.kind == contract::CredentialKind::Unspecified as i32
        {
            return Err(ProtocolError::InvalidState(
                "identity view has incomplete current authority".into(),
            ));
        }
        Ok((principal, credential))
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
        request: api::heddle::api::v2alpha1::CreateSignupInvitationRequest,
    ) -> Result<api::heddle::api::v2alpha1::CreateSignupInvitationResponse, ProtocolError> {
        let remote = self.native().await.map_err(native_protocol_error)?;
        let response = remote
            .api
            .call::<thread_api::rpc::IdentityServiceCreateSignupInvitation>(&request)
            .await
            .map_err(super::helpers::native_client_error)?;
        require_applied_receipt(
            response.receipt.clone(),
            &request.client_operation_id,
            &remote.description.endpoint,
            "signup invitation creation",
        )?;
        Ok(response)
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

    pub async fn list_signup_invitations(
        &mut self,
    ) -> Result<(Vec<api::heddle::api::v2alpha1::SignupInvitation>, u32), ProtocolError> {
        self.signup_invitation_view(false).await
    }

    pub async fn signup_invitation_quota(&mut self) -> Result<u32, ProtocolError> {
        self.signup_invitation_view(true)
            .await
            .map(|(_, quota)| quota)
    }

    async fn signup_invitation_view(
        &mut self,
        only_quota: bool,
    ) -> Result<(Vec<api::heddle::api::v2alpha1::SignupInvitation>, u32), ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let remote = self.native().await.map_err(native_protocol_error)?;
        let mut records = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut after_page = Vec::new();
        let mut quota = None;
        loop {
            let mut observation = remote
                .observe::<thread_api::rpc::IdentityServiceObserveIdentity>(
                    contract::ObserveIdentityRequest {
                        signup_invitations: Some(contract::PageRequest {
                            after_page: after_page.clone(),
                            size: if only_quota { 1 } else { 32 },
                        }),
                        observe: Some(contract::ObserveOptions {
                            mode: contract::ObservationMode::Once as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .map_err(native_protocol_error)?;
            let batch = observation
                .next_commit()
                .await
                .map_err(native_protocol_error)?
                .ok_or_else(|| {
                    ProtocolError::InvalidState("invitation view ended without a checkpoint".into())
                })?;
            let mut page = None;
            let mut page_quota = None;
            for change in batch.changes {
                match change {
                    contract::identity_event::Payload::SignupInvitation(record) => {
                        let id = record
                            .r#ref
                            .as_ref()
                            .ok_or_else(|| {
                                ProtocolError::InvalidState("invitation row has no identity".into())
                            })?
                            .id
                            .clone();
                        uuid::Uuid::parse_str(&id).map_err(native_protocol_error)?;
                        if !seen.insert(id) || records.len() >= 4096 {
                            return Err(ProtocolError::InvalidState(
                                "invitation view contains duplicate or too many rows".into(),
                            ));
                        }
                        records.push(record);
                    }
                    contract::identity_event::Payload::InvitationQuota(value) => {
                        if !value.distribution_id.is_empty()
                            || page_quota.replace(value.remaining).is_some()
                        {
                            return Err(ProtocolError::InvalidState(
                                "account invitation quota is ambiguous".into(),
                            ));
                        }
                    }
                    contract::identity_event::Payload::Status(status)
                        if status.section == "signup_invitations" =>
                    {
                        if !matches!(
                            contract::Coverage::try_from(status.coverage),
                            Ok(contract::Coverage::Complete | contract::Coverage::Partial)
                        ) || page.replace(status.page).is_some()
                        {
                            return Err(ProtocolError::InvalidState(
                                "invitation list coverage is incomplete or duplicated".into(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            let current = page_quota.ok_or_else(|| {
                ProtocolError::InvalidState("account invitation quota absent".into())
            })?;
            if quota.replace(current).is_some_and(|prior| prior != current) {
                return Err(ProtocolError::InvalidState(
                    "invitation quota changed during pagination".into(),
                ));
            }
            if only_quota {
                return Ok((Vec::new(), current));
            }
            let page = page.flatten().ok_or_else(|| {
                ProtocolError::InvalidState("invitation page status absent".into())
            })?;
            if page.exhausted {
                return Ok((records, current));
            }
            if page.next_page.is_empty() || page.next_page == after_page {
                return Err(ProtocolError::InvalidState(
                    "invitation page cursor did not advance".into(),
                ));
            }
            after_page = page.next_page;
        }
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
        use api::heddle::api::v2alpha1 as contract;
        let remote = self.native().await.map_err(native_protocol_error)?;
        let mut observation = remote
            .observe::<thread_api::rpc::IdentityServiceObserveIdentity>(
                contract::ObserveIdentityRequest {
                    observe: Some(contract::ObserveOptions {
                        mode: contract::ObservationMode::Once as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_protocol_error)?;
        let batch = observation
            .next_commit()
            .await
            .map_err(native_protocol_error)?
            .ok_or_else(|| {
                ProtocolError::InvalidState(
                    "identity observation ended without a checkpoint".into(),
                )
            })?;
        let mut principals = batch.changes.into_iter().filter_map(|change| match change {
            contract::identity_event::Payload::Identity(principal) => Some(principal),
            _ => None,
        });
        let principal = principals.next().ok_or_else(|| {
            ProtocolError::InvalidState("personal Spool identity unavailable".into())
        })?;
        if principals.next().is_some() {
            return Err(ProtocolError::InvalidState(
                "identity observation returned multiple principals".into(),
            ));
        }
        let address = principal.personal_spool.ok_or_else(|| {
            ProtocolError::ObjectNotFound("personal Spool has not been created".into())
        })?;
        let reference = address.r#ref.ok_or_else(|| {
            ProtocolError::InvalidState("personal Spool has no stable identity".into())
        })?;
        uuid::Uuid::parse_str(&reference.id).map_err(native_protocol_error)?;
        if address.path_segments.is_empty()
            || address
                .path_segments
                .iter()
                .any(|segment| segment.is_empty() || segment.contains('/'))
        {
            return Err(ProtocolError::InvalidState(
                "personal Spool address is invalid".into(),
            ));
        }
        Ok(wire::HostedSpoolInfo {
            spool_id: reference.id,
            full_path: address.path_segments.join("/"),
            kind: "spool".into(),
            is_repo: false,
            display_name: address.path_segments.last().cloned(),
        })
    }

    pub async fn get_spool(
        &mut self,
        full_path: &str,
    ) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        native_spool_info(self.native_spool_overview(full_path).await?)
    }

    pub async fn promote_spool(
        &mut self,
        full_path: &str,
        client_operation_id: &str,
    ) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let current = self.native_spool_overview(full_path).await?;
        let operation_id = if client_operation_id.is_empty() {
            ClientOperationId::fresh("heddle.api.v2alpha1.SpoolService/PromoteSpool")
        } else {
            ClientOperationId::for_required_method(
                "heddle.api.v2alpha1.SpoolService/PromoteSpool",
                client_operation_id.to_owned(),
            )?
        };
        let request = contract::PromoteSpoolRequest {
            client_operation_id: operation_id.to_wire(),
            spool: current.r#ref.clone(),
            expected_version: current.version,
        };
        let remote = self.native().await.map_err(native_protocol_error)?;
        let response = remote
            .api
            .call::<thread_api::rpc::SpoolServicePromoteSpool>(&request)
            .await
            .map_err(super::helpers::native_client_error)?;
        let receipt = response
            .receipt
            .ok_or_else(|| ProtocolError::InvalidState("Spool promotion receipt absent".into()))?;
        if receipt.client_operation_id != request.client_operation_id
            || receipt.endpoint != remote.description.endpoint
            || !matches!(
                receipt.outcome,
                Some(contract::mutation_receipt::Outcome::Applied(_))
            )
        {
            return Err(ProtocolError::InvalidState(
                "Spool promotion was not applied to the requested endpoint".into(),
            ));
        }
        let promoted = response
            .spool
            .ok_or_else(|| ProtocolError::InvalidState("promoted Spool overview absent".into()))?;
        if promoted.r#ref != request.spool || promoted.parent.is_some() {
            return Err(ProtocolError::InvalidState(
                "promoted Spool differs from requested identity or remains nested".into(),
            ));
        }
        native_spool_info(promoted)
    }

    pub async fn list_spools(
        &mut self,
    ) -> Result<Vec<api::heddle::api::v2alpha1::SpoolOverview>, ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let remote = self.native().await.map_err(native_protocol_error)?;
        let mut rows = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut after_page = Vec::new();
        loop {
            let mut observation = remote
                .observe::<thread_api::rpc::WorkspaceServiceObserveWorkspace>(
                    contract::ObserveWorkspaceRequest {
                        pages: Some(contract::WorkspacePages {
                            spools: Some(contract::PageRequest {
                                after_page: after_page.clone(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        observe: Some(contract::ObserveOptions {
                            mode: contract::ObservationMode::Once as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .map_err(native_protocol_error)?;
            let batch = observation
                .next_commit()
                .await
                .map_err(native_protocol_error)?
                .ok_or_else(|| {
                    ProtocolError::InvalidState(
                        "workspace Spool list ended without checkpoint".into(),
                    )
                })?;
            let mut spool_page = None;
            for change in batch.changes {
                match change {
                    contract::workspace_event::Payload::Spool(spool) => {
                        let reference = spool.r#ref.as_ref().ok_or_else(|| {
                            ProtocolError::InvalidState("Spool list row has no identity".into())
                        })?;
                        uuid::Uuid::parse_str(&reference.id).map_err(native_protocol_error)?;
                        if !seen.insert(reference.id.clone()) || rows.len() >= 4096 {
                            return Err(ProtocolError::InvalidState(
                                "Spool list contains duplicates or exceeds local bound".into(),
                            ));
                        }
                        rows.push(spool);
                    }
                    contract::workspace_event::Payload::Status(status)
                        if status.section == "spools" =>
                    {
                        if status.coverage != contract::Coverage::Complete as i32
                            || spool_page.replace(status.page).is_some()
                        {
                            return Err(ProtocolError::InvalidState(
                                "Spool list coverage is incomplete or duplicated".into(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            let page = spool_page.flatten().ok_or_else(|| {
                ProtocolError::InvalidState("Spool list page status absent".into())
            })?;
            if page.exhausted {
                return Ok(rows);
            }
            if page.next_page.is_empty() || page.next_page == after_page {
                return Err(ProtocolError::InvalidState(
                    "Spool list cursor did not advance".into(),
                ));
            }
            after_page = page.next_page;
        }
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
        is_repo: bool,
        display_name: Option<String>,
    ) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        self.create_spool_with_id(
            parent_path,
            slug,
            is_repo,
            display_name,
            uuid::Uuid::now_v7(),
        )
        .await
    }

    /// Publishing an existing local spool retains its original UUID, so every
    /// already signed Thread and capture keeps the same identity remotely.
    pub async fn create_spool_with_id(
        &mut self,
        parent_path: &str,
        slug: &str,
        is_repo: bool,
        display_name: Option<String>,
        spool_uuid: uuid::Uuid,
    ) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let parent = if parent_path.is_empty() {
            None
        } else {
            Some(self.resolve_spool_ref(parent_path).await?)
        };
        let operation_id = ClientOperationId::fresh("heddle.api.v2alpha1.SpoolService/CreateSpool");
        let owner = self.current_owner_state().await?;
        let genesis = self
            .context
            .mint_spool_creation(
                repo::SpoolCreationIntent {
                    spool_uuid,
                    parent_spool_uuid: parent
                        .as_ref()
                        .map(|value| uuid::Uuid::parse_str(&value.id))
                        .transpose()
                        .map_err(native_protocol_error)?,
                    parent_path_segments: if parent_path.is_empty() {
                        Vec::new()
                    } else {
                        parent_path.split('/').map(str::to_owned).collect()
                    },
                    name: slug.to_owned(),
                },
                &owner,
            )
            .map_err(hosted_to_protocol_error)?;
        let new_id = genesis
            .genesis
            .as_ref()
            .ok_or_else(|| ProtocolError::InvalidState("new spool genesis is absent".into()))?
            .spool_uuid
            .clone();
        let remote = self.native().await.map_err(native_protocol_error)?;
        let request = contract::CreateSpoolRequest {
            client_operation_id: operation_id.to_wire(),
            parent: parent.clone(),
            slug: slug.into(),
            settings: Some(contract::SpoolSettings {
                audience: contract::Audience::Private as i32,
                default_state_audience: contract::Audience::Members as i32,
                ..Default::default()
            }),
            ownership: Some(contract::create_spool_request::Ownership::OwnerGenesis(
                genesis.clone(),
            )),
            display_name,
        };
        let response = remote
            .api
            .call::<thread_api::rpc::SpoolServiceCreateSpool>(&request)
            .await
            .map_err(super::helpers::native_client_error)?;
        let receipt = response
            .receipt
            .ok_or_else(|| ProtocolError::InvalidState("spool creation receipt absent".into()))?;
        if receipt.client_operation_id != request.client_operation_id
            || receipt.endpoint != remote.description.endpoint
            || !matches!(
                receipt.outcome,
                Some(contract::mutation_receipt::Outcome::Applied(_))
            )
        {
            return Err(ProtocolError::InvalidState(
                "spool creation was not applied to the requested endpoint".into(),
            ));
        }
        let spool = response
            .spool
            .ok_or_else(|| ProtocolError::InvalidState("created spool absent".into()))?;
        let reference = spool
            .r#ref
            .as_ref()
            .ok_or_else(|| ProtocolError::InvalidState("created spool identity absent".into()))?;
        if uuid::Uuid::parse_str(&reference.id)
            .map_err(native_protocol_error)?
            .as_bytes()
            != new_id.as_slice()
            || spool.owner_genesis.as_ref() != Some(&genesis)
            || spool.parent != parent
            || spool.slug != slug
        {
            return Err(ProtocolError::InvalidState(
                "created spool differs from signed request".into(),
            ));
        }
        Ok(wire::HostedSpoolInfo {
            spool_id: reference.id.clone(),
            full_path: if parent_path.is_empty() {
                slug.into()
            } else {
                format!("{}/{slug}", parent_path.trim_end_matches('/'))
            },
            kind: "spool".into(),
            is_repo,
            display_name: (!spool.name.is_empty()).then_some(spool.name),
        })
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

    pub async fn update_spool(
        &mut self,
        full_path: &str,
        new_slug: Option<&str>,
        display_name: Option<Option<String>>,
    ) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let current = self.native_spool_overview(full_path).await?;
        let operation_id = ClientOperationId::fresh("heddle.api.v2alpha1.SpoolService/ReviseSpool");
        let request = contract::ReviseSpoolRequest {
            client_operation_id: operation_id.to_wire(),
            spool: current.r#ref.clone(),
            expected_version: current.version.clone(),
            name: display_name
                .unwrap_or_else(|| Some(current.name.clone()))
                .unwrap_or_default(),
            settings: current.settings.clone(),
            slug: new_slug.map(ToOwned::to_owned),
        };
        let remote = self.native().await.map_err(native_protocol_error)?;
        let response = remote
            .api
            .call::<thread_api::rpc::SpoolServiceReviseSpool>(&request)
            .await
            .map_err(super::helpers::native_client_error)?;
        let receipt = response
            .receipt
            .ok_or_else(|| ProtocolError::InvalidState("Spool revision receipt absent".into()))?;
        if receipt.client_operation_id != request.client_operation_id
            || receipt.endpoint != remote.description.endpoint
            || !matches!(
                receipt.outcome,
                Some(contract::mutation_receipt::Outcome::Applied(_))
            )
        {
            return Err(ProtocolError::InvalidState(
                "Spool revision was not applied to the requested endpoint".into(),
            ));
        }
        let revised = response
            .spool
            .ok_or_else(|| ProtocolError::InvalidState("revised Spool overview absent".into()))?;
        if revised.r#ref != request.spool
            || revised.name != request.name
            || revised.settings != request.settings
            || revised.slug != request.slug.as_deref().unwrap_or(&current.slug)
            || revised.path_segments.is_empty()
        {
            return Err(ProtocolError::InvalidState(
                "revised Spool differs from requested mutation".into(),
            ));
        }
        Ok(wire::HostedSpoolInfo {
            spool_id: revised
                .r#ref
                .as_ref()
                .map(|value| value.id.clone())
                .unwrap_or_default(),
            full_path: revised.path_segments.join("/"),
            kind: "spool".into(),
            is_repo: false,
            display_name: (!revised.name.is_empty()).then_some(revised.name),
        })
    }

    pub async fn delete_spool(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        let overview = self.native_spool_overview(full_path).await?;
        let operation_id = ClientOperationId::fresh("heddle.api.v2alpha1.SpoolService/DeleteSpool");
        let request = api::heddle::api::v2alpha1::DeleteSpoolRequest {
            client_operation_id: operation_id.to_wire(),
            spool: overview.r#ref,
            expected_version: overview.version,
        };
        let remote = self.native().await.map_err(native_protocol_error)?;
        let response = remote
            .api
            .call::<thread_api::rpc::SpoolServiceDeleteSpool>(&request)
            .await
            .map_err(super::helpers::native_client_error)?;
        let receipt = response
            .receipt
            .ok_or_else(|| ProtocolError::InvalidState("Spool deletion receipt absent".into()))?;
        if receipt.client_operation_id != request.client_operation_id
            || receipt.endpoint != remote.description.endpoint
            || !matches!(
                receipt.outcome,
                Some(api::heddle::api::v2alpha1::mutation_receipt::Outcome::Applied(_))
            )
        {
            return Err(ProtocolError::InvalidState(
                "Spool deletion was not applied to the requested endpoint".into(),
            ));
        }
        Ok(())
    }

    pub async fn update_namespace(
        &mut self,
        full_path: &str,
        new_slug: Option<&str>,
        display_name: Option<Option<String>>,
    ) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        self.update_spool(full_path, new_slug, display_name).await
    }

    pub async fn delete_namespace(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        self.delete_spool(full_path).await
    }

    pub async fn update_repository(
        &mut self,
        full_path: &str,
        new_slug: &str,
    ) -> Result<wire::HostedSpoolInfo, ProtocolError> {
        self.update_spool(full_path, Some(new_slug), None).await
    }

    pub async fn delete_repository(&mut self, full_path: &str) -> Result<(), ProtocolError> {
        self.delete_spool(full_path).await
    }

    pub async fn create_grant(
        &mut self,
        subject: &str,
        role: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
        client_operation_id: String,
    ) -> Result<api::heddle::api::v2alpha1::GrantRecord, ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let address = grant_spool_address(namespace_path, repo_path)?;
        let spool = self.resolve_spool_ref(address).await?;
        let principal_id = self.resolve_principal_id(subject, &spool).await?;
        let method = "heddle.api.v2alpha1.SpoolService/PutGrant";
        let operation_id = if client_operation_id.is_empty() {
            ClientOperationId::fresh(method)
        } else {
            ClientOperationId::for_required_method(method, client_operation_id)?
        };
        let grant = contract::GrantRecord {
            r#ref: Some(contract::RecordRef {
                spool: Some(spool),
                id: uuid::Uuid::now_v7().to_string(),
            }),
            principal: Some(contract::PublicPrincipalSummary {
                id: principal_id.clone(),
                handle: subject.to_owned(),
                ..Default::default()
            }),
            principal_id,
            role: native_grant_role(role)? as i32,
            ..Default::default()
        };
        let request = contract::PutGrantRequest {
            client_operation_id: operation_id.to_wire(),
            grant: Some(grant.clone()),
            ..Default::default()
        };
        let remote = self.native().await.map_err(native_protocol_error)?;
        let response = remote
            .api
            .call::<thread_api::rpc::SpoolServicePutGrant>(&request)
            .await
            .map_err(super::helpers::native_client_error)?;
        require_applied_receipt(
            response.receipt,
            &request.client_operation_id,
            &remote.description.endpoint,
            "grant creation",
        )?;
        Ok(grant)
    }

    pub async fn list_grants(
        &mut self,
        resource: Option<&str>,
    ) -> Result<Vec<api::heddle::api::v2alpha1::GrantRecord>, ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let address = resource.ok_or_else(|| {
            ProtocolError::InvalidState("grant list requires a Spool address".into())
        })?;
        let spool = self.resolve_spool_ref(address).await?;
        let remote = self.native().await.map_err(native_protocol_error)?;
        let mut rows = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut after_page = Vec::new();
        loop {
            let mut observation = remote
                .observe::<thread_api::rpc::SpoolServiceObserveSpool>(
                    contract::ObserveSpoolRequest {
                        spool: Some(spool.clone()),
                        sections: vec![contract::SpoolSection::Grants as i32],
                        pages: Some(contract::SpoolPages {
                            grants: Some(contract::PageRequest {
                                after_page: after_page.clone(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        observe: Some(contract::ObserveOptions {
                            mode: contract::ObservationMode::Once as i32,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .map_err(native_protocol_error)?;
            let batch = observation
                .next_commit()
                .await
                .map_err(native_protocol_error)?
                .ok_or_else(|| {
                    ProtocolError::InvalidState("grant observation ended without checkpoint".into())
                })?;
            let mut status = None;
            for change in batch.changes {
                match change {
                    contract::spool_event::Payload::Grant(grant) => {
                        let reference = grant.r#ref.as_ref().ok_or_else(|| {
                            ProtocolError::InvalidState("grant row has no stable identity".into())
                        })?;
                        if reference.spool.as_ref() != Some(&spool)
                            || uuid::Uuid::parse_str(&reference.id).is_err()
                            || !seen.insert(reference.id.clone())
                            || rows.len() >= 4096
                        {
                            return Err(ProtocolError::InvalidState(
                                "grant observation contains an invalid, duplicate or excess row"
                                    .into(),
                            ));
                        }
                        rows.push(grant);
                    }
                    contract::spool_event::Payload::Status(section)
                        if section.section == "grants" =>
                    {
                        if section.coverage != contract::Coverage::Complete as i32
                            || status.replace(section.page).is_some()
                        {
                            return Err(ProtocolError::InvalidState(
                                "grant coverage is incomplete or duplicated".into(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            let page = status
                .flatten()
                .ok_or_else(|| ProtocolError::InvalidState("grant page status absent".into()))?;
            if page.exhausted {
                return Ok(rows);
            }
            if page.next_page.is_empty() || page.next_page == after_page {
                return Err(ProtocolError::InvalidState(
                    "grant cursor did not advance".into(),
                ));
            }
            after_page = page.next_page;
        }
    }

    pub async fn update_grant(
        &mut self,
        subject: &str,
        role: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
    ) -> Result<api::heddle::api::v2alpha1::GrantRecord, ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let address = grant_spool_address(namespace_path, repo_path)?;
        let spool = self.resolve_spool_ref(address).await?;
        let principal_id = self.resolve_principal_id(subject, &spool).await?;
        let mut matching = self
            .list_grants(Some(address))
            .await?
            .into_iter()
            .filter(|grant| grant.principal_id == principal_id);
        let mut grant = matching.next().ok_or_else(|| {
            ProtocolError::ObjectNotFound("principal has no grant on this Spool".into())
        })?;
        if matching.next().is_some() {
            return Err(ProtocolError::InvalidState(
                "principal has multiple grants; select a stable grant ID".into(),
            ));
        }
        let expected_version = grant.version.clone();
        grant.role = native_grant_role(role)? as i32;
        let method = "heddle.api.v2alpha1.SpoolService/PutGrant";
        let request = contract::PutGrantRequest {
            client_operation_id: ClientOperationId::fresh(method).to_wire(),
            grant: Some(grant.clone()),
            expected_version,
        };
        let remote = self.native().await.map_err(native_protocol_error)?;
        let response = remote
            .api
            .call::<thread_api::rpc::SpoolServicePutGrant>(&request)
            .await
            .map_err(super::helpers::native_client_error)?;
        require_applied_receipt(
            response.receipt,
            &request.client_operation_id,
            &remote.description.endpoint,
            "grant update",
        )?;
        Ok(grant)
    }

    pub async fn delete_grant(
        &mut self,
        grant_id: &str,
        namespace_path: Option<&str>,
        repo_path: Option<&str>,
        client_operation_id: String,
    ) -> Result<(), ProtocolError> {
        use api::heddle::api::v2alpha1 as contract;
        let address = grant_spool_address(namespace_path, repo_path)?;
        let spool = self.resolve_spool_ref(address).await?;
        let grant_id = uuid::Uuid::parse_str(grant_id)
            .map_err(native_protocol_error)?
            .to_string();
        let record = self
            .list_grants(Some(address))
            .await?
            .into_iter()
            .find(|value| {
                value
                    .r#ref
                    .as_ref()
                    .is_some_and(|reference| reference.id == grant_id)
            })
            .ok_or_else(|| {
                ProtocolError::ObjectNotFound("grant ID is not visible on this Spool".into())
            })?;
        let method = "heddle.api.v2alpha1.SpoolService/RevokeGrant";
        let operation_id = if client_operation_id.is_empty() {
            ClientOperationId::fresh(method)
        } else {
            ClientOperationId::for_required_method(method, client_operation_id)?
        };
        let request = contract::RevokeGrantRequest {
            client_operation_id: operation_id.to_wire(),
            grant: Some(contract::RecordRef {
                spool: Some(spool),
                id: grant_id,
            }),
            expected_version: record.version,
        };
        let remote = self.native().await.map_err(native_protocol_error)?;
        let response = remote
            .api
            .call::<thread_api::rpc::SpoolServiceRevokeGrant>(&request)
            .await
            .map_err(super::helpers::native_client_error)?;
        require_applied_receipt(
            response.receipt,
            &request.client_operation_id,
            &remote.description.endpoint,
            "grant revocation",
        )?;
        Ok(())
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

fn native_spool_info(
    spool: api::heddle::api::v2alpha1::SpoolOverview,
) -> Result<wire::HostedSpoolInfo, ProtocolError> {
    let reference = spool.r#ref.ok_or_else(|| {
        ProtocolError::InvalidState("Spool overview has no stable identity".into())
    })?;
    uuid::Uuid::parse_str(&reference.id).map_err(native_protocol_error)?;
    if spool.path_segments.is_empty()
        || spool
            .path_segments
            .iter()
            .any(|segment| segment.is_empty() || segment.contains('/'))
    {
        return Err(ProtocolError::InvalidState(
            "Spool overview has no canonical address".into(),
        ));
    }
    Ok(wire::HostedSpoolInfo {
        spool_id: reference.id,
        full_path: spool.path_segments.join("/"),
        kind: "spool".into(),
        is_repo: false,
        display_name: (!spool.name.is_empty()).then_some(spool.name),
    })
}

fn grant_spool_address<'a>(
    namespace_path: Option<&'a str>,
    repo_path: Option<&'a str>,
) -> Result<&'a str, ProtocolError> {
    match (namespace_path, repo_path) {
        (Some(address), None) | (None, Some(address)) if !address.is_empty() => Ok(address),
        _ => Err(ProtocolError::InvalidState(
            "grant operation requires exactly one Spool address".into(),
        )),
    }
}

fn native_grant_role(
    value: &str,
) -> Result<api::heddle::api::v2alpha1::ResourceRole, ProtocolError> {
    use api::heddle::api::v2alpha1::ResourceRole;
    match value {
        "reader" => Ok(ResourceRole::Reader),
        "writer" => Ok(ResourceRole::Writer),
        "administrator" => Ok(ResourceRole::Administrator),
        _ => Err(ProtocolError::InvalidState(
            "grant role must be reader, writer or administrator".into(),
        )),
    }
}

pub(super) fn require_applied_receipt(
    receipt: Option<api::heddle::api::v2alpha1::MutationReceipt>,
    operation_id: &str,
    endpoint: &Option<api::heddle::api::v2alpha1::EndpointRef>,
    action: &str,
) -> Result<(), ProtocolError> {
    let receipt =
        receipt.ok_or_else(|| ProtocolError::InvalidState(format!("{action} receipt absent")))?;
    if receipt.client_operation_id != operation_id
        || &receipt.endpoint != endpoint
        || !matches!(
            receipt.outcome,
            Some(api::heddle::api::v2alpha1::mutation_receipt::Outcome::Applied(_))
        )
    {
        return Err(ProtocolError::InvalidState(format!(
            "{action} was not applied to the requested endpoint"
        )));
    }
    Ok(())
}

fn native_protocol_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::InvalidState(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::MutexGuard};

    use api::heddle::api::v1alpha1::{HostedRole, grant_target_ref::Target};

    use super::*;

    struct IsolatedHeddleHome {
        _guard: MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        previous_home: Option<OsString>,
    }

    impl IsolatedHeddleHome {
        fn new() -> Self {
            let guard = config::credentials::lock_test_env();
            let temp = tempfile::TempDir::new().expect("temporary Heddle home");
            let previous_home = std::env::var_os("HEDDLE_HOME");
            unsafe { std::env::set_var("HEDDLE_HOME", temp.path()) };
            Self {
                _guard: guard,
                _temp: temp,
                previous_home,
            }
        }
    }

    impl Drop for IsolatedHeddleHome {
        fn drop(&mut self) {
            unsafe {
                match &self.previous_home {
                    Some(value) => std::env::set_var("HEDDLE_HOME", value),
                    None => std::env::remove_var("HEDDLE_HOME"),
                }
            }
        }
    }

    #[tokio::test]
    async fn personal_spool_comes_from_native_identity_observation() {
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let personal = client
            .get_current_user_spool()
            .await
            .expect("v2 personal Spool");
        assert_eq!(personal.full_path, "acme");
        assert_eq!(
            personal.spool_id,
            uuid::Uuid::from_bytes([2; 16]).to_string()
        );
        client.close().await;
        server.await.expect("server");
    }

    #[tokio::test]
    async fn spool_list_comes_from_native_workspace_observation() {
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        assert!(
            client
                .list_spools()
                .await
                .expect("v2 Spool list")
                .is_empty()
        );
        client.close().await;
        server.await.expect("server");
    }

    #[tokio::test]
    async fn promote_spool_uses_native_versioned_mutation() {
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let original = client
            .get_spool("acme")
            .await
            .expect("native Spool overview");
        let promoted = client
            .promote_spool("acme", "")
            .await
            .expect("native versioned promotion");
        assert_eq!(promoted.spool_id, original.spool_id);
        assert_eq!(promoted.full_path, "acme");
        client.close().await;
        server.await.expect("server");
    }

    #[tokio::test]
    async fn native_review_observation_feeds_original_signed_record() {
        use objects::object::{ContentHash, StateId};
        use thread_api::thread_control::{Author, Control, PreparedControl, Review, ReviewKind};

        let (client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let snapshot = client
            .observe_review("acme", "feature")
            .await
            .expect("review snapshot");
        assert_eq!(snapshot.overview.name, "feature");
        assert_eq!(
            snapshot
                .comparison
                .as_ref()
                .expect("comparison")
                .policy_version,
            vec![6; 32]
        );
        let landing = client
            .observe_landing_assessment("acme", "feature", "main")
            .await
            .expect("target-bound assessment");
        let assessment = landing
            .overview
            .landing_assessment
            .expect("exact landing target");
        assert_eq!(
            assessment.target.expect("target").id.expect("ID").value,
            vec![4; 32]
        );
        let signer = crypto::Ed25519Signer::from_seed(&[13; 32]).expect("test author");
        let prepared = PreparedControl::sign(
            &snapshot.overview,
            Control::Review(Review {
                id: uuid::Uuid::now_v7(),
                source: StateId::from_bytes([5; 32]),
                target: StateId::from_bytes([5; 32]),
                policy_version: ContentHash::from_bytes([6; 32]),
                kind: ReviewKind::Approval,
                explanation: "looks ready".into(),
                revokes: None,
                expires_at_unix_seconds: None,
                coverage: None,
            }),
            Author {
                account: uuid::Uuid::from_bytes([9; 16]),
                agent_id: None,
                authority_envelope: b"test original authority",
            },
            uuid::Uuid::now_v7(),
            1_800_000_000_000,
            &signer,
        )
        .expect("sign exact review comparison");
        client
            .record_review(&prepared.record_review().expect("wire request"))
            .await
            .expect("native typed review RPC");
        client.close().await;
        server.await.expect("server");
    }

    #[tokio::test]
    async fn administration_facade_builds_and_dispatches_every_native_request() {
        let _home = IsolatedHeddleHome::new();
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;

        let (principal, credential) = client.observe_current_identity().await.unwrap();
        assert_eq!(
            principal.account_id,
            uuid::Uuid::from_bytes([9; 16]).to_string()
        );
        assert_eq!(credential.subject, "agent:reviewer-1");
        let _ = client
            .create_service_account(CreateServiceAccountRequest::default())
            .await;
        let created = client
            .create_signup_invite(api::heddle::api::v2alpha1::CreateSignupInvitationRequest {
                invitation: Some(api::heddle::api::v2alpha1::SignupInvitation {
                    bound_email: "alice@example.com".to_string(),
                    ..Default::default()
                }),
                client_operation_id: "signup-invite-op".to_string(),
            })
            .await
            .expect("native invitation mutation");
        assert_eq!(created.redemption_secret, b"one-time-invite");
        let _ = client
            .issue_service_account_credential(IssueServiceAccountCredentialRequest::default())
            .await;
        let (invites, quota) = client
            .list_signup_invitations()
            .await
            .expect("native invitation observation");
        assert!(
            invites.is_empty(),
            "undistributed allowance is not an invitation"
        );
        assert_eq!(quota, 2);
        client.begin_login("alice@example.com").await.unwrap();
        let personal = client
            .get_current_user_spool()
            .await
            .expect("v2 personal Spool");
        assert_eq!(personal.full_path, "acme");
        assert_eq!(
            personal.spool_id,
            uuid::Uuid::from_bytes([2; 16]).to_string()
        );
        assert!(client.list_spools().await.unwrap().is_empty());
        client
            .create_spool("acme", "widgets", true, Some("Widgets".to_string()))
            .await
            .expect("CreateSpool mints owner genesis and reaches the server");
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
        let created_grant = client
            .create_grant("alice", "reader", None, Some("acme/widgets"), String::new())
            .await
            .expect("native grant creation");
        assert_eq!(
            client
                .list_grants(Some("acme/widgets"))
                .await
                .unwrap()
                .len(),
            1
        );
        let updated_grant = client
            .update_grant("alice", "writer", None, Some("acme/widgets"))
            .await
            .expect("native grant update uses observed version");
        assert_eq!(
            updated_grant.role,
            api::heddle::api::v2alpha1::ResourceRole::Writer as i32
        );
        assert_eq!(
            client
                .list_grants(Some("acme/widgets"))
                .await
                .unwrap()
                .len(),
            1
        );
        client
            .delete_grant(
                &created_grant.r#ref.as_ref().expect("grant ref").id,
                None,
                Some("acme/widgets"),
                String::new(),
            )
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

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn namespace_and_repository_mutations_use_spool_requests() {
        let (mut client, server, captured) =
            crate::hosted_runtime::hosted::test_server::start_recording_spool_mutations().await;

        client
            .update_namespace("acme", Some("acme-new"), Some(None))
            .await
            .unwrap();
        client.delete_namespace("acme-new").await.unwrap();
        client
            .update_repository("acme/widgets", "widgets-new")
            .await
            .unwrap();
        client.delete_repository("acme/widgets-new").await.unwrap();

        client.close().await;
        server.await.unwrap();

        let captured = captured.lock().unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(captured.native_updates.len(), 2);
        assert_eq!(captured.native_deletes.len(), 2);

        let namespace_update = &captured.native_updates[0];
        assert_eq!(
            namespace_update.spool.as_ref().expect("Spool").id,
            uuid::Uuid::from_bytes([2; 16]).to_string()
        );
        assert_eq!(namespace_update.expected_version, vec![7; 32]);
        assert_eq!(namespace_update.slug.as_deref(), Some("acme-new"));
        assert_eq!(namespace_update.name, "");
        assert!(!namespace_update.client_operation_id.is_empty());

        let repository_update = &captured.native_updates[1];
        assert_eq!(repository_update.expected_version, vec![7; 32]);
        assert_eq!(repository_update.slug.as_deref(), Some("widgets-new"));
        assert!(!repository_update.client_operation_id.is_empty());

        for deletion in &captured.native_deletes {
            assert_eq!(
                deletion.spool.as_ref().expect("resolved Spool").id,
                uuid::Uuid::from_bytes([2; 16]).to_string()
            );
            assert_eq!(deletion.expected_version, vec![7; 32]);
            assert!(!deletion.client_operation_id.is_empty());
        }
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

    #[tokio::test]
    async fn create_spool_sends_device_key_signed_uuidv7_owner_genesis() {
        use crypto::Ed25519Signer;
        use sha2::{Digest, Sha256};

        let _home = IsolatedHeddleHome::new();
        let (mut client, server, captured) =
            crate::hosted_runtime::hosted::test_server::start_recording_create_spool().await;
        let created = client
            .create_spool("cedar-jay-9dce33", "spool-d", true, None)
            .await
            .expect("CreateSpool with minted genesis");
        assert_eq!(created.full_path, "cedar-jay-9dce33/spool-d");

        let request = captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .pop()
            .expect("CreateSpool reached the server");
        let signed = request
            .owner_genesis
            .expect("CreateSpool must send SignedSpoolOwnerGenesis");
        let genesis = signed.genesis.expect("signed genesis payload");
        let spool_uuid: [u8; 16] = genesis
            .spool_uuid
            .as_slice()
            .try_into()
            .expect("spool UUID is 16 bytes");
        assert_eq!(
            uuid::Uuid::from_bytes(spool_uuid).get_version_num(),
            7,
            "weft takes the new spool UUID from genesis and checks version 7"
        );
        let owner_public_key = genesis
            .owner_public_key
            .expect("owner public key")
            .public_key;
        let digest = Sha256::new()
            .chain_update(&owner_public_key)
            .chain_update(spool_uuid)
            .finalize();
        Ed25519Signer::verify_with_public_key(
            &digest,
            &owner_public_key,
            &signed.owner_signature.expect("owner signature").signature,
        )
        .expect("genesis is a protocol-2 self-signature");

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn create_spool_provision_sequence_excludes_owner_root_ceremony() {
        use crypto::{Ed25519Signer, Signer as _};

        let _home = IsolatedHeddleHome::new();
        let (mut client, server, calls, signer_pem) =
            crate::hosted_runtime::auth_login_agent::test_support::start_recording_client().await;
        let signer = Ed25519Signer::from_pem(&signer_pem).expect("test signer");
        let mut state = crate::hosted_runtime::identity_state::ClaimState::new(
            "api.provision.test".to_string(),
            uuid::Uuid::parse_str("7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28").expect("account id"),
            "subject-1".to_string(),
            "quiet-otter".to_string(),
            hex::encode(signer.public_key()),
            None,
        );
        state.record_claimable_owner_root(signer.public_key(), b"invalid owner-root ceremony");
        crate::hosted_runtime::identity_state::store(&state).expect("store claim state");

        client
            .create_spool("quiet-otter", "spool-d", true, None)
            .await
            .expect("CreateSpool must not read or upload the claimable owner root");
        client.close().await;
        server.await.expect("recording server");

        assert_eq!(
            *calls.lock().unwrap_or_else(|poison| poison.into_inner()),
            ["/heddle.api.v1alpha1.RegistryService/CreateSpool"],
            "auto-provision must issue CreateSpool without BootstrapOwnerRoot"
        );
    }
}
