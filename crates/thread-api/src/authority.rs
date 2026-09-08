// SPDX-License-Identifier: Apache-2.0
//! The same client-minted Biscuit rules at device and hosted boundaries. The
//! application supplies roots already attached to the owner/account and a
//! resolved spool path. No key or authority is minted by this verifier.
use std::{
    sync::RwLock,
    time::{SystemTime, UNIX_EPOCH},
};

use api::{
    heddle::api::v1alpha1::{CallContext, RequestProof},
    v2::MethodDescriptor,
};
use biscuit_verifier::{BiscuitFacts, PublicKey};
use chrono::{DateTime, Utc};
use crypto::{Ed25519Signer, Signer};

use crate::transport::{Authorize, Error};

pub struct RootAuthority {
    roots: RwLock<Vec<PublicKey>>,
    spool_path: String,
}
impl RootAuthority {
    pub fn new(roots: Vec<PublicKey>, spool_path: String) -> Result<Self, Error> {
        if roots.is_empty() || spool_path.is_empty() {
            return Err(Error::Protocol(
                "root attachment and resolved spool path required",
            ));
        }
        Ok(Self {
            roots: RwLock::new(roots),
            spool_path,
        })
    }
    /// Called once per opening; nonce claims remain durable across restarts.
    pub fn verify(
        &self,
        context: &CallContext,
        method: &'static MethodDescriptor,
        body: &[u8],
        right: &str,
        registry: &repo::thread_replication::ThreadReplica,
    ) -> Result<VerifiedCall, Error> {
        let now = Utc::now();
        let facts = self.check(context, method, right, now)?;
        let proof = context
            .request_proof
            .as_ref()
            .ok_or(Error::Protocol("request PoP required"))?;
        if proof.algorithm != "ed25519"
            || proof.nonce.len() != 16
            || (now
                .timestamp_millis()
                .saturating_sub(proof.timestamp_millis))
            .unsigned_abs()
                > 60_000
        {
            return Err(Error::Protocol("invalid or expired request PoP"));
        }
        let cnf = facts
            .cnf
            .as_deref()
            .ok_or(Error::Protocol("Biscuit must bind a request signing key"))?;
        let key = hex::decode(cnf).map_err(|_| Error::Protocol("invalid Biscuit proof key"))?;
        let identity = format!("principal:device-key:{cnf}");
        if proof.signing_identity != identity {
            return Err(Error::Protocol(
                "request signing identity differs from Biscuit",
            ));
        }
        Ed25519Signer::verify_with_public_key(
            &api::signing::unary_bytes(
                &identity,
                method.path,
                proof.timestamp_millis,
                &proof.nonce,
                body,
            ),
            &key,
            &proof.signature,
        )
        .map_err(|_| Error::Protocol("invalid request signature"))?;
        if !registry
            .claim_request_nonce(&identity, &proof.nonce, now.timestamp_millis())
            .map_err(|e| Error::Io(e.to_string()))?
        {
            return Err(Error::Protocol("request nonce already consumed"));
        }
        Ok(VerifiedCall {
            context: context.clone(),
            method,
            right: right.into(),
            principal: facts.sub,
        })
    }
    fn check(
        &self,
        context: &CallContext,
        method: &'static MethodDescriptor,
        right: &str,
        now: DateTime<Utc>,
    ) -> Result<BiscuitFacts, Error> {
        if context
            .deadline
            .as_ref()
            .is_some_and(|deadline| deadline.seconds <= now.timestamp())
        {
            return Err(Error::Protocol("request deadline expired"));
        }
        let bearer = std::str::from_utf8(&context.bearer_capability)
            .map_err(|_| Error::Protocol("invalid Biscuit encoding"))?;
        let envelope = if context.bearer_grant_envelope.is_empty() {
            None
        } else {
            Some(
                std::str::from_utf8(&context.bearer_grant_envelope)
                    .map_err(|_| Error::Protocol("invalid grant envelope encoding"))?,
            )
        };
        let operation = method
            .path
            .rsplit('/')
            .next()
            .ok_or(Error::Protocol("invalid method path"))?;
        let facts = biscuit_verifier::verify_any_at_with_resource(
            bearer,
            envelope,
            &self
                .roots
                .read()
                .map_err(|_| Error::Protocol("root registry unavailable"))?,
            &[],
            operation,
            Some(("spool", &self.spool_path)),
            now,
        )
        .map_err(|_| Error::Protocol("Biscuit does not authorize this operation"))?;
        if !facts.has_right("spool", &self.spool_path, right) {
            return Err(Error::Protocol("Biscuit does not grant this spool right"));
        }
        Ok(facts)
    }
    pub fn recheck(&self, call: &VerifiedCall) -> Result<(), Error> {
        self.check(&call.context, call.method, &call.right, Utc::now())
            .map(|_| ())
    }

    /// Replace the application's current root attachments. An empty set revokes
    /// all ongoing calls as well as future openings at the next recheck.
    pub fn replace_roots(&self, roots: Vec<PublicKey>) -> Result<(), Error> {
        *self
            .roots
            .write()
            .map_err(|_| Error::Protocol("root registry unavailable"))? = roots;
        Ok(())
    }
}

#[derive(Clone)]
pub struct VerifiedCall {
    context: CallContext,
    method: &'static MethodDescriptor,
    right: String,
    pub principal: String,
}

/// A caller-owned credential and signer. Browser/device-root attachment is
/// supplied by the application, exactly as it is for a Weft request.
pub struct CredentialSigner {
    pub signer: Ed25519Signer,
    pub bearer: String,
    pub grant_envelope: Vec<u8>,
}
impl Authorize for CredentialSigner {
    async fn context(
        &self,
        method: &'static MethodDescriptor,
        body: &[u8],
    ) -> Result<CallContext, Error> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::Io(e.to_string()))?
            .as_millis();
        let timestamp = i64::try_from(timestamp).map_err(|_| Error::Protocol("invalid clock"))?;
        let identity = format!(
            "principal:device-key:{}",
            hex::encode(self.signer.public_key())
        );
        // UUID v4 is an OS-random nonce; there is no authority generation here.
        let nonce = uuid::Uuid::new_v4().as_bytes().to_vec();
        let signature = self
            .signer
            .sign(&api::signing::unary_bytes(
                &identity,
                method.path,
                timestamp,
                &nonce,
                body,
            ))
            .map_err(|e| Error::Io(e.to_string()))?;
        Ok(CallContext {
            bearer_capability: self.bearer.as_bytes().to_vec(),
            bearer_grant_envelope: self.grant_envelope.clone(),
            request_proof: Some(RequestProof {
                algorithm: "ed25519".into(),
                signing_identity: identity,
                timestamp_millis: timestamp,
                nonce,
                signature,
            }),
            ..Default::default()
        })
    }
}
