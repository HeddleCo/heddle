// SPDX-License-Identifier: Apache-2.0
//! The same client-minted Biscuit rules at device and hosted boundaries. The
//! application supplies roots already attached to the owner/account and a
//! resolved spool path. No key or authority is minted by this verifier.
use std::sync::RwLock;

use api::{heddle::api::v1alpha1::CallContext, v2::MethodDescriptor};
use base64::Engine as _;
use biscuit_verifier::{BiscuitFacts, PublicKey};
use chrono::{DateTime, Utc};

use crate::transport::Error;

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
        let cnf = facts
            .cnf
            .as_deref()
            .ok_or(Error::Protocol("Biscuit must bind a request signing key"))?;
        let mut key = [0; 32];
        hex::decode_to_slice(cnf, &mut key)
            .map_err(|_| Error::Protocol("invalid Biscuit proof key"))?;
        let proof =
            crate::request_proof::verify(context, method, body, &key, now.timestamp_millis())?;
        if !registry
            .claim_request_nonce(proof.identity(), proof.nonce(), now.timestamp_millis())
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
        // The RPC carries raw Biscuit bytes. Text is confined to the verifier's
        // existing storage/configuration API; it is never a second wire form.
        let bearer = base64::engine::general_purpose::URL_SAFE.encode(&context.bearer_capability);
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
            &bearer,
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
