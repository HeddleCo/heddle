use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
};

use anyhow::{Context, Result, bail};
use api::{heddle::api::v1alpha1::CallContext, v2::MethodDescriptor};
use biscuit_verifier::{BiscuitFacts, PublicKey};
use chrono::Utc;
use objects::store::ObjectStore;
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
    if replica.ownership_claims()?.len() > 1 && replica.ownership_resolution()?.is_none() {
        return Ok(false);
    }
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

/// A verified explicit Thread participant receives the ordinary Internal
/// floor. Other authorized Spool readers receive Public until a current
/// local audience grant supplies a stronger tier. Neither floor implies an
/// embargo label; Private remains withheld without one.
pub(super) fn record_visible(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
    tier: &objects::object::VisibilityTier,
) -> Result<bool> {
    let Some(audience) = reader_audience(repository, replica, principal, agent)? else {
        return Ok(false);
    };
    Ok(objects::object::visible(tier, &audience))
}

fn reader_audience(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
) -> Result<Option<objects::object::AudienceTier>> {
    if !thread_visible(repository, replica, principal, agent)? {
        return Ok(None);
    }
    let genesis = replica.genesis()?;
    let local = match genesis.owner {
        objects::object::thread_replication::GenesisOwner::LocalKey(key)
            if repository.holds_native_owner_key(&key)? =>
        {
            Some(key)
        }
        _ => None,
    };
    let explicit = replica.audience_allows(principal, agent, false, local.as_ref())?;
    let audience = if explicit {
        objects::object::AudienceTier::Internal
    } else {
        objects::object::AudienceTier::Public
    };
    Ok(Some(audience))
}

/// Exact accepted source membership and downward-closed state visibility are
/// independent of a readable Thread's metadata or a guessed genesis base.
pub(super) fn source_revision_visible(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
    revision: objects::object::StateId,
) -> Result<bool> {
    let mut owner = replica.clone();
    // A fork's base is carried by its parent Thread, not by an operation
    // invented under the child identity. Every hop needs its own audience.
    for _ in 0..128 {
        let Some(audience) = reader_audience(repository, &owner, principal, agent)? else {
            return Ok(false);
        };
        let genesis = owner.genesis()?;
        if revision == genesis.base {
            let seed =
                objects::object::thread_replication::hosted_import::synthetic_initial_base()?;
            if revision != seed.id() {
                let Some(parent) = genesis.parent else {
                    return Ok(false);
                };
                owner =
                    repo::thread_replication::ThreadReplica::open(repository.heddle_dir(), parent)?;
                if owner.genesis()?.spool != genesis.spool {
                    return Ok(false);
                }
                continue;
            }
            if !owner.has_source_possession(revision)? {
                return Ok(false);
            }
            let Some(stored) = repository.store().get_state(&revision)? else {
                return Ok(false);
            };
            if stored.encode_current_msgpack()? != seed.encode_current_msgpack()? {
                return Ok(false);
            }
            return Ok(repository
                .withholding_visibility_for_audience(&revision, &audience)?
                .is_none()
                && objects::object::visible(
                    &repository.resolve_capture_default_visibility(),
                    &audience,
                ));
        }
        if owner.accepted_source_revision(revision)?.is_none()
            || !owner.has_source_possession(revision)?
        {
            return Ok(false);
        }
        // A metadata-only courier can supply the accepted signed source before
        // this checkout has a local visibility sidecar. The original declaration
        // still binds the reader, including when the same State has several
        // accepted source operations.
        let operations = owner.source_operation_page(revision, None, 65)?;
        if operations.len() > 64 {
            return Ok(false);
        }
        for id in operations {
            let Some((signed, _)) = owner.operation(&id)? else {
                return Ok(false);
            };
            let operation = signed.verify()?;
            let Some(capture) = operation.source_result()? else {
                return Ok(false);
            };
            match capture.visibility.and_then(|visibility| visibility.state) {
                Some(tier) if !objects::object::visible(&tier, &audience) => return Ok(false),
                None if !objects::object::visible(
                    &repository.resolve_capture_default_visibility(),
                    &audience,
                ) =>
                {
                    return Ok(false);
                }
                _ => {}
            }
        }
        return Ok(repository
            .withholding_visibility_for_audience(&revision, &audience)?
            .is_none());
    }
    Ok(false)
}

