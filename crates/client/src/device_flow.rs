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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentAuthOperationDisposition {
    ReviewedSafe,
    Denied,
}

/// Exhaustive agent-policy classification for `IdentityService`.
///
/// The exact-set test below compares this table with the shared API descriptor,
/// so adding an identity RPC requires an explicit decision before derived-agent
/// CI can pass.
const AUTH_SERVICE_AGENT_POLICY: &[(&str, AgentAuthOperationDisposition)] = &[
    (
        "BeginWebAuthnRegistration",
        AgentAuthOperationDisposition::Denied,
    ),
    ("RegisterPublicKey", AgentAuthOperationDisposition::Denied),
    (
        "BeginWebAuthnAuthentication",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    ("ClaimHandle", AgentAuthOperationDisposition::Denied),
    (
        "FinishWebAuthnAuthentication",
        // The WebAuthn ceremony, not an attached bearer, proves this request.
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "CreateDeviceAuthorization",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "ApproveDeviceAuthorization",
        AgentAuthOperationDisposition::Denied,
    ),
    (
        "ExchangeDeviceAuthorization",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "WaitForDeviceAuthorization",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    ("RotateCredential", AgentAuthOperationDisposition::Denied),
    ("RevokeCredential", AgentAuthOperationDisposition::Denied),
    (
        "CreateServiceAccount",
        AgentAuthOperationDisposition::Denied,
    ),
    (
        "IssueServiceAccountCredential",
        AgentAuthOperationDisposition::Denied,
    ),
    (
        "RevokeServiceAccount",
        AgentAuthOperationDisposition::Denied,
    ),
    ("WhoAmI", AgentAuthOperationDisposition::ReviewedSafe),
    // The hosted handler still requires the billing service account's
    // billing:write right; an attached derived bearer cannot create it.
    (
        "RecordSubscription",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "IntrospectCredential",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "ListServiceAccounts",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "ListScopeCapabilities",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    ("LinkOAuthIdentity", AgentAuthOperationDisposition::Denied),
    ("StoreProviderToken", AgentAuthOperationDisposition::Denied),
    (
        "VerifySignupEmail",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "GetInvitationSummary",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "GetHandleStatus",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    ("ListSessions", AgentAuthOperationDisposition::ReviewedSafe),
    ("RevokeSession", AgentAuthOperationDisposition::Denied),
    ("RequestHeldName", AgentAuthOperationDisposition::Denied),
    ("ResolveHandle", AgentAuthOperationDisposition::ReviewedSafe),
    // MintBiscuit authenticates its own keypair/device proof; an attached
    // derived bearer cannot authorize or widen the minted credential.
    ("MintBiscuit", AgentAuthOperationDisposition::ReviewedSafe),
    // Presence currently re-mints an authority token instead of preserving
    // the caller's complete attenuation chain, so agents must not invoke it.
    ("IssuePresenceToken", AgentAuthOperationDisposition::Denied),
    (
        "MintAnonBiscuit",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    (
        "DeclareRecoveryMethod",
        AgentAuthOperationDisposition::Denied,
    ),
    // Recovery execution is authorized by public, independent proof material
    // and the veto-window state rather than by an attached derived bearer.
    ("BeginRecovery", AgentAuthOperationDisposition::ReviewedSafe),
    (
        "SubmitRecoveryProof",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
    ("VetoRecovery", AgentAuthOperationDisposition::ReviewedSafe),
    (
        "CompleteRecovery",
        AgentAuthOperationDisposition::ReviewedSafe,
    ),
];

/// Destructive non-auth methods that remain mandatory denials for every
/// derived token, even when the caller constructs [`AgentAttenuation`]
/// directly without an allowlist.
const NON_AUTH_AGENT_OPERATION_DENY_FLOOR: &[&str] = &["DeleteRepository", "DeleteNamespace"];

fn mandatory_agent_denied_operations() -> impl Iterator<Item = &'static str> {
    NON_AUTH_AGENT_OPERATION_DENY_FLOOR.iter().copied().chain(
        AUTH_SERVICE_AGENT_POLICY
            .iter()
            .filter_map(|(operation, disposition)| {
                (*disposition == AgentAuthOperationDisposition::Denied).then_some(*operation)
            }),
    )
}

/// Curated W1 operation ceiling for `heddle auth derive-agent`.
///
/// `--allow` may select a subset of these methods. Parent and child blocks are
/// both evaluated by the server, so sub-derivation computes an intersection
/// and cannot widen an ancestor's selection.
pub const SAFE_AGENT_OPERATIONS: &[&str] = &[
    // Hosted push and pull.
    "Push",
    "Pull",
    "ListRefs",
    "UpdateRef",
    // Repository reads.
    "GetRefs",
    "ListStates",
    "GetState",
    "GetBlame",
    "ListProvenanceSummaries",
    "GetTree",
    "GetBlob",
    "GetCompare",
    "GetDiff",
    "GetSemanticHotSpots",
    "ListActions",
    // Context reads and writes.
    "ListContext",
    "GetContextHistory",
    "ListContextSuggestions",
    "SetContext",
    "ReviseContext",
    "SupersedeContext",
    // Discussions.
    "OpenDiscussion",
    "AppendTurn",
    "ResolveDiscussion",
    "ListByState",
    "ListBySymbol",
    "GetDiscussion",
    // Session identity.
    "WhoAmI",
];

/// Restrictions applied to a sub-agent's Biscuit. Constructed via
/// [`AgentAttenuation::time_bounded`] for the simplest case (no
/// operation/resource narrowing) or built up field-by-field for
/// richer restrictions.
///
/// Mirrors the server-side `weft_server::biscuit::AgentAttenuation`
/// shape — duplicated here because `server` is a heavy dep
/// (sqlx, tonic, axum, ...) we don't want to pull into the CLI's
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
    /// When `Some`, the agent is restricted to the listed gRPC
    /// operations. Each entry is the bare method name (e.g.
    /// `"GetState"`, `"ListRefs"`).
    pub allowed_operations: Option<Vec<String>>,
    /// When `Some`, the agent is restricted to resources whose path matches
    /// one of the entries. Format: `(kind, path)` where
    /// `kind ∈ {"repo", "namespace"}`. Emits an ENFORCEABLE
    /// `check if resource($k, $p), …` caveat against the `resource("repo", …)`
    /// fact the server injects per request (weft#644): a `repo` entry matches
    /// that exact repo or any subtree path, a `namespace` entry matches the
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
    let unverified = biscuit_auth::UnverifiedBiscuit::from_base64(parent_token_b64.as_bytes())
        .context("parse parent biscuit (unverified)")?;
    let parent_revocation_id = unverified
        .revocation_identifiers()
        .last()
        .context("parent Biscuit has no revocation identifier")?
        .to_vec();
    let signature = parent_signer
        .sign(&pop_delegation_payload(
            &parent_revocation_id,
            child_public_key,
        ))
        .context("sign child PoP delegation")?;
    let mut block = build_attenuation_block(&restrictions)?;
    block = block
        .fact(
            format!(
                "pop_delegation({}, {}, {})",
                biscuit_string(&hex::encode(parent_revocation_id)),
                biscuit_string(&hex::encode(child_public_key)),
                biscuit_string(&hex::encode(signature)),
            )
            .as_str(),
        )
        .context("child PoP delegation fact")?;
    let attenuated = unverified
        .append(block)
        .context("append attenuation block")?;
    attenuated.to_base64().context("encode attenuated biscuit")
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
    // Fail closed on characters that could break out of a Biscuit string
    // literal or inject operators into the DSL before we assemble the block.
    validate_biscuit_token_string("agent_id", &restrictions.agent_id)?;
    if let Some(ops) = &restrictions.allowed_operations {
        for op in ops {
            validate_biscuit_token_string("allowed_operations entry", op)?;
        }
    }
    if let Some(resources) = &restrictions.allowed_resources {
        for (kind, path) in resources {
            validate_biscuit_token_string("resource kind", kind)?;
            validate_biscuit_token_string("resource path", path)?;
        }
    }
    for (kind, path) in &restrictions.declared_scopes {
        validate_biscuit_token_string("scope kind", kind)?;
        validate_biscuit_token_string("scope path", path)?;
    }

    let mut block = biscuit_auth::builder::BlockBuilder::new();
    block = block
        .fact(format!("agent({})", biscuit_string(&restrictions.agent_id)).as_str())
        .context("agent fact")?;
    block = block
        .fact(format!("agent_expires_at({})", restrictions.expires_at.to_rfc3339()).as_str())
        .context("agent expiry fact")?;
    block = block
        .check(
            format!(
                "check if time($now), $now < {}",
                restrictions.expires_at.to_rfc3339()
            )
            .as_str(),
        )
        .context("expiry check")?;
    // These independent checks are deliberately present even when the caller
    // supplies no operation allowlist. They are the mandatory auth-trust,
    // credential, recovery-enrollment, presence, and destructive-operation
    // floor for every token from this primitive.
    for denied in mandatory_agent_denied_operations() {
        block = block
            .check(format!("check if operation($op), $op != {}", biscuit_string(denied)).as_str())
            .context("agent operation deny floor")?;
    }
    if let Some(ops) = &restrictions.allowed_operations {
        let pred = if ops.is_empty() {
            // A syntactically valid predicate that no real gRPC method can
            // match. `Some(vec![])` therefore means deny all, not unrestricted.
            "$op == \"__heddle_no_agent_operations__\"".to_string()
        } else {
            ops.iter()
                .map(|op| format!("$op == {}", biscuit_string(op)))
                .collect::<Vec<_>>()
                .join(" || ")
        };
        block = block
            .check(format!("check if operation($op), {pred}").as_str())
            .context("operation allowlist check")?;
    }
    if let Some(resources) = &restrictions.allowed_resources
        && !resources.is_empty()
    {
        let mut clauses = Vec::new();
        for (kind, path) in resources {
            let prefix = format!("{path}/");
            match kind.as_str() {
                // A namespace grant authorizes the whole repo subtree under it.
                // The server only ever injects `resource("repo", <repo_path>)`
                // facts (weft#644) — it never emits a `resource("namespace", …)`
                // fact — so a `$k == "namespace"` caveat would match no fact and
                // reject EVERY repo RPC. Encode the namespace scope as a repo-path
                // PREFIX caveat against `<namespace>/` so the injected repo fact
                // and this caveat compare like-for-like.
                "namespace" | "ns" => {
                    clauses.push(format!(
                        "($k == \"repo\" && $p.starts_with({prefix_lit}))",
                        prefix_lit = biscuit_string(&prefix),
                    ));
                }
                // A repo grant matches that exact repo, or any nested path under
                // it (monorepo subtree). Kind is preserved so a future non-repo
                // resource kind cannot be satisfied by a repo fact.
                _ => {
                    clauses.push(format!(
                        "($k == {kind_lit} && ($p == {path_lit} || $p.starts_with({prefix_lit})))",
                        kind_lit = biscuit_string(kind),
                        path_lit = biscuit_string(path),
                        prefix_lit = biscuit_string(&prefix),
                    ));
                }
            }
        }
        let pred = clauses.join(" || ");
        block = block
            .check(format!("check if resource($k, $p), {pred}").as_str())
            .context("resource allowlist check")?;
    }
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

/// Allowlist for values interpolated into Biscuit DSL string literals.
///
/// Restricted to `[A-Za-z0-9._/@:+-]` so quotes, newlines, `$`, `|`, and
/// other DSL/metacharacters cannot inject facts or checks. Paths may use
/// `/`; operation names and agent ids are alphanumeric-plus-punctuation.
fn validate_biscuit_token_string(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    for ch in value.chars() {
        if !matches!(
            ch,
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '/' | '@' | ':' | '+' | '-'
        ) {
            anyhow::bail!(
                "{field} contains forbidden character {ch:?}; allowed: [A-Za-z0-9._/@:+-]"
            );
        }
    }
    Ok(())
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
    fn identity_service_agent_policy_exactly_matches_the_shared_descriptor() {
        use prost::Message;

        let descriptor = prost_types::FileDescriptorSet::decode(grpc::FILE_DESCRIPTOR_SET)
            .expect("the shared API descriptor must decode");
        let proto_operations = descriptor
            .file
            .iter()
            .filter(|file| file.package.as_deref() == Some("heddle.api.v1alpha1"))
            .flat_map(|file| &file.service)
            .find(|service| service.name.as_deref() == Some("IdentityService"))
            .expect("the shared descriptor must define IdentityService")
            .method
            .iter()
            .map(|method| method.name.as_deref().expect("RPC method name"))
            .collect::<BTreeSet<_>>();
        let policy_operations = AUTH_SERVICE_AGENT_POLICY
            .iter()
            .map(|(operation, _)| *operation)
            .collect::<BTreeSet<_>>();

        assert_eq!(
            policy_operations.len(),
            AUTH_SERVICE_AGENT_POLICY.len(),
            "the agent auth policy must classify each RPC exactly once"
        );
        assert_eq!(
            policy_operations, proto_operations,
            "every IdentityService RPC must be explicitly classified for derived agents"
        );
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
    /// the hosted server (`time` + bare gRPC `operation`).
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
    fn rejects_injection_in_allowed_operations() {
        let (parent, _kp, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let err = attenuate_for_agent(
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
        .expect_err("injection payload must be rejected");
        let message = format!("{err:#}");
        assert!(
            message.contains("forbidden character") || message.contains("allowed_operations"),
            "unexpected error: {message}"
        );
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
    fn server_accepts_allowed_operation_and_rejects_the_mandatory_operation_floor() {
        let (parent, root, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-safe".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(
                    SAFE_AGENT_OPERATIONS
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                ),
                allowed_resources: None,
                declared_scopes: vec![("repo".to_string(), "acme/heddle".to_string())],
            },
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("derive safe child");

        let parent_authority = biscuit_auth::UnverifiedBiscuit::from_base64(parent.as_bytes())
            .expect("parse parent")
            .print_block_source(0)
            .expect("parent authority");
        let child_authority = biscuit_auth::UnverifiedBiscuit::from_base64(child.as_bytes())
            .expect("parse child")
            .print_block_source(0)
            .expect("child authority");
        assert_eq!(
            child_authority, parent_authority,
            "offline attenuation must leave the root-human authority block unchanged"
        );

        server_authorizes(&child, &root, "Push", Utc::now())
            .expect("server accepts an allowlisted push");
        for denied in mandatory_agent_denied_operations() {
            assert!(
                server_authorizes(&child, &root, denied, Utc::now()).is_err(),
                "server must reject hard-denied operation {denied}"
            );
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

    #[test]
    fn validate_biscuit_token_string_allowlist() {
        assert!(validate_biscuit_token_string("op", "GetState").is_ok());
        assert!(validate_biscuit_token_string("path", "org/acme/heddle").is_ok());
        assert!(validate_biscuit_token_string("op", r#"x" || true"#).is_err());
        assert!(validate_biscuit_token_string("op", "a$b").is_err());
        assert!(validate_biscuit_token_string("op", "").is_err());
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
        server_authorizes_resource(&child, &root, "GetState", ("repo", "alice/repoA"), Utc::now())
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
    fn namespace_scope_caveat_is_a_repo_prefix_not_a_namespace_kind() {
        let (parent, root, parent_pop) = fresh_parent_token();
        let child_pop = Ed25519Signer::generate().expect("child PoP key");
        let child = attenuate_for_agent(
            &parent,
            AgentAttenuation {
                agent_id: "agent-ns".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                allowed_operations: Some(vec!["GetState".to_string()]),
                allowed_resources: Some(vec![("namespace".to_string(), "alice".to_string())]),
                declared_scopes: vec![("namespace".to_string(), "alice".to_string())],
            },
            &parent_pop,
            child_pop.public_key(),
        )
        .expect("derive namespace-scoped child");

        // The caveat is encoded against `resource("repo", …)` as a path prefix —
        // NOT `resource("namespace", …)`, which would match no injected fact and
        // brick every repo RPC.
        let block = biscuit_auth::UnverifiedBiscuit::from_base64(child.as_bytes())
            .expect("parse child")
            .print_block_source(1)
            .expect("child block");
        assert!(
            block.contains("$k == \"repo\" && $p.starts_with(\"alice/\")"),
            "namespace scope must emit a repo-path prefix caveat: {block}"
        );
        assert!(
            !block.contains("$k == \"namespace\""),
            "namespace scope must not emit a resource(\"namespace\", …) caveat: {block}"
        );

        // Every repo under the namespace is reachable; a repo outside is not.
        server_authorizes_resource(&child, &root, "GetState", ("repo", "alice/repoA"), Utc::now())
            .expect("repoA under the namespace is admitted");
        server_authorizes_resource(&child, &root, "GetState", ("repo", "alice/repoB"), Utc::now())
            .expect("repoB under the namespace is admitted");
        assert!(
            server_authorizes_resource(
                &child,
                &root,
                "GetState",
                ("repo", "bob/repoC"),
                Utc::now()
            )
            .is_err(),
            "a repo outside the namespace must be rejected"
        );
    }
}
