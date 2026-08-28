// SPDX-License-Identifier: Apache-2.0
//! Wire payloads for `auth`, `whoami`, and `identity`.
//!
//! These payloads are emitted by `hosted-client`, which sits above this
//! crate; the types live here so the schema registry registers the real
//! serialization structs.

use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "AgentAccountCreatedSchema")]
pub struct AgentAccountCreatedOutput {
    pub output_kind: &'static str,
    pub account_id: String,
    pub pet_name: String,
    /// Subject carried by the client-minted, persisted agent capability.
    pub subject: String,
    pub authenticated: bool,
    pub credential_saved: bool,
    pub next: HumanPromotionDirective,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HumanPromotionDirective {
    pub kind: &'static str,
    pub summary: &'static str,
    pub account_id: String,
    /// Agent-native command that starts the short-lived human claim ceremony.
    pub command: &'static str,
    /// Reserved for a concrete server-provided promotion affordance.
    pub promotion_uri: Option<String>,
}

/// How a server's descriptor trust was established.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DescriptorTrustSource {
    Explicit,
    Automatic,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AuthLogoutSchema")]
pub struct AuthLogoutOutput {
    pub output_kind: &'static str,
    pub server: String,
    pub removed: bool,
    /// Whether a device signing identity recorded for this server (heddle#482)
    /// was removed. `false` when none was linked; on a removal failure logout
    /// errors instead of emitting this output, so a `true` here always means
    /// the logged-out private key is no longer on disk.
    pub device_identity_removed: bool,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AuthStatusSchema")]
pub struct AuthStatusOutput {
    pub output_kind: &'static str,
    pub server: String,
    pub authenticated: bool,
    /// Credential origin: `env:<path>` when `HEDDLE_CREDENTIAL` is set,
    /// `keystore` for a stored credential, `none` when unauthenticated.
    pub source: String,
    pub proof_key_available: bool,
    pub subject: Option<String>,
    pub credential_id: Option<String>,
    pub expires_at: Option<String>,
    pub recommended_action: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AuthTrustSchema")]
pub struct AuthTrustOutput {
    pub output_kind: &'static str,
    pub canonical_server: String,
    /// `explicit` or `automatic`.
    pub source: DescriptorTrustSource,
    pub key_id: String,
    pub public_key: String,
    pub fingerprint: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AuthCreateServiceTokenSchema")]
pub struct ServiceTokenOutput {
    pub output_kind: &'static str,
    pub name: String,
    pub namespace: String,
    pub scope: String,
    /// Absolute path of the `.hcred` credential file written with mode 0600.
    /// The token and proof key never appear on stdout or in JSON.
    pub credential_path: String,
    pub expires_in_days: u32,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AuthSignupInviteCreatedSchema")]
pub struct SignupInviteCreatedOutput {
    pub output_kind: &'static str,
    pub invite_id: String,
    /// The server returns this code only on surfaces where it may be shown.
    pub invite_code: String,
    pub allowance_remaining: u32,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AuthSignupInviteSchema")]
pub struct SignupInviteOutput {
    /// Present because `ListSignupInvites` includes it. Clients must not derive
    /// or fabricate invite codes from IDs or other metadata.
    pub invite_code: String,
    pub status: String,
    pub created_at: Option<String>,
    pub consumed: bool,
    pub consumed_at: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AuthSignupInviteListSchema")]
pub struct SignupInviteListOutput {
    pub output_kind: &'static str,
    pub invites: Vec<SignupInviteOutput>,
    pub allowance_remaining: u32,
}

// ---- whoami ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "WhoamiCaptureActorSchema")]
pub struct CaptureActor {
    pub name: String,
    pub email: String,
    /// `environment`, `repository`, `git_config`, `user_config`, or null when unknown.
    pub source: Option<&'static str>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "WhoamiSchema")]
pub struct WhoamiOutput {
    pub output_kind: &'static str,
    /// Who the next capture is attributed to. Distinct from hosted auth.
    pub capture_actor: CaptureActor,
    pub server: String,
    /// A usable credential resolves for this server.
    pub authenticated: bool,
    /// Credential origin: `env:<path>`, `keystore`, or `none`.
    pub source: String,
    /// Locally verified subject, present even when the server is unreachable.
    pub subject: Option<String>,
    /// The server answered `WhoAmI` — the hosted identity below is authoritative.
    pub reachable: bool,
    /// `root` (full-authority device/human token), `agent` (an offline-derived,
    /// attenuated delegation), or `service-account`. `None` when unauthenticated.
    pub token_kind: Option<String>,
    /// Resource scopes the delegation chain restricts this token to. Empty ⇒
    /// full resource authority.
    pub scopes: Vec<String>,
    /// The intersected hosted-operation ceiling from the delegation chain.
    /// `None` ⇒ no operation allowlist (full authority).
    pub operation_ceiling: Option<Vec<String>>,
    /// Effective token expiry (RFC3339). `None` ⇒ no expiry recorded.
    pub expires_at: Option<String>,
    /// Seconds until `expires_at`; negative when already expired.
    pub ttl_seconds_remaining: Option<i64>,
    /// The device proof key needed to sign hosted requests is present and valid.
    pub proof_key_available: bool,
    /// Server-authoritative identity, present only when `reachable`.
    pub identity: Option<WhoamiIdentity>,
    pub recommended_action: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "WhoamiIdentitySchema")]
pub struct WhoamiIdentity {
    pub subject: String,
    pub actor_subject: String,
    pub is_staff: bool,
    pub is_service_account: bool,
    pub is_biscuit: bool,
    pub session_id: String,
    pub amr: Vec<String>,
    /// The scope string the server records for this credential.
    pub server_scope: String,
    pub credential_id: String,
    pub device_id: Option<String>,
    pub agent_provider: Option<String>,
    pub agent_model: Option<String>,
    /// Resource roles the caller holds directly (UI gating only; the server
    /// enforces effective, inherited roles on each RPC).
    pub roles: Vec<WhoamiRole>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "WhoamiRoleSchema")]
pub struct WhoamiRole {
    pub resource_path: String,
    pub resource_kind: String,
    pub role: String,
}
