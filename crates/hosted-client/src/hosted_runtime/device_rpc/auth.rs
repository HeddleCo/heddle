use std::{collections::BTreeSet, path::Path, sync::Mutex};

use anyhow::{Context, Result, bail};
use api::{heddle::api::v1alpha1::CallContext, v2::MethodDescriptor};
use biscuit_verifier::{BiscuitFacts, PublicKey};
use chrono::Utc;
use repo::device_catalog::DeviceSpool;

#[derive(Default)]
struct BoundSources {
    threads: BTreeSet<objects::object::ContentHash>,
    epoch: Vec<u8>,
}
pub(super) struct Session {
    sources: Mutex<BoundSources>,
    pub principal: String,
    pub actor: String,
    pub agent_id: Option<String>,
    pub publisher: [u8; 32],
    pub attribution: objects::object::Attribution,
    pub spool: DeviceSpool,
    token: biscuit_auth::Biscuit,
    root: PublicKey,
    authority_proof: Vec<u8>,
    method: &'static MethodDescriptor,
    pub expires: i64,
    pub request_proof: objects::object::ContentHash,
}
impl Session {
    pub fn authorize_thread(
        &self,
        repository: &repo::Repository,
        replica: &repo::thread_replication::ThreadReplica,
    ) -> Result<()> {
        self.check_clock()?;
        anyhow::ensure!(
            replica.genesis()?.spool == self.spool.id.to_string(),
            "Thread belongs to another Spool"
        );
        anyhow::ensure!(
            thread_visible(
                repository,
                replica,
                uuid::Uuid::parse_str(&self.principal)?,
                self.agent_id.as_deref()
            )?,
            "Thread audience does not include authenticated caller"
        );
        Ok(())
    }

    pub fn authorize_revision(
        &self,
        repository: &repo::Repository,
        revision: objects::object::StateId,
    ) -> Result<()> {
        self.check_clock()?;
        let candidates = repo::thread_replication::ThreadReplica::source_thread_candidates(
            &self.spool.heddle_dir,
            revision,
            None,
            1024,
        )?;
        for thread in candidates {
            let replica =
                repo::thread_replication::ThreadReplica::open(&self.spool.heddle_dir, thread)?;
            if replica.genesis()?.spool == self.spool.id.to_string()
                && thread_visible(
                    repository,
                    &replica,
                    uuid::Uuid::parse_str(&self.principal)?,
                    self.agent_id.as_deref(),
                )?
            {
                self.bind_thread(thread)?;
                return Ok(());
            }
        }
        bail!("source revision has no accessible Thread within device authorization work budget")
    }
    pub(super) fn bind_thread(&self, thread: objects::object::ContentHash) -> Result<()> {
        let mut sources = self
            .sources
            .lock()
            .map_err(|_| anyhow::anyhow!("source authorization guard poisoned"))?;
        anyhow::ensure!(
            sources.threads.contains(&thread) || sources.threads.len() < 256,
            "source authorization dependency bound"
        );
        if sources.threads.insert(thread) {
            sources.epoch.clear();
        }
        Ok(())
    }
    fn check_sources(&self) -> Result<()> {
        let (threads, observed) = {
            let sources = self
                .sources
                .lock()
                .map_err(|_| anyhow::anyhow!("source authorization guard poisoned"))?;
            if sources.threads.is_empty() {
                return Ok(());
            }
            (sources.threads.clone(), sources.epoch.clone())
        };
        let epoch = repo::operation_dedup::observation::generation(&self.spool.heddle_dir)?;
        if epoch == observed {
            return Ok(());
        }
        let repository = repo::Repository::open(&self.spool.root)?;
        for thread in &threads {
            let replica =
                repo::thread_replication::ThreadReplica::open(&self.spool.heddle_dir, *thread)?;
            self.authorize_thread(&repository, &replica)?;
        }
        anyhow::ensure!(
            repo::operation_dedup::observation::generation(&self.spool.heddle_dir)? == epoch,
            "source authorization changed during disclosure check"
        );
        let mut sources = self
            .sources
            .lock()
            .map_err(|_| anyhow::anyhow!("source authorization guard poisoned"))?;
        if sources.threads == threads {
            sources.epoch = epoch;
        }
        Ok(())
    }
    pub fn command_namespace(&self) -> Result<String> {
        Ok(serde_json::to_string(&(
            self.principal.as_str(),
            self.agent_id.as_deref(),
        ))?)
    }
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
        authority.verify_presented_authority(
            &self.root.to_bytes(),
            &self.token,
            &self.authority_proof,
            Utc::now().timestamp(),
        )?;
        authority.verify_publisher(&self.publisher)?;
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
        self.check_sources()?;
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
    let parsed = biscuit_verifier::parse_token(token, &[root])?;
    let attachment_deadline = authority.verify_presented_authority(
        &root.to_bytes(),
        &parsed,
        &context.bearer_authority_proof,
        now.timestamp(),
    )?;
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
    authority.verify_publisher(&key)?;
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
    if let Some(attachment_expiry) = attachment_deadline {
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
        sources: Mutex::new(BoundSources::default()),
        principal,
        agent_id: checked.delegation_agent_id,
        attribution,
        actor: hex::encode(key),
        publisher: key,
        spool,
        token: parsed,
        authority_proof: context.bearer_authority_proof.clone(),
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

/// The caller has independently admitted the exact Spool capability first.
/// A local-key owner proof comes only from retained private key material.
pub(super) fn thread_visible(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
) -> Result<bool> {
    if replica.ownership_claims()?.len() > 1 { return Ok(false); }
    let genesis = replica.genesis()?;
    let local = match genesis.owner {
        objects::object::thread_replication::GenesisOwner::LocalKey(key)
            if repository.holds_native_owner_key(&key)? =>
        {
            Some(key)
        }
        _ => None,
    };
    Ok(replica.audience_allows(principal, agent, true, local.as_ref())?)
}
