// SPDX-License-Identifier: Apache-2.0
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewJobKey {
    pub provider: String,
    pub owner: String,
    pub repo: String,
    pub pr_number: u32,
}

impl ReviewJobKey {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewAnalysisRequest {
    pub provider: String,
    pub owner: String,
    pub repo: String,
    pub pr_number: u32,
    pub force_refresh: bool,
}

impl ReviewAnalysisRequest {
    pub fn key(&self) -> ReviewJobKey {
        ReviewJobKey {
            provider: self.provider.clone(),
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            pr_number: self.pr_number,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewJobStatus {
    Pending,
    Analyzing,
    Posted,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewJobPhase {
    Fetch,
    Highlight,
    Finalize,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewStatus {
    pub status: ReviewJobStatus,
    pub phase: Option<ReviewJobPhase>,
    pub current: u32,
    pub total: u32,
    pub label: String,
    pub updated_at: Option<DateTime<Utc>>,
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewStatusSnapshot {
    pub job_id: Uuid,
    pub key: ReviewJobKey,
    pub head_sha: Option<String>,
    pub status: ReviewStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrAuthor {
    pub login: String,
    pub avatar_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrLabel {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrMetadata {
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub draft: bool,
    pub changed_files: u32,
    pub additions: u32,
    pub deletions: u32,
    pub head_sha: String,
    pub base_branch: String,
    pub head_branch: String,
    pub author: Option<PrAuthor>,
    pub labels: Vec<PrLabel>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub merged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewContributor {
    pub name: String,
    pub email: String,
    pub is_agent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewCommentAuthor {
    pub login: String,
    pub avatar_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewComment {
    pub author: Option<ReviewCommentAuthor>,
    pub body: String,
    pub created_at: Option<DateTime<Utc>>,
    pub is_review_comment: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewSemanticChange {
    pub change_type: String,
    pub description: String,
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub impact: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewNoiseFile {
    pub path: String,
    pub reason: String,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewPacket {
    pub files_changed: u32,
    pub semantic_changes: u32,
    pub noise_filtered: u32,
    pub changes: Vec<ReviewSemanticChange>,
    pub noise: Vec<ReviewNoiseFile>,
    pub contributors: Vec<ReviewContributor>,
    pub agents: u32,
    pub humans: u32,
    pub narrative: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewFileArtifact {
    pub index: u32,
    pub change_type: String,
    pub description: String,
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub impact: u32,
    pub patch: Option<String>,
    pub truncated: bool,
    pub highlights: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewAnalysisResult {
    pub job_id: Uuid,
    pub key: ReviewJobKey,
    pub head_sha: String,
    pub metadata: PrMetadata,
    pub packet: ReviewPacket,
    pub comments: Vec<ReviewComment>,
    pub files: Vec<ReviewFileArtifact>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewJobRecord {
    pub job_id: Uuid,
    pub key: ReviewJobKey,
    pub head_sha: Option<String>,
    pub status: ReviewJobStatus,
    pub phase: Option<ReviewJobPhase>,
    pub progress_current: u32,
    pub progress_total: u32,
    pub progress_label: Option<String>,
    pub error_message: Option<String>,
    pub metadata: Option<PrMetadata>,
    pub result: Option<ReviewAnalysisResult>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}
