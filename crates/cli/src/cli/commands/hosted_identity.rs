// SPDX-License-Identifier: Apache-2.0
//! CLI Adapter for hosted identity operations.

use std::io::Write;

use anyhow::{Context, Result};
use heddle_cli_contract::cli::commands::wire::auth::{
    AgentAccountCreatedOutput, AuthLogoutOutput, AuthStatusOutput, AuthTrustOutput, CaptureActor,
    DescriptorTrustSource as WireDescriptorTrustSource, HumanPromotionDirective,
    ServiceTokenOutput, SignupInviteCreatedOutput, SignupInviteListOutput, SignupInviteOutput,
    WhoamiIdentity, WhoamiOutput, WhoamiRole,
};
use hosted_client::hosted_runtime::{
    AgentTemplate,
    auth::{
        AgentAccountCreated, AgentCredentialDestination, AuthEvent, AuthLoginOutcome, AuthOutcome,
        AuthStatus, AuthTrust, DerivedAgent, DescriptorTrustSource, ServiceTokenCreated,
        SignupInvite, SignupInviteCreated, SignupInviteList,
    },
    auth_requests::{AuthCommand, AuthOptions, AuthTrustCommand, LoginPermission},
    claim_offer::{ClaimOfferReady, ClaimOptions, ClaimOutcome},
    whoami::{WhoamiIdentity as HostedIdentity, WhoamiReport},
};

use crate::cli::{
    AgentTemplateArg, AuthCommands, AuthInviteCommands, AuthTrustCommands, ClaimArgs, Cli,
    should_output_json,
};

pub async fn cmd_hosted_auth(cli: &Cli, command: AuthCommands) -> Result<()> {
    let json = should_output_json(cli, None);
    let command = auth_command(command, crate::cli::is_interactive_tty());
    let outcome = hosted_client::hosted_runtime::auth::execute(
        AuthOptions::new(cli.op_id.clone()),
        command,
        write_auth_event,
    )
    .await?;
    write_auth_outcome(outcome, json)
}

const DERIVED_TOKEN_SECURITY_NOTE: &str = "Derived credential has its own proof key and is operation/TTL/resource-scope-limited and enforced server-side. The token and proof key travel together inside the .hcred file; the parent device key is not exported.";

fn write_auth_event(event: AuthEvent) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    write_auth_event_to(&mut stdout, &mut stderr, event, open_url)
}

fn write_auth_event_to(
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    event: AuthEvent,
    mut open_browser: impl FnMut(&str) -> std::io::Result<()>,
) -> Result<()> {
    match event {
        AuthEvent::DeviceAuthorizationReady {
            verification_uri,
            user_code,
        } => {
            writeln!(stdout)?;
            writeln!(stdout, "Open this URL to authorize:")?;
            writeln!(stdout, "  {verification_uri}")?;
            writeln!(stdout)?;
            writeln!(stdout, "Enter code: {user_code}")?;
            writeln!(stdout)?;
        }
        AuthEvent::BrowserOpenRequested { url } => {
            if open_browser(&url).is_err() {
                writeln!(
                    stderr,
                    "Could not open browser automatically. Please open the URL above."
                )?;
            }
        }
        AuthEvent::BrowserUrlRejected { reason } => {
            writeln!(stderr, "Refusing to open browser URL: {reason}")?;
            writeln!(stderr, "Please open the URL printed above in your browser.")?;
        }
        AuthEvent::WaitingForAuthorization => writeln!(stdout, "Waiting for authorization...")?,
    }
    Ok(())
}

fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        // Heddle's auth Module validates the URL before emitting this event.
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    Ok(())
}

fn write_auth_outcome(outcome: AuthOutcome, json: bool) -> Result<()> {
    write_auth_outcome_to(&mut std::io::stdout().lock(), outcome, json)
}

fn write_auth_outcome_to(writer: &mut impl Write, outcome: AuthOutcome, json: bool) -> Result<()> {
    match outcome {
        AuthOutcome::Login(outcome) => write_login_outcome(writer, outcome, json)?,
        AuthOutcome::Logout(outcome) => {
            if json {
                writeln!(
                    writer,
                    "{}",
                    serde_json::to_string(&AuthLogoutOutput {
                        output_kind: "auth_logout",
                        server: outcome.server,
                        removed: true,
                        device_identity_removed: outcome.device_identity_removed,
                    })?
                )?;
            } else {
                writeln!(writer, "Credentials removed for {}.", outcome.server)?;
                if outcome.device_identity_removed {
                    writeln!(writer, "Device signing identity removed.")?;
                }
            }
        }
        AuthOutcome::Status(outcome) => write_auth_status(writer, outcome, json)?,
        AuthOutcome::SignupInviteCreated(outcome) => write_invite_created(writer, outcome, json)?,
        AuthOutcome::SignupInviteList(outcome) => write_invite_list(writer, outcome, json)?,
        AuthOutcome::Trust(outcome) => write_auth_trust(writer, outcome, json)?,
        AuthOutcome::AgentDerived(outcome) => write_derived_agent(writer, outcome)?,
        AuthOutcome::ServiceTokenCreated(outcome) => write_service_token(writer, outcome, json)?,
    }
    Ok(())
}

