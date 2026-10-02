// SPDX-License-Identifier: Apache-2.0
//! Bounded local operation observations, correlated by native IDs rather than
//! the workspace's latest model. No prompts, arguments or tool output persist.
use crate::IdentityCursor;
use fs2::FileExt;
use objects::object::{
    AttributionCollectionMethod, AttributionEvidenceV1, AttributionFileChange,
    AttributionOperation, AttributionOperationIdentity, AttributionOperationResolution, Blob,
    ContentHash,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
};

const MAX_OPERATIONS: usize = 64;
const MAX_PATHS: usize = 32;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationEventPhase {
    /// Enrich an existing causal operation without sampling filesystem state.
    Observe,
    Before,
    After,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    key: String,
    identity: AttributionOperationIdentity,
    files: Vec<AttributionFileChange>,
    ambiguous: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    pending: Vec<Pending>,
    completed: Vec<(String, AttributionOperation)>,
    incomplete: bool,
}
fn path(root: &Path) -> PathBuf {
    let marker = root.join(".heddle");
    if marker.is_file() {
        root.join(".heddle.operations")
    } else {
        marker.join("operations")
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid operation attribution journal",
    )
}
fn read(file: &Path) -> io::Result<Journal> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let input = match options.open(file) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Journal::default()),
        Err(error) => return Err(error),
    };
    if input.metadata()?.len() > MAX_JOURNAL_BYTES {
        return Err(invalid());
    }
    use io::Read;
    let mut bytes = Vec::new();
    input.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(invalid());
    }
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if journal.pending.len() > MAX_OPERATIONS || journal.completed.len() > MAX_OPERATIONS {
        return Err(invalid());
    }
    for pending in &journal.pending {
        if pending.key.len() > 2048 || pending.files.len() > MAX_PATHS {
            return Err(invalid());
        }
        pending.identity.validate().map_err(|_| invalid())?;
        for file in &pending.files {
            file.validate().map_err(|_| invalid())?;
        }
    }
    for (key, operation) in &journal.completed {
        if key.len() > 2048 {
            return Err(invalid());
        }
        operation.validate().map_err(|_| invalid())?;
    }
    Ok(journal)
}
fn locked<T>(root: &Path, action: impl FnOnce(&mut Journal) -> io::Result<T>) -> io::Result<T> {
    let file = path(root);
    let parent = file.parent().ok_or_else(invalid)?;
    fs::create_dir_all(parent)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(file.with_extension("operations-lock"))?;
    lock.lock_exclusive()?;
    let result = (|| {
        let mut journal = read(&file)?;
        let result = action(&mut journal)?;
        let bytes = serde_json::to_vec(&journal).map_err(|_| invalid())?;
        if bytes.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(invalid());
        }
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temp = file.with_extension(format!(
            "operations-tmp-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        use io::Write;
        output.write_all(&bytes)?;
        output.sync_all()?;
        fs::rename(&temp, &file)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(result)
    })();
    let _ = lock.unlock();
    result
}
fn key(evidence: &AttributionEvidenceV1) -> Option<String> {
    let scope = &evidence.scope;
    let harness = &evidence.harness.as_ref()?.value;
    let session = scope.harness_session_id.as_ref()?;
    let actor = scope.actor_id.as_ref().unwrap_or(session);
    let tool = scope.tool_call_id.as_ref()?;
    // JSON tuple avoids collisions between opaque IDs containing punctuation.
    serde_json::to_string(&(
        harness,
        session,
        actor,
        &scope.turn_id,
        &scope.message_id,
        tool,
    ))
    .ok()
}
fn relative(root: &Path, input: &Path) -> Option<String> {
    let relative = if input.is_absolute() {
        input.strip_prefix(root).ok()?
    } else {
        input
    };
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let value = relative.to_str()?;
    if value.len() > 1024
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part == ".git" || part == ".heddle" || part.starts_with(".heddle."))
    {
        return None;
    }
    Some(value.to_owned())
}
#[cfg(unix)]
fn file_hash(root: &Path, relative: &str) -> io::Result<Option<ContentHash>> {
    use std::{
        ffi::CString,
        io::Read,
        os::fd::{AsRawFd, FromRawFd},
    };
    let mut handle = fs::File::open(root)?;
    let mut components = Path::new(relative).components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(invalid());
        };
        let name = CString::new(name.as_encoded_bytes()).map_err(|_| invalid())?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if components.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // Each directory handle pins the traversal; no pathname symlink race
        // can redirect an observation outside the selected repository.
        let fd = unsafe { libc::openat(handle.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error);
        }
        handle = unsafe { fs::File::from_raw_fd(fd) };
    }
    let metadata = handle.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    handle.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid());
    }
    Ok(Some(Blob::new(bytes).hash()))
}
#[cfg(not(unix))]
fn file_hash(_root: &Path, _relative: &str) -> io::Result<Option<ContentHash>> {
    // Until a handle-relative no-reparse-point implementation is verified,
    // non-Unix adapters retain identity but cannot claim a content binding.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "safe operation file binding is not supported on this platform",
    ))
}

