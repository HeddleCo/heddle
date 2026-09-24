//! Canonical pure service-scope syntax and signed child attenuation recipe.
use std::collections::HashSet;

use chrono::{DateTime, Utc};
use heddle_biscuit_verifier::{CI_VERDICT_WRITE_ACTION, delegation::AgentAttenuation};

/// A canonical capability token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Read Spool content.
    SpoolRead,
    /// Write Spool content.
    SpoolWrite,
    /// Administer a Spool.
    SpoolAdmin,
    /// Read Threads.
    ThreadRead,
    /// Write Threads.
    ThreadWrite,
    /// Delegate to an agent.
    AgentSpawn,
    /// Read agent state.
    AgentRead,
    /// Update agent state.
    AgentUpdate,
    /// Read grants.
    GrantRead,
    /// Write grants.
    GrantWrite,
    /// Read presence.
    PresenceRead,
    /// Submit CI verdicts.
    CiVerdictWrite,
}

impl Capability {
    /// Return the canonical scope token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Capability::SpoolRead => "read",
            Capability::SpoolWrite => "write",
            Capability::SpoolAdmin => "admin",
            Capability::ThreadRead => "thread:read",
            Capability::ThreadWrite => "thread:write",
            Capability::AgentSpawn => "agent:spawn",
            Capability::AgentRead => "agent:read",
            Capability::AgentUpdate => "agent:update",
            Capability::GrantRead => "grant:read",
            Capability::GrantWrite => "grant:write",
            Capability::PresenceRead => "presence:read",
            Capability::CiVerdictWrite => CI_VERDICT_WRITE_ACTION,
        }
    }

    /// Return the capability category.
    pub const fn category(self) -> &'static str {
        match self {
            Capability::SpoolRead | Capability::SpoolWrite | Capability::SpoolAdmin => "spool",
            Capability::ThreadRead | Capability::ThreadWrite => "thread",
            Capability::AgentSpawn | Capability::AgentRead | Capability::AgentUpdate => "agent",
            Capability::GrantRead | Capability::GrantWrite => "grant",
            Capability::PresenceRead => "presence",
            Capability::CiVerdictWrite => "ci-verdict",
        }
    }

    /// Request-time operation gated directly by this capability, when one
    /// exists. Most capabilities are enforced by their hosted handler after
    /// Biscuit verification; CI verdict submission is fail-closed in the
    /// authorizer itself so ordinary spool write cannot satisfy it.
    pub const fn operation(self) -> Option<&'static str> {
        match self {
            Capability::CiVerdictWrite => Some(heddle_biscuit_verifier::CI_VERDICT_WRITE_OPERATION),
            _ => None,
        }
    }

    /// Return a concise human description.
    pub const fn description(self) -> &'static str {
        match self {
            Capability::SpoolRead => "Read spool metadata and content in scope.",
            Capability::SpoolWrite => "Write spool metadata and content in scope.",
            Capability::SpoolAdmin => "Administer spools and descendants in scope.",
            Capability::ThreadRead => "List and inspect threads.",
            Capability::ThreadWrite => "Create, rename, and close threads.",
            Capability::AgentSpawn => "Register new agent sessions on behalf of the subject.",
            Capability::AgentRead => "Observe active agent sessions and registry state.",
            Capability::AgentUpdate => "Update agent status, heartbeat, and metadata.",
            Capability::GrantRead => "Read role grants on spools.",
            Capability::GrantWrite => "Modify role grants (typically admin-only).",
            Capability::PresenceRead => "Subscribe to presence channels for scoped spools.",
            Capability::CiVerdictWrite => "Submit signed CI verdicts for scoped spools.",
        }
    }

    /// Parse one canonical capability token.
    pub fn from_token(token: &str) -> Option<Capability> {
        for cap in Capability::all() {
            if cap.as_str() == token {
                return Some(*cap);
            }
        }
        None
    }

    /// Return the canonical capability inventory.
    pub const fn all() -> &'static [Capability] {
        &[
            Capability::SpoolRead,
            Capability::SpoolWrite,
            Capability::SpoolAdmin,
            Capability::ThreadRead,
            Capability::ThreadWrite,
            Capability::AgentSpawn,
            Capability::AgentRead,
            Capability::AgentUpdate,
            Capability::GrantRead,
            Capability::GrantWrite,
            Capability::PresenceRead,
            Capability::CiVerdictWrite,
        ]
    }
}

/// Tokens that grant the operator (Heddle staff) capability. The `staff`
/// short-circuit makes a token equivalent to global access for internal
/// authorization checks (operator surface only — staff still need an
/// explicit support-access grant to act on customer resources). `*` is
/// the bare wildcard for the same effect.
const STAFF_TOKENS: &[&str] = &["staff", "staff:*", "*"];

/// Whether `token` is a staff (operator) short-circuit marker.
fn is_staff_token(token: &str) -> bool {
    STAFF_TOKENS.contains(&token)
}

