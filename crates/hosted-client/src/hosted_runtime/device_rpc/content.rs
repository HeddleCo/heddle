//! Finite private content reads. Exact revision reachability is checked before
//! decoding a blob; a retained artifact uses its own policy-bound catalog.
use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use api::heddle::api::{v1alpha1 as shared, v2alpha1::*};
use iroh::endpoint::SendStream;
use objects::{
    object::{ContentHash, State, TreeEntry, TreeEntryTarget},
    store::ObjectStore,
};
use prost::Message;

use super::{DeviceRpc, auth::Session, checkout, failure};

const MAX_WORK: usize = 100_000;
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ITEMS: u32 = 4096;
const MAX_FRAME: u32 = 256 * 1024;

#[derive(Clone, Copy)]
struct Budget {
    limits: ReadBudget,
    items: u32,
    bytes: u64,
}
impl Budget {
    fn new(requested: Option<ReadBudget>) -> Result<Self> {
        let requested = requested.unwrap_or_default();
        let limits = ReadBudget {
            max_items: if requested.max_items == 0 {
                1024
            } else {
                requested.max_items
            },
            max_frame_bytes: if requested.max_frame_bytes == 0 {
                64 * 1024
            } else {
                requested.max_frame_bytes
            },
            max_snapshot_bytes: if requested.max_snapshot_bytes == 0 {
                MAX_BYTES
            } else {
                requested.max_snapshot_bytes
            },
        };
        if limits.max_items > MAX_ITEMS
            || !(1024..=MAX_FRAME).contains(&limits.max_frame_bytes)
            || limits.max_snapshot_bytes > MAX_BYTES
        {
            bail!("content read budget exceeds endpoint limits");
        }
        Ok(Self {
            limits,
            items: 0,
            bytes: 0,
        })
    }
    fn charge(&mut self, message: &impl Message) -> Result<()> {
        let length = message.encoded_len() as u64;
        if self.items >= self.limits.max_items
            || length > u64::from(self.limits.max_frame_bytes)
            || length > self.limits.max_snapshot_bytes.saturating_sub(self.bytes)
        {
            bail!("content budget exhausted; resume with explicit ranges or pages");
        }
        self.items += 1;
        self.bytes += length;
        Ok(())
    }
}
impl DeviceRpc {
    pub(super) async fn read_content(
        &self,
        session: Session,
        body: &[u8],
        mut send: SendStream,
    ) -> Result<()> {
        let session = Arc::new(session);
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            self.content_response(&session, body, &mut send),
        )
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
        if let Err(error) = result {
            tokio::time::timeout(
                Duration::from_secs(5),
                send.write_all(&api::framing::encode_stream_failure(&failure(
                    shared::CallFailureCode::FailedPrecondition,
                    error,
                ))?),
            )
            .await??;
        }
        send.finish()?;
        Ok(())
    }
    async fn content_response(
        &self,
        session: &Arc<Session>,
        body: &[u8],
        send: &mut SendStream,
    ) -> Result<()> {
        let request = ReadContentRequest::decode(body)?;
        let slot = self
            .content_work
            .clone()
            .try_acquire_owned()
            .context("device content readers at capacity")?;
        let current = session.clone();
        let home = self.home.clone();
        let response_home = home.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Vec<u8>>>(1);
        let worker = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            current.check_current(&home)?;
            let mut budget = Budget::new(request.budget)?;
            let revision = request.revision.context("exact revision required")?;
            let state_id = checkout::revision(&current, Some(&revision))?;
            let repository = repo::Repository::open(&current.spool.root)?;
            let state = repository
                .store()
                .get_state(&state_id)?
                .context("local revision unavailable")?;
            if state.id() != state_id {
                bail!("stored revision identity differs from request");
            }
            if request.selections.is_empty() || request.selections.len() > 128 {
                bail!("invalid content selection count");
            }
            let mut ids = BTreeSet::new();
            let mut work = 0usize;
            for selection in request.selections {
                if selection.selection_id.is_empty()
                    || selection.selection_id.len() > 256
                    || !ids.insert(selection.selection_id.clone())
                {
                    bail!("selection IDs must be nonempty, bounded and unique");
                }
                let id = &selection.selection_id;
                let limits = budget.limits;
                let mut emit = |payload| -> Result<()> {
                    current.check_clock()?;
                    let event = ContentEvent {
                        selection_id: id.clone(),
                        revision: Some(revision.clone()),
                        payload: Some(payload),
                    };
                    budget.charge(&event)?;
                    sender
                        .blocking_send(Ok(event.encode_to_vec()))
                        .map_err(|_| anyhow::anyhow!("content read cancelled"))
                };
                match selection.selection.context("content selection required")? {
                    content_read::Selection::Blob(read) => {
                        let hash = blob_hash(repository.store(), &state, &read, &mut work)?;
                        let size = objects::store::ObjectSource::decoded_blob_len(
                            repository.store(),
                            &hash,
                        )?
                        .context("selected blob unavailable")?;
                        if size > limits.max_snapshot_bytes {
                            bail!("blob exceeds decoded read budget");
                        }
                        let blob = repository
                            .store()
                            .get_blob(&hash)?
                            .context("selected blob unavailable")?;
                        if blob.hash() != hash || blob.size() as u64 != size {
                            bail!("blob identity differs from selected revision");
                        }
                        let (mut offset, end) = range(size, read.offset, read.length)?;
                        loop {
                            let stop = end.min(offset + u64::from(limits.max_frame_bytes / 2));
                            emit(content_event::Payload::Blob(BlobChunk {
                                offset,
                                data: blob.content()[offset as usize..stop as usize].to_vec(),
                                total_size: size,
                                object_hash: hash.as_bytes().to_vec(),
                                range_complete: stop == end,
                            }))?;
                            offset = stop;
                            if offset == end {
                                break;
                            }
                        }
                        emit(content_event::Payload::SelectionComplete(complete(
                            "blob",
                            &revision,
                            Coverage::Complete,
                            None,
                        )))?;
                    }
                    content_read::Selection::Tree(read) => {
                        let (entries, page) = tree_page(
                            repository.store(),
                            &state,
                            &revision,
                            &read,
                            limits,
                            &mut work,
                        )?;
                        for entry in entries {
                            emit(content_event::Payload::TreeEntry(entry))?;
                        }
                        emit(content_event::Payload::SelectionComplete(complete(
                            "tree",
                            &revision,
                            Coverage::Complete,
                            Some(page),
                        )))?;
                    }
                    content_read::Selection::State(_) => {
                        super::content_detail::state(&state, &revision, &mut emit)?
                    }
                    content_read::Selection::Diff(read) => super::content_detail::diff(
                        &repository,
                        &current,
                        &state,
                        &revision,
                        &read,
                        limits,
                        &mut emit,
                    )?,
                    content_read::Selection::Provenance(read) => super::content_detail::provenance(
                        &repository,
                        &state,
                        &revision,
                        &read,
                        limits,
                        &mut emit,
                    )?,
                }
            }
            Ok::<(), anyhow::Error>(())
        });
        while let Some(bytes) = receiver.recv().await {
            session.check_current(&response_home)?;
            write(send, bytes?).await?;
        }
        worker.await??;
        Ok(())
    }
}