/// Record only an adapter's allowlisted paths and direct event identity. A post
/// event is joined to its pre-event, never to a later cursor/model selection.
pub fn record_operation_event(
    root: &Path,
    patch: &IdentityCursor,
    phase: OperationEventPhase,
    paths: &[PathBuf],
) -> io::Result<()> {
    record_with_method(root, patch, phase, paths, AttributionCollectionMethod::Hook)
}

pub(crate) fn record_with_method(
    root: &Path,
    patch: &IdentityCursor,
    phase: OperationEventPhase,
    paths: &[PathBuf],
    method: AttributionCollectionMethod,
) -> io::Result<()> {
    locked(root, |journal| {
        let Some(evidence) = patch
            .attribution_evidence
            .as_ref()
            .filter(|e| e.validate().is_ok())
        else {
            journal.incomplete = true;
            return Ok(());
        };
        let mut identity = evidence.operation_identity();
        identity.collection_methods = vec![method];
        let Some(key) = key(evidence) else {
            if phase == OperationEventPhase::Observe {
                return Ok(());
            }
            journal.incomplete = true;
            let unknown_key = format!(
                "unresolved:{}",
                evidence.to_blob().map_err(|_| invalid())?.hash().to_hex()
            );
            if journal.completed.len() < MAX_OPERATIONS
                && !journal.completed.iter().any(|(key, _)| key == &unknown_key)
            {
                journal.completed.push((
                    unknown_key,
                    AttributionOperation {
                        identity: identity.clone(),
                        changes: Vec::new(),
                        resolution: AttributionOperationResolution::Unresolved,
                    },
                ));
            }
            return Ok(());
        };
        if replay_seen(&replay_database(root)?, &key)? {
            return Ok(());
        }
        if let Some((_, previous)) = journal.completed.iter_mut().find(|(old, _)| old == &key) {
            if merge_identity(&mut previous.identity, &identity) {
                previous.resolution = AttributionOperationResolution::Unresolved;
                journal.incomplete = true;
                if journal.completed.len() < MAX_OPERATIONS {
                    journal.completed.push((
                        format!("conflict:{key}:{}", journal.completed.len()),
                        AttributionOperation {
                            identity: identity.clone(),
                            changes: Vec::new(),
                            resolution: AttributionOperationResolution::Unresolved,
                        },
                    ));
                }
            }
            return Ok(());
        }
        let mut names: Vec<_> = paths.iter().filter_map(|p| relative(root, p)).collect();
        names.sort();
        names.dedup();
        let invalid_paths =
            names.is_empty() || names.len() > MAX_PATHS || paths.len() != names.len();
        names.truncate(MAX_PATHS);
        match phase {
            OperationEventPhase::Observe => {
                // Metadata never opens/closes an operation or resamples bytes.
                if let Some(pending) = journal.pending.iter_mut().find(|p| p.key == key)
                    && merge_identity(&mut pending.identity, &identity)
                {
                    pending.ambiguous = true;
                    journal.incomplete = true;
                    if journal.completed.len() < MAX_OPERATIONS {
                        journal.completed.push((
                            format!("conflict:{key}:{}", journal.completed.len()),
                            AttributionOperation {
                                identity,
                                changes: Vec::new(),
                                resolution: AttributionOperationResolution::Unresolved,
                            },
                        ));
                    }
                }
            }
            OperationEventPhase::Before => {
                if let Some(pending) = journal.pending.iter_mut().find(|p| p.key == key) {
                    if invalid_paths
                        || names
                            != pending
                                .files
                                .iter()
                                .map(|file| file.path.clone())
                                .collect::<Vec<_>>()
                    {
                        pending.ambiguous = true;
                        journal.incomplete = true;
                    }
                    if merge_identity(&mut pending.identity, &identity) {
                        pending.ambiguous = true;
                        journal.incomplete = true;
                        if journal.completed.len() < MAX_OPERATIONS {
                            journal.completed.push((
                                format!("conflict:{key}:{}", journal.completed.len()),
                                AttributionOperation {
                                    identity: identity.clone(),
                                    changes: Vec::new(),
                                    resolution: AttributionOperationResolution::Unresolved,
                                },
                            ));
                        }
                    }
                    return Ok(());
                }
                if journal.pending.len() == MAX_OPERATIONS {
                    journal.incomplete = true;
                    return Ok(());
                }
                let mut ambiguous = invalid_paths;
                let mut files = Vec::new();
                for name in names {
                    let before = match file_hash(root, &name) {
                        Ok(hash) => hash,
                        Err(_) => {
                            ambiguous = true;
                            None
                        }
                    };
                    for pending in &mut journal.pending {
                        if pending.files.iter().any(|file| file.path == name) {
                            pending.ambiguous = true;
                            ambiguous = true;
                        }
                    }
                    files.push(AttributionFileChange {
                        path: name,
                        before,
                        after: before,
                    });
                }
                journal.pending.push(Pending {
                    key,
                    identity: identity.clone(),
                    files,
                    ambiguous,
                });
            }
            OperationEventPhase::After | OperationEventPhase::Failed => {
                let pending = journal
                    .pending
                    .iter()
                    .position(|p| p.key == key)
                    .map(|index| journal.pending.remove(index));
                let mut operation = if let Some(mut pending) = pending {
                    let conflict = merge_identity(&mut pending.identity, &identity);
                    if conflict && journal.completed.len() < MAX_OPERATIONS {
                        journal.completed.push((
                            format!("conflict:{key}:{}", journal.completed.len()),
                            AttributionOperation {
                                identity: identity.clone(),
                                changes: Vec::new(),
                                resolution: AttributionOperationResolution::Unresolved,
                            },
                        ));
                    }
                    let pending_names: Vec<_> =
                        pending.files.iter().map(|f| f.path.clone()).collect();
                    let mut ambiguous = conflict
                        || pending.ambiguous
                        || phase == OperationEventPhase::Failed
                        || (!paths.is_empty() && invalid_paths)
                        || (!names.is_empty() && names != pending_names);
                    for file in &mut pending.files {
                        file.after = match file_hash(root, &file.path) {
                            Ok(hash) => hash,
                            Err(_) => {
                                ambiguous = true;
                                None
                            }
                        };
                    }
                    pending.files.retain(|file| file.before != file.after);
                    if pending.files.is_empty() {
                        ambiguous = true;
                    }
                    AttributionOperation {
                        identity: pending.identity,
                        changes: pending.files,
                        resolution: if ambiguous {
                            AttributionOperationResolution::Unresolved
                        } else {
                            AttributionOperationResolution::ContentBound
                        },
                    }
                } else {
                    journal.incomplete = true;
                    AttributionOperation {
                        identity: identity.clone(),
                        changes: Vec::new(),
                        resolution: AttributionOperationResolution::Unresolved,
                    }
                };
                // This is provisional content matching; capture validates the
                // complete chain against its parent and exact committed tree.
                if phase == OperationEventPhase::Failed {
                    operation.resolution = AttributionOperationResolution::Unresolved;
                }
                if journal.completed.len() == MAX_OPERATIONS {
                    journal.incomplete = true;
                } else {
                    journal.completed.push((key, operation));
                }
            }
        }
        Ok(())
    })
}

