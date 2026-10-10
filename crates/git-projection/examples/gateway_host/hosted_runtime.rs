// SPDX-License-Identifier: Apache-2.0
//! Concrete native Container composition. The caller provisions existing scoped
//! credentials; this executable never enrolls, discovers or creates an identity.
use crate::{
    Result,
    catalog_publication::{ArtifactsGit, CatalogCommit},
    native_frame::{self, Frame},
    native_publication::{ExportArtifact, HydratedPublication, StagedPublication},
    policy::{canonical, digest, hex},
    runtime_trace::{Span, count},
    session_authority::{SessionAuthority, VerifiedActor, session},
    transport,
};
use api::heddle::api::v1alpha2::*;
use crypto::{Signer, thread_operation::SignedOperation};
use heddle_git_projection::{
    gateway_publication::{HistoryBudget, HistorySelection, PreparedHistory, PublicationScope},
    gateway_received::{RECEIVED_GIT_RECIPE, ReceivedGitProjection},
    gateway_view::{HistoryTip, ViewLimits, export_public_git_history},
    gateway_write::{LocalPush, PushAuthor, WriteLimits, parse_receive_pack, prepare_git_push},
};
use hosted_client::gateway::GatewayClient;
use objects::{
    object::{
        ContentHash, OperationId, StateId, ThreadName, VisibilityTier,
        original_boundary_acceptance::BoundaryOriginalKind, thread_replication::SourceAuthor,
    },
    store::ObjectStore,
};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{BTreeMap, HashSet},
    fs::File,
    future::Future,
    io::Read,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    tenant_spool_id: String,
    spool_id: String,
    repository: String,
    repo_path: String,
    thread_id: String,
    thread: String,
    disclosure_audience: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[cfg(feature = "gateway-fixture")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fixture: Option<FixtureConfig>,
    schema: u32,
    scope: Scope,
    weft_server: String,
    authority_origin: String,
    descriptor_key_id: String,
    descriptor_public_key: String,
    gateway_principal: String,
    gateway_biscuit: PathBuf,
    gateway_signer_pem: PathBuf,
    gateway_source_author: PathBuf,
    gateway_mint_attachment: Option<PathBuf>,
    artifacts_url: String,
    artifacts_credential: PathBuf,
    service_sha256: String,
    scratch: PathBuf,
}
#[cfg(feature = "gateway-fixture")]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureConfig {
    ca_pem: PathBuf,
    native_address: SocketAddr,
}
#[cfg(feature = "gateway-fixture")]
fn fixture_ca(config: &Config, bind: &str) -> Result<Option<Vec<u8>>> {
    let Some(fixture) = &config.fixture else {
        return Ok(None);
    };
    let listener: SocketAddr = bind.parse()?;
    if !listener.ip().is_loopback() {
        return Err("fixture listener must be loopback".into());
    }
    if config.weft_server != "https://native-fixture.example"
        || !fixture.native_address.ip().is_loopback()
        || fixture.native_address.port() == 0
    {
        return Err("explicit canonical native fixture and loopback target required".into());
    }
    for origin in [&config.authority_origin, &config.artifacts_url] {
        let url = reqwest::Url::parse(origin)?;
        if url.scheme() != "https"
            || url.host_str() != Some("127.0.0.1")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err("fixture HTTPS destinations must be literal loopback".into());
        }
    }
    Ok(Some(read(&fixture.ca_pem, 64 * 1024)?))
}
fn read(path: &Path, max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    File::open(path)?
        .take(max as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > max {
        return Err("hosted input bound".into());
    }
    Ok(out)
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}").into())
}
fn take_part(
    v: &Value,
    key: &str,
    max: usize,
    parts: &mut BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let name = text(v, key)?;
    let out = parts.remove(name).ok_or("binary part missing or reused")?;
    if out.len() > max {
        return Err("binary part size limit".into());
    }
    Ok(out)
}

fn state(v: &Value, key: &str) -> Result<StateId> {
    let raw = text(v, key)?;
    let state = StateId::parse(raw)?;
    if state.to_string_full() != raw {
        return Err("canonical full native State required".into());
    }
    Ok(state)
}
fn h32(bytes: &[u8]) -> Result<ContentHash> {
    Ok(ContentHash::from_bytes(bytes.try_into()?))
}
fn revision(r: &RevisionRef) -> Result<StateId> {
    match &r.revision {
        Some(revision_ref::Revision::State(s)) => {
            Ok(StateId::from_bytes(s.value.as_slice().try_into()?))
        }
        _ => Err("native State revision required".into()),
    }
}
fn err(e: impl std::fmt::Display) -> heddle_git_projection::GitProjectionError {
    heddle_git_projection::GitProjectionError::Git(e.to_string())
}
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex<const N: usize>(s: &str) -> Result<[u8; N]> {
    if !hex(s, N * 2) {
        return Err("canonical hex required".into());
    }
    let mut out = [0; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?
    }
    Ok(out)
}
impl Scope {
    fn authority_scope(&self) -> Result<biscuit_verifier::git_transport::GitScope> {
        use biscuit_verifier::git_transport::{GitAction, GitScope};
        let scope = GitScope {
            service_audience: "git-gateway".into(),
            tenant_spool_id: self.tenant_spool_id.parse()?,
            spool_id: self.spool_id.parse()?,
            repository_path: self.repo_path.clone(),
            thread_id: unhex::<32>(&self.thread_id)?,
            action: GitAction::Write,
            disclosure_audience: "public".into(),
        };
        scope.validate()?;
        Ok(scope)
    }

