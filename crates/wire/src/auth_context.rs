// SPDX-License-Identifier: Apache-2.0
use crate::{Permission, TokenScope, scope_contains};

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
            TokenScope::NamespaceTree(namespace) => scope_contains(namespace, repo),
        }
    }

    /// Whether this context may access `namespace`.
    ///
    /// `NamespaceTree(scope)` grants access **downward only**: the scope
    /// namespace itself and any descendant (`scope/...`). It deliberately does
    /// NOT grant upward access — a token scoped to a child namespace cannot
    /// reach its parent or ancestors. This mirrors [`Self::can_access_repo`].
    ///
    /// Containment is segment-aware via [`scope_contains`]: candidates with
    /// `.`/`..`/empty segments are denied outright (no check-then-normalize
    /// bypass).
    pub fn can_access_namespace(&self, namespace: &str) -> bool {
        match &self.scope {
            TokenScope::Global => true,
            TokenScope::Repositories(_) => false,
            TokenScope::NamespaceTree(scope) => scope_contains(scope, namespace),
        }
    }
}
