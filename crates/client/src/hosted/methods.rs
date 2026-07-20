use api::heddle::api::v1alpha1::*;

use super::{BidirectionalStream, HostedClient, Result, ServerStream};

/// Exact generated-contract call surface used by higher-level hosted helpers.
#[derive(Clone, Copy)]
pub struct HostedRoutes<'a> {
    client: &'a HostedClient,
}

impl<'a> HostedRoutes<'a> {
    pub(super) fn new(client: &'a HostedClient) -> Self {
        Self { client }
    }
}

macro_rules! unary_method {
    ($name:ident, $service:literal, $rpc:literal, $request:ty, $response:ty) => {
        pub async fn $name(&self, request: &$request) -> Result<$response> {
            self.client
                .call_unary(
                    concat!("/heddle.api.v1alpha1.", $service, "/", $rpc),
                    request,
                )
                .await
        }
    };
}

impl HostedRoutes<'_> {
    unary_method!(
        begin_web_authn_authentication,
        "IdentityService",
        "BeginWebAuthnAuthentication",
        BeginWebAuthnAuthenticationRequest,
        AuthChallengeResponse
    );
    unary_method!(
        create_device_authorization,
        "IdentityService",
        "CreateDeviceAuthorization",
        CreateDeviceAuthorizationRequest,
        DeviceAuthorizationResponse
    );
    unary_method!(
        create_service_account,
        "IdentityService",
        "CreateServiceAccount",
        CreateServiceAccountRequest,
        ServiceAccountResponse
    );
    unary_method!(
        exchange_device_authorization,
        "IdentityService",
        "ExchangeDeviceAuthorization",
        ExchangeDeviceAuthorizationRequest,
        AccessTokenResponse
    );
    unary_method!(
        issue_service_account_credential,
        "IdentityService",
        "IssueServiceAccountCredential",
        IssueServiceAccountCredentialRequest,
        IssuedCredentialResponse
    );
    unary_method!(
        mint_biscuit,
        "IdentityService",
        "MintBiscuit",
        MintBiscuitRequest,
        AccessTokenResponse
    );
    unary_method!(
        who_am_i,
        "IdentityService",
        "WhoAmI",
        WhoAmIRequest,
        WhoAmIResponse
    );

    unary_method!(
        create_grant,
        "RegistryService",
        "CreateGrant",
        CreateGrantRequest,
        HostedGrant
    );
    unary_method!(
        create_namespace,
        "RegistryService",
        "CreateNamespace",
        CreateNamespaceRequest,
        HostedNamespace
    );
    unary_method!(
        create_repository,
        "RegistryService",
        "CreateRepository",
        CreateRepositoryRequest,
        HostedRepository
    );
    unary_method!(
        delete_grant,
        "RegistryService",
        "DeleteGrant",
        DeleteGrantRequest,
        DeleteResponse
    );
    unary_method!(
        delete_namespace,
        "RegistryService",
        "DeleteNamespace",
        DeleteNamespaceRequest,
        DeleteResponse
    );
    unary_method!(
        delete_repository,
        "RegistryService",
        "DeleteRepository",
        DeleteRepositoryRequest,
        DeleteResponse
    );
    unary_method!(
        get_current_user_namespace,
        "RegistryService",
        "GetCurrentUserNamespace",
        GetCurrentUserNamespaceRequest,
        HostedNamespace
    );
    unary_method!(
        grant_support_access,
        "RegistryService",
        "GrantSupportAccess",
        GrantSupportAccessRequest,
        SupportAccessGrant
    );
    unary_method!(
        list_grants,
        "RegistryService",
        "ListGrants",
        ListGrantsRequest,
        ListGrantsResponse
    );
    unary_method!(
        list_spools,
        "RegistryService",
        "ListSpools",
        ListSpoolsRequest,
        ListSpoolsResponse
    );
    unary_method!(
        list_support_access_grants,
        "RegistryService",
        "ListSupportAccessGrants",
        ListSupportAccessGrantsRequest,
        ListSupportAccessGrantsResponse
    );
    unary_method!(
        resolve_monorepo,
        "RegistryService",
        "ResolveMonorepo",
        ResolveMonorepoRequest,
        MonorepoNode
    );
    unary_method!(
        revoke_support_access,
        "RegistryService",
        "RevokeSupportAccess",
        RevokeSupportAccessRequest,
        DeleteResponse
    );
    unary_method!(
        update_grant,
        "RegistryService",
        "UpdateGrant",
        UpdateGrantRequest,
        HostedGrant
    );
    unary_method!(
        update_namespace,
        "RegistryService",
        "UpdateNamespace",
        UpdateNamespaceRequest,
        HostedNamespace
    );
    unary_method!(
        update_repository,
        "RegistryService",
        "UpdateRepository",
        UpdateRepositoryRequest,
        HostedRepository
    );

    unary_method!(
        list_refs,
        "RepoSyncService",
        "ListRefs",
        ListRefsRequest,
        ListRefsResponse
    );
    unary_method!(
        update_ref,
        "RepoSyncService",
        "UpdateRef",
        UpdateRefRequest,
        UpdateRefResponse
    );

    unary_method!(
        get_blame,
        "RepositoryService",
        "GetBlame",
        GetBlameRequest,
        GetBlameResponse
    );
    unary_method!(
        get_blob,
        "RepositoryService",
        "GetBlob",
        GetBlobRequest,
        BlobResponse
    );
    unary_method!(
        get_compare,
        "RepositoryService",
        "GetCompare",
        GetCompareRequest,
        CompareResponse
    );
    unary_method!(
        get_context_history,
        "RepositoryService",
        "GetContextHistory",
        GetContextHistoryRequest,
        GetContextHistoryResponse
    );
    unary_method!(
        list_context,
        "RepositoryService",
        "ListContext",
        ListContextRequest,
        ListContextResponse
    );
    unary_method!(
        list_context_suggestions,
        "RepositoryService",
        "ListContextSuggestions",
        ListContextSuggestionsRequest,
        ListContextSuggestionsResponse
    );
    unary_method!(
        revise_context,
        "RepositoryService",
        "ReviseContext",
        ReviseContextRequest,
        ReviseContextResponse
    );
    unary_method!(
        set_context,
        "RepositoryService",
        "SetContext",
        SetContextRequest,
        SetContextResponse
    );
    unary_method!(
        supersede_context,
        "RepositoryService",
        "SupersedeContext",
        SupersedeContextRequest,
        SupersedeContextResponse
    );

    unary_method!(
        approve_thread,
        "WorkflowService",
        "ApproveThread",
        ApproveThreadRequest,
        ThreadApproval
    );
    unary_method!(
        check_merge_eligibility,
        "WorkflowService",
        "CheckMergeEligibility",
        CheckMergeEligibilityRequest,
        CheckMergeEligibilityResponse
    );
    unary_method!(
        list_thread_approvals,
        "WorkflowService",
        "ListThreadApprovals",
        ListThreadApprovalsRequest,
        ListThreadApprovalsResponse
    );
    unary_method!(
        revoke_approval,
        "WorkflowService",
        "RevokeApproval",
        RevokeApprovalRequest,
        DeleteResponse
    );

    unary_method!(
        open_discussion,
        "CollaborationService",
        "OpenDiscussion",
        OpenDiscussionRequest,
        Discussion
    );
    unary_method!(
        append_turn,
        "CollaborationService",
        "AppendTurn",
        AppendTurnRequest,
        Discussion
    );
    unary_method!(
        list_discussions_by_state,
        "CollaborationService",
        "ListByState",
        ListDiscussionsByStateRequest,
        ListDiscussionsResponse
    );

    unary_method!(
        sign_state,
        "StateReviewService",
        "SignState",
        SignStateRequest,
        SignStateResponse
    );
    pub async fn wait_for_device_authorization(
        &self,
        request: &WaitForDeviceAuthorizationRequest,
    ) -> Result<ServerStream<DeviceAuthorizationEvent>> {
        self.client
            .call_server_stream(
                "/heddle.api.v1alpha1.IdentityService/WaitForDeviceAuthorization",
                request,
                "",
            )
            .await
    }

    pub async fn push(
        &self,
        client_operation_id: impl Into<String>,
    ) -> Result<BidirectionalStream<PushClientFrame, PushServerFrame>> {
        self.client
            .call_bidirectional(
                "/heddle.api.v1alpha1.RepoSyncService/Push",
                client_operation_id,
            )
            .await
    }

    pub async fn pull(&self) -> Result<BidirectionalStream<PullClientFrame, PullServerFrame>> {
        self.client
            .call_bidirectional("/heddle.api.v1alpha1.RepoSyncService/Pull", "")
            .await
    }
}

