// SPDX-License-Identifier: Apache-2.0
//! CLI default human-signature (WebAuthn) callback for destructive hosted RPCs.
//!
//! When the server marks an RPC `human`-tier and rejects it with
//! `x-weft-sig-required: human`, `heddle-client`'s request-signing interceptor
//! invokes an app-registered callback to produce a WebAuthn assertion over the
//! action, then retries once (see `heddle_client::HumanSignatureCallback`).
//!
//! # What the CLI supports vs defers
//!
//! A full WebAuthn ceremony needs a platform/roaming authenticator (touch,
//! biometric, or security key) driven by an OS/browser WebAuthn stack. The
//! `heddle` CLI runs headless in a terminal and has **no** in-process WebAuthn
//! authenticator binding today, so it cannot mint a genuine assertion — and we
//! must never fake one (a forged assertion would either be rejected by the
//! server's UV check or, worse, defeat the entire human-gesture control).
//!
//! Therefore the CLI's default callback **surfaces a clear, typed
//! user-verification-required error** naming a surface that can complete the
//! ceremony (the web UI / tapestry), rather than attempting a partial/fake
//! ceremony. The consent surface (the action summary) is still shown to the
//! user before the error so they understand what was blocked.
//!
//! Deferred (tracked for a follow-up): binding a platform authenticator via a
//! native WebAuthn crate (e.g. `webauthn-authenticator-rs`) so the CLI can
//! prompt for a security-key touch inline. When that lands, this callback swaps
//! its error branch for the real ceremony; the interceptor contract is
//! unchanged.

use heddle_client::{HumanSignatureCallback, HumanSignatureRequest, WebAuthnAssertion};
use std::sync::Arc;
use wire::ProtocolError;

/// The default human-signature callback for CLI-opened hosted sessions.
///
/// Renders the action being authorized (so the user sees *what* required
/// verification), then returns a typed error directing the user to a surface
/// that can complete the WebAuthn ceremony. It never fabricates an assertion.
pub fn cli_human_signature_callback() -> HumanSignatureCallback {
    Arc::new(|req: HumanSignatureRequest| -> Result<WebAuthnAssertion, ProtocolError> {
        // Show the consent surface: the user should always learn which action
        // was gated, even though the CLI can't complete the gesture itself.
        eprintln!(
            "⚠ This action requires user verification (WebAuthn), which the CLI can't perform in \
             a headless terminal:\n  {}",
            req.action_summary
        );
        // When the server sent a deep-link (weft#338), point the user straight at the surface
        // that CAN complete the ceremony; otherwise fall back to generic guidance. Either way
        // we return a typed error and NEVER fabricate an assertion.
        match req.action_url.as_deref() {
            Some(url) => {
                eprintln!("Complete it in the web app:\n  {url}");
                Err(ProtocolError::AuthorizationFailed(format!(
                    "user verification required for {}: complete this action in the web app:\n  {}",
                    req.method_path, url
                )))
            }
            None => {
                eprintln!(
                    "The `heddle` CLI cannot perform the WebAuthn ceremony in a headless terminal."
                );
                Err(ProtocolError::AuthorizationFailed(format!(
                    "user verification required for {}: run this destructive action from a surface \
                     with a WebAuthn authenticator (the web UI), or re-run once CLI authenticator \
                     support lands",
                    req.method_path
                )))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with_action_url(action_url: Option<String>) -> HumanSignatureRequest {
        HumanSignatureRequest {
            method_path: "/heddle.v1.HostedUserService/DeleteRepository".to_string(),
            action_summary: "Authorize /heddle.v1.HostedUserService/DeleteRepository".to_string(),
            challenge: "abc".to_string(),
            canonical: b"weft-req-sig-v1:...".to_vec(),
            action_url,
        }
    }

    /// Without a server deep-link, the callback keeps the generic guidance and still returns a
    /// typed error, never an assertion.
    #[test]
    fn cli_callback_returns_typed_error_and_never_fakes_an_assertion() {
        let cb = cli_human_signature_callback();
        let result = cb(req_with_action_url(None));
        match result {
            Err(ProtocolError::AuthorizationFailed(msg)) => {
                assert!(msg.contains("user verification required"));
                assert!(msg.contains("DeleteRepository"));
                // No URL was provided → generic guidance, no link.
                assert!(msg.contains("web UI"));
                assert!(!msg.contains("https://"));
            }
            other => panic!("expected a typed AuthorizationFailed error, got {other:?}"),
        }
    }

    /// With a server deep-link (weft#338), the typed error message includes the URL so the user
    /// can open it — and the callback still returns a typed error, never an assertion.
    #[test]
    fn cli_callback_includes_action_url_in_typed_error_when_present() {
        let cb = cli_human_signature_callback();
        let url = "https://app.heddle.sh/verify-action?method=%2Fheddle.v1.HostedUserService%2FDeleteRepository&challenge=CHAL";
        let result = cb(req_with_action_url(Some(url.to_string())));
        match result {
            Err(ProtocolError::AuthorizationFailed(msg)) => {
                assert!(msg.contains("user verification required"));
                assert!(msg.contains("DeleteRepository"));
                assert!(msg.contains(url), "message must carry the deep-link URL: {msg}");
                assert!(msg.contains("web app"));
            }
            other => panic!("expected a typed AuthorizationFailed error, got {other:?}"),
        }
    }
}
