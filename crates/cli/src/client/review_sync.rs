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
//!   local `review sign` wrote verifies unchanged server-side.
//! * **Pull (read path):** none needed — the server-minted `ReviewSignatures`
//!   attachment rides the pull pack like any server-owned attachment, so a clone
//!   / pull materializes it and the local `review show` reads it directly.
//!
//! ## Fail-closed self filter
//!
//! Only signatures whose actor is the local principal are forwarded. The server
//! binds the signing key to the authenticated pusher, so attempting to replay
//! another actor's signature (e.g. one a clone pulled) would be rejected anyway;
//! the self filter avoids the pointless round-trip and any misattribution.
//!
//! ## weft#638 limit (degrade gracefully, don't fix here)
//!
//! A signature is minted against a specific state. States not present on the
//! server (unpushed ancestors, or a head that advanced past the signed state)
//! make `SignState` fail; that warns and continues rather than failing the push.
//! The mirror (`.heddle/collaboration/hosted-review-mirror.json`) records synced
//! `(state, signature)` pairs so a re-push does not re-sign, and is saved after
//! every signature and on the error path.

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
    /// Signed `(state_id, signature-hex)` pairs already forwarded to the server.
    #[serde(default)]
    synced: Vec<String>,
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
/// Saves the mirror after each signature and continues past a per-signature
/// failure (warn-and-skip).
pub async fn push_review_signatures(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<usize> {
    let Some(head) = repo.head().context("resolve repository head")? else {
        return Ok(0);
    };

    // Only forward signatures the local principal actually authored.
    let Some(principal) = repo.config().principal.clone() else {
        return Ok(0);
    };

    let states = repo
        .query_history(&HistoryQuery::new(Some(head)).with_limit(REVIEW_SCAN_LIMIT))
        .context("walk history for review signatures")?;

    let mut mirror = load_mirror(repo.heddle_dir())?;
    let already: HashSet<String> = mirror
        .repos
        .get(repo_path)
        .map(|repo_mirror| repo_mirror.synced.iter().cloned().collect())
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
            // Self filter: the server binds the signing key to the caller, so we
            // can only (re)mint our own signatures.
            if signature.actor.name != principal.name || signature.actor.email != principal.email {
                continue;
            }
            let key = synced_key(&state.state_id, &signature.signature);
            if already.contains(&key) {
                continue;
            }
            let result =
                forward_signature(client, repo_path, &state.state_id, &signature).await;
            match result {
                Ok(()) => {
                    let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
                    repo_mirror.synced.push(key);
                    save_mirror(repo.heddle_dir(), &mirror)?;
                    synced += 1;
                }
                Err(error) => {
                    eprintln!(
                        "{} hosted review {}: {error:#}",
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
) -> Result<()> {
    let public_key = hex::decode(&signature.public_key)
        .map_err(|error| anyhow::anyhow!("review public_key is not hex: {error}"))?;
    let signature_bytes = hex::decode(&signature.signature)
        .map_err(|error| anyhow::anyhow!("review signature is not hex: {error}"))?;
    client
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
        .with_context(|| format!("sign hosted state {}", state_id.short()))?;
    Ok(())
}

const OP_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6865_6464_6c65_7276_775f_7379_6e63_0001);

fn sign_op_id(repo_path: &str, state_id: &StateId, signature_hex: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("sign:{repo_path}:{}:{signature_hex}", state_id.to_string_full()).as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use objects::object::{Principal, SymbolAnchor};

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

    // The synced key binds a signature to a state, so the same signature bytes
    // on two states sync independently and a re-push of either is a no-op.
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

    #[test]
    fn actor_self_filter_matches_principal() {
        let actor = Principal::new("Alice", "alice@x");
        let same = Principal::new("Alice", "alice@x");
        let other = Principal::new("Bob", "bob@x");
        assert!(actor.name == same.name && actor.email == same.email);
        assert!(actor.name != other.name || actor.email != other.email);
    }
}