async fn write(send: &mut SendStream, bytes: Vec<u8>) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(30),
        send.write_all(&api::framing::encode_stream_message(&bytes)?),
    )
    .await??;
    Ok(())
}
pub(super) fn complete(
    section: &str,
    revision: &RevisionRef,
    coverage: Coverage,
    page: Option<PageInfo>,
) -> SectionStatus {
    SectionStatus {
        section: section.into(),
        coverage: coverage as i32,
        computed_for: Some(revision.clone()),
        page,
        ..Default::default()
    }
}
fn range(size: u64, offset: u64, length: u64) -> Result<(u64, u64)> {
    if offset > size {
        bail!("content offset exceeds size");
    }
    Ok((
        offset,
        if length == 0 {
            size
        } else {
            size.min(offset.saturating_add(length))
        },
    ))
}
fn charge(work: &mut usize) -> Result<()> {
    *work = work.checked_add(1).context("tree work overflow")?;
    if *work > MAX_WORK {
        bail!("content tree walk exceeds budget");
    }
    Ok(())
}
pub(super) fn normalize(path: &str, empty: bool) -> Result<()> {
    if (!empty && path.is_empty())
        || path.len() > 4096
        || (!path.is_empty()
            && path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == ".." || p.contains(['\\', '\0'])))
    {
        bail!("normalized repository-relative path required");
    }
    Ok(())
}
pub(super) fn path_entry(
    store: &objects::store::FsStore,
    root: ContentHash,
    path: &str,
    work: &mut usize,
) -> Result<TreeEntry> {
    normalize(path, false)?;
    let mut tree = root;
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        charge(work)?;
        let value = store.get_tree(&tree)?.context("source tree unavailable")?;
        if value.hash() != tree {
            bail!("source tree identity mismatch");
        }
        let entry = value.get(component).context("source path unavailable")?;
        if components.peek().is_none() {
            return Ok(entry.clone());
        }
        tree = entry
            .tree_hash()
            .context("path crosses a non-directory source entry")?;
    }
    bail!("source path unavailable")
}
pub(super) fn blob_hash(
    store: &objects::store::FsStore,
    state: &State,
    read: &BlobRead,
    work: &mut usize,
) -> Result<ContentHash> {
    match read.source.as_ref().context("blob source required")? {
        blob_read::Source::Path(path) => path_entry(store, state.tree, path, work)?
            .leaf_content_hash()
            .context("path is not a blob"),
        blob_read::Source::ObjectHash(bytes) => {
            let hash = ContentHash::from_bytes(
                bytes
                    .as_slice()
                    .try_into()
                    .context("blob hash requires32bytes")?,
            );
            let mut pending = vec![state.tree];
            let mut visited = BTreeSet::new();
            while let Some(tree) = pending.pop() {
                if !visited.insert(tree) {
                    continue;
                }
                let tree = store.get_tree(&tree)?.context("source tree unavailable")?;
                for entry in tree.entries() {
                    charge(work)?;
                    if entry.leaf_content_hash() == Some(hash) {
                        return Ok(hash);
                    }
                    if let Some(child) = entry.tree_hash() {
                        pending.push(child);
                    }
                }
            }
            bail!("requested blob is not reachable from the exact revision")
        }
    }
}
fn tree_page(
    store: &objects::store::FsStore,
    state: &State,
    revision: &RevisionRef,
    read: &TreeRead,
    limits: ReadBudget,
    work: &mut usize,
) -> Result<(Vec<ContentTreeEntry>, PageInfo)> {
    normalize(&read.path, true)?;
    if read.depth > 8 {
        bail!("tree depth exceeds8");
    }
    let root = if read.path.is_empty() {
        state.tree
    } else {
        path_entry(store, state.tree, &read.path, work)?
            .tree_hash()
            .context("tree path is not a directory")?
    };
    let page = read.page.clone().unwrap_or_default();
    let size = if page.size == 0 { 100 } else { page.size }.min(limits.max_items.saturating_sub(1))
        as usize;
    if size == 0 {
        bail!("tree page needs entry and completion frame budget");
    }
    let mut normalized = read.clone();
    normalized.page = None;
    let binding = blake3::hash(&[revision.encode_to_vec(), normalized.encode_to_vec()].concat());
    let after = if page.after_page.is_empty() {
        String::new()
    } else {
        if page.after_page.len() > 8192
            || page.after_page.len() < 32
            || page.after_page[..32] != binding.as_bytes()[..]
        {
            bail!("tree page token differs from revision or query");
        }
        String::from_utf8(page.after_page[32..].to_vec())?
    };
    let mut pending = VecDeque::from([(read.path.clone(), root, 0u32)]);
    let mut entries = Vec::new();
    while let Some((prefix, hash, depth)) = pending.pop_front() {
        let tree = store.get_tree(&hash)?.context("source tree unavailable")?;
        if tree.hash() != hash {
            bail!("source tree identity mismatch");
        }
        for entry in tree.entries() {
            charge(work)?;
            let path = if prefix.is_empty() {
                entry.name().to_owned()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            if depth < read.depth {
                if let Some(child) = entry.tree_hash() {
                    pending.push_back((path.clone(), child, depth + 1));
                }
            }
            if path <= after {
                continue;
            }
            entries.push(tree_entry(store, path, entry)?);
            if entries.len() > MAX_WORK {
                bail!("tree page walk exceeds budget");
            }
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let exhausted = entries.len() <= size;
    entries.truncate(size);
    let next_page = if exhausted {
        vec![]
    } else {
        [
            binding.as_bytes().as_slice(),
            entries.last().context("page empty")?.path.as_bytes(),
        ]
        .concat()
    };
    Ok((
        entries,
        PageInfo {
            next_page,
            exhausted,
            ..Default::default()
        },
    ))
}
fn tree_entry(
    store: &objects::store::FsStore,
    path: String,
    entry: &TreeEntry,
) -> Result<ContentTreeEntry> {
    use content_tree_entry::Target;
    let target = match entry.target() {
        TreeEntryTarget::Tree { hash } => Target::TreeHash(hash.as_bytes().to_vec()),
        TreeEntryTarget::Blob { hash, executable } => Target::File(ContentFile {
            object_hash: hash.as_bytes().to_vec(),
            size: objects::store::ObjectSource::decoded_blob_len(store, hash)?,
            executable: *executable,
        }),
        TreeEntryTarget::Symlink { hash } => Target::Symlink(ContentFile {
            object_hash: hash.as_bytes().to_vec(),
            size: objects::store::ObjectSource::decoded_blob_len(store, hash)?,
            executable: false,
        }),
        TreeEntryTarget::Gitlink { target } => Target::Gitlink(shared::GitObjectId {
            digest: hex::decode(target.to_string())?,
            algorithm: if target.to_string().len() == 40 {
                shared::GitObjectAlgorithm::Sha1
            } else {
                shared::GitObjectAlgorithm::Sha256
            } as i32,
            ..Default::default()
        }),
        TreeEntryTarget::Spoollink { spool_id, state_id } => Target::Spoollink(ContentSpoolLink {
            native_spool_id: spool_id.to_string(),
            state: Some(shared::StateId {
                value: state_id.as_bytes().to_vec(),
            }),
        }),
    };
    Ok(ContentTreeEntry {
        path,
        target: Some(target),
        last_changed_at: None,
        last_changed_by: None,
    })
}
