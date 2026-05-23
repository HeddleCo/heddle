// SPDX-License-Identifier: Apache-2.0
use async_trait::async_trait;
use uuid::Uuid;

use crate::{
    Result,
    types::{
        PrMetadata, ReviewAnalysisRequest, ReviewAnalysisResult, ReviewJobKey, ReviewJobPhase,
        ReviewJobRecord, ReviewJobStatus, ReviewStatusSnapshot,
    },
};

#[derive(Debug, Clone)]
pub struct ReviewProgressUpdate {
    pub status: ReviewJobStatus,
    pub phase: Option<ReviewJobPhase>,
    pub current: Option<u32>,
    pub total: Option<u32>,
    pub label: Option<String>,
    pub error_message: Option<String>,
    pub head_sha: Option<String>,
}

impl ReviewProgressUpdate {
    /// Progress update for the analyzer's `Fetch` phase — kicks the
    /// counters at zero before any work has happened.
    pub fn fetching(label: impl Into<String>) -> Self {
        Self {
            status: ReviewJobStatus::Analyzing,
            phase: Some(ReviewJobPhase::Fetch),
            current: Some(0),
            total: Some(0),
            label: Some(label.into()),
            error_message: None,
            head_sha: None,
        }
    }

    /// Progress update for the analyzer's `Finalize` phase — current ==
    /// total once every file is classified.
    pub fn finalizing(files: u32, head_sha: impl Into<String>, label: impl Into<String>) -> Self {
        let head = head_sha.into();
        Self {
            status: ReviewJobStatus::Analyzing,
            phase: Some(ReviewJobPhase::Finalize),
            current: Some(files),
            total: Some(files),
            label: Some(label.into()),
            error_message: None,
            head_sha: Some(head),
        }
    }

    /// Terminal failure update; carries the error message so the UI can
    /// render it without a separate lookup.
    pub fn failed(
        error: impl Into<String>,
        head_sha: Option<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            status: ReviewJobStatus::Failed,
            phase: Some(ReviewJobPhase::Failed),
            current: None,
            total: None,
            label: Some(label.into()),
            error_message: Some(error.into()),
            head_sha,
        }
    }
}

#[async_trait]
pub trait ReviewJobStore: Send + Sync {
    async fn create_job(&self, request: &ReviewAnalysisRequest) -> Result<ReviewJobRecord>;
    async fn latest_job(&self, key: &ReviewJobKey) -> Result<Option<ReviewJobRecord>>;
    async fn job_by_id(&self, job_id: Uuid) -> Result<Option<ReviewJobRecord>>;
    async fn status_by_selector(
        &self,
        job_id: Option<Uuid>,
        key: Option<&ReviewJobKey>,
    ) -> Result<Option<ReviewStatusSnapshot>>;
    async fn result_by_selector(
        &self,
        job_id: Option<Uuid>,
        key: Option<&ReviewJobKey>,
    ) -> Result<Option<ReviewAnalysisResult>>;
    async fn attach_metadata(
        &self,
        job_id: Uuid,
        head_sha: &str,
        metadata: &PrMetadata,
    ) -> Result<()>;
    async fn update_progress(&self, job_id: Uuid, update: ReviewProgressUpdate) -> Result<()>;
    async fn complete_job(&self, job_id: Uuid, result: &ReviewAnalysisResult) -> Result<()>;
}
