// SPDX-License-Identifier: Apache-2.0

use super::IdentityCursor;

fn main_cursor() -> IdentityCursor {
    IdentityCursor {
        attribution_evidence: None,
        provider: Some("anthropic".into()),
        model: Some("opus".into()),
        thought_level: Some("high".into()),
        session: Some("session-1".into()),
        parent: None,
    }
}

#[test]
fn new_session_does_not_inherit_identity_or_parent() {
    let mut current = main_cursor();
    current.parent = Some("child-1".into());
    let patch = IdentityCursor {
        session: Some("session-2".into()),
        ..IdentityCursor::default()
    };

    assert_eq!(current.merge_event(&patch), patch);
}

#[test]
fn first_known_session_does_not_claim_unscoped_identity() {
    let mut current = main_cursor();
    current.session = None;
    let patch = IdentityCursor {
        session: Some("session-1".into()),
        ..IdentityCursor::default()
    };

    assert_eq!(current.merge_event(&patch), patch);
}

#[test]
fn new_session_keeps_only_published_incoming_identity() {
    let current = main_cursor();
    let patch = IdentityCursor {
        provider: Some("openai".into()),
        model: Some("unknown".into()),
        thought_level: Some(" ".into()),
        session: Some("session-2".into()),
        parent: None,
        attribution_evidence: None,
    };

    assert_eq!(current.merge_event(&patch), patch.omit_unpublished());
}

#[test]
fn actor_change_keeps_explicit_model_without_inheriting_effort() {
    let current = main_cursor();
    for patch in [
        IdentityCursor {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            session: Some("session-2".into()),
            ..IdentityCursor::default()
        },
        IdentityCursor {
            provider: Some("anthropic".into()),
            model: Some("sonnet".into()),
            session: current.session.clone(),
            parent: Some("child-1".into()),
            ..IdentityCursor::default()
        },
    ] {
        assert_eq!(current.merge_event(&patch), patch);
    }
}

#[test]
fn claude_child_entry_and_sibling_change_clear_missing_identity() {
    let current = main_cursor();
    let child = IdentityCursor {
        session: current.session.clone(),
        parent: Some("child-1".into()),
        ..IdentityCursor::default()
    };
    assert_eq!(current.merge_event(&child), child);

    let current_child = current.merge_event(&IdentityCursor {
        provider: Some("anthropic".into()),
        model: Some("sonnet".into()),
        thought_level: Some("low".into()),
        ..child
    });
    let sibling = IdentityCursor {
        session: current.session,
        parent: Some("child-2".into()),
        ..IdentityCursor::default()
    };
    assert_eq!(current_child.merge_event(&sibling), sibling);
}

#[test]
fn claude_child_exit_does_not_leave_child_identity_on_main() {
    let current = IdentityCursor {
        parent: Some("child-1".into()),
        ..main_cursor()
    };
    let next = current.merge_event(&IdentityCursor {
        session: current.session.clone(),
        parent: current.session.clone(),
        ..IdentityCursor::default()
    });

    assert_eq!(
        next,
        IdentityCursor {
            session: current.session,
            ..IdentityCursor::default()
        }
    );
}

#[test]
fn codex_child_exit_does_not_leave_child_identity_on_parent() {
    let current = IdentityCursor {
        parent: Some("parent-session".into()),
        ..main_cursor()
    };
    let patch = IdentityCursor {
        session: current.parent.clone(),
        ..IdentityCursor::default()
    };

    assert_eq!(current.merge_event(&patch), patch);
}

#[test]
fn claude_sparse_main_event_preserves_identity() {
    let current = main_cursor();
    let patch = IdentityCursor {
        session: Some(" session-1 ".into()),
        parent: Some("session-1".into()),
        model: Some("unknown".into()),
        ..IdentityCursor::default()
    };

    assert_eq!(current.merge_event(&patch), current);
}

#[test]
fn same_child_partial_event_preserves_missing_fields() {
    let current = IdentityCursor {
        parent: Some("child-1".into()),
        ..main_cursor()
    };
    for parent in [None, current.parent.clone()] {
        let next = current.merge_event(&IdentityCursor {
            session: current.session.clone(),
            parent,
            thought_level: Some("low".into()),
            ..IdentityCursor::default()
        });
        assert_eq!(
            next,
            IdentityCursor {
                thought_level: Some("low".into()),
                ..current.clone()
            }
        );
    }
}

