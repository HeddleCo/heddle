// SPDX-License-Identifier: Apache-2.0
//! Hosted review-signature sync bridge.
//!
//! `heddle review sign` records a `ReviewSignatures` state-attachment LOCALLY.
//! weft#549 rejects a client-pushed attachment in the pack, so a signature only
//! reaches the hosted server through the caller-authenticated, PoP-signed
//! `StateReviewService::SignState` RPC (which binds the signing key to the
//! authenticated caller). This module replays our local signatures over that
//! RPC after a successful `heddle push`, mirroring [`crate::client::discussion_sync`]:
//!
//! * **Push (write path):** for the pushed state(s), forward each review
//!   signature WE authored to the hosted `SignState`. The signature bytes were
//!   computed over the deterministic [`objects::object::state_review::signing_payload`],
//!   byte-identical to the server's reconstruction, so the exact signature the
//!   local `review sign` wrote verifies unchanged server-side. weft relaxes the
//!   `signed_at` skew gate for this authenticated install path, so a signature
//!   minted long before the push still lands.
//! * **Pull (read path):** none needed — the server-minted `ReviewSignatures`
//!   attachment rides the pull pack like any server-owned attachment, so a clone
//!   / pull materializes it and the local `review show` reads it directly.
//!
//! ## Fail-closed self filter
//!
//! Only signatures whose actor is the local principal are forwarded. The actor
//! is resolved from [`Repository::get_principal`] (env → config → git) — the SAME
//! source `review sign` stamped the `actor` with — NOT `config().principal`
//! alone, which is empty in git-overlay / env-identity repos and would silently
//! drop our own signatures. When the principal is unresolvable we warn and skip
//! (we cannot tell which signatures are ours).
//!
//! ## Retry discipline
//!
//! The mirror (`.heddle/collaboration/hosted-review-mirror.json`) records both
//! `synced` and permanently-`rejected` `(state, signature)` pairs. A transient
//! failure (network / server unavailable / state not yet on the server) is left
//! for the next push to retry; a permanent rejection (bad signature, key not
//! owned by the caller) is recorded so it stops retrying and warning every push.

#![cfg(feature = "client")]

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use api::heddle::api::v1alpha1::{
    PathSymbolRef, ReviewKind as ProtoReviewKind, ReviewScope as ProtoReviewScope, review_scope,
};
use objects::fs_atomic::write_file_atomic;
use objects::object::{
    ReviewKind, ReviewScope, ReviewSignature, ReviewSignaturesBlob, StateAttachmentBody, StateId,
};
use objects::store::ObjectStore;
use repo::{HistoryQuery, Repository, StateAttachmentKind};
use serde::{Deserialize, Serialize};
use wire::ProtocolError;

use crate::client::HostedGrpcClient;