/// Freeze all observed contributors at the same time as capture identity.
/// In-flight/opaque/missing events make coverage explicitly incomplete.
pub(crate) fn freeze(root: &Path, evidence: &mut AttributionEvidenceV1) -> io::Result<()> {
    locked(root, |journal| {
        let database = replay_database(root)?;
        let mut completed = Vec::new();
        for (key, operation) in journal.completed.drain(..) {
            if !replay_seen(&database, &key)? {
                completed.push((key, operation));
            }
        }
        journal.completed = completed;
        evidence.operations = journal.completed.iter().map(|(_, op)| op.clone()).collect();
        evidence.operations_incomplete =
            journal.incomplete || !journal.pending.is_empty() || evidence.operations.is_empty();
        for pending in &journal.pending {
            if evidence.operations.len() == MAX_OPERATIONS {
                break;
            }
            evidence.operations.push(AttributionOperation {
                identity: pending.identity.clone(),
                changes: Vec::new(),
                resolution: AttributionOperationResolution::Unresolved,
            });
        }
        Ok(())
    })
}

/// Remove only observations included in this published state. Events arriving
/// during capture remain queued; a failed capture consumes nothing.
pub(crate) fn acknowledge(root: &Path, evidence: &AttributionEvidenceV1) -> io::Result<()> {
    locked(root, |journal| {
        let mut database = replay_database(root)?;
        let transaction = database.transaction().map_err(database_error)?;
        let mut retained = Vec::new();
        for (key, operation) in journal.completed.drain(..) {
            if evidence.operations.iter().any(|seen| {
                seen.identity == operation.identity && seen.changes == operation.changes
            }) {
                let id = ContentHash::compute(key.as_bytes());
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO seen (id) VALUES (?1)",
                        [id.as_bytes().as_slice()],
                    )
                    .map_err(database_error)?;
            } else {
                retained.push((key, operation));
            }
        }
        // Commit replay protection first. If rewriting the bounded journal is
        // interrupted, freeze removes these already-published observations.
        transaction.commit().map_err(database_error)?;
        journal.completed = retained;
        if journal.completed.is_empty() && journal.pending.is_empty() {
            journal.incomplete = false;
        }
        Ok(())
    })
}

