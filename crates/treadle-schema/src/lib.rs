//! `treadle-schema` — the OSS contract for heddle CI verdicts.
//!
//! A CI result is a signed [`Verdict`](SignedVerdict): an ed25519 signature over
//! the BLAKE3-256 content digest of a canonical [`CiVerdictBody`]. The body
//! carries everything a gate or a fixer agent needs without reading logs — the
//! check identity and its definition digest, the evaluated-tree digest (branch vs
//! merged-with-target), the conclusion, the failure class + ANSI-stripped excerpt,
//! and the exact reproduction recipe.
//!
//! This crate is dependency-light and clock-free on purpose: it is the schema any
//! runner or orchestrator can depend on. The hosted control plane (weft) and the
//! runner both target these types.
//!
//! # Quickstart
//!
//! ```
//! use treadle_schema::{fixture, sign, SignerKind};
//! use ed25519_dalek::SigningKey;
//!
//! let body = fixture::passing_body();
//! let key = SigningKey::from_bytes(&[7u8; 32]);
//! let signed = sign(body, &key, "2026-06-11T18:05:54Z".into(), SignerKind::Delegated);
//! assert!(signed.verify().is_ok());
//! ```

mod body;
mod signed;

pub use body::{
    Basis, BasisKind, CheckClass, CheckDescriptor, CiVerdictBody, Conclusion, DIGEST_PREFIX,
    Execution, FailureClass, FailureDetail, LogRef, Outcome, Repro, SCHEMA_VERSION, StateRef,
};
pub use signed::{
    SIGNING_PAYLOAD_VERSION_TAG, SignedVerdict, SignerKind, VerifyError, is_content_digest, sign,
    signed_payload,
};

/// Test/example fixtures. Public so the engine, runner, and downstream consumers
/// can build canonical sample verdicts without hand-assembling every field.
pub mod fixture {
    use std::collections::BTreeMap;

    use crate::body::{
        Basis, BasisKind, CheckClass, CheckDescriptor, CiVerdictBody, Conclusion, Execution,
        FailureClass, FailureDetail, LogRef, Outcome, Repro, SCHEMA_VERSION, StateRef,
    };