/// How far back from HEAD to scan for locally-recorded review signatures.
const REVIEW_SCAN_LIMIT: usize = 50;

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostedReviewMirror {
    #[serde(default)]
    repos: BTreeMap<String, RepoReviewMirror>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RepoReviewMirror {
    /// `(state_id, signature-hex)` pairs successfully installed on the server.
    #[serde(default)]
    synced: Vec<String>,
    /// Pairs the server permanently rejected — do not retry (avoids warning
    /// every push over a signature that will never install).
    #[serde(default)]
    rejected: Vec<String>,
}

fn synced_key(state_id: &StateId, signature_hex: &str) -> String {
    format!("{}#{signature_hex}", state_id.to_string_full())
}

fn mirror_path(heddle_dir: &Path) -> PathBuf {
    heddle_dir
        .join("collaboration")
        .join("hosted-review-mirror.json")
}

fn load_mirror(heddle_dir: &Path) -> Result<HostedReviewMirror> {
    match fs::read(mirror_path(heddle_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("decode hosted review mirror map"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(HostedReviewMirror::default())
        }
        Err(error) => Err(error).context("read hosted review mirror map"),
    }
}

fn save_mirror(heddle_dir: &Path, mirror: &HostedReviewMirror) -> Result<()> {
    let path = mirror_path(heddle_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create collaboration dir")?;
    }
    let bytes = serde_json::to_vec_pretty(mirror).context("encode hosted review mirror map")?;
    write_file_atomic(&path, &bytes).context("write hosted review mirror map")?;
    Ok(())
}

fn kind_to_proto(kind: ReviewKind) -> ProtoReviewKind {
    match kind {
        ReviewKind::Read => ProtoReviewKind::Read,
        ReviewKind::AgentPreview => ProtoReviewKind::AgentPreview,
        ReviewKind::AgentCoReview => ProtoReviewKind::AgentCoReview,
    }
}

fn scope_to_proto(scope: &ReviewScope) -> ProtoReviewScope {
    let inner = match scope {
        ReviewScope::WholeChange => review_scope::Scope::WholeChange(review_scope::WholeChange {}),
        ReviewScope::Symbols(symbols) => review_scope::Scope::Symbols(review_scope::SymbolList {
            symbols: symbols
                .iter()
                .map(|anchor| PathSymbolRef {
                    file: anchor.file.clone(),
                    symbol: anchor.symbol.clone(),
                })
                .collect(),
        }),
    };
    ProtoReviewScope { scope: Some(inner) }
}

/// Whether a hosted rejection is permanent (won't succeed on retry) vs transient
/// (retry next push). A malformed/invalid signature or a key the caller does not
/// own will never install; a network error or a state not yet on the server may.
fn is_permanent(error: &ProtocolError) -> bool {
    matches!(
        error,
        ProtocolError::InvalidState(_) | ProtocolError::AuthorizationFailed(_)
    )
}

enum ForwardOutcome {
    Installed,
    Permanent(String),
    Transient(String),
}

/// Read the current `ReviewSignatures` blob for a state, if any.
fn read_signatures(repo: &Repository, state_id: &StateId) -> Result<Vec<ReviewSignature>> {
    let Some(attachment) =
        repo.latest_state_attachment(state_id, StateAttachmentKind::ReviewSignatures)?
    else {
        return Ok(Vec::new());
    };
    let StateAttachmentBody::ReviewSignatures(hash) = attachment.body else {
        return Ok(Vec::new());
    };
    let Some(blob) = repo.store().get_blob(&hash)? else {
        return Ok(Vec::new());
    };
    let decoded = ReviewSignaturesBlob::decode(blob.content())
        .map_err(|error| anyhow::anyhow!("decode review signatures blob: {error}"))?;
    Ok(decoded.signatures)
}

/// Replay local review signatures we authored to the hosted `StateReviewService`.
pub async fn push_review_signatures(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<usize> {
    let Some(head) = repo.head().context("resolve repository head")? else {
        return Ok(0);
    };

    // Resolve our identity from the SAME source `review sign` stamped the actor
    // with (env → config → git). Warn + skip if unresolvable — we cannot tell
    // which signatures are ours.
    let principal = match repo.get_principal() {
        Ok(principal) => principal,
        Err(error) => {
            eprintln!(
                "{} review sync skipped: could not resolve the local principal ({error}); \
                 set one with `heddle init --principal-name <name> --principal-email <email>`",
                crate::cli::style::warn_marker(),
            );
            return Ok(0);
        }
    };

    let states = repo
        .query_history(&HistoryQuery::new(Some(head)).with_limit(REVIEW_SCAN_LIMIT))
        .context("walk history for review signatures")?;

    let heddle_dir = repo.heddle_dir().to_path_buf();
    let mut mirror = load_mirror(&heddle_dir)?;
    let skip: HashSet<String> = mirror
        .repos
        .get(repo_path)
        .map(|repo_mirror| {
            repo_mirror
                .synced
                .iter()
                .chain(repo_mirror.rejected.iter())
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    let mut synced = 0usize;
    for state in states {
        let signatures = match read_signatures(repo, &state.state_id) {
            Ok(signatures) => signatures,
            Err(error) => {
                eprintln!(
                    "{} hosted review {}: {error:#}",
                    crate::cli::style::warn_marker(),
                    state.state_id.short()
                );
                continue;
            }
        };
        for signature in signatures {
            if signature.actor.name != principal.name || signature.actor.email != principal.email {
                continue;
            }
            let key = synced_key(&state.state_id, &signature.signature);
            if skip.contains(&key) {
                continue;
            }
            match forward_signature(client, repo_path, &state.state_id, &signature).await {
                ForwardOutcome::Installed => {
                    mirror
                        .repos
                        .entry(repo_path.to_string())
                        .or_default()
                        .synced
                        .push(key);
                    save_mirror(&heddle_dir, &mirror)?;
                    synced += 1;
                }
                ForwardOutcome::Permanent(message) => {
                    // Record so we stop retrying + warning on every push.
                    mirror
                        .repos
                        .entry(repo_path.to_string())
                        .or_default()
                        .rejected
                        .push(key);
                    save_mirror(&heddle_dir, &mirror)?;
                    eprintln!(
                        "{} hosted review {}: permanently rejected, will not retry: {message}",
                        crate::cli::style::warn_marker(),
                        state.state_id.short()
                    );
                }
                ForwardOutcome::Transient(message) => {
                    eprintln!(
                        "{} hosted review {}: {message} (will retry on next push)",
                        crate::cli::style::warn_marker(),
                        state.state_id.short()
                    );
                }
            }
        }
    }
    Ok(synced)
}

async fn forward_signature(
    client: &mut HostedGrpcClient,
    repo_path: &str,
    state_id: &StateId,
    signature: &ReviewSignature,
) -> ForwardOutcome {
    // A malformed stored signature will never install → permanent.
    let public_key = match hex::decode(&signature.public_key) {
        Ok(bytes) => bytes,
        Err(error) => return ForwardOutcome::Permanent(format!("public_key is not hex: {error}")),
    };
    let signature_bytes = match hex::decode(&signature.signature) {
        Ok(bytes) => bytes,
        Err(error) => return ForwardOutcome::Permanent(format!("signature is not hex: {error}")),
    };
    match client
        .sign_state(
            repo_path,
            state_id,
            kind_to_proto(signature.kind),
            scope_to_proto(&signature.scope),
            signature.justification.as_deref().unwrap_or_default(),
            &signature.algorithm,
            public_key,
            signature_bytes,
            signature.signed_at,
            sign_op_id(repo_path, state_id, &signature.signature),
        )
        .await
    {
        // Idempotent success or an already-installed signature both mean "on the
        // server".
        Ok(_) | Err(ProtocolError::AlreadyExists(_)) => ForwardOutcome::Installed,
        Err(error) if is_permanent(&error) => ForwardOutcome::Permanent(error.to_string()),
        Err(error) => ForwardOutcome::Transient(error.to_string()),
    }
}

const OP_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6865_6464_6c65_7276_775f_7379_6e63_0001);

fn sign_op_id(repo_path: &str, state_id: &StateId, signature_hex: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!(
            "sign:{repo_path}:{}:{signature_hex}",
            state_id.to_string_full()
        )
        .as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use objects::object::SymbolAnchor;

    use super::*;

    #[test]
    fn whole_change_scope_maps_to_proto() {
        let proto = scope_to_proto(&ReviewScope::WholeChange);
        assert!(matches!(
            proto.scope,
            Some(review_scope::Scope::WholeChange(_))
        ));
    }

    #[test]
    fn symbol_scope_maps_to_proto() {
        let proto = scope_to_proto(&ReviewScope::Symbols(vec![SymbolAnchor::new("a.rs", "foo")]));
        match proto.scope {
            Some(review_scope::Scope::Symbols(list)) => {
                assert_eq!(list.symbols.len(), 1);
                assert_eq!(list.symbols[0].file, "a.rs");
                assert_eq!(list.symbols[0].symbol, "foo");
            }
            other => panic!("expected symbols scope, got {other:?}"),
        }
    }

    #[test]
    fn synced_key_is_state_scoped() {
        let a = StateId::from_bytes([1; 32]);
        let b = StateId::from_bytes([2; 32]);
        assert_ne!(synced_key(&a, "abad1dea"), synced_key(&b, "abad1dea"));
        assert_eq!(synced_key(&a, "abad1dea"), synced_key(&a, "abad1dea"));
    }

    #[test]
    fn kind_maps_to_proto() {
        assert_eq!(kind_to_proto(ReviewKind::Read), ProtoReviewKind::Read);
        assert_eq!(
            kind_to_proto(ReviewKind::AgentPreview),
            ProtoReviewKind::AgentPreview
        );
        assert_eq!(
            kind_to_proto(ReviewKind::AgentCoReview),
            ProtoReviewKind::AgentCoReview
        );
    }

    // A bad-signature / key-not-owned rejection is permanent (stops retrying);
    // a network / not-yet-pushed error is transient (retries next push).
    #[test]
    fn permanent_vs_transient_classification() {
        assert!(is_permanent(&ProtocolError::InvalidState("bad sig".into())));
        assert!(is_permanent(&ProtocolError::AuthorizationFailed(
            "key not owned".into()
        )));
        assert!(!is_permanent(&ProtocolError::ObjectNotFound(
            "state not on server yet".into()
        )));
        assert!(!is_permanent(&ProtocolError::Remote("unavailable".into())));
    }
}
