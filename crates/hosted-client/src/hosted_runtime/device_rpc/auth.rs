use std::path::Path;

use anyhow::{Context, Result, bail};
use api::{heddle::api::v1alpha1::CallContext, v2::MethodDescriptor};
use biscuit_verifier::{BiscuitFacts, PublicKey};
use chrono::Utc;
use repo::device_catalog::DeviceSpool;

pub(super) struct Session {
    pub principal: String,
    pub actor: String,
    pub attribution: objects::object::Attribution,
    pub spool: DeviceSpool,
    token: biscuit_auth::Biscuit,
    root: PublicKey,
    method: &'static MethodDescriptor,
    pub expires: i64,
    pub request_proof: objects::object::ContentHash,
}
impl Session {
    pub fn permits(&self, method: &str) -> bool {
        api::v2::method_descriptor(method).is_some_and(|descriptor| {
            facts(&self.token, descriptor, &self.spool, Utc::now()).is_ok()
        })
    }
    /// Local clock checks keep arbitrary ancestor caveats effective without any
    /// hosted call or periodic disk/SQL lookup on an idle subscription.
    pub fn check_clock(&self) -> Result<()> {
        let now = Utc::now();
        if self.expires != 0 && now.timestamp() >= self.expires {
            bail!("device capability expired");
        }
        facts(&self.token, self.method, &self.spool, now)?;
        Ok(())
    }
    pub fn check_current(&self, home: &Path) -> Result<()> {
        self.check_clock()?;
        let authority = repo::device_authority::load(home, Utc::now().timestamp())?;
        authority.verify_mint_root(&self.root.to_bytes(), Utc::now().timestamp())?;
        let checked = facts(&self.token, self.method, &self.spool, Utc::now())?;
        if checked
            .revocation_ids
            .iter()
            .any(|id| authority.revoked_ids.contains(id))
        {
            bail!("device capability revoked");
        }
        let current =
            repo::verify_account_owner_observation(&authority.owner, Utc::now().timestamp())?;
        let owner = current
            .signed_root()
            .root
            .as_ref()
            .context("account root missing")?;
        if uuid::Uuid::from_slice(&owner.account_uuid)?.to_string() != self.principal {
            bail!("device account changed");
        }
        let registered = repo::device_catalog::load(home, self.spool.id)?;
        if registered.capability_path != self.spool.capability_path
            || registered.heddle_dir != self.spool.heddle_dir
        {
            bail!("device spool registration changed");
        }
        Ok(())
    }
}
fn facts(
    token: &biscuit_auth::Biscuit,
    method: &MethodDescriptor,
    spool: &DeviceSpool,
    now: chrono::DateTime<Utc>,
) -> Result<BiscuitFacts> {
    let operation = method.path.rsplit('/').next().context("invalid method")?;
    Ok(biscuit_verifier::authorize_at(
        token,
        operation,
        now,
        None,
        &[],
        Some(("spool", &spool.capability_path)),
    )?)
}
pub(super) fn authorize(
    home: &Path,
    method: &'static MethodDescriptor,
    context: &CallContext,
    body: &[u8],
    spool: DeviceSpool,
) -> Result<Session> {
    let now = Utc::now();
    if !context.bearer_grant_envelope.is_empty()
        || context
            .deadline
            .as_ref()
            .is_some_and(|deadline| deadline.seconds < now.timestamp())
    {
        bail!("invalid or expired device request");
    }
    let token =
        std::str::from_utf8(&context.bearer_capability).context("device biscuit must be base64")?;
    if token.is_empty() || token.len() > 64 * 1024 {
        bail!("device biscuit required");
    }
    let root = if context.bearer_authority_key_selector.is_empty() {
        biscuit_verifier::unverified_authority_device_pop_key(token)?
            .context("Biscuit root selector required")?
    } else {
        PublicKey::from_bytes(
            &context.bearer_authority_key_selector,
            biscuit_auth::Algorithm::Ed25519,
        )?
    };
    let authority = repo::device_authority::load(home, now.timestamp())?;
    authority.verify_mint_root(&root.to_bytes(), now.timestamp())?;
    let parsed = biscuit_verifier::parse_token(token, &[root])?;
    let checked = facts(&parsed, method, &spool, now)?;
    if checked
        .revocation_ids
        .iter()
        .any(|id| authority.revoked_ids.contains(id))
    {
        bail!("device capability revoked");
    }
    let key: [u8; 32] = hex::decode(
        checked
            .cnf
            .as_deref()
            .context("Biscuit proof key missing")?,
    )?
    .try_into()
    .map_err(|_| anyhow::anyhow!("invalid proof key"))?;
    let proof =
        thread_api::request_proof::verify(context, method, body, &key, now.timestamp_millis())?;
    if !repo::device_catalog::claim_nonce(
        home,
        proof.identity(),
        proof.nonce(),
        now.timestamp_millis(),
    )? {
        bail!("request proof was already used");
    }
    let current = repo::verify_account_owner_observation(&authority.owner, now.timestamp())?;
    let owner = current
        .signed_root()
        .root
        .as_ref()
        .context("account root missing")?;
    let principal = uuid::Uuid::from_slice(&owner.account_uuid)?.to_string();
    let mut expires = i64::try_from(checked.exp).context("capability expiration out of range")?;
    if current.authority_key().public_key != root.to_bytes() {
        let attachment_expiry = authority
            .mint_roots
            .iter()
            .filter_map(|signed| signed.attachment.as_ref())
            .filter(|attachment| {
                attachment
                    .mint_root_key
                    .as_ref()
                    .is_some_and(|key| key.public_key == root.to_bytes())
            })
            .map(|attachment| attachment.expires_at_unix_seconds)
            .filter(|expiry| *expiry > now.timestamp())
            .min()
            .context("mint-root lifetime missing")?;
        expires = if expires == 0 {
            attachment_expiry
        } else {
            expires.min(attachment_expiry)
        };
    }
    let accountable = objects::object::Principal::new(&principal, "");
    let attribution = if checked.delegation_agent_id.is_some()
        || checked.agent_provider.is_some()
        || checked.agent_model.is_some()
    {
        let mut agent = objects::object::Agent::new(
            checked.agent_provider.clone().unwrap_or_default(),
            checked.agent_model.clone().unwrap_or_default(),
        );
        agent.session_id = checked
            .delegation_agent_id
            .clone()
            .or_else(|| Some(checked.sid.clone()));
        objects::object::Attribution::with_agent(accountable, agent)
    } else {
        objects::object::Attribution::human(accountable)
    };
    Ok(Session {
        principal,
        attribution,
        actor: hex::encode(key),
        spool,
        token: parsed,
        root,
        method,
        expires,
        request_proof: {
            use prost::Message;
            objects::object::ContentHash::compute_typed(
                "heddle-device-request-proof-v2",
                &context
                    .request_proof
                    .as_ref()
                    .context("request proof missing")?
                    .encode_to_vec(),
            )
        },
    })
}