    /// A representative *passing* verdict body. Deterministic — no clock, no rng —
    /// so its [`crate::CiVerdictBody::body_digest`] is a stable golden value.
    #[must_use]
    pub fn passing_body() -> CiVerdictBody {
        let mut params = BTreeMap::new();
        params.insert("features".to_string(), "default".to_string());

        CiVerdictBody {
            schema_version: SCHEMA_VERSION,
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
                finished_at: "2026-06-11T18:05:54Z".to_string(),
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

    /// A representative *failing* (build-class) verdict body.
    #[must_use]
    pub fn failing_body() -> CiVerdictBody {
        let mut body = passing_body();
        body.outcome = Outcome {
            conclusion: Conclusion::Failure,
            failure: Some(FailureDetail {
                class: FailureClass::Build,
                subclass: Some("default_features".to_string()),
                failing_step: Some("check.test".to_string()),
                excerpt: "error[E0308]: mismatched types\n  --> crates/client/src/sync.rs:412:18"
                    .to_string(),
                excerpt_encoding: "utf8".to_string(),
            }),
        };
        body
    }

    /// A *maximal* verdict body with **every** optional field set, including the
    /// pre-freeze D18 attestation block. Deterministic. Used to pin the canonical
    /// bytes when the omit-when-absent Option rule has nothing to omit — the
    /// adversarial complement to [`passing_body`] (which leaves most Options unset).
    #[must_use]
    pub fn maximal_body() -> CiVerdictBody {
        let mut params = BTreeMap::new();
        params.insert("features".to_string(), "postgres".to_string());
        params.insert("toolchain".to_string(), "stable".to_string());

        let mut env = BTreeMap::new();
        env.insert("CARGO_TERM_COLOR".to_string(), "never".to_string());
        env.insert("RUST_BACKTRACE".to_string(), "1".to_string());

        CiVerdictBody {
            schema_version: SCHEMA_VERSION,
            repo: "heddle/core/weft".to_string(),
            state: StateRef {
                content_hash: "b3:5e11a7c0".to_string(),
                change_id: "hd-9q2x".to_string(),
                logical_change_id: Some("lc-weft-postgres".to_string()),
            },
            basis: Basis {
                kind: BasisKind::MergedWith {
                    target_state: "hd-main-0001".to_string(),
                    behind_count: 7,
                    merge_algorithm_version: Some("recursive-v2".to_string()),
                    conflict_policy: Some("refuse-on-conflict".to_string()),
                },
                evaluated_tree_digest: "b3:7eaf00d5".to_string(),
            },
            check: CheckDescriptor {
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
                params,
                services: vec!["postgres".into()],
                node_id: Some("node-postgres-suite".to_string()),
            },
            outcome: Outcome {
                conclusion: Conclusion::Failure,
                failure: Some(FailureDetail {
                    class: FailureClass::Test,
                    subclass: Some("ignored_suite".to_string()),
                    failing_step: Some("check.postgres".to_string()),
                    excerpt: "test schema::migrations::applies ... FAILED".to_string(),
                    excerpt_encoding: "utf8".to_string(),
                }),
            },
            execution: Execution {
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
            },
            log: Some(LogRef {
                manifest_digest: "b3:109111".to_string(),
                size_bytes: 48_122,
            }),
            repro: Repro {
                command: vec![
                    "cargo".into(),
                    "test".into(),
                    "--locked".into(),
                    "--features".into(),
                    "postgres".into(),
                    "--".into(),
                    "--ignored".into(),
                ],
                env,
                image: Some("sha256:abc123".to_string()),
                services: vec!["postgres".into()],
            },
            check_set_digest: Some("b3:cs0001".to_string()),
        }
    }

    /// A *branch-basis* verdict body: a passing check evaluated against the branch
    /// tree exactly as-pushed (NOT a speculative merge). This is the **only**
    /// fixture exercising [`BasisKind::Branch`] — every other fixture is
    /// `MergedWith` — so it pins the canonical bytes of the bare `"branch"` tag
    /// (serde's externally-tagged unit variant). Without it, a schema/serde
    /// mismatch on the `Branch` form passes every test silently. Deterministic.
    #[must_use]
    pub fn branch_basis_body() -> CiVerdictBody {
        let mut body = passing_body();
        body.basis = Basis {
            kind: BasisKind::Branch,
            evaluated_tree_digest: "b3:branchasis".to_string(),
        };
        body
    }

    /// A *merge-basis* verdict body: a passing check evaluated against a merged
    /// tree (not the branch as-pushed), with the pre-freeze merge fields set. This
    /// is the dogfood's most expensive GHA surprise (stale-target false-green) made
    /// explicit, and a third vector shape for cross-language reproduction.
    #[must_use]
    pub fn merge_basis_body() -> CiVerdictBody {
        let mut body = passing_body();
        body.basis = Basis {
            kind: BasisKind::MergedWith {
                target_state: "hd-main-0042".to_string(),
                behind_count: 2,
                merge_algorithm_version: Some("recursive-v2".to_string()),
                conflict_policy: None,
            },
            evaluated_tree_digest: "b3:merged0042".to_string(),
        };
        body
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rng;
    use serde_json::Value;

    use super::*;

    /// Golden digest snapshot for [`fixture::passing_body`]. If this value changes,
    /// the canonical bytes changed.
    ///
    /// **Pre-freeze status:** the schema is not yet frozen (never published as an
    /// authoritative wire contract), so canonical-bytes changes are permitted now
    /// and the value below is regenerated *deliberately* rather than bumping
    /// [`SCHEMA_VERSION`] — `v1` is still being *defined*, not revised. The most
    /// recent regeneration folded in two deliberate pre-freeze changes:
    /// (1) the D18 day-one attestation fields, and (2) the omit-when-absent Option
    /// rule (every `None`/empty field now omitted from the bytes rather than
    /// emitted as `null`/`[]`). **Once the schema freezes, any further change here
    /// MUST bump [`SCHEMA_VERSION`] and the golden vectors together.**
    const PASSING_BODY_GOLDEN_DIGEST: &str =
        "b3:993b1a126b4521684ed957390f10d3445f7b3e8d5c644ccbca1f6cff88544be5";

    #[test]
    fn passing_body_digest_is_stable() {
        // The digest must be reproducible across runs / machines.
        let a = fixture::passing_body().body_digest();
        let b = fixture::passing_body().body_digest();
        assert_eq!(a, b, "digest must be deterministic");
        assert!(
            is_content_digest(&a),
            "digest must be a well-formed b3: digest, got {a}"
        );
    }

    #[test]
    fn passing_body_matches_golden_digest() {
        // This pins the canonicalization. A diff here means canonical bytes moved.
        let digest = fixture::passing_body().body_digest();
        assert_eq!(
            digest, PASSING_BODY_GOLDEN_DIGEST,
            "canonical bytes changed — bump SCHEMA_VERSION and update the golden \
             digest deliberately (current: {digest})"
        );
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let key = SigningKey::generate(&mut rng());
        let signed = sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::ServiceAccount,
        );
        assert!(
            signed.verify().is_ok(),
            "freshly-signed verdict must verify"
        );
        assert_eq!(signed.signer_public_key(), &signed.public_key);
    }

    #[test]
    fn tampering_with_conclusion_fails_verify() {
        let key = SigningKey::generate(&mut rng());
        let mut signed = sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::ServiceAccount,
        );
        // Flip the conclusion to Failure but leave the signature + digest as-is.
        signed.body.outcome.conclusion = Conclusion::Failure;
        let err = signed.verify().expect_err("tampered body must fail verify");
        assert!(
            matches!(err, VerifyError::BodyDigestMismatch { .. }),
            "expected digest mismatch, got {err:?}"
        );
    }

    #[test]
    fn tampering_then_recomputing_digest_still_fails_signature() {
        // A smarter attacker also recomputes body_digest to match the tampered body.
        // The signature (over the *original* digest) must then fail.
        let key = SigningKey::generate(&mut rng());
        let mut signed = sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::ServiceAccount,
        );
        signed.body.outcome.conclusion = Conclusion::Failure;
        signed.body_digest = signed.body.body_digest(); // re-derive to match
        let err = signed
            .verify()
            .expect_err("re-digested tampered body must fail signature");
        assert_eq!(err, VerifyError::SignatureInvalid);
    }

