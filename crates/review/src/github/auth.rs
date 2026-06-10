// SPDX-License-Identifier: Apache-2.0
use async_trait::async_trait;
use base64::Engine;
use chrono::{Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

use crate::{Result, errors::ReviewError, types::ReviewJobKey};

const GITHUB_API: &str = "https://api.github.com";

#[async_trait]
pub trait GitHubInstallationLookup: Send + Sync {
    async fn installation_id_for_repo(&self, key: &ReviewJobKey) -> Result<Option<i64>>;
}

#[derive(Debug, Clone)]
pub struct GitHubAuthClient {
    http: reqwest::Client,
    app_id: String,
    private_key_pem: String,
}

#[derive(Debug, Serialize)]
struct AppClaims {
    iat: usize,
    exp: usize,
    iss: String,
}

impl GitHubAuthClient {
    pub fn from_env() -> Result<Self> {
        let app_id = std::env::var("GITHUB_APP_ID")
            .ok()
            .or_else(|| std::env::var("GITHUB_CLIENT_ID").ok())
            .ok_or_else(|| {
                ReviewError::MissingConfiguration(
                    "GITHUB_APP_ID or GITHUB_CLIENT_ID must be configured".to_string(),
                )
            })?;
        let private_key = std::env::var("GITHUB_APP_PRIVATE_KEY").map_err(|_| {
            ReviewError::MissingConfiguration("GITHUB_APP_PRIVATE_KEY must be configured".into())
        })?;
        let private_key_pem = if !private_key.contains("BEGIN") && !private_key.contains('\n') {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(private_key)
                .map_err(|err| ReviewError::MissingConfiguration(err.to_string()))?;
            String::from_utf8(bytes)
                .map_err(|err| ReviewError::MissingConfiguration(err.to_string()))?
        } else {
            private_key.replace("\\n", "\n")
        };
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("heddle-review")
                // Mirror the REST client: never follow redirects, so a
                // server-controlled `Location` can't steer auth traffic
                // off-origin (SSRF, #521).
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|err| ReviewError::Github(err.to_string()))?,
            app_id,
            private_key_pem,
        })
    }

    fn app_jwt(&self) -> Result<String> {
        let now = Utc::now();
        let claims = AppClaims {
            iat: (now - Duration::seconds(60)).timestamp() as usize,
            exp: (now + Duration::minutes(10)).timestamp() as usize,
            iss: self.app_id.clone(),
        };
        encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(self.private_key_pem.as_bytes())
                .map_err(|err| ReviewError::MissingConfiguration(err.to_string()))?,
        )
        .map_err(|err| ReviewError::Github(err.to_string()))
    }

    pub async fn installation_token(&self, installation_id: i64) -> Result<String> {
        let jwt = self.app_jwt()?;
        let response = self
            .http
            .post(format!(
                "{GITHUB_API}/app/installations/{installation_id}/access_tokens"
            ))
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|err| ReviewError::Github(err.to_string()))?;
        if !response.status().is_success() {
            return Err(ReviewError::Github(format!(
                "installation token request failed with {}",
                response.status()
            )));
        }
        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(|err| ReviewError::Serialization(err.to_string()))?;
        body.get("token")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| ReviewError::Serialization("missing installation token".into()))
    }
}