/// Content projection for one exact accepted source, including original
/// signed entry declarations that have not yet been installed as local
/// sidecars. Every ancestor is traced through accepted originals in this
/// Thread's parent lineage or the canonical system seed.
pub(super) fn source_content_visibility(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
    revision: objects::object::StateId,
) -> Result<Option<objects::object::EntryRedactions>> {
    Ok(
        match source_content_admission(repository, replica, principal, agent, revision)? {
            SourceContentAdmission::Visible(redactions) => Some(redactions),
            SourceContentAdmission::Unavailable | SourceContentAdmission::Withheld => None,
        },
    )
}

/// A missing local State can be called unavailable only after its complete
/// signed lineage independently proves the same whole-tip audience rule.
pub(super) enum SourceContentAdmission {
    Visible(objects::object::EntryRedactions),
    Unavailable,
    Withheld,
}

pub(super) fn source_content_admission(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
    revision: objects::object::StateId,
) -> Result<SourceContentAdmission> {
    if let Some((redactions, _)) =
        source_content_projection(repository, replica, principal, agent, revision)?
    {
        return Ok(SourceContentAdmission::Visible(redactions));
    }
    if (!replica.has_source_possession(revision)?
        || repository.store().get_state(&revision)?.is_none())
        && signed_unmaterialized_source_visible(repository, replica, principal, agent, revision)?
    {
        return Ok(SourceContentAdmission::Unavailable);
    }
    Ok(SourceContentAdmission::Withheld)
}

fn signed_unmaterialized_source_visible(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
    revision: objects::object::StateId,
) -> Result<bool> {
    use objects::object::{ContentHash, StateId, visible};
    let seed = objects::object::thread_replication::hosted_import::synthetic_initial_base()?;
    let Some(target_audience) = reader_audience(repository, replica, principal, agent)? else {
        return Ok(false);
    };
    let mut pending = vec![(replica.thread_id(), revision, None::<ContentHash>)];
    let mut seen = BTreeSet::<(ContentHash, StateId)>::new();
    let mut original_bytes = 0usize;
    let mut edges = 0usize;
    while let Some((owner_id, id, exact_operation)) = pending.pop() {
        edges += 1;
        if edges > 4096 || pending.len() > 4096 {
            return Ok(false);
        }
        // An integration names one exact accepted operation. Check that edge
        // even when another path already visited the same State. Its policy
        // floor is then intersected with every accepted original for that
        // State, not just the integration's selected operation.
        if let Some(exact) = exact_operation {
            let owner =
                repo::thread_replication::ThreadReplica::open(repository.heddle_dir(), owner_id)?;
            let Some((signed, status)) = owner.operation(&exact)? else {
                return Ok(false);
            };
            original_bytes =
                original_bytes.saturating_add(signed.canonical.len() + signed.signature.len());
            if original_bytes > 16 * 1024 * 1024 {
                return Ok(false);
            }
            let operation = signed.verify()?;
            if status != objects::object::thread_replication::Admission::Accepted
                || operation.id()? != exact
                || operation.thread != owner_id
                || operation
                    .source_state()?
                    .is_none_or(|state| state.id() != id)
            {
                return Ok(false);
            }
        }
        if !seen.insert((owner_id, id)) {
            continue;
        }
        if seen.len() > 4096 || pending.len() > 4096 {
            return Ok(false);
        }
        let owner =
            repo::thread_replication::ThreadReplica::open(repository.heddle_dir(), owner_id)?;
        if owner.genesis()?.spool != replica.genesis()?.spool {
            return Ok(false);
        }
        let Some(owner_audience) = reader_audience(repository, &owner, principal, agent)? else {
            return Ok(false);
        };
        let genesis = owner.genesis()?;
        if id == seed.id() && id == genesis.base && genesis.parent.is_none() {
            for tier in [
                repository.effective_visibility_tier(&id)?,
                repository.resolve_capture_default_visibility(),
            ] {
                if (id == revision || tier.is_embargo())
                    && (!visible(&tier, &owner_audience) || !visible(&tier, &target_audience))
                {
                    return Ok(false);
                }
            }
            continue;
        }
        if id == genesis.base {
            let Some(parent) = genesis.parent else {
                return Ok(false);
            };
            pending.push((parent, id, None));
            continue;
        }
        let operations = owner.source_operation_page(id, None, 65)?;
        if operations.is_empty() || operations.len() > 64 {
            return Ok(false);
        }
        if exact_operation.is_some_and(|exact| !operations.contains(&exact)) {
            return Ok(false);
        }
        let mut state = None;
        let mut edge_parents = BTreeSet::new();
        for operation_id in operations {
            let Some((signed, status)) = owner.operation(&operation_id)? else {
                return Ok(false);
            };
            if status != objects::object::thread_replication::Admission::Accepted {
                return Ok(false);
            }
            original_bytes =
                original_bytes.saturating_add(signed.canonical.len() + signed.signature.len());
            if original_bytes > 16 * 1024 * 1024 {
                return Ok(false);
            }
            let operation = signed.verify()?;
            if operation.thread != owner_id || operation.id()? != operation_id {
                return Ok(false);
            }
            let Some(capture) = operation.source_result()? else {
                return Ok(false);
            };
            let original_state = capture.validated_state()?;
            if original_state.id() != id {
                return Ok(false);
            }
            if state
                .as_ref()
                .is_some_and(|prior: &objects::object::State| prior != &original_state)
            {
                return Ok(false);
            }
            let tier = capture
                .visibility
                .and_then(|visibility| visibility.state)
                .unwrap_or_else(|| repository.resolve_capture_default_visibility());
            if (id == revision || tier.is_embargo())
                && (!visible(&tier, &owner_audience) || !visible(&tier, &target_audience))
            {
                return Ok(false);
            }
            let edge = if let Some(integration) = operation.local_integration()? {
                Some((
                    integration.source_thread,
                    integration.source_operation,
                    integration.source_revision,
                ))
            } else {
                operation.integration()?.map(|integration| {
                    (
                        integration.source_thread,
                        integration.source_operation,
                        integration.source_revision,
                    )
                })
            };
            if let Some((source_thread, source_operation, source_revision)) = edge {
                if source_thread == owner_id
                    || (source_revision != id && !original_state.parents.contains(&source_revision))
                {
                    return Ok(false);
                }
                let source = repo::thread_replication::ThreadReplica::open(
                    repository.heddle_dir(),
                    source_thread,
                )?;
                let Some((original, status)) = source.operation(&source_operation)? else {
                    return Ok(false);
                };
                let source_original = original.verify()?;
                if status != objects::object::thread_replication::Admission::Accepted
                    || source_original.thread != source_thread
                    || source_original.id()? != source_operation
                    || source_original
                        .source_state()?
                        .is_none_or(|state| state.id() != source_revision)
                {
                    return Ok(false);
                }
                edge_parents.insert(source_revision);
                pending.push((source_thread, source_revision, Some(source_operation)));
            }
            state = Some(original_state);
        }
        let Some(state) = state else {
            return Ok(false);
        };
        let local_tier = repository.effective_visibility_tier(&id)?;
        if (id == revision || local_tier.is_embargo())
            && (!visible(&local_tier, &owner_audience) || !visible(&local_tier, &target_audience))
        {
            return Ok(false);
        }
        for parent in state.parents {
            if !edge_parents.contains(&parent) {
                pending.push((owner_id, parent, None));
            }
        }
    }
    Ok(true)
}

