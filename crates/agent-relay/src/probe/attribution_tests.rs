// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn claude_session_metadata_is_not_misclassified_as_opencode() {
    let result = probe_harness_actor(&HarnessProbeInput {
        probe_metadata: BTreeMap::from([("session_id".into(), "claude-session".into())]),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(result.harness.as_deref(), Some("claude-code"));
}

#[test]
fn explicit_other_harness_is_not_overridden_by_inherited_codex() {
    let result = probe_harness_actor(&HarnessProbeInput {
        explicit_harness: Some("aider".into()),
        env_hints: BTreeMap::from([("CODEX_THREAD_ID".into(), "inherited".into())]),
        current_provider: Some("local".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(result.harness.as_deref(), Some("aider"));
    assert_eq!(result.provider.as_deref(), Some("local"));
}

#[test]
fn codex_environment_identity_is_not_protocol_evidence() {
    let result = probe_harness_actor(&HarnessProbeInput {
        env_hints: BTreeMap::from([("CODEX_THREAD_ID".into(), "thread".into())]),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(result.probe_source.as_deref(), Some("argv_env"));
    assert_eq!(result.confidence, Some(0.55));
}

#[test]
fn codex_hook_child_retains_parent_and_hook_provenance() {
    let result = probe_harness_actor(&HarnessProbeInput {
        explicit_harness: Some("codex".into()),
        probe_metadata: BTreeMap::from([
            ("thread_id".into(), "child".into()),
            ("parent_thread_id".into(), "root".into()),
            ("hook_event".into(), "PreToolUse".into()),
        ]),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        result.native_actor_key.as_deref(),
        Some("codex:thread:child")
    );
    assert_eq!(
        result.native_parent_actor_key.as_deref(),
        Some("codex:thread:root")
    );
    assert!(!result.attach_hints.root_actor);
    assert_eq!(result.probe_source.as_deref(), Some("hook_payload"));
}

#[test]
fn other_harnesses_never_inherit_codex_session_scope() {
    for harness in ["claude-code", "opencode", "aider"] {
        let result = probe_harness_actor(&HarnessProbeInput {
            explicit_harness: Some(harness.into()),
            env_hints: BTreeMap::from([("CODEX_THREAD_ID".into(), "foreign".into())]),
            ..Default::default()
        })
        .unwrap();
        let evidence = result.attribution_evidence.unwrap();
        assert_eq!(evidence.harness.unwrap().value, harness);
        assert!(evidence.scope.harness_session_id.is_none());
        assert!(evidence.scope.actor_id.is_none());
    }
}

#[test]
fn provider_override_and_model_keep_independent_provenance() {
    use objects::object::{AttributionBasis as Basis, AttributionSource as Source};
    let result = probe_harness_actor(&HarnessProbeInput {
        explicit_harness: Some("codex".into()),
        env_hints: BTreeMap::from([("HEDDLE_AGENT_PROVIDER".into(), "override".into())]),
        probe_metadata: BTreeMap::from([
            ("model_provider".into(), "route".into()),
            ("model".into(), "selected-alias".into()),
            ("hook_event".into(), "PreToolUse".into()),
        ]),
        ..Default::default()
    })
    .unwrap();
    let evidence = result.attribution_evidence.unwrap();
    let provider = evidence.selected.provider.unwrap();
    assert_eq!(Some(provider.value), result.provider);
    assert_eq!(provider.basis, Basis::Explicit);
    assert_eq!(provider.source, Source::Environment);
    let model = evidence.selected.model.unwrap();
    assert_eq!(model.basis, Basis::RequestReported);
    assert_eq!(model.source, Source::HarnessHook);
    assert!(evidence.response.model.is_none());
}

#[test]
fn codex_rollout_observations_are_request_and_session_scoped() {
    use objects::object::{
        AttributionBasis as Basis, AttributionSource as Source, HarnessVersionScope,
    };
    let result = probe_harness_actor(&HarnessProbeInput {
        explicit_harness: Some("codex".into()),
        probe_metadata: BTreeMap::from([
            ("thread_id".into(), "thread".into()),
            ("turn_id".into(), "turn".into()),
            ("session_source".into(), "rollout_session_meta".into()),
            ("model_source".into(), "rollout_turn_context".into()),
            ("model".into(), "alias".into()),
            ("model_provider".into(), "route".into()),
            ("cli_version".into(), "0.100.0".into()),
        ]),
        ..Default::default()
    })
    .unwrap();
    let evidence = result.attribution_evidence.unwrap();
    let model = evidence.selected.model.unwrap();
    assert_eq!(model.value, "alias");
    assert_eq!(model.basis, Basis::RequestReported);
    assert_eq!(model.source, Source::Transcript);
    let provider = evidence.selected.provider.unwrap();
    assert_eq!(provider.basis, Basis::Configured);
    assert_eq!(provider.source, Source::SessionMetadata);
    assert_eq!(
        evidence.harness_version_scope,
        Some(HarnessVersionScope::SessionCreation)
    );
    assert_eq!(evidence.scope.harness_session_id.as_deref(), Some("thread"));
    assert_eq!(evidence.scope.turn_id.as_deref(), Some("turn"));
    assert!(evidence.response.model.is_none());
}
