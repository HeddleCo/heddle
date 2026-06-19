//! `heddle auth` command implementations.

use anyhow::{Result, bail};
use crypto::{Ed25519Signer, Signer};
use grpc::heddle::v1::{
    CreateDeviceAuthorizationRequest, CreateServiceAccountRequest, DeviceAuthorizationResponse,
    ExchangeDeviceAuthorizationRequest, IssueServiceAccountCredentialRequest,
    WaitForDeviceAuthorizationRequest, auth_service_client::AuthServiceClient,
};
use serde::Serialize;
use tonic::{
    metadata::MetadataValue,
    transport::{Channel, Endpoint},
};

use crate::{auth_args::AuthCommands, credentials, credentials::ServerCredential};
use cli_shared::{ClientCommandContext, RemoteRecoveryAdvice};

/// Top-level dispatch for `heddle auth <subcommand>`. `_ctx` is
/// reserved for future remote commands that need repo path / output
/// mode — today's auth subcommands all operate on global credential
/// state and don't read it.
#[derive(Serialize)]
struct AuthLogoutOutput {
    output_kind: &'static str,
    server: String,
    removed: bool,
    /// Whether a device signing identity recorded for this server (heddle#482)
    /// was removed. `false` when none was linked; on a removal failure logout
    /// errors instead of emitting this output, so a `true` here always means
    /// the logged-out private key is no longer on disk.
    device_identity_removed: bool,
}

#[derive(Serialize)]
struct AuthStatusOutput {
    output_kind: &'static str,
    server: String,
    authenticated: bool,
    subject: Option<String>,
    credential_id: Option<String>,
    expires_at: Option<String>,
    recommended_action: Option<String>,
}

#[derive(Serialize)]
struct ServiceTokenOutput {
    output_kind: &'static str,
    name: String,
    namespace: String,
    scope: String,
    token: String,
    expires_in_days: u32,
}

pub async fn cmd_auth(ctx: &ClientCommandContext, command: AuthCommands) -> Result<()> {
    match command {
        AuthCommands::Login { server, no_browser } => cmd_auth_login(&server, no_browser).await,
        AuthCommands::Logout { server } => cmd_auth_logout(ctx, server.as_deref()),
        AuthCommands::Status { server } => cmd_auth_status(ctx, server.as_deref()),
        AuthCommands::CreateServiceToken {
            name,
            namespace,
            server,
        } => cmd_create_service_token(ctx, server.as_deref(), name, namespace).await,
    }
}