    fn wire(&self, write: bool) -> Result<GitTransportScope> {
        for id in [&self.tenant_spool_id, &self.spool_id] {
            let u = uuid::Uuid::parse_str(id)?;
            if u.is_nil() || u.to_string() != *id {
                return Err("canonical Spool required".into());
            }
        }
        if self.disclosure_audience != "public"
            || self.repository.is_empty()
            || self.repository.len() > 64
            || !self
                .repository
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("bounded public repository required".into());
        }
        ThreadName::from_git_branch(&self.thread)?;
        Ok(GitTransportScope {
            service_audience: "git-gateway".into(),
            tenant_spool: Some(SpoolRef {
                id: self.tenant_spool_id.clone(),
            }),
            spool: Some(SpoolRef {
                id: self.spool_id.clone(),
            }),
            repository_path: self.repo_path.clone(),
            thread: Some(ThreadId {
                value: unhex::<32>(&self.thread_id)?.to_vec(),
            }),
            action: if write {
                GitTransportAction::Write as i32
            } else {
                GitTransportAction::Read as i32
            },
            disclosure_audience: "public".into(),
        })
    }
    fn thread_ref(&self) -> Result<ThreadRef> {
        let s = self.wire(true)?;
        Ok(ThreadRef {
            spool: s.spool,
            id: s.thread,
        })
    }
}
struct ColdRepository {
    _root: tempfile::TempDir,
    repo: repo::Repository,
}
struct RestoredPublication {
    _directory: tempfile::TempDir,
    stage: StagedPublication,
}
impl std::ops::Deref for RestoredPublication {
    type Target = StagedPublication;
    fn deref(&self) -> &Self::Target {
        &self.stage
    }
}
pub struct Runtime {
    config: Config,
    client: GatewayClient,
    authority: SessionAuthority,
    executor: tokio::runtime::Runtime,
    deadline: Cell<Option<Instant>>,
    #[cfg(feature = "gateway-fixture")]
    fixture_ca: Option<Vec<u8>>,
}
impl Runtime {
    fn remaining(&self) -> Result<Duration> {
        self.deadline
            .get()
            .ok_or("request deadline absent")?
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "native request deadline".into())
    }
    fn wait<T, E>(&self, future: impl Future<Output = std::result::Result<T, E>>) -> Result<T>
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let remaining = self.remaining()?;
        self.executor.block_on(async {
            tokio::time::timeout(remaining, future)
                .await
                .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
                    "native request deadline".into()
                })?
                .map_err(Into::into)
        })
    }
    fn scope(&self, write: bool) -> Result<GitTransportScope> {
        self.config.scope.wire(write)
    }
    fn same_repo(&self, value: &Value) -> Result<()> {
        if text(value, "repository")? != self.config.scope.repository {
            return Err("repository scope differs".into());
        }
        Ok(())
    }
    fn history(
        &self,
        header: &str,
        write: bool,
        head: StateId,
    ) -> Result<GitHistoryAuthorizeResponse> {
        let _trace = Span::new("current_history_authority");
        self.remaining()?;
        let result =
            self.authority
                .history(header, &self.scope(write)?, None, Some(head.as_bytes()))?;
        self.remaining()?;
        Ok(result)
    }
    fn generation(&self, h: &GitHistoryAuthorizeResponse) -> Result<String> {
        if h.billing_owner_account_id.is_empty()
            || h.spool_genesis.len() != 32
            || h.sharing_policy_version.len() != 32
        {
            return Err("current ownership/policy absent".into());
        }
        Ok(digest(&canonical(
            &json!({"scope":self.config.scope,"epoch":h.authorization_epoch,"billing_owner":h.billing_owner_account_id,"sharing_policy":hex_bytes(&h.sharing_policy_version)}),
        )?))
    }
    fn require_same(
        &self,
        old: &GitHistoryAuthorizeResponse,
        new: &GitHistoryAuthorizeResponse,
    ) -> Result<()> {
        if old.encode_to_vec() != new.encode_to_vec() {
            return Err("source authority changed during work".into());
        }
        Ok(())
    }
    fn hydrate(
        &self,
        header: &str,
        h: &GitHistoryAuthorizeResponse,
        head: StateId,
    ) -> Result<ColdRepository> {
        let _trace = Span::new("cold_native_hydration");
        let inventory = h
            .bootstrap
            .as_ref()
            .ok_or("exact current bootstrap inventory required")?;
        if revision(inventory.head.as_ref().ok_or("native head absent")?)? != head
            || inventory.revisions.is_empty()
            || inventory.revisions.len() > 128
        {
            return Err("native closure inventory differs".into());
        }
        count("hydration_revisions", inventory.revisions.len());
        let root = tempfile::tempdir_in(&self.config.scratch)?;
        repo::clone_intent::CloneIntent {
            origin: self.config.authority_origin.clone(),
            endpoint: self.config.weft_server.clone(),
            repository: self.config.scope.repo_path.clone(),
            thread: Some(self.config.scope.thread.clone()),
            advertised_head: Some(self.config.scope.thread.clone()),
            depth: None,
            lazy: false,
        }
        .create(root.path())?;
        let repo =
            repo::Repository::init_clone(root.path(), repo::RepositorySourceAuthority::Native)?;
        let mut total = 0u64;
        for selected in &inventory.revisions {
            if selected.thread.as_ref() != Some(&self.config.scope.thread_ref()?) {
                return Err("cross-Thread write hydration unsupported".into());
            }
            let r = selected
                .revision
                .clone()
                .ok_or("historical revision absent")?;
            let id = revision(&r)?;
            let limits = thread_api::fetch::Limits {
                max_artifact_bytes: 64 * 1024 * 1024,
                max_total_bytes: 64 * 1024 * 1024,
                max_operations: 4096,
            };
            self.wait(self.client.hydrate_exact(
                &repo,
                &self.config.scope.authority_scope()?,
                FetchOpen {
                    thread: selected.thread.clone(),
                    revision: Some(r),
                    selection: Some(TransferSelection {
                        facets: vec![SharedFacet::Source as i32],
                        ..Default::default()
                    }),
                    protocol: Some(thread_api::hybrid::protocol()),
                    ..Default::default()
                },
                limits,
                root.path(),
            ))?;
            // Native source installation authenticates pack/index and originals. Bound the
            // whole retained store as well, rather than resetting the byte budget per revision.
            total = directory_bytes(repo.heddle_dir())?;
            if total > 128 * 1024 * 1024 || repo.store().get_state(&id)?.is_none() {
                return Err("hydrated source budget or identity".into());
            }
        }
        count("hydrated_store_bytes", total as usize);
        let replica = repo::thread_replication::ThreadReplica::open(
            repo.heddle_dir(),
            h32(&unhex::<32>(&self.config.scope.thread_id)?)?,
        )?;
        replica.bind_local_name(&self.config.scope.thread)?;
        let projection = replica.projection()?;
        if projection.source_heads != [head] {
            return Err("hydrated native head differs".into());
        }
        repo.set_thread_recorded(
            &ThreadName::from_git_branch(&self.config.scope.thread)?,
            &head,
        )?;
        repo.refs().write_head(&refs::Head::Attached {
            thread: ThreadName::from_git_branch(&self.config.scope.thread)?,
        })?;
        let mut config = repo.config().clone();
        config.review.discussion.default_visibility = VisibilityTier::Public;
        config.save(&repo.heddle_dir().join("config.toml"))?;
        repo::clone_intent::CloneIntent::clear(root.path())?;
        let repo = repo::Repository::open(root.path())?;
        self.require_same(h, &self.history(header, true, head)?)?;
        Ok(ColdRepository { _root: root, repo })
    }
    fn publication_scope(
        &self,
        h: &GitHistoryAuthorizeResponse,
        command: OperationId,
    ) -> Result<PublicationScope> {
        Ok(PublicationScope {
            thread: self.config.scope.thread_ref()?,
            source: EndpointRef {
                public_key: self.client.signer().public_key().to_vec(),
                kind: EndpointKind::Device as i32,
            },
            spool_genesis: h32(&h.spool_genesis)?,
            sharing_policy: h32(&h.sharing_policy_version)?,
            command,
        })
    }
    fn prepared(
        &self,
        input: &Value,
        header: &str,
        bootstrap: bool,
        refresh: bool,
        parts: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<Frame> {
        let _trace = Span::new("prepare_native_publication");
        self.same_repo(input)?;
        let actor = self.authority.authorize(header, &self.scope(true)?)?;
        let published = input.get("published");
        let head = if bootstrap {
            state(input, "expected_native")?
        } else {
            state(
                published
                    .and_then(|v| v.get("intent"))
                    .ok_or("published intent absent")?,
                "native_state",
            )?
        };
        let before = self.history(header, true, head)?;
        let generation = self.generation(&before)?;
        let cold = self.hydrate(header, &before, head)?;
        let native = &cold.repo;
        let remote = self.wait(self.client.hosted().native())?;
        let replica = native.native_thread(&self.config.scope.thread)?;
        let mut history;
        let signed;
        if bootstrap {
            let selection = before.bootstrap.as_ref().ok_or("bootstrap missing")?;
            let states = selection
                .revisions
                .iter()
                .map(|r| revision(r.revision.as_ref().ok_or("revision missing")?))
                .collect::<Result<Vec<_>>>()?;
            let originals = replica
                .accepted_source_originals_for_revisions(&states)?
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>();
            // Deterministic command namespace; still no native acceptance or source mutation.
            let command:OperationId=digest(&canonical(&json!({"bootstrap":head.to_string_full(),"scope":self.config.scope,"generation":generation}))?)[..32].parse()?;
            history = PreparedHistory::prepare(
                &remote,
                native.store(),
                HistorySelection {
                    tip: head,
                    genesis: replica.genesis_record()?,
                    originals: &originals,
                },
                self.publication_scope(&before, command)?,
                cold._root.path(),
                HistoryBudget::default(),
                |_| {
                    self.require_same(&before, &self.history(header, true, head).map_err(err)?)
                        .map_err(err)
                },
            )?;
            signed = None;
        } else {
            let raw = take_part(input, "request_part", transport::MAX_RECEIVE_REQUEST, parts)?;
            let received = parse_receive_pack(&raw, WriteLimits::default())?;
            if received.update.thread != self.config.scope.thread {
                return Err("Git branch differs".into());
            }
            let quarantine = tempfile::tempdir_in(&self.config.scratch)?;
            let git = sley::Repository::init_bare(quarantine.path())?;
            export_public_git_history(
                native,
                &git,
                &[HistoryTip {
                    thread: &self.config.scope.thread,
                    state: head,
                }],
                &[&self.config.scope.thread],
                ViewLimits::default(),
            )?;
            let mut command = Command::new("git");
            command.arg("--git-dir").arg(quarantine.path()).args([
                "index-pack",
                "--stdin",
                "--strict",
                "--fix-thin",
            ]);
            if !transport::run_bounded_pack(&mut command, received.pack, tempfile::tempfile()?)?
                .success()
            {
                return Err("hostile or incomplete Git pack".into());
            }
            let source_authority = self.wait(
                self.client
                    .refresh_source_authority(&self.config.scope.authority_scope()?),
            )?;
            let publisher: [u8; 32] = self.client.signer().public_key().try_into()?;
            let prepare = prepare_git_push(
                native,
                &git,
                LocalPush {
                    update: &received.update,
                    expected_native: head,
                    policy_generation: &generation,
                },
                PushAuthor {
                    actor: actor.actor(),
                    publisher,
                    source_author: self.client.source_author(),
                },
                WriteLimits::default(),
                |scope| {
                    if scope.actor != actor.actor()
                        || scope.publisher != publisher
                        || scope.spool != self.config.scope.spool_id
                    {
                        return Err(err("prepared identity differs"));
                    }
                    self.require_same(&before, &self.history(header, true, head).map_err(err)?)
                        .map_err(err)
                },
            )?;
            let known = prepare
                .historical_originals()
                .iter()
                .map(|s| Ok(s.verify()?.id()?))
                .collect::<Result<HashSet<_>>>()?;
            let originals = prepare
                .operations()
                .iter()
                .map(|o| SignedOperation::sign(o, self.client.signer()))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let fresh = repo::Repository::open(native.root())?;
            let push = prepare.bind_signed(
                &fresh,
                originals,
                |_| {
                    self.require_same(&before, &self.history(header, true, head).map_err(err)?)
                        .map_err(err)
                },
                |_, op| {
                    if known.contains(&op.id().map_err(err)?) {
                        Ok(())
                    } else {
                        source_authority.verify_original(op).map_err(err)
                    }
                },
            )?;
            history = PreparedHistory::for_git_push(
                &remote,
                &fresh,
                &push,
                replica.genesis_record()?,
                self.publication_scope(&before, push.receipt().command_id)?,
                cold._root.path(),
                HistoryBudget::default(),
                |_, _| {
                    self.require_same(&before, &self.history(header, true, head).map_err(err)?)
                        .map_err(err)
                },
            )?;
            signed = Some(push);
        }
        self.wait(
            self.client
                .refresh_source_authority(&self.config.scope.authority_scope()?),
        )?;
        for revision in history.revisions_mut() {
            revision.sign_acceptance(
                self.client.source_author().clone(),
                [
                    BoundaryOriginalKind::Source,
                    BoundaryOriginalKind::AccountGenesis,
                    BoundaryOriginalKind::OwnershipClaim,
                    BoundaryOriginalKind::OwnershipResolution,
                ]
                .into(),
                self.client.signer(),
            )?;
        }
        self.wait(
            self.client
                .refresh_source_authority(&self.config.scope.authority_scope()?),
        )?;
        self.require_same(&before, &self.history(header, true, head)?)?;
        // R2 owns pending recovery. Container preparation is disposable per request.
        let stage_root = tempfile::tempdir_in(&self.config.scratch)?;
        let stage = if let Some(push) = &signed {
            self.wait(StagedPublication::stage(
                stage_root.path(),
                &history,
                push,
                actor.actor(),
                &self.scope(true)?,
                before.source_generation,
            ))?
        } else {
            self.wait(StagedPublication::stage_bootstrap(
                stage_root.path(),
                &history,
                actor.actor(),
                &self.scope(true)?,
            ))?
        };
        let expected_catalog = if bootstrap && !refresh {
            None
        } else {
            Some(text(published.ok_or("publication missing")?, "pin")?.to_string())
        };
        let mut result = self.envelope(&stage, &before, &actor, expected_catalog.as_deref())?;
        if refresh {
            let old = published
                .and_then(|p| p.get("intent"))
                .ok_or("refresh base absent")?;
            let intent = &mut result.payload["intent"];
            intent["schema"] = json!(3);
            intent["expected_native"] = old["native_state"].clone();
            intent["old_git"] = old["new_git"].clone();
            intent["expected_generation"] = json!(before.source_generation);
            *intent = normalize(intent.clone())?;
        }
        Ok(result)
    }
    fn derive_intent(
        &self,
        stage: &StagedPublication,
        h: &GitHistoryAuthorizeResponse,
        _actor: &VerifiedActor,
        expected_catalog: Option<&str>,
    ) -> Result<Value> {
        let _trace = Span::new("derive_native_catalog");
        let hydrated = stage.hydrate(h32(&h.spool_genesis)?)?;
        let projected = hydrated.project()?;
        self.intent_from_projection(stage, h, expected_catalog, &hydrated, &projected)
    }
    fn intent_from_projection(
        &self,
        stage: &StagedPublication,
        h: &GitHistoryAuthorizeResponse,
        expected_catalog: Option<&str>,
        hydrated: &HydratedPublication,
        projected: &ReceivedGitProjection,
    ) -> Result<Value> {
        let frames = stage.openings()?;
        let ids = projected
            .history()
            .states()
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let mut history = Vec::new();
        for (i, frame) in frames.iter().enumerate() {
            let open = opening(frame)?;
            let id = revision(open.revision.as_ref().ok_or("source revision absent")?)?;
            let state = hydrated
                .source
                .get_state(&id)?
                .ok_or("source State absent")?;
            let mut artifacts = Vec::new();
            for a in 0..2 {
                let data = stage.artifact(i, a)?;
                artifacts.push(json!({"sha256":digest(&data),"size":data.len(),"kind":if a==0{"pack"}else{"index"}}));
            }
            artifacts.sort_by_key(|v| v["sha256"].as_str().unwrap_or_default().to_string());
            history.push(json!({"state":id.to_string_full(),"parents":state.parents.iter().filter(|p|ids.contains(p)).map(StateId::to_string_full).collect::<Vec<_>>(),"artifacts":artifacts}));
        }
        let last = opening(frames.last().ok_or("empty native plan")?)?;
        let (expected_native, old_git) = if let Some(g) = &last.git_acceptance {
            (
                Some(
                    StateId::from_bytes(g.expected_revision.as_slice().try_into()?)
                        .to_string_full(),
                ),
                Some(hex_bytes(&g.expected_git_commit)),
            )
        } else {
            (None, None)
        };
        let intent = normalize(
            json!({"schema":if stage.is_bootstrap(){2}else{1},"scope":self.config.scope,"actor":stage.actor(),"gateway_signer":hex_bytes(self.client.signer().public_key()),"billing_owner":h.billing_owner_account_id,"expected_catalog":expected_catalog,"expected_native":expected_native,"expected_generation":if stage.is_bootstrap(){None}else{Some(h.source_generation)},"old_git":old_git,"new_git":projected.history().git_oid(&hydrated.tip).ok_or("tip Git identity absent")?.to_string(),"native_state":hydrated.tip.to_string_full(),"authority_generation":self.generation(h)?,"history":history}),
        )?;
        if canonical(&intent)?.len() > 60 * 1024 {
            return Err("catalog intent metadata budget".into());
        }
        Ok(intent)
    }
    fn envelope(
        &self,
        stage: &StagedPublication,
        h: &GitHistoryAuthorizeResponse,
        actor: &VerifiedActor,
        expected_catalog: Option<&str>,
    ) -> Result<Frame> {
        let intent = self.derive_intent(stage, h, actor, expected_catalog)?;
        let mut parts = BTreeMap::new();
        parts.insert("proof".into(), stage.export_proof()?);
        let mut artifacts = Vec::new();
        for artifact in stage.export_artifacts()? {
            let name = format!("artifact/{}", artifact.sha256);
            match parts.entry(name.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(artifact.bytes);
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    if entry.get() != &artifact.bytes {
                        return Err("conflicting artifact bytes".into());
                    }
                }
            }
            artifacts.push(json!({"sha256":artifact.sha256,"bytes_part":name}));
        }
        Ok(Frame {
            payload: json!({"intent":intent,"proof_part":"proof","artifacts":artifacts,"recipe":RECEIVED_GIT_RECIPE}),
            parts,
        })
    }

    fn restore(
        &self,
        input: &Value,
        parts: &mut BTreeMap<String, Vec<u8>>,
        proof: &[u8],
    ) -> Result<RestoredPublication> {
        let _trace = Span::new("restore_native_artifacts");
        let values = input["artifacts"]
            .as_array()
            .ok_or("native artifacts absent")?;
        if values.len() > 256 {
            return Err("native artifact count".into());
        }
        let mut artifacts = Vec::new();
        let mut total = 0usize;
        for v in values {
            let data = take_part(v, "bytes_part", 64 * 1024 * 1024, parts)?;
            total = total.checked_add(data.len()).ok_or("artifact overflow")?;
            if total > 64 * 1024 * 1024 {
                return Err("native artifact budget".into());
            }
            artifacts.push(ExportArtifact {
                sha256: text(v, "sha256")?.into(),
                bytes: data,
            });
        }
        count("native_artifact_bytes", total);
        count("native_artifact_count", artifacts.len());
        let directory = tempfile::tempdir_in(&self.config.scratch)?;
        let stage = StagedPublication::import(directory.path(), proof, &artifacts)?;
        if stage.scope()? != self.scope(true)? {
            return Err("native stage scope differs".into());
        }
        Ok(RestoredPublication {
            _directory: directory,
            stage,
        })
    }
    fn exact_intent(&self, intent: &Value) -> Result<Value> {
        let intent = normalize(intent.clone())?;
        if intent["scope"] != serde_json::to_value(&self.config.scope)?
            || intent["gateway_signer"] != hex_bytes(self.client.signer().public_key())
        {
            return Err("native intent scope differs".into());
        }
        Ok(intent)
    }
    fn current_for(
        &self,
        intent: &Value,
        header: &str,
        write: bool,
        proof: Option<&[u8]>,
        require_receipt: bool,
    ) -> Result<GitHistoryAuthorizeResponse> {
        if intent["schema"] == 2 || intent["schema"] == 3 {
            let current = self.history(header, write, state(intent, "native_state")?)?;
            if intent["schema"] == 3
                && intent["expected_generation"].as_i64() != Some(current.source_generation)
            {
                return Err("native refresh generation changed".into());
            }
            return Ok(current);
        }
        if !require_receipt
            && let Ok(current) = self.history(header, write, state(intent, "expected_native")?)
        {
            if intent["expected_generation"].as_i64() != Some(current.source_generation) {
                return Err("expected native generation changed".into());
            }
            return Ok(current);
        }
        let response = self.accepted_audit_for(
            intent,
            header,
            write,
            proof.ok_or("recovery needs exact native proof")?,
        )?;
        require_current_acceptance(&response)?;
        Ok(response)
    }
    // Explicit immutable audit lookup. A receipt can remain valid after another
    // native writer advances; only reconciliation may use that non-current result.
    fn accepted_audit_for(
        &self,
        intent: &Value,
        header: &str,
        write: bool,
        proof: &[u8],
    ) -> Result<GitHistoryAuthorizeResponse> {
        let plan = StagedPublication::inspect_proof(proof)?;
        if plan.scope != self.scope(true)? || plan.actor != text(intent, "actor")? || plan.bootstrap
        {
            return Err("native recovery proof scope differs".into());
        }
        let receipt = self.authority.history(
            header,
            &self.scope(write)?,
            Some(
                &plan
                    .openings
                    .last()
                    .ok_or("native command absent")?
                    .client_operation_id,
            ),
            None,
        )?;
        let actual = receipt
            .receipt
            .as_ref()
            .and_then(|r| r.git_acceptance.as_ref())
            .ok_or("real native acceptance absent")?;
        let open = opening(plan.openings.last().ok_or("native command absent")?)?;
        let command = open.git_acceptance.as_ref().ok_or("Git command absent")?;
        if intent["expected_generation"].as_i64() != Some(actual.previous_native_generation)
            || actual.request_digest != unhex::<32>(&plan.request_digest)?
            || actual.actor_account_id != plan.actor
            || actual.gateway_publisher != self.client.signer().public_key()
            || actual.expected_revision != state(intent, "expected_native")?.as_bytes()
            || hex_bytes(&actual.expected_git_commit) != text(intent, "old_git")?
            || hex_bytes(&actual.accepted_git_commit) != text(intent, "new_git")?
            || command.accepted_git_commit != actual.accepted_git_commit
            || revision(
                receipt
                    .receipt
                    .as_ref()
                    .and_then(|r| r.revision.as_ref())
                    .ok_or("accepted revision absent")?,
            )? != state(intent, "native_state")?
        {
            return Err("accepted native command differs".into());
        }
        Ok(receipt)
    }
    fn dimensions(
        &self,
        _intent: &Value,
        actor: &VerifiedActor,
        h: &GitHistoryAuthorizeResponse,
    ) -> Result<Value> {
        let head = if let Some(b) = &h.bootstrap {
            revision(b.head.as_ref().ok_or("bootstrap head absent")?)?
        } else {
            revision(
                h.receipt
                    .as_ref()
                    .and_then(|r| r.revision.as_ref())
                    .ok_or("accepted head absent")?,
            )?
        };
        Ok(
            json!({"scope":self.config.scope,"actor":actor.actor(),"gateway_signer":hex_bytes(self.client.signer().public_key()),"billing_owner":h.billing_owner_account_id,"authority_generation":self.generation(h)?,"native_state":head.to_string_full(),"native_generation":h.source_generation}),
        )
    }
    fn receipt(&self, intent: &Value, h: &GitHistoryAuthorizeResponse) -> Result<Value> {
        if self.generation(h)? != text(intent, "authority_generation")?
            || h.billing_owner_account_id != text(intent, "billing_owner")?
        {
            return Err("publication authority changed".into());
        }
        normalized_receipt(intent, h.source_generation)
    }
    fn reconcile_inspect(
        &self,
        input: &Value,
        header: &str,
        parts: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<Frame> {
        let intent = self.exact_intent(&input["intent"])?;
        if intent["schema"] != 1 || operation(&intent)? != text(input, "operation")? {
            return Err("only an exact pending Git push can be reconciled".into());
        }
        let actor = self.authority.authorize(header, &self.scope(true)?)?;
        if actor.actor() != text(&intent, "actor")? {
            return Err("reconciliation requires the original writer".into());
        }
        let proof = take_part(input, "proof_part", 17 * 1024 * 1024, parts)?;
        match input.get("outcome") {
            Some(Value::String(kind)) if kind == "unresolved-superseded" => {
                return self.reconcile_unresolved(input, header, &intent, &proof, &actor);
            }
            None => {}
            Some(Value::String(kind)) if kind == "accepted-superseded" => {}
            _ => return Err("explicit reconciliation outcome invalid".into()),
        }
        // This selector retrieves actual durable acceptance, not a claim from the
        // journal. It also rechecks disclosure of the complete accepted closure.
        let accepted = self.accepted_audit_for(&intent, header, true, &proof)?;
        let actual = accepted
            .receipt
            .as_ref()
            .and_then(|r| r.git_acceptance.as_ref())
            .ok_or("durable Git receipt absent")?;
        let current_head = state(input, "expected_native")?;
        let current = self.history(header, true, current_head)?;
        reconcile_fence(
            &accepted,
            &current,
            input["expected_generation"].as_i64(),
            actual.native_generation,
        )?;
        let receipt = normalized_receipt(&intent, actual.native_generation)?;
        if input
            .get("receipt")
            .is_some_and(|v| !v.is_null() && *v != receipt)
        {
            return Err("pending native receipt differs".into());
        }
        let operation = operation(&intent)?;
        let manifest = canonical(
            &json!({"schema":2,"operation":operation,"intent":intent,"native_receipt":receipt}),
        )?;
        let pending =
            CatalogCommit::prepare(text(&intent, "expected_catalog")?, &operation, &manifest)?;
        let expected_catalog = text(input, "expected_catalog")?;
        if !hex(expected_catalog, 40) {
            return Err("canonical observed catalog pin required".into());
        }
        let credential = String::from_utf8(read(&self.config.artifacts_credential, 512)?)?;
        #[cfg(feature = "gateway-fixture")]
        let artifacts = match &self.fixture_ca {
            Some(ca) => ArtifactsGit::new_fixture(&self.config.artifacts_url, &credential, ca)?,
            None => ArtifactsGit::new(&self.config.artifacts_url, &credential)?,
        };
        #[cfg(not(feature = "gateway-fixture"))]
        let artifacts = ArtifactsGit::new(&self.config.artifacts_url, &credential)?;
        let catalog_pin = artifacts.head()?;
        if catalog_pin != expected_catalog
            || (catalog_pin != pending.expected && catalog_pin != pending.pin)
        {
            return Err("catalog changed or has an unrelated publication".into());
        }
        // A receipt remains immutable audit evidence after advancement. Current
        // source/disclosure snapshots must remain identical across inspection.
        self.require_same(&current, &self.history(header, true, current_head)?)?;
        self.require_same(
            &accepted,
            &self.accepted_audit_for(&intent, header, true, &proof)?,
        )?;
        if artifacts.head()? != catalog_pin {
            return Err("catalog changed during reconciliation".into());
        }
        self.remaining()?;
        Ok(Frame::json(json!({
            "kind":"accepted-superseded", "operation":operation, "receipt":receipt,
            "catalog_pin":catalog_pin, "pending_catalog_pin":pending.pin,
            "current_native":current_head.to_string_full(), "current_generation":current.source_generation,
            "actor":actor.actor(), "scope":self.config.scope
        })))
    }
    fn reconcile_unresolved(
        &self,
        input: &Value,
        header: &str,
        intent: &Value,
        proof: &[u8],
        actor: &VerifiedActor,
    ) -> Result<Frame> {
        if input.get("receipt").is_some_and(|v| !v.is_null()) {
            return Err("known acceptance cannot be relabeled unknown".into());
        }
        let plan = StagedPublication::inspect_proof(proof)?;
        let open = opening(plan.openings.last().ok_or("native command absent")?)?;
        let command = open.git_acceptance.as_ref().ok_or("Git command absent")?;
        if plan.bootstrap
            || plan.scope != self.scope(true)?
            || plan.actor != actor.actor()
            || command.expected_native_generation != intent["expected_generation"].as_i64()
            || command.expected_revision != state(intent, "expected_native")?.as_bytes()
            || hex_bytes(&command.expected_git_commit) != text(intent, "old_git")?
            || hex_bytes(&command.accepted_git_commit) != text(intent, "new_git")?
            || command.gateway_publisher != self.client.signer().public_key()
            || revision(open.revision.as_ref().ok_or("candidate revision absent")?)?
                != state(intent, "native_state")?
        {
            return Err("unresolved native proof differs from pending command".into());
        }
        let current_head = state(input, "expected_native")?;
        let current = self.history(header, true, current_head)?;
        let old_generation = intent["expected_generation"]
            .as_i64()
            .ok_or("pending generation absent")?;
        if old_generation < 0
            || current.source_generation <= old_generation
            || input["expected_generation"].as_i64() != Some(current.source_generation)
        {
            return Err(
                "unresolved reconciliation requires a strictly advanced native fence".into(),
            );
        }
        let expected = text(intent, "expected_catalog")?;
        if !hex(expected, 40) || text(input, "expected_catalog")? != expected {
            return Err("unknown acceptance permits only the exact prior catalog".into());
        }
        let credential = String::from_utf8(read(&self.config.artifacts_credential, 512)?)?;
        #[cfg(feature = "gateway-fixture")]
        let artifacts = match &self.fixture_ca {
            Some(ca) => ArtifactsGit::new_fixture(&self.config.artifacts_url, &credential, ca)?,
            None => ArtifactsGit::new(&self.config.artifacts_url, &credential)?,
        };
        #[cfg(not(feature = "gateway-fixture"))]
        let artifacts = ArtifactsGit::new(&self.config.artifacts_url, &credential)?;
        if artifacts.head()? != expected {
            return Err(
                "unresolved reconciliation cannot discard another catalog publication".into(),
            );
        }
        self.require_same(&current, &self.history(header, true, current_head)?)?;
        if artifacts.head()? != expected {
            return Err("catalog changed during unresolved reconciliation".into());
        }
        self.remaining()?;
        Ok(Frame::json(
            json!({"kind":"unresolved-superseded","operation":operation(intent)?,
            "receipt":null,"catalog_pin":expected,"pending_catalog_pin":null,
            "current_native":current_head.to_string_full(),"current_generation":current.source_generation,
            "actor":actor.actor(),"scope":self.config.scope}),
        ))
    }
    fn dispatch(&self, frame: Frame, request: &mut transport::Request) -> Result<Frame> {
        let Frame {
            payload: input,
            mut parts,
        } = frame;
        self.remaining()?;
        let header = request
            .header("authorization")
            .ok_or("Git session absent")?
            .to_string();
        let method = text(&input, "method")?;
        if matches!(method, "bootstrap-plan" | "refresh-plan" | "prepare") {
            return self.prepared(
                &input,
                &header,
                method != "prepare",
                method == "refresh-plan",
                &mut parts,
            );
        }
        if method == "authorize" && input.get("intent").is_none() {
            self.same_repo(&input)?;
            let write = match text(&input, "action")? {
                "read" => false,
                "write" => true,
                _ => return Err("unknown Git action".into()),
            };
            self.authority.authorize(&header, &self.scope(write)?)?;
            return Ok(Frame::json(json!({"authorized":true})));
        }
        if method == "reconcile-inspect" {
            return self.reconcile_inspect(&input, &header, &mut parts);
        }
        if method == "catalog-publish" {
            self.same_repo(&input)?;
            let manifest = take_part(&input, "manifest_part", 64 * 1024, &mut parts)?;
            let v: Value = serde_json::from_slice(&manifest)?;
            if canonical(&v)? != manifest {
                return Err("canonical catalog required".into());
            }
            let intent = self.exact_intent(&v["intent"])?;
            let actor = self.authority.authorize(&header, &self.scope(true)?)?;
            if actor.actor() != text(&intent, "actor")? {
                return Err("publication writer differs".into());
            }
            let proof = take_part(&input, "proof_part", 17 * 1024 * 1024, &mut parts)?;
            let current = self.current_for(&intent, &header, true, Some(&proof), true)?;
            if self.receipt(&intent, &current)? != v["native_receipt"]
                || operation(&intent)? != text(&input, "operation")?
            {
                return Err("catalog lacks exact current native receipt".into());
            }
            let expected = if intent["schema"] == 2 {
                "0000000000000000000000000000000000000000"
            } else {
                text(&intent, "expected_catalog")?
            };
            if text(&input, "expected")? != expected {
                return Err("catalog fence differs".into());
            }
            let stage = self.restore(&input, &mut parts, &proof)?;
            let mut reconstructed = self.derive_intent(
                &stage,
                &current,
                &actor,
                intent["expected_catalog"].as_str(),
            )?;
            reconstruct_fences(&mut reconstructed, &intent)?;
            if reconstructed != intent {
                return Err("catalog source inventory differs from native proof".into());
            }
            let commit = CatalogCommit::prepare(expected, text(&input, "operation")?, &manifest)?;
            let credential = String::from_utf8(read(&self.config.artifacts_credential, 512)?)?;
            {
                let _trace = Span::new("artifacts_catalog_cas");
                #[cfg(feature = "gateway-fixture")]
                let artifacts = match &self.fixture_ca {
                    Some(ca) => {
                        ArtifactsGit::new_fixture(&self.config.artifacts_url, &credential, ca)?
                    }
                    None => ArtifactsGit::new(&self.config.artifacts_url, &credential)?,
                };
                #[cfg(not(feature = "gateway-fixture"))]
                let artifacts = ArtifactsGit::new(&self.config.artifacts_url, &credential)?;
                artifacts.publish(&commit)?;
            }
            self.require_same(
                &current,
                &self.current_for(&intent, &header, true, Some(&proof), true)?,
            )?;
            return Ok(Frame::json(json!({"pin":commit.pin})));
        }
        let supplied = input
            .get("intent")
            .or_else(|| input.get("published").and_then(|v| v.get("intent")))
            .ok_or("native intent absent")?;
        let intent = self.exact_intent(supplied)?;
        let write = method_write(method, &input)?;
        let actor = self.authority.authorize(&header, &self.scope(write)?)?;
        if write && actor.actor() != text(&intent, "actor")? {
            return Err("writer differs from pending command".into());
        }
        let proof = input
            .get("proof_part")
            .map(|_| take_part(&input, "proof_part", 17 * 1024 * 1024, &mut parts))
            .transpose()?;
        let has_receipt = input.get("receipt").is_some_and(|v| !v.is_null()) || method == "project";
        let before = self.current_for(&intent, &header, write, proof.as_deref(), has_receipt)?;
        if self.generation(&before)? != text(&intent, "authority_generation")?
            || before.billing_owner_account_id != text(&intent, "billing_owner")?
        {
            return Err("current authority differs".into());
        }
        if method == "authorize" {
            return self.dimensions(&intent, &actor, &before).map(Frame::json);
        }
        let stage = self.restore(
            &input,
            &mut parts,
            proof.as_deref().ok_or("native proof absent")?,
        )?;
        let hydrated = stage.hydrate(h32(&before.spool_genesis)?)?;
        let projected = {
            let _trace = Span::new("git_reconstruction");
            hydrated.project()?
        };
        if projected
            .history()
            .git_oid(&hydrated.tip)
            .ok_or("projected tip absent")?
            .to_string()
            != text(&intent, "new_git")?
            || hydrated.tip != state(&intent, "native_state")?
        {
            return Err("intent Git/native closure differs".into());
        }
        // Reconstruct structural intent from verified native files. Old generation is
        // separately fenced by live authority, and later retained by native receipt.
        let mut reconstructed = self.intent_from_projection(
            &stage,
            &before,
            intent["expected_catalog"].as_str(),
            &hydrated,
            &projected,
        )?;
        reconstruct_fences(&mut reconstructed, &intent)?;
        if reconstructed != intent {
            return Err("native proof differs from complete intent".into());
        }
        match method {
            "validate-plan" => {
                self.require_same(
                    &before,
                    &self.current_for(&intent, &header, write, proof.as_deref(), has_receipt)?,
                )?;
                Ok(Frame::json(json!({"valid":true})))
            }
            "bootstrap" | "refresh" => {
                if (method == "refresh") != (intent["schema"] == 3) {
                    return Err("native publication kind differs".into());
                }
                if !stage.is_bootstrap() {
                    return Err("push is not bootstrap".into());
                }
                self.receipt(&intent, &before).map(Frame::json)
            }
            "submit" => {
                if stage.is_bootstrap() {
                    return Err("bootstrap is not a push".into());
                }
                let _trace = Span::new("receiver_native_acceptance_cas");
                let remote = self.wait(self.client.hosted().native())?;
                let native = self.wait(stage.submit(&remote, &session(&header)?))?;
                let after = self.current_for(&intent, &header, true, proof.as_deref(), true)?;
                if after.receipt.as_ref() != Some(&native) {
                    return Err("accepted receipt changed".into());
                }
                self.receipt(&intent, &after).map(Frame::json)
            }
            "project" => {
                let git = input.get("git").ok_or("Git read request absent")?;
                let body = take_part(git, "request_part", transport::MAX_REQUEST, &mut parts)?;
                let mut command = Command::new("git");
                command.arg("--git-dir").arg(projected.git_dir()).args([
                    "update-ref",
                    &format!("refs/heads/{}", self.config.scope.thread),
                    text(&intent, "new_git")?,
                ]);
                if !transport::run_bounded(&mut command, &[], tempfile::tempfile()?)?.success() {
                    return Err("projection ref failed".into());
                }
                let mut command = Command::new("git");
                command.arg("--git-dir").arg(projected.git_dir()).args([
                    "symbolic-ref",
                    "HEAD",
                    &format!("refs/heads/{}", self.config.scope.thread),
                ]);
                if !transport::run_bounded(&mut command, &[], tempfile::tempfile()?)?.success() {
                    return Err("projection HEAD failed".into());
                }
                let mut headers = vec![(
                    "content-type".into(),
                    "application/x-git-upload-pack-request".into(),
                )];
                if let Some(p) = git["protocol"].as_str() {
                    if !matches!(p, "version=1" | "version=2") {
                        return Err("Git protocol refused".into());
                    }
                    headers.push(("git-protocol".into(), p.into()));
                }
                let proxy = transport::Request {
                    stream: request.stream.try_clone()?,
                    method: text(git, "method")?.into(),
                    pin: self.config.scope.repository.clone(),
                    endpoint: text(git, "endpoint")?.into(),
                    query: text(git, "query")?.into(),
                    headers,
                    body,
                };
                let result = {
                    let _trace = Span::new("git_protocol");
                    transport::prepare_git(&proxy, projected.git_dir())?.into_bytes()?
                };
                count("git_response_bytes", result.2.len());
                self.require_same(
                    &before,
                    &self.current_for(&intent, &header, false, proof.as_deref(), true)?,
                )?;
                Ok(Frame {
                    payload: json!({"status":result.0,"content_type":result.1,"output_part":"output"}),
                    parts: BTreeMap::from([("output".into(), result.2)]),
                })
            }
            _ => Err("unknown native method".into()),
        }
    }
}
// Preserve the generation of durable acceptance rather than replacing it with
// a later native head generation during explicit reconciliation.
fn normalized_receipt(intent: &Value, generation: i64) -> Result<Value> {
    if !(0..=9_007_199_254_740_991).contains(&generation) {
        return Err("native receipt generation invalid".into());
    }
    let mut result = json!({"schema":1,"operation":operation(intent)?,"native_state":text(intent,"native_state")?,"generation":generation,"authority_generation":text(intent,"authority_generation")?,"history_sha256":digest(&canonical(&intent["history"])?),"actor":text(intent,"actor")?,"gateway_signer":text(intent,"gateway_signer")?,"billing_owner":text(intent,"billing_owner")?});
    if intent["schema"] == 2 {
        result["kind"] = json!("native-bootstrap");
    } else if intent["schema"] == 3 {
        result["kind"] = json!("native-refresh");
    } else {
        result["previous_native"] = intent["expected_native"].clone();
    }
    Ok(result)
}
fn require_current_acceptance(response: &GitHistoryAuthorizeResponse) -> Result<()> {
    let generation = response
        .receipt
        .as_ref()
        .and_then(|r| r.git_acceptance.as_ref())
        .ok_or("durable Git acceptance absent")?
        .native_generation;
    if generation < 0 || response.source_generation != generation {
        return Err(
            "native source advanced after Git acceptance; explicit reconciliation required".into(),
        );
    }
    Ok(())
}
fn reconcile_fence(
    accepted: &GitHistoryAuthorizeResponse,
    current: &GitHistoryAuthorizeResponse,
    observed: Option<i64>,
    acceptance_generation: i64,
) -> Result<()> {
    if observed != Some(current.source_generation)
        || acceptance_generation < 0
        || current.source_generation <= acceptance_generation
        || accepted.source_generation != current.source_generation
        || accepted.authorization_epoch != current.authorization_epoch
        || accepted.billing_owner_account_id != current.billing_owner_account_id
        || accepted.spool_genesis != current.spool_genesis
        || accepted.sharing_policy_version != current.sharing_policy_version
    {
        return Err(
            "reconciliation needs one current advanced source and authority snapshot".into(),
        );
    }
    Ok(())
}

