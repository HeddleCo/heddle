// SPDX-License-Identifier: Apache-2.0
//! Generated gRPC surface for the hosted transport rewrite.

pub mod heddle {
    pub mod v1 {
        #![allow(clippy::large_enum_variant)]
        tonic::include_proto!("heddle.v1");
    }
}

pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("heddle_descriptor");

pub use heddle::v1::{
    auth_service_server::AuthServiceServer, content_service_server::ContentServiceServer,
    discussion_service_server::DiscussionServiceServer, feed_service_server::FeedServiceServer,
    hook_service_server::HookServiceServer,
    operation_log_query_service_server::OperationLogQueryServiceServer,
    repo_event_service_server::RepoEventServiceServer,
    repo_sync_service_server::RepoSyncServiceServer, review_service_server::ReviewServiceServer,
    signal_service_server::SignalServiceServer,
    state_review_service_server::StateReviewServiceServer,
    thread_workflow_service_server::ThreadWorkflowServiceServer,
    timeline_service_server::TimelineServiceServer,
    transaction_service_server::TransactionServiceServer,
};
