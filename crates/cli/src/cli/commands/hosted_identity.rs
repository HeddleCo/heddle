// SPDX-License-Identifier: Apache-2.0
//! CLI Adapter for hosted identity operations.

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

use super::action_line::print_next;

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
    match event {
        AuthEvent::DeviceAuthorizationReady {
            verification_uri,
            user_code,
        } => {
            println!();
            println!("Open this URL to authorize:");
            println!("  {verification_uri}");
            println!();
            println!("Enter code: {user_code}");
            println!();
        }
        AuthEvent::BrowserOpenRequested { url } => {
            if open_url(&url).is_err() {
                eprintln!("Could not open browser automatically. Please open the URL above.");
            }
        }
        AuthEvent::BrowserUrlRejected { reason } => {
            eprintln!("Refusing to open browser URL: {reason}");
            eprintln!("Please open the URL printed above in your browser.");
        }
        AuthEvent::WaitingForAuthorization => println!("Waiting for authorization..."),
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
    match outcome {
        AuthOutcome::Login(outcome) => write_login_outcome(outcome, json)?,
        AuthOutcome::Logout(outcome) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&AuthLogoutOutput {
                        output_kind: "auth_logout",
                        server: outcome.server,
                        removed: true,
                        device_identity_removed: outcome.device_identity_removed,
                    })?
                );
            } else {
                println!("Credentials removed for {}.", outcome.server);
                if outcome.device_identity_removed {
                    println!("Device signing identity removed.");
                }
            }
        }
        AuthOutcome::Status(outcome) => write_auth_status(outcome, json)?,
        AuthOutcome::SignupInviteCreated(outcome) => write_invite_created(outcome, json)?,
        AuthOutcome::SignupInviteList(outcome) => write_invite_list(outcome, json)?,
        AuthOutcome::Trust(outcome) => write_auth_trust(outcome, json)?,
        AuthOutcome::AgentDerived(outcome) => write_derived_agent(outcome),
        AuthOutcome::ServiceTokenCreated(outcome) => write_service_token(outcome, json)?,
    }
    Ok(())
}