fn write_login_outcome(
    writer: &mut impl Write,
    outcome: AuthLoginOutcome,
    json: bool,
) -> Result<()> {
    match outcome {
        AuthLoginOutcome::Authenticated {
            subject,
            credential_saved,
        } => {
            if credential_saved {
                writeln!(writer, "Authenticated as {subject}. Credentials saved.")?;
            } else {
                writeln!(writer, "Authenticated as {subject}.")?;
            }
        }
        AuthLoginOutcome::AgentAccountCreated(outcome) => {
            if json {
                writeln!(
                    writer,
                    "{}",
                    serde_json::to_string(&agent_account_output(outcome))?
                )?;
            } else {
                writeln!(
                    writer,
                    "Authenticated as {}. Credentials saved.",
                    outcome.subject
                )?;
                writeln!(
                    writer,
                    "Agent account {} is active; a human can claim it later.",
                    outcome.pet_name
                )?;
                writeln!(
                    writer,
                    "Next: {}",
                    crate::cli::style::bold(outcome.next.command)
                )?;
            }
        }
    }
    Ok(())
}

fn agent_account_output(outcome: AgentAccountCreated) -> AgentAccountCreatedOutput {
    AgentAccountCreatedOutput {
        output_kind: "agent_account_created",
        account_id: outcome.account_id,
        pet_name: outcome.pet_name,
        subject: outcome.subject,
        authenticated: outcome.authenticated,
        credential_saved: outcome.credential_saved,
        next: HumanPromotionDirective {
            kind: outcome.next.kind,
            summary: outcome.next.summary,
            account_id: outcome.next.account_id,
            command: outcome.next.command,
            promotion_uri: outcome.next.promotion_uri,
        },
    }
}

fn write_auth_status(writer: &mut impl Write, outcome: AuthStatus, json: bool) -> Result<()> {
    if json {
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&AuthStatusOutput {
                output_kind: "auth_status",
                server: outcome.server,
                authenticated: outcome.authenticated,
                source: outcome.source,
                proof_key_available: outcome.proof_key_available,
                subject: outcome.subject,
                credential_id: outcome.credential_id,
                expires_at: outcome.expires_at,
                recommended_action: outcome.recommended_action,
            })?
        )?;
    } else if outcome.authenticated {
        writeln!(writer, "Server:        {}", outcome.server)?;
        writeln!(writer, "Source:        {}", outcome.source)?;
        writeln!(
            writer,
            "Subject:       {}",
            outcome.subject.as_deref().unwrap_or_default()
        )?;
        if let Some(credential_id) = outcome.credential_id {
            writeln!(writer, "Credential:    {credential_id}")?;
        }
        if let Some(expires_at) = outcome.expires_at {
            writeln!(writer, "Expires:       {expires_at}")?;
        }
        if outcome.proof_key_available {
            writeln!(writer, "Hosted writes: ready (device proof key available)")?;
        } else {
            writeln!(
                writer,
                "Hosted writes: unavailable — credential missing device proof key; re-login / re-install"
            )?;
            if let Some(action) = outcome.recommended_action {
                writeln!(writer, "Run `{action}` to repair the credential.")?;
            }
        }
    } else {
        writeln!(writer, "Not authenticated with {}.", outcome.server)?;
        if let Some(action) = outcome.recommended_action {
            writeln!(writer, "Run `{action}` to authenticate.")?;
        }
    }
    Ok(())
}

fn write_invite_created(
    writer: &mut impl Write,
    outcome: SignupInviteCreated,
    json: bool,
) -> Result<()> {
    if json {
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&SignupInviteCreatedOutput {
                output_kind: "auth_invite",
                invite_id: outcome.invite_id,
                invite_code: outcome.invite_code,
                allowance_remaining: outcome.allowance_remaining,
            })?
        )?;
    } else {
        // The code deliberately appears on exactly one output line.
        writeln!(writer, "{}", outcome.invite_code)?;
        writeln!(
            writer,
            "Allowance remaining: {}",
            outcome.allowance_remaining
        )?;
    }
    Ok(())
}

fn write_invite_list(writer: &mut impl Write, outcome: SignupInviteList, json: bool) -> Result<()> {
    if json {
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&SignupInviteListOutput {
                output_kind: "auth_invite_list",
                invites: outcome.invites.into_iter().map(invite_output).collect(),
                allowance_remaining: outcome.allowance_remaining,
            })?
        )?;
    } else {
        if outcome.invites.is_empty() {
            writeln!(writer, "No signup invites.")?;
        } else {
            writeln!(writer, "CODE\tSTATUS\tCREATED_AT\tCONSUMED_AT")?;
            for invite in outcome.invites {
                writeln!(
                    writer,
                    "{}\t{}\t{}\t{}",
                    invite.invite_code,
                    invite.status,
                    invite.created_at.as_deref().unwrap_or("-"),
                    invite.consumed_at.as_deref().unwrap_or("-")
                )?;
            }
        }
        writeln!(
            writer,
            "Allowance remaining: {}",
            outcome.allowance_remaining
        )?;
    }
    Ok(())
}

fn invite_output(invite: SignupInvite) -> SignupInviteOutput {
    SignupInviteOutput {
        invite_code: invite.invite_code,
        status: invite.status,
        created_at: invite.created_at,
        consumed: invite.consumed,
        consumed_at: invite.consumed_at,
    }
}

