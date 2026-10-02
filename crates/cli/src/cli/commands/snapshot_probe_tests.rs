// SPDX-License-Identifier: Apache-2.0
use super::tests::{
    EnvVarGuard, empty_agent_overrides, isolate_child_identity_env, user_config_with_principal,
};
use super::*;
use objects::object::{AttributionBasis, AttributionSource, HarnessVersionScope};

#[test]
#[serial_test::serial]
fn codex_probe_refreshes_capture_evidence_from_synthetic_rollout() {
    let _env = isolate_child_identity_env();
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions/2026/10/02");
    std::fs::create_dir_all(&sessions).unwrap();
    let rollout = sessions.join("rollout-2026-10-02T00-00-00-synthetic-thread.jsonl");
    let session = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"synthetic-thread\",\"cli_version\":\"0.100.0\",\"model_provider\":\"custom-route\"}}\n";
    std::fs::write(&rollout, format!("{session}{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"one\",\"model\":\"alias-one\"}}}}\n")).unwrap();
    let _home = EnvVarGuard::set("CODEX_HOME", home.path().to_str().unwrap());
    let _thread = EnvVarGuard::set("CODEX_THREAD_ID", "synthetic-thread");
    let temp = tempfile::tempdir().unwrap();
    let repo = crate::init_test_repository(temp.path()).unwrap();
    std::fs::write(temp.path().join("file"), "one").unwrap();
    create_snapshot(
        &repo,
        &user_config_with_principal(),
        Some("first".into()),
        None,
        empty_agent_overrides(),
    )
    .unwrap();
    let first = repo
        .store()
        .get_state(&repo.head().unwrap().unwrap())
        .unwrap()
        .unwrap();
    let before = repo::load_attribution_evidence(repo.store(), &first)
        .unwrap()
        .unwrap();
    assert_eq!(before.selected.model.as_ref().unwrap().value, "alias-one");
    assert_eq!(
        before.selected.model.as_ref().unwrap().basis,
        AttributionBasis::RequestReported
    );
    assert_eq!(
        before.selected.model.as_ref().unwrap().source,
        AttributionSource::Transcript
    );
    assert_eq!(
        before.selected.provider.as_ref().unwrap().basis,
        AttributionBasis::Configured
    );
    assert_eq!(
        before.harness_version_scope,
        Some(HarnessVersionScope::SessionCreation)
    );
    assert!(before.response.model.is_none());

    std::fs::write(&rollout, format!("{session}{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"two\",\"model\":\"alias-two\"}}}}\n")).unwrap();
    std::fs::write(temp.path().join("file"), "two").unwrap();
    create_snapshot(
        &repo,
        &user_config_with_principal(),
        Some("second".into()),
        None,
        empty_agent_overrides(),
    )
    .unwrap();
    let second = repo
        .store()
        .get_state(&repo.head().unwrap().unwrap())
        .unwrap()
        .unwrap();
    let after = repo::load_attribution_evidence(repo.store(), &second)
        .unwrap()
        .unwrap();
    assert_eq!(after.selected.model.as_ref().unwrap().value, "alias-two");
    assert_eq!(after.scope.turn_id.as_deref(), Some("two"));
    assert_ne!(first.attribution_evidence, second.attribution_evidence);
    assert_eq!(
        repo::load_attribution_evidence(repo.store(), &first)
            .unwrap()
            .unwrap(),
        before
    );
}

#[test]
#[serial_test::serial]
fn ambient_probe_does_not_replace_stamped_producing_hook() {
    let _env = isolate_child_identity_env();
    let _thread = EnvVarGuard::set("CODEX_THREAD_ID", "foreign-parent");
    let temp = tempfile::tempdir().unwrap();
    let repo = crate::init_test_repository(temp.path()).unwrap();
    let patch = verbs::claude_cursor_patch(&serde_json::json!({
        "session_id":"claude-child", "model":"selected-alias", "turn_id":"producing-turn"
    }));
    verbs::stamp_identity_cursor(repo.root(), &patch).unwrap();
    let options = capture_agent_options(
        &repo,
        &user_config_with_principal(),
        &empty_agent_overrides(),
    );
    assert!(options.identity_patch.is_empty());
    let (_, evidence) = verbs::resolve_capture_identity(
        &repo,
        user_config_with_principal().principal_pair(),
        &options,
    )
    .unwrap();
    let evidence = evidence.unwrap();
    assert_eq!(evidence.harness.unwrap().value, "claude-code");
    assert_eq!(
        evidence.scope.harness_session_id.as_deref(),
        Some("claude-child")
    );
    assert_eq!(evidence.scope.turn_id.as_deref(), Some("producing-turn"));
    assert_eq!(
        evidence.selected.model.unwrap().source,
        AttributionSource::HarnessHook
    );
}

#[test]
fn process_only_parent_hint_cannot_replace_child_environment_identity() {
    use objects::object::{AttributionClaim, AttributionEvidenceV1};
    let child = verbs::cursor_patch_from_child_env(&std::collections::BTreeMap::from([
        ("PI_SESSION_ID".into(), "pi-session".into()),
        ("PI_MODEL".into(), "pi-model".into()),
        ("PI_PROVIDER".into(), "route".into()),
    ]));
    let probe = agent_relay::HarnessProbeResult {
        attribution_evidence: Some(AttributionEvidenceV1 {
            harness: Some(AttributionClaim::new(
                "codex",
                AttributionBasis::Observed,
                AttributionSource::Process,
            )),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        capture_probe_patch(&verbs::IdentityCursor::default(), child.clone(), probe),
        child
    );
}