fn method_write(method: &str, input: &Value) -> Result<bool> {
    Ok(match method {
        "project" => false,
        "authorize" | "validate-plan" => {
            match input.get("mode").and_then(Value::as_str).unwrap_or("write") {
                "read" => false,
                "write" => true,
                _ => return Err("unknown authority mode".into()),
            }
        }
        "submit" | "bootstrap" | "refresh" => true,
        _ => return Err("unknown native method".into()),
    })
}

fn opening(frame: &PublishContentClientFrame) -> Result<&PublishContentOpen> {
    match &frame.body {
        Some(publish_content_client_frame::Body::Open(o)) => Ok(o),
        _ => Err("publication opening absent".into()),
    }
}
fn directory_bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut todo = vec![path.to_path_buf()];
    while let Some(p) = todo.pop() {
        for e in std::fs::read_dir(p)? {
            let e = e?;
            let m = std::fs::symlink_metadata(e.path())?;
            if m.file_type().is_symlink() {
                return Err("native cache symlink refused".into());
            }
            if m.is_dir() {
                todo.push(e.path())
            } else {
                total = total.checked_add(m.len()).ok_or("native cache overflow")?;
                if total > 128 * 1024 * 1024 {
                    return Err("native cache budget".into());
                }
            }
        }
    }
    Ok(total)
}
fn normalize(mut intent: Value) -> Result<Value> {
    let history = intent["history"]
        .as_array_mut()
        .ok_or("native history absent")?;
    if history.is_empty() || history.len() > 128 {
        return Err("bounded native history required".into());
    }
    for item in history.iter_mut() {
        item["artifacts"]
            .as_array_mut()
            .ok_or("artifacts absent")?
            .sort_by_key(|v| v["sha256"].as_str().unwrap_or_default().to_string());
    }
    let mut pending = std::mem::take(history);
    let mut emitted = HashSet::<String>::new();
    while !pending.is_empty() {
        let mut ready = pending
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v["parents"].as_array().is_some_and(|a| {
                    a.iter()
                        .all(|p| p.as_str().is_some_and(|s| emitted.contains(s)))
                })
            })
            .map(|(i, v)| Ok((text(v, "state")?.to_string(), i)))
            .collect::<Result<Vec<_>>>()?;
        ready.sort();
        let (_, i) = ready.first().ok_or("incomplete or cyclic history")?;
        let item = pending.remove(*i);
        if !emitted.insert(text(&item, "state")?.to_string()) {
            return Err("duplicate history".into());
        }
        history.push(item);
    }
    if canonical(&intent)?.len() > 60 * 1024 {
        return Err("catalog intent metadata budget".into());
    }
    Ok(intent)
}
fn reconstruct_fences(reconstructed: &mut Value, intent: &Value) -> Result<()> {
    if intent["schema"] == 3 {
        if reconstructed["schema"] != 2 {
            return Err("refresh requires native-only proof".into());
        }
        reconstructed["schema"] = json!(3);
        reconstructed["expected_native"] = intent["expected_native"].clone();
        reconstructed["old_git"] = intent["old_git"].clone();
    }
    reconstructed["expected_generation"] = intent["expected_generation"].clone();
    Ok(())
}
fn operation(intent: &Value) -> Result<String> {
    Ok(digest(&canonical(
        &json!({"domain":if intent["schema"]==2{"heddle-native-bootstrap-publication-v1"}else if intent["schema"]==3{"heddle-native-refresh-publication-v1"}else{"heddle-hosted-git-publication-v1"},"intent":normalize(intent.clone())?}),
    )?))
}

