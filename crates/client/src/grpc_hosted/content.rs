use grpc::heddle::v1::{
    AnnotationScope, CompareResponse, ContextAnnotationKind, GetBlameRequest, GetBlameResponse,
    GetCompareRequest, GetContextHistoryRequest, GetContextHistoryResponse, ListContextRequest,
    ListContextResponse, ListContextSuggestionsRequest, ListContextSuggestionsResponse,
    ReviseContextRequest, ReviseContextResponse, SetContextRequest, SetContextResponse,
    SupersedeContextRequest, SupersedeContextResponse,
};
use tonic::Request;
use wire::ProtocolError;

use super::{HostedGrpcClient, helpers::status_to_protocol_error};

impl HostedGrpcClient {
    pub async fn get_compare(
        &mut self,
        repo_path: &str,
        from: &str,
        to: &str,
        include_semantic: bool,
    ) -> Result<CompareResponse, ProtocolError> {
        let mut request = Request::new(GetCompareRequest {
            repo_path: repo_path.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            include_semantic,
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.ContentService/GetCompare")?;
        self.content
            .get_compare(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
    }

    pub async fn get_blame(
        &mut self,
        repo_path: &str,
        r#ref: Option<&str>,
        path: &str,
    ) -> Result<GetBlameResponse, ProtocolError> {
        let mut request = Request::new(GetBlameRequest {
            repo_path: repo_path.to_string(),
            r#ref: r#ref.unwrap_or_default().to_string(),
            path: path.to_string(),
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.ContentService/GetBlame")?;
        self.content
            .get_blame(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
    }

    pub async fn list_context(
        &mut self,
        repo_path: &str,
        r#ref: Option<&str>,
        prefix: Option<&str>,
        tag_filter: Option<&str>,
    ) -> Result<ListContextResponse, ProtocolError> {
        let mut request = Request::new(ListContextRequest {
            repo_path: repo_path.to_string(),
            r#ref: r#ref.unwrap_or_default().to_string(),
            prefix: prefix.map(str::to_string),
            tag_filter: tag_filter.map(str::to_string),
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.ContentService/ListContext")?;
        self.content
            .list_context(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
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
    ) -> Result<SetContextResponse, ProtocolError> {
        let mut request = Request::new(SetContextRequest {
            repo_path: repo_path.to_string(),
            path: path.to_string(),
            scope: Some(scope),
            tags,
            content: content.to_string(),
            agent_provider: agent_provider.unwrap_or_default().to_string(),
            agent_model: agent_model.unwrap_or_default().to_string(),
            target_state_id: target_state_id
                .and_then(|s| objects::object::ChangeId::parse(s).ok())
                .map(|id| id.as_bytes().to_vec()),
            kind: kind as i32,
            client_operation_id: String::new(),
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.ContentService/SetContext")?;
        self.content
            .set_context(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
    }

    pub async fn get_context_history(
        &mut self,
        repo_path: &str,
        r#ref: Option<&str>,
        annotation_id: &str,
    ) -> Result<GetContextHistoryResponse, ProtocolError> {
        let mut request = Request::new(GetContextHistoryRequest {
            repo_path: repo_path.to_string(),
            r#ref: r#ref.unwrap_or_default().to_string(),
            annotation_id: annotation_id.to_string(),
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.ContentService/GetContextHistory")?;
        self.content
            .get_context_history(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
    }

    pub async fn list_context_suggestions(
        &mut self,
        repo_path: &str,
        r#ref: Option<&str>,
        limit: u32,
    ) -> Result<ListContextSuggestionsResponse, ProtocolError> {
        let mut request = Request::new(ListContextSuggestionsRequest {
            repo_path: repo_path.to_string(),
            r#ref: r#ref.unwrap_or_default().to_string(),
            limit,
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.ContentService/ListContextSuggestions")?;
        self.content
            .list_context_suggestions(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
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
    ) -> Result<ReviseContextResponse, ProtocolError> {
        let mut request = Request::new(ReviseContextRequest {
            repo_path: repo_path.to_string(),
            annotation_id: annotation_id.to_string(),
            content: content.to_string(),
            tags,
            agent_provider: agent_provider.unwrap_or_default().to_string(),
            agent_model: agent_model.unwrap_or_default().to_string(),
            kind: kind as i32,
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.ContentService/ReviseContext")?;
        self.content
            .revise_context(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
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
    ) -> Result<SupersedeContextResponse, ProtocolError> {
        let mut request = Request::new(SupersedeContextRequest {
            repo_path: repo_path.to_string(),
            annotation_id: annotation_id.to_string(),
            path: path.unwrap_or_default().to_string(),
            scope: Some(scope),
            tags,
            content: content.to_string(),
            agent_provider: agent_provider.unwrap_or_default().to_string(),
            agent_model: agent_model.unwrap_or_default().to_string(),
            target_state_id: target_state_id
                .and_then(|s| objects::object::ChangeId::parse(s).ok())
                .map(|id| id.as_bytes().to_vec()),
            kind: kind as i32,
        });
        self.apply_signed_auth(&mut request, "/heddle.v1.ContentService/SupersedeContext")?;
        self.content
            .supersede_context(request)
            .await
            .map_err(status_to_protocol_error)
            .map(|response| response.into_inner())
    }
}
