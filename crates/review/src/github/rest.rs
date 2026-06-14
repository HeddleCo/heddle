// SPDX-License-Identifier: Apache-2.0
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{
    Result,
    errors::ReviewError,
    types::{
        PrAuthor, PrLabel, PrMetadata, ReviewComment, ReviewCommentAuthor, ReviewContributor,
        ReviewJobKey,
    },
};

const GITHUB_API: &str = "https://api.github.com";
const GITHUB_PER_PAGE: usize = 100;

#[derive(Debug, Clone)]
pub struct GitHubRestClient {
    http: reqwest::Client,
    api_base_url: String,
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
        Self::new_with_api_base(GITHUB_API.to_string())
    }

    fn new_with_api_base(api_base_url: String) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("heddle-review")
                // Never follow redirects: GitHub REST pagination returns
                // `200`+`Link`, never `3xx`. Following a server-controlled
                // `Location` would bypass the `validate_pagination_origin`
                // Link-header check and re-open the SSRF gap it closes (#521).
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|err| ReviewError::Github(err.to_string()))?,
            api_base_url,
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
                    "{}/repos/{}/{}/pulls/{}",
                    self.api_base_url, key.owner, key.repo, key.pr_number
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
        let files: Vec<PullRequestFileResponse> = self
            .fetch_paginated_json(
                format!(
                    "{}/repos/{}/{}/pulls/{}/files?per_page={GITHUB_PER_PAGE}",
                    self.api_base_url, key.owner, key.repo, key.pr_number
                ),
                token,
                "pull request files",
            )
            .await?;
        Ok(files
            .into_iter()
            .map(|file| GitHubPullRequestFile {
                filename: file.filename,
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                patch: file.patch,
            })
            .collect())
    }

    async fn fetch_contributors(
        &self,
        key: &ReviewJobKey,
        token: Option<&str>,
    ) -> Result<Vec<ReviewContributor>> {
        let mut seen = std::collections::BTreeMap::<String, ReviewContributor>::new();
        let mut seen_names = std::collections::BTreeSet::<String>::new();
        let commits: Vec<PullRequestCommitResponse> = self
            .fetch_paginated_json(
                format!(
                    "{}/repos/{}/{}/pulls/{}/commits?per_page={GITHUB_PER_PAGE}",
                    self.api_base_url, key.owner, key.repo, key.pr_number
                ),
                token,
                "pull request commits",
            )
            .await?;

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
                    "{}/repos/{}/{}/issues/{}/comments?per_page={GITHUB_PER_PAGE}",
                    self.api_base_url, key.owner, key.repo, key.pr_number
                ),
                false,
                token,
            )
            .await?;
        comments.extend(
            self.fetch_comment_pages(
                format!(
                    "{}/repos/{}/{}/pulls/{}/comments?per_page={GITHUB_PER_PAGE}",
                    self.api_base_url, key.owner, key.repo, key.pr_number
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
        let comments: Vec<IssueCommentResponse> = self
            .fetch_paginated_json(first_url, token, "pull request comments")
            .await?;
        Ok(comments
            .into_iter()
            .map(|comment| ReviewComment {
                author: comment.user.map(|user| ReviewCommentAuthor {
                    login: user.login,
                    avatar_url: user.avatar_url,
                }),
                body: comment.body,
                created_at: comment.created_at,
                is_review_comment,
            })
            .collect())
    }

    async fn fetch_paginated_json<T>(
        &self,
        first_url: String,
        token: Option<&str>,
        label: &str,
    ) -> Result<Vec<T>>
    where
        T: DeserializeOwned,
    {
        let mut items = Vec::new();
        let mut url = Some(first_url);

        while let Some(next_url) = url.take() {
            let response = self
                .request(token, next_url)
                .send()
                .await
                .map_err(|err| ReviewError::Github(err.to_string()))?;
            let status = response.status();
            if status == reqwest::StatusCode::FORBIDDEN
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
                    "{label} fetch failed with {}",
                    response.status()
                )));
            }
            let link_header = response
                .headers()
                .get("link")
                .map(|value| {
                    value
                        .to_str()
                        .map_err(|err| {
                            ReviewError::Github(format!(
                                "{label} pagination failed: malformed Link header: {err}"
                            ))
                        })
                        .map(ToOwned::to_owned)
                })
                .transpose()?;
            let page: Vec<T> = response
                .json()
                .await
                .map_err(|err| ReviewError::Serialization(err.to_string()))?;
            items.extend(page);
            let next = next_page_url(link_header.as_deref())?;
            if let Some(candidate) = next.as_deref() {
                validate_pagination_origin(&self.api_base_url, candidate)?;
            }
            url = next;
        }

        Ok(items)
    }
}

