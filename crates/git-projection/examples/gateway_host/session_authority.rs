// SPDX-License-Identifier: Apache-2.0
//! The real Weft Git-session boundary. Static catalog metadata never supplies user authority.
use crate::{Result, policy::now};
use api::heddle::api::v1alpha2::{
    GitTransportAuthorizeRequest, GitTransportAuthorizeResponse, GitTransportScope,
};
use base64::Engine;
use prost::Message;
use std::io::Read;

pub struct SessionAuthority {
    origin: reqwest::Url,
    client: reqwest::blocking::Client,
}
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedActor {
    actor: String,
    epoch: i64,
    expires_at: i64,
    credential_digest: [u8; 32],
    scope: GitTransportScope,
}
impl VerifiedActor {
    pub fn actor(&self) -> &str {
        &self.actor
    }
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }
    pub fn epoch(&self) -> i64 {
        self.epoch
    }
    pub fn scope(&self) -> &GitTransportScope {
        &self.scope
    }
    pub fn credential_digest(&self) -> &[u8; 32] {
        &self.credential_digest
    }
    pub fn same_authority(&self, other: &Self) -> bool {
        self.actor == other.actor && self.epoch == other.epoch && self.scope == other.scope
    }
}
pub fn session(header: &str) -> Result<String> {
    let token = if let Some(token) = header.strip_prefix("Bearer ") {
        token.to_owned()
    } else if let Some(encoded) = header.strip_prefix("Basic ") {
        if encoded.len() > 256 {
            return Err("Git credential limit".into());
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        if base64::engine::general_purpose::STANDARD.encode(&bytes) != encoded {
            return Err("noncanonical Git credential".into());
        }
        std::str::from_utf8(&bytes)?
            .strip_prefix("git:")
            .ok_or("Git username must be git")?
            .to_owned()
    } else {
        return Err("Git session required".into());
    };
    let suffix = token.strip_prefix("ggit1_").ok_or("Git session required")?;
    if suffix.len() != 64
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid Git session".into());
    }
    Ok(token)
}
impl SessionAuthority {
    /// Trusted host configuration supplies a bare Weft HTTPS origin. Caller URLs and forwarding
    /// headers cannot choose another authority or broaden canonical scope. Loopback is test-only.
    pub fn new(origin: &str, local_fixture: bool) -> Result<Self> {
        let url = reqwest::Url::parse(origin)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || local_fixture && url.scheme() == "http" && url.host_str() == Some("127.0.0.1"))
        {
            return Err("fixed Weft authority origin required".into());
        }
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        Ok(Self {
            origin: url,
            client,
        })
    }
    /// Synthetic HTTPS integration only. No insecure certificate bypass and no default roots.
    #[cfg(feature = "gateway-fixture")]
    pub fn new_fixture(origin: &str, ca_pem: &[u8]) -> Result<Self> {
        let url = reqwest::Url::parse(origin)?;
        if url.scheme() != "https"
            || url.host_str() != Some("127.0.0.1")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || ca_pem.is_empty()
            || ca_pem.len() > 64 * 1024
        {
            return Err("explicit loopback HTTPS fixture authority required".into());
        }
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(15))
            .tls_certs_only([reqwest::Certificate::from_pem(ca_pem)?])
            .build()?;
        Ok(Self {
            origin: url,
            client,
        })
    }
    pub fn authorize(&self, header: &str, scope: &GitTransportScope) -> Result<VerifiedActor> {
        let token = session(header)?;
        let body = GitTransportAuthorizeRequest {
            transport_token: token,
            expected_scope: Some(scope.clone()),
        }
        .encode_to_vec();
        let response = self
            .client
            .post(self.origin.join("git/auth/authorize")?)
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
            .body(body)
            .send()?;
        if response.status() != reqwest::StatusCode::OK
            || response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                != Some("application/x-protobuf")
            || response
                .headers()
                .contains_key(reqwest::header::CONTENT_ENCODING)
            || response.content_length().is_some_and(|n| n > 4096)
        {
            return Err("current Git session denied".into());
        }
        let mut bytes = Vec::new();
        response.take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err("Git authority response limit".into());
        }
        let result = GitTransportAuthorizeResponse::decode(bytes.as_slice())?;
        let actor = result.actor.ok_or("verified Git actor absent")?.id;
        let uuid = uuid::Uuid::parse_str(&actor)?;
        let instant = now()? as i64;
        if uuid.is_nil()
            || uuid.to_string() != actor
            || result.authorization_epoch <= 0
            || result.expires_at_seconds <= instant
            || result.expires_at_seconds > instant + 300
        {
            return Err("invalid or expired Git authority".into());
        }
        Ok(VerifiedActor {
            actor,
            epoch: result.authorization_epoch,
            expires_at: result.expires_at_seconds,
            credential_digest: result.credential_digest.as_slice().try_into()?,
            scope: scope.clone(),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_git_basic_and_bearer_resolve_same_transport_only_secret() {
        let token = format!("ggit1_{}", "a".repeat(64));
        assert_eq!(session(&format!("Bearer {token}")).unwrap(), token);
        assert_eq!(
            session(&format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("git:{token}"))
            ))
            .unwrap(),
            token
        );
        for bad in [
            "Bearer root",
            "Basic Z2l0OnJlYWRlcg==",
            "Bearer ggit1_A",
            "Bearer ",
        ] {
            assert!(session(bad).is_err());
        }
        assert!(
            session(&format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("admin:{token}"))
            ))
            .is_err()
        );
    }
}