fn write_auth_trust(writer: &mut impl Write, outcome: AuthTrust, json: bool) -> Result<()> {
    let source = match outcome.source {
        DescriptorTrustSource::Explicit => WireDescriptorTrustSource::Explicit,
        DescriptorTrustSource::Automatic => WireDescriptorTrustSource::Automatic,
    };
    if json {
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&AuthTrustOutput {
                output_kind: if outcome.replaced {
                    "auth_trust_replace"
                } else {
                    "auth_trust_show"
                },
                canonical_server: outcome.canonical_server,
                source,
                key_id: outcome.key_id,
                public_key: outcome.public_key,
                fingerprint: outcome.fingerprint,
            })?
        )?;
    } else {
        writeln!(
            writer,
            "Server:                {}",
            outcome.canonical_server
        )?;
        writeln!(
            writer,
            "Source:                {}",
            match source {
                WireDescriptorTrustSource::Explicit => "explicit",
                WireDescriptorTrustSource::Automatic => "automatic",
            }
        )?;
        writeln!(writer, "Descriptor key id:     {}", outcome.key_id)?;
        writeln!(writer, "Descriptor public key: {}", outcome.public_key)?;
        writeln!(writer, "Fingerprint:           {}", outcome.fingerprint)?;
    }
    Ok(())
}

fn write_derived_agent(writer: &mut impl Write, outcome: DerivedAgent) -> Result<()> {
    match &outcome.destination {
        AgentCredentialDestination::File(path) => {
            writeln!(
                writer,
                "Agent credential {} written to {}.",
                outcome.agent_id,
                path.display()
            )?;
            writeln!(writer, "Parent source: {}", outcome.parent_source)?;
            if let Some(template) = outcome.template {
                writeln!(writer, "Template: {} ceiling", template.as_str())?;
            }
            writeln!(
                writer,
                "Allowed operations: {}",
                outcome.allowed_operations.join(", ")
            )?;
            if let Some(scope) = outcome.rendered_scope {
                writeln!(writer, "Scope: {scope}")?;
            }
        }
        AgentCredentialDestination::Installed => {
            writeln!(
                writer,
                "Derived and installed agent token {} for {}.",
                outcome.agent_id, outcome.server
            )?;
            writeln!(writer, "Parent source: {}", outcome.parent_source)?;
            writeln!(writer, "Expires: {}", outcome.expires_at)?;
            if let Some(template) = outcome.template {
                writeln!(writer, "Template: {} ceiling", template.as_str())?;
            }
            writeln!(
                writer,
                "Allowed operations: {}",
                outcome.allowed_operations.join(", ")
            )?;
            if outcome.scopes.is_empty() {
                writeln!(
                    writer,
                    "Scopes: none (full resource authority inherited from parent)"
                )?;
            } else if let Some(scope) = outcome.rendered_scope {
                writeln!(writer, "Scope: {scope} (enforced server-side per request)")?;
            } else {
                writeln!(
                    writer,
                    "Scopes: {} (enforced server-side per request)",
                    outcome.scopes.join(", ")
                )?;
            }
        }
    }
    writeln!(writer, "{DERIVED_TOKEN_SECURITY_NOTE}")?;
    Ok(())
}

fn write_service_token(
    writer: &mut impl Write,
    outcome: ServiceTokenCreated,
    json: bool,
) -> Result<()> {
    if json {
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&ServiceTokenOutput {
                output_kind: "auth_create_service_token",
                name: outcome.name,
                namespace: outcome.namespace,
                scope: outcome.scope,
                credential_path: outcome.credential_path,
                expires_in_days: outcome.expires_in_days,
            })?
        )?;
    } else {
        writeln!(writer)?;
        writeln!(
            writer,
            "Service token created for \"{}\" (scope: {})",
            outcome.name, outcome.scope
        )?;
        writeln!(writer)?;
        writeln!(writer, "Credential written to: {}", outcome.credential_path)?;
        writeln!(writer, "Expires in {} days.", outcome.expires_in_days)?;
        writeln!(
            writer,
            "The single .hcred file carries the token and its proof key; keep it secret (mode 0600)."
        )?;
        writeln!(
            writer,
            "Point the runtime at it with HEDDLE_CREDENTIAL={}.",
            outcome.credential_path
        )?;
        writeln!(
            writer,
            "This token is scoped to the {} namespace.",
            outcome.namespace
        )?;
    }
    Ok(())
}

pub async fn cmd_hosted_claim(args: ClaimArgs) -> Result<()> {
    let outcome = hosted_client::hosted_runtime::claim_offer::claim(
        ClaimOptions {
            server: args.server,
            web_origin: args.web_origin,
            timeout: args.timeout,
        },
        write_claim_offer,
    )
    .await?;
    match outcome {
        ClaimOutcome::Claimed => {
            println!("Claim complete. This agent account now has a human owner.")
        }
        ClaimOutcome::Expired => println!("Claim offer expired without changing the account."),
        ClaimOutcome::Interrupted => {
            println!("Claim offer stopped; the link is no longer active.")
        }
    }
    Ok(())
}

fn write_claim_offer(offer: &ClaimOfferReady) -> Result<()> {
    write_claim_offer_to(&mut std::io::stdout().lock(), offer)
}

fn write_claim_offer_to(writer: &mut impl Write, offer: &ClaimOfferReady) -> Result<()> {
    writeln!(writer, "Claim offer ready for {}.", offer.pet_name)?;
    writeln!(
        writer,
        "\nOpen this short-lived claim link:\n\n{}\n",
        offer.claim_link
    )?;
    writeln!(
        writer,
        "Waiting up to {} for a human to finish claiming this account. Press Ctrl-C to stop.",
        display_duration(offer.timeout)
    )?;
    Ok(())
}

