#![allow(dead_code)]

use std::collections::BTreeMap;

use crypto::{
    Basis, BasisKind, CI_VERDICT_BODY_SCHEMA_VERSION, CheckClass, CheckDescriptor, CiVerdictBody,
    Conclusion, Ed25519Signer, Execution, FailureClass, FailureDetail, LogRef, Outcome, Repro,
    SignedVerdict, SignerKind, StateRef, signed_verdict_from_signer,
};
use objects::object::{ChangeId, ContentHash};

pub const SIGNED_AT: &str = "2026-06-11T18:05:54Z";

pub fn bindings() -> (ChangeId, ContentHash) {
    (
        ChangeId::from_bytes([2; 16]),
        ContentHash::from_bytes([3; 32]),
    )
}

pub fn test_signer() -> Ed25519Signer {
    Ed25519Signer::from_seed(&[1; 32]).expect("fixed test signer")
}

pub fn sign(body: CiVerdictBody, signer_kind: SignerKind, signed_at: &str) -> SignedVerdict {
    let (change_id, tree_digest) = bindings();
    signed_verdict_from_signer(
        body,
        &change_id,
        &tree_digest,
        signer_kind,
        signed_at.to_string(),
        &test_signer(),
    )
    .expect("fixture must sign")
}

pub fn passing_body() -> CiVerdictBody {
    let mut params = BTreeMap::new();
    params.insert("features".to_string(), "default".to_string());

    CiVerdictBody {
        schema_version: CI_VERDICT_BODY_SCHEMA_VERSION,
        repo: "heddle/core/heddle".to_string(),
        state: StateRef {
            content_hash: "b3:9af2c0de".to_string(),
            change_id: "hd-7m3k".to_string(),
            logical_change_id: None,
        },
        basis: Basis {
            kind: BasisKind::MergedWith {
                target_state: "b3:1c0dba5e".to_string(),
                behind_count: 3,
                merge_algorithm_version: None,
                conflict_policy: None,
            },
            evaluated_tree_digest: "b3:deadbeef".to_string(),
        },
        check: CheckDescriptor {
            name: "check.test".to_string(),
            class: CheckClass::Required,
            definition_digest: "b3:c0ffee".to_string(),
            command: vec!["cargo".into(), "test".into(), "--workspace".into()],
            image_digest: None,
            toolchain: Some("rustc 1.96.0".to_string()),
            params,
            services: vec![],
            node_id: None,
        },
        outcome: Outcome {
            conclusion: Conclusion::Success,
            failure: None,
        },
        execution: Execution {
            pick_id: Some("pk_01J".to_string()),
            attempt: 1,
            runner: Some("svc:runner-pool-a".to_string()),
            started_at: "2026-06-11T18:02:11Z".to_string(),
            finished_at: SIGNED_AT.to_string(),
            duration_ms: 223_000,
            ran_suites: vec!["workspace".into()],
            skipped_suites: vec![],
            runner_pool: None,
            trust_tier: None,
            isolation_tier: None,
            materialization_proof: None,
            secret_grants: vec![],
        },
        log: None,
        repro: Repro {
            command: vec![
                "cargo".into(),
                "test".into(),
                "--locked".into(),
                "--workspace".into(),
            ],
            env: BTreeMap::new(),
            image: None,
            services: vec![],
        },
        check_set_digest: None,
    }
}

pub fn maximal_body() -> CiVerdictBody {
    let mut body = passing_body();
    body.repo = "heddle/core/weft".to_string();
    body.state = StateRef {
        content_hash: "b3:5e11a7c0".to_string(),
        change_id: "hd-9q2x".to_string(),
        logical_change_id: Some("lc-weft-postgres".to_string()),
    };
    body.basis = Basis {
        kind: BasisKind::MergedWith {
            target_state: "hd-main-0001".to_string(),
            behind_count: 7,
            merge_algorithm_version: Some("recursive-v2".to_string()),
            conflict_policy: Some("refuse-on-conflict".to_string()),
        },
        evaluated_tree_digest: "b3:7eaf00d5".to_string(),
    };
    body.check = maximal_check();
    body.outcome = Outcome {
        conclusion: Conclusion::Failure,
        failure: Some(FailureDetail {
            class: FailureClass::Test,
            subclass: Some("ignored_suite".to_string()),
            failing_step: Some("check.postgres".to_string()),
            excerpt: "test schema::migrations::applies ... FAILED".to_string(),
            excerpt_encoding: "utf8".to_string(),
        }),
    };
    body.execution = maximal_execution();
    body.log = Some(LogRef {
        manifest_digest: "b3:109111".to_string(),
        size_bytes: 48_122,
    });
    body.repro = maximal_repro();
    body.check_set_digest = Some("b3:cs0001".to_string());
    body
}

fn maximal_check() -> CheckDescriptor {
    CheckDescriptor {
        name: "check.postgres".to_string(),
        class: CheckClass::Required,
        definition_digest: "b3:def111".to_string(),
        command: vec![
            "cargo".into(),
            "test".into(),
            "--features".into(),
            "postgres".into(),
            "--".into(),
            "--ignored".into(),
        ],
        image_digest: Some("sha256:abc123".to_string()),
        toolchain: Some("rustc 1.96.0".to_string()),
        params: BTreeMap::from([
            ("features".to_string(), "postgres".to_string()),
            ("toolchain".to_string(), "stable".to_string()),
        ]),
        services: vec!["postgres".into()],
        node_id: Some("node-postgres-suite".to_string()),
    }
}

fn maximal_execution() -> Execution {
    Execution {
        pick_id: Some("pk_02K".to_string()),
        attempt: 2,
        runner: Some("svc:runner-pool-b".to_string()),
        started_at: "2026-06-11T19:00:00Z".to_string(),
        finished_at: "2026-06-11T19:04:30Z".to_string(),
        duration_ms: 270_000,
        ran_suites: vec!["postgres".into(), "migrations".into()],
        skipped_suites: vec!["unit".into()],
        runner_pool: Some("pool-b".to_string()),
        trust_tier: Some("t1_container".to_string()),
        isolation_tier: Some("t1_container".to_string()),
        materialization_proof: Some("b3:merged7eaf00d5".to_string()),
        secret_grants: vec!["DATABASE_URL".into()],
    }
}

fn maximal_repro() -> Repro {
    Repro {
        command: vec![
            "cargo".into(),
            "test".into(),
            "--locked".into(),
            "--features".into(),
            "postgres".into(),
            "--".into(),
            "--ignored".into(),
        ],
        env: BTreeMap::from([
            ("CARGO_TERM_COLOR".to_string(), "never".to_string()),
            ("RUST_BACKTRACE".to_string(), "1".to_string()),
        ]),
        image: Some("sha256:abc123".to_string()),
        services: vec!["postgres".into()],
    }
}

pub fn branch_basis_body() -> CiVerdictBody {
    let mut body = passing_body();
    body.basis = Basis {
        kind: BasisKind::Branch,
        evaluated_tree_digest: "b3:branchasis".to_string(),
    };
    body
}
