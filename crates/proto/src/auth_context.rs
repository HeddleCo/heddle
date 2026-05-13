// SPDX-License-Identifier: Apache-2.0
use crate::{Permission, TokenScope};

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user: String,
    pub permissions: Vec<Permission>,
    pub token_id: String,
    pub scope: TokenScope,
}

impl AuthContext {
    pub fn has_permission(&self, perm: Permission) -> bool {
        self.permissions.contains(&perm)
    }

    pub fn can_read(&self) -> bool {
        self.has_permission(Permission::Read)
    }

    pub fn can_write(&self) -> bool {
        self.has_permission(Permission::Write)
    }

    pub fn can_push(&self) -> bool {
        self.has_permission(Permission::Push)
    }

    pub fn is_admin(&self) -> bool {
        self.has_permission(Permission::Admin)
    }

    pub fn can_access_repo(&self, repo: &str) -> bool {
        match &self.scope {
            TokenScope::Global => true,
            TokenScope::Repositories(repos) => repos.iter().any(|candidate| candidate == repo),
            TokenScope::NamespaceTree(namespace) => {
                repo == namespace || repo.starts_with(&format!("{}/", namespace))
            }
        }
    }

    pub fn can_access_namespace(&self, namespace: &str) -> bool {
        match &self.scope {
            TokenScope::Global => true,
            TokenScope::Repositories(_) => false,
            TokenScope::NamespaceTree(scope) => {
                namespace == scope
                    || namespace.starts_with(&format!("{}/", scope))
                    || scope.starts_with(&format!("{}/", namespace))
            }
        }
    }
}