// SPDX-License-Identifier: Apache-2.0
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::{
    Result,
    errors::ReviewError,
    types::{
        PrAuthor, PrLabel, PrMetadata, ReviewComment, ReviewCommentAuthor, ReviewContributor,
        ReviewJobKey,
    },
};

const GITHUB_API: &str = "https://api.github.com";

#[derive(Debug, Clone)]
pub struct GitHubRestClient {
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct GitHubPullRequestData {
    pub metadata: PrMetadata,
    pub files: Vec<GitHubPullRequestFile>,
    pub comments: Vec<ReviewComment>,
    pub contributors: Vec<ReviewContributor>,
}

#[derive(Debug, Clone)]
pub struct GitHubPullRequestFile {
    pub filename: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub patch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PullRequestUser {
    login: String,
    avatar_url: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestLabel {
    name: String,
    color: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestRef {
    #[serde(default)]
    sha: String,
    #[serde(default)]
    r#ref: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestResponse {
    title: String,
    body: Option<String>,
    state: String,
    draft: Option<bool>,
    changed_files: u32,
    additions: u32,
    deletions: u32,
    head: PullRequestRef,
    base: PullRequestRef,
    user: Option<PullRequestUser>,
    #[serde(default)]
    labels: Vec<PullRequestLabel>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    merged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct PullRequestFileResponse {
    filename: String,
    status: String,
    additions: u32,
    deletions: u32,
    patch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommitAuthorInfo {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommitEnvelope {
    author: Option<CommitAuthorInfo>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommitUser {
    login: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PullRequestCommitResponse {
    commit: CommitEnvelope,
    author: Option<CommitUser>,
}

#[derive(Debug, Deserialize)]
struct IssueCommentResponse {
    user: Option<PullRequestUser>,
    body: String,
    created_at: Option<DateTime<Utc>>,
}

impl GitHubRestClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("heddle-review")
                .build()
                .map_err(|err| ReviewError::Github(err.to_string()))?,
        })
    }

    fn request(&self, token: Option<&str>, url: String) -> reqwest::RequestBuilder {
        let builder = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = token {
            builder.header("Authorization", format!("Bearer {token}"))
        } else {
            builder
        }
    }

    pub async fn fetch_pull_request(
        &self,
        key: &ReviewJobKey,
        token: Option<&str>,
    ) -> Result<GitHubPullRequestData> {
        let metadata = self.fetch_metadata(key, token).await?;
        let files = self.fetch_files(key, token).await?;
        let contributors = self.fetch_contributors(key, token).await?;
        let comments = self.fetch_comments(key, token).await?;
        Ok(GitHubPullRequestData {
            metadata,
            files,
            comments,
            contributors,
        })
    }

    async fn fetch_metadata(&self, key: &ReviewJobKey, token: Option<&str>) -> Result<PrMetadata> {
        let response = self
            .request(
                token,
                format!(
                    "{GITHUB_API}/repos/{}/{}/pulls/{}",
                    key.owner, key.repo, key.pr_number
                ),
            )
            .send()
            .await
            .map_err(|err| ReviewError::Github(err.to_string()))?;
        if response.status() == reqwest::StatusCode::FORBIDDEN
            && response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|value| value.to_str().ok())
                == Some("0")
        {
            return Err(ReviewError::GithubRateLimited);
        }
        if !response.status().is_success() {
            return Err(ReviewError::Github(format!(
                "pull request fetch failed with {}",
                response.status()
            )));
        }
        let pr: PullRequestResponse = response
            .json()
            .await
            .map_err(|err| ReviewError::Serialization(err.to_string()))?;
        Ok(PrMetadata {
            title: pr.title,
            body: pr.body,
            state: pr.state,
            draft: pr.draft.unwrap_or(false),
            changed_files: pr.changed_files,
            additions: pr.additions,
            deletions: pr.deletions,
            head_sha: pr.head.sha,
            base_branch: pr.base.r#ref,
            head_branch: pr.head.r#ref,
            author: pr.user.map(|user| PrAuthor {
                login: user.login,
                avatar_url: user.avatar_url,
            }),
            labels: pr
                .labels
                .into_iter()
                .map(|label| PrLabel {
                    name: label.name,
                    color: label.color,
                })
                .collect(),
            created_at: pr.created_at,
            updated_at: pr.updated_at,
            merged_at: pr.merged_at,
        })
    }

    async fn fetch_files(
        &self,
        key: &ReviewJobKey,
        token: Option<&str>,
    ) -> Result<Vec<GitHubPullRequestFile>> {
        let mut url = Some(format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}/files?per_page=100",
            key.owner, key.repo, key.pr_number
        ));
        let mut files = Vec::new();
        while let Some(next_url) = url.take() {
            let response = self
                .request(token, next_url)
                .send()
                .await
                .map_err(|err| ReviewError::Github(err.to_string()))?;
            if !response.status().is_success() {
                return Err(ReviewError::Github(format!(
                    "pull request files fetch failed with {}",
                    response.status()
                )));
            }
            let link_header = response
                .headers()
                .get("link")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let page: Vec<PullRequestFileResponse> = response
                .json()
                .await
                .map_err(|err| ReviewError::Serialization(err.to_string()))?;
            files.extend(page.into_iter().map(|file| GitHubPullRequestFile {
                filename: file.filename,
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                patch: file.patch,
            }));
            url = link_header.and_then(parse_next_link);
        }
        Ok(files)
    }

    async fn fetch_contributors(
        &self,
        key: &ReviewJobKey,
        token: Option<&str>,
    ) -> Result<Vec<ReviewContributor>> {
        let mut seen = std::collections::BTreeMap::<String, ReviewContributor>::new();
        let mut seen_names = std::collections::BTreeSet::<String>::new();
        let mut url = Some(format!(
            "{GITHUB_API}/repos/{}/{}/pulls/{}/commits?per_page=100",
            key.owner, key.repo, key.pr_number
        ));

        while let Some(next_url) = url.take() {
            let response = self
                .request(token, next_url)
                .send()
                .await
                .map_err(|err| ReviewError::Github(err.to_string()))?;
            if !response.status().is_success() {
                return Ok(seen.into_values().collect());
            }
            let link_header = response
                .headers()
                .get("link")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let commits: Vec<PullRequestCommitResponse> = response
                .json()
                .await
                .map_err(|err| ReviewError::Serialization(err.to_string()))?;

            for commit in commits {
                let author_name = commit
                    .author
                    .as_ref()
                    .and_then(|author| author.login.clone())
                    .or_else(|| {
                        commit
                            .commit
                            .author
                            .as_ref()
                            .and_then(|author| author.name.clone())
                    })
                    .unwrap_or_else(|| "unknown".to_string());
                let author_email = commit
                    .commit
                    .author
                    .as_ref()
                    .and_then(|author| author.email.clone())
                    .unwrap_or_default();
                insert_contributor(&mut seen, &mut seen_names, &author_name, &author_email);

                if let Some(message) = commit.commit.message {
                    for raw in co_authored_lines(&message) {
                        let (name, email) = parse_name_email(&raw);
                        insert_contributor(&mut seen, &mut seen_names, &name, &email);
                    }
                }
            }

            url = link_header.and_then(parse_next_link);
        }

        Ok(seen.into_values().collect())
    }

    async fn fetch_comments(
        &self,
        key: &ReviewJobKey,
        token: Option<&str>,
    ) -> Result<Vec<ReviewComment>> {
        let mut comments = self
            .fetch_comment_pages(
                format!(
                    "{GITHUB_API}/repos/{}/{}/issues/{}/comments?per_page=100",
                    key.owner, key.repo, key.pr_number
                ),
                false,
                token,
            )
            .await?;
        comments.extend(
            self.fetch_comment_pages(
                format!(
                    "{GITHUB_API}/repos/{}/{}/pulls/{}/comments?per_page=100",
                    key.owner, key.repo, key.pr_number
                ),
                true,
                token,
            )
            .await?,
        );
        comments.sort_by_key(|comment| comment.created_at);
        Ok(comments)
    }

    async fn fetch_comment_pages(
        &self,
        first_url: String,
        is_review_comment: bool,
        token: Option<&str>,
    ) -> Result<Vec<ReviewComment>> {
        let mut comments = Vec::new();
        let mut url = Some(first_url);

        while let Some(next_url) = url.take() {
            let response = self
                .request(token, next_url)
                .send()
                .await
                .map_err(|err| ReviewError::Github(err.to_string()))?;
            if !response.status().is_success() {
                break;
            }
            let link_header = response
                .headers()
                .get("link")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let page: Vec<IssueCommentResponse> = response
                .json()
                .await
                .map_err(|err| ReviewError::Serialization(err.to_string()))?;
            comments.extend(page.into_iter().map(|comment| ReviewComment {
                author: comment.user.map(|user| ReviewCommentAuthor {
                    login: user.login,
                    avatar_url: user.avatar_url,
                }),
                body: comment.body,
                created_at: comment.created_at,
                is_review_comment,
            }));
            url = link_header.and_then(parse_next_link);
        }

        Ok(comments)
    }
}

fn parse_next_link(link: String) -> Option<String> {
    link.split(',').find_map(|part| {
        let trimmed = part.trim();
        trimmed
            .strip_suffix("rel=\"next\"")
            .and_then(|value| value.split('<').nth(1))
            .and_then(|value| value.split('>').next())
            .map(ToOwned::to_owned)
    })
}

fn is_agent_author(name: &str, email: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let email = email.to_ascii_lowercase();
    name.ends_with("[bot]")
        || name.starts_with("dependabot")
        || name.starts_with("renovate")
        || name.starts_with("github-actions")
        || name.starts_with("copilot")
        || email.ends_with("noreply@github.com")
        || email.ends_with("noreply@anthropic.com")
        || email.ends_with("noreply@openai.com")
        || email.starts_with("devin@cognition")
}

fn insert_contributor(
    seen: &mut std::collections::BTreeMap<String, ReviewContributor>,
    seen_names: &mut std::collections::BTreeSet<String>,
    name: &str,
    email: &str,
) {
    let key = if email.is_empty() { name } else { email }.to_ascii_lowercase();
    if seen.contains_key(&key) {
        return;
    }
    let name_key = name.to_ascii_lowercase();
    if !name.is_empty() && seen_names.contains(&name_key) {
        return;
    }
    seen.insert(
        key,
        ReviewContributor {
            name: name.to_string(),
            email: email.to_string(),
            is_agent: is_agent_author(name, email),
        },
    );
    if !name.is_empty() {
        seen_names.insert(name_key);
    }
}

fn co_authored_lines(message: &str) -> Vec<String> {
    message
        .lines()
        .filter_map(|line| line.strip_prefix("Co-Authored-By:"))
        .map(|line| line.trim().to_string())
        .collect()
}

fn parse_name_email(raw: &str) -> (String, String) {
    if let Some((name, email)) = raw.split_once('<') {
        return (
            name.trim().to_string(),
            email.trim_end_matches('>').trim().to_string(),
        );
    }
    (raw.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_next_link_extracts_next_url_from_link_header() {
        let next = parse_next_link(
            r#"<https://api.github.com/resource?page=2>; rel="next", <https://api.github.com/resource?page=4>; rel="last""#
                .to_string(),
        );
        assert_eq!(
            next.as_deref(),
            Some("https://api.github.com/resource?page=2")
        );
    }

    #[test]
    fn contributor_dedupes_by_email_and_name() {
        let mut seen = std::collections::BTreeMap::new();
        let mut seen_names = std::collections::BTreeSet::new();
        insert_contributor(&mut seen, &mut seen_names, "Alice", "alice@example.com");
        insert_contributor(&mut seen, &mut seen_names, "Alice", "alice+2@example.com");
        insert_contributor(&mut seen, &mut seen_names, "Bob", "alice@example.com");
        assert_eq!(seen.len(), 1);
        let contributor = seen.values().next().unwrap();
        assert_eq!(contributor.name, "Alice");
        assert_eq!(contributor.email, "alice@example.com");
    }

    #[test]
    fn parses_co_authored_lines_and_agent_detection() {
        let lines = co_authored_lines(
            "subject\n\nCo-Authored-By: dependabot[bot] <bot@noreply.github.com>\nCo-Authored-By: Alice <alice@example.com>",
        );
        assert_eq!(lines.len(), 2);
        let first = parse_name_email(&lines[0]);
        let second = parse_name_email(&lines[1]);
        assert_eq!(first.0, "dependabot[bot]");
        assert_eq!(first.1, "bot@noreply.github.com");
        assert!(is_agent_author(&first.0, &first.1));
        assert!(!is_agent_author(&second.0, &second.1));
    }
}
