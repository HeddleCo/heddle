//! Resident agent side of the human claim ceremony.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use config::UserConfig;
use heddle_cli_args::{ClaimArgs, DEFAULT_CLAIM_WEB_ORIGIN};

use super::{
    HostedAuthMode, HostedSession, agent_node_identity,
    auth::resolve_server,
    claim_authorization::validate_stored_claim_signer,
    hosted::{canonical_server_authority, server_keys_match},
    identity_state::{self, ClaimIssuanceStatus, ClaimSecret, ClaimState},
};

const HEDDLE_SAAS_API: &str = "https://api.heddle.sh";

const CLAIM_STATUS_POLL: Duration = Duration::from_millis(200);

struct ActiveClaimOffer {
    secret: ClaimSecret,
    authorization_hash: String,
    node_id: String,
    pet_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClaimWaitOutcome {
    Claimed,
    Expired,
    Interrupted,
    Replaced,
}

pub(crate) async fn cmd_claim(args: ClaimArgs) -> Result<()> {
    let server = resolve_server(args.server.as_deref())?;
    let state = claimable_state(&server)?;
    // Bind the destination before activate_offer mints the local bearer.
    let web_origin = resolve_web_origin(
        args.web_origin.as_deref(),
        state.web_origin.as_deref(),
        &server,
    )?;
    validate_stored_claim_signer(&state)?;

    let identity = agent_node_identity::load()?
        .context("no agent node identity exists; create an agent account with `heddle auth login --invite <code>` first")?;
    if identity.node_id().to_string() != state.node_id {
        bail!("agent-node-identity.toml does not match the account waiting to be claimed");
    }

    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?;
    let mut client = session
        .connect(([127, 0, 0, 1], 0).into())
        .await
        .with_context(|| {
            format!("connecting to {server} and bringing the claim listener online")
        })?;
    // Upload the claimable seq-0 root before the Iroh ceremony so claim
    // works even if this account never created a project spool.
    if let Err(error) = client.ensure_claimable_owner_root().await {
        client.close().await;
        return Err(error);
    }
    let completion = client.claim_completion();

    let offer = match activate_offer(&state, args.timeout) {
        Ok(offer) => offer,
        Err(error) => {
            client.close().await;
            return Err(error);
        }
    };
    let claim_link = claim_link(&web_origin, &offer.node_id, offer.secret.as_str());
    println!("Claim offer ready for {}.", offer.pet_name);
    println!("\nOpen this short-lived claim link:\n\n{claim_link}\n");
    println!(
        "Waiting up to {} for a human to finish claiming this account. Press Ctrl-C to stop.",
        display_duration(args.timeout)
    );
    drop(claim_link);
    drop(offer.secret);

    let outcome = wait_for_claim(&offer.authorization_hash, args.timeout, completion).await;
    let claimed = matches!(outcome, Ok(ClaimWaitOutcome::Claimed));
    let owner_root_claim = if claimed {
        super::owner_root::send_pending_register_public_key_claim(&mut client).await
    } else {
        Ok(None)
    };
    let cleanup = (!claimed)
        .then(|| deactivate_offer(&offer.authorization_hash))
        .transpose();
    client.close().await;
    cleanup?;
    owner_root_claim?;

    match outcome? {
        ClaimWaitOutcome::Claimed => {
            // Iroh promotes the handle. Owner-root claim is RegisterPublicKey
            // plus ClaimDeferredHuman (tag 16) over the existing sequence-0
            // agent key — never a replacement OwnerRootInstall.
            println!("Claim complete. This agent account now has a human owner.")
        }
        ClaimWaitOutcome::Expired => println!("Claim offer expired without changing the account."),
        ClaimWaitOutcome::Interrupted => {
            println!("Claim offer stopped; the link is no longer active.")
        }
        ClaimWaitOutcome::Replaced => {
            bail!("this claim offer was replaced by another `heddle claim` process")
        }
    }
    Ok(())
}

fn claimable_state(server: &str) -> Result<ClaimState> {
    let state = identity_state::load()?.context(
        "no agent account is waiting to be claimed; create one with `heddle auth login --invite <code>` first",
    )?;
    if !server_keys_match(&state.server, server) {
        bail!(
            "the stored agent account belongs to {}, but `heddle claim` targets {server}",
            state.server
        );
    }
    if state.is_claimed() {
        bail!("this agent account has already been claimed");
    }
    Ok(state)
}

fn activate_offer(expected: &ClaimState, timeout: Duration) -> Result<ActiveClaimOffer> {
    let timeout_millis = i64::try_from(timeout.as_millis())
        .context("claim timeout is too large for a millisecond timestamp")?;
    let expires_at_millis = chrono::Utc::now()
        .timestamp_millis()
        .checked_add(timeout_millis)
        .context("claim expiry overflows the timestamp range")?;
    let _guard = identity_state::write_lock()?;
    let mut state = identity_state::load_while_locked()?
        .context("agent claim state disappeared before the offer was activated")?;
    if state.owner_id != expected.owner_id
        || state.node_id != expected.node_id
        || !server_keys_match(&state.server, &expected.server)
    {
        bail!("agent claim state changed before the offer was activated");
    }
    let secret = state
        .activate(expires_at_millis)?
        .context("this agent account has already been claimed")?;
    let offer = ActiveClaimOffer {
        secret,
        authorization_hash: state.authorization_hash().to_string(),
        node_id: state.node_id.clone(),
        pet_name: state.pet_name.clone(),
    };
    identity_state::store_while_locked(&state)?;
    Ok(offer)
}

async fn wait_for_claim(
    authorization_hash: &str,
    timeout: Duration,
    mut completion: tokio::sync::watch::Receiver<bool>,
) -> Result<ClaimWaitOutcome> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(CLAIM_STATUS_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;

            delivered = completion.changed() => {
                delivered.context("claim listener stopped before promotion was delivered")?;
                if *completion.borrow() {
                    let state = identity_state::load()?
                        .context("agent claim state disappeared after promotion")?;
                    if state.issuance_status(
                        authorization_hash,
                        chrono::Utc::now().timestamp_millis(),
                    ) == ClaimIssuanceStatus::Claimed
                    {
                        return Ok(ClaimWaitOutcome::Claimed);
                    }
                }
            }
            result = tokio::signal::ctrl_c() => {
                result.context("waiting for Ctrl-C")?;
                return Ok(ClaimWaitOutcome::Interrupted);
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(ClaimWaitOutcome::Expired);
            }
            _ = poll.tick() => {
                let Some(state) = identity_state::load()? else {
                    return Ok(ClaimWaitOutcome::Replaced);
                };
                match state.issuance_status(
                    authorization_hash,
                    chrono::Utc::now().timestamp_millis(),
                ) {
                    ClaimIssuanceStatus::Active => {}
                    // ClaimState flips before the response is written. Wait
                    // for the delivery signal so shutdown cannot race the
                    // browser's PromoteConsent response.
                    ClaimIssuanceStatus::Claimed => {}
                    ClaimIssuanceStatus::Expired => return Ok(ClaimWaitOutcome::Expired),
                    ClaimIssuanceStatus::Replaced => return Ok(ClaimWaitOutcome::Replaced),
                }
            }
        }
    }
}