fn display_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds.is_multiple_of(24 * 60 * 60) {
        format!("{}d", seconds / (24 * 60 * 60))
    } else if seconds.is_multiple_of(60 * 60) {
        format!("{}h", seconds / (60 * 60))
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

pub async fn cmd_hosted_whoami(cli: &Cli, server: Option<String>) -> Result<()> {
    let start_path = command_start_path(cli)?;
    let report =
        hosted_client::hosted_runtime::whoami::whoami(&start_path, server.as_deref()).await?;
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&whoami_output(report))?);
    } else {
        write_whoami_human(&mut std::io::stdout().lock(), &report)?;
    }
    Ok(())
}

fn command_start_path(cli: &Cli) -> Result<std::path::PathBuf> {
    match &cli.repo {
        Some(path) => Ok(path.clone()),
        None => std::env::current_dir().context("get current working directory"),
    }
}

fn whoami_output(report: WhoamiReport) -> WhoamiOutput {
    WhoamiOutput {
        output_kind: "whoami",
        capture_actor: CaptureActor {
            name: report.capture_actor.name,
            email: report.capture_actor.email,
            source: report.capture_actor.source,
        },
        server: report.server,
        authenticated: report.authenticated,
        source: report.source,
        subject: report.subject,
        reachable: report.reachable,
        token_kind: report.token_kind,
        scopes: report.scopes,
        operation_ceiling: report.operation_ceiling,
        expires_at: report.expires_at,
        ttl_seconds_remaining: report.ttl_seconds_remaining,
        proof_key_available: report.proof_key_available,
        identity: report.identity.map(whoami_identity),
        recommended_action: report.recommended_action,
    }
}

fn whoami_identity(identity: HostedIdentity) -> WhoamiIdentity {
    WhoamiIdentity {
        subject: identity.subject,
        actor_subject: identity.actor_subject,
        is_staff: identity.is_staff,
        is_service_account: identity.is_service_account,
        is_biscuit: identity.is_biscuit,
        session_id: identity.session_id,
        amr: identity.amr,
        server_scope: identity.server_scope,
        credential_id: identity.credential_id,
        device_id: identity.device_id,
        agent_provider: identity.agent_provider,
        agent_model: identity.agent_model,
        roles: identity
            .roles
            .into_iter()
            .map(|role| WhoamiRole {
                resource_path: role.resource_path,
                resource_kind: role.resource_kind,
                role: role.role,
            })
            .collect(),
    }
}