fn next_page_url(link: Option<&str>) -> Result<Option<String>> {
    let Some(link) = link else {
        return Ok(None);
    };
    parse_next_link(link)
}

fn parse_next_link(link: &str) -> Result<Option<String>> {
    let mut next = None;
    for part in link.split(',') {
        let trimmed = part.trim();
        let (url, attrs) = trimmed
            .split_once(';')
            .ok_or_else(|| ReviewError::Github("malformed Link header".to_string()))?;
        let url = url
            .trim()
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'))
            .ok_or_else(|| ReviewError::Github("malformed Link header".to_string()))?;
        let mut has_rel = false;
        for attr in attrs.split(';') {
            let attr = attr.trim();
            if attr.starts_with("rel=") {
                has_rel = true;
            }
            if attr == "rel=\"next\"" {
                next = Some(url.to_string());
            }
        }
        if !has_rel {
            return Err(ReviewError::Github("malformed Link header".to_string()));
        }
    }
    Ok(next)
}

/// Reject a server-provided pagination URL whose origin (scheme + host + port)
/// differs from the configured API base. GitHub's `Link` header is
/// server-controlled; without this check a spoofed or compromised endpoint
/// could point pagination at an internal address (SSRF-adjacent, #521).
fn validate_pagination_origin(base: &str, candidate: &str) -> Result<()> {
    let base_url = reqwest::Url::parse(base)
        .map_err(|err| ReviewError::Github(format!("invalid API base URL `{base}`: {err}")))?;
    let next_url = reqwest::Url::parse(candidate).map_err(|err| {
        ReviewError::Github(format!("malformed pagination URL `{candidate}`: {err}"))
    })?;
    let same_origin = next_url.scheme() == base_url.scheme()
        && next_url.host_str() == base_url.host_str()
        && next_url.port_or_known_default() == base_url.port_or_known_default();
    if !same_origin {
        return Err(ReviewError::Github(format!(
            "refusing to follow pagination URL to unexpected origin `{candidate}` \
             (expected the origin of `{base}`)"
        )));
    }
    Ok(())
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
    use serde_json::json;
    use std::collections::VecDeque;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    struct MockResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl MockResponse {
        fn json(status: u16, body: String) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body,
            }
        }

        fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
            self.headers.push((name.into(), value.into()));
            self
        }
    }

    async fn spawn_server(build_responses: impl FnOnce(&str) -> Vec<MockResponse>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let mut responses = VecDeque::from(build_responses(&base_url));
        tokio::spawn(async move {
            while let Some(response) = responses.pop_front() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }

                let mut header = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
                    response.status,
                    response.body.len()
                );
                for (name, value) in response.headers {
                    header.push_str(&format!("{name}: {value}\r\n"));
                }
                header.push_str("\r\n");
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(response.body.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        base_url
    }

    fn review_key() -> ReviewJobKey {
        ReviewJobKey {
            provider: "github".to_string(),
            owner: "heddle".to_string(),
            repo: "repo".to_string(),
            pr_number: 439,
        }
    }

    fn file_page(start: usize, count: usize) -> String {
        let files: Vec<_> = (start..start + count)
            .map(|index| {
                json!({
                    "filename": format!("file-{index}.rs"),
                    "status": "modified",
                    "additions": 1,
                    "deletions": 0,
                    "patch": null
                })
            })
            .collect();
        serde_json::to_string(&files).unwrap()
    }

    fn commit_page(start: usize, count: usize) -> String {
        let commits: Vec<_> = (start..start + count)
            .map(|index| {
                json!({
                    "commit": {
                        "author": {
                            "name": format!("Author {index}"),
                            "email": format!("author-{index}@example.com")
                        },
                        "message": "subject"
                    },
                    "author": {
                        "login": format!("author-{index}")
                    }
                })
            })
            .collect();
        serde_json::to_string(&commits).unwrap()
    }

    fn comment_page(start: usize, count: usize) -> String {
        let comments: Vec<_> = (start..start + count)
            .map(|index| {
                json!({
                    "user": {
                        "login": format!("commenter-{index}"),
                        "avatar_url": "https://example.com/avatar.png"
                    },
                    "body": format!("comment {index}"),
                    "created_at": null
                })
            })
            .collect();
        serde_json::to_string(&comments).unwrap()
    }

    fn metadata_body() -> String {
        serde_json::to_string(&json!({
            "title": "Fix pagination",
            "body": "Fail loud",
            "state": "open",
            "draft": false,
            "changed_files": 1,
            "additions": 2,
            "deletions": 1,
            "head": {
                "sha": "abc123",
                "ref": "task/439"
            },
            "base": {
                "sha": "def456",
                "ref": "main"
            },
            "user": {
                "login": "reviewer",
                "avatar_url": "https://example.com/reviewer.png"
            },
            "labels": [
                {
                    "name": "bug",
                    "color": "d73a4a"
                }
            ],
            "created_at": null,
            "updated_at": null,
            "merged_at": null
        }))
        .unwrap()
    }

    fn client_for(base_url: &str) -> GitHubRestClient {
        GitHubRestClient::new_with_api_base(base_url.to_string()).unwrap()
    }

    #[test]
    fn parse_next_link_extracts_next_url_from_link_header() {
        let next = parse_next_link(
            r#"<https://api.github.com/resource?page=2>; rel="next", <https://api.github.com/resource?page=4>; rel="last""#
        )
        .unwrap();
        assert_eq!(
            next.as_deref(),
            Some("https://api.github.com/resource?page=2")
        );
    }

    #[test]
    fn parse_next_link_rejects_malformed_link_header() {
        let err = parse_next_link("https://api.github.com/resource?page=2; rel=\"next\"")
            .expect_err("malformed link header must fail");
        assert!(err.to_string().contains("malformed Link header"));
    }

    #[test]
    fn parse_next_link_rejects_link_without_rel_attribute() {
        let err = parse_next_link("<https://api.github.com/resource?page=2>; title=\"next\"")
            .expect_err("Link header entries without rel are malformed");
        assert!(err.to_string().contains("malformed Link header"));
    }

    #[test]
    fn validate_pagination_origin_accepts_same_origin() {
        validate_pagination_origin(
            "https://api.github.com",
            "https://api.github.com/repositories/1/pulls?per_page=100&page=2",
        )
        .expect("same-origin pagination URL must be accepted");
    }

    #[test]
    fn validate_pagination_origin_accepts_explicit_default_port() {
        validate_pagination_origin(
            "https://api.github.com",
            "https://api.github.com:443/resource?page=2",
        )
        .expect("an explicit default port is the same origin as the implicit one");
    }

    #[test]
    fn validate_pagination_origin_rejects_foreign_host() {
        let err = validate_pagination_origin(
            "https://api.github.com",
            "https://169.254.169.254/resource?page=2",
        )
        .expect_err("a foreign host must be rejected");
        assert!(err.to_string().contains("unexpected origin"));
    }

    #[test]
    fn validate_pagination_origin_rejects_scheme_downgrade() {
        let err = validate_pagination_origin(
            "https://api.github.com",
            "http://api.github.com/resource?page=2",
        )
        .expect_err("a scheme downgrade must be rejected");
        assert!(err.to_string().contains("unexpected origin"));
    }

    #[test]
    fn validate_pagination_origin_rejects_foreign_port() {
        let err = validate_pagination_origin(
            "https://api.github.com",
            "https://api.github.com:8443/resource?page=2",
        )
        .expect_err("a foreign port must be rejected");
        assert!(err.to_string().contains("unexpected origin"));
    }

    #[test]
    fn validate_pagination_origin_rejects_unparseable_candidate() {
        let err = validate_pagination_origin("https://api.github.com", "not a url")
            .expect_err("an unparseable pagination URL must be rejected");
        assert!(err.to_string().contains("malformed pagination URL"));
    }

    #[tokio::test]
    async fn fetch_pull_request_accepts_complete_data() {
        let base_url = spawn_server(|_| {
            vec![
                MockResponse::json(200, metadata_body()),
                MockResponse::json(200, file_page(0, 1)),
                MockResponse::json(200, commit_page(0, 1)),
                MockResponse::json(200, comment_page(0, 1)),
                MockResponse::json(200, comment_page(1, 1)),
            ]
        })
        .await;
        let data = client_for(&base_url)
            .fetch_pull_request(&review_key(), None)
            .await
            .unwrap();

        assert_eq!(data.metadata.title, "Fix pagination");
        assert_eq!(data.metadata.head_sha, "abc123");
        assert_eq!(data.files.len(), 1);
        assert_eq!(data.contributors.len(), 1);
        assert_eq!(data.comments.len(), 2);
        assert_eq!(
            data.comments
                .iter()
                .filter(|comment| comment.is_review_comment)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn fetch_files_accepts_single_page() {
        let base_url = spawn_server(|_| vec![MockResponse::json(200, file_page(0, 2))]).await;
        let files = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].filename, "file-0.rs");
    }

    #[tokio::test]
    async fn fetch_files_accepts_multi_page_complete_result() {
        let base_url = spawn_server(|base_url| {
            vec![
                MockResponse::json(200, file_page(0, GITHUB_PER_PAGE)).with_header(
                    "Link",
                    format!(
                        "<{base_url}/repos/heddle/repo/pulls/439/files?per_page={GITHUB_PER_PAGE}&page=2>; rel=\"next\""
                    ),
                ),
                MockResponse::json(200, file_page(GITHUB_PER_PAGE, 2)),
            ]
        })
        .await;
        let files = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .unwrap();
        assert_eq!(files.len(), GITHUB_PER_PAGE + 2);
        assert_eq!(files.last().unwrap().filename, "file-101.rs");
    }

    #[tokio::test]
    async fn fetch_files_accepts_genuinely_empty_page() {
        let base_url = spawn_server(|_| vec![MockResponse::json(200, "[]".to_string())]).await;
        let files = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .unwrap();
        assert!(files.is_empty());
    }

    #[tokio::test]
    async fn fetch_files_errors_on_mid_pagination_failure() {
        let base_url = spawn_server(|base_url| {
            vec![
                MockResponse::json(200, file_page(0, GITHUB_PER_PAGE)).with_header(
                    "Link",
                    format!(
                        "<{base_url}/repos/heddle/repo/pulls/439/files?per_page={GITHUB_PER_PAGE}&page=2>; rel=\"next\""
                    ),
                ),
                MockResponse::json(500, "[]".to_string()),
            ]
        })
        .await;
        let err = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .expect_err("mid-pagination failure must not return a partial file list");
        assert!(err.to_string().contains("pull request files fetch failed"));
    }

    #[tokio::test]
    async fn fetch_files_accepts_exact_full_page_without_link_header() {
        let base_url =
            spawn_server(|_| vec![MockResponse::json(200, file_page(0, GITHUB_PER_PAGE))]).await;
        let files = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .unwrap();
        assert_eq!(files.len(), GITHUB_PER_PAGE);
        assert_eq!(files.last().unwrap().filename, "file-99.rs");
    }

    #[tokio::test]
    async fn fetch_files_accepts_exact_full_page_terminal_link_without_next() {
        let base_url = spawn_server(|base_url| {
            vec![
                MockResponse::json(200, file_page(0, GITHUB_PER_PAGE)).with_header(
                    "Link",
                    format!(
                        "<{base_url}/repos/heddle/repo/pulls/439/files?per_page={GITHUB_PER_PAGE}&page=2>; rel=\"next\""
                    ),
                ),
                MockResponse::json(200, file_page(GITHUB_PER_PAGE, GITHUB_PER_PAGE)).with_header(
                    "Link",
                    format!(
                        "<{base_url}/repos/heddle/repo/pulls/439/files?per_page={GITHUB_PER_PAGE}>; rel=\"first\", \
                         <{base_url}/repos/heddle/repo/pulls/439/files?per_page={GITHUB_PER_PAGE}>; rel=\"prev\", \
                         <{base_url}/repos/heddle/repo/pulls/439/files?per_page={GITHUB_PER_PAGE}&page=2>; rel=\"last\""
                    ),
                ),
            ]
        })
        .await;
        let files = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .unwrap();
        assert_eq!(files.len(), GITHUB_PER_PAGE * 2);
        assert_eq!(files.last().unwrap().filename, "file-199.rs");
    }

    #[tokio::test]
    async fn fetch_files_errors_on_malformed_link_header() {
        let base_url = spawn_server(|_| {
            vec![
                MockResponse::json(200, file_page(0, GITHUB_PER_PAGE))
                    .with_header("Link", "not a link"),
            ]
        })
        .await;
        let err = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .expect_err("malformed Link header must not return a partial file list");
        assert!(err.to_string().contains("malformed Link header"));
    }

    #[tokio::test]
    async fn fetch_files_rejects_cross_origin_pagination_url() {
        let base_url = spawn_server(|_| {
            // Same host, but a port nothing answers on — a stand-in for an
            // attacker-controlled `next` redirecting pagination off-origin
            // (e.g. at a cloud metadata endpoint). Validation must refuse it
            // before any request is issued, so the error is an origin
            // rejection rather than a connection failure.
            vec![MockResponse::json(200, file_page(0, GITHUB_PER_PAGE)).with_header(
                "Link",
                "<http://127.0.0.1:1/repos/heddle/repo/pulls/439/files?per_page=100&page=2>; rel=\"next\"",
            )]
        })
        .await;
        let err = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .expect_err("cross-origin pagination URL must be rejected");
        assert!(
            err.to_string().contains("unexpected origin"),
            "expected an origin-rejection error, got: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_files_does_not_follow_redirect_to_internal_address() {
        // The client must not follow redirects: GitHub pagination is
        // `200`+`Link`, never `3xx`. A server-controlled `302` whose
        // `Location` points at an internal/dead address would bypass the
        // Link-header origin check entirely if reqwest auto-followed it
        // (the SSRF sibling of the cross-origin pagination case, #521).
        // With redirects disabled, the `302` surfaces as a non-success
        // status and the fetch fails loudly *without* ever connecting to
        // the dead port — proving the redirect was not chased.
        let base_url = spawn_server(|_| {
            vec![MockResponse::json(302, String::new()).with_header(
                "Location",
                "http://127.0.0.1:1/repos/heddle/repo/pulls/439/files?per_page=100&page=2",
            )]
        })
        .await;
        let err = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .expect_err("a redirect to an internal address must not be followed");
        assert!(
            err.to_string().contains("pull request files fetch failed"),
            "expected a non-success status error from the unfollowed 302, got: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_files_returns_rate_limit_error() {
        let base_url = spawn_server(|_| {
            vec![
                MockResponse::json(403, "[]".to_string()).with_header("x-ratelimit-remaining", "0"),
            ]
        })
        .await;
        let err = client_for(&base_url)
            .fetch_files(&review_key(), None)
            .await
            .expect_err("rate-limited pagination must fail loudly");
        assert!(matches!(err, ReviewError::GithubRateLimited));
    }

    #[tokio::test]
    async fn fetch_contributors_errors_instead_of_returning_partial_on_failure() {
        let base_url = spawn_server(|base_url| {
            vec![
                MockResponse::json(200, commit_page(0, GITHUB_PER_PAGE)).with_header(
                    "Link",
                    format!(
                        "<{base_url}/repos/heddle/repo/pulls/439/commits?per_page={GITHUB_PER_PAGE}&page=2>; rel=\"next\""
                    ),
                ),
                MockResponse::json(502, "[]".to_string()),
            ]
        })
        .await;
        let err = client_for(&base_url)
            .fetch_contributors(&review_key(), None)
            .await
            .expect_err("commit pagination failure must not return partial contributors");
        assert!(
            err.to_string()
                .contains("pull request commits fetch failed")
        );
    }

    #[tokio::test]
    async fn fetch_comment_pages_errors_instead_of_returning_partial_on_failure() {
        let base_url = spawn_server(|base_url| {
            vec![
                MockResponse::json(200, comment_page(0, GITHUB_PER_PAGE)).with_header(
                    "Link",
                    format!(
                        "<{base_url}/repos/heddle/repo/issues/439/comments?per_page={GITHUB_PER_PAGE}&page=2>; rel=\"next\""
                    ),
                ),
                MockResponse::json(404, "[]".to_string()),
            ]
        })
        .await;
        let err = client_for(&base_url)
            .fetch_comment_pages(
                format!("{base_url}/repos/heddle/repo/issues/439/comments?per_page=100"),
                false,
                None,
            )
            .await
            .expect_err("comment pagination failure must not return partial comments");
        assert!(
            err.to_string()
                .contains("pull request comments fetch failed")
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