/// Only an unbroken observed file transition from parent to current bytes can
/// be content-bound. Ambiguous histories remain visible as unresolved claims.
pub(crate) fn bind(
    repo: &repo::Repository,
    evidence: &mut AttributionEvidenceV1,
) -> anyhow::Result<()> {
    let parent = repo
        .current_state()?
        .map(|state| repo.require_tree(&state.tree))
        .transpose()?;
    let mut by_path: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, op) in evidence.operations.iter().enumerate() {
        for change in &op.changes {
            by_path.entry(change.path.clone()).or_default().push(index);
        }
    }
    for (path, indexes) in by_path {
        let parent_hash = match parent.as_ref() {
            Some(tree) => parent_file(repo, tree, &path).ok(),
            None => Some(None),
        };
        let mut expected = parent_hash.flatten();
        let mut valid = parent_hash.is_some();
        for index in &indexes {
            let op = &evidence.operations[*index];
            let change = op
                .changes
                .iter()
                .find(|file| file.path == path)
                .ok_or_else(invalid)?;
            if op.resolution != AttributionOperationResolution::ContentBound
                || change.before != expected
            {
                valid = false;
            }
            expected = change.after;
        }
        if file_hash(repo.root(), &path).ok() != Some(expected) {
            valid = false;
        }
        // A reverted chain leaves no source change. When a sequence ends in
        // deletion its earlier written bytes likewise cannot survive capture.
        if parent_hash == Some(expected) || (expected.is_none() && indexes.len() > 1) {
            valid = false;
        }
        if !valid {
            evidence.operations_incomplete = true;
            for index in indexes {
                evidence.operations[index].resolution = AttributionOperationResolution::Unresolved;
            }
        }
    }
    // A multi-path operation is indivisible. Downgrading it on one path must
    // invalidate every connected chain, including paths visited earlier.
    loop {
        let unresolved_paths: std::collections::BTreeSet<_> = evidence
            .operations
            .iter()
            .filter(|operation| operation.resolution == AttributionOperationResolution::Unresolved)
            .flat_map(|operation| operation.changes.iter().map(|change| change.path.clone()))
            .collect();
        let mut changed = false;
        for operation in &mut evidence.operations {
            if operation.resolution == AttributionOperationResolution::ContentBound
                && operation
                    .changes
                    .iter()
                    .any(|change| unresolved_paths.contains(&change.path))
            {
                operation.resolution = AttributionOperationResolution::Unresolved;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if evidence
        .operations
        .iter()
        .any(|op| op.resolution == AttributionOperationResolution::Unresolved)
    {
        evidence.operations_incomplete = true;
    }
    Ok(())
}

fn parent_file(
    repo: &repo::Repository,
    tree: &objects::object::Tree,
    path: &str,
) -> anyhow::Result<Option<ContentHash>> {
    Ok(repo::attribution_path_blob(
        repo.store(),
        tree,
        &Default::default(),
        path,
    )?)
}

// Absent fields can enrich the same exact causal observation. Known conflicting
// values are retained separately; source arrival order never settles a conflict.
fn merge_identity(
    current: &mut AttributionOperationIdentity,
    patch: &AttributionOperationIdentity,
) -> bool {
    let mut next = current.clone();
    let mut conflict = false;
    macro_rules! claim {
        ($old:expr, $new:expr) => {
            match (&$old, &$new) {
                (Some(old), Some(new)) if old.value != new.value => conflict = true,
                (None, Some(new)) => $old = Some(new.clone()),
                _ => {}
            }
        };
    }
    claim!(next.harness, patch.harness);
    claim!(next.harness_version, patch.harness_version);
    for (old, new) in [
        (&mut next.selected, &patch.selected),
        (&mut next.response, &patch.response),
    ] {
        claim!(old.provider, new.provider);
        claim!(old.model, new.model);
        claim!(old.version, new.version);
        claim!(old.thought_level, new.thought_level);
    }
    macro_rules! scope {
        ($field:ident) => {
            match (&next.scope.$field, &patch.scope.$field) {
                (Some(a), Some(b)) if a != b => conflict = true,
                (None, Some(b)) => next.scope.$field = Some(b.clone()),
                _ => {}
            }
        };
    }
    scope!(request_id);
    scope!(response_id);
    scope!(attempt_id);
    scope!(harness_instance_id);
    match (next.harness_version_scope, patch.harness_version_scope) {
        (Some(a), Some(b)) if a != b => conflict = true,
        (None, Some(b)) => next.harness_version_scope = Some(b),
        _ => {}
    }
    for method in &patch.collection_methods {
        if !next.collection_methods.contains(method) {
            next.collection_methods.push(*method);
        }
    }
    if !conflict {
        *current = next;
    }
    conflict
}

#[cfg(test)]
#[path = "operation_attribution_tests.rs"]
mod tests;

fn database_error(_: rusqlite::Error) -> io::Error {
    io::Error::other("operation replay index unavailable")
}
fn replay_database(root: &Path) -> io::Result<rusqlite::Connection> {
    // macOS exposes its ordinary temp root through /var -> /private/var.
    // Resolve the repository root alias before SQLite's NOFOLLOW check;
    // keep NOFOLLOW on the control file and all paths beneath that root.
    let root = fs::canonicalize(root)?;
    let file = path(&root).with_extension("operations-seen.sqlite3");
    let connection = rusqlite::Connection::open_with_flags(
        file,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(database_error)?;
    connection.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS seen (id BLOB PRIMARY KEY NOT NULL CHECK(length(id)=32)) WITHOUT ROWID;").map_err(database_error)?;
    Ok(connection)
}
fn replay_seen(database: &rusqlite::Connection, key: &str) -> io::Result<bool> {
    let id = ContentHash::compute(key.as_bytes());
    database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM seen WHERE id=?1)",
            [id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .map_err(database_error)
}