/// Authenticate via device authorization flow.
async fn cmd_auth_login(server: &str, no_browser: bool) -> Result<()> {
    // 1. Generate Ed25519 keypair for device binding.
    let signer = Ed25519Signer::generate()
        .map_err(|e| anyhow::anyhow!("failed to generate keypair: {e}"))?;
    let public_key_bytes = signer.public_key().to_vec();
    let private_key_pem = signer
        .to_pem()
        .map_err(|e| anyhow::anyhow!("failed to export private key: {e}"))?;

    // 2. Connect to the auth service.
    let mut auth_client: AuthServiceClient<Channel> = connect_auth_client(server).await?;

    // 3. Create device authorization.
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "heddle-cli".to_string());

    let response: DeviceAuthorizationResponse = auth_client
        .create_device_authorization(CreateDeviceAuthorizationRequest {
            device_name: hostname,
            device_public_key: public_key_bytes.clone(),
            scope: "repo:*".to_string(),
            client_operation_id: String::new(),
        })
        .await
        .map_err(|status: tonic::Status| {
            anyhow::anyhow!("create_device_authorization failed: {}", status.message())
        })?
        .into_inner();

    let verification_uri = &response.verification_uri;
    let user_code = &response.user_code;
    let device_code = &response.device_code;

    // 4. Print instructions.
    println!();
    println!("Open this URL to authorize:");
    println!("  {verification_uri}");
    println!();
    println!("Enter code: {user_code}");
    println!();

    // 5. Attempt to open browser.
    if !no_browser {
        let url = format!("{verification_uri}?code={user_code}");
        if let Err(_e) = open_url(&url) {
            eprintln!("Could not open browser automatically. Please open the URL above.");
        }
    }

    // 6. Poll for approval.
    println!("Waiting for authorization...");

    let access_token = poll_for_approval(
        &mut auth_client,
        device_code,
        &public_key_bytes,
        &signer,
        response.expires_at,
    )
    .await?;

    // 7. Store credential.
    let credential = ServerCredential {
        token: access_token.token,
        subject: access_token.subject.clone(),
        device_id: None,
        credential_id: if access_token.credential_id.is_empty() {
            None
        } else {
            Some(access_token.credential_id)
        },
        private_key_pem: Some(private_key_pem.clone()),
        expires_at: access_token.expires_at.as_ref().and_then(|ts| {
            chrono::DateTime::from_timestamp(ts.seconds, ts.nanos.max(0) as u32)
                .map(|dt| dt.to_rfc3339())
        }),
    };

    credentials::store_server_credential(server, credential)?;

    // Reconcile the local signing identity with the device key (heddle#482):
    // record the device key as the machine's active signing identity so
    // subsequent captures sign with it (it supersedes any per-repo local key;
    // states already signed by a local key keep verifying). Best-effort — a
    // failure here just leaves captures signing with the local key.
    if let Err(error) = repo::identity::link_device_key(&public_key_bytes, &private_key_pem, server)
    {
        tracing::warn!(%error, "could not record device signing identity; captures will use the per-repo local key");
    }

    println!();
    println!(
        "Authenticated as {}. Credentials saved.",
        access_token.subject
    );
    Ok(())
}

