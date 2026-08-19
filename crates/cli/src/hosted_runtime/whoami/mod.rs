//! `heddle whoami` — capture actor first, hosted auth second.
//!
//! The capture actor is who the next capture is attributed to
//! (`user_config`, `init --principal-*`, environment). Hosted auth is
//! whether this machine has a server credential. These are different
//! objects. `identity ensure` does not set the local actor.

use anyhow::Result;
use weft_client_shim::CliContext;

use super::{auth::resolve_server, hosted::resolve_hosted_credential};

mod actor;
mod hosted;
mod report;

#[cfg(test)]
mod tests;

use actor::resolve_capture_actor;
use hosted::{fetch_identity, resolve_local_whoami};
use report::{WhoamiOutput, print_human};

/// `heddle whoami [--server <addr>]`.
pub async fn cmd_whoami(ctx: &dyn CliContext, server: Option<String>) -> Result<()> {
    let server = resolve_server(server.as_deref())?;
    let output = resolve_whoami(ctx, &server).await?;
    if ctx.should_output_json(None) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        print_human(&output);
    }
    Ok(())
}

async fn resolve_whoami(ctx: &dyn CliContext, server: &str) -> Result<WhoamiOutput> {
    let capture_actor = resolve_capture_actor(ctx)?;
    let resolved = resolve_hosted_credential(Some(server))?;
    let mut output = resolve_local_whoami(server, &resolved, capture_actor)?;
    if !output.authenticated {
        return Ok(output);
    }

    // Server round trip for the authoritative identity. Failure (unreachable,
    // rejected, or missing proof key) degrades to a local-only answer rather
    // than erroring — `reachable` records which case this is.
    output.identity = fetch_identity(server).await.ok();
    output.reachable = output.identity.is_some();
    if output
        .identity
        .as_ref()
        .is_some_and(|identity| identity.is_service_account)
    {
        output.token_kind = Some("service-account".to_string());
    }
    output.recommended_action = if !output.proof_key_available {
        Some(format!("heddle auth login --server {server}"))
    } else if !output.reachable {
        Some(format!(
            "server did not answer WhoAmI; check connectivity to {server} or re-run `heddle auth login --server {server}`"
        ))
    } else {
        None
    };
    Ok(output)
}