impl SessionAuthority {
    /// Fresh authorization for every retained historical source, original author, current
    /// visibility/redaction/retention and accepted native head. Session authorization alone
    /// does not suffice. Select a receipt OR an existing native bootstrap head, never both.
    pub fn history(
        &self,
        header: &str,
        scope: &GitTransportScope,
        operation: Option<&str>,
        bootstrap: Option<&[u8; 32]>,
    ) -> Result<api::heddle::api::v1alpha2::GitHistoryAuthorizeResponse> {
        use api::heddle::api::v1alpha2::{GitHistoryAuthorizeRequest, GitHistoryAuthorizeResponse};
        if operation.is_some() == bootstrap.is_some() || operation.is_some_and(str::is_empty) {
            return Err("one native history selector required".into());
        }
        let request = GitHistoryAuthorizeRequest {
            transport_token: session(header)?,
            scope: Some(scope.clone()),
            client_operation_id: operation.unwrap_or_default().to_string(),
            expected_revision: bootstrap.map(|b| b.to_vec()).unwrap_or_default(),
        };
        let response = self
            .client
            .post(self.origin.join("git/history/authorize")?)
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
            .body(request.encode_to_vec())
            .send()?;
        const LIMIT: u64 = 16 * 1024 * 1024;
        if response.status() != reqwest::StatusCode::OK
            || response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                != Some("application/x-protobuf")
            || response
                .headers()
                .contains_key(reqwest::header::CONTENT_ENCODING)
            || response.content_length().is_some_and(|n| n > LIMIT)
        {
            return Err("current native history unavailable".into());
        }
        let mut bytes = Vec::new();
        response.take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err("native authority response limit".into());
        }
        let result = GitHistoryAuthorizeResponse::decode(bytes.as_slice())?;
        let billing_owner = uuid::Uuid::parse_str(&result.billing_owner_account_id)?;
        if result.authorization_epoch <= 0
            || result.source_generation < 0
            || billing_owner.is_nil()
            || billing_owner.to_string() != result.billing_owner_account_id
            || result.spool_genesis.len() != 32
            || result.sharing_policy_version.len() != 32
        {
            return Err("native authority generation absent".into());
        }
        if let Some(operation) = operation {
            if result.bootstrap.is_some()
                || result.receipt.as_ref().is_none_or(|r| {
                    r.client_operation_id != operation || r.git_acceptance.is_none()
                })
            {
                return Err("native acceptance authority selector mismatch".into());
            }
        } else {
            let head = result
                .bootstrap
                .as_ref()
                .ok_or("native bootstrap proof absent")?;
            let expected = bootstrap.ok_or("native bootstrap selector absent")?;
            if result.receipt.is_some() || head.revisions.is_empty() || head.revisions.len()>128 || head.closure_digest.len()!=32
                || head.thread.as_ref().is_none_or(|t| t.spool!=scope.spool || t.id!=scope.thread)
                || head.head.as_ref().is_none_or(|r|r.spool!=scope.spool || !matches!(&r.revision,
                    Some(api::heddle::api::v1alpha2::revision_ref::Revision::State(s)) if s.value==expected.as_slice())) {
                return Err("native bootstrap authority selector mismatch".into());
            }
        }
        Ok(result)
    }
}