pub fn run(path: &Path, bind: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    if unsafe { libc::geteuid() } == 0 {
        return Err("native hosted bridge requires non-root Linux".into());
    }
    let raw = read(path, 64 * 1024)?;
    let config: Config = serde_json::from_slice(&raw)?;
    if canonical(&config)? != raw
        || config.schema != 1
        || !config.scratch.is_absolute()
        || !hex(&config.service_sha256, 64)
    {
        return Err("canonical hosted configuration required".into());
    }
    config.scope.authority_scope()?;
    #[cfg(feature = "gateway-fixture")]
    let fixture_ca = fixture_ca(&config, bind)?;
    #[cfg(feature = "gateway-fixture")]
    let authority = match &fixture_ca {
        Some(ca) => SessionAuthority::new_fixture(&config.authority_origin, ca)?,
        None => SessionAuthority::new(&config.authority_origin, false)?,
    };
    #[cfg(not(feature = "gateway-fixture"))]
    let authority = SessionAuthority::new(&config.authority_origin, false)?;
    let source: SourceAuthor =
        serde_json::from_slice(&read(&config.gateway_source_author, 128 * 1024)?)?;
    let token = String::from_utf8(read(&config.gateway_biscuit, 64 * 1024)?)?;
    let pem = String::from_utf8(read(&config.gateway_signer_pem, 16 * 1024)?)?;
    let mut client_config = hosted_client::client::ClientConfig::new("heddle-scoped-git-gateway")
        .with_token(wire::AuthToken::new(token, &config.gateway_principal))
        .with_auth_proof_key_pem(pem)
        .with_authenticated_principal(&config.gateway_principal)
        .with_descriptor_trust(
            &config.descriptor_key_id,
            unhex::<32>(&config.descriptor_public_key)?,
        )
        .with_tls(false);
    #[cfg(feature = "gateway-fixture")]
    if let Some(ca) = &fixture_ca {
        client_config = client_config.with_tls_ca_certificate_pem(String::from_utf8(ca.clone())?);
        client_config = client_config.with_gateway_fixture_address(
            config
                .fixture
                .as_ref()
                .ok_or("native fixture absent")?
                .native_address,
        );
    }
    if let Some(path) = &config.gateway_mint_attachment {
        client_config.mint_root_attachment = Some(read(path, 128 * 1024)?);
    }
    std::fs::create_dir_all(&config.scratch)?;
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let client = executor.block_on(GatewayClient::connect(
        &config.weft_server,
        &client_config,
        source,
    ))?;
    let runtime = Runtime {
        config,
        client,
        authority,
        executor,
        deadline: Cell::new(None),
        #[cfg(feature = "gateway-fixture")]
        fixture_ca,
    };
    let address: SocketAddr = bind.parse()?;
    if !address.ip().is_loopback() && bind != "0.0.0.0:8080" {
        return Err("fixed Container or loopback listener required".into());
    }
    let listener = TcpListener::bind(address)?;
    let host = if address.ip().is_loopback() {
        listener.local_addr()?.to_string()
    } else {
        "native-container.invalid".into()
    };
    println!(
        "Native hosted Git bridge ready at {}",
        listener.local_addr()?
    );
    for stream in listener.incoming() {
        let stream = stream?;
        let fallback = stream.try_clone()?;
        let mut request =
            match transport::read_native_request(stream, &host, &runtime.config.service_sha256) {
                Ok(r) => r,
                Err(_) => {
                    let _ = transport::reply_stream(&mut { fallback }, 400, b"");
                    continue;
                }
            };
        let mut trace = Span::request();
        count("request_body_bytes", request.body.len());
        runtime
            .deadline
            .set(Some(Instant::now() + Duration::from_secs(120)));
        let result: Result<Frame> = (|| {
            let input = Frame::decode(std::mem::take(&mut request.body), 112 * 1024 * 1024)?;
            let result = runtime.dispatch(input, &mut request)?;
            runtime.remaining()?;
            Ok(result)
        })();
        trace.outcome(result.is_ok());
        match result {
            Ok(value) => {
                count(
                    "response_raw_part_bytes",
                    value.parts.values().map(Vec::len).sum(),
                );
                if let Err(_error) = native_frame::reply(&mut request, value) {
                    trace.outcome(false);
                    let _ = request.stream.shutdown(std::net::Shutdown::Both);
                }
            }
            Err(error) => {
                // This build-time fixture feature only accepts explicitly pinned
                // loopback services. Diagnostics are opt-in and never sent to Git.
                #[cfg(feature = "gateway-fixture")]
                if runtime.fixture_ca.is_some()
                    && std::env::var_os("HEDDLE_GATEWAY_SYNTHETIC_DIAGNOSTICS").is_some()
                {
                    let message = format!("{error:#}");
                    if !["ggit1_", "BEGIN ", "Bearer ", "Basic "]
                        .iter()
                        .any(|marker| message.contains(marker))
                    {
                        eprintln!(
                            "{}",
                            json!({"event":"synthetic_fixture_error", "message":message.chars().take(1024).collect::<String>()})
                        );
                    }
                }
                #[cfg(not(feature = "gateway-fixture"))]
                let _ = error;
                let _ = request.reply(503, b"");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_publication_never_relabels_an_old_receipt_as_current() {
        let mut h = GitHistoryAuthorizeResponse {
            source_generation: 6,
            receipt: Some(PublicationReceipt {
                git_acceptance: Some(GitPushAcceptanceReceipt {
                    native_generation: 6,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(require_current_acceptance(&h).is_ok());
        h.source_generation = 7;
        assert!(require_current_acceptance(&h).is_err());
        h.source_generation = 5;
        assert!(require_current_acceptance(&h).is_err());
        h.receipt = None;
        assert!(require_current_acceptance(&h).is_err());
    }
    #[test]
    fn reconciliation_requires_one_strictly_advanced_snapshot() {
        let before = GitHistoryAuthorizeResponse {
            source_generation: 7,
            authorization_epoch: 11,
            billing_owner_account_id: "owner".into(),
            spool_genesis: vec![1; 32],
            sharing_policy_version: vec![2; 32],
            ..Default::default()
        };
        assert!(reconcile_fence(&before, &before, Some(7), 6).is_ok());
        assert!(reconcile_fence(&before, &before, Some(7), 7).is_err());
        assert!(reconcile_fence(&before, &before, Some(7), -1).is_err());
        assert!(reconcile_fence(&before, &before, Some(6), 5).is_err());
        assert!(reconcile_fence(&before, &before, None, 5).is_err());
        for field in 0..5 {
            let mut changed = before.clone();
            match field {
                0 => changed.source_generation += 1,
                1 => changed.authorization_epoch += 1,
                2 => changed.billing_owner_account_id = "other".into(),
                3 => changed.spool_genesis[0] ^= 1,
                _ => changed.sharing_policy_version[0] ^= 1,
            }
            assert!(
                reconcile_fence(&before, &changed, Some(changed.source_generation), 5).is_err()
            );
        }
    }
    #[test]
    fn reconciliation_retains_original_acceptance_generation() {
        let intent = json!({"schema":1,"history":[{"state":"a","parents":[],"artifacts":[]}],
            "native_state":"a","authority_generation":"policy","actor":"writer",
            "gateway_signer":"signer","billing_owner":"owner","expected_native":"old"});
        let receipt = normalized_receipt(&intent, 3).unwrap();
        assert_eq!(receipt["generation"], 3);
        assert_eq!(receipt["previous_native"], "old");
        assert_eq!(receipt["operation"], operation(&intent).unwrap());
        assert!(normalized_receipt(&intent, -1).is_err());
        assert!(normalized_receipt(&intent, 9_007_199_254_740_992).is_err());
    }
    #[test]
    fn native_state_wire_identity_is_full_and_canonical() {
        let id = StateId::from_bytes([7; 32]);
        assert_eq!(
            state(&json!({"state":id.to_string_full()}), "state").unwrap(),
            id
        );
        assert!(state(&json!({"state":id.to_string()}), "state").is_err());
        assert!(
            state(
                &json!({"state":id.to_string_full().to_uppercase()}),
                "state"
            )
            .is_err()
        );
    }
    #[test]
    fn mutations_cannot_select_read_authority() {
        for mode in [Value::Null, json!("read"), json!("write"), json!(false)] {
            let input = json!({"mode":mode});
            assert!(method_write("submit", &input).unwrap());
            assert!(method_write("bootstrap", &input).unwrap());
            assert!(method_write("refresh", &input).unwrap());
            assert!(!method_write("project", &input).unwrap());
        }
        assert!(!method_write("authorize", &json!({"mode":"read"})).unwrap());
        assert!(method_write("validate-plan", &json!({"mode":"bogus"})).is_err());
        assert!(method_write("receive-pack", &json!({})).is_err());
    }
    #[test]
    fn operation_normalizes_transport_order_but_binds_ordered_parents() {
        let base = json!({"schema":1,"history":[
            {"state":"a","parents":[],"artifacts":[{"sha256":"b"},{"sha256":"a"}]},
            {"state":"b","parents":["a"],"artifacts":[]},
            {"state":"c","parents":["a"],"artifacts":[]},
            {"state":"d","parents":["b","c"],"artifacts":[]}]});
        let mut moved = base.clone();
        moved["history"].as_array_mut().unwrap().reverse();
        moved["history"][3]["artifacts"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert_eq!(operation(&base).unwrap(), operation(&moved).unwrap());
        moved["history"][0]["parents"] = json!(["c", "b"]);
        assert_ne!(operation(&base).unwrap(), operation(&moved).unwrap());
        assert!(
            normalize(json!({"history":[{"state":"a","parents":["b"],"artifacts":[]}]})).is_err()
        );
    }
}
