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
