// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn message_turn_and_tool_ids_never_become_provider_response_ids() {
    let mut value = evidence();
    value.scope.message_id = Some("message-1".into());
    value.scope.root_turn_id = Some("turn-root".into());
    value.scope.tool_call_id = Some("tool-7".into());
    let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().unwrap()).unwrap();
    assert_eq!(decoded.scope, value.scope);
    assert!(decoded.scope.response_id.is_none());
    value.scope.message_id = Some("invalid message prose".into());
    assert!(value.to_blob().is_err());
}

#[test]
fn legacy_parent_projection_preserves_distinct_native_namespaces() {
    let mut value = evidence();
    value.scope.parent_actor_id = None;
    value.scope.parent_harness_session_id = Some("parent-session".into());
    let agent = value.legacy_agent().unwrap();
    assert_eq!(agent.parent.as_deref(), Some("parent-session"));
    value.validate_legacy_agent(Some(&agent)).unwrap();
    value.scope.parent_actor_id = Some("parent-actor".into());
    assert_eq!(
        value.legacy_agent().unwrap().parent.as_deref(),
        Some("parent-actor")
    );
    let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().unwrap()).unwrap();
    assert_eq!(decoded.scope, value.scope);
}

#[test]
fn claude_child_can_share_the_root_session_without_invented_parent_actor() {
    let mut value = evidence();
    value.scope.parent_actor_id = None;
    value.scope.parent_harness_session_id = value.scope.harness_session_id.clone();
    let decoded = AttributionEvidenceV1::from_blob(&value.to_blob().unwrap()).unwrap();
    assert!(decoded.scope.parent_actor_id.is_none());
    assert_eq!(
        decoded.scope.parent_harness_session_id,
        decoded.scope.harness_session_id
    );
    assert_eq!(decoded.scope.actor_id.as_deref(), Some("child-2"));
    value.scope.actor_id = value.scope.harness_session_id.clone();
    assert!(value.to_blob().is_err());
    value.scope.actor_id = None;
    assert!(value.to_blob().is_err());
}
