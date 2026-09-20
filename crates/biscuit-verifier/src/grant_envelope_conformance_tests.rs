//! Property conformance for the load-bearing client-minted grant-envelope gate.
//!
//! Every generated graph produces assertions at the exact, subset, superset,
//! disjoint, cross-resource, descendant, ancestor, and mixed-set boundaries.
//! The expected result is computed independently from the production verifier:
//! kind and path are exact, and the action lattice is copied explicitly from
//! the `rules.biscuit` header. Each case then traverses the real signed-envelope
//! and client-signed Biscuit verification path.

use biscuit_auth::{Biscuit, KeyPair, builder::BiscuitBuilder};
use chrono::{DateTime, Duration, Utc};
use proptest::prelude::*;

use super::{
    BiscuitError, Right,
    envelope::{GrantEnvelope, SignedGrantEnvelope},
    verify_client_minted_at_with_resource,
};

const SUBJECT: &str = "grant-envelope-property-subject";
const OPERATION: &str = "GrantEnvelopeProperty";
const NOW_SECONDS: i64 = 2_000_000_000;

#[derive(Clone, Copy, Debug)]
enum Boundary {
    Exact,
    Subset,
    Superset,
    Disjoint,
    CrossResource,
    Descendant,
    Ancestor,
    MixedSet,
}

#[derive(Clone, Debug)]
struct AssertionCase {
    boundary: Boundary,
    rights: Vec<Right>,
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(NOW_SECONDS, 0).expect("fixed property-test clock is valid")
}

fn action(index: u8) -> &'static str {
    match index % 5 {
        0 => "read",
        1 => "write",
        2 => "admin",
        3 => "merge",
        _ => "approve",
    }
}

fn kind(index: u8) -> &'static str {
    match index % 3 {
        0 => "spool",
        1 => "thread",
        _ => "context",
    }
}

fn graph_strategy() -> impl Strategy<Value = Vec<Right>> {
    (
        (0_u8..3, 0_u8..2, prop::collection::vec(0_u8..10, 1..7)),
        prop::collection::vec(
            (0_u8..3, 0_u8..9, prop::collection::vec(0_u8..10, 1..7)),
            0..8,
        ),
    )
        .prop_map(|(anchor, rest)| {
            std::iter::once(anchor)
                .chain(rest)
                .enumerate()
                .map(|(node, (kind_index, action_index, segments))| {
                    let path = segments
                        .into_iter()
                        .enumerate()
                        .map(|(depth, segment)| format!("n{node}-{depth}-{segment}"))
                        .collect::<Vec<_>>()
                        .join("/");
                    let node_action = if node == 0 {
                        match action_index {
                            0 => "write",
                            _ => "admin",
                        }
                    } else {
                        action(action_index)
                    };
                    Right::new(kind(kind_index), path, node_action)
                })
                .collect()
        })
}

/// Independent copy of the envelope action lattice declared in
/// `rules.biscuit`. Deliberately does not call `envelope_action_implies`.
fn oracle_action_implies(granted: &str, asserted: &str) -> bool {
    granted == asserted
        || matches!(
            (granted, asserted),
            ("admin", "write") | ("admin", "read") | ("write", "read")
        )
}

fn oracle_covers(grants: &[Right], assertion: &Right) -> bool {
    grants.iter().any(|grant| {
        grant.kind == assertion.kind
            && grant.path == assertion.path
            && oracle_action_implies(&grant.action, &assertion.action)
    })
}

fn oracle_accepts(grants: &[Right], assertions: &[Right]) -> bool {
    assertions
        .iter()
        .all(|assertion| oracle_covers(grants, assertion))
}

fn strict_subset_action(action: &str) -> &'static str {
    match action {
        "admin" | "write" => "read",
        _ => unreachable!("the generated graph anchor is always write or admin"),
    }
}

fn uncovered_action(action: &str) -> &'static str {
    if action == "approve" {
        "merge"
    } else {
        "approve"
    }
}