    #[test]
    fn wrong_key_fails_verify() {
        let key = SigningKey::generate(&mut rng());
        let mut signed = sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::ServiceAccount,
        );
        // Substitute a different, valid public key.
        let other = SigningKey::generate(&mut rng());
        signed.public_key = {
            let mut s = String::new();
            for b in other.verifying_key().as_bytes() {
                s.push_str(&format!("{b:02x}"));
            }
            s
        };
        assert_eq!(signed.verify(), Err(VerifyError::SignatureInvalid));
    }

    #[test]
    fn unknown_fields_deserialize_for_forward_compat() {
        // A newer producer appends a field; an older consumer must still parse.
        // This asserts deny_unknown_fields is NOT set on the body.
        let mut value =
            serde_json::to_value(fixture::passing_body()).expect("body serializes to json");
        value
            .as_object_mut()
            .unwrap()
            .insert("future_field_v2".to_string(), serde_json::json!("ignored"));
        let parsed: CiVerdictBody =
            serde_json::from_value(value).expect("unknown fields must be tolerated");
        assert_eq!(parsed.repo, "heddle/core/heddle");
    }

    #[test]
    fn deny_unknown_fields_is_off_on_every_body_struct() {
        // Brief item 5: keep deny_unknown_fields OFF and ASSERT it. We assert it
        // structurally — inject an unknown key into each *nested* object and prove
        // it still deserializes (deny_unknown_fields would reject it). Covers the
        // body + every nested struct a newer producer might extend.
        let body = fixture::maximal_body();
        let mut value = serde_json::to_value(&body).unwrap();
        let root = value.as_object_mut().unwrap();
        root.insert("future_root".into(), serde_json::json!(1));
        for nested in ["state", "basis", "check", "outcome", "execution", "repro"] {
            root.get_mut(nested)
                .and_then(Value::as_object_mut)
                .unwrap_or_else(|| panic!("{nested} is an object"))
                .insert("future_nested".into(), serde_json::json!("x"));
        }
        let parsed: CiVerdictBody = serde_json::from_value(value)
            .expect("unknown fields in body + nested structs must be tolerated");
        assert_eq!(
            parsed, body,
            "unknown fields must be dropped, not alter data"
        );
    }

    #[test]
    fn absent_options_are_omitted_not_null() {
        // The canonical-bytes contract: a None/empty field is OMITTED, never
        // serialized as null/[]. passing_body leaves many Options unset.
        let json = String::from_utf8(fixture::passing_body().canonical_bytes()).unwrap();
        // None-valued fields must be entirely absent (no `:null`, no key at all).
        for absent in [
            "logical_change_id",
            "image_digest",
            "failure",
            "log",
            "check_set_digest",
            "node_id",
            "runner_pool",
            "trust_tier",
            "isolation_tier",
            "materialization_proof",
            "merge_algorithm_version",
            "conflict_policy",
        ] {
            assert!(
                !json.contains(absent),
                "unset field {absent:?} must be omitted from canonical bytes, found in: {json}"
            );
        }
        // And there must be no literal `null` anywhere in the canonical bytes.
        assert!(
            !json.contains("null"),
            "omit-when-absent rule violated: a null leaked into canonical bytes: {json}"
        );
        // empty secret_grants (a Vec) is omitted too.
        assert!(!json.contains("secret_grants"));
    }

    #[test]
    fn present_options_do_serialize() {
        // The flip side: when set, the fields ARE present (so omission is value-
        // driven, not a dropped field).
        let json = String::from_utf8(fixture::maximal_body().canonical_bytes()).unwrap();
        for present in [
            "logical_change_id",
            "image_digest",
            "node_id",
            "check_set_digest",
            "runner_pool",
            "trust_tier",
            "isolation_tier",
            "materialization_proof",
            "secret_grants",
            "merge_algorithm_version",
            "conflict_policy",
            "failure",
            "log",
        ] {
            assert!(
                json.contains(present),
                "set field {present:?} missing from: {json}"
            );
        }
    }

    #[test]
    fn maximal_and_merge_basis_bodies_sign_and_verify() {
        let key = SigningKey::generate(&mut rng());
        for body in [fixture::maximal_body(), fixture::merge_basis_body()] {
            let signed = sign(
                body,
                &key,
                "2026-06-11T19:04:30Z".into(),
                SignerKind::ServiceAccount,
            );
            assert!(signed.verify().is_ok(), "fixture must verify");
        }
    }

    #[test]
    fn newer_schema_version_is_rejected_distinctly_from_tampering() {
        // P1-3: a verdict from a strictly-newer producer must NOT be misreported
        // as BodyDigestMismatch (== tampering). It gets its own error.
        let key = SigningKey::generate(&mut rng());
        let mut signed = sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::ServiceAccount,
        );
        signed.body.schema_version = SCHEMA_VERSION + 1;
        let err = signed
            .verify()
            .expect_err("newer schema must not verify here");
        assert_eq!(
            err,
            VerifyError::UnsupportedSchemaVersion {
                found: SCHEMA_VERSION + 1,
                supported: SCHEMA_VERSION,
            },
            "must be the dedicated error, not a digest mismatch"
        );
        assert!(
            !matches!(err, VerifyError::BodyDigestMismatch { .. }),
            "the whole point: distinguishable from tampering"
        );
    }

    #[test]
    fn signer_kind_is_bound_into_the_signature() {
        // P1-2: flipping signer_kind without re-signing must break verify().
        let key = SigningKey::generate(&mut rng());
        let mut signed = sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::Device,
        );
        assert!(signed.verify().is_ok());
        signed.signer_kind = SignerKind::ServiceAccount; // relabel device -> service
        assert_eq!(
            signed.verify(),
            Err(VerifyError::SignatureInvalid),
            "signer_kind relabel must invalidate the signature"
        );
    }

    #[test]
    fn signed_at_is_bound_into_the_signature() {
        // P1-2: rewriting signed_at without re-signing must break verify().
        let key = SigningKey::generate(&mut rng());
        let mut signed = sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::ServiceAccount,
        );
        assert!(signed.verify().is_ok());
        signed.signed_at = "2026-06-11T18:05:55Z".into(); // +1s replay-presentation
        assert_eq!(
            signed.verify(),
            Err(VerifyError::SignatureInvalid),
            "signed_at rewrite must invalidate the signature"
        );
    }

    #[test]
    fn signature_payload_has_domain_separation() {
        // P0-1: the preimage must begin with the domain tag and must NOT be the
        // bare digest string (cross-protocol confusion guard).
        let body = fixture::passing_body();
        let digest = body.body_digest();
        let payload = signed_payload(&digest, SignerKind::ServiceAccount, "2026-06-11T18:05:54Z");
        assert!(
            payload.starts_with(SIGNING_PAYLOAD_VERSION_TAG),
            "preimage must carry the domain tag"
        );
        assert_ne!(
            payload,
            digest.as_bytes(),
            "preimage must not be the bare digest string"
        );
        // A signature over the bare digest must NOT verify as a verdict signature
        // (the exact cross-protocol confusion the tag prevents).
        let key = SigningKey::generate(&mut rng());
        let bare_sig = key.sign(digest.as_bytes());
        let mut signed = sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::ServiceAccount,
        );
        signed.signature = {
            let mut s = String::new();
            for b in bare_sig.to_bytes() {
                s.push_str(&format!("{b:02x}"));
            }
            s
        };
        assert_eq!(
            signed.verify(),
            Err(VerifyError::SignatureInvalid),
            "a bare-digest signature must be rejected as a verdict signature"
        );
    }

    #[test]
    fn tampering_each_load_bearing_field_fails_verify() {
        // P2-5: table-driven — every field an attack targets must be covered by
        // the digest, not just outcome.conclusion. Each mutation flips the body
        // digest, so verify() must reject with BodyDigestMismatch.
        let key = SigningKey::generate(&mut rng());
        type Mut = fn(&mut CiVerdictBody);
        let mutate: Vec<(&str, Mut)> = vec![
            ("basis.evaluated_tree_digest", |b| {
                b.basis.evaluated_tree_digest = "b3:tampered".into();
            }),
            ("check.definition_digest", |b| {
                b.check.definition_digest = "b3:tampered".into();
            }),
            ("check.params", |b| {
                b.check.params.insert("features".into(), "all".into());
            }),
            ("check.class", |b| {
                b.check.class = CheckClass::Advisory; // required -> advisory downgrade
            }),
            ("execution.attempt", |b| {
                b.execution.attempt = 99;
            }),
            (
                "check_set_digest (pre-freeze field is still digest-covered)",
                |b| {
                    b.check_set_digest = Some("b3:injected".into());
                },
            ),
            (
                "execution.trust_tier (pre-freeze field is still digest-covered)",
                |b| {
                    b.execution.trust_tier = Some("t2_microvm".into());
                },
            ),
        ];
        for (label, f) in mutate {
            let mut signed = sign(
                fixture::passing_body(),
                &key,
                "2026-06-11T18:05:54Z".into(),
                SignerKind::ServiceAccount,
            );
            f(&mut signed.body);
            let err = signed
                .verify()
                .expect_err(&format!("tampering {label} must fail verify"));
            assert!(
                matches!(err, VerifyError::BodyDigestMismatch { .. }),
                "tampering {label}: expected digest mismatch, got {err:?}"
            );
        }
    }

    #[test]
    fn signed_verdict_json_roundtrips() {
        let key = SigningKey::generate(&mut rng());
        let signed = sign(
            fixture::failing_body(),
            &key,
            "2026-06-11T18:05:54Z".into(),
            SignerKind::ServiceAccount,
        );
        let json = serde_json::to_string(&signed).expect("serialize");
        let back: SignedVerdict = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(signed, back);
        assert!(back.verify().is_ok());
    }
}