fn write_login_outcome(outcome: AuthLoginOutcome, json: bool) -> Result<()> {
    match outcome {
        AuthLoginOutcome::Authenticated {
            subject,
            credential_saved,
        } => {
            if credential_saved {
                println!("Authenticated as {subject}. Credentials saved.");
            } else {
                println!("Authenticated as {subject}.");
            }
        }
        AuthLoginOutcome::AgentAccountCreated(outcome) => {
            if json {
                println!("{}", serde_json::to_string(&agent_account_output(outcome))?);
            } else {
                println!("Authenticated as {}. Credentials saved.", outcome.subject);
                println!(
                    "Agent account {} is active; a human can claim it later.",
                    outcome.pet_name
                );
                print_next(outcome.next.command);
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

fn write_auth_status(outcome: AuthStatus, json: bool) -> Result<()> {
    if json {
        println!(
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
        );
    } else if outcome.authenticated {
        println!("Server:        {}", outcome.server);
        println!("Source:        {}", outcome.source);
        println!(
            "Subject:       {}",
            outcome.subject.as_deref().unwrap_or_default()
        );
        if let Some(credential_id) = outcome.credential_id {
            println!("Credential:    {credential_id}");
        }
        if let Some(expires_at) = outcome.expires_at {
            println!("Expires:       {expires_at}");
        }
        if outcome.proof_key_available {
            println!("Hosted writes: ready (device proof key available)");
        } else {
            println!(
                "Hosted writes: unavailable — credential missing device proof key; re-login / re-install"
            );
            if let Some(action) = outcome.recommended_action {
                println!("Run `{action}` to repair the credential.");
            }
        }
    } else {
        println!("Not authenticated with {}.", outcome.server);
        if let Some(action) = outcome.recommended_action {
            println!("Run `{action}` to authenticate.");
        }
    }
    Ok(())
}

fn write_invite_created(outcome: SignupInviteCreated, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&SignupInviteCreatedOutput {
                output_kind: "auth_invite",
                invite_id: outcome.invite_id,
                invite_code: outcome.invite_code,
                allowance_remaining: outcome.allowance_remaining,
            })?
        );
    } else {
        // The code deliberately appears on exactly one output line.
        println!("{}", outcome.invite_code);
        println!("Allowance remaining: {}", outcome.allowance_remaining);
    }
    Ok(())
}

fn write_invite_list(outcome: SignupInviteList, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&SignupInviteListOutput {
                output_kind: "auth_invite_list",
                invites: outcome.invites.into_iter().map(invite_output).collect(),
                allowance_remaining: outcome.allowance_remaining,
            })?
        );
    } else {
        if outcome.invites.is_empty() {
            println!("No signup invites.");
        } else {
            println!("CODE\tSTATUS\tCREATED_AT\tCONSUMED_AT");
            for invite in outcome.invites {
                println!(
                    "{}\t{}\t{}\t{}",
                    invite.invite_code,
                    invite.status,
                    invite.created_at.as_deref().unwrap_or("-"),
                    invite.consumed_at.as_deref().unwrap_or("-")
                );
            }
        }
        println!("Allowance remaining: {}", outcome.allowance_remaining);
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

fn write_auth_trust(outcome: AuthTrust, json: bool) -> Result<()> {
    let source = match outcome.source {
        DescriptorTrustSource::Explicit => WireDescriptorTrustSource::Explicit,
        DescriptorTrustSource::Automatic => WireDescriptorTrustSource::Automatic,
    };
    if json {
        println!(
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
        );
    } else {
        println!("Server:                {}", outcome.canonical_server);
        println!(
            "Source:                {}",
            match source {
                WireDescriptorTrustSource::Explicit => "explicit",
                WireDescriptorTrustSource::Automatic => "automatic",
            }
        );
        println!("Descriptor key id:     {}", outcome.key_id);
        println!("Descriptor public key: {}", outcome.public_key);
        println!("Fingerprint:           {}", outcome.fingerprint);
    }
    Ok(())
}

fn write_derived_agent(outcome: DerivedAgent) {
    match &outcome.destination {
        AgentCredentialDestination::File(path) => {
            println!(
                "Agent credential {} written to {}.",
                outcome.agent_id,
                path.display()
            );
            println!("Parent source: {}", outcome.parent_source);
            if let Some(template) = outcome.template {
                println!("Template: {} ceiling", template.as_str());
            }
            println!(
                "Allowed operations: {}",
                outcome.allowed_operations.join(", ")
            );
            if let Some(scope) = outcome.rendered_scope {
                println!("Scope: {scope}");
            }
        }
        AgentCredentialDestination::Installed => {
            println!(
                "Derived and installed agent token {} for {}.",
                outcome.agent_id, outcome.server
            );
            println!("Parent source: {}", outcome.parent_source);
            println!("Expires: {}", outcome.expires_at);
            if let Some(template) = outcome.template {
                println!("Template: {} ceiling", template.as_str());
            }
            println!(
                "Allowed operations: {}",
                outcome.allowed_operations.join(", ")
            );
            if outcome.scopes.is_empty() {
                println!("Scopes: none (full resource authority inherited from parent)");
            } else if let Some(scope) = outcome.rendered_scope {
                println!("Scope: {scope} (enforced server-side per request)");
            } else {
                println!(
                    "Scopes: {} (enforced server-side per request)",
                    outcome.scopes.join(", ")
                );
            }
        }
    }
    println!("{DERIVED_TOKEN_SECURITY_NOTE}");
}

fn write_service_token(outcome: ServiceTokenCreated, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&ServiceTokenOutput {
                output_kind: "auth_create_service_token",
                name: outcome.name,
                namespace: outcome.namespace,
                scope: outcome.scope,
                credential_path: outcome.credential_path,
                expires_in_days: outcome.expires_in_days,
            })?
        );
    } else {
        println!();
        println!(
            "Service token created for \"{}\" (scope: {})",
            outcome.name, outcome.scope
        );
        println!();
        println!("Credential written to: {}", outcome.credential_path);
        println!("Expires in {} days.", outcome.expires_in_days);
        println!(
            "The single .hcred file carries the token and its proof key; keep it secret (mode 0600)."
        );
        println!(
            "Point the runtime at it with HEDDLE_CREDENTIAL={}.",
            outcome.credential_path
        );
        println!(
            "This token is scoped to the {} namespace.",
            outcome.namespace
        );
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
    println!("Claim offer ready for {}.", offer.pet_name);
    println!(
        "\nOpen this short-lived claim link:\n\n{}\n",
        offer.claim_link
    );
    println!(
        "Waiting up to {} for a human to finish claiming this account. Press Ctrl-C to stop.",
        display_duration(offer.timeout)
    );
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
