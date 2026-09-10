//! Client-side Biscuit attenuation helpers for the agent flow.
//!
//! Spawning a sub-agent in Heddle doesn't require a server round trip:
//! the parent process appends an attenuation block to its own Biscuit and
//! binds a fresh child proof key to that block. The agent receives the
//! resulting bytes plus only its child private key. The coordinated, pending
//! Weft PR HeddleCo/weft#577 enforces every block's checks and key transition;
//! this client half is HeddleCo/heddle#1022 and must merge with it.
//!
//! See `.agents/agent-attenuation.md` for cookbook recipes (read-only
//! agent, single-repo agent, time-bounded inspector, sub-sub-agent
//! chain).

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use crypto::{Ed25519Signer, Signer};

/// Request-time CI verdict action, narrower than general review decisions.
pub const CI_VERDICT_WRITE_ACTION: &str = "ci-verdict:write";
pub const CI_VERDICT_WRITE_OPERATION: &str = "CiVerdictWrite";

const TEMPLATE_READ_OPERATIONS: &[&str] = &[
    "DescribeEndpoint",
    "ResolveResources",
    "Fetch",
    "ObserveIdentity",
    "ObserveOwnership",
    "ObserveWorkspace",
    "ObserveSpool",
    "ObserveThreads",
    "ObserveThread",
    "ObserveCollaboration",
    "ObserveAnalysis",
    "ReadContent",
    "ReadArtifact",
    "ObserveCheckouts",
    "ObserveRuns",
    "ObserveAttention",
];
const TEMPLATE_CONTRIBUTOR_WRITES: &[&str] = &[
    "StartThread",
    "RenameThread",
    "ChangeLifecycle",
    "PublishContent",
    "LandThread",
    "PutContext",
    "OpenDiscussion",
    "AppendTurn",
    "ResolveDiscussion",
    "RecordReview",
    "CreateSpool",
    "Capture",
    "Refresh",
    "Resolve",
    "LandCheckout",
    "ClaimCheckoutWriter",
    "ReleaseCheckoutWriter",
    CI_VERDICT_WRITE_OPERATION,
];
const TEMPLATE_CI_LANDING_WRITES: &[&str] = &["LandThread", "PublishContent"];
const TEMPLATE_RUNNER_WRITES: &[&str] = &[CI_VERDICT_WRITE_OPERATION];

/// Optional named restrictions. Omitting a template inherits parent authority.
/// Explicit operations may narrow a chosen template; no preset expands a parent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentTemplate {
    Reviewer,
    Contributor,
    CiLanding,
    Runner,
}
impl AgentTemplate {
    #[cfg(test)]
    pub const ALL: [Self; 4] = [
        Self::Reviewer,
        Self::Runner,
        Self::CiLanding,
        Self::Contributor,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reviewer => "reviewer",
            Self::Contributor => "contributor",
            Self::CiLanding => "ci-landing",
            Self::Runner => "runner",
        }
    }
    pub fn operations(&self) -> Vec<String> {
        let mut set: std::collections::BTreeSet<&str> =
            TEMPLATE_READ_OPERATIONS.iter().copied().collect();
        match self {
            Self::Reviewer => {}
            Self::Contributor => set.extend(TEMPLATE_CONTRIBUTOR_WRITES.iter().copied()),
            Self::CiLanding => set.extend(TEMPLATE_CI_LANDING_WRITES.iter().copied()),
            Self::Runner => {
                set.clear();
                set.extend(TEMPLATE_RUNNER_WRITES.iter().copied());
            }
        }
        set.into_iter().map(str::to_string).collect()
    }
}

