//! Shared account projections use post-commit pushes and exact local versions.
use anyhow::{Context, Result, bail, ensure};
use api::heddle::api::v2alpha1::*;
use prost::Message;

use super::{
    DeviceRpc, account_auth::AccountSession, account_feed::AccountFeed, account_spool::scope_id,
};
impl DeviceRpc {
    pub(super) async fn observe_account(
        &self,
        session: &AccountSession,
        method: &str,
        body: &[u8],
        send: iroh::endpoint::SendStream,
    ) -> Result<()> {
        let feed = self.account_feed()?;
        let authority = AccountObservation {
            session,
            home: &self.home,
            spools: std::sync::Mutex::new(Vec::new()),
        };
        macro_rules! observe {
            ($request:expr,$query:expr,$snapshot:expr) => {{
                let request = $request;
                let options = request.observe.clone().unwrap_or_default();
                let mut normalized = request.clone();
                normalized.observe = None;
                clear_pages(&mut normalized);
                let thread_query = $query(&request);
                self.observe_authorized_view(
                    &authority,
                    method,
                    &normalized.encode_to_vec(),
                    options,
                    send,
                    feed.changes.subscribe(),
                    |budget, binding| {
                        feed.refresh()?;
                        authority.bind()?;
                        let version =
                            self.account_version(&feed, session, thread_query.as_ref())?;
                        let (rows, page) = $snapshot(&request, budget, binding)?;
                        if self.account_version(&feed, session, thread_query.as_ref())? != version {
                            return Err(super::stream::SnapshotChanged.into());
                        }
                        Ok((rows, page, version))
                    },
                    || self.account_version(&feed, session, thread_query.as_ref()),
                )
                .await
            }};
        }
        match method.rsplit('/').next().context("method")? {
            "ObserveIdentity" => observe!(
                ObserveIdentityRequest::decode(body)?,
                |_: &ObserveIdentityRequest| None::<ThreadQuery>,
                |q: &ObserveIdentityRequest, _: &ReadBudget, _: &[u8]| -> Result<_> {
                    Ok((self.identity_snapshot(session, q)?, complete()))
                }
            ),
            "ObserveOwnership" => observe!(
                ObserveOwnershipRequest::decode(body)?,
                |_: &ObserveOwnershipRequest| None::<ThreadQuery>,
                |q: &ObserveOwnershipRequest, _: &ReadBudget, _: &[u8]| -> Result<_> {
                    if let Some(spool) = &q.spool {
                        let record =
                            repo::device_catalog::load(&self.home, scope_id(Some(spool))?)?;
                        session.facts(Some(&record.capability_path))?;
                    }
                    Ok((self.ownership_snapshot(session, q)?, complete()))
                }
            ),
            "ObserveWorkspace" => observe!(
                ObserveWorkspaceRequest::decode(body)?,
                |q: &ObserveWorkspaceRequest| q.threads.clone(),
                |q: &ObserveWorkspaceRequest, b: &ReadBudget, k: &[u8]| self
                    .workspace_snapshot(session, q, b, k)
            ),
            "ObserveSpool" => observe!(
                ObserveSpoolRequest::decode(body)?,
                |q: &ObserveSpoolRequest| if q.sections.contains(&(SpoolSection::Threads as i32)) {
                    let mut t = q.threads.clone().unwrap_or_default();
                    t.spools = q.spool.clone().into_iter().collect();
                    Some(t)
                } else {
                    None
                },
                |q: &ObserveSpoolRequest, b: &ReadBudget, k: &[u8]| self
                    .spool_snapshot(session, q, b, k)
            ),
            "ObserveThreads" => observe!(
                ObserveThreadsRequest::decode(body)?,
                |q: &ObserveThreadsRequest| Some(q.query.clone().unwrap_or_default()),
                |q: &ObserveThreadsRequest, b: &ReadBudget, k: &[u8]| -> Result<_> {
                    let (records, page) = self.local_threads(
                        session,
                        &q.query.clone().unwrap_or_default(),
                        &q.page.clone().unwrap_or_default(),
                        b,
                        k,
                    )?;
                    Ok((
                        records
                            .into_iter()
                            .map(|thread| {
                                let key = format!(
                                    "thread:{}",
                                    hex::encode(
                                        &thread
                                            .r#ref
                                            .as_ref()
                                            .and_then(|r| r.id.as_ref())
                                            .map(|i| i.value.clone())
                                            .unwrap_or_default()
                                    )
                                );
                                (
                                    key,
                                    ThreadListEvent {
                                        frame: None,
                                        payload: Some(thread_list_event::Payload::Thread(thread)),
                                    },
                                )
                            })
                            .collect(),
                        page,
                    ))
                }
            ),
            _ => bail!("unknown account observation"),
        }
    }
    fn account_version(
        &self,
        feed: &AccountFeed,
        session: &AccountSession,
        threads: Option<&ThreadQuery>,
    ) -> Result<Vec<u8>> {
        let mut hash = blake3::Hasher::new_derive_key("heddle-local-account-view-v2");
        hash.update(&feed.version()?);
        if let Some(query) = threads {
            for spool in self.thread_spools(session, query)? {
                hash.update(spool.id.as_bytes());
                hash.update(
                    &repo::thread_replication::listing::epoch(&spool.heddle_dir)?.to_be_bytes(),
                );
            }
        }
        Ok(hash.finalize().as_bytes().to_vec())
    }
    fn workspace_snapshot(
        &self,
        session: &AccountSession,
        request: &ObserveWorkspaceRequest,
        budget: &ReadBudget,
        binding: &[u8],
    ) -> Result<(Vec<(String, WorkspaceEvent)>, PageInfo)> {
        let pages = request.pages.clone().unwrap_or_default();
        let requested_sections = 3
            + usize::from(request.threads.is_some())
            + usize::from(request.include_devices)
            + usize::from(request.include_bookmarks);
        ensure!(
            budget.max_items as usize > requested_sections,
            "workspace item budget too small for requested section statuses"
        );
        let mut quota = budget.clone();
        quota.max_items = (budget.max_items as usize - requested_sections)
            .checked_div(
                1 + usize::from(request.threads.is_some())
                    + usize::from(request.include_devices)
                    + usize::from(request.include_bookmarks),
            )
            .unwrap_or(1)
            .max(1) as u32;
        let mut rows = Vec::new();
        let mut exhausted = true;
        if let Some(catalog) = repo::device_catalog::store::Catalog::read(&self.home)? {
            let page = pages.spools.clone().unwrap_or_default();
            let after: Option<String> = decode_page(&page.after_page, binding, b"spools")?;
            let candidates = catalog.spools(
                after.as_deref().unwrap_or(""),
                page_size(&page, &quota),
                budget.max_snapshot_bytes as usize,
            )?;
            let last = candidates
                .records
                .last()
                .map(|r| r.registration.id.to_string());
            for record in candidates.records {
                if !session.permits(&record.registration.capability_path) {
                    continue;
                }
                let id = record.registration.id;
                rows.push((
                    format!("spool:{id}"),
                    workspace(workspace_event::Payload::Spool(
                        self.spool_actions(session, &record),
                    )),
                ));
            }
            let page = PageInfo {
                next_page: if candidates.has_more {
                    encode_page(&last.context("Spool page progress")?, binding, b"spools")?
                } else {
                    Vec::new()
                },
                exhausted: !candidates.has_more,
                matching_count: None,
            };
            exhausted &= page.exhausted;
            rows.push(workspace_status("spools", Coverage::Complete, Some(page)));
            if request.include_bookmarks {
                let page = pages.bookmarks.clone().unwrap_or_default();
                let after: Option<Vec<u8>> = decode_page(&page.after_page, binding, b"bookmarks")?;
                let records = catalog.bookmarks(
                    &session.principal,
                    after.as_deref().unwrap_or_default(),
                    page_size(&page, &quota),
                    budget.max_snapshot_bytes as usize,
                )?;
                let last = records
                    .records
                    .last()
                    .and_then(|r| r.r#ref.as_ref())
                    .map(Message::encode_to_vec);
                for record in records.records {
                    let Some(reference) = &record.r#ref else {
                        bail!("bookmark reference missing")
                    };
                    if !self.bookmark_visible(session, reference)? {
                        continue;
                    }
                    rows.push((
                        format!("bookmark:{}", hex::encode(reference.encode_to_vec())),
                        workspace(workspace_event::Payload::Bookmark(record)),
                    ))
                }
                let page = PageInfo {
                    next_page: if records.has_more {
                        encode_page(&last.context("bookmark progress")?, binding, b"bookmarks")?
                    } else {
                        Vec::new()
                    },
                    exhausted: !records.has_more,
                    matching_count: None,
                };
                exhausted &= page.exhausted;
                rows.push(workspace_status(
                    "bookmarks",
                    Coverage::Complete,
                    Some(page),
                ));
            }
        } else {
            rows.push(workspace_status(
                "spools",
                Coverage::Complete,
                Some(complete()),
            ));
            if request.include_bookmarks {
                rows.push(workspace_status(
                    "bookmarks",
                    Coverage::Complete,
                    Some(complete()),
                ));
            }
        }
        if let Some(query) = &request.threads {
            let (records, page) = self.local_threads(
                session,
                query,
                &pages.threads.clone().unwrap_or_default(),
                &quota,
                binding,
            )?;
            for record in records {
                rows.push((
                    format!(
                        "thread:{}",
                        hex::encode(
                            record
                                .r#ref
                                .as_ref()
                                .context("Thread ref")?
                                .id
                                .as_ref()
                                .context("Thread ID")?
                                .value
                                .clone()
                        )
                    ),
                    workspace(workspace_event::Payload::Thread(record)),
                ))
            }
            exhausted &= page.exhausted;
            rows.push(workspace_status("threads", Coverage::Complete, Some(page)));
        }
        if request.include_devices {
            ensure!(
                pages
                    .devices
                    .as_ref()
                    .is_none_or(|p| p.after_page.is_empty()),
                "single local device has no next page"
            );
            rows.push((
                "device".into(),
                workspace(workspace_event::Payload::Device(self.endpoint())),
            ));
            rows.push(workspace_status(
                "devices",
                Coverage::Complete,
                Some(complete()),
            ));
        }
        for (section, requested) in [
            ("attention", request.include_attention),
            ("operations", request.include_operations),
        ] {
            rows.push(workspace_status(
                section,
                if requested {
                    Coverage::Unavailable
                } else {
                    Coverage::NotRequested
                },
                None,
            ));
        }
        Ok((
            rows,
            PageInfo {
                exhausted,
                ..Default::default()
            },
        ))
    }
    fn spool_snapshot(
        &self,
        session: &AccountSession,
        request: &ObserveSpoolRequest,
        budget: &ReadBudget,
        binding: &[u8],
    ) -> Result<(Vec<(String, SpoolEvent)>, PageInfo)> {
        let id = scope_id(request.spool.as_ref())?;
        let catalog = repo::device_catalog::store::Catalog::read(&self.home)?
            .context("local catalog missing")?;
        let record = catalog.spool(id)?.context("local Spool unavailable")?;
        session.facts(Some(&record.registration.capability_path))?;
        let mut overview = self.spool_actions(session, &record);
        overview.current_bookmark = Some(catalog.bookmark(&BookmarkRef {
            account: Some(PrincipalRef {
                id: session.principal.clone(),
            }),
            target: Some(bookmark_ref::Target::Spool(SpoolRef { id: id.to_string() })),
        })?);
        let mut rows = vec![(
            format!("spool:{id}"),
            spool(spool_event::Payload::Spool(overview)),
        )];
        let mut exhausted = true;
        let pages = request.pages.clone().unwrap_or_default();
        let sections: std::collections::BTreeSet<_> = request.sections.iter().copied().collect();
        ensure!(
            budget.max_items as usize > sections.len() + 1,
            "Spool budget too small"
        );
        let mut quota = budget.clone();
        quota.max_items = ((budget.max_items as usize - sections.len() - 1) / 2).max(1) as u32;
        for section in sections {
            let section = SpoolSection::try_from(section).context("unknown Spool section")?;
            let (name, coverage, page) = match section {
                SpoolSection::Unspecified => bail!("unspecified Spool section"),
                SpoolSection::Overview => continue,
                SpoolSection::Threads => {
                    let mut query = request.threads.clone().unwrap_or_default();
                    if !query.spools.is_empty() {
                        ensure!(
                            query.spools.iter().all(|r| r.id == id.to_string()),
                            "Spool Thread query names another Spool"
                        )
                    }
                    query.spools = vec![SpoolRef { id: id.to_string() }];
                    let (records, page) = self.local_threads(
                        session,
                        &query,
                        &pages.threads.clone().unwrap_or_default(),
                        &quota,
                        binding,
                    )?;
                    for record in records {
                        rows.push((
                            format!(
                                "thread:{}",
                                hex::encode(
                                    &record
                                        .r#ref
                                        .as_ref()
                                        .context("Thread")?
                                        .id
                                        .as_ref()
                                        .context("Thread ID")?
                                        .value
                                )
                            ),
                            spool(spool_event::Payload::Thread(record)),
                        ));
                    }
                    ("threads", Coverage::Complete, Some(page))
                }
                SpoolSection::Children => {
                    let page = pages.children.clone().unwrap_or_default();
                    let after: Option<String> =
                        decode_page(&page.after_page, binding, b"children")?;
                    let size = page_size(&page, &quota);
                    let mut remaining = size;
                    let mut next = None;
                    let mut mount_after = "";
                    if let Some(value) = after.as_deref().and_then(|v| v.strip_prefix("m:")) {
                        mount_after = value;
                    } else {
                        let child_after = match after.as_deref() {
                            None => "",
                            Some(value) => value
                                .strip_prefix("c:")
                                .context("invalid child page phase")?,
                        };
                        let children = catalog.children(
                            id,
                            child_after,
                            size,
                            budget.max_snapshot_bytes as usize,
                        )?;
                        remaining = remaining.saturating_sub(children.records.len());
                        let last = children
                            .records
                            .last()
                            .map(|r| r.registration.id.to_string());
                        for child in children.records {
                            if session.permits(&child.registration.capability_path) {
                                rows.push((
                                    format!("child:{}", child.registration.id),
                                    spool(spool_event::Payload::Child(child.overview)),
                                ));
                            }
                        }
                        if children.has_more {
                            next = Some(format!("c:{}", last.context("child progress")?));
                        }
                    }
                    if next.is_none() {
                        let mounts = catalog.mounts(id, mount_after, remaining.max(1))?;
                        if remaining == 0 {
                            if !mounts.records.is_empty() {
                                next = Some("m:".into());
                            }
                        } else {
                            let last = mounts
                                .records
                                .last()
                                .and_then(|r| r.r#ref.as_ref())
                                .map(|r| r.id.clone());
                            for mount in mounts.records {
                                let child = catalog
                                    .spool(scope_id(mount.child.as_ref())?)?
                                    .context("mounted child missing")?;
                                if session.permits(&child.registration.capability_path) {
                                    rows.push((
                                        format!(
                                            "mount:{}",
                                            mount.r#ref.as_ref().context("mount ref")?.id
                                        ),
                                        spool(spool_event::Payload::Mount(mount)),
                                    ));
                                }
                            }
                            if mounts.has_more {
                                next = Some(format!("m:{}", last.context("mount progress")?));
                            }
                        }
                    }
                    let page = PageInfo {
                        next_page: next
                            .as_ref()
                            .map(|value| encode_page(value, binding, b"children"))
                            .transpose()?
                            .unwrap_or_default(),
                        exhausted: next.is_none(),
                        matching_count: None,
                    };
                    ("children", Coverage::Complete, Some(page))
                }
                SpoolSection::Members => ("members", Coverage::Unavailable, None),
                SpoolSection::Invitations => ("invitations", Coverage::Unavailable, None),
                SpoolSection::Grants => ("grants", Coverage::Unavailable, None),
                SpoolSection::Policies => ("policies", Coverage::Unavailable, None),
                SpoolSection::Context => ("context", Coverage::Unavailable, None),
                SpoolSection::Operations => ("operations", Coverage::Unavailable, None),
                SpoolSection::SupportAccess => ("support_access", Coverage::Unavailable, None),
            };
            exhausted &= page.as_ref().is_none_or(|p| p.exhausted);
            rows.push((
                format!("status:{name}"),
                spool(spool_event::Payload::Status(SectionStatus {
                    section: name.into(),
                    coverage: coverage as i32,
                    page,
                    ..Default::default()
                })),
            ));
        }
        Ok((
            rows,
            PageInfo {
                exhausted,
                ..Default::default()
            },
        ))
    }
    fn spool_actions(
        &self,
        session: &AccountSession,
        record: &repo::device_catalog::store::SpoolRecord,
    ) -> SpoolOverview {
        let mut overview = record.overview.clone();
        let resource = EntityRef {
            entity: Some(entity_ref::Entity::Spool(SpoolRef {
                id: record.registration.id.to_string(),
            })),
        };
        overview.actions = ["CreateSpool", "ReviseSpool", "DeleteSpool"]
            .into_iter()
            .map(|suffix| {
                let method = format!("/heddle.api.v2alpha1.SpoolService/{suffix}");
                let may_create = suffix != "CreateSpool"
                    || overview
                        .settings
                        .as_ref()
                        .is_some_and(|s| s.allow_child_creation);
                ActionAvailability {
                    authorized: may_create
                        && session.permits_method(&method, &record.registration.capability_path),
                    method,
                    implemented: true,
                    target: Some(resource.clone()),
                    endpoint: Some(self.endpoint()),
                    observed_versions: if suffix == "CreateSpool" {
                        vec![]
                    } else {
                        vec![ExpectedVersion {
                            resource: Some(resource.clone()),
                            version: overview.version.clone(),
                        }]
                    },
                    ..Default::default()
                }
            })
            .collect();
        overview
    }
    pub(super) fn bookmark_visible(
        &self,
        session: &AccountSession,
        bookmark: &BookmarkRef,
    ) -> Result<bool> {
        if bookmark
            .account
            .as_ref()
            .is_none_or(|a| a.id != session.principal)
        {
            return Ok(false);
        }
        let reference = match bookmark.target.as_ref() {
            Some(bookmark_ref::Target::Spool(spool)) => Some(spool),
            Some(bookmark_ref::Target::Thread(thread)) => thread.spool.as_ref(),
            None => None,
        };
        let Some(reference) = reference else {
            return Ok(false);
        };
        let Some(catalog) = repo::device_catalog::store::Catalog::read(&self.home)? else {
            return Ok(false);
        };
        Ok(catalog
            .spool(scope_id(Some(reference))?)?
            .is_some_and(|r| session.permits(&r.registration.capability_path)))
    }
}
fn workspace(payload: workspace_event::Payload) -> WorkspaceEvent {
    WorkspaceEvent {
        frame: None,
        payload: Some(payload),
    }
}
fn spool(payload: spool_event::Payload) -> SpoolEvent {
    SpoolEvent {
        frame: None,
        payload: Some(payload),
    }
}
fn workspace_status(
    name: &str,
    coverage: Coverage,
    page: Option<PageInfo>,
) -> (String, WorkspaceEvent) {
    (
        format!("status:{name}"),
        workspace(workspace_event::Payload::Status(SectionStatus {
            section: name.into(),
            coverage: coverage as i32,
            page,
            ..Default::default()
        })),
    )
}
fn complete() -> PageInfo {
    PageInfo {
        exhausted: true,
        ..Default::default()
    }
}
pub(super) fn page_size(page: &PageRequest, budget: &ReadBudget) -> usize {
    (if page.size == 0 {
        budget.max_items.min(100).max(1)
    } else {
        page.size.min(budget.max_items).min(1024).max(1)
    }) as usize
}
pub(super) fn encode_page<T: serde::Serialize>(
    value: &T,
    binding: &[u8],
    section: &[u8],
) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= 8192, "page cursor payload exceeds bound");
    let hash = blake3::hash(&[binding, section, &bytes].concat());
    Ok([hash.as_bytes().as_slice(), &bytes].concat())
}
pub(super) fn decode_page<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    binding: &[u8],
    section: &[u8],
) -> Result<Option<T>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    ensure!(
        bytes.len() > 32 && bytes.len() <= 8224,
        "invalid page cursor bound"
    );
    let hash = blake3::hash(&[binding, section, &bytes[32..]].concat());
    ensure!(
        bytes[..32] == hash.as_bytes()[..],
        "page cursor belongs to another view or section"
    );
    Ok(Some(serde_json::from_slice(&bytes[32..])?))
}
trait ClearPages {
    fn clear_pages(&mut self);
}
fn clear_pages(value: &mut impl ClearPages) {
    value.clear_pages()
}
impl ClearPages for ObserveOwnershipRequest {
    fn clear_pages(&mut self) {}
}
impl ClearPages for ObserveThreadsRequest {
    fn clear_pages(&mut self) {
        if let Some(p) = &mut self.page {
            p.after_page.clear()
        }
    }
}
impl ClearPages for ObserveIdentityRequest {
    fn clear_pages(&mut self) {
        for p in [
            &mut self.devices,
            &mut self.sessions,
            &mut self.signup_invitations,
            &mut self.delegations,
            &mut self.recovery,
        ]
        .into_iter()
        .flatten()
        {
            p.after_page.clear()
        }
    }
}
impl ClearPages for ObserveWorkspaceRequest {
    fn clear_pages(&mut self) {
        if let Some(p) = &mut self.pages {
            for p in [
                &mut p.spools,
                &mut p.threads,
                &mut p.attention,
                &mut p.operations,
                &mut p.devices,
                &mut p.bookmarks,
            ]
            .into_iter()
            .flatten()
            {
                p.after_page.clear()
            }
        }
    }
}
impl ClearPages for ObserveSpoolRequest {
    fn clear_pages(&mut self) {
        if let Some(p) = &mut self.pages {
            for p in [
                &mut p.threads,
                &mut p.members,
                &mut p.invitations,
                &mut p.grants,
                &mut p.policies,
                &mut p.approval_groups,
                &mut p.context,
                &mut p.operations,
                &mut p.children,
                &mut p.support_access,
            ]
            .into_iter()
            .flatten()
            {
                p.after_page.clear()
            }
        }
    }
}