fn write_whoami_human(
    writer: &mut impl std::io::Write,
    output: &WhoamiReport,
) -> std::io::Result<()> {
    writeln!(
        writer,
        "Capture actor: {} <{}>",
        output.capture_actor.name, output.capture_actor.email
    )?;
    if let Some(source) = output.capture_actor.source {
        writeln!(
            writer,
            "Source:        {}",
            verbs::principal_source_display(source)
        )?;
    }
    writeln!(writer)?;
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

fn auth_command(command: AuthCommands, interactive: bool) -> AuthCommand {
    match command {
        AuthCommands::Login {
            server,
            open_browser,
            invite,
            credential,
        } => AuthCommand::Login {
            server,
            permission: if open_browser || interactive {
                LoginPermission::Browser { open_browser }
            } else {
                LoginPermission::HeadlessOnly
            },
            invite,
            credential,
        },
        AuthCommands::Logout { server } => AuthCommand::Logout { server },
        AuthCommands::Status { server } => AuthCommand::Status { server },
        AuthCommands::Invite {
            email,
            server,
            command,
        } => AuthCommand::Invite {
            email,
            server,
            list: matches!(command, Some(AuthInviteCommands::List)),
        },
        AuthCommands::Trust { command } => AuthCommand::Trust {
            command: match command {
                AuthTrustCommands::Show(args) => AuthTrustCommand::Show {
                    server: args.server,
                },
                AuthTrustCommands::Replace(args) => AuthTrustCommand::Replace {
                    server: args.server,
                    expected_current_public_key: args.expect_current_public_key,
                    key_id: args.key_id,
                    public_key: args.public_key,
                },
            },
        },
        AuthCommands::DeriveAgent {
            server,
            agent_id,
            ttl_secs,
            scopes,
            allowed_operations,
            template,
            runner,
            out,
        } => AuthCommand::DeriveAgent {
            server,
            agent_id,
            ttl_secs,
            scopes,
            allowed_operations,
            template: if runner {
                Some(AgentTemplate::Runner)
            } else {
                template.map(agent_template)
            },
            out,
        },
        AuthCommands::CreateServiceToken {
            name,
            namespace,
            server,
            out,
        } => AuthCommand::CreateServiceToken {
            name,
            namespace,
            server,
            out,
        },
    }
}

fn agent_template(template: AgentTemplateArg) -> AgentTemplate {
    match template {
        AgentTemplateArg::Reviewer => AgentTemplate::Reviewer,
        AgentTemplateArg::Contributor => AgentTemplate::Contributor,
        AgentTemplateArg::CiLanding => AgentTemplate::CiLanding,
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use hosted_client::hosted_runtime::{
        auth::{AuthLogout, HumanPromotionDirective as HostedHumanPromotionDirective},
        whoami::{CaptureActor as HostedCaptureActor, WhoamiRole as HostedWhoamiRole},
    };

    use crate::cli::{AuthTrustReplaceArgs, AuthTrustShowArgs};

    use super::*;

    fn rendered(outcome: AuthOutcome, json: bool) -> String {
        let mut bytes = Vec::new();
        write_auth_outcome_to(&mut bytes, outcome, json).expect("render auth outcome");
        String::from_utf8(bytes).expect("auth output is UTF-8")
    }

    fn agent_account() -> AgentAccountCreated {
        AgentAccountCreated {
            account_id: "account-1".into(),
            pet_name: "bright-otter".into(),
            subject: "agent:account-1".into(),
            authenticated: true,
            credential_saved: true,
            next: HostedHumanPromotionDirective {
                kind: "claim",
                summary: "claim this account",
                account_id: "account-1".into(),
                command: "heddle claim",
                promotion_uri: Some("https://app.heddle.sh/claim/account-1".into()),
            },
        }
    }

    fn auth_status(authenticated: bool, proof_key_available: bool) -> AuthStatus {
        AuthStatus {
            server: "api.heddle.test".into(),
            authenticated,
            source: "credential-file".into(),
            proof_key_available,
            subject: Some("human:1".into()),
            credential_id: Some("credential-1".into()),
            expires_at: Some("2030-01-01T00:00:00Z".into()),
            recommended_action: Some("heddle auth login --server api.heddle.test".into()),
        }
    }

    fn invite() -> SignupInvite {
        SignupInvite {
            invite_code: "invite-code".into(),
            status: "consumed".into(),
            created_at: Some("2026-09-07T00:00:00Z".into()),
            consumed: true,
            consumed_at: Some("2026-09-07T01:00:00Z".into()),
        }
    }

    fn trust(source: DescriptorTrustSource, replaced: bool) -> AuthTrust {
        AuthTrust {
            replaced,
            canonical_server: "api.heddle.test".into(),
            source,
            key_id: "descriptor-1".into(),
            public_key: "ab".repeat(32),
            fingerprint: "sha256:test".into(),
        }
    }

    fn derived(destination: AgentCredentialDestination) -> DerivedAgent {
        DerivedAgent {
            agent_id: "reviewer-1".into(),
            server: "api.heddle.test".into(),
            parent_source: "device".into(),
            expires_at: "2030-01-01T00:00:00Z".into(),
            template: Some(AgentTemplate::Reviewer),
            allowed_operations: vec!["Pull".into(), "WhoAmI".into()],
            scopes: vec!["repo:heddle/heddle".into()],
            rendered_scope: Some("repo:heddle/heddle".into()),
            destination,
        }
    }

    fn service_token() -> ServiceTokenCreated {
        ServiceTokenCreated {
            name: "ci-main".into(),
            namespace: "heddle".into(),
            scope: "namespace:heddle".into(),
            credential_path: "/tmp/ci-main.hcred".into(),
            expires_in_days: 30,
        }
    }

    #[test]
    fn auth_events_keep_browser_effects_at_the_adapter_boundary() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        write_auth_event_to(
            &mut stdout,
            &mut stderr,
            AuthEvent::DeviceAuthorizationReady {
                verification_uri: "https://app.heddle.test/device".into(),
                user_code: "ABCD-EFGH".into(),
            },
            |_| Ok(()),
        )
        .expect("render device authorization");
        let text = String::from_utf8(stdout).expect("event output is UTF-8");
        assert!(text.contains("https://app.heddle.test/device"));
        assert!(text.contains("ABCD-EFGH"));
        assert!(stderr.is_empty());

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        write_auth_event_to(
            &mut stdout,
            &mut stderr,
            AuthEvent::BrowserOpenRequested {
                url: "https://app.heddle.test/device".into(),
            },
            |url| {
                assert_eq!(url, "https://app.heddle.test/device");
                Ok(())
            },
        )
        .expect("request browser open");
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());

        write_auth_event_to(
            &mut stdout,
            &mut stderr,
            AuthEvent::BrowserOpenRequested {
                url: "https://app.heddle.test/device".into(),
            },
            |_| Err(std::io::Error::other("browser unavailable")),
        )
        .expect("render browser fallback");
        assert!(String::from_utf8_lossy(&stderr).contains("open browser automatically"));

        stderr.clear();
        write_auth_event_to(
            &mut stdout,
            &mut stderr,
            AuthEvent::BrowserUrlRejected {
                reason: "unexpected scheme".into(),
            },
            |_| Ok(()),
        )
        .expect("render rejected browser URL");
        assert!(String::from_utf8_lossy(&stderr).contains("unexpected scheme"));

        write_auth_event_to(
            &mut stdout,
            &mut stderr,
            AuthEvent::WaitingForAuthorization,
            |_| Ok(()),
        )
        .expect("render authorization wait");
        assert!(String::from_utf8_lossy(&stdout).contains("Waiting for authorization"));
    }

    #[test]
    fn auth_outcome_rendering_covers_human_and_machine_contracts() {
        let authenticated = AuthOutcome::Login(AuthLoginOutcome::Authenticated {
            subject: "human:1".into(),
            credential_saved: true,
        });
        assert!(rendered(authenticated, false).contains("Credentials saved"));
        assert!(
            rendered(
                AuthOutcome::Login(AuthLoginOutcome::Authenticated {
                    subject: "human:1".into(),
                    credential_saved: false,
                }),
                false,
            )
            .contains("Authenticated as human:1.")
        );

        let account_json = rendered(
            AuthOutcome::Login(AuthLoginOutcome::AgentAccountCreated(agent_account())),
            true,
        );
        let account: serde_json::Value =
            serde_json::from_str(&account_json).expect("agent account JSON");
        assert_eq!(account["output_kind"], "agent_account_created");
        assert_eq!(account["next"]["account_id"], "account-1");
        let account_human = rendered(
            AuthOutcome::Login(AuthLoginOutcome::AgentAccountCreated(agent_account())),
            false,
        );
        assert!(account_human.contains("bright-otter"));
        assert!(account_human.contains("Next: heddle claim"));

        let logout = AuthOutcome::Logout(AuthLogout {
            server: "api.heddle.test".into(),
            device_identity_removed: true,
        });
        assert!(rendered(logout.clone(), false).contains("Device signing identity removed"));
        let logout_json: serde_json::Value =
            serde_json::from_str(&rendered(logout, true)).expect("logout JSON");
        assert_eq!(logout_json["removed"], true);

        let ready = rendered(AuthOutcome::Status(auth_status(true, true)), false);
        assert!(ready.contains("Hosted writes: ready"));
        assert!(ready.contains("credential-1"));
        let repair = rendered(AuthOutcome::Status(auth_status(true, false)), false);
        assert!(repair.contains("unavailable"));
        assert!(repair.contains("to repair the credential"));
        let signed_out = rendered(AuthOutcome::Status(auth_status(false, false)), false);
        assert!(signed_out.contains("Not authenticated"));
        assert!(signed_out.contains("to authenticate"));
        let status_json: serde_json::Value = serde_json::from_str(&rendered(
            AuthOutcome::Status(auth_status(true, true)),
            true,
        ))
        .expect("status JSON");
        assert_eq!(status_json["output_kind"], "auth_status");

        let created = SignupInviteCreated {
            invite_id: "invite-1".into(),
            invite_code: "invite-code".into(),
            allowance_remaining: 3,
        };
        assert!(
            rendered(AuthOutcome::SignupInviteCreated(created.clone()), false)
                .contains("Allowance remaining: 3")
        );
        let created_json: serde_json::Value =
            serde_json::from_str(&rendered(AuthOutcome::SignupInviteCreated(created), true))
                .expect("invite JSON");
        assert_eq!(created_json["invite_id"], "invite-1");

        let invites = SignupInviteList {
            invites: vec![invite()],
            allowance_remaining: 2,
        };
        let invite_list = rendered(AuthOutcome::SignupInviteList(invites.clone()), false);
        assert!(invite_list.contains("CODE\tSTATUS"));
        assert!(invite_list.contains("invite-code\tconsumed"));
        let invite_json: serde_json::Value =
            serde_json::from_str(&rendered(AuthOutcome::SignupInviteList(invites), true))
                .expect("invite list JSON");
        assert_eq!(invite_json["invites"][0]["consumed"], true);
        assert!(
            rendered(
                AuthOutcome::SignupInviteList(SignupInviteList {
                    invites: Vec::new(),
                    allowance_remaining: 4,
                }),
                false,
            )
            .contains("No signup invites")
        );

        let explicit = rendered(
            AuthOutcome::Trust(trust(DescriptorTrustSource::Explicit, true)),
            true,
        );
        let explicit_json: serde_json::Value = serde_json::from_str(&explicit).expect("trust JSON");
        assert_eq!(explicit_json["output_kind"], "auth_trust_replace");
        assert!(
            rendered(
                AuthOutcome::Trust(trust(DescriptorTrustSource::Automatic, false)),
                false,
            )
            .contains("automatic")
        );
        let shown_json: serde_json::Value = serde_json::from_str(&rendered(
            AuthOutcome::Trust(trust(DescriptorTrustSource::Automatic, false)),
            true,
        ))
        .expect("shown trust JSON");
        assert_eq!(shown_json["output_kind"], "auth_trust_show");

        let file_agent = rendered(
            AuthOutcome::AgentDerived(derived(AgentCredentialDestination::File(PathBuf::from(
                "/tmp/reviewer.hcred",
            )))),
            false,
        );
        assert!(file_agent.contains("/tmp/reviewer.hcred"));
        assert!(file_agent.contains(DERIVED_TOKEN_SECURITY_NOTE));
        let installed = rendered(
            AuthOutcome::AgentDerived(derived(AgentCredentialDestination::Installed)),
            false,
        );
        assert!(installed.contains("enforced server-side per request"));
        let mut unscoped = derived(AgentCredentialDestination::Installed);
        unscoped.scopes.clear();
        unscoped.rendered_scope = None;
        assert!(
            rendered(AuthOutcome::AgentDerived(unscoped), false)
                .contains("full resource authority inherited")
        );
        let mut raw_scopes = derived(AgentCredentialDestination::Installed);
        raw_scopes.rendered_scope = None;
        assert!(
            rendered(AuthOutcome::AgentDerived(raw_scopes), false)
                .contains("Scopes: repo:heddle/heddle")
        );

        let service_json: serde_json::Value = serde_json::from_str(&rendered(
            AuthOutcome::ServiceTokenCreated(service_token()),
            true,
        ))
        .expect("service token JSON");
        assert_eq!(service_json["namespace"], "heddle");
        let service_human = rendered(AuthOutcome::ServiceTokenCreated(service_token()), false);
        assert!(service_human.contains("HEDDLE_CREDENTIAL=/tmp/ci-main.hcred"));
        assert!(service_human.contains("mode 0600"));
    }

    fn identity() -> HostedIdentity {
        HostedIdentity {
            subject: "human:1".into(),
            actor_subject: "agent:reviewer-1".into(),
            is_staff: true,
            is_service_account: false,
            is_biscuit: true,
            session_id: "session-1".into(),
            amr: vec!["passkey".into()],
            server_scope: "api.heddle.test".into(),
            credential_id: "credential-1".into(),
            device_id: Some("device-1".into()),
            agent_provider: Some("codex".into()),
            agent_model: Some("gpt".into()),
            roles: vec![HostedWhoamiRole {
                resource_path: "heddle/heddle".into(),
                resource_kind: "repo".into(),
                role: "owner".into(),
            }],
        }
    }

    fn whoami_report() -> WhoamiReport {
        WhoamiReport {
            capture_actor: HostedCaptureActor {
                name: "Heddle Human".into(),
                email: "human@example.com".into(),
                source: Some("environment"),
            },
            server: "api.heddle.test".into(),
            authenticated: true,
            source: "credential-file".into(),
            subject: Some("human:1".into()),
            reachable: true,
            token_kind: Some("biscuit".into()),
            scopes: vec!["repo:heddle/heddle".into()],
            operation_ceiling: Some(vec!["Pull".into(), "Push".into()]),
            expires_at: Some("2030-01-01T00:00:00Z".into()),
            ttl_seconds_remaining: Some(60),
            proof_key_available: true,
            identity: Some(identity()),
            recommended_action: Some("heddle auth login".into()),
        }
    }

    #[test]
    fn whoami_rendering_distinguishes_actor_authority_and_reachability() {
        let report = whoami_report();
        let machine = whoami_output(report.clone());
        assert_eq!(machine.output_kind, "whoami");
        assert_eq!(machine.capture_actor.email, "human@example.com");
        let mapped = machine.identity.expect("mapped hosted identity");
        assert_eq!(mapped.actor_subject, "agent:reviewer-1");
        assert_eq!(mapped.roles[0].role, "owner");

        let mut bytes = Vec::new();
        write_whoami_human(&mut bytes, &report).expect("render reachable whoami");
        let human = String::from_utf8(bytes).expect("whoami output is UTF-8");
        for expected in [
            "Capture actor: Heddle Human <human@example.com>",
            "Source:        environment",
            "Acting as:     agent:reviewer-1",
            "Credential:    credential-1",
            "Session:       session-1",
            "Staff:         yes",
            "Server scope:  api.heddle.test",
            "repo:heddle/heddle=owner",
            "Scopes:        repo:heddle/heddle",
            "Op ceiling:    Pull, Push",
            "(in 60s)",
            "Signing:       ready",
            "Note:          run `heddle auth login`.",
        ] {
            assert!(human.contains(expected), "missing `{expected}` in {human}");
        }

        let mut signed_out = whoami_report();
        signed_out.authenticated = false;
        signed_out.recommended_action = Some("heddle auth login --server api.heddle.test".into());
        let mut bytes = Vec::new();
        write_whoami_human(&mut bytes, &signed_out).expect("render signed-out whoami");
        let human = String::from_utf8(bytes).expect("whoami output is UTF-8");
        assert!(human.contains("Not authenticated"));
        assert!(human.contains("to authenticate"));

        let mut unreachable = whoami_report();
        unreachable.identity = None;
        unreachable.scopes.clear();
        unreachable.operation_ceiling = None;
        unreachable.ttl_seconds_remaining = Some(-30);
        unreachable.proof_key_available = false;
        unreachable.recommended_action = None;
        let mut bytes = Vec::new();
        write_whoami_human(&mut bytes, &unreachable).expect("render unreachable whoami");
        let human = String::from_utf8(bytes).expect("whoami output is UTF-8");
        assert!(human.contains("unreachable"));
        assert!(human.contains("full resource authority"));
        assert!(human.contains("full (no operation allowlist)"));
        assert!(human.contains("EXPIRED 30s ago"));
        assert!(human.contains("Signing:       unavailable"));

        unreachable.ttl_seconds_remaining = None;
        let mut bytes = Vec::new();
        write_whoami_human(&mut bytes, &unreachable).expect("render unknown TTL");
        assert!(String::from_utf8_lossy(&bytes).contains("Expires:       2030-01-01T00:00:00Z"));
    }

    #[test]
    fn claim_offer_uses_compact_human_durations() {
        for (seconds, expected) in [
            (2 * 24 * 60 * 60, "2d"),
            (3 * 60 * 60, "3h"),
            (4 * 60, "4m"),
            (5, "5s"),
        ] {
            assert_eq!(display_duration(Duration::from_secs(seconds)), expected);
        }
        let mut bytes = Vec::new();
        write_claim_offer_to(
            &mut bytes,
            &ClaimOfferReady {
                pet_name: "bright-otter".into(),
                claim_link: "https://app.heddle.test/claim/1".into(),
                timeout: Duration::from_secs(15 * 60),
            },
        )
        .expect("render claim offer");
        let text = String::from_utf8(bytes).expect("claim output is UTF-8");
        assert!(text.contains("bright-otter"));
        assert!(text.contains("https://app.heddle.test/claim/1"));
        assert!(text.contains("Waiting up to 15m"));
    }

    #[test]
    fn auth_commands_preserve_cli_permissions_and_scope() {
        match auth_command(
            AuthCommands::Login {
                server: Some("api.heddle.test".into()),
                open_browser: false,
                invite: Some("invite-code".into()),
                credential: None,
            },
            false,
        ) {
            AuthCommand::Login {
                server,
                permission,
                invite,
                credential,
            } => {
                assert_eq!(server.as_deref(), Some("api.heddle.test"));
                assert_eq!(permission, LoginPermission::HeadlessOnly);
                assert_eq!(invite.as_deref(), Some("invite-code"));
                assert!(credential.is_none());
            }
            other => panic!("expected login, got {other:?}"),
        }
        for (interactive, open_browser, expected) in [
            (
                true,
                false,
                LoginPermission::Browser {
                    open_browser: false,
                },
            ),
            (false, true, LoginPermission::Browser { open_browser: true }),
        ] {
            match auth_command(
                AuthCommands::Login {
                    server: None,
                    open_browser,
                    invite: None,
                    credential: None,
                },
                interactive,
            ) {
                AuthCommand::Login { permission, .. } => assert_eq!(permission, expected),
                other => panic!("expected login, got {other:?}"),
            }
        }
        assert!(matches!(
            auth_command(AuthCommands::Logout { server: None }, false),
            AuthCommand::Logout { server: None }
        ));
        assert!(matches!(
            auth_command(AuthCommands::Status { server: None }, false),
            AuthCommand::Status { server: None }
        ));
        assert!(matches!(
            auth_command(
                AuthCommands::Invite {
                    email: Some("human@example.com".into()),
                    server: None,
                    command: Some(AuthInviteCommands::List),
                },
                false,
            ),
            AuthCommand::Invite { list: true, .. }
        ));
        assert!(matches!(
            auth_command(
                AuthCommands::Invite {
                    email: None,
                    server: None,
                    command: None,
                },
                false,
            ),
            AuthCommand::Invite { list: false, .. }
        ));
        match auth_command(
            AuthCommands::Trust {
                command: AuthTrustCommands::Show(AuthTrustShowArgs {
                    server: "api.heddle.test".into(),
                }),
            },
            false,
        ) {
            AuthCommand::Trust {
                command: AuthTrustCommand::Show { server },
            } => assert_eq!(server, "api.heddle.test"),
            other => panic!("expected trust show, got {other:?}"),
        }
        match auth_command(
            AuthCommands::Trust {
                command: AuthTrustCommands::Replace(AuthTrustReplaceArgs {
                    server: "api.heddle.test".into(),
                    expect_current_public_key: "old".into(),
                    key_id: "new-key".into(),
                    public_key: "new".into(),
                }),
            },
            false,
        ) {
            AuthCommand::Trust {
                command:
                    AuthTrustCommand::Replace {
                        server,
                        expected_current_public_key,
                        key_id,
                        public_key,
                    },
            } => {
                assert_eq!(server, "api.heddle.test");
                assert_eq!(expected_current_public_key, "old");
                assert_eq!(key_id, "new-key");
                assert_eq!(public_key, "new");
            }
            other => panic!("expected trust replace, got {other:?}"),
        }
        match auth_command(
            AuthCommands::DeriveAgent {
                server: "api.heddle.test".into(),
                agent_id: Some("runner-1".into()),
                ttl_secs: 300,
                scopes: vec!["spool:heddle/heddle".into()],
                allowed_operations: Vec::new(),
                template: None,
                runner: true,
                out: Some(PathBuf::from("runner.hcred")),
            },
            false,
        ) {
            AuthCommand::DeriveAgent {
                template,
                ttl_secs,
                scopes,
                out,
                ..
            } => {
                assert_eq!(template, Some(AgentTemplate::Runner));
                assert_eq!(ttl_secs, 300);
                assert_eq!(scopes, ["spool:heddle/heddle"]);
                assert_eq!(out.as_deref(), Some(std::path::Path::new("runner.hcred")));
            }
            other => panic!("expected derive-agent, got {other:?}"),
        }
        for (argument, expected) in [
            (AgentTemplateArg::Reviewer, AgentTemplate::Reviewer),
            (AgentTemplateArg::Contributor, AgentTemplate::Contributor),
            (AgentTemplateArg::CiLanding, AgentTemplate::CiLanding),
        ] {
            assert_eq!(agent_template(argument), expected);
        }
        match auth_command(
            AuthCommands::DeriveAgent {
                server: "api.heddle.test".into(),
                agent_id: None,
                ttl_secs: 600,
                scopes: Vec::new(),
                allowed_operations: vec!["Pull".into()],
                template: Some(AgentTemplateArg::Contributor),
                runner: false,
                out: None,
            },
            false,
        ) {
            AuthCommand::DeriveAgent {
                template,
                allowed_operations,
                ..
            } => {
                assert_eq!(template, Some(AgentTemplate::Contributor));
                assert_eq!(allowed_operations, ["Pull"]);
            }
            other => panic!("expected derive-agent, got {other:?}"),
        }
        match auth_command(
            AuthCommands::CreateServiceToken {
                name: "ci-main".into(),
                namespace: "heddle".into(),
                server: Some("api.heddle.test".into()),
                out: Some(PathBuf::from("ci-main.hcred")),
            },
            false,
        ) {
            AuthCommand::CreateServiceToken {
                name,
                namespace,
                server,
                out,
            } => {
                assert_eq!(name, "ci-main");
                assert_eq!(namespace, "heddle");
                assert_eq!(server.as_deref(), Some("api.heddle.test"));
                assert_eq!(out.as_deref(), Some(std::path::Path::new("ci-main.hcred")));
            }
            other => panic!("expected service token, got {other:?}"),
        }
    }
}
