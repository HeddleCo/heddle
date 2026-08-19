use cli_shared::{ResolvedPrincipal, principal_source_display};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(super) struct CaptureActor {
    pub name: String,
    pub email: String,
    /// `environment`, `repository`, `git_config`, `user_config`, or null when unknown.
    pub source: Option<&'static str>,
}

impl CaptureActor {
    pub(super) fn from_resolved(resolved: &ResolvedPrincipal) -> Self {
        Self {
            name: resolved.principal.name_lossy().into_owned(),
            email: resolved.principal.email_lossy().into_owned(),
            source: resolved.source,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct WhoamiOutput {
    pub output_kind: &'static str,
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
    /// Resource scopes the delegation chain restricts this token to, as
    /// `kind:path` (e.g. `repo:alice/api`, `namespace:alice`). Empty ⇒ full
    /// resource authority.
    pub scopes: Vec<String>,
    /// The intersected hosted-operation ceiling from the delegation chain. `None`
    /// ⇒ no operation allowlist (full authority, minus the mandatory deny floor).
    pub operation_ceiling: Option<Vec<String>>,
    /// Effective token expiry (RFC3339), the earliest of the authority and every
    /// attenuation hop. `None` ⇒ no expiry recorded.
    pub expires_at: Option<String>,
    /// Seconds until `expires_at`; negative when already expired.
    pub ttl_seconds_remaining: Option<i64>,
    /// The device proof key needed to sign hosted requests is present and valid.
    pub proof_key_available: bool,
    /// Server-authoritative identity, present only when `reachable`.
    pub identity: Option<WhoamiIdentity>,
    pub recommended_action: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct WhoamiIdentity {
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

#[derive(Debug, Serialize)]
pub(super) struct WhoamiRole {
    pub resource_path: String,
    pub resource_kind: String,
    pub role: String,
}

pub(super) fn print_human(output: &WhoamiOutput) {
    write_human(&mut std::io::stdout().lock(), output).expect("write whoami output");
}

pub(super) fn write_human(
    writer: &mut impl std::io::Write,
    output: &WhoamiOutput,
) -> std::io::Result<()> {
    write_capture_actor(writer, &output.capture_actor)?;
    writeln!(writer)?;
    write_hosted_auth(writer, output)
}

fn write_capture_actor(
    writer: &mut impl std::io::Write,
    actor: &CaptureActor,
) -> std::io::Result<()> {
    writeln!(writer, "Capture actor: {} <{}>", actor.name, actor.email)?;
    if let Some(source) = actor.source {
        writeln!(
            writer,
            "Source:        {}",
            principal_source_display(source)
        )?;
    }
    Ok(())
}

fn write_hosted_auth(
    writer: &mut impl std::io::Write,
    output: &WhoamiOutput,
) -> std::io::Result<()> {
    writeln!(writer, "Hosted auth:")?;
    writeln!(writer, "Server:        {}", output.server)?;
    if !output.authenticated {
        writeln!(writer, "Not authenticated with {}.", output.server)?;
        if let Some(action) = &output.recommended_action {
            writeln!(writer, "Run `{action}` to authenticate.")?;
        }
        return Ok(());
    }
    writeln!(writer, "Source:        {}", output.source)?;
    if let Some(subject) = &output.subject {
        writeln!(writer, "Subject:       {subject}")?;
    }
    if let Some(identity) = &output.identity {
        if identity.actor_subject != identity.subject && !identity.actor_subject.is_empty() {
            writeln!(writer, "Acting as:     {}", identity.actor_subject)?;
        }
        if !identity.credential_id.is_empty() {
            writeln!(writer, "Credential:    {}", identity.credential_id)?;
        }
        if !identity.session_id.is_empty() {
            writeln!(writer, "Session:       {}", identity.session_id)?;
        }
        if identity.is_staff {
            writeln!(writer, "Staff:         yes")?;
        }
        if !identity.server_scope.is_empty() {
            writeln!(writer, "Server scope:  {}", identity.server_scope)?;
        }
        if !identity.roles.is_empty() {
            let roles = identity
                .roles
                .iter()
                .map(|role| {
                    format!(
                        "{}:{}={}",
                        role.resource_kind, role.resource_path, role.role
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(writer, "Roles:         {roles}")?;
        }
    } else {
        writeln!(
            writer,
            "Server:        unreachable (showing locally-known token facts)"
        )?;
    }
    writeln!(
        writer,
        "Token kind:    {}",
        output.token_kind.as_deref().unwrap_or("unknown")
    )?;
    if output.scopes.is_empty() {
        writeln!(writer, "Scopes:        full resource authority")?;
    } else {
        writeln!(writer, "Scopes:        {}", output.scopes.join(", "))?;
    }
    match &output.operation_ceiling {
        Some(ops) => writeln!(writer, "Op ceiling:    {}", ops.join(", "))?,
        None => writeln!(writer, "Op ceiling:    full (no operation allowlist)")?,
    }
    if let Some(expires_at) = &output.expires_at {
        match output.ttl_seconds_remaining {
            Some(secs) if secs >= 0 => {
                writeln!(writer, "Expires:       {expires_at} (in {secs}s)")?;
            }
            Some(secs) => writeln!(
                writer,
                "Expires:       {expires_at} (EXPIRED {}s ago)",
                -secs
            )?,
            None => writeln!(writer, "Expires:       {expires_at}")?,
        }
    }
    if output.proof_key_available {
        writeln!(writer, "Signing:       ready (device proof key available)")?;
    } else {
        writeln!(writer, "Signing:       unavailable (no device proof key)")?;
    }
    if let Some(action) = &output.recommended_action {
        writeln!(writer, "Note:          run `{action}`.")?;
    }
    Ok(())
}
