use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::*;
use crypto::Ed25519Signer;
use objects::{
    object::{ContentHash, StateId},
    store::{
        ObjectStore, WriterLeaseAuthOutcome, WriterLeaseDraft, WriterLeaseReserveOutcome,
        WriterLeaseStatus, WriterLeaseStore,
    },
};
use prost::Message;
use repo::thread_replication::{
    ThreadReplica,
    checkout::{CaptureInput, ThreadCheckout},
};
use serde::{Deserialize, Serialize};

use super::{DeviceRpc, auth::Session, read_bounded};

#[derive(Serialize, Deserialize)]
struct Command {
    method: String,
    actor: String,
    request: Vec<u8>,
    response: Option<Vec<u8>>,
    lease: Option<LeaseToken>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct LeaseToken {
    pub(super) id: String,
    pub(super) secret: String,
}

impl DeviceRpc {
    pub(super) fn checkout_command(
        &self,
        session: &Session,
        method: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        let descriptor = api::v2::method_descriptor(method).context("unknown checkout method")?;
        let operation = descriptor
            .client_operation_id(body)?
            .context("operation id required")?;
        uuid::Uuid::parse_str(operation)?;
        let directory = session.spool.heddle_dir.join("device-checkout-commands");
        objects::fs_atomic::create_private_dir_all(&directory)?;
        let path = directory.join(format!("{operation}.json"));
        let _command_lock =
            objects::lock::RepoLock::at(directory.join(format!("{operation}.lock")))
                .try_write()?
                .context("command already running")?;
        let mut command = if path.exists() {
            let command: Command = serde_json::from_slice(&read_bounded(&path, 1024 * 1024)?)?;
            if command.method != method || command.actor != session.actor || command.request != body
            {
                bail!("operation id reused with different inputs or actor");
            }
            if let Some(response) = command.response {
                return Ok(response);
            }
            command
        } else {
            let command = Command {
                method: method.into(),
                actor: session.actor.clone(),
                request: body.to_vec(),
                response: None,
                lease: None,
            };
            objects::fs_atomic::write_file_atomic_secret(&path, &serde_json::to_vec(&command)?)?;
            command
        };
        let response = match method.rsplit('/').next().context("method missing")? {
            "Materialize" => {
                let request = MaterializeCheckoutRequest::decode(body)?;
                let thread = thread(session, request.thread.as_ref())?;
                let replica = ThreadReplica::open(&session.spool.heddle_dir, thread)?;
                let revision = revision(session, request.revision.as_ref())?;
                let source = repo::Repository::open(&session.spool.root)?;
                let base = source.heddle_dir().join("device-checkouts");
                objects::fs_atomic::create_dir_all_durable(&base)?;
                let destination = match request.requested_path {
                    Some(path) => {
                        let path = std::path::PathBuf::from(path);
                        let parent = path
                            .parent()
                            .context("checkout parent missing")?
                            .canonicalize()?;
                        if !parent.starts_with(base.canonicalize()?) {
                            bail!(
                                "requested checkout must be under the device's managed checkout directory"
                            );
                        }
                        parent.join(path.file_name().context("checkout filename required")?)
                    }
                    None => base.join(operation),
                };
                let checkout = if destination.exists() {
                    let checkout = ThreadCheckout::open(&destination)?;
                    if checkout.binding.thread != thread
                        || checkout.repository.head()? != Some(revision)
                    {
                        bail!("materialization retry conflicts with destination");
                    }
                    checkout
                } else {
                    ThreadCheckout::create(
                        &source,
                        &replica,
                        &destination,
                        revision,
                        &local_audience(&source, revision)?,
                    )?
                };
                self.checkout_response(session, &checkout, operation, None)?
            }
            "ClaimCheckoutWriter" => {
                let request = ClaimCheckoutWriterRequest::decode(body)?;
                let checkout = self.checked_checkout(session, request.checkout.as_ref())?;
                self.check_version(session, &checkout, &request.expected_checkout_version)?;
                if request
                    .requested_duration
                    .as_ref()
                    .is_some_and(|d| d.seconds <= 0 || d.nanos != 0 || d.seconds > 300)
                {
                    bail!("writer duration must be at most 300 seconds");
                }
                let store = WriterLeaseStore::new(checkout.repository.heddle_dir());
                let token = if !request.current_token.is_empty() {
                    decode_token(&request.current_token)?
                } else {
                    if command.lease.is_none() {
                        command.lease = Some(LeaseToken {
                            id: objects::store::generate_writer_lease_id(),
                            secret: objects::store::generate_writer_lease_token(),
                        });
                        objects::fs_atomic::write_file_atomic_secret(
                            &path,
                            &serde_json::to_vec(&command)?,
                        )?;
                    }
                    command
                        .lease
                        .clone()
                        .context("prepared writer token missing")?
                };
                let lease = if request.current_token.is_empty() {
                    match store.reserve_prepared(
                        WriterLeaseDraft {
                            thread: checkout.binding.thread.to_hex(),
                            actor_session_id: Some(session.actor.clone()),
                            task_assignment_id: None,
                            anchor_state: checkout.repository.head()?.map(|s| s.to_string_full()),
                            anchor_root: None,
                            path: Some(checkout.repository.root().to_owned()),
                            pid: None,
                            boot_id: None,
                        },
                        token.id.clone(),
                        token.secret.clone(),
                        chrono::Utc::now(),
                    )? {
                        WriterLeaseReserveOutcome::Reserved(grant) => grant.lease,
                        WriterLeaseReserveOutcome::LiveOwner(_) => {
                            bail!("checkout has another active writer")
                        }
                    }
                } else {
                    verified_lease(&checkout, session, &token)?;
                    match store.authenticate_and_renew(
                        &token.id,
                        &token.secret,
                        chrono::Utc::now(),
                    )? {
                        WriterLeaseAuthOutcome::Authorized(lease) => lease,
                        _ => bail!("writer lease changed"),
                    }
                };
                ClaimCheckoutWriterResponse {
                    receipt: Some(self.receipt(operation)),
                    lease: Some(CheckoutWriterLease {
                        checkout: request.checkout,
                        token: serde_json::to_vec(&token)?,
                        actor_session_id: session.actor.clone(),
                        expires_at: Some(prost_types::Timestamp {
                            seconds: lease.lease_expires_at().timestamp(),
                            nanos: 0,
                        }),
                    }),
                }
                .encode_to_vec()
            }
            "ReleaseCheckoutWriter" => {
                let request = ReleaseCheckoutWriterRequest::decode(body)?;
                let checkout = self.checked_checkout(session, request.checkout.as_ref())?;
                let token = decode_token(&request.token)?;
                let store = WriterLeaseStore::new(checkout.repository.heddle_dir());
                let lease = store.load(&token.id)?.context("writer lease unavailable")?;
                if lease.actor_session_id.as_deref() != Some(&session.actor)
                    || lease.path.as_deref() != Some(checkout.repository.root())
                    || lease.thread != checkout.binding.thread.to_hex()
                {
                    bail!("writer belongs to another actor or checkout");
                }
                if lease.status == WriterLeaseStatus::Active {
                    if !matches!(
                        store.release(
                            &token.id,
                            &token.secret,
                            WriterLeaseStatus::Complete,
                            chrono::Utc::now()
                        )?,
                        WriterLeaseAuthOutcome::Authorized(_)
                    ) {
                        bail!("writer token invalid");
                    }
                } else if lease.token_hash
                    != blake3::hash(token.secret.as_bytes()).to_hex().to_string()
                {
                    bail!("writer token invalid");
                }
                MutationResponse {
                    receipt: Some(self.receipt(operation)),
                }
                .encode_to_vec()
            }
            "Capture" => {
                let request = CaptureCheckoutRequest::decode(body)?;
                let checkout = self.checked_checkout(session, request.checkout.as_ref())?;
                let expected = revision(session, request.expected_source.as_ref())?;
                let token = decode_token(&request.writer_lease_token)?;
                verified_lease(&checkout, session, &token)?;
                let replica =
                    ThreadReplica::open(checkout.repository.heddle_dir(), checkout.binding.thread)?;
                // The native capture journal checks immutable retry inputs before
                // source CAS, including recovery after capture committed.
                if checkout.repository.head()? == Some(expected) {
                    self.check_version(session, &checkout, &request.expected_checkout_version)?;
                }
                let signer = signer(&checkout.repository)?;
                let signed = checkout.capture_with_paths(
                    &replica,
                    CaptureInput {
                        lease: &token.id,
                        token: &token.secret,
                        operation_id: operation,
                        expected,
                        summary: &request.summary,
                        attribution: session.attribution.clone(),
                    },
                    &signer,
                    &request.selected_paths,
                )?;
                let state = signed
                    .verify()?
                    .source_state()?
                    .context("capture state missing")?;
                let capture = CaptureSummary {
                    revision: Some(wire_revision(session, state.id())),
                    thread: Some(wire_thread(session, checkout.binding.thread)),
                    summary: request.summary,
                    principal_id: session.principal.clone(),
                    captured_at: Some(prost_types::Timestamp {
                        seconds: state.created_at.timestamp(),
                        nanos: state.created_at.timestamp_subsec_nanos() as i32,
                    }),
                    ..Default::default()
                };
                self.checkout_response(session, &checkout, operation, Some(capture))?
            }
            "Refresh" => {
                let request = RefreshCheckoutRequest::decode(body)?;
                let checkout = self.checked_checkout(session, request.checkout.as_ref())?;
                self.move_checkout(
                    session,
                    &checkout,
                    &request.writer_lease_token,
                    &request.expected_checkout_version,
                    Some(revision(session, request.expected_source.as_ref())?),
                    revision(session, request.target.as_ref())?,
                    false,
                )?;
                self.checkout_response(session, &checkout, operation, None)?
            }
            "Recover" => {
                let request = RecoverCheckoutRequest::decode(body)?;
                let checkout = self.checked_checkout(session, request.checkout.as_ref())?;
                self.move_checkout(
                    session,
                    &checkout,
                    &request.writer_lease_token,
                    &request.expected_checkout_version,
                    None,
                    revision(session, request.recover_to.as_ref())?,
                    true,
                )?;
                self.checkout_response(session, &checkout, operation, None)?
            }
            "Resolve" => {
                let request = ResolveCheckoutRequest::decode(body)?;
                let checkout = self.checked_checkout(session, request.checkout.as_ref())?;
                let expected = revision(session, request.expected_source.as_ref())?;
                let token = decode_token(&request.writer_lease_token)?;
                verified_lease(&checkout, session, &token)?;
                let replica =
                    ThreadReplica::open(checkout.repository.heddle_dir(), checkout.binding.thread)?;
                let choices = request
                    .chosen_candidate_ids
                    .iter()
                    .map(|id| {
                        Ok(StateId::from_bytes(id.as_slice().try_into().map_err(
                            |_| anyhow::anyhow!("invalid source candidate"),
                        )?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                if choices.len() != 1 {
                    bail!("whole-source resolution selects exactly one observed candidate");
                }
                if checkout.repository.head()? == Some(expected) {
                    self.check_version(session, &checkout, &request.expected_checkout_version)?;
                }
                checkout.resolve_source_choice(
                    &replica,
                    &token.id,
                    &token.secret,
                    operation,
                    expected,
                    &request.conflict_set_version,
                    choices[0],
                    session.attribution.clone(),
                    &signer(&checkout.repository)?,
                )?;
                self.checkout_response(session, &checkout, operation, None)?
            }
            "LandCheckout" => self.land_checkout(session, LandCheckoutRequest::decode(body)?)?,
            _ => bail!("unknown checkout RPC"),
        };
        command.response = Some(response.clone());
        objects::fs_atomic::write_file_atomic_secret(&path, &serde_json::to_vec(&command)?)?;
        Ok(response)
    }
    fn move_checkout(
        &self,
        session: &Session,
        checkout: &ThreadCheckout,
        token: &[u8],
        version: &[u8],
        expected: Option<StateId>,
        target: StateId,
        recovery: bool,
    ) -> Result<()> {
        let token = decode_token(token)?;
        verified_lease(checkout, session, &token)?;
        let _writer = checkout.repository.authenticate_checkout_writer(
            checkout.binding.thread,
            &token.id,
            &token.secret,
        )?;
        let current = checkout
            .repository
            .head()?
            .context("checkout source missing")?;
        if !recovery && current == target {
            return Ok(());
        }
        self.check_version(session, checkout, version)?;
        if expected.is_some_and(|expected| expected != current) {
            bail!("checkout source changed");
        }
        let replica =
            ThreadReplica::open(checkout.repository.heddle_dir(), checkout.binding.thread)?;
        if target != replica.genesis()?.base && replica.accepted_source_revision(target)?.is_none()
        {
            bail!("target is not admitted in this Thread");
        }
        if recovery && checkout.repository.worktree_matches_state(&target)? {
            return Ok(());
        }
        if !checkout.repository.worktree_matches_state(&current)? {
            bail!("checkout has working edits; capture them before moving");
        }
        checkout
            .repository
            .restore_state_tree_to_worktree(&target)?;
        if !recovery {
            checkout
                .repository
                .write_head_recorded(&refs::Head::Detached { state: target })?;
        }
        Ok(())
    }
    pub(super) fn checked_checkout(
        &self,
        session: &Session,
        reference: Option<&CheckoutRef>,
    ) -> Result<ThreadCheckout> {
        let reference = reference.context("checkout required")?;
        same_spool(session, reference.spool.as_ref())?;
        if reference.device.as_ref() != Some(&self.endpoint()) {
            bail!("checkout targets another device");
        }
        repo::device_catalog::checkout(&session.spool, &reference.id)
    }
    pub(super) fn check_version(
        &self,
        session: &Session,
        checkout: &ThreadCheckout,
        expected: &[u8],
    ) -> Result<()> {
        if expected.is_empty() || self.checkout_overview(session, checkout)?.version != expected {
            bail!("checkout version changed");
        }
        Ok(())
    }
    pub(super) fn checkout_overview(
        &self,
        session: &Session,
        checkout: &ThreadCheckout,
    ) -> Result<CheckoutOverview> {
        let head = checkout
            .repository
            .head()?
            .context("checkout source missing")?;
        let state = checkout
            .repository
            .store()
            .get_state(&head)?
            .context("checkout State unavailable")?;
        let tree = checkout
            .repository
            .store()
            .get_tree(&state.tree)?
            .context("checkout tree unavailable")?;
        let changes = checkout
            .repository
            .compare_worktree_cached_detailed(&tree)?;
        let version = ContentHash::compute_typed(
            "heddle-device-checkout-v2",
            &[checkout.binding.id.as_bytes(), head.as_bytes()].concat(),
        )
        .as_bytes()
        .to_vec();
        let replica =
            ThreadReplica::open(checkout.repository.heddle_dir(), checkout.binding.thread)?;
        let heads = replica.view()?.source_heads;
        let policy_version = super::land::policy_version(&checkout.repository)?;
        Ok(CheckoutOverview {
            r#ref: Some(CheckoutRef {
                spool: Some(SpoolRef {
                    id: session.spool.id.to_string(),
                }),
                device: Some(self.endpoint()),
                id: checkout.binding.id.clone(),
            }),
            thread: Some(wire_thread(session, checkout.binding.thread)),
            version,
            materialized: Some(wire_revision(session, head)),
            actions: super::METHODS
                .iter()
                .filter(|method| {
                    method.contains("CheckoutService/")
                        && !method.ends_with("/ObserveCheckouts")
                        && !method.ends_with("/Materialize")
                })
                .map(|method| ActionAvailability {
                    method: (*method).into(),
                    endpoint: Some(self.endpoint()),
                    implemented: true,
                    authorized: session.permits(method),
                    observed_versions: if method.ends_with("/LandCheckout") {
                        vec![ExpectedVersion {
                            resource: Some(EntityRef {
                                entity: Some(entity_ref::Entity::Policy(RecordRef {
                                    spool: Some(SpoolRef {
                                        id: session.spool.id.to_string(),
                                    }),
                                    id: "device-local-integration-policy".into(),
                                })),
                            }),
                            version: policy_version.as_bytes().to_vec(),
                        }]
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                })
                .collect(),
            dirty: !changes.is_clean(),
            changed_paths: changes.change_count() as u64,
            display_path: checkout.repository.root().display().to_string(),
            conflicts: if heads.len() > 1 {
                Some(SourceConflictSet {
                    version: conflict_version(checkout.binding.thread, &heads),
                    candidates: heads
                        .into_iter()
                        .map(|state| SourceConflictCandidate {
                            id: state.as_bytes().to_vec(),
                            revision: Some(wire_revision(session, state)),
                        })
                        .collect(),
                })
            } else {
                None
            },
            ..Default::default()
        })
    }
    pub(super) fn checkout_response(
        &self,
        session: &Session,
        checkout: &ThreadCheckout,
        operation: &str,
        capture: Option<CaptureSummary>,
    ) -> Result<Vec<u8>> {
        Ok(CheckoutMutationResponse {
            receipt: Some(self.receipt(operation)),
            checkout: Some(self.checkout_overview(session, checkout)?),
            capture,
        }
        .encode_to_vec())
    }
}
pub(super) fn same_spool(session: &Session, spool: Option<&SpoolRef>) -> Result<()> {
    if spool.is_none_or(|spool| spool.id != session.spool.id.to_string()) {
        bail!("request targets another Spool");
    }
    Ok(())
}
pub(super) fn thread(session: &Session, reference: Option<&ThreadRef>) -> Result<ContentHash> {
    let reference = reference.context("Thread required")?;
    same_spool(session, reference.spool.as_ref())?;
    Ok(ContentHash::from_bytes(
        reference
            .id
            .as_ref()
            .context("Thread identity missing")?
            .value
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid Thread hash"))?,
    ))
}
pub(super) fn revision(session: &Session, reference: Option<&RevisionRef>) -> Result<StateId> {
    let reference = reference.context("source revision required")?;
    same_spool(session, reference.spool.as_ref())?;
    match reference.revision.as_ref() {
        Some(revision_ref::Revision::State(id)) => Ok(StateId::from_bytes(
            id.value
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid State id"))?,
        )),
        _ => bail!("checkout needs a native source revision"),
    }
}
fn wire_revision(session: &Session, state: StateId) -> RevisionRef {
    RevisionRef {
        spool: Some(SpoolRef {
            id: session.spool.id.to_string(),
        }),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::v1alpha1::StateId {
                value: state.as_bytes().to_vec(),
            },
        )),
    }
}
fn wire_thread(session: &Session, thread: ContentHash) -> ThreadRef {
    ThreadRef {
        spool: Some(SpoolRef {
            id: session.spool.id.to_string(),
        }),
        id: Some(ThreadId {
            value: thread.as_bytes().to_vec(),
        }),
    }
}
pub(super) fn decode_token(bytes: &[u8]) -> Result<LeaseToken> {
    if bytes.len() > 1024 {
        bail!("writer token exceeds bound");
    }
    Ok(serde_json::from_slice(bytes)?)
}
pub(super) fn verified_lease(
    checkout: &ThreadCheckout,
    session: &Session,
    token: &LeaseToken,
) -> Result<()> {
    let lease = WriterLeaseStore::new(checkout.repository.heddle_dir())
        .load(&token.id)?
        .context("writer unavailable")?;
    if lease.actor_session_id.as_deref() != Some(&session.actor)
        || lease.thread != checkout.binding.thread.to_hex()
        || lease.path.as_deref() != Some(checkout.repository.root())
        || lease.token_hash != blake3::hash(token.secret.as_bytes()).to_hex().to_string()
        || lease.liveness_at(chrono::Utc::now()) == objects::store::Liveness::Dead
    {
        bail!("active writer token belongs to another actor or checkout");
    }
    Ok(())
}
pub(super) fn signer(repository: &repo::Repository) -> Result<Ed25519Signer> {
    let pem = match repo::identity::load_device(&repo::identity::device_identity_path())? {
        Some(device) => device.private_key_pem,
        None => {
            repo::identity::load_or_mint_local(
                &repository
                    .heddle_dir()
                    .join(repo::identity::LOCAL_IDENTITY_FILE),
            )?
            .private_key_pem
        }
    };
    Ok(Ed25519Signer::from_pem(&pem)?)
}
fn conflict_version(thread: ContentHash, heads: &BTreeSet<StateId>) -> Vec<u8> {
    repo::thread_replication::source_conflict_version(thread, heads)
}

fn local_audience(repository: &repo::Repository, state: StateId) -> Result<repo::AudienceTier> {
    Ok(match repository.effective_visibility_tier(&state)? {
        objects::object::VisibilityTier::Private { scope_label }
        | objects::object::VisibilityTier::Restricted { scope_label } => {
            repo::AudienceTier::Restricted(scope_label)
        }
        objects::object::VisibilityTier::TeamScoped { team_id } => {
            repo::AudienceTier::Team(team_id)
        }
        _ => repo::AudienceTier::Internal,
    })
}