/// Restrictions applied to a sub-agent's Biscuit. Constructed via
/// [`AgentAttenuation::time_bounded`] for the simplest case (no
/// operation/resource narrowing) or built up field-by-field for
/// richer restrictions.
///
/// Mirrors the server-side `weft_server::biscuit::AgentAttenuation`
/// shape — duplicated here because `server` is a heavy dep
/// (sqlx, axum, ...) we don't want to pull into the CLI's
/// production binary just for the attenuation machinery.
#[derive(Debug, Clone)]
pub struct AgentAttenuation {
    /// Stable id of the spawned agent — emitted as an `agent($id)`
    /// fact for audit trails. A reasonable default is
    /// `format!("agent-{}", uuid::Uuid::new_v4())`.
    pub agent_id: String,
    /// Hard expiry for this attenuation chain. The verifier injects
    /// `time(now())` on every authorized request; if it's past
    /// `expires_at`, the chain rejects regardless of the parent's
    /// own expiry.
    pub expires_at: DateTime<Utc>,
    /// When `Some`, the agent is restricted to the listed hosted operations.
    /// Each entry is a bare method name (e.g. `"GetState"`, `"ListRefs"`) or
    /// a verifier operation such as `"CiVerdictWrite"`.
    pub allowed_operations: Option<Vec<String>>,
    /// When `Some`, the agent is restricted to resources whose path matches
    /// one of the entries. Format: `(kind, path)` where
    /// `kind ∈ {"repo", "namespace", "spool"}`. Emits an ENFORCEABLE
    /// `check if resource($k, $p), …` caveat against the resource fact the
    /// server injects per request (weft#644). A `repo` or `spool` entry matches
    /// that exact path or any subtree path; a `namespace` entry matches the
    /// whole `<namespace>/` repo subtree. An entry rejects a request whose
    /// target the caveat does not cover; a full-authority token (`None`) is
    /// unaffected because facts never reject, only caveats do.
    pub allowed_resources: Option<Vec<(String, String)>>,
    /// Resource scopes recorded as `agent_scope(kind, path)` facts for the
    /// audit trail and for client-side sub-derivation narrowing checks
    /// (`validate_scope_narrowing`). Enforcement rides on `allowed_resources`
    /// above — these facts are metadata, not the caveat.
    pub declared_scopes: Vec<(String, String)>,
}

impl AgentAttenuation {
    /// Time-bounded attenuation with no further restrictions. The
    /// agent inherits the full set of rights from the parent.
    #[cfg(test)]
    pub fn time_bounded(agent_id: impl Into<String>, expires_at: DateTime<Utc>) -> Self {
        Self {
            agent_id: agent_id.into(),
            expires_at,
            allowed_operations: None,
            allowed_resources: None,
            declared_scopes: Vec::new(),
        }
    }
}

/// Bind the persisted account credential to the local proof key and lifetime.
/// Account authority remains inherited; account state and signed owner transitions
/// are checked by the host rather than hidden mandatory agent denials.
pub(crate) fn restrict_agent_account_root(
    root_token: &str,
    signer: &Ed25519Signer,
    expires_at: DateTime<Utc>,
) -> Result<String> {
    attenuate_for_agent(
        root_token,
        AgentAttenuation {
            agent_id: "local-agent-root".to_string(),
            expires_at,
            allowed_operations: None,
            allowed_resources: None,
            declared_scopes: Vec::new(),
        },
        signer,
        signer.public_key(),
    )
}

/// Attenuate a parent Biscuit (decoded base64 string) with the
/// supplied restrictions and return the attenuated Biscuit's
/// base64-encoded bytes.
///
/// Uses `UnverifiedBiscuit` because attenuation appends a new block
/// to bytes the parent already holds; the new block's signature
/// chains off the parent's keys, and the server validates the full
/// chain against its trust list when the agent presents the token.
/// The CLI never holds the server's signing key.
pub fn attenuate_for_agent(
    parent_token_b64: &str,
    restrictions: AgentAttenuation,
    parent_signer: &Ed25519Signer,
    child_public_key: &[u8],
) -> Result<String> {
    if child_public_key.len() != 32 {
        bail!("child PoP public key must be 32 bytes");
    }
    let effective_parent_key = effective_pop_public_key_hex(parent_token_b64)
        .context("resolve parent token's effective PoP key")?;
    if !effective_parent_key.eq_ignore_ascii_case(&hex::encode(parent_signer.public_key())) {
        bail!("parent signer does not match the parent token's effective PoP key");
    }
    let child: &[u8; 32] = child_public_key
        .try_into()
        .context("child PoP public key must be 32 bytes")?;
    let signature = parent_signer
        .sign(
            &biscuit_verifier::key_delegation::statement(parent_token_b64, child)
                .context("prepare child proof-key statement")?,
        )
        .context("sign child proof-key delegation")?;
    let signature: &[u8; 64] = signature
        .as_slice()
        .try_into()
        .context("Ed25519 delegation signature must be 64 bytes")?;
    biscuit_verifier::key_delegation::append(
        parent_token_b64,
        child,
        signature,
        build_attenuation_block(&restrictions)?,
    )
    .context("append child proof-key delegation")
}

/// Versioned byte domain shared with weft's delegated-PoP verifier. The
/// payload is exactly `domain || raw parent revocation id || raw child key`.
pub(crate) const POP_DELEGATION_DOMAIN: &[u8] = b"heddle-pop-delegation-v1\0";

pub(crate) fn pop_delegation_payload(
    parent_revocation_id: &[u8],
    child_public_key: &[u8],
) -> Vec<u8> {
    [
        POP_DELEGATION_DOMAIN,
        parent_revocation_id,
        child_public_key,
    ]
    .concat()
}