/// The signed floor is used when authoring an integration. It cannot be
/// reconstructed from local sidecars alone after a metadata-only import.
pub(super) fn source_visibility_floor(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
    revision: objects::object::StateId,
) -> Result<Option<objects::object::VisibilityTier>> {
    Ok(
        source_content_projection(repository, replica, principal, agent, revision)?
            .map(|(_, tier)| tier),
    )
}

fn source_content_projection(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
    revision: objects::object::StateId,
) -> Result<
    Option<(
        objects::object::EntryRedactions,
        objects::object::VisibilityTier,
    )>,
> {
    if !source_revision_visible(repository, replica, principal, agent, revision)? {
        return Ok(None);
    }
    let Some(audience) = reader_audience(repository, replica, principal, agent)? else {
        return Ok(None);
    };
    let Some(local_proof) = repository.collect_content_disclosure(&revision)? else {
        return Ok(None);
    };
    let lineage = local_proof.states();
    let Some(mut redactions) = local_proof.for_audience(&audience) else {
        return Ok(None);
    };
    let mut floor = repository.effective_visibility_tier(&revision)?;
    let seed = objects::object::thread_replication::hosted_import::synthetic_initial_base()?;
    let lineage_set: BTreeSet<_> = lineage.iter().copied().collect();
    let mut originals: BTreeMap<
        objects::object::StateId,
        Vec<objects::object::thread_replication::CaptureVisibility>,
    > = BTreeMap::new();
    // A local/hosted Integration imports one exact original from its source
    // Thread. That signed edge, or an independently admitted fork parent,
    // justifies entering another Thread; a matching State in the same Spool
    // does not. Every entered Thread gets its own current audience check.
    let mut pending_threads = vec![replica.thread_id()];
    let mut owner_ids = BTreeSet::new();
    let mut work = 0usize;
    let mut bytes = 0usize;
    while let Some(owner_id) = pending_threads.pop() {
        if !owner_ids.insert(owner_id) {
            continue;
        }
        if owner_ids.len() + pending_threads.len() > 128 {
            return Ok(None);
        }
        let owner =
            repo::thread_replication::ThreadReplica::open(repository.heddle_dir(), owner_id)?;
        let Some(owner_audience) = reader_audience(repository, &owner, principal, agent)? else {
            return Ok(None);
        };
        for (id, signed) in owner.accepted_source_originals_for_revisions(lineage)? {
            work += 1;
            bytes = bytes.saturating_add(signed.canonical.len());
            if work > 4096 || bytes > 16 * 1024 * 1024 {
                return Ok(None);
            }
            let operation = signed.verify()?;
            if operation.thread != owner.thread_id() {
                return Ok(None);
            }
            let Some(capture) = operation.source_result()? else {
                return Ok(None);
            };
            let state = capture.validated_state()?;
            if state.id() != id {
                return Ok(None);
            }
            let dependency = if let Some(integration) = operation.local_integration()? {
                Some((
                    integration.source_thread,
                    integration.source_operation,
                    integration.source_revision,
                ))
            } else {
                operation.integration()?.map(|integration| {
                    (
                        integration.source_thread,
                        integration.source_operation,
                        integration.source_revision,
                    )
                })
            };
            if let Some((source_thread, source_operation, source_revision)) = dependency {
                if !lineage_set.contains(&source_revision) || source_thread == owner_id {
                    return Ok(None);
                }
                let source = repo::thread_replication::ThreadReplica::open(
                    repository.heddle_dir(),
                    source_thread,
                )?;
                if source.genesis()?.spool != owner.genesis()?.spool {
                    return Ok(None);
                }
                let Some((original, status)) = source.operation(&source_operation)? else {
                    return Ok(None);
                };
                let source_original = original.verify()?;
                if status != objects::object::thread_replication::Admission::Accepted
                    || source_original.thread != source_thread
                    || source_original
                        .source_state()?
                        .is_none_or(|state| state.id() != source_revision)
                {
                    return Ok(None);
                }
                pending_threads.push(source_thread);
            }
            if state.id() == revision
                && capture
                    .visibility
                    .as_ref()
                    .and_then(|visibility| visibility.state.as_ref())
                    .is_none()
            {
                floor =
                    objects::object::thread_replication::local_integration::intersect_visibility(
                        &floor,
                        &repository.resolve_capture_default_visibility(),
                    )?;
            }
            if let Some(visibility) = capture.visibility {
                if let Some(tier) = &visibility.state {
                    let check = state.id() == revision || tier.is_embargo();
                    if check
                        && (!objects::object::visible(tier, &owner_audience)
                            || !objects::object::visible(tier, &audience))
                    {
                        return Ok(None);
                    }
                    if check {
                        floor = objects::object::thread_replication::local_integration::intersect_visibility(
                            &floor, tier,
                        )?;
                    }
                }
                originals.entry(state.id()).or_default().push(visibility);
            } else {
                originals.entry(state.id()).or_default();
            }
        }
        let genesis = owner.genesis()?;
        if let Some(parent) = genesis.parent {
            let parent_replica =
                repo::thread_replication::ThreadReplica::open(repository.heddle_dir(), parent)?;
            if parent_replica.genesis()?.spool != genesis.spool {
                return Ok(None);
            }
            pending_threads.push(parent);
        }
    }
    for id in lineage {
        if *id != seed.id() && !originals.contains_key(id) {
            return Ok(None);
        }
        for visibility in originals.get(id).into_iter().flatten() {
            redactions.extend_overrides(&visibility.entries, |tier| {
                objects::object::visible(tier, &audience)
            });
        }
    }
    Ok(Some((redactions, floor)))
}