fn deactivate_offer(authorization_hash: &str) -> Result<()> {
    let _guard = identity_state::write_lock()?;
    let Some(mut state) = identity_state::load_while_locked()? else {
        return Ok(());
    };
    if state.deactivate_issuance(authorization_hash) {
        identity_state::store_while_locked(&state)?;
    }
    Ok(())
}

fn normalized_web_origin(value: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(value).context("claim web origin must be an absolute URL")?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!("claim web origin must be an https origin (scheme, host, and optional port only)");
    }
    Ok(parsed.origin().ascii_serialization())
}

/// Claim-link destination for the local bearer secret.
///
/// Precedence: explicit `--web-origin` (https origin-only, any host), then a
/// server-advertised origin that binds to the configured hosted server
/// (same host or same Public Suffix registrable domain), then
/// [`DEFAULT_CLAIM_WEB_ORIGIN`] only when that server is `api.heddle.sh`.
fn resolve_web_origin(
    explicit: Option<&str>,
    advertised: Option<&str>,
    server: &str,
) -> Result<String> {
    if let Some(explicit) = explicit {
        return normalized_web_origin(explicit).with_context(
            || "--web-origin must be an https origin (scheme, host, and optional port only)",
        );
    }
    if let Some(advertised) = advertised.map(str::trim).filter(|value| !value.is_empty()) {
        let origin = normalized_web_origin(advertised).with_context(|| {
            "server-advertised claim origin is not a valid https origin; pass --web-origin <https origin> to override"
        })?;
        bind_origin_to_server(&origin, server)?;
        return Ok(origin);
    }
    if is_heddle_saas_api(server)? {
        return normalized_web_origin(DEFAULT_CLAIM_WEB_ORIGIN);
    }
    bail!(
        "hosted server {server} is not {HEDDLE_SAAS_API} and has no bound claim web origin; pass --web-origin <https origin>"
    )
}