#[test]
fn same_actor_model_switch_preserves_other_fields_and_previous_cursor() {
    let current = main_cursor();
    for session in [None, current.session.clone()] {
        let next = current.merge_event(&IdentityCursor {
            model: Some("sonnet".into()),
            session,
            ..IdentityCursor::default()
        });
        assert_eq!(
            next,
            IdentityCursor {
                model: Some("sonnet".into()),
                ..current.clone()
            }
        );
        assert_eq!(current.model.as_deref(), Some("opus"));
    }
}

#[test]
fn parent_only_actor_change_clears_identity_and_keeps_session() {
    let current = main_cursor();
    let next = current.merge_event(&IdentityCursor {
        parent: Some("child-1".into()),
        ..IdentityCursor::default()
    });

    assert_eq!(
        next,
        IdentityCursor {
            session: current.session,
            parent: Some("child-1".into()),
            ..IdentityCursor::default()
        }
    );
}

#[test]
fn new_turn_clears_unknown_model_and_effort_in_both_projections() {
    let current = crate::cursor_patch_from_stdin(
        "codex",
        r#"{"session_id":"s","turn_id":"one","model":"model-one","effort":"high"}"#,
    );
    let next = current.merge_event(&crate::cursor_patch_from_stdin(
        "codex",
        r#"{"session_id":"s","turn_id":"two"}"#,
    ));
    assert!(next.model.is_none());
    assert!(next.thought_level.is_none());
    let evidence = next.attribution_evidence.unwrap();
    assert!(evidence.selected.model.is_none());
    assert!(evidence.selected.thought_level.is_none());
    assert_eq!(evidence.scope.turn_id.as_deref(), Some("two"));
}

#[test]
fn new_opencode_message_cannot_borrow_previous_message_model() {
    let current = crate::cursor_patch_from_stdin(
        "opencode",
        r#"{"event":{"type":"message.updated","properties":{"info":{"role":"assistant","id":"m1","sessionID":"s","modelID":"model-one","providerID":"local"}}}}"#,
    );
    let next = current.merge_event(&crate::cursor_patch_from_stdin("opencode", r#"{"event":{"type":"message.updated","properties":{"info":{"role":"assistant","id":"m2","sessionID":"s"}}}}"#));
    assert!(next.model.is_none());
    assert!(next.provider.is_none());
    let evidence = next.attribution_evidence.unwrap();
    assert!(evidence.selected.model.is_none());
    assert_eq!(evidence.scope.message_id.as_deref(), Some("m2"));
}

#[test]
fn sparse_same_request_preserves_response_identity_with_claim() {
    let current = crate::cursor_patch_from_stdin(
        "claude-code",
        r#"{"type":"assistant","session_id":"s","message":{"id":"response-one","model":"model-one"}}"#,
    );
    let next = current.merge_event(&crate::cursor_patch_from_stdin(
        "claude-code",
        r#"{"session_id":"s","tool_use_id":"tool-one"}"#,
    ));
    let evidence = next.attribution_evidence.unwrap();
    assert_eq!(evidence.response.model.unwrap().value, "model-one");
    assert_eq!(evidence.scope.response_id.as_deref(), Some("response-one"));
}

#[test]
fn model_switch_does_not_keep_old_response_claim() {
    let current = crate::cursor_patch_from_stdin(
        "claude-code",
        r#"{"type":"assistant","session_id":"s","model":"model-one","message":{"id":"response-one","model":"model-one"}}"#,
    );
    let next = current.merge_event(&crate::cursor_patch_from_stdin(
        "claude-code",
        r#"{"session_id":"s","hook_event_name":"PostModelSwitch","to_model":"model-two"}"#,
    ));
    let evidence = next.attribution_evidence.unwrap();
    assert_eq!(evidence.selected.model.unwrap().value, "model-two");
    assert!(evidence.response.model.is_none());
    assert!(evidence.scope.response_id.is_none());
}

#[test]
fn unscoped_status_version_is_not_reused_after_an_unobserved_resume() {
    let current = crate::cursor_patch_from_stdin(
        "claude-code",
        r#"{"session_id":"s","version":"1.2.3","model":{"id":"model-one"}}"#,
    );
    let next = current.merge_event(&crate::cursor_patch_from_stdin(
        "claude-code",
        r#"{"session_id":"s","hook_event_name":"SessionStart"}"#,
    ));
    assert!(next.attribution_evidence.unwrap().harness_version.is_none());
}
