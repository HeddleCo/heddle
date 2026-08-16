use super::identity::{EnsureAction, ensure_action};

#[test]
fn invite_is_never_selected_when_any_credential_exists() {
    assert_eq!(ensure_action(Some(true), true), EnsureAction::Reuse);
    assert_eq!(ensure_action(Some(false), true), EnsureAction::Derive);
}

#[test]
fn provisioning_is_only_the_no_credential_fallback() {
    assert_eq!(ensure_action(None, true), EnsureAction::Provision);
    assert_eq!(ensure_action(None, false), EnsureAction::RequireInvite);
}
