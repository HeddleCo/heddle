// SPDX-License-Identifier: Apache-2.0
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReviewError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("missing configuration: {0}")]
    MissingConfiguration(String),
    #[error("github request failed: {0}")]
    Github(String),
    #[error("github rate limit exceeded")]
    GithubRateLimited,
    #[error("review store failure: {0}")]
    Store(String),
    #[error("serialization failure: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, ReviewError>;
