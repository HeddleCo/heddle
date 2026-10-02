// SPDX-License-Identifier: Apache-2.0
use super::*;

fn fixture() -> (tempfile::TempDir, ExecutionContext) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init_default(dir.path()).unwrap();
    let context = ExecutionContext::builder()
        .repo(repo)
        .principal_fallback(Some((
            "Attribution Test".into(),
            "attribution@example.test".into(),
        )))
        .build();
    (dir, context)
}
fn record(ctx: &ExecutionContext, content: &str, patch: IdentityCursor) -> CaptureReport {
    let repo = ctx.require_repo().unwrap();
    std::fs::write(repo.root().join("tracked.txt"), content).unwrap();
    capture(
        ctx,
        CaptureOptions {
            intent: "test immutable attribution".into(),
            confidence: None,
            force: false,
            agent: CaptureAgentOptions {
                identity_patch: patch,
                ..Default::default()
            },
            machine_contract_input: None,
        },
    )
    .unwrap()
}
fn stored(ctx: &ExecutionContext) -> (State, AttributionEvidenceV1) {
    let repo = ctx.require_repo().unwrap();
    let state = repo.current_state().unwrap().unwrap();
    let hash = state.attribution_evidence.unwrap();
    let blob = repo.store().get_blob(&hash).unwrap().unwrap();
    (
        state,
        AttributionEvidenceV1::from_blob_with_hash(&blob, hash).unwrap(),
    )
}

#[test]
fn capture_commits_harness_even_when_model_is_unknown() {
    let (_dir, ctx) = fixture();
    let report = record(
        &ctx,
        "one",
        crate::cursor_patch_from_stdin("codex", r#"{"session_id":"session-a"}"#),
    );
    assert!(report.agent.is_none());
    let (state, evidence) = stored(&ctx);
    assert!(state.is_agent_authored());
    assert_eq!(evidence.harness.unwrap().value, "codex");
    assert!(evidence.selected.model.is_none());
    assert_eq!(
        report
            .attribution_evidence
            .unwrap()
            .scope
            .harness_session_id
            .as_deref(),
        Some("session-a")
    );
}

#[test]
fn human_capture_does_not_fabricate_agent_evidence() {
    let (_dir, ctx) = fixture();
    let report = record(&ctx, "human", IdentityCursor::default());
    assert!(report.agent.is_none());
    assert!(report.attribution_evidence.is_none());
    assert!(
        !ctx.require_repo()
            .unwrap()
            .current_state()
            .unwrap()
            .unwrap()
            .is_agent_authored()
    );
}

#[test]
fn session_switch_unknown_model_cannot_borrow_previous_model() {
    let (_dir, ctx) = fixture();
    record(
        &ctx,
        "one",
        crate::cursor_patch_from_stdin("codex", r#"{"session_id":"session-a","model":"model-a"}"#),
    );
    let first = stored(&ctx);
    let report = record(
        &ctx,
        "two",
        crate::cursor_patch_from_stdin("codex", r#"{"session_id":"session-b"}"#),
    );
    assert!(report.agent.is_none());
    let second = stored(&ctx);
    assert!(second.0.is_agent_authored());
    assert!(second.1.selected.model.is_none());
    assert_ne!(first.0.attribution_evidence, second.0.attribution_evidence);
    let old_blob = ctx
        .require_repo()
        .unwrap()
        .store()
        .get_blob(&first.0.attribution_evidence.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        AttributionEvidenceV1::from_blob(&old_blob)
            .unwrap()
            .selected
            .model
            .unwrap()
            .value,
        "model-a"
    );
}

#[test]
fn capture_drops_url_shaped_model_but_retains_known_harness() {
    let (_dir, ctx) = fixture();
    let report = record(
        &ctx,
        "one",
        crate::cursor_patch_from_stdin(
            "claude-code",
            r#"{"session_id":"session-a","model":"https://example.invalid/DO_NOT_PUBLISH"}"#,
        ),
    );
    assert!(report.agent.is_none());
    let (_, evidence) = stored(&ctx);
    assert!(evidence.selected.model.is_none());
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("DO_NOT_PUBLISH")
    );
    assert!(
        !std::fs::read_to_string(crate::identity_cursor_path(
            ctx.require_repo().unwrap().root()
        ))
        .unwrap()
        .contains("DO_NOT_PUBLISH")
    );
}