fn cases_for_graph(grants: &[Right]) -> Vec<AssertionCase> {
    let pivot = &grants[0];
    let exact = pivot.clone();
    let subset = Right::new(
        pivot.kind.clone(),
        pivot.path.clone(),
        strict_subset_action(&pivot.action),
    );
    let missing = Right::new(
        pivot.kind.clone(),
        pivot.path.clone(),
        uncovered_action(&pivot.action),
    );
    let disjoint = Right::new(
        pivot.kind.clone(),
        format!("disjoint/{}", pivot.path),
        pivot.action.clone(),
    );
    let cross_resource = Right::new(
        match pivot.kind.as_str() {
            "spool" => "thread",
            _ => "spool",
        },
        pivot.path.clone(),
        pivot.action.clone(),
    );
    let descendant = Right::new(
        pivot.kind.clone(),
        format!("{}/child", pivot.path),
        pivot.action.clone(),
    );
    let ancestor_path = pivot
        .path
        .rsplit_once('/')
        .map_or_else(|| "root".to_string(), |(parent, _)| parent.to_string());
    let ancestor = Right::new(pivot.kind.clone(), ancestor_path, pivot.action.clone());

    vec![
        AssertionCase {
            boundary: Boundary::Exact,
            rights: vec![exact],
        },
        AssertionCase {
            boundary: Boundary::Subset,
            rights: vec![subset.clone()],
        },
        AssertionCase {
            boundary: Boundary::Superset,
            rights: vec![subset.clone(), missing],
        },
        AssertionCase {
            boundary: Boundary::Disjoint,
            rights: vec![disjoint],
        },
        AssertionCase {
            boundary: Boundary::CrossResource,
            rights: vec![cross_resource],
        },
        AssertionCase {
            boundary: Boundary::Descendant,
            rights: vec![descendant],
        },
        AssertionCase {
            boundary: Boundary::Ancestor,
            rights: vec![ancestor],
        },
        AssertionCase {
            boundary: Boundary::MixedSet,
            rights: vec![subset, Right::new("spool", "outside/graph", "admin")],
        },
    ]
}

fn signed_envelope(grants: Vec<Right>, device: &KeyPair, root: &KeyPair) -> SignedGrantEnvelope {
    let mut device_pubkey = [0_u8; 32];
    device_pubkey.copy_from_slice(&device.public().to_bytes());
    GrantEnvelope {
        device_pubkey,
        subject: SUBJECT.to_string(),
        rights: grants,
        issued_at: now() - Duration::seconds(1),
        expires_at: now() + Duration::hours(1),
    }
    .sign_with_root(root)
    .expect("generated envelope signs")
}

fn client_token(assertions: &[Right], device: &KeyPair) -> String {
    let issued_at = now() - Duration::seconds(1);
    let expires_at = now() + Duration::hours(1);
    let mut builder: BiscuitBuilder = Biscuit::builder();
    for fact in [
        format!("user({SUBJECT:?})"),
        "session(\"grant-envelope-property-session\")".to_string(),
        format!("issued_at({})", issued_at.to_rfc3339()),
        format!("expires_at({})", expires_at.to_rfc3339()),
        "amr(\"property\")".to_string(),
    ] {
        builder = builder.fact(fact.as_str()).expect("add identity fact");
    }
    for right in assertions {
        let fact = format!(
            "right({:?}, {:?}, {:?})",
            right.kind, right.path, right.action
        );
        builder = builder.fact(fact.as_str()).expect("add generated right");
    }
    builder
        .build(device)
        .expect("build generated client token")
        .to_base64()
        .expect("encode generated client token")
}

fn production_accepts(grants: &[Right], assertions: &[Right]) -> Result<bool, BiscuitError> {
    let root = KeyPair::new();
    let device = KeyPair::new();
    let envelope = signed_envelope(grants.to_vec(), &device, &root)
        .to_base64()
        .expect("encode generated envelope");
    let token = client_token(assertions, &device);
    match verify_client_minted_at_with_resource(
        &token,
        &envelope,
        &[root.public()],
        &[root.public()],
        OPERATION,
        None,
        now(),
    ) {
        Ok(_) => Ok(true),
        Err(BiscuitError::EnvelopeInvalid(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn random_grant_graphs_accept_all_in_envelope_and_reject_all_outside(
        grants in graph_strategy(),
    ) {
        for case in cases_for_graph(&grants) {
            let expected = oracle_accepts(&grants, &case.rights);
            let actual = production_accepts(&grants, &case.rights)
                .map_err(|error| TestCaseError::fail(format!(
                    "unexpected verifier error at {:?}: {error}", case.boundary
                )))?;
            prop_assert_eq!(
                actual,
                expected,
                "boundary={:?}, grants={:?}, assertions={:?}",
                case.boundary,
                grants,
                case.rights,
            );
        }
    }
}

#[test]
fn oracle_catches_deliberately_broken_any_assertion_verifier() {
    let grants = vec![Right::spool_write("org/acme")];
    let assertions = vec![
        Right::spool_read("org/acme"),
        Right::spool_admin("org/mallory"),
    ];

    let deliberately_broken_accepts = assertions
        .iter()
        .any(|assertion| oracle_covers(&grants, assertion));
    assert!(
        deliberately_broken_accepts,
        "the broken any-match verifier must accept the mixed set"
    );
    assert!(
        !oracle_accepts(&grants, &assertions),
        "the all-assertions oracle must catch the deliberately broken verifier"
    );
    assert!(
        !production_accepts(&grants, &assertions).expect("real verifier returns a decision"),
        "the production verifier must reject the same known out-of-envelope assertion"
    );
    println!("BROKEN_VERIFIER_SANITY=CAUGHT shape=any-match-mixed-set production=REJECT");
}