/// An override from a historical State matters only if its exact salted leaf
/// still occurs in the selected tree. This bounded walk is shared by native
/// Read attestation; a stale hidden ancestor leaf does not taint new content.
pub(super) fn selected_source_full_content_visible(
    repository: &repo::Repository,
    revision: objects::object::StateId,
    redactions: &objects::object::EntryRedactions,
) -> Result<bool> {
    use objects::object::TreeEntryTarget;
    let Some(state) = repository.store().get_state(&revision)? else {
        return Ok(false);
    };
    if state.id() != revision {
        return Ok(false);
    }
    let mut pending = vec![state.tree];
    let mut seen = BTreeSet::new();
    let mut bytes = 0usize;
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if seen.len() > 4096 {
            return Ok(false);
        }
        let Some(tree) = repository.store().get_tree(&id)? else {
            return Ok(false);
        };
        let canonical = tree.encode_canonical()?;
        bytes = bytes.saturating_add(canonical.len());
        if tree.hash() != id || bytes > 16 * 1024 * 1024 {
            return Ok(false);
        }
        for (index, entry) in tree.entries().iter().enumerate() {
            if !redactions.entry_visible(&tree, index) {
                return Ok(false);
            }
            if let TreeEntryTarget::Tree { hash } = entry.target() {
                pending.push(*hash);
            }
        }
    }
    Ok(true)
}

