//! Account admission from locally pinned authority; enumeration checks each real
//! resource path. Neither scoped tokens nor empty accounts need a fake Spool.
use std::path::Path;

use anyhow::{Context, Result, bail};
use api::{heddle::api::v1alpha1::CallContext, v2::MethodDescriptor};
use biscuit_verifier::{BiscuitFacts, InspectedCredential, PublicKey};
use chrono::Utc;

use super::stream::ObservationAuthority;

pub(super) struct AccountSession {
    pub principal: String,
    pub publisher: [u8; 32],
    pub inspected: InspectedCredential,
    pub token: biscuit_auth::Biscuit,
    pub root: PublicKey,
    authority_proof: Vec<u8>,
    pub expires: i64,
    pub method: &'static MethodDescriptor,
    admission: (String, Vec<u8>, i64),
    account_wide: bool,
    scopes: Vec<String>,
}
impl AccountSession {
    pub fn finish_admission(&self, home: &Path) -> Result<()> {
        self.check_current(home)?;
        if !repo::device_catalog::claim_nonce(
            home,
            &self.admission.0,
            &self.admission.1,
            self.admission.2,
        )? {
            bail!("request proof already used");
        }
        Ok(())
    }
    pub fn facts(&self, path: Option<&str>) -> Result<BiscuitFacts> {
        Ok(biscuit_verifier::authorize_at(
            &self.token,
            self.method
                .path
                .rsplit('/')
                .next()
                .context("invalid method")?,
            Utc::now(),
            None,
            &[],
            path.map(|path| ("spool", path)),
        )?)
    }
    /// Bind only authorized paths from a bounded current catalog projection.
    pub fn bind_scopes(&mut self, paths: impl IntoIterator<Item = String>) -> Result<()> {
        self.account_wide = self.facts(None).is_ok();
        self.scopes.clear();
        let mut bytes = 0usize;
        for path in paths {
            if path.is_empty() || path.len() > 4096 {
                bail!("invalid catalog capability path");
            }
            if self.facts(Some(&path)).is_ok() {
                bytes = bytes
                    .checked_add(path.len())
                    .context("scope bytes overflow")?;
                if self.scopes.len() >= 4096 || bytes > 1024 * 1024 {
                    bail!("account scope exceeds view budget");
                }
                self.scopes.push(path);
            }
        }
        if !self.account_wide && self.scopes.is_empty() {
            bail!("credential does not authorize this account view");
        }
        self.check_clock()
    }
    pub fn view_facts(&self) -> Result<BiscuitFacts> {
        if self.account_wide {
            self.facts(None)
        } else {
            self.facts(Some(self.scopes.first().context("account scope missing")?))
        }
    }
    pub fn permits_method(&self, method: &str, path: &str) -> bool {
        method.rsplit('/').next().is_some_and(|name| {
            biscuit_verifier::authorize_at(
                &self.token,
                name,
                Utc::now(),
                None,
                &[],
                Some(("spool", path)),
            )
            .is_ok()
        })
    }
    pub fn permits(&self, path: &str) -> bool {
        self.facts(Some(path)).is_ok()
    }
    pub fn owner(&self, home: &Path) -> Result<repo::device_authority::DeviceAuthority> {
        let now = Utc::now().timestamp();
        let authority = repo::device_authority::load(home, now)?;
        authority.verify_presented_authority(
            &self.root.to_bytes(),
            &self.token,
            &self.authority_proof,
            now,
        )?;
        authority.verify_publisher(&self.publisher)?;
        if self
            .inspected
            .revocation_ids
            .iter()
            .any(|id| authority.revoked_ids.contains(id))
        {
            bail!("device capability revoked");
        }
        let current = repo::verify_account_owner_observation(&authority.owner, now)?;
        let account = current
            .signed_root()
            .root
            .as_ref()
            .context("account root missing")?;
        if uuid::Uuid::from_slice(&account.account_uuid)?.to_string() != self.principal {
            bail!("device account changed");
        }
        Ok(authority)
    }
}
impl ObservationAuthority for AccountSession {
    fn binding(&self) -> Vec<u8> {
        [self.principal.as_bytes(), self.publisher.as_slice()].concat()
    }
    fn expires(&self) -> i64 {
        self.expires
    }
    fn check_clock(&self) -> Result<()> {
        if self.expires != 0 && Utc::now().timestamp() >= self.expires {
            bail!("device capability expired");
        }
        if self.account_wide {
            self.facts(None)?;
        }
        if !self.account_wide && self.scopes.is_empty() {
            bail!("account scope missing");
        }
        for path in &self.scopes {
            self.facts(Some(path))?;
        }
        Ok(())
    }
    fn check_current(&self, home: &Path) -> Result<()> {
        self.check_clock()?;
        self.owner(home)?;
        Ok(())
    }
}
pub(super) fn authorize(
    home: &Path,
    method: &'static MethodDescriptor,
    context: &CallContext,
    body: &[u8],
) -> Result<AccountSession> {
    let now = Utc::now();
    if !context.bearer_grant_envelope.is_empty()
        || context
            .deadline
            .as_ref()
            .is_some_and(|deadline| deadline.seconds < now.timestamp())
    {
        bail!("invalid or expired device request");
    }
    let encoded =
        std::str::from_utf8(&context.bearer_capability).context("device Biscuit must be base64")?;
    if encoded.is_empty() || encoded.len() > 64 * 1024 {
        bail!("bounded device Biscuit required");
    }
    let root = if context.bearer_authority_key_selector.is_empty() {
        biscuit_verifier::unverified_authority_device_pop_key(encoded)?
            .context("Biscuit root selector required")?
    } else {
        PublicKey::from_bytes(
            &context.bearer_authority_key_selector,
            biscuit_auth::Algorithm::Ed25519,
        )?
    };
    let authority = repo::device_authority::load(home, now.timestamp())?;
    let token = biscuit_verifier::parse_token(encoded, &[root])?;
    let attachment_deadline = authority.verify_presented_authority(
        &root.to_bytes(),
        &token,
        &context.bearer_authority_proof,
        now.timestamp(),
    )?;
    let inspected = biscuit_verifier::inspect_verified_credential(&token, &root)?;
    if inspected
        .revocation_ids
        .iter()
        .any(|id| authority.revoked_ids.contains(id))
    {
        bail!("device capability revoked");
    }
    let publisher: [u8; 32] = inspected
        .proof_public_key
        .as_slice()
        .try_into()
        .context("Ed25519 proof key required")?;
    authority.verify_publisher(&publisher)?;
    let current = repo::verify_account_owner_observation(&authority.owner, now.timestamp())?;
    let account = current
        .signed_root()
        .root
        .as_ref()
        .context("account root missing")?;
    let principal = uuid::Uuid::from_slice(&account.account_uuid)?;
    if inspected
        .asserted_account
        .is_some_and(|asserted| asserted != principal)
    {
        bail!("credential account differs from admitted owner");
    }
    let mut expires = i64::try_from(inspected.expires_at_unix_seconds)
        .context("credential expiry out of range")?;
    if let Some(certificate_expiry) = attachment_deadline {
        expires = if expires == 0 {
            certificate_expiry
        } else {
            expires.min(certificate_expiry)
        };
    }
    if expires != 0 && now.timestamp() >= expires {
        bail!("device capability expired");
    }
    let proof = thread_api::request_proof::verify(
        context,
        method,
        body,
        &publisher,
        now.timestamp_millis(),
    )?;
    Ok(AccountSession {
        principal: principal.to_string(),
        publisher,
        inspected,
        authority_proof: context.bearer_authority_proof.clone(),
        token,
        root,
        expires,
        method,
        admission: (
            proof.identity().to_owned(),
            proof.nonce().to_vec(),
            now.timestamp_millis(),
        ),
        account_wide: false,
        scopes: vec![],
    })
}