fn is_heddle_saas_api(server: &str) -> Result<bool> {
    Ok(canonical_server_authority(server)? == HEDDLE_SAAS_API)
}

fn bind_origin_to_server(origin: &str, server: &str) -> Result<()> {
    let origin_host = origin_host(origin)?;
    let server_host = hosted_server_host(server)?;
    if origin_host == server_host || same_registrable_domain(&origin_host, &server_host) {
        return Ok(());
    }
    bail!(
        "server-advertised claim origin {origin} is not bound to hosted server {server}; pass --web-origin <https origin> to override"
    )
}

fn origin_host(origin: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(origin).context("claim web origin is not a valid URL")?;
    parsed
        .host_str()
        .map(ascii_host)
        .context("claim web origin has no host")
}

fn hosted_server_host(server: &str) -> Result<String> {
    let canonical = canonical_server_authority(server)?;
    let parsed = reqwest::Url::parse(&canonical)
        .context("configured hosted server is not a valid HTTPS authority")?;
    parsed
        .host_str()
        .map(ascii_host)
        .context("configured hosted server has no host")
}

fn ascii_host(host: &str) -> String {
    host.to_ascii_lowercase()
}

fn same_registrable_domain(left_host: &str, right_host: &str) -> bool {
    match (psl::domain_str(left_host), psl::domain_str(right_host)) {
        (Some(left), Some(right)) => left.as_bytes() == right.as_bytes(),
        _ => false,
    }
}

fn claim_link(web_origin: &str, node_id: &str, secret: &str) -> String {
    format!("{web_origin}/claim/{node_id}.{secret}")
}

