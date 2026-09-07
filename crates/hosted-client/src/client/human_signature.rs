// SPDX-License-Identifier: Apache-2.0
//! Headless human-signature (WebAuthn) callback for destructive hosted RPCs.
//!
//! When the server marks an RPC `human`-tier and rejects it with
//! `x-weft-sig-required: human`, the native hosted runtime's request-signing
//! interceptor invokes an app-registered callback to produce a WebAuthn
//! assertion over the action, then retries once (see
//! `crate::hosted_runtime::hosted::HumanSignatureCallback`).
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
//! ceremony. The typed error carries the action summary so the caller can show
//! what was blocked on its own presentation surface.
//!
//! Deferred (tracked for a follow-up): binding a platform authenticator via a
//! native WebAuthn crate (e.g. `webauthn-authenticator-rs`) so the CLI can
//! prompt for a security-key touch inline. When that lands, this callback swaps
//! its error branch for the real ceremony; the interceptor contract is
//! unchanged.

use std::sync::Arc;

use wire::ProtocolError;

use crate::hosted_runtime::hosted::{
    HumanSignatureCallback, HumanSignatureRequest, WebAuthnAssertion,
};

/// A quiet callback for clients without an attached WebAuthn authenticator.
///
/// The action summary and optional Tapestry link travel in the typed error so
/// the caller can render them on its own output channel. The Adapter never
/// fabricates an assertion.
pub fn headless_human_signature_callback() -> HumanSignatureCallback {
    Arc::new(
        |req: HumanSignatureRequest| -> Result<WebAuthnAssertion, ProtocolError> {
            match req.action_url.as_deref() {
                Some(url) => Err(ProtocolError::AuthorizationFailed(format!(
                    "user verification required for {} ({}): complete this action in Tapestry:\n  {}",
                    req.method_path, req.action_summary, url
                ))),
                None => Err(ProtocolError::AuthorizationFailed(format!(
                    "user verification required for {} ({}): use a client with a WebAuthn authenticator",
                    req.method_path, req.action_summary
                ))),
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with_action_url(action_url: Option<String>) -> HumanSignatureRequest {
        HumanSignatureRequest {
            method_path: "/heddle.api.v1alpha1.RegistryService/DeleteSpool".to_string(),
            action_summary: "Authorize /heddle.api.v1alpha1.RegistryService/DeleteSpool"
                .to_string(),
            challenge: "abc".to_string(),
            canonical: b"heddle-req-sig-v1:...".to_vec(),
            action_url,
        }
    }

    /// Without a server deep-link, the callback keeps the generic guidance and still returns a
    /// typed error, never an assertion.
    #[test]
    fn headless_callback_returns_typed_error_and_never_fakes_an_assertion() {
        let cb = headless_human_signature_callback();
        let result = cb(req_with_action_url(None));
        match result {
            Err(ProtocolError::AuthorizationFailed(msg)) => {
                assert!(msg.contains("user verification required"));
                assert!(msg.contains("DeleteSpool"));
                // No URL was provided → generic guidance, no link.
                assert!(msg.contains("WebAuthn authenticator"));
                assert!(!msg.contains("https://"));
            }
            other => panic!("expected a typed AuthorizationFailed error, got {other:?}"),
        }
    }

    /// With a server deep-link (weft#338), the typed error message includes the URL so the user
    /// can open it — and the callback still returns a typed error, never an assertion.
    #[test]
    fn headless_callback_includes_action_url_in_typed_error_when_present() {
        let cb = headless_human_signature_callback();
        let url = "https://app.heddle.sh/verify-action?method=%2Fheddle.api.v1alpha1.RegistryService%2FDeleteSpool&challenge=CHAL";
        let result = cb(req_with_action_url(Some(url.to_string())));
        match result {
            Err(ProtocolError::AuthorizationFailed(msg)) => {
                assert!(msg.contains("user verification required"));
                assert!(msg.contains("DeleteSpool"));
                assert!(
                    msg.contains(url),
                    "message must carry the deep-link URL: {msg}"
                );
                assert!(msg.contains("Tapestry"));
            }
            other => panic!("expected a typed AuthorizationFailed error, got {other:?}"),
        }
    }
}
