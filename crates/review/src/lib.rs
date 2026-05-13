// SPDX-License-Identifier: Apache-2.0
//! Review analysis domain and GitHub-backed pipeline.

pub mod errors;
pub mod github;
pub mod pipeline;
pub mod store;
pub mod types;

pub use errors::{Result, ReviewError};
pub use pipeline::ReviewPipeline;
pub use types::{
    PrAuthor, PrLabel, PrMetadata, ReviewAnalysisRequest, ReviewAnalysisResult, ReviewComment,
    ReviewCommentAuthor, ReviewContributor, ReviewFileArtifact, ReviewJobKey, ReviewJobPhase,
    ReviewJobRecord, ReviewJobStatus, ReviewNoiseFile, ReviewPacket, ReviewSemanticChange,
    ReviewStatus, ReviewStatusSnapshot,
};