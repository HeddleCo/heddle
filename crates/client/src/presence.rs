//! Local-agent presence publisher (Track B).
//!
//! Runs in the foreground: reads the configured hosted upstream + namespace
//! from `.heddle/config.toml`, resolves a bearer token from the user config or
//! `HEDDLE_REMOTE_TOKEN`, opens a WebSocket to `<upstream>/presence/ws`, and
//! streams `agent_start` → periodic `agent_heartbeat` → `agent_done` events.
//!
//! This module is gated on the `client` feature because the WebSocket
//! client (and therefore `tokio-tungstenite`) is only pulled in for hosted
//! builds.
//!
//! # Scope
//!
//! - No daemonization, no PID stashing, no signal handling beyond Ctrl-C.
//! - Orchestrators (e.g. Claude Code's hook system, or `heddle actor spawn`)
//!   are expected to invoke this subcommand themselves; there is no
//!   automatic launch on actor registration.
//! - Tokens are fetched once at startup. Long-running agents should use
//!   device-bound tokens (30-day TTL minted via `heddle auth login`). On
//!   `Unauthorized` mid-stream the publisher exits cleanly and logs a
//!   pointer at re-running `heddle auth login` — no in-band refresh.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use cli_shared::UserConfig;
use futures::{SinkExt, StreamExt};
use objects::store::{AgentEntry, AgentRegistry};
use repo::{HostedConfig, Repository};
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    sync::Mutex,
    time::{self, Instant},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::header::AUTHORIZATION,
        protocol::{CloseFrame, Message, frame::coding::CloseCode},
    },
};
use tracing::{debug, info, warn};
use weft_client_shim::CliContext;

use crate::credentials;

