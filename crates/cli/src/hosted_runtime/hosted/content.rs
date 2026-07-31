use api::heddle::api::v1alpha1::{
    AnnotationScope, CompareResponse, ContextAnnotationKind, GetBlameRequest, GetBlameResponse,
    GetCompareRequest, GetContextHistoryRequest, GetContextHistoryResponse, ListContextRequest,
    ListContextResponse, ListContextSuggestionsRequest, ListContextSuggestionsResponse,
    ReviseContextRequest, ReviseContextResponse, SetContextRequest, SetContextResponse,
    SupersedeContextRequest, SupersedeContextResponse,
};
use wire::ProtocolError;

use super::{HostedClient, helpers::hosted_to_protocol_error, operation_id::ClientOperationId};

const SET_CONTEXT: &str = "heddle.api.v1alpha1.RepositoryService/SetContext";
const REVISE_CONTEXT: &str = "heddle.api.v1alpha1.RepositoryService/ReviseContext";
const SUPERSEDE_CONTEXT: &str = "heddle.api.v1alpha1.RepositoryService/SupersedeContext";

impl HostedClient {
    pub async fn get_compare(
        &mut self,
        repo_path: &str,
        from: &str,
        to: &str,
        include_semantic: bool,
    ) -> Result<CompareResponse, ProtocolError> {
        let request = GetCompareRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            from: from.to_string(),
            to: to.to_string(),
            include_semantic,
        };
        self.routes()
            .get_compare(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    pub async fn get_blame(
        &mut self,
        repo_path: &str,
        r#ref: Option<&str>,
        path: &str,
    ) -> Result<GetBlameResponse, ProtocolError> {
        let request = GetBlameRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            r#ref: r#ref.unwrap_or_default().to_string(),
            path: path.to_string(),
        };
        self.routes()
            .get_blame(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    pub async fn list_context(
        &mut self,
        repo_path: &str,
        r#ref: Option<&str>,
        prefix: Option<&str>,
        tag_filter: Option<&str>,
    ) -> Result<ListContextResponse, ProtocolError> {
        let request = ListContextRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            r#ref: r#ref.unwrap_or_default().to_string(),
            prefix: prefix.map(str::to_string),
            tag_filter: tag_filter.map(str::to_string),
        };
        self.routes()
            .list_context(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn set_context(
        &mut self,
        repo_path: &str,
        path: &str,
        target_state_id: Option<&str>,
        scope: AnnotationScope,
        kind: ContextAnnotationKind,
        tags: Vec<String>,
        content: &str,
        agent_provider: Option<&str>,
        agent_model: Option<&str>,
        client_operation_id: String,
    ) -> Result<SetContextResponse, ProtocolError> {
        let operation_id =
            ClientOperationId::for_required_method(SET_CONTEXT, client_operation_id)?;
        let request = SetContextRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            path: path.to_string(),
            scope: Some(scope),
            tags,
            content: content.to_string(),
            agent_provider: agent_provider.unwrap_or_default().to_string(),
            agent_model: agent_model.unwrap_or_default().to_string(),
            target_state_id: target_state_id
                .and_then(|s| objects::object::StateId::parse(s).ok())
                .and_then(super::helpers::proto_state_id),
            kind: kind as i32,
            client_operation_id: operation_id.to_wire(),
        };
        self.routes()
            .set_context(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    pub async fn get_context_history(
        &mut self,
        repo_path: &str,
        r#ref: Option<&str>,
        annotation_id: &str,
    ) -> Result<GetContextHistoryResponse, ProtocolError> {
        let request = GetContextHistoryRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            r#ref: r#ref.unwrap_or_default().to_string(),
            annotation_id: annotation_id.to_string(),
        };
        self.routes()
            .get_context_history(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    pub async fn list_context_suggestions(
        &mut self,
        repo_path: &str,
        r#ref: Option<&str>,
        limit: u32,
    ) -> Result<ListContextSuggestionsResponse, ProtocolError> {
        let request = ListContextSuggestionsRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            r#ref: r#ref.unwrap_or_default().to_string(),
            limit,
        };
        self.routes()
            .list_context_suggestions(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn revise_context(
        &mut self,
        repo_path: &str,
        annotation_id: &str,
        content: &str,
        tags: Vec<String>,
        agent_provider: Option<&str>,
        agent_model: Option<&str>,
        kind: ContextAnnotationKind,
        client_operation_id: String,
    ) -> Result<ReviseContextResponse, ProtocolError> {
        let operation_id =
            ClientOperationId::for_required_method(REVISE_CONTEXT, client_operation_id)?;
        let request = revise_context_request(
            repo_path,
            annotation_id,
            content,
            tags,
            agent_provider,
            agent_model,
            kind,
            &operation_id,
        );
        self.routes()
            .revise_context(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn supersede_context(
        &mut self,
        repo_path: &str,
        annotation_id: &str,
        path: Option<&str>,
        target_state_id: Option<&str>,
        scope: AnnotationScope,
        tags: Vec<String>,
        content: &str,
        agent_provider: Option<&str>,
        agent_model: Option<&str>,
        kind: ContextAnnotationKind,
        client_operation_id: String,
    ) -> Result<SupersedeContextResponse, ProtocolError> {
        let operation_id =
            ClientOperationId::for_required_method(SUPERSEDE_CONTEXT, client_operation_id)?;
        let request = supersede_context_request(
            repo_path,
            annotation_id,
            path,
            target_state_id,
            scope,
            tags,
            content,
            agent_provider,
            agent_model,
            kind,
            &operation_id,
        );
        self.routes()
            .supersede_context(&request)
            .await
            .map_err(hosted_to_protocol_error)
    }
}

#[allow(clippy::too_many_arguments)]
fn revise_context_request(
    repo_path: &str,
    annotation_id: &str,
    content: &str,
    tags: Vec<String>,
    agent_provider: Option<&str>,
    agent_model: Option<&str>,
    kind: ContextAnnotationKind,
    operation_id: &ClientOperationId,
) -> ReviseContextRequest {
    ReviseContextRequest {
        repo_path: super::helpers::repository_ref(repo_path),
        annotation_id: annotation_id.to_string(),
        content: content.to_string(),
        tags,
        agent_provider: agent_provider.unwrap_or_default().to_string(),
        agent_model: agent_model.unwrap_or_default().to_string(),
        kind: kind as i32,
        client_operation_id: operation_id.to_wire(),
    }
}

#[allow(clippy::too_many_arguments)]
fn supersede_context_request(
    repo_path: &str,
    annotation_id: &str,
    path: Option<&str>,
    target_state_id: Option<&str>,
    scope: AnnotationScope,
    tags: Vec<String>,
    content: &str,
    agent_provider: Option<&str>,
    agent_model: Option<&str>,
    kind: ContextAnnotationKind,
    operation_id: &ClientOperationId,
) -> SupersedeContextRequest {
    SupersedeContextRequest {
        repo_path: super::helpers::repository_ref(repo_path),
        annotation_id: annotation_id.to_string(),
        path: path.unwrap_or_default().to_string(),
        scope: Some(scope),
        tags,
        content: content.to_string(),
        agent_provider: agent_provider.unwrap_or_default().to_string(),
        agent_model: agent_model.unwrap_or_default().to_string(),
        target_state_id: target_state_id
            .and_then(|s| objects::object::StateId::parse(s).ok())
            .and_then(super::helpers::proto_state_id),
        kind: kind as i32,
        client_operation_id: operation_id.to_wire(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_revision_retry_request_reuses_the_callers_operation_id() {
        let operation_id =
            ClientOperationId::for_required_method(REVISE_CONTEXT, "caller-op-1").unwrap();
        let build = || {
            revise_context_request(
                "acme/widgets",
                "annotation-1",
                "updated context",
                vec!["reviewed".to_string()],
                None,
                None,
                ContextAnnotationKind::Rationale,
                &operation_id,
            )
        };
        let first = build();
        let retry = build();
        assert!(!first.client_operation_id.is_empty());
        assert_eq!(retry.client_operation_id, first.client_operation_id);
    }

    #[test]
    fn context_supersession_retry_request_reuses_the_callers_operation_id() {
        let operation_id =
            ClientOperationId::for_required_method(SUPERSEDE_CONTEXT, "caller-op-2").unwrap();
        let build = || {
            supersede_context_request(
                "acme/widgets",
                "annotation-1",
                Some("src/lib.rs"),
                None,
                AnnotationScope::default(),
                vec!["replacement".to_string()],
                "replacement context",
                None,
                None,
                ContextAnnotationKind::Rationale,
                &operation_id,
            )
        };
        let first = build();
        let retry = build();
        assert!(!first.client_operation_id.is_empty());
        assert_eq!(retry.client_operation_id, first.client_operation_id);
    }

    #[test]
    fn context_write_rejects_an_empty_caller_operation_id_before_transport() {
        for method in [SET_CONTEXT, REVISE_CONTEXT, SUPERSEDE_CONTEXT] {
            let error = ClientOperationId::for_required_method(method, "")
                .expect_err("required context writes must fail before transport");
            assert!(error.to_string().contains("non-empty client operation ID"));
        }
    }

    #[test]
    fn actual_context_rpc_retry_boundary_requires_reusing_the_caller_id() {
        #[allow(dead_code)]
        async fn compile_caller_retry(client: &mut HostedClient, client_operation_id: String) {
            let _ = client
                .revise_context(
                    "acme/widgets",
                    "annotation-1",
                    "updated context",
                    Vec::new(),
                    None,
                    None,
                    ContextAnnotationKind::Rationale,
                    client_operation_id.clone(),
                )
                .await;
            let _ = client
                .revise_context(
                    "acme/widgets",
                    "annotation-1",
                    "updated context",
                    Vec::new(),
                    None,
                    None,
                    ContextAnnotationKind::Rationale,
                    client_operation_id,
                )
                .await;
        }

        let _ = compile_caller_retry;
    }
}