fn display_duration(duration: Duration) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_link_uses_the_origin_node_and_bearer_secret() {
        let origin = normalized_web_origin("https://heddle.example:8443/").expect("origin");
        assert_eq!(
            claim_link(&origin, &"11".repeat(32), "short-lived-secret"),
            format!(
                "https://heddle.example:8443/claim/{}.short-lived-secret",
                "11".repeat(32)
            )
        );
    }

    #[test]
    fn claim_link_refuses_a_non_origin_base() {
        for invalid in [
            "heddle.example",
            "ftp://heddle.example",
            "http://heddle.example",
            "http://localhost:3000",
            "https://heddle.example/path",
            "https://heddle.example/?query=1",
            "https://user:pass@heddle.example",
        ] {
            assert!(
                normalized_web_origin(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn explicit_https_origin_wins_even_when_cross_site() {
        assert_eq!(
            resolve_web_origin(
                Some("https://explicit.example:8443/"),
                Some("https://app.acme.example"),
                "api.acme.example",
            )
            .expect("explicit origin"),
            "https://explicit.example:8443"
        );
    }

    #[test]
    fn saas_api_falls_back_to_app_heddle_sh() {
        assert_eq!(
            resolve_web_origin(None, None, "api.heddle.sh").expect("saas default"),
            DEFAULT_CLAIM_WEB_ORIGIN
        );
        assert_eq!(
            resolve_web_origin(None, None, "https://api.heddle.sh").expect("canonical saas"),
            DEFAULT_CLAIM_WEB_ORIGIN
        );
        assert_eq!(DEFAULT_CLAIM_WEB_ORIGIN, "https://app.heddle.sh");
    }

    #[test]
    fn self_hosted_without_a_bound_origin_refuses_the_saas_default() {
        let error = resolve_web_origin(None, None, "api.acme.example")
            .expect_err("self-hosted must not fall back to SaaS");
        let message = error.to_string();
        assert!(
            message.contains("--web-origin"),
            "refusal should name the override: {message}"
        );
        assert!(
            !message.contains(DEFAULT_CLAIM_WEB_ORIGIN) || message.contains("pass --web-origin"),
            "refusal must not silently select the SaaS app origin: {message}"
        );
    }

    #[test]
    fn bound_server_advertised_origin_is_accepted() {
        assert_eq!(
            resolve_web_origin(None, Some("https://app.acme.example/"), "api.acme.example")
                .expect("same registrable domain"),
            "https://app.acme.example"
        );
        assert_eq!(
            resolve_web_origin(None, Some("https://api.acme.example"), "api.acme.example")
                .expect("same host"),
            "https://api.acme.example"
        );
        assert_eq!(
            resolve_web_origin(None, Some("https://app.acme.co.uk/"), "api.acme.co.uk")
                .expect("psl multi-part suffix"),
            "https://app.acme.co.uk"
        );
    }

    #[test]
    fn leftover_unbound_web_origin_cannot_become_a_claim_link() {
        for advertised in [
            "https://evil.com",
            "http://evil.com",
            "https://evil.com/claim",
            "https://notacme.example",
            "https://bar.github.io",
        ] {
            let error = resolve_web_origin(None, Some(advertised), "api.acme.example")
                .expect_err("unbound leftover origin");
            let message = error.to_string();
            assert!(
                !message.contains("https://evil.com/claim/")
                    && !claim_link_would_use_host(&message, "evil.com"),
                "refused origin must not be formatted as a claim link: {message}"
            );
            assert!(
                message.contains("--web-origin") || message.contains("https"),
                "refusal should be actionable: {message}"
            );
        }
        let error = resolve_web_origin(None, Some("https://bar.github.io"), "api.foo.github.io")
            .expect_err("public suffix is not a string suffix");
        assert!(
            error.to_string().contains("--web-origin"),
            "github.io siblings must not share a registrable domain: {error}"
        );
    }

    #[test]
    fn string_suffix_is_not_a_registrable_domain_bind() {
        let error = resolve_web_origin(None, Some("https://evil.com"), "api.notevil.com")
            .expect_err("ends-with is not a bind");
        assert!(
            error.to_string().contains("--web-origin"),
            "string-suffix trap must fail closed: {error}"
        );
    }

    #[test]
    fn claim_link_refuses_http_even_when_explicit() {
        for invalid in ["http://evil.example", "http://app.heddle.sh"] {
            let error =
                resolve_web_origin(Some(invalid), None, "api.heddle.sh").expect_err("http origin");
            assert!(
                error.to_string().contains("https"),
                "http refusal should name the https requirement: {error}"
            );
        }
    }

    fn claim_link_would_use_host(message: &str, host: &str) -> bool {
        message.contains(&format!("https://{host}/claim/"))
            || message.contains(&format!("http://{host}/claim/"))
    }
}
