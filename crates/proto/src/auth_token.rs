// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};

use crate::Permission;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken {
    pub id: String,
    pub user: String,
    pub permissions: Vec<Permission>,
    pub expires_at: u64,
    pub scope: TokenScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TokenScope {
    Global,
    Repositories(Vec<String>),
    NamespaceTree(String),
}

impl AuthToken {
    pub fn new(id: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            user: user.into(),
            permissions: vec![Permission::Read],
            expires_at: 0,
            scope: TokenScope::Global,
        }
    }

    pub fn with_permissions(mut self, permissions: Vec<Permission>) -> Self {
        self.permissions = permissions;
        self
    }

    pub fn with_expiration(mut self, expires_at: u64) -> Self {
        self.expires_at = expires_at;
        self
    }

    pub fn with_scope(mut self, scope: TokenScope) -> Self {
        self.scope = scope;
        self
    }

    pub fn has_permission(&self, perm: Permission) -> bool {
        self.permissions.contains(&perm)
    }

    pub fn is_expired(&self) -> bool {
        if self.expires_at == 0 {
            return false;
        }
        let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(_) => return true,
        };
        now > self.expires_at
    }

    pub fn can_access_repo(&self, repo: &str) -> bool {
        match &self.scope {
            TokenScope::Global => true,
            TokenScope::Repositories(repos) => repos.contains(&repo.to_string()),
            TokenScope::NamespaceTree(namespace) => {
                repo == namespace || repo.starts_with(&format!("{}/", namespace))
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.id.as_bytes().to_vec()
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let token_str = std::str::from_utf8(bytes).ok()?.trim().to_string();
        if token_str.is_empty() {
            return None;
        }
        let id = token_str.split(':').next()?.to_string();
        Some(Self {
            id,
            user: String::new(),
            permissions: vec![],
            expires_at: 0,
            scope: TokenScope::Global,
        })
    }
}