/// Resolve and verify the leaf PoP key of a root or delegated token without
/// trusting its server signature. Callers use this only after obtaining the
/// token from their local credential store. The coordinated, pending server
/// PR HeddleCo/weft#577 performs the same walk after full Biscuit verification.
pub(crate) fn effective_pop_public_key_hex(token_b64: &str) -> Result<String> {
    use biscuit_auth::builder::{BlockBuilder, Term};

    let biscuit = biscuit_auth::UnverifiedBiscuit::from_base64(token_b64.as_bytes())
        .context("parse Biscuit while resolving its proof key")?;
    let authority_source = biscuit
        .print_block_source(0)
        .context("read Biscuit authority block")?;
    let authority = BlockBuilder::new()
        .code(&authority_source)
        .context("parse Biscuit authority block")?;
    if authority
        .facts
        .iter()
        .any(|fact| fact.predicate.name == "pop_delegation")
    {
        bail!("pop_delegation is valid only in post-authority blocks");
    }
    let authority_keys = authority
        .facts
        .iter()
        .filter_map(|fact| {
            match (
                fact.predicate.name.as_str(),
                fact.predicate.terms.as_slice(),
            ) {
                ("device_pop_key", [Term::Str(key)]) => Some(key.clone()),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    let [authority_key_hex] = authority_keys.as_slice() else {
        bail!("Biscuit authority block must contain exactly one device_pop_key fact");
    };
    let mut effective_key = decode_fixed_hex(authority_key_hex, 32, "device_pop_key")?;

    let revocation_ids = biscuit.revocation_identifiers();
    for index in 1..biscuit.block_count() {
        let source = biscuit
            .print_block_source(index)
            .with_context(|| format!("read Biscuit attenuation block {index}"))?;
        let block = BlockBuilder::new()
            .code(&source)
            .with_context(|| format!("parse Biscuit attenuation block {index}"))?;
        let delegations = block
            .facts
            .iter()
            .filter(|fact| fact.predicate.name == "pop_delegation")
            .collect::<Vec<_>>();
        let [delegation] = delegations.as_slice() else {
            bail!("attenuation block {index} must contain exactly one pop_delegation fact");
        };
        let [Term::Str(parent), Term::Str(child), Term::Str(signature)] =
            delegation.predicate.terms.as_slice()
        else {
            bail!("attenuation block {index} has malformed pop_delegation fact");
        };
        let parent = hex::decode(parent).context("pop_delegation parent is not hex")?;
        let expected_parent = revocation_ids
            .get(index - 1)
            .with_context(|| format!("attenuation block {index} has no preceding block"))?;
        if parent.as_slice() != *expected_parent {
            bail!(
                "attenuation block {index} pop_delegation must reference its immediately preceding block"
            );
        }
        let child = decode_fixed_hex(child, 32, "pop_delegation child public key")?;
        let signature = decode_fixed_hex(signature, 64, "pop_delegation signature")?;
        Ed25519Signer::verify_with_public_key(
            &pop_delegation_payload(&parent, &child),
            &effective_key,
            &signature,
        )
        .context("pop_delegation signature does not match the effective parent key")?;
        effective_key = child;
    }
    Ok(hex::encode(effective_key))
}

/// Read the one stable subject asserted by a Biscuit authority block.
///
/// The authority subject is the authenticated principal used by request
/// signing. Attenuation blocks may narrow authorization, but cannot replace
/// the authority identity.
pub(crate) fn authenticated_subject(token_b64: &str) -> Result<String> {
    use biscuit_auth::builder::{BlockBuilder, Term};

    let biscuit = biscuit_auth::UnverifiedBiscuit::from_base64(token_b64.as_bytes())
        .context("parse Biscuit while resolving its authenticated subject")?;
    let authority_source = biscuit
        .print_block_source(0)
        .context("read Biscuit authority block")?;
    let authority = BlockBuilder::new()
        .code(&authority_source)
        .context("parse Biscuit authority block")?;
    let subjects = authority
        .facts
        .iter()
        .filter_map(|fact| {
            match (
                fact.predicate.name.as_str(),
                fact.predicate.terms.as_slice(),
            ) {
                ("user", [Term::Str(subject)]) if !subject.trim().is_empty() => {
                    Some(subject.clone())
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    let [subject] = subjects.as_slice() else {
        bail!("Biscuit authority block must contain exactly one non-empty user(subject) fact");
    };
    Ok(subject.clone())
}

fn decode_fixed_hex(value: &str, expected_len: usize, label: &str) -> Result<Vec<u8>> {
    let decoded = hex::decode(value).with_context(|| format!("{label} is not valid hex"))?;
    if decoded.len() != expected_len {
        bail!("{label} must decode to {expected_len} bytes");
    }
    Ok(decoded)
}

/// Build the BlockBuilder that holds the attenuation's facts +
/// checks. Pulled out so the agent-side code path can be unit-tested
/// without round-tripping through a parent token.
fn build_attenuation_block(
    restrictions: &AgentAttenuation,
) -> Result<biscuit_auth::builder::BlockBuilder> {
    let shared = biscuit_verifier::delegation::AgentAttenuation {
        agent_id: restrictions.agent_id.clone(),
        expires_at: restrictions.expires_at,
        allowed_operations: restrictions.allowed_operations.clone(),
        allowed_resources: restrictions.allowed_resources.clone(),
    };
    let mut block = shared.block().context("shared agent attenuation")?;
    for (kind, path) in &restrictions.declared_scopes {
        block = block
            .fact(
                format!(
                    "agent_scope({}, {})",
                    biscuit_string(kind),
                    biscuit_string(path)
                )
                .as_str(),
            )
            .context("forward-compatible resource scope")?;
    }
    Ok(block)
}

fn biscuit_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Convenience constructor for the common "spawn an agent for the
/// next N hours, no further restrictions" case.
#[cfg(test)]
pub fn time_bounded(
    parent_token_b64: &str,
    agent_id: impl Into<String>,
    expires_at: DateTime<Utc>,
    parent_signer: &Ed25519Signer,
    child_public_key: &[u8],
) -> Result<String> {
    attenuate_for_agent(
        parent_token_b64,
        AgentAttenuation::time_bounded(agent_id, expires_at),
        parent_signer,
        child_public_key,
    )
}

/// Convenience: attenuate to a read-only sub-agent on a single repo
/// for `duration_hours`. Emits both an operation allowlist (limited
/// to common read RPCs) and a resource allowlist scoped to the
/// repo's path. Use as a starting point — for finer-grained access,
/// build the [`AgentAttenuation`] directly.
#[cfg(test)]
pub fn read_only_repo_agent(
    parent_token_b64: &str,
    agent_id: impl Into<String>,
    repo_path: impl Into<String>,
    duration_hours: i64,
    parent_signer: &Ed25519Signer,
    child_public_key: &[u8],
) -> Result<String> {
    attenuate_for_agent(
        parent_token_b64,
        AgentAttenuation {
            agent_id: agent_id.into(),
            expires_at: Utc::now() + chrono::Duration::hours(duration_hours),
            allowed_operations: Some(vec![
                "GetState".to_string(),
                "GetTree".to_string(),
                "GetBlob".to_string(),
                "GetCompare".to_string(),
                "GetDiff".to_string(),
                "ListRefs".to_string(),
                "ListStates".to_string(),
                "ListContext".to_string(),
            ]),
            allowed_resources: Some(vec![("repo".to_string(), repo_path.into())]),
            declared_scopes: Vec::new(),
        },
        parent_signer,
        child_public_key,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use biscuit_auth::{Biscuit, KeyPair, builder::AuthorizerBuilder, datalog::RunLimits};

    use super::*;

    #[test]
    fn templates_name_real_native_operations() {
        let methods: BTreeSet<&str> = api::v2::ALL_METHODS
            .iter()
            .filter_map(|method| method.path.rsplit('/').next())
            .collect();
        for template in AgentTemplate::ALL {
            for operation in template.operations() {
                assert!(
                    operation == CI_VERDICT_WRITE_OPERATION || methods.contains(operation.as_str()),
                    "unknown native operation: {operation}"
                );
            }
        }
    }

    #[test]
    fn template_privilege_ordering_holds() {
        let reviewer: BTreeSet<String> = AgentTemplate::Reviewer.operations().into_iter().collect();
        let contributor: BTreeSet<String> = AgentTemplate::Contributor
            .operations()
            .into_iter()
            .collect();
        let ci: BTreeSet<String> = AgentTemplate::CiLanding.operations().into_iter().collect();
        let runner: BTreeSet<String> = AgentTemplate::Runner.operations().into_iter().collect();
        // Reviewer is the read-only floor for contributor and CI landing.
        assert!(reviewer.is_subset(&contributor));
        assert!(reviewer.is_subset(&ci));
        // CI landing sits between reviewer and contributor: it adds only
        // Push/UpdateRef, so it is a proper subset of contributor (the ceiling).
        assert!(ci.is_subset(&contributor));
        // Contributor carries collaboration writes CI landing does not.
        assert!(contributor.contains("OpenDiscussion"));
        assert!(!ci.contains("OpenDiscussion"));
        assert!(runner.is_subset(&contributor));
    }

    #[test]
    fn runner_template_can_write_verdicts_but_cannot_write_source() {
        let runner: BTreeSet<String> = AgentTemplate::Runner.operations().into_iter().collect();

        assert_eq!(
            runner,
            BTreeSet::from([CI_VERDICT_WRITE_OPERATION.to_string()])
        );
        for forbidden in ["Push", "UpdateRef", "spool:write", "spool-write"] {
            assert!(
                !runner.contains(forbidden),
                "runner operation ceiling must deny source-write operation {forbidden}"
            );
        }
    }

    /// Mint a parent Biscuit using biscuit-auth directly. We avoid
    /// pulling `weft_server::biscuit::mint` because that would force
    /// server into the regular dep graph; the goal here is to
    /// keep the CLI small.
    fn fresh_parent_token() -> (String, KeyPair, Ed25519Signer) {
        let kp = KeyPair::new();
        let parent_pop = Ed25519Signer::generate().expect("parent PoP key");
        let mut builder = biscuit_auth::Biscuit::builder();
        builder = builder.fact(r#"user("alice")"#).expect("user fact");
        builder = builder.fact(r#"session("sess-1")"#).expect("session fact");
        builder = builder
            .fact(
                format!(
                    "device_pop_key(\"{}\")",
                    hex::encode(parent_pop.public_key())
                )
                .as_str(),
            )
            .expect("device PoP fact");
        let exp = chrono::Utc::now() + chrono::Duration::hours(2);
        builder = builder
            .fact(format!("expires_at({})", exp.to_rfc3339()).as_str())
            .expect("expires_at fact");
        builder = builder
            .check(format!("check if time($now), $now < {}", exp.to_rfc3339()).as_str())
            .expect("expiry check");
        let biscuit = builder.build(&kp).expect("build parent biscuit");
        (biscuit.to_base64().expect("to_base64"), kp, parent_pop)
    }

    #[test]
    fn pop_delegation_payload_layout_matches_the_versioned_server_contract() {
        let parent = [0x11; 64];
        let child = [0x22; 32];
        let payload = pop_delegation_payload(&parent, &child);

        assert_eq!(
            &payload[..POP_DELEGATION_DOMAIN.len()],
            POP_DELEGATION_DOMAIN
        );
        assert_eq!(
            &payload[POP_DELEGATION_DOMAIN.len()..POP_DELEGATION_DOMAIN.len() + parent.len()],
            parent
        );
        assert_eq!(
            &payload[POP_DELEGATION_DOMAIN.len() + parent.len()..],
            child
        );
        assert_eq!(payload.len(), POP_DELEGATION_DOMAIN.len() + 64 + 32);
    }

    #[test]
    fn authenticated_subject_is_unique_authority_owned_and_required() {
        let (authority_token, _, _) = fresh_parent_token();
        assert_eq!(
            authenticated_subject(&authority_token).expect("authority subject"),
            "alice"
        );

        let attenuated = biscuit_auth::UnverifiedBiscuit::from_base64(authority_token.as_bytes())
            .expect("parse authority token")
            .append(
                biscuit_auth::builder::BlockBuilder::new()
                    .fact(r#"user("mallory")"#)
                    .expect("attenuation-local user fact"),
            )
            .expect("append attenuation")
            .to_base64()
            .expect("encode attenuation");
        assert_eq!(
            authenticated_subject(&attenuated).expect("authority remains authoritative"),
            "alice"
        );

        let missing = Biscuit::builder()
            .fact(r#"session("sess-1")"#)
            .expect("session fact")
            .build(&KeyPair::new())
            .expect("build missing-subject token")
            .to_base64()
            .expect("encode missing-subject token");
        assert!(authenticated_subject(&missing).is_err());

        let duplicate = Biscuit::builder()
            .fact(r#"user("alice")"#)
            .expect("first user fact")
            .fact(r#"user("mallory")"#)
            .expect("second user fact")
            .build(&KeyPair::new())
            .expect("build duplicate-subject token")
            .to_base64()
            .expect("encode duplicate-subject token");
        assert!(authenticated_subject(&duplicate).is_err());
    }

    #[test]
    fn effective_pop_key_rejects_a_delegationless_attenuation_block() {
        let signer = Ed25519Signer::generate().expect("root PoP key");
        let token = Biscuit::builder()
            .fact(r#"user("alice")"#)
            .expect("user fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
            .expect("root PoP fact")
            .build(&KeyPair::new())
            .expect("build root")
            .append(
                biscuit_auth::builder::BlockBuilder::new()
                    .fact(r#"agent("raw-child")"#)
                    .expect("raw attenuation fact"),
            )
            .expect("append raw attenuation")
            .to_base64()
            .expect("encode raw attenuation");

        let error = effective_pop_public_key_hex(&token)
            .expect_err("a child block without a key transition must fail closed");
        assert!(error.to_string().contains("exactly one pop_delegation"));
    }

    #[test]
    fn effective_pop_key_rejects_duplicate_authority_anchors() {
        let first = Ed25519Signer::generate().expect("first root PoP key");
        let second = Ed25519Signer::generate().expect("second root PoP key");
        let token = Biscuit::builder()
            .fact(r#"user("alice")"#)
            .expect("user fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(first.public_key())).as_str())
            .expect("first root PoP fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(second.public_key())).as_str())
            .expect("second root PoP fact")
            .build(&KeyPair::new())
            .expect("build root")
            .to_base64()
            .expect("encode root");

        let error = effective_pop_public_key_hex(&token)
            .expect_err("multiple authority proof anchors must fail closed");
        assert!(error.to_string().contains("exactly one device_pop_key"));
    }

    #[test]
    fn effective_pop_key_rejects_authority_block_delegations() {
        let signer = Ed25519Signer::generate().expect("root PoP key");
        let token = Biscuit::builder()
            .fact(r#"user("alice")"#)
            .expect("user fact")
            .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
            .expect("root PoP fact")
            .fact(r#"pop_delegation("parent", "child", "signature")"#)
            .expect("misplaced delegation fact")
            .build(&KeyPair::new())
            .expect("build malformed root")
            .to_base64()
            .expect("encode malformed root");

        let error = effective_pop_public_key_hex(&token)
            .expect_err("an authority-block delegation must fail closed");
        assert!(error.to_string().contains("only in post-authority blocks"));
    }

    #[test]
    fn every_public_derivation_entrypoint_rejects_the_wrong_parent_signer() {
        let (parent, _root, _parent_pop) = fresh_parent_token();
        let wrong_parent_pop = Ed25519Signer::generate().expect("wrong parent PoP key");
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let expires_at = Utc::now() + chrono::Duration::hours(1);

        let direct_error = attenuate_for_agent(
            &parent,
            AgentAttenuation::time_bounded("direct", expires_at),
            &wrong_parent_pop,
            child_pop.public_key(),
        )
        .expect_err("direct derivation must reject a non-matching parent signer");
        assert!(
            direct_error
                .to_string()
                .contains("parent signer does not match")
        );

        let time_bounded_error = time_bounded(
            &parent,
            "time-bounded",
            expires_at,
            &wrong_parent_pop,
            child_pop.public_key(),
        )
        .expect_err("time-bounded derivation must use the validated chokepoint");
        assert!(
            time_bounded_error
                .to_string()
                .contains("parent signer does not match")
        );

        let read_only_error = read_only_repo_agent(
            &parent,
            "read-only",
            "acme/heddle",
            1,
            &wrong_parent_pop,
            child_pop.public_key(),
        )
        .expect_err("read-only derivation must use the validated chokepoint");
        assert!(
            read_only_error
                .to_string()
                .contains("parent signer does not match")
        );
    }

    /// Exercise the same Biscuit chain verification and request-fact shape as
    /// the hosted server (`time` + bare hosted `operation`).
    fn server_authorizes(
        token: &str,
        root: &KeyPair,
        operation: &str,
        now: DateTime<Utc>,
    ) -> Result<(), biscuit_auth::error::Token> {
        let root_public = root.public();
        let biscuit = Biscuit::from_base64(token, move |_| Ok(root_public))?;
        let mut authorizer = AuthorizerBuilder::new()
            .set_limits(RunLimits {
                max_facts: 1000,
                max_iterations: 100,
                max_time: std::time::Duration::from_secs(1),
            })
            .fact(format!("time({})", now.to_rfc3339()).as_str())?
            .fact(format!("operation({})", biscuit_string(operation)).as_str())?
            .policy("allow if true")?
            .build(&biscuit)?;
        authorizer.authorize().map(|_| ())
    }

    #[test]
    fn attenuate_appends_a_block_with_agent_marker() {
        let (parent, _kp, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let attenuated = time_bounded(
            &parent,
            "agent-1",
            Utc::now() + chrono::Duration::hours(2),
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("attenuate");
        // The attenuated bytes are strictly longer than the parent's
        // (the new block adds bytes). End-to-end verify happens in
        // the integration tests where a real server's keypair is
        // available.
        assert!(attenuated.len() > parent.len());
    }

    #[test]
    fn time_bounded_with_past_expiry_still_attenuates() {
        // The helper itself doesn't enforce expiry — that's the
        // verifier's job. A past-expiry attenuation builds fine but
        // gets rejected at verify time. This test just guards
        // against the helper accidentally rejecting timestamps it
        // doesn't like.
        let (parent, _kp, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let result = time_bounded(
            &parent,
            "agent-1",
            Utc::now() - chrono::Duration::hours(1),
            &parent_pop,
            child_pop.public_key(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn read_only_repo_agent_builds_with_op_and_resource_restrictions() {
        let (parent, _kp, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let attenuated = read_only_repo_agent(
            &parent,
            "agent-r",
            "org/acme/heddle",
            2,
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("attenuate");
        // Sanity: the attenuated bytes parse back as a Biscuit (via
        // the unverified path so we don't need the parent's root
        // key). The verifier round-trip is exercised in the
        // integration tests.
        let parsed =
            biscuit_auth::UnverifiedBiscuit::from_base64(attenuated.as_bytes()).expect("parse");
        assert!(parsed.block_count() >= 2, "expected attenuation block");
    }

    #[test]
    fn operation_values_are_literals_and_cannot_broaden_authority() {
        let (parent, root, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-1".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec![r#"x" || true || $op == "y"#.to_string()]),
                allowed_resources: None,
                declared_scopes: Vec::new(),
            },
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("shared builder safely quotes literal operation values");
        for operation in ["x", "y", "DeleteSpool"] {
            assert!(
                server_authorizes(&child, &root, operation, Utc::now()).is_err(),
                "literal operation cannot grant {operation}"
            );
        }
    }

    #[test]
    fn accepts_normal_operation_names() {
        let (parent, _kp, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-1".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec!["GetState".to_string(), "ListRefs".to_string()]),
                allowed_resources: Some(vec![("repo".to_string(), "org/acme/heddle".to_string())]),
                declared_scopes: Vec::new(),
            },
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("normal ops must attenuate");
    }

    #[test]
    fn unscoped_child_inherits_delegated_admin_authority() {
        let (parent, root, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child");
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation::time_bounded("admin-agent", Utc::now() + chrono::Duration::hours(1)),
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("derived child");
        for operation in [
            "CreateInvite",
            "SetMembership",
            "RevokeCredential",
            "BootstrapOwnership",
        ] {
            server_authorizes(&child, &root, operation, Utc::now())
                .expect("parent authority inherited");
        }
    }

    #[test]
    fn server_rejects_child_after_attenuation_ttl() {
        let (parent, root, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let expires_at = Utc::now() + chrono::Duration::minutes(5);
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-expiring".to_string(),
                expires_at,
                allowed_operations: Some(vec!["GetState".to_string()]),
                allowed_resources: None,
                declared_scopes: Vec::new(),
            },
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("derive expiring child");

        server_authorizes(&child, &root, "GetState", Utc::now())
            .expect("server accepts child before expiry");
        assert!(
            server_authorizes(
                &child,
                &root,
                "GetState",
                expires_at + chrono::Duration::seconds(1)
            )
            .is_err(),
            "server must reject child after TTL"
        );
    }

    #[test]
    fn sub_derivation_intersects_every_operation_block() {
        let (parent, root, root_pop) = fresh_parent_token();
        let parent_agent_pop = Ed25519Signer::generate().expect("parent-agent PoP key");
        let subagent_pop = Ed25519Signer::generate().expect("subagent PoP key");
        let parent_agent = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-parent".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec!["Push".to_string()]),
                allowed_resources: None,
                declared_scopes: vec![("repo".to_string(), "acme/heddle".to_string())],
            },
            &root_pop,
            parent_agent_pop.public_key(),
        )
        .expect("derive parent agent");
        assert_eq!(
            effective_pop_public_key_hex(&parent_agent).expect("resolve parent-agent PoP key"),
            hex::encode(parent_agent_pop.public_key())
        );
        let wrong_signer_error = attenuate_for_agent(
            &parent_agent,
            AgentAttenuation {
                agent_id: "agent-wrong-signer".to_string(),
                expires_at: Utc::now() + chrono::Duration::minutes(30),
                allowed_operations: Some(vec!["Push".to_string()]),
                allowed_resources: None,
                declared_scopes: Vec::new(),
            },
            &root_pop,
            subagent_pop.public_key(),
        )
        .expect_err("an attenuated parent requires its current leaf signer");
        assert!(
            wrong_signer_error
                .to_string()
                .contains("parent signer does not match")
        );
        let subagent = attenuate_for_agent(
            &parent_agent,
            AgentAttenuation {
                agent_id: "agent-child".to_string(),
                expires_at: Utc::now() + chrono::Duration::minutes(30),
                // Attempt to add GetState cannot override the parent's block.
                allowed_operations: Some(vec!["Push".to_string(), "GetState".to_string()]),
                allowed_resources: None,
                declared_scopes: vec![("repo".to_string(), "acme/heddle/subdir".to_string())],
            },
            &parent_agent_pop,
            subagent_pop.public_key(),
        )
        .expect("derive subagent");

        server_authorizes(&subagent, &root, "Push", Utc::now())
            .expect("operation retained by both blocks is allowed");
        assert!(
            server_authorizes(&subagent, &root, "GetState", Utc::now()).is_err(),
            "a child cannot widen its parent's operation set"
        );
        let parsed = biscuit_auth::UnverifiedBiscuit::from_base64(subagent.as_bytes())
            .expect("parse subagent");
        assert_eq!(parsed.block_count(), 3, "authority plus two agent hops");
        let parent_source = parsed.print_block_source(1).expect("parent block source");
        let child_source = parsed.print_block_source(2).expect("child block source");
        assert!(parent_source.contains("agent_scope(\"repo\", \"acme/heddle\")"));
        assert!(child_source.contains("agent_scope(\"repo\", \"acme/heddle/subdir\")"));
    }

    /// Like `server_authorizes` but also injects the per-request
    /// `resource(kind, path)` fact the weft verifier adds (weft#644), so
    /// resource-scope caveats are actually exercised end-to-end.
    fn server_authorizes_resource(
        token: &str,
        root: &KeyPair,
        operation: &str,
        resource: (&str, &str),
        now: DateTime<Utc>,
    ) -> Result<(), biscuit_auth::error::Token> {
        let root_public = root.public();
        let biscuit = Biscuit::from_base64(token, move |_| Ok(root_public))?;
        let mut authorizer = AuthorizerBuilder::new()
            .set_limits(RunLimits {
                max_facts: 1000,
                max_iterations: 100,
                max_time: std::time::Duration::from_secs(1),
            })
            .fact(format!("time({})", now.to_rfc3339()).as_str())?
            .fact(format!("operation({})", biscuit_string(operation)).as_str())?
            .fact(
                format!(
                    "resource({}, {})",
                    biscuit_string(resource.0),
                    biscuit_string(resource.1)
                )
                .as_str(),
            )?
            .policy("allow if true")?
            .build(&biscuit)?;
        authorizer.authorize().map(|_| ())
    }

    #[test]
    fn repo_scope_caveat_admits_in_scope_repo_and_rejects_siblings() {
        let (parent, root, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-repo".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec!["GetState".to_string()]),
                allowed_resources: Some(vec![("repo".to_string(), "alice/repoA".to_string())]),
                declared_scopes: vec![("repo".to_string(), "alice/repoA".to_string())],
            },
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("derive repo-scoped child");

        // In scope: the exact repo and any nested subtree path.
        server_authorizes_resource(
            &child,
            &root,
            "GetState",
            ("repo", "alice/repoA"),
            Utc::now(),
        )
        .expect("in-scope repo is admitted");
        server_authorizes_resource(
            &child,
            &root,
            "GetState",
            ("repo", "alice/repoA/pkg"),
            Utc::now(),
        )
        .expect("in-scope subtree is admitted");
        // Out of scope: a sibling repo in the same namespace is rejected.
        assert!(
            server_authorizes_resource(
                &child,
                &root,
                "GetState",
                ("repo", "alice/repoB"),
                Utc::now()
            )
            .is_err(),
            "an out-of-scope sibling repo must be rejected by the resource caveat"
        );
        // Fail closed when no resource fact is injected at all.
        assert!(
            server_authorizes(&child, &root, "GetState", Utc::now()).is_err(),
            "a resource-scoped caveat must fail closed when the request has no target"
        );
    }

    #[test]
    fn spool_scope_includes_descendants_and_excludes_siblings() {
        let (parent, root, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-ns".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec!["ObserveThread".to_string()]),
                allowed_resources: Some(vec![("spool".to_string(), "alice".to_string())]),
                declared_scopes: vec![("spool".to_string(), "alice".to_string())],
            },
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("derive namespace-scoped child");

        // Every repo under the namespace is reachable; a repo outside is not.
        server_authorizes_resource(
            &child,
            &root,
            "ObserveThread",
            ("spool", "alice/repoA"),
            Utc::now(),
        )
        .expect("repoA under the namespace is admitted");
        server_authorizes_resource(
            &child,
            &root,
            "ObserveThread",
            ("spool", "alice/repoB"),
            Utc::now(),
        )
        .expect("repoB under the namespace is admitted");
        assert!(
            server_authorizes_resource(
                &child,
                &root,
                "ObserveThread",
                ("spool", "bob/repoC"),
                Utc::now()
            )
            .is_err(),
            "a repo outside the namespace must be rejected"
        );
    }
}