/// Each published batch binds its actual local registrations. Time checks are
/// memory-only; protected output verifies the same durable path/physical identity.
struct AccountObservation<'a> {
    session: &'a AccountSession,
    home: &'a std::path::Path,
    spools: std::sync::Mutex<Vec<repo::device_catalog::DeviceSpool>>,
}
impl AccountObservation<'_> {
    fn bind(&self) -> Result<()> {
        let records = repo::device_catalog::store::Catalog::read(self.home)?
            .map(|catalog| catalog.registrations())
            .transpose()?
            .unwrap_or_default();
        let visible = records
            .into_iter()
            .filter(|record| self.session.permits(&record.capability_path))
            .collect();
        *self
            .spools
            .lock()
            .map_err(|_| anyhow::anyhow!("account projection scope poisoned"))? = visible;
        Ok(())
    }
}
impl super::stream::ObservationAuthority for AccountObservation<'_> {
    fn binding(&self) -> Vec<u8> {
        self.session.binding()
    }
    fn expires(&self) -> i64 {
        self.session.expires
    }
    fn check_clock(&self) -> Result<()> {
        self.session.check_clock()?;
        for spool in self
            .spools
            .lock()
            .map_err(|_| anyhow::anyhow!("account projection scope poisoned"))?
            .iter()
        {
            self.session.facts(Some(&spool.capability_path))?;
        }
        Ok(())
    }
    fn check_current(&self, home: &std::path::Path) -> Result<()> {
        self.session.check_current(home)?;
        self.check_clock()?;
        let records = repo::device_catalog::store::Catalog::read(home)?
            .map(|catalog| catalog.registrations())
            .transpose()?
            .unwrap_or_default();
        let current: std::collections::BTreeMap<_, _> = records
            .into_iter()
            .map(|record| (record.id, record))
            .collect();
        for old in self
            .spools
            .lock()
            .map_err(|_| anyhow::anyhow!("account projection scope poisoned"))?
            .iter()
        {
            let current = current
                .get(&old.id)
                .context("observed Spool registration removed")?;
            ensure!(
                current.root == old.root
                    && current.heddle_dir == old.heddle_dir
                    && current.capability_path == old.capability_path,
                "observed Spool registration changed"
            );
        }
        Ok(())
    }
}