/// Whether the given **stored credential scope string** carries a staff
/// short-circuit marker. This is the post-Biscuit replacement for the
/// old `scope_allows_admin` free function — but it is intentionally
/// limited to stored-credential scope strings (e.g.
/// `IssuedCredentialRecord::scope`, `ServiceAccount::scope`). For token
/// claims, callers must use `BiscuitFacts::is_staff()` instead, which
/// reads the explicit `staff(true)` fact rather than parsing a rendered
/// string.
pub fn is_staff_scope(scope: &str) -> bool {
    scope
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .any(is_staff_token)
}

/// Whether `token` encodes a spool resource binding.
/// Accepts:
///   - `spool:{path}` (a concrete spool, no `*` in path)
///   - `spool:*` (live-grant wildcard, staff-only at issuance)
fn is_resource_token(token: &str) -> bool {
    match token.strip_prefix("spool:") {
        Some("*") => true,
        Some(path) => !path.is_empty() && !path.contains('*'),
        None => false,
    }
}

/// A parsed scope string.
#[derive(Debug, Default, Clone)]
pub struct ParsedScope {
    /// Set of canonical capabilities granted by the token.
    pub capabilities: HashSet<Capability>,
    /// Exact paths extracted from `spool:{path}` bindings. Empty when the only
    /// binding is `spool:*` (live-grant wildcard) or
    /// the scope contains no binding.
    pub spools: Vec<String>,
    /// `true` iff the scope contains `spool:*` (access resolved from live grants).
    pub spool_wildcard: bool,
    /// `true` iff the scope grants Heddle staff capability.
    pub staff: bool,
    /// Tokens that did not match any canonical capability, admin marker, or
    /// spool binding. Used at issuance time to reject typos like
    /// `change:read` rather than silently accepting them.
    pub unknown_tokens: Vec<String>,
}

impl ParsedScope {
    /// Whether this scope includes the capability.
    pub fn has(&self, cap: Capability) -> bool {
        self.staff || self.capabilities.contains(&cap)
    }

    /// Whether the scope covers `spool_path`: an explicit binding covers that
    /// spool and its descendants. Staff and `spool:*` cover every path.
    pub fn covers_spool(&self, spool_path: &str) -> bool {
        if self.staff || self.spool_wildcard {
            return true;
        }
        self.spools
            .iter()
            .any(|scoped| spool_path == scoped || spool_path.starts_with(&format!("{scoped}/")))
    }
}

/// Parse a scope string.
pub fn parse_scope(scope: &str) -> ParsedScope {
    let mut parsed = ParsedScope::default();
    for token in scope.split(|ch: char| ch.is_whitespace() || ch == ',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if is_staff_token(token) {
            parsed.staff = true;
            continue;
        }
        if is_resource_token(token) {
            match token.strip_prefix("spool:") {
                Some("*") => parsed.spool_wildcard = true,
                Some(path) if !parsed.spools.iter().any(|existing| existing == path) => {
                    parsed.spools.push(path.to_string());
                }
                None => {}
                Some(_) => {}
            }
            continue;
        }
        if let Some(cap) = Capability::from_token(token) {
            parsed.capabilities.insert(cap);
            continue;
        }
        parsed.unknown_tokens.push(token.to_string());
    }
    parsed
}

/// Error returned by [`validate_scope`] when the scope string contains a
/// token the server doesn't recognise, or no grantable content at all.
#[derive(Debug)]
pub enum ScopeValidationError {
    /// Scope string is empty or contained only whitespace.
    Empty,
    /// Scope contained tokens that are neither capabilities, staff markers,
    /// nor spool bindings. Lists the offending tokens so the client can
    /// render a precise error.
    UnknownTokens(Vec<String>),
    /// Scope contains no capability or staff marker. A resource binding by
    /// itself is a footgun we reject rather than minting a useless token.
    NoGrantableCapability,
}

impl std::fmt::Display for ScopeValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScopeValidationError::Empty => write!(f, "scope is empty"),
            ScopeValidationError::UnknownTokens(tokens) => {
                write!(f, "unknown scope tokens: {}", tokens.join(", "))
            }
            ScopeValidationError::NoGrantableCapability => {
                write!(
                    f,
                    "scope must include at least one capability or staff marker"
                )
            }
        }
    }
}

impl std::error::Error for ScopeValidationError {}

/// Validate one grantable scope.
pub fn validate_scope(scope: &str) -> Result<ParsedScope, ScopeValidationError> {
    let parsed = parse_scope(scope);
    if scope.trim().is_empty() {
        return Err(ScopeValidationError::Empty);
    }
    if !parsed.unknown_tokens.is_empty() {
        return Err(ScopeValidationError::UnknownTokens(
            parsed.unknown_tokens.clone(),
        ));
    }
    if !parsed.staff && parsed.capabilities.is_empty() {
        return Err(ScopeValidationError::NoGrantableCapability);
    }
    Ok(parsed)
}