/// Local mirror of `weft_server::presence::hub::PresenceEvent`.
///
/// Kept in sync with the server definition so we don't add a
/// `cli → server` dep (which would pull in axum + sqlx into the
/// CLI build). The shape is small and stable — if either side evolves, a
/// compile-time reminder should land via a failing integration test.
// The `Agent` prefix is load-bearing: variant names mirror
// `weft_server::presence::hub::PresenceEvent` exactly so the mirror
// invariant documented above holds. Renaming would drift the two sides
// and make any future grep-based refactor miss this side.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PresenceEvent {
    #[serde(rename = "agent_start")]
    Start {
        session_id: String,
        subject: String,
        namespace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor_path: Option<String>,
        started_at_ms: u64,
    },
    #[serde(rename = "agent_heartbeat")]
    Heartbeat {
        session_id: String,
        subject: String,
        namespace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor_path: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        recent_actions: Vec<String>,
        ts_ms: u64,
    },
    #[serde(rename = "agent_done")]
    Done {
        session_id: String,
        subject: String,
        namespace: String,
        ts_ms: u64,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame<'a> {
    Hello { role: &'a str },
    Publish { event: &'a PresenceEvent },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame {
    Ready {
        #[allow(dead_code)]
        subscribed: Vec<String>,
    },
    Event {
        #[allow(dead_code)]
        ts_ms: u64,
        #[allow(dead_code)]
        event: serde_json::Value,
    },
    Error {
        code: String,
        message: String,
    },
}

/// Configuration resolved from the repository + user configs + CLI flags.
#[derive(Debug, Clone)]
pub struct PublisherConfig {
    pub session_id: String,
    pub subject: String,
    pub namespace: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub token: String,
    pub ws_url: String,
    pub interval: Duration,
}

/// Entry point used by `main.rs`.
pub async fn cmd_presence_publish(
    ctx: &dyn CliContext,
    session: String,
    interval_secs: u64,
) -> Result<()> {
    let repo_root = match ctx.repo_path() {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let repo = Repository::open(&repo_root).with_context(|| {
        format!(
            "failed to open Heddle repository at {}",
            repo_root.display()
        )
    })?;

    let hosted = repo.config().hosted.clone();
    let agent = load_agent_entry(repo.heddle_dir(), &session)?;

    let user_config = UserConfig::load_default()?;

    match resolve_publisher_config(
        &hosted,
        &agent,
        &user_config,
        Duration::from_secs(interval_secs),
    )? {
        Some(config) => run_publisher(config).await,
        None => Err(anyhow!(
            "hosted presence requires a repository linked to a Heddle hosted upstream. Configure [hosted] in .heddle/config.toml or use a hosted-enabled repository."
        )),
    }
}

fn load_agent_entry(heddle_dir: &Path, session: &str) -> Result<AgentEntry> {
    let registry = AgentRegistry::new(heddle_dir);
    registry
        .load(session)
        .with_context(|| format!("failed to read agent registry for session '{session}'"))?
        .ok_or_else(|| {
            anyhow!(
                "no agent entry found for session '{session}' at {}",
                heddle_dir
                    .join("agents")
                    .join(format!("{session}.toml"))
                    .display()
            )
        })
}

/// Resolve everything the publisher loop needs up front.
///
/// Returns `Ok(None)` when the repository is local-only (no upstream or
/// namespace configured). Returns `Err` only for unrecoverable setup problems
/// (missing token, malformed URL, etc).
pub fn resolve_publisher_config(
    hosted: &HostedConfig,
    agent: &AgentEntry,
    user_config: &UserConfig,
    interval: Duration,
) -> Result<Option<PublisherConfig>> {
    let (Some(upstream), Some(namespace)) = (
        hosted.upstream_url.as_deref().filter(|s| !s.is_empty()),
        hosted.namespace.as_deref().filter(|s| !s.is_empty()),
    ) else {
        return Ok(None);
    };

    let (token, credential_subject) = if let Some(token) = user_config.remote_token()? {
        (token.id, None)
    } else {
        let stored_credential = credentials::resolve_credential_for_server(upstream)
            .with_context(|| format!("loading stored credential for {upstream}"))?;
        if let Some(credential) = stored_credential {
            (credential.token, Some(credential.subject))
        } else {
            return Err(anyhow!(
                "no remote token available — set HEDDLE_REMOTE_TOKEN or run `heddle auth login`"
            ));
        }
    };

    let ws_url = normalize_ws_url(upstream)?;

    // Biscuit tokens are intentionally opaque to the CLI. Use the configured
    // principal as the subject we publish, and let the server validate it
    // against the authenticated Biscuit facts.
    let subject = user_config
        .principal
        .as_ref()
        .map(|p| p.email.clone())
        .or(credential_subject)
        .ok_or_else(|| {
            anyhow!("could not derive subject from principal config — run `heddle auth login`")
        })?;

    Ok(Some(PublisherConfig {
        session_id: agent.session_id.clone(),
        subject,
        namespace: namespace.to_string(),
        model: agent.model.clone(),
        provider: agent.provider.clone(),
        token,
        ws_url,
        interval,
    }))
}

/// Normalise an upstream URL into a `ws(s)://…/presence/ws` target.
///
/// Accepts any of: `https://host`, `http://host:8421`, `ws://host`,
/// `wss://host/path` (with or without trailing slash). The path is
/// replaced with `/presence/ws`.
fn normalize_ws_url(upstream: &str) -> Result<String> {
    let trimmed = upstream.trim_end_matches('/');
    let (scheme_end, ws_scheme) = if let Some(rest) = trimmed.strip_prefix("https://") {
        (trimmed.len() - rest.len(), "wss://")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        (trimmed.len() - rest.len(), "ws://")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        return Ok(strip_path_and_append_presence(trimmed));
    } else {
        return Err(anyhow!(
            "unsupported upstream URL '{upstream}' (expected http(s):// or ws(s)://)"
        ));
    };
    // Strip any path component from `<scheme>host[:port]/…`.
    let host_start = scheme_end;
    let after = &trimmed[host_start..];
    let host = match after.find('/') {
        Some(idx) => &after[..idx],
        None => after,
    };
    Ok(format!("{ws_scheme}{host}/presence/ws"))
}

fn strip_path_and_append_presence(url: &str) -> String {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("wss://") {
        ("wss://", r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        ("ws://", r)
    } else {
        return url.to_string();
    };
    let host = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}{host}/presence/ws")
}

/// Main reconnect loop. Runs until Ctrl-C or an authoritative auth failure.
pub async fn run_publisher(config: PublisherConfig) -> Result<()> {
    let config = Arc::new(config);
    let cancelled = Arc::new(Mutex::new(false));
    let cancel_signal = {
        let cancelled = cancelled.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                *cancelled.lock().await = true;
                info!("presence: Ctrl-C received, shutting down");
            }
        }
    };
    tokio::spawn(cancel_signal);

    let mut backoff = BackoffPlan::new();
    loop {
        if *cancelled.lock().await {
            return Ok(());
        }
        match connect_and_stream(config.clone(), cancelled.clone()).await {
            Ok(exit) => match exit {
                LoopExit::Cancelled => return Ok(()),
                LoopExit::Disconnected => {
                    let delay = backoff.next();
                    warn!(
                        retry_in_ms = delay.as_millis() as u64,
                        "presence: disconnected, reconnecting"
                    );
                    if wait_or_cancel(delay, cancelled.clone()).await {
                        return Ok(());
                    }
                }
            },
            Err(ConnectError::Unauthorized) => {
                // Bail cleanly; long-running agents should mint a
                // device-bound token via `heddle auth login` (30-day TTL)
                // to avoid hitting this mid-session.
                warn!(
                    "presence: authentication failed (401) — token expired or revoked. \
                     Re-run `heddle auth login` to continue publishing. \
                     (Device-bound tokens have a 30-day TTL and are recommended for \
                     long-running agent sessions.)"
                );
                return Err(anyhow!("presence publisher: unauthorized"));
            }
            Err(ConnectError::Forbidden(err)) => {
                warn!(
                    error = %err,
                    "presence: server returned 403 forbidden — scope mismatch or namespace \
                     not provisioned. Not retrying.",
                );
                return Err(anyhow!("presence publisher: forbidden — {err}"));
            }
            Err(ConnectError::Fatal(err)) => {
                return Err(err);
            }
            Err(ConnectError::Transient(err)) => {
                let delay = backoff.next();
                warn!(error = %err, retry_in_ms = delay.as_millis() as u64, "presence: transient error, reconnecting");
                if wait_or_cancel(delay, cancelled.clone()).await {
                    return Ok(());
                }
            }
        }
    }
}

enum LoopExit {
    Cancelled,
    Disconnected,
}

enum ConnectError {
    /// HTTP 401 on connect or server reported `unauthorized` mid-stream.
    /// Structural auth failure — don't retry without a fresh token.
    Unauthorized,
    /// HTTP 403 on connect or server reported `forbidden` mid-stream.
    /// Scope/namespace mismatch — a refresh won't help.
    Forbidden(anyhow::Error),
    Transient(anyhow::Error),
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for ConnectError {
    fn from(err: anyhow::Error) -> Self {
        ConnectError::Transient(err)
    }
}

async fn wait_or_cancel(delay: Duration, cancelled: Arc<Mutex<bool>>) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if *cancelled.lock().await {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let step = (deadline - now).min(Duration::from_millis(250));
        tokio::time::sleep(step).await;
    }
}

async fn connect_and_stream(
    config: Arc<PublisherConfig>,
    cancelled: Arc<Mutex<bool>>,
) -> Result<LoopExit, ConnectError> {
    debug!(url = %config.ws_url, "presence: connecting");

    let request = config
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| ConnectError::Fatal(anyhow!("invalid WS URL: {e}")))?;
    let mut request = request;
    request.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {}", config.token)
            .parse()
            .map_err(|e| ConnectError::Fatal(anyhow!("invalid bearer token: {e}")))?,
    );

    let (ws, _resp) = match connect_async(request).await {
        Ok(pair) => pair,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) if resp.status() == 401 => {
            return Err(ConnectError::Unauthorized);
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) if resp.status() == 403 => {
            return Err(ConnectError::Forbidden(anyhow!(
                "server returned 403 on WebSocket handshake"
            )));
        }
        Err(err) => return Err(ConnectError::Transient(anyhow!("ws connect: {err}"))),
    };

    let (mut tx, mut rx) = ws.split();

    // Hello.
    send_client_frame(&mut tx, &ClientFrame::Hello { role: "cli" })
        .await
        .map_err(ConnectError::Transient)?;

    // Drain the initial `ready` (or error) from the server before starting.
    match tokio::time::timeout(Duration::from_secs(10), rx.next()).await {
        Ok(Some(Ok(Message::Text(txt)))) => {
            if let Ok(ServerFrame::Error { code, message }) =
                serde_json::from_str::<ServerFrame>(txt.as_str())
            {
                return Err(ConnectError::Fatal(anyhow!(
                    "server rejected hello: {code} — {message}"
                )));
            }
        }
        Ok(Some(Err(err))) => return Err(ConnectError::Transient(anyhow!("ws: {err}"))),
        Ok(None) => return Ok(LoopExit::Disconnected),
        Err(_) => return Err(ConnectError::Transient(anyhow!("hello timeout"))),
        _ => {}
    }

    // Publish agent_start.
    let start_event = PresenceEvent::Start {
        session_id: config.session_id.clone(),
        subject: config.subject.clone(),
        namespace: config.namespace.clone(),
        model: config.model.clone(),
        provider: config.provider.clone(),
        cursor_path: None,
        started_at_ms: now_millis(),
    };
    send_client_frame(
        &mut tx,
        &ClientFrame::Publish {
            event: &start_event,
        },
    )
    .await
    .map_err(ConnectError::Transient)?;
    info!(
        session = %config.session_id,
        namespace = %config.namespace,
        "presence: published agent_start"
    );

    let mut ticker = time::interval(config.interval);
    ticker.tick().await; // skip immediate first tick

    loop {
        // Check cancellation before each iteration — lets the Ctrl-C handler
        // interrupt even during an in-flight interval wait.
        if *cancelled.lock().await {
            let done_event = PresenceEvent::Done {
                session_id: config.session_id.clone(),
                subject: config.subject.clone(),
                namespace: config.namespace.clone(),
                ts_ms: now_millis(),
            };
            let _ = send_client_frame(&mut tx, &ClientFrame::Publish { event: &done_event }).await;
            let _ = tx
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "agent_done".into(),
                })))
                .await;
            return Ok(LoopExit::Cancelled);
        }

        select! {
            _ = ticker.tick() => {
                let heartbeat = PresenceEvent::Heartbeat {
                    session_id: config.session_id.clone(),
                    subject: config.subject.clone(),
                    namespace: config.namespace.clone(),
                    cursor_path: None,
                    recent_actions: Vec::new(),
                    ts_ms: now_millis(),
                };
                if let Err(err) = send_client_frame(&mut tx, &ClientFrame::Publish { event: &heartbeat }).await {
                    warn!(error = %err, "presence: heartbeat send failed; reconnecting");
                    return Ok(LoopExit::Disconnected);
                }
            }
            msg = rx.next() => {
                match msg {
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(ServerFrame::Error { code, message }) = serde_json::from_str::<ServerFrame>(txt.as_str()) {
                            warn!(code = %code, message = %message, "presence: server reported error");
                            match code.as_str() {
                                // Structural auth failure mid-stream — treat
                                // identically to a 401 on connect so the
                                // outer loop stops and the user gets the
                                // "re-run `heddle auth login`" hint.
                                "unauthorized" => return Err(ConnectError::Unauthorized),
                                "forbidden" => {
                                    return Err(ConnectError::Forbidden(anyhow!(
                                        "server forbade publish: {message}"
                                    )));
                                }
                                _ => return Ok(LoopExit::Disconnected),
                            }
                        }
                        // Event/Ready frames are benign for a publisher — ignore.
                    },
                    Some(Ok(Message::Ping(p))) => match tx.send(Message::Pong(p)).await {
                        Ok(()) => {}
                        Err(_) => return Ok(LoopExit::Disconnected),
                    },
                    Some(Ok(Message::Close(_))) | None => return Ok(LoopExit::Disconnected),
                    Some(Err(err)) => {
                        warn!(error = %err, "presence: ws recv error");
                        return Ok(LoopExit::Disconnected);
                    }
                    _ => {}
                }
            }
            // Wake periodically so Ctrl-C doesn't have to wait the full
            // interval before we check `cancelled` at the top of the loop.
            () = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

