mod ci_verdict_support;

use ci_verdict_support::{SIGNED_AT, maximal_body, sign};
use crypto::{
    BasisKind, CheckClass, CiVerdictBody, Conclusion, FailureClass, SignedVerdictError, SignerKind,
};

type Mutation = fn(&mut CiVerdictBody);

#[test]
fn tampering_each_body_field_is_a_body_digest_mismatch() {
    let mutations: &[(&str, Mutation)] = &[
        ("repo", |b| b.repo.push_str("-tampered")),
        ("state.content_hash", |b| {
            b.state.content_hash.push_str("-tampered");
        }),
        ("state.change_id", |b| {
            b.state.change_id.push_str("-tampered");
        }),
        ("state.logical_change_id", |b| {
            b.state.logical_change_id = None;
        }),
        ("basis.kind.target_state", |b| match &mut b.basis.kind {
            BasisKind::MergedWith { target_state, .. } => target_state.push_str("-tampered"),
            BasisKind::Branch => panic!("maximal fixture must have merge basis"),
        }),
        ("basis.kind.behind_count", |b| match &mut b.basis.kind {
            BasisKind::MergedWith { behind_count, .. } => *behind_count += 1,
            BasisKind::Branch => panic!("maximal fixture must have merge basis"),
        }),
        ("basis.kind.merge_algorithm_version", |b| {
            match &mut b.basis.kind {
                BasisKind::MergedWith {
                    merge_algorithm_version,
                    ..
                } => *merge_algorithm_version = None,
                BasisKind::Branch => panic!("maximal fixture must have merge basis"),
            }
        }),
        ("basis.kind.conflict_policy", |b| match &mut b.basis.kind {
            BasisKind::MergedWith {
                conflict_policy, ..
            } => *conflict_policy = None,
            BasisKind::Branch => panic!("maximal fixture must have merge basis"),
        }),
        ("basis.evaluated_tree_digest", |b| {
            b.basis.evaluated_tree_digest.push_str("-tampered");
        }),
        ("check.name", |b| b.check.name.push_str("-tampered")),
        ("check.class", |b| b.check.class = CheckClass::Advisory),
        ("check.definition_digest", |b| {
            b.check.definition_digest.push_str("-tampered");
        }),
        ("check.command", |b| {
            b.check.command.push("--release".into())
        }),
        ("check.image_digest", |b| b.check.image_digest = None),
        ("check.toolchain", |b| b.check.toolchain = None),
        ("check.params", |b| {
            b.check.params.insert("features".into(), "all".into());
        }),
        ("check.services", |b| b.check.services.push("redis".into())),
        ("check.node_id", |b| b.check.node_id = None),
        ("outcome.conclusion", |b| {
            b.outcome.conclusion = Conclusion::Success;
        }),
        ("outcome.failure", |b| b.outcome.failure = None),
        ("outcome.failure.class", |b| {
            b.outcome.failure.as_mut().expect("failure").class = FailureClass::Lint;
        }),
        ("outcome.failure.subclass", |b| {
            b.outcome.failure.as_mut().expect("failure").subclass = None;
        }),
        ("outcome.failure.failing_step", |b| {
            b.outcome.failure.as_mut().expect("failure").failing_step = None;
        }),
        ("outcome.failure.excerpt", |b| {
            b.outcome
                .failure
                .as_mut()
                .expect("failure")
                .excerpt
                .push_str(" tampered");
        }),
        ("outcome.failure.excerpt_encoding", |b| {
            b.outcome
                .failure
                .as_mut()
                .expect("failure")
                .excerpt_encoding = "base64".into();
        }),
        ("execution.pick_id", |b| b.execution.pick_id = None),
        ("execution.attempt", |b| b.execution.attempt += 1),
        ("execution.runner", |b| b.execution.runner = None),
        ("execution.started_at", |b| {
            b.execution.started_at.push_str("-tampered");
        }),
        ("execution.finished_at", |b| {
            b.execution.finished_at.push_str("-tampered");
        }),
        ("execution.duration_ms", |b| b.execution.duration_ms += 1),
        ("execution.ran_suites", |b| {
            b.execution.ran_suites.push("tampered".into());
        }),
        ("execution.skipped_suites", |b| {
            b.execution.skipped_suites.push("tampered".into());
        }),
        ("execution.runner_pool", |b| b.execution.runner_pool = None),
        ("execution.trust_tier", |b| b.execution.trust_tier = None),
        ("execution.isolation_tier", |b| {
            b.execution.isolation_tier = None;
        }),
        ("execution.materialization_proof", |b| {
            b.execution.materialization_proof = None;
        }),
        ("execution.secret_grants", |b| {
            b.execution.secret_grants.push("OTHER_SECRET".into());
        }),
        ("log", |b| b.log = None),
        ("log.manifest_digest", |b| {
            b.log
                .as_mut()
                .expect("log")
                .manifest_digest
                .push_str("-tampered");
        }),
        ("log.size_bytes", |b| {
            b.log.as_mut().expect("log").size_bytes += 1;
        }),
        ("repro.command", |b| {
            b.repro.command.push("--release".into())
        }),
        ("repro.env", |b| {
            b.repro
                .env
                .insert("RUSTFLAGS".into(), "-C opt-level=0".into());
        }),
        ("repro.image", |b| b.repro.image = None),
        ("repro.services", |b| b.repro.services.push("redis".into())),
        ("check_set_digest", |b| b.check_set_digest = None),
    ];

    for (field, mutate) in mutations {
        let mut signed = sign(maximal_body(), SignerKind::ServiceAccount, SIGNED_AT);
        mutate(&mut signed.body);
        assert!(
            matches!(
                signed.verify(),
                Err(SignedVerdictError::BodyDigestMismatch { .. })
            ),
            "tampering {field} must be reported as a body digest mismatch"
        );
    }
}

#[test]
fn recomputing_a_tampered_body_hash_still_breaks_the_signature() {
    let mut signed = sign(maximal_body(), SignerKind::ServiceAccount, SIGNED_AT);
    signed.body.check.class = CheckClass::Advisory;
    signed.content_hash = signed.body.content_hash();

    assert!(matches!(
        signed.verify(),
        Err(SignedVerdictError::Signer(
            crypto::SignerError::VerificationFailed
        ))
    ));
}