/// Build the exact child ceiling used by the hosted issuer and local client.
/// An empty scope inherits the parent's operation/resource ceiling, while an
/// explicit scope always contributes a deterministic method set.
pub fn service_attenuation(
    scope: &str,
    delegation_id: &str,
    expiry: DateTime<Utc>,
) -> Result<AgentAttenuation, ScopeValidationError> {
    let mut restriction = AgentAttenuation::time_bounded(delegation_id, expiry);
    if scope.is_empty() {
        return Ok(restriction);
    }
    let parsed = validate_scope(scope)?;
    let allows = |capability| parsed.capabilities.contains(&capability);
    let admin = allows(Capability::SpoolAdmin) || parsed.staff;
    restriction.allowed_operations = Some(
        heddle_api::v2::ALL_METHODS
            .iter()
            .filter(|method| {
                use heddle_api::heddle::api::common::{AuthorizationRole as Role, RpcEffect};
                let read = method.effect == RpcEffect::ReadOnly;
                admin
                    || (allows(Capability::SpoolRead) && read)
                    || (allows(Capability::SpoolWrite)
                        && matches!(
                            method.authorization.role,
                            Role::ResourceReader | Role::ResourceWriter
                        ))
                    || (method.path.contains("ThreadService/")
                        && ((read && allows(Capability::ThreadRead))
                            || allows(Capability::ThreadWrite)))
                    || (method.path.contains("Grant")
                        && ((read && allows(Capability::GrantRead))
                            || allows(Capability::GrantWrite)))
                    || (allows(Capability::AgentRead)
                        && (method.path.ends_with("/ObserveIdentity")
                            || method.path.ends_with("/GetIdentity")))
                    || (allows(Capability::AgentUpdate)
                        && (method.path.ends_with("/PutDelegation")
                            || method.path.ends_with("/RevokeDelegation")))
                    || (allows(Capability::PresenceRead)
                        && method.path.ends_with("/ObserveActivity"))
                    || (allows(Capability::CiVerdictWrite)
                        && method.path.ends_with("/RecordEvidence"))
                    || (method.path.ends_with("/PutDelegation") && allows(Capability::AgentSpawn))
                    || (method.path.ends_with("/IssueDelegationCredential")
                        && allows(Capability::AgentSpawn))
            })
            .filter_map(|method| method.path.rsplit('/').next().map(str::to_string))
            .collect(),
    );
    if !parsed.spools.is_empty() {
        restriction.allowed_resources = Some(
            parsed
                .spools
                .into_iter()
                .map(|path| ("spool".into(), path))
                .collect(),
        );
    }
    Ok(restriction)
}

/// Verify that an issued child actually carries the requested signed ceiling.
/// The exact expected checks are parsed to Biscuit ASTs before comparison;
/// extra checks can only narrow. Facts or rules other than the verified PoP
/// transfer are refused because they can make an otherwise present check true.
pub fn verify_service_child_ceiling(
    issued: &biscuit_auth::Biscuit,
    scope: &str,
    delegation_id: &str,
    expiry: DateTime<Utc>,
) -> Result<(), ServiceCeilingError> {
    use biscuit_auth::builder::BlockBuilder;
    let restriction = service_attenuation(scope, delegation_id, expiry)?;
    let expected = restriction
        .block()
        .map_err(|_| ServiceCeilingError::Invalid)?;
    let index = issued
        .block_count()
        .checked_sub(1)
        .ok_or(ServiceCeilingError::Invalid)?;
    let source = issued
        .print_block_source(index)
        .map_err(|_| ServiceCeilingError::Invalid)?;
    let actual = BlockBuilder::new()
        .code(&source)
        .map_err(|_| ServiceCeilingError::Invalid)?;
    if actual.facts.len() != 1
        || actual.facts[0].predicate.name != "pop_delegation"
        || !actual.rules.is_empty()
        || !actual.scopes.is_empty()
        || !expected
            .checks
            .iter()
            .all(|check| actual.checks.contains(check))
    {
        return Err(ServiceCeilingError::Invalid);
    }
    Ok(())
}

/// Issued Biscuit does not contain the canonical requested service ceiling.
#[derive(Debug, thiserror::Error)]
pub enum ServiceCeilingError {
    /// The requested scope is not canonical and grantable.
    #[error(transparent)]
    Scope(#[from] ScopeValidationError),
    /// Signed child lacks the exact required time, method or resource caveat.
    #[error("issued service credential does not contain the requested scope ceiling")]
    Invalid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_read_scope_can_get_identity_snapshot() {
        let restriction = service_attenuation(
            "agent:read",
            "delegation-1",
            Utc::now() + chrono::Duration::hours(1),
        )
        .expect("valid agent read scope");
        let operations = restriction.allowed_operations.expect("operation ceiling");
        assert!(operations.contains(&"GetIdentity".to_string()));
        assert!(operations.contains(&"ObserveIdentity".to_string()));
        assert!(!operations.contains(&"ListSpools".to_string()));
    }
}