pub(super) fn discussion_visible(
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    principal: uuid::Uuid,
    agent: Option<&str>,
    id: objects::object::DiscussionRecordId,
) -> Result<bool> {
    let summary = replica.discussion_summary(id, 1024 * 1024)?;
    record_visible(
        repository,
        replica,
        principal,
        agent,
        &summary.discussion.visibility,
    )
}

#[cfg(test)]
mod metadata_source_tests {
    use crypto::{Signer, thread_operation::SignedOperation};
    use objects::object::{
        Attribution, Principal, State, VisibilityTier,
        thread_replication::{
            Admission, AuthoredCapture, Capture, CaptureVisibility, ThreadOperation,
            ThreadOperationBody,
        },
    };

    use super::*;

    #[test]
    fn missing_source_state_needs_all_signed_original_and_ancestor_floors() {
        let directory = tempfile::tempdir().expect("source fixture directory");
        let repository = repo::Repository::init_default(directory.path()).expect("repository");
        let seed_id = repository.head().expect("head").expect("canonical seed");
        let thread = repository
            .create_native_thread("metadata-source", seed_id, None, "metadata source")
            .expect("local Thread");
        let signer = repository
            .native_original_owner_signer(&thread)
            .expect("owner signer");
        let principal = uuid::Uuid::from_bytes([7; 16]);
        let publish = |state: &State, tier: Option<VisibilityTier>, parents: BTreeSet<_>| {
            let visibility = tier.map(|state| CaptureVisibility {
                state: Some(state),
                embargo_until: None,
                entries: vec![],
            });
            let operation = ThreadOperation {
                version: 1,
                thread: thread.thread_id(),
                parents,
                publisher: signer.public_key().try_into().expect("publisher key"),
                body: ThreadOperationBody::Capture(AuthoredCapture::local(Capture {
                    state: state.encode_current_msgpack().expect("signed State bytes"),
                    source_targets: None,
                    visibility,
                })),
            };
            let id = operation.id().expect("operation ID");
            let signed = SignedOperation::sign(&operation, &signer).expect("source signature");
            assert_eq!(
                thread
                    .receive_source_metadata(&signed, repository.store(), None, |_| Ok(()))
                    .expect("metadata-only source admission"),
                Admission::Accepted
            );
            id
        };
        let state = State::new_snapshot(
            repository
                .store()
                .get_state(&seed_id)
                .expect("seed read")
                .expect("seed")
                .tree,
            vec![seed_id],
            Attribution::human(Principal::new("owner", "")),
        );
        assert!(
            repository
                .store()
                .get_state(&state.id())
                .expect("source read")
                .is_none()
        );
        let first = publish(&state, None, BTreeSet::new());
        assert!(matches!(
            source_content_admission(&repository, &thread, principal, None, state.id())
                .expect("signed public source"),
            SourceContentAdmission::Unavailable
        ));
        publish(
            &state,
            Some(VisibilityTier::Private {
                scope_label: "held".into(),
            }),
            BTreeSet::new(),
        );
        assert!(
            matches!(
                source_content_admission(&repository, &thread, principal, None, state.id())
                    .expect("second original floor"),
                SourceContentAdmission::Withheld
            ),
            "a second accepted original for the same State must not be skipped"
        );
        let child = State::new_snapshot(
            state.tree,
            vec![state.id()],
            Attribution::human(Principal::new("owner", "")),
        );
        publish(&child, None, BTreeSet::from([first]));
        assert!(
            matches!(
                source_content_admission(&repository, &thread, principal, None, child.id())
                    .expect("signed hidden ancestor"),
                SourceContentAdmission::Withheld
            ),
            "an embargoed signed ancestor must not make missing child bytes observable"
        );
    }
}