impl HostedClient {
    pub fn stream_opening_proof(
        &self,
        method: &str,
        stream_id: impl Into<String>,
        repository: RepositoryRef,
        resume_cursor: impl Into<String>,
        capability_context: Vec<u8>,
    ) -> Result<StreamOpeningProof> {
        self.context.stream_opening_proof(
            method,
            stream_id,
            repository,
            resume_cursor,
            capability_context,
        )
    }
}

#[cfg(test)]
mod tests {
    use api::{
        ALL_METHODS, MethodRoute, StreamingShape,
        heddle::api::v1alpha1::{DeploymentTarget, ServiceMaturity},
    };

    #[test]
    fn shipped_native_inventory_is_42_unary_one_server_stream_and_two_bidi() {
        const ROUTES: &[MethodRoute] = &[
            MethodRoute::CollaborationServiceAppendTurn,
            MethodRoute::CollaborationServiceListByState,
            MethodRoute::CollaborationServiceOpenDiscussion,
            MethodRoute::IdentityServiceBeginWebAuthnAuthentication,
            MethodRoute::IdentityServiceCreateDeviceAuthorization,
            MethodRoute::IdentityServiceCreateServiceAccount,
            MethodRoute::IdentityServiceExchangeDeviceAuthorization,
            MethodRoute::IdentityServiceIssueServiceAccountCredential,
            MethodRoute::IdentityServiceMintBiscuit,
            MethodRoute::IdentityServiceWaitForDeviceAuthorization,
            MethodRoute::IdentityServiceWhoAmI,
            MethodRoute::RegistryServiceCreateGrant,
            MethodRoute::RegistryServiceCreateNamespace,
            MethodRoute::RegistryServiceCreateRepository,
            MethodRoute::RegistryServiceDeleteGrant,
            MethodRoute::RegistryServiceDeleteNamespace,
            MethodRoute::RegistryServiceDeleteRepository,
            MethodRoute::RegistryServiceGetCurrentUserNamespace,
            MethodRoute::RegistryServiceGrantSupportAccess,
            MethodRoute::RegistryServiceListGrants,
            MethodRoute::RegistryServiceListSpools,
            MethodRoute::RegistryServiceListSupportAccessGrants,
            MethodRoute::RegistryServiceResolveMonorepo,
            MethodRoute::RegistryServiceRevokeSupportAccess,
            MethodRoute::RegistryServiceUpdateGrant,
            MethodRoute::RegistryServiceUpdateNamespace,
            MethodRoute::RegistryServiceUpdateRepository,
            MethodRoute::RepoSyncServiceListRefs,
            MethodRoute::RepoSyncServicePull,
            MethodRoute::RepoSyncServicePush,
            MethodRoute::RepoSyncServiceUpdateRef,
            MethodRoute::RepositoryServiceGetBlame,
            MethodRoute::RepositoryServiceGetBlob,
            MethodRoute::RepositoryServiceGetCompare,
            MethodRoute::RepositoryServiceGetContextHistory,
            MethodRoute::RepositoryServiceListContext,
            MethodRoute::RepositoryServiceListContextSuggestions,
            MethodRoute::RepositoryServiceReviseContext,
            MethodRoute::RepositoryServiceSetContext,
            MethodRoute::RepositoryServiceSupersedeContext,
            MethodRoute::StateReviewServiceSignState,
            MethodRoute::WorkflowServiceApproveThread,
            MethodRoute::WorkflowServiceCheckMergeEligibility,
            MethodRoute::WorkflowServiceListThreadApprovals,
            MethodRoute::WorkflowServiceRevokeApproval,
        ];
        let shipped = ALL_METHODS
            .iter()
            .filter(|method| {
                method.maturity == ServiceMaturity::Shipped
                    && method.deployment_targets.contains(&DeploymentTarget::Weft)
                    && ROUTES.contains(&method.route)
            })
            .collect::<Vec<_>>();

        assert_eq!(shipped.len(), 45);
        assert_eq!(
            shipped
                .iter()
                .filter(|method| method.streaming == StreamingShape::Unary)
                .count(),
            42
        );
        assert_eq!(
            shipped
                .iter()
                .filter(|method| method.streaming == StreamingShape::ServerStreaming)
                .count(),
            1
        );
        assert_eq!(
            shipped
                .iter()
                .filter(|method| method.streaming == StreamingShape::Bidirectional)
                .count(),
            2
        );
    }
}
