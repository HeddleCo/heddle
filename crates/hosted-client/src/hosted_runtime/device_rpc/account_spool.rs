//! Local catalog mutations share exact durable receipts and transaction fences.
use anyhow::{Context, Result, bail, ensure};
use api::heddle::api::v2alpha1::*;
use prost::Message;
use repo::device_catalog::{
    DeviceSpool, mutations,
    store::{Catalog, insert_spool_in, spool_in},
};

use super::{DeviceRpc, account_auth::AccountSession, stream::ObservationAuthority};
impl DeviceRpc {
    pub(super) fn account_command(
        &self,
        session: &AccountSession,
        method: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        if method.ends_with("/IntrospectCredential") {
            return Ok(self
                .introspect(session, &IntrospectCredentialRequest::decode(body)?)?
                .encode_to_vec());
        }
        let mut catalog = Catalog::open(&self.home)?;
        self.authorize_catalog_command(session, &catalog, method, body)?;
        macro_rules! edit {
            ($request:expr,$operation:expr,$body:expr) => {{
                let request = $request;
                let operation = $operation(&request).to_owned();
                let response: MutationResponse =
                    catalog.mutate(&session.principal, method, &operation, body, |tx| {
                        session.check_current(&self.home)?;
                        let (resource, version) = $body(tx, &request)?;
                        session.check_current(&self.home)?;
                        Ok(MutationResponse {
                            receipt: Some(self.account_receipt(&operation, resource, version)),
                        })
                    })?;
                Ok(response.encode_to_vec())
            }};
        }
        match method.rsplit('/').next().context("method")? {
            "CreateSpool" => {
                let request = CreateSpoolRequest::decode(body)?;
                let response: SpoolMutationResponse = catalog.mutate(
                    &session.principal,
                    method,
                    &request.client_operation_id,
                    body,
                    |tx| self.create_local_spool(session, tx, &request),
                )?;
                let id = scope_id(
                    response
                        .spool
                        .as_ref()
                        .and_then(|spool| spool.r#ref.as_ref()),
                )?;
                // Repair only a live creation. A historical retry after deletion
                // returns its receipt without resurrecting the removed identity.
                if catalog.spool(id)?.is_some() {
                    let registration=repo::device_catalog::load(&self.home,id)?;
                    repo::Repository::open(&registration.root)?.seed_default_thread()?;
                }
                session.check_current(&self.home)?;
                Ok(response.encode_to_vec())
            }
            "ReviseSpool" => {
                let request = ReviseSpoolRequest::decode(body)?;
                let response: SpoolMutationResponse = catalog.mutate(
                    &session.principal,
                    method,
                    &request.client_operation_id,
                    body,
                    |tx| {
                        let id = scope_id(request.spool.as_ref())?;
                        self.require_catalog_scope(session, tx, id)?;
                        let overview = mutations::revise(
                            tx,
                            id,
                            &request.expected_version,
                            &request.name,
                            request
                                .settings
                                .as_ref()
                                .context("complete settings required")?,
                        )?;
                        session.check_current(&self.home)?;
                        Ok(SpoolMutationResponse {
                            receipt: Some(self.account_receipt(
                                &request.client_operation_id,
                                entity_ref::Entity::Spool(SpoolRef { id: id.to_string() }),
                                overview.version.clone(),
                            )),
                            spool: Some(overview),
                            ownership: None,
                        })
                    },
                )?;
                Ok(response.encode_to_vec())
            }
            "DeleteSpool" => edit!(
                DeleteSpoolRequest::decode(body)?,
                |r: &DeleteSpoolRequest| r.client_operation_id.clone(),
                |tx: &repo::device_catalog::store::CatalogTransaction<'_>,
                 r: &DeleteSpoolRequest|
                 -> Result<_> {
                    let id = scope_id(r.spool.as_ref())?;
                    self.require_catalog_scope(session, tx, id)?;
                    mutations::delete(tx, id, &r.expected_version)?;
                    let version = i64::from_be_bytes(
                        r.expected_version
                            .as_slice()
                            .try_into()
                            .context("Spool version")?,
                    )
                    .checked_add(1)
                    .context("version overflow")?
                    .to_be_bytes()
                    .to_vec();
                    Ok((
                        entity_ref::Entity::Spool(SpoolRef { id: id.to_string() }),
                        version,
                    ))
                }
            ),
            "SetBookmark" => edit!(
                SetBookmarkRequest::decode(body)?,
                |r: &SetBookmarkRequest| r.client_operation_id.clone(),
                |tx: &repo::device_catalog::store::CatalogTransaction<'_>,
                 r: &SetBookmarkRequest|
                 -> Result<_> {
                    if r.bookmarked {
                        self.require_catalog_scope(
                            session,
                            tx,
                            bookmark_spool(r.bookmark.as_ref())?,
                        )?;
                    }
                    let bookmark = mutations::bookmark(tx, &session.principal, r)?;
                    Ok((
                        entity_ref::Entity::Bookmark(bookmark.r#ref.context("bookmark reference")?),
                        bookmark.version,
                    ))
                }
            ),
            "SetSpoolMount" => edit!(
                SetSpoolMountRequest::decode(body)?,
                |r: &SetSpoolMountRequest| r.client_operation_id.clone(),
                |tx: &repo::device_catalog::store::CatalogTransaction<'_>,
                 r: &SetSpoolMountRequest|
                 -> Result<_> {
                    let mount = r.mount.as_ref().context("mount")?;
                    let parent = scope_id(mount.parent.as_ref())?;
                    self.require_catalog_scope(session, tx, parent)?;
                    self.require_catalog_scope(session, tx, scope_id(mount.child.as_ref())?)?;
                    if let Some(old) = mutations::mount_in(
                        tx,
                        uuid::Uuid::parse_str(&mount.r#ref.as_ref().context("mount ref")?.id)?,
                    )? {
                        self.require_catalog_scope(session, tx, scope_id(old.child.as_ref())?)?;
                    }
                    let changed = mutations::mount(tx, r)?;
                    Ok((
                        entity_ref::Entity::Mount(changed.r#ref.context("mount ref")?),
                        changed.version,
                    ))
                }
            ),
            "RemoveSpoolMount" => edit!(
                RemoveSpoolMountRequest::decode(body)?,
                |r: &RemoveSpoolMountRequest| r.client_operation_id.clone(),
                |tx: &repo::device_catalog::store::CatalogTransaction<'_>,
                 r: &RemoveSpoolMountRequest|
                 -> Result<_> {
                    let mount = r.mount.as_ref().context("mount")?;
                    self.require_catalog_scope(session, tx, scope_id(mount.parent.as_ref())?)?;
                    self.require_catalog_scope(session, tx, scope_id(mount.child.as_ref())?)?;
                    let changed = mutations::remove_mount(tx, r)?;
                    Ok((
                        entity_ref::Entity::Mount(changed.r#ref.context("mount ref")?),
                        changed.version,
                    ))
                }
            ),
            _ => bail!("unknown account mutation"),
        }
    }
    fn authorize_catalog_command(
        &self,
        session: &AccountSession,
        catalog: &Catalog,
        method: &str,
        body: &[u8],
    ) -> Result<()> {
        let mut scopes = Vec::new();
        match method.rsplit('/').next().context("method")? {
            "CreateSpool" => {
                let request = CreateSpoolRequest::decode(body)?;
                if let Some(parent) = request.parent {
                    scopes.push(scope_id(Some(&parent))?);
                } else {
                    session.facts(None)?;
                }
            }
            "ReviseSpool" => {
                scopes.push(scope_id(ReviseSpoolRequest::decode(body)?.spool.as_ref())?)
            }
            "DeleteSpool" => {
                scopes.push(scope_id(DeleteSpoolRequest::decode(body)?.spool.as_ref())?)
            }
            "SetBookmark" => scopes.push(bookmark_spool(
                SetBookmarkRequest::decode(body)?.bookmark.as_ref(),
            )?),
            "SetSpoolMount" | "RemoveSpoolMount" => {
                let mount = if method.ends_with("/SetSpoolMount") {
                    SetSpoolMountRequest::decode(body)?.mount
                } else {
                    RemoveSpoolMountRequest::decode(body)?.mount
                }
                .context("mount required")?;
                scopes.push(scope_id(mount.parent.as_ref())?);
                scopes.push(scope_id(mount.child.as_ref())?);
                if let Some(current) = catalog.current_mount(uuid::Uuid::parse_str(
                    &mount.r#ref.as_ref().context("mount ref")?.id,
                )?)? {
                    scopes.push(scope_id(current.child.as_ref())?);
                }
            }
            _ => bail!("unknown catalog command"),
        }
        // This runs before receipt lookup as well as fresh mutation. Returning
        // an old result cannot borrow a different resource's delegated scope.
        for id in scopes {
            let path = catalog
                .authorization_path(id)?
                .context("target Spool unavailable")?;
            session.facts(Some(&path))?;
        }
        session.check_current(&self.home)
    }
    fn create_local_spool(
        &self,
        session: &AccountSession,
        tx: &repo::device_catalog::store::CatalogTransaction<'_>,
        request: &CreateSpoolRequest,
    ) -> Result<SpoolMutationResponse> {
        session.check_current(&self.home)?;
        mutations::validate_name(&request.slug)?;
        let name = request.display_name.as_deref().unwrap_or(&request.slug);
        mutations::validate_name(name)?;
        let authority = session.owner(&self.home)?;
        let now = chrono::Utc::now().timestamp();
        let owner = repo::verify_account_owner_observation(&authority.owner, now)?;
        let Some(create_spool_request::Ownership::OwnerGenesis(genesis)) = &request.ownership
        else {
            bail!("local creation requires caller-signed owner genesis")
        };
        let verified = heddleco_capability_verifier::verify_spool_owner_genesis(genesis)?;
        ensure!(
            verified.owner_public_key() == owner.authority_key(),
            "new Spool requires current owner key"
        );
        let id = uuid::Uuid::from_bytes(verified.spool_uuid());
        ensure!(id.get_version_num() == 7, "fresh Spool UUIDv7 required");
        let parent = request
            .parent
            .as_ref()
            .map(|r| uuid::Uuid::parse_str(&r.id))
            .transpose()?;
        let parent_path = if let Some(parent) = parent {
            let record = self.require_catalog_scope(session, tx, parent)?;
            ensure!(
                record
                    .overview
                    .settings
                    .as_ref()
                    .is_some_and(|s| s.allow_child_creation),
                "parent does not permit child creation"
            );
            record.registration.capability_path
        } else {
            session.facts(None)?;
            String::new()
        };
        if let Some(proof) = &genesis.delegated_creation {
            let facts = heddleco_capability_verifier::creation::admit_fresh_spool_creation(
                genesis, &owner, now,
            )?;
            let statement = proof.statement.as_ref().context("creation statement")?;
            let issuer = proof
                .mint_root_attachment
                .as_ref()
                .and_then(|a| a.attachment.as_ref())
                .and_then(|a| a.mint_root_key.as_ref())
                .unwrap_or(owner.authority_key());
            authority.verify_mint_root(&issuer.public_key, now)?;
            ensure!(
                statement.account_uuid == uuid::Uuid::parse_str(&session.principal)?.as_bytes()
                    && statement.parent_spool_uuid
                        == parent.map(|p| p.as_bytes().to_vec()).unwrap_or_default()
                    && statement.parent_path_segments.join("/") == parent_path
                    && statement.name == request.slug
                    && facts.cnf.as_deref()
                        == Some(hex::encode(&session.inspected.proof_public_key).as_str())
                    && !facts
                        .revocation_ids
                        .iter()
                        .any(|id| authority.revoked_ids.contains(id)),
                "creation proof differs from admitted caller, parent or intent"
            );
        }
        let settings = request.settings.clone().unwrap_or(SpoolSettings {
            audience: Audience::Private as i32,
            default_state_audience: Audience::Private as i32,
            allow_child_creation: true,
            ..Default::default()
        });
        mutations::validate_settings(&settings)?;
        ensure!(
            settings.default_review_policy.is_none(),
            "default review policy must exist before selection"
        );
        let path = if parent_path.is_empty() {
            request.slug.clone()
        } else {
            format!("{parent_path}/{}", request.slug)
        };
        let root = self
            .home
            .join("state/device-rpc/repositories")
            .join(id.to_string());
        let registration = DeviceSpool {
            id,
            root: root.clone(),
            heddle_dir: root.join(".heddle"),
            capability_path: path.clone(),
        };
        insert_spool_in(
            tx,
            &registration,
            &SpoolOverview {
                r#ref: Some(SpoolRef { id: id.to_string() }),
                parent: request.parent.clone(),
                name: name.into(),
                slug: request.slug.clone(),
                audience: settings.audience,
                settings: Some(settings),
                owner_genesis: Some(genesis.clone()),
                ..Default::default()
            },
        )?;
        objects::fs_atomic::create_private_dir_all(&root)?;
        let repository = if root.join(".heddle/config.toml").try_exists()? {
            repo::Repository::open(&root)?
        } else {
            repo::Repository::init(&root)?
        };
        let id_path = repository.heddle_dir().join("spool-id");
        if id_path.try_exists()? {
            ensure!(
                std::fs::read_to_string(&id_path)?.trim() == id.to_string(),
                "physical Spool identity mismatch"
            )
        } else {
            objects::fs_atomic::write_file_atomic_secret(&id_path, id.to_string().as_bytes())?;
        }
        repository.verify_and_pin_owner_genesis(
            2,
            Some(genesis),
            &path.split('/').map(str::to_owned).collect::<Vec<_>>(),
        )?;
        let overview = spool_in(tx, id)?.context("created Spool")?.overview;
        ensure!(
            session.owner(&self.home)?.owner.version == authority.owner.version,
            "owner changed during creation"
        );
        session.check_current(&self.home)?;
        Ok(SpoolMutationResponse {
            receipt: Some(self.account_receipt(
                &request.client_operation_id,
                entity_ref::Entity::Spool(SpoolRef { id: id.to_string() }),
                overview.version.clone(),
            )),
            spool: Some(overview),
            ownership: Some(authority.owner),
        })
    }
    pub(super) fn require_catalog_scope(
        &self,
        session: &AccountSession,
        tx: &repo::device_catalog::store::CatalogTransaction<'_>,
        id: uuid::Uuid,
    ) -> Result<repo::device_catalog::store::SpoolRecord> {
        session.check_current(&self.home)?;
        let record = spool_in(tx, id)?.context("local Spool unavailable")?;
        session.facts(Some(&record.registration.capability_path))?;
        Ok(record)
    }
    fn account_receipt(
        &self,
        operation: &str,
        resource: entity_ref::Entity,
        version: Vec<u8>,
    ) -> MutationReceipt {
        let mut receipt = self.receipt(operation);
        receipt.outcome = Some(mutation_receipt::Outcome::Applied(Applied {
            resulting_versions: vec![ExpectedVersion {
                resource: Some(EntityRef {
                    entity: Some(resource),
                }),
                version,
            }],
        }));
        receipt
    }
}
pub(super) fn scope_id(reference: Option<&SpoolRef>) -> Result<uuid::Uuid> {
    let id = uuid::Uuid::parse_str(&reference.context("Spool reference required")?.id)?;
    ensure!(!id.is_nil(), "Spool identity required");
    Ok(id)
}

fn bookmark_spool(reference: Option<&BookmarkRef>) -> Result<uuid::Uuid> {
    let scope = match reference
        .context("bookmark reference required")?
        .target
        .as_ref()
    {
        Some(bookmark_ref::Target::Spool(spool)) => Some(spool),
        Some(bookmark_ref::Target::Thread(thread)) => thread.spool.as_ref(),
        None => None,
    };
    scope_id(scope)
}
