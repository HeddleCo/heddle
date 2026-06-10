// SPDX-License-Identifier: Apache-2.0
use crate::{AuthContext, AuthToken, Permission, TokenScope};

#[test]
fn test_auth_token_creation() {
    let token = AuthToken::new("token123", "alice")
        .with_permissions(vec![Permission::Read, Permission::Write]);

    assert_eq!(token.id, "token123");
    assert_eq!(token.user, "alice");
    assert!(token.has_permission(Permission::Read));
    assert!(token.has_permission(Permission::Write));
    assert!(!token.has_permission(Permission::Push));
}

#[test]
fn test_auth_token_expiration() {
    let token = AuthToken::new("token123", "alice").with_expiration(1);

    assert!(token.is_expired());

    let token_no_exp = AuthToken::new("token123", "alice");
    assert!(!token_no_exp.is_expired());
}

#[test]
fn test_auth_token_scope() {
    let global_token = AuthToken::new("token1", "alice");
    assert!(global_token.can_access_repo("any-repo"));

    let repo_token = AuthToken::new("token2", "bob")
        .with_scope(TokenScope::Repositories(vec!["my-repo".to_string()]));
    assert!(repo_token.can_access_repo("my-repo"));
    assert!(!repo_token.can_access_repo("other-repo"));
}

#[test]
fn test_auth_token_serialization() {
    let token = AuthToken::new("token123", "alice")
        .with_permissions(vec![Permission::Read, Permission::Write]);

    let bytes = token.to_bytes();
    let restored = AuthToken::from_bytes(&bytes).unwrap();

    assert_eq!(restored.id, token.id);
    assert_eq!(restored.user, "");
    assert!(restored.permissions.is_empty());
}

#[test]
fn test_auth_context() {
    let ctx = AuthContext {
        user: "alice".to_string(),
        permissions: vec![Permission::Read, Permission::Push],
        token_id: "token123".to_string(),
        scope: TokenScope::Global,
    };

    assert!(ctx.can_read());
    assert!(ctx.can_push());
    assert!(!ctx.can_write());
    assert!(!ctx.is_admin());
}

fn namespace_ctx(scope: &str) -> AuthContext {
    AuthContext {
        user: "alice".to_string(),
        permissions: vec![Permission::Read],
        token_id: "token123".to_string(),
        scope: TokenScope::NamespaceTree(scope.to_string()),
    }
}

#[test]
fn test_namespace_tree_exact_match() {
    let ctx = namespace_ctx("team");
    assert!(ctx.can_access_namespace("team"));
}

#[test]
fn test_namespace_tree_downward_access() {
    // A token scoped to a namespace can reach the namespace itself and its descendants.
    let ctx = namespace_ctx("team");
    assert!(ctx.can_access_namespace("team/project"));
    assert!(ctx.can_access_namespace("team/project/sub"));
}

#[test]
fn test_namespace_tree_no_upward_access() {
    // A token scoped to a child namespace must NOT reach its parent or ancestors.
    let ctx = namespace_ctx("team/project");
    assert!(ctx.can_access_namespace("team/project")); // exact still allowed
    assert!(!ctx.can_access_namespace("team")); // parent — DENIED
    assert!(!ctx.can_access_namespace("")); // root — DENIED
}

#[test]
fn test_namespace_tree_no_sibling_access() {
    // Siblings under a shared ancestor must not reach one another.
    let ctx = namespace_ctx("team/project");
    assert!(!ctx.can_access_namespace("team/other"));
}

#[test]
fn test_namespace_tree_no_prefix_false_positive() {
    // A non-boundary prefix match must not grant access in either direction.
    let ctx = namespace_ctx("team");
    assert!(!ctx.can_access_namespace("teamwork"));

    let child = namespace_ctx("teamwork");
    assert!(!child.can_access_namespace("team"));
}