async fn send_client_frame<S>(tx: &mut S, frame: &ClientFrame<'_>) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures::Sink<Message>>::Error: std::fmt::Display,
{
    let payload = serde_json::to_string(frame).context("serialise client frame")?;
    tx.send(Message::Text(payload.into()))
        .await
        .map_err(|e| anyhow!("ws send: {e}"))?;
    Ok(())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct BackoffPlan {
    next: Duration,
}

impl BackoffPlan {
    fn new() -> Self {
        Self {
            next: Duration::from_secs(1),
        }
    }

    fn next(&mut self) -> Duration {
        let out = self.next;
        self.next = (self.next * 2).min(Duration::from_secs(30));
        out
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::store::AgentStatus;

    use super::*;

    fn make_agent(session_id: &str) -> AgentEntry {
        AgentEntry {
            session_id: session_id.into(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
            heddle_session_id: None,
            thread_id: None,
            thread: format!("agent/{session_id}"),
            pid: None,
            boot_id: None,
            liveness_path: None,
            heartbeat_at: None,
            anchor_state: None,
            anchor_root: None,
            reservation_token: None,
            path: None,
            base_state: "base".into(),
            started_at: Utc::now(),
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-6".into()),
            harness: None,
            thinking_level: None,
            usage_summary: Default::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: None,
            attach_precedence: vec![],
            winning_attach_rule: None,
            probe_source: None,
            probe_confidence: None,
            status: AgentStatus::Active,
            completed_at: None,
            context_queries: vec![],
        }
    }

    use cli_shared::config::UserPrincipalConfig;

    fn user_with_token_and_principal() -> UserConfig {
        let mut user = UserConfig::default();
        user.remote.token = Some("opaque-token".into());
        user.principal = Some(UserPrincipalConfig {
            name: "Alice".into(),
            email: "alice@example.com".into(),
        });
        user
    }

    #[test]
    fn skips_when_upstream_missing() {
        let hosted = HostedConfig {
            upstream_url: None,
            namespace: Some("heddle/core".into()),
        };
        let result = resolve_publisher_config(
            &hosted,
            &make_agent("agent-1"),
            &user_with_token_and_principal(),
            Duration::from_secs(15),
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn skips_when_namespace_missing() {
        let hosted = HostedConfig {
            upstream_url: Some("https://heddle.example.com".into()),
            namespace: None,
        };
        let result = resolve_publisher_config(
            &hosted,
            &make_agent("agent-1"),
            &user_with_token_and_principal(),
            Duration::from_secs(15),
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn resolves_subject_from_principal_when_token_is_opaque() {
        let hosted = HostedConfig {
            upstream_url: Some("https://heddle.example.com".into()),
            namespace: Some("heddle/core".into()),
        };
        let config = resolve_publisher_config(
            &hosted,
            &make_agent("agent-1"),
            &user_with_token_and_principal(),
            Duration::from_secs(15),
        )
        .unwrap()
        .expect("config should resolve");
        assert_eq!(config.subject, "alice@example.com");
        assert_eq!(config.namespace, "heddle/core");
        assert_eq!(config.ws_url, "wss://heddle.example.com/presence/ws");
    }

    #[test]
    fn env_token_skips_malformed_credential_store() {
        let _guard = crate::credentials::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var_os("HOME");
        let original_token = std::env::var_os("HEDDLE_REMOTE_TOKEN");
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::set_var("HEDDLE_REMOTE_TOKEN", "env-token");
        }
        std::fs::create_dir_all(temp.path().join(".heddle")).unwrap();
        std::fs::write(
            temp.path().join(".heddle/credentials.toml"),
            "this is not valid toml =",
        )
        .unwrap();

        let hosted = HostedConfig {
            upstream_url: Some("https://heddle.example.com".into()),
            namespace: Some("heddle/core".into()),
        };
        let config = resolve_publisher_config(
            &hosted,
            &make_agent("agent-1"),
            &UserConfig {
                remote: Default::default(),
                principal: user_with_token_and_principal().principal,
                ..Default::default()
            },
            Duration::from_secs(15),
        )
        .unwrap()
        .expect("config should resolve from env token");

        unsafe {
            if let Some(home) = original_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
            if let Some(token) = original_token {
                std::env::set_var("HEDDLE_REMOTE_TOKEN", token);
            } else {
                std::env::remove_var("HEDDLE_REMOTE_TOKEN");
            }
        }
        assert_eq!(config.token, "env-token");
    }

    #[test]
    fn normalises_https_upstream_to_wss() {
        assert_eq!(
            normalize_ws_url("https://heddle.example.com").unwrap(),
            "wss://heddle.example.com/presence/ws"
        );
        assert_eq!(
            normalize_ws_url("https://heddle.example.com/").unwrap(),
            "wss://heddle.example.com/presence/ws"
        );
        assert_eq!(
            normalize_ws_url("http://127.0.0.1:8421").unwrap(),
            "ws://127.0.0.1:8421/presence/ws"
        );
        assert_eq!(
            normalize_ws_url("ws://localhost:8421/any/path").unwrap(),
            "ws://localhost:8421/presence/ws"
        );
    }

    #[test]
    fn errors_on_missing_token() {
        // Isolate the env so an ambient `HEDDLE_REMOTE_TOKEN` (common in
        // dev shells that source a `.env` for the hosted services) can't
        // satisfy `user_config.remote_token()` and flip the error path
        // to "could not derive subject from principal config" instead of
        // the "no remote token available" message this test pins. The
        // sibling tests use the same lock + save/restore dance.
        let _guard = crate::credentials::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var_os("HOME");
        let original_token = std::env::var_os("HEDDLE_REMOTE_TOKEN");
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::remove_var("HEDDLE_REMOTE_TOKEN");
        }

        let hosted = HostedConfig {
            upstream_url: Some("https://heddle.example.com".into()),
            namespace: Some("heddle/core".into()),
        };
        let user = UserConfig::default();
        let err = resolve_publisher_config(
            &hosted,
            &make_agent("agent-1"),
            &user,
            Duration::from_secs(15),
        )
        .unwrap_err();
        let msg = format!("{err}");

        unsafe {
            if let Some(home) = original_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
            if let Some(token) = original_token {
                std::env::set_var("HEDDLE_REMOTE_TOKEN", token);
            } else {
                std::env::remove_var("HEDDLE_REMOTE_TOKEN");
            }
        }

        assert!(msg.contains("remote token"), "unexpected err: {msg}");
    }
}