/// Remove stored credentials.
fn cmd_auth_logout(ctx: &ClientCommandContext, server: Option<&str>) -> Result<()> {
    let server = resolve_server(server)?;

    // Remove the device signing identity `auth login` recorded for THIS server
    // BEFORE dropping the credential (heddle#482 ordering). Fail-closed: if a
    // matching device key is on disk but can't be removed, surface the error
    // rather than reporting a clean logout while the logged-out private key —
    // which `signing_signer()` would keep preferring for every capture — still
    // persists. Unlinking first keeps a failed logout retryable: were the
    // credential removed first, `resolve_server` would no longer resolve back to
    // this server, so a no-arg retry could never re-target the matching
    // `device-identity.toml` still on disk. Holding the credential until the
    // unlink succeeds preserves the same server resolution for the retry.
    let device_identity_removed = repo::identity::unlink_device_key(&server).map_err(|error| {
        anyhow::anyhow!("failed to remove device signing identity for {server}: {error}")
    })?;
    credentials::remove_server_credential(&server)?;

    if ctx.should_output_json(None) {
        let output = AuthLogoutOutput {
            output_kind: "auth_logout",
            server,
            removed: true,
            device_identity_removed,
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Credentials removed for {server}.");
        if device_identity_removed {
            println!("Device signing identity removed.");
        }
    }
    Ok(())
}

/// Show current authentication status.
fn cmd_auth_status(ctx: &ClientCommandContext, server: Option<&str>) -> Result<()> {
    let server = resolve_server(server)?;
    match credentials::get_server_credential(&server)? {
        Some(cred) => {
            if ctx.should_output_json(None) {
                let output = AuthStatusOutput {
                    output_kind: "auth_status",
                    server,
                    authenticated: true,
                    subject: Some(cred.subject),
                    credential_id: cred.credential_id,
                    expires_at: cred.expires_at,
                    recommended_action: None,
                };
                println!("{}", serde_json::to_string(&output)?);
            } else {
                println!("Server:        {server}");
                println!("Subject:       {}", cred.subject);
                if let Some(ref cred_id) = cred.credential_id {
                    println!("Credential:    {cred_id}");
                }
                if let Some(ref expires) = cred.expires_at {
                    println!("Expires:       {expires}");
                }
            }
        }
        None => {
            let recommended_action = format!("heddle auth login --server {server}");
            if ctx.should_output_json(None) {
                let output = AuthStatusOutput {
                    output_kind: "auth_status",
                    server,
                    authenticated: false,
                    subject: None,
                    credential_id: None,
                    expires_at: None,
                    recommended_action: Some(recommended_action),
                };
                println!("{}", serde_json::to_string(&output)?);
            } else {
                println!("Not authenticated with {server}.");
                println!("Run `{recommended_action}` to authenticate.");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Create service token
// ---------------------------------------------------------------------------

/// Create a namespace-scoped service token for CI/ephemeral runners.
async fn cmd_create_service_token(
    ctx: &ClientCommandContext,
    server: Option<&str>,
    name: String,
    namespace: String,
) -> Result<()> {
    let server = resolve_server(server)?;
    let scope = format!("repo:{namespace}/*");

    // Load the calling user's token to authenticate with the server.
    let cred = credentials::get_server_credential(&server)?
        .ok_or_else(|| anyhow::anyhow!(RemoteRecoveryAdvice::auth_required(&server)))?;

    // Generate a fresh Ed25519 keypair for the service account credential.
    let signer = Ed25519Signer::generate()
        .map_err(|e| anyhow::anyhow!("failed to generate keypair: {e}"))?;
    let public_key_bytes = signer.public_key().to_vec();

    let channel = connect_channel(&server).await?;

    // Attach the caller's token as a Bearer header on every request.
    let bearer: MetadataValue<_> = format!("Bearer {}", cred.token)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid token format: {e}"))?;

    let mut auth_client =
        AuthServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        });

    // 1. Create the service account.
    let sa_response = auth_client
        .create_service_account(CreateServiceAccountRequest {
            subject: name.clone(),
            display_name: name.clone(),
            scope: scope.clone(),
            client_operation_id: String::new(),
        })
        .await
        .map_err(|status| anyhow::anyhow!("create_service_account failed: {}", status.message()))?
        .into_inner();

    tracing::info!(
        service_account_id = %sa_response.service_account_id,
        subject = %sa_response.subject,
        "service account created"
    );

    // 2. Issue a credential (token) for the service account.
    let issued = auth_client
        .issue_service_account_credential(IssueServiceAccountCredentialRequest {
            service_account_id: sa_response.service_account_id,
            public_key: public_key_bytes,
            scope: scope.clone(),
            // CLI-issued tokens retain their pre-TTL behaviour: 30-day
            // expiry from the server's default (applied when ttl_secs == 0
            // in the handler is "never expires", so pass 30 days here
            // explicitly to preserve prior semantics).
            ttl_secs: Some(prost_types::Duration {
                seconds: 30 * 24 * 3600,
                nanos: 0,
            }),
            client_operation_id: String::new(),
        })
        .await
        .map_err(|status| {
            anyhow::anyhow!(
                "issue_service_account_credential failed: {}",
                status.message()
            )
        })?
        .into_inner();

    if ctx.should_output_json(None) {
        let output = ServiceTokenOutput {
            output_kind: "auth_create_service_token",
            name,
            namespace,
            scope,
            token: issued.token,
            expires_in_days: 30,
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!();
        println!("Service token created for \"{}\" (scope: {scope})", name);
        println!();
        println!("Token: {}", issued.token);
        println!();
        println!("Set this as HEDDLE_REMOTE_TOKEN in your CI environment.");
        println!("This token is scoped to the {namespace} namespace.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve server from explicit arg, default credential, or fallback.
fn resolve_server(explicit: Option<&str>) -> Result<String> {
    if let Some(s) = explicit {
        return Ok(s.to_string());
    }
    if let Some(default) = credentials::default_server()? {
        return Ok(default);
    }
    Ok("grpc.heddle.sh".to_string())
}

/// Connect a raw gRPC channel to the given server.
async fn connect_channel(server: &str) -> Result<Channel> {
    let uri = infer_server_uri(server);
    let endpoint = Endpoint::from_shared(uri.clone())
        .map_err(|e| anyhow::anyhow!("invalid server address '{server}': {e}"))?;
    endpoint
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to {server}: {e}"))
}

/// Connect an unauthenticated `AuthServiceClient` to the given server.
async fn connect_auth_client(server: &str) -> Result<AuthServiceClient<Channel>> {
    Ok(AuthServiceClient::new(connect_channel(server).await?))
}

fn infer_server_uri(server: &str) -> String {
    if server.starts_with("http://") || server.starts_with("https://") {
        return server.to_string();
    }

    let authority = server.split('/').next().unwrap_or(server);
    let host = authority
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
        .map(|(value, _)| value)
        .unwrap_or_else(|| {
            authority
                .rsplit_once(':')
                .map(|(value, _)| value)
                .unwrap_or(authority)
        });

    let use_http = host.contains("localhost")
        || host.parse::<std::net::IpAddr>().is_ok()
        || authority.parse::<std::net::SocketAddr>().is_ok();

    if use_http {
        format!("http://{server}")
    } else {
        format!("https://{server}")
    }
}

/// Poll `ExchangeDeviceAuthorization` until the device code is approved or
/// the authorization expires.
async fn poll_for_approval(
    client: &mut AuthServiceClient<Channel>,
    device_code: &str,
    public_key: &[u8],
    signer: &Ed25519Signer,
    expires_at: Option<prost_types::Timestamp>,
) -> Result<AccessToken> {
    let proof_data = format!("device:{device_code}");
    let proof_bytes = signer
        .sign(proof_data.as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to sign proof: {e}"))?;

    let expires_at_secs = expires_at
        .as_ref()
        .map(|t| t.seconds.max(0) as u64)
        .unwrap_or(0);
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(
            expires_at_secs
                .saturating_sub(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                )
                .max(30),
        );

    let mut events = client
        .wait_for_device_authorization(WaitForDeviceAuthorizationRequest {
            device_code: device_code.to_string(),
        })
        .await
        .map_err(|status| {
            anyhow::anyhow!("wait_for_device_authorization failed: {}", status.message())
        })?
        .into_inner();

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            bail!("Authorization timed out. Please try again.");
        }

        let event = tokio::time::timeout(remaining, events.message())
            .await
            .map_err(|_| anyhow::anyhow!("Authorization timed out. Please try again."))?
            .map_err(|status| {
                anyhow::anyhow!("device authorization wait failed: {}", status.message())
            })?;

        match event {
            Some(status) if status.status == "pending" => continue,
            Some(status) if status.status == "approved" => break,
            Some(status) if status.status == "expired" => {
                bail!("Authorization expired before approval. Please try again.");
            }
            Some(status) => bail!("Unexpected device authorization status: {}", status.status),
            None => bail!("Device authorization ended before approval. Please try again."),
        }
    }

    let inner = client
        .exchange_device_authorization(ExchangeDeviceAuthorizationRequest {
            device_code: device_code.to_string(),
            device_public_key: public_key.to_vec(),
            proof: proof_bytes,
        })
        .await
        .map_err(|status| anyhow::anyhow!("device authorization failed: {}", status.message()))?
        .into_inner();

    Ok(AccessToken {
        token: inner.token,
        subject: inner.subject,
        expires_at: inner.expires_at,
        credential_id: inner.credential_id,
    })
}

/// Extracted token fields from `AccessTokenResponse`.
struct AccessToken {
    token: String,
    subject: String,
    expires_at: Option<prost_types::Timestamp>,
    credential_id: String,
}

/// Best-effort browser open.
fn open_url(url: &str) -> Result<()> {
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
        std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_http_for_plain_ip_targets() {
        assert_eq!(
            infer_server_uri("192.168.1.44:8421"),
            "http://192.168.1.44:8421"
        );
        assert_eq!(infer_server_uri("10.0.0.8"), "http://10.0.0.8");
    }

    #[test]
    fn infers_http_for_loopback_targets() {
        assert_eq!(infer_server_uri("localhost:8421"), "http://localhost:8421");
        assert_eq!(infer_server_uri("[::1]:8421"), "http://[::1]:8421");
    }

    #[test]
    fn keeps_https_default_for_hostnames() {
        assert_eq!(infer_server_uri("grpc.heddle.sh"), "https://grpc.heddle.sh");
        assert_eq!(
            infer_server_uri("example.internal:8443"),
            "https://example.internal:8443"
        );
    }

    #[test]
    fn preserves_explicit_scheme() {
        assert_eq!(
            infer_server_uri("http://example.internal:8421"),
            "http://example.internal:8421"
        );
        assert_eq!(
            infer_server_uri("https://grpc.heddle.sh"),
            "https://grpc.heddle.sh"
        );
    }

    fn text_ctx() -> ClientCommandContext {
        ClientCommandContext::new(None, "", repo::OutputFormat::Text, None)
    }

    /// Run `f` with `HOME` pointed at a fresh temp dir and `HEDDLE_HOME` cleared,
    /// so the credential store and device identity both resolve under
    /// `<temp>/.heddle`. Serialised with the credential store's env tests.
    fn with_isolated_home<T>(f: impl FnOnce() -> T) -> T {
        let _guard = credentials::lock_test_env();
        let temp = tempfile::TempDir::new().expect("temp home");
        let prev_home = std::env::var_os("HOME");
        let prev_heddle_home = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::remove_var("HEDDLE_HOME");
        }
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prev_heddle_home {
                Some(value) => std::env::set_var("HEDDLE_HOME", value),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
        }
        drop(temp);
        match out {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn sample_credential() -> ServerCredential {
        ServerCredential {
            token: "tkn".to_string(),
            subject: "dev".to_string(),
            device_id: None,
            credential_id: None,
            private_key_pem: None,
            expires_at: None,
        }
    }

    /// On a successful logout, both the credential and the matching device
    /// signing identity are removed (heddle#482).
    #[test]
    fn logout_removes_credential_and_device_identity_on_success() {
        with_isolated_home(|| {
            credentials::store_server_credential("grpc.S", sample_credential())
                .expect("store credential");

            // Record a matching device identity via the real link path.
            let signer = Ed25519Signer::generate().expect("keypair");
            repo::identity::link_device_key(
                signer.public_key(),
                &signer.to_pem().expect("pem"),
                "grpc.S",
            )
            .expect("link device key");
            assert!(repo::identity::device_identity_path().exists());

            // No explicit --server: resolve_server falls back to the stored
            // default written by `store_server_credential`.
            cmd_auth_logout(&text_ctx(), None).expect("logout succeeds");

            assert!(
                credentials::get_server_credential("grpc.S")
                    .expect("load")
                    .is_none(),
                "credential must be removed on a successful logout",
            );
            assert!(
                !repo::identity::device_identity_path().exists(),
                "device identity must be removed on a successful logout",
            );
        });
    }

    /// Ordering (heddle#482): logout unlinks the device identity BEFORE dropping
    /// the credential, so a fail-closed unlink leaves the credential/default
    /// intact and `heddle auth logout` stays retryable with the SAME server
    /// resolution. A corrupt device-identity file stands in for any unlink
    /// failure (it fails the parse). Were the credential removed first, a no-arg
    /// retry could no longer resolve back to this server.
    #[test]
    fn logout_preserves_credential_when_device_unlink_fails() {
        with_isolated_home(|| {
            credentials::store_server_credential("grpc.S", sample_credential())
                .expect("store credential");

            let device_path = repo::identity::device_identity_path();
            std::fs::create_dir_all(device_path.parent().expect("home parent")).expect("home dir");
            std::fs::write(
                &device_path,
                b"!!! definitely not valid device-identity toml !!!",
            )
            .expect("write corrupt device identity");

            let result = cmd_auth_logout(&text_ctx(), None);
            assert!(
                result.is_err(),
                "a failed device unlink must fail the logout, not report a clean removal",
            );

            assert!(
                credentials::get_server_credential("grpc.S")
                    .expect("load")
                    .is_some(),
                "credential must be preserved when unlink fails, so logout is retryable",
            );
            assert_eq!(
                credentials::default_server().expect("default").as_deref(),
                Some("grpc.S"),
                "default server must be preserved so a no-arg retry re-targets the same server",
            );
        });
    }
}
