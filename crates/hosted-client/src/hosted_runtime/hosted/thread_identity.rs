use api::heddle::api::v1alpha1::{ListThreadsRequest, ThreadOrder, list_threads_response::Frame};
use wire::ProtocolError;

use super::{HostedClient, helpers::hosted_to_protocol_error};

impl HostedClient {
    pub(super) async fn require_thread_id(
        &self,
        repo_path: &str,
        thread_name: &str,
    ) -> Result<String, ProtocolError> {
        let mut page_token = String::new();
        loop {
            let request = ListThreadsRequest {
                repo_path: super::helpers::repository_ref(repo_path),
                page_size: api::MAX_PAGE_SIZE,
                page_token: page_token.clone(),
                states: Vec::new(),
                query: thread_name.to_string(),
                order: ThreadOrder::NameAsc as i32,
            };
            let mut stream = self
                .routes()
                .list_threads(&request)
                .await
                .map_err(hosted_to_protocol_error)?;
            let mut found = None;
            let mut next_page_token = None;
            while let Some(response) = stream.next().await.map_err(hosted_to_protocol_error)? {
                match response.frame {
                    Some(Frame::Item(item)) if item.name == thread_name => {
                        if item.thread_id.is_empty() {
                            return Err(ProtocolError::InvalidState(format!(
                                "hosted thread '{thread_name}' is missing its stable identity"
                            )));
                        }
                        match found.as_deref() {
                            Some(existing) if existing != item.thread_id => {
                                return Err(ProtocolError::InvalidState(format!(
                                    "hosted thread name '{thread_name}' resolves to multiple identities"
                                )));
                            }
                            None => found = Some(item.thread_id),
                            _ => {}
                        }
                    }
                    Some(Frame::Item(_)) => {}
                    Some(Frame::PageEnd(page_end)) => {
                        next_page_token = Some(page_end.next_page_token);
                    }
                    None => {
                        return Err(ProtocolError::InvalidState(
                            "ListThreads emitted an empty frame".to_string(),
                        ));
                    }
                }
            }
            let next_page_token = next_page_token.ok_or_else(|| {
                ProtocolError::InvalidState(
                    "ListThreads ended without a terminal page frame".to_string(),
                )
            })?;
            if let Some(thread_id) = found {
                return Ok(thread_id);
            }
            if next_page_token.is_empty() {
                return Err(ProtocolError::ObjectNotFound(format!(
                    "hosted thread '{thread_name}'"
                )));
            }
            if next_page_token == page_token {
                return Err(ProtocolError::InvalidState(
                    "ListThreads returned a repeated page token".to_string(),
                ));
            }
            page_token = next_page_token;
        }
    }
}
