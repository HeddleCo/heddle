// SPDX-License-Identifier: Apache-2.0
//! Optional change-monitor integration for cached worktree status.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use ignore::WalkBuilder;
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use objects::{error::HeddleError, object::Tree};
use rmp_serde::{decode::from_slice, encode::to_vec_named};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use crate::{
    FsMonitorMode, FsMonitorSettings, WorktreeIndex,
    daemon::{
        EndpointState, HELPER_HOST, persist_endpoint, pid_alive, remove_endpoint,
        remove_endpoint_if_owned, send_json_request,
        server::{
            DaemonHandler, IdleDecision, default_idle_policy, handle_json_connection,
            run_server_loop,
        },
    },
    worktree_walk::{cache_key, modified_parts},
};

const INITIAL_CLOCK: &str = "c:0:0";

#[derive(Debug, Default, Serialize, Deserialize)]
struct MonitorCursorState {
    #[serde(default)]
    clock: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MonitorStatus {
    #[default]
    Disabled,
    Usable,
    FreshInstance,
}

#[derive(Debug, Clone)]
pub struct ChangeMonitorReport {
    pub backend: String,
    pub status: String,
    pub reason: Option<String>,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MonitorHelperRequest {
    version: u32,
    command: String,
    since: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MonitorHelperResponse {
    version: u32,
    ok: bool,
    status: String,
    reason: Option<String>,
    clock: Option<String>,
    changed_paths: Vec<String>,
    error: Option<String>,
}

trait ChangeMonitorBackend {
    fn prepare(repo_root: &Path, state_path: PathBuf) -> ChangeMonitorSession;
}

struct LocalMonitor;
struct WatchmanMonitor;

/// fsmonitor wire-protocol version. The shared daemon scaffolding
/// stores this on the endpoint file (under `version`); fsmonitor's
/// verbs (`query`, `refresh`) speak v1 and have not been bumped.
/// The mount daemon ships with its own protocol version (v2) on a
/// separate endpoint file — see `crates/repo/src/daemon/mount.rs`.
const HELPER_PROTOCOL_VERSION: u32 = 1;
const HELPER_START_POLL_MS: u64 = 5;
const HELPER_START_POLLS: usize = 400;
/// Query result and persisted state for one compare run.
#[derive(Debug, Default)]
pub(crate) struct ChangeMonitorSession {
    changed_paths: Option<BTreeSet<String>>,
    next_cursor: Option<String>,
    state_path: PathBuf,
    pending_snapshot: Option<MonitorSnapshotState>,
    repo_root: PathBuf,
    establish_baseline: bool,
    pub(crate) backend: Option<&'static str>,
    pub(crate) reason: Option<String>,
    pub(crate) status: MonitorStatus,
}

impl ChangeMonitorSession {
    /// Prepare a change-monitor query for a worktree compare run.
    pub(crate) fn prepare(repo_root: &Path, settings: FsMonitorSettings) -> Self {
        let state_path = repo_root.join(".heddle/state").join("fsmonitor.toml");
        match settings.mode {
            FsMonitorMode::Off => Self {
                state_path,
                repo_root: repo_root.to_path_buf(),
                reason: Some("disabled".to_string()),
                status: MonitorStatus::Disabled,
                ..Self::default()
            },
            FsMonitorMode::Native => try_local_helper_query(repo_root, &state_path)
                .unwrap_or(None)
                .unwrap_or_else(|| LocalMonitor::prepare(repo_root, state_path)),
            FsMonitorMode::Auto if native_backend_supported() => {
                try_local_helper_query(repo_root, &state_path)
                    .unwrap_or(None)
                    .unwrap_or_else(|| LocalMonitor::prepare(repo_root, state_path))
            }
            FsMonitorMode::Auto => Self {
                state_path,
                repo_root: repo_root.to_path_buf(),
                backend: Some("off"),
                reason: Some("native_unsupported_platform".to_string()),
                status: MonitorStatus::Disabled,
                ..Self::default()
            },
            FsMonitorMode::Watchman => WatchmanMonitor::prepare(repo_root, state_path),
        }
    }

    pub(crate) fn changed_path_count(&self) -> u64 {
        self.changed_paths
            .as_ref()
            .map_or(0, |paths| paths.len() as u64)
    }

    pub(crate) fn changed_directory_keys(&self) -> BTreeSet<String> {
        let mut directories = BTreeSet::from([String::new()]);
        let Some(changed_paths) = &self.changed_paths else {
            return directories;
        };
        for changed in changed_paths {
            let mut current = Path::new(changed).parent();
            while let Some(path) = current {
                directories.insert(cache_key(path));
                current = path.parent();
            }
            directories.insert(changed.clone());
        }
        directories
    }

    pub(crate) fn path_may_have_changed(&self, rel_path: &Path) -> bool {
        if self.status != MonitorStatus::Usable {
            return true;
        }
        let Some(changed_paths) = &self.changed_paths else {
            return true;
        };
        // Ignore-policy files rewrite which paths are visible. Any change to
        // them invalidates per-path monitor skips so previously-ignored junk
        // reappears (and newly-ignored paths disappear) without requiring a
        // touch inside those trees (heddle#1155).
        if ignore_policy_paths_changed(changed_paths) {
            return true;
        }
        let key = cache_key(rel_path);
        changed_paths.iter().any(|changed| {
            changed == &key
                || changed
                    .strip_prefix(&key)
                    .is_some_and(|suffix| suffix.starts_with('/'))
                || key
                    .strip_prefix(changed)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    }

    pub(crate) fn can_filter_directory_children(
        &self,
        rel_path: &Path,
        index: &WorktreeIndex,
    ) -> bool {
        !subtree_skip_disabled()
            && self.status == MonitorStatus::Usable
            && self.changed_paths.is_some()
            && index.get_directory(&cache_key(rel_path)).is_some()
    }

    pub(crate) fn can_skip_directory(
        &self,
        rel_path: &Path,
        tree: Option<&Tree>,
        index: &WorktreeIndex,
    ) -> bool {
        if subtree_skip_disabled() {
            return false;
        }
        if self.status != MonitorStatus::Usable {
            return false;
        }
        let changed_paths = match &self.changed_paths {
            Some(paths) => paths,
            None => return false,
        };
        if ignore_policy_paths_changed(changed_paths) {
            return false;
        }
        let tree = match tree {
            Some(tree) => tree,
            None => return false,
        };

        let dir_key = cache_key(rel_path);
        let dir_entry = match index.get_directory(&dir_key) {
            Some(entry) => entry,
            None => return false,
        };
        let tree_hash = tree.hash();
        if dir_entry.clean_tree_hash.as_ref() != Some(&tree_hash) {
            return false;
        }

        !subtree_has_changes(changed_paths, &dir_key)
    }

    pub(crate) fn persist(&self, worktree_clean: bool) -> Result<(), HeddleError> {
        if !worktree_clean {
            return Ok(());
        }
        if let Some(snapshot) = &self.pending_snapshot {
            persist_snapshot(&snapshot_path(&self.state_path), snapshot)?;
        }
        if self.establish_baseline {
            return try_establish_local_helper_baseline(
                &self.repo_root,
                &self.state_path,
                self.next_cursor.as_deref(),
            );
        }
        let Some(cursor) = &self.next_cursor else {
            return Ok(());
        };
        persist_cursor(&self.state_path, cursor)
    }

    pub(crate) fn report(&self) -> ChangeMonitorReport {
        ChangeMonitorReport {
            backend: self.backend.unwrap_or("off").to_string(),
            status: match self.status {
                MonitorStatus::Disabled => "disabled",
                MonitorStatus::Usable => "usable",
                MonitorStatus::FreshInstance => "fresh_instance",
            }
            .to_string(),
            reason: self.reason.clone(),
            changed_paths: self
                .changed_paths
                .as_ref()
                .map(|paths| paths.iter().cloned().collect())
                .unwrap_or_default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_usable(repo_root: &Path, changed_paths: BTreeSet<String>) -> Self {
        Self {
            changed_paths: Some(changed_paths),
            state_path: repo_root.join(".heddle/state/fsmonitor.toml"),
            repo_root: repo_root.to_path_buf(),
            backend: Some("test"),
            status: MonitorStatus::Usable,
            ..Self::default()
        }
    }
}

fn subtree_skip_disabled() -> bool {
    std::env::var("HEDDLE_PERF_DISABLE_SUBTREE_SKIP")
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

/// Root ignore-policy files whose content rewrite which paths status/capture
/// may observe. A change to any of these forces a full ignore re-evaluation
/// under the change monitor (same as editing a tracked source file would).
fn ignore_policy_paths_changed(changed_paths: &BTreeSet<String>) -> bool {
    changed_paths.iter().any(|path| {
        path == ".heddleignore" || path == ".gitignore" || path == ".heddle/info/exclude"
    })
}

const fn native_backend_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Rebuild the native change-monitor snapshot + cursor sidecars from a full
/// worktree scan.
///
/// This is the deliberate, maintenance-time counterpart to the hot-path
/// `prepare`/`persist_current_cursor` no-op: `heddle maintenance run` is where
/// we are *supposed* to pay for a full `scan_snapshot_entries` walk to
/// (re)materialize `monitor-native.bin` + `fsmonitor.toml`, so a subsequent
/// live helper (or a later usable session) has a baseline to diff against.
/// The status hot path must never call this — it exists solely so maintenance
/// keeps refreshing the monitor sidecars it always has.
///
/// A live helper already owns the snapshot under its long-lived watcher, so
/// when one answers we leave it alone. `Off`/`Watchman` modes have no native
/// snapshot to build and are treated as no-ops.
pub(crate) fn rebuild_local_monitor_snapshot(
    repo_root: &Path,
    settings: FsMonitorSettings,
) -> Result<(), HeddleError> {
    match settings.mode {
        FsMonitorMode::Off | FsMonitorMode::Watchman => Ok(()),
        FsMonitorMode::Native | FsMonitorMode::Auto => {
            let state_path = repo_root.join(".heddle/state").join("fsmonitor.toml");
            if try_local_helper_refresh(repo_root, &state_path)? {
                return Ok(());
            }
            let previous = load_snapshot(&snapshot_path(&state_path)).unwrap_or_default();
            let next_generation = previous.generation.saturating_add(1);
            let snapshot = MonitorSnapshotState {
                version: MONITOR_SNAPSHOT_VERSION,
                generation: next_generation,
                entries: scan_snapshot_entries(repo_root)?,
            };
            persist_snapshot(&snapshot_path(&state_path), &snapshot)?;
            persist_cursor(&state_path, &next_generation.to_string())
        }
    }
}

pub fn run_local_monitor_helper(repo_root: &Path) -> Result<(), HeddleError> {
    let state_path = repo_root.join(".heddle/state").join("fsmonitor.toml");
    let endpoint_path = helper_endpoint_path(&state_path);
    let Some(_lifetime_lease) =
        objects::lock::RepoLock::at(helper_lifetime_lock_path(&state_path)).try_write()?
    else {
        return Ok(());
    };
    if let Some(parent) = endpoint_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let listener = TcpListener::bind((HELPER_HOST, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let endpoint = EndpointState {
        version: HELPER_PROTOCOL_VERSION,
        host: HELPER_HOST.to_string(),
        port,
        pid: Some(std::process::id()),
    };
    persist_endpoint(&endpoint_path, &endpoint)?;
    remove_starting_helper_if_owned(&state_path, std::process::id());

    let mut server = LocalMonitorServer::new(repo_root.to_path_buf(), state_path)?;
    let result = run_local_monitor_helper_loop(&listener, &mut server);
    remove_endpoint_if_owned(&endpoint_path, &endpoint);
    result
}

fn run_local_monitor_helper_loop(
    listener: &TcpListener,
    server: &mut LocalMonitorServer,
) -> Result<(), HeddleError> {
    // The fsmonitor's `LocalMonitorServer` is itself the daemon
    // handler — it owns the notify watcher state and the per-verb
    // dispatch. Implemented inline below.
    run_server_loop(listener, server)
}

impl DaemonHandler for LocalMonitorServer {
    fn handle(&mut self, stream: TcpStream) -> Result<(), HeddleError> {
        self.last_activity = Instant::now();
        handle_local_helper_stream(self, stream)
    }

    fn on_tick(&mut self, idle_for: std::time::Duration) -> IdleDecision {
        if self.shutdown_requested {
            return IdleDecision::Exit;
        }
        // fsmonitor drains pending notify events between accepts so
        // the change cursor stays current even when no CLI is
        // querying. Errors here historically propagated; preserve
        // that signal at the warn level.
        if let Err(error) = self.drain_events() {
            warn!(%error, "fsmonitor drain failed; will surface on next query");
        }
        default_idle_policy(idle_for)
    }
}

impl ChangeMonitorBackend for LocalMonitor {
    fn prepare(repo_root: &Path, state_path: PathBuf) -> ChangeMonitorSession {
        // In-process fallback when the helper daemon is unavailable.
        // Never pay for a full-tree WalkBuilder `scan_snapshot_entries` on
        // the status hot path: without a live watcher we cannot produce a
        // reliable changed-paths set, so return a session that simply never
        // skips directories (`can_skip_directory` requires `Usable`).
        // Once that correct fallback scan completes, attempt a non-blocking
        // baseline handshake with the watcher that was spawned by `prepare`.
        // If the endpoint is not ready yet, or the watcher observed a change
        // during the scan, no cursor is advanced and the next command safely
        // falls back again.
        ChangeMonitorSession {
            state_path,
            repo_root: repo_root.to_path_buf(),
            establish_baseline: true,
            backend: Some("native"),
            reason: Some("helper_unavailable_no_full_scan".to_string()),
            status: MonitorStatus::Disabled,
            ..ChangeMonitorSession::default()
        }
    }
}

struct LocalMonitorServer {
    repo_root: PathBuf,
    state_path: PathBuf,
    snapshot_path: PathBuf,
    snapshot: MonitorSnapshotState,
    current_cursor: u64,
    startup_cursor: u64,
    recent_changes: BTreeMap<String, u64>,
    desync_reason: Option<String>,
    last_activity: Instant,
    event_rx: Receiver<notify::Result<Event>>,
    _watcher: RecommendedWatcher,
    shutdown_requested: bool,
}

impl LocalMonitorServer {
    fn new(repo_root: PathBuf, state_path: PathBuf) -> Result<Self, HeddleError> {
        let snapshot_path = snapshot_path(&state_path);
        let snapshot = load_snapshot(&snapshot_path)?;
        let (event_tx, event_rx) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(
            move |result| {
                let _ = event_tx.send(result);
            },
            NotifyConfig::default(),
        )
        .map_err(|error| HeddleError::Config(format!("start native watcher: {error}")))?;
        watcher
            .watch(&repo_root, RecursiveMode::Recursive)
            .map_err(|error| HeddleError::Config(format!("watch repo root: {error}")))?;
        let persisted_cursor = load_cursor_state(&state_path)
            .clock
            .and_then(|clock| clock.parse::<u64>().ok())
            .unwrap_or_default();
        let helper_restarted = persisted_cursor > 0;
        let current_cursor = snapshot.generation.max(persisted_cursor).saturating_add(1);
        Ok(Self {
            repo_root,
            state_path,
            snapshot_path,
            snapshot,
            current_cursor,
            startup_cursor: current_cursor,
            recent_changes: BTreeMap::new(),
            desync_reason: helper_restarted.then(|| "helper_restart".to_string()),
            last_activity: Instant::now(),
            event_rx,
            _watcher: watcher,
            shutdown_requested: false,
        })
    }

    fn query(&mut self, since: Option<&str>) -> Result<MonitorHelperResponse, HeddleError> {
        self.drain_events()?;
        let since_cursor = since.and_then(|value| value.parse::<u64>().ok());
        let status = if self.desync_reason.is_none()
            && matches!(since_cursor, Some(cursor) if cursor >= self.startup_cursor && cursor <= self.current_cursor)
        {
            MonitorStatus::Usable
        } else {
            MonitorStatus::FreshInstance
        };
        let changed_paths = if status == MonitorStatus::Usable {
            self.recent_changes
                .iter()
                .filter(|(_, seq)| since_cursor.is_some_and(|since| **seq > since))
                .map(|(path, _)| path.clone())
                .collect()
        } else {
            Vec::new()
        };

        Ok(MonitorHelperResponse {
            version: HELPER_PROTOCOL_VERSION,
            ok: true,
            status: monitor_status_name(status).to_string(),
            reason: (status == MonitorStatus::FreshInstance).then_some(
                self.desync_reason.clone().unwrap_or_else(|| {
                    if self.current_cursor > self.startup_cursor {
                        "cursor_mismatch".to_string()
                    } else {
                        "fresh_instance".to_string()
                    }
                }),
            ),
            clock: Some(self.current_cursor.to_string()),
            changed_paths,
            error: None,
        })
    }

    fn refresh(&mut self) -> Result<MonitorHelperResponse, HeddleError> {
        self.drain_events()?;
        self.snapshot = MonitorSnapshotState {
            version: MONITOR_SNAPSHOT_VERSION,
            generation: self.current_cursor.saturating_add(1),
            entries: scan_snapshot_entries(&self.repo_root)?,
        };
        self.current_cursor = self.snapshot.generation;
        self.startup_cursor = self.current_cursor;
        self.recent_changes.clear();
        self.desync_reason = None;
        persist_snapshot(&self.snapshot_path, &self.snapshot)?;
        persist_cursor(&self.state_path, &self.current_cursor.to_string())?;

        Ok(MonitorHelperResponse {
            version: HELPER_PROTOCOL_VERSION,
            ok: true,
            status: monitor_status_name(MonitorStatus::Usable).to_string(),
            reason: None,
            clock: Some(self.current_cursor.to_string()),
            changed_paths: Vec::new(),
            error: None,
        })
    }

    fn establish_baseline(
        &mut self,
        expected_cursor: Option<&str>,
    ) -> Result<MonitorHelperResponse, HeddleError> {
        self.drain_events()?;
        let expected = expected_cursor.and_then(|cursor| cursor.parse::<u64>().ok());
        let stable = expected.map_or_else(
            || self.recent_changes.is_empty(),
            |cursor| cursor == self.current_cursor,
        );
        if !stable {
            return Ok(MonitorHelperResponse {
                version: HELPER_PROTOCOL_VERSION,
                ok: true,
                status: "fresh_instance".to_string(),
                reason: Some("changes_during_baseline".to_string()),
                clock: Some(self.current_cursor.to_string()),
                changed_paths: Vec::new(),
                error: None,
            });
        }

        self.startup_cursor = self.current_cursor;
        self.recent_changes.clear();
        self.desync_reason = None;
        persist_cursor(&self.state_path, &self.current_cursor.to_string())?;
        Ok(MonitorHelperResponse {
            version: HELPER_PROTOCOL_VERSION,
            ok: true,
            status: "usable".to_string(),
            reason: None,
            clock: Some(self.current_cursor.to_string()),
            changed_paths: Vec::new(),
            error: None,
        })
    }

    fn drain_events(&mut self) -> Result<(), HeddleError> {
        while let Ok(result) = self.event_rx.try_recv() {
            match result {
                Ok(event) => self.apply_event(event),
                Err(error) => {
                    self.desync_reason = Some(format!("watch_error:{error}"));
                    self.recent_changes.clear();
                }
            }
        }
        Ok(())
    }

    fn apply_event(&mut self, event: Event) {
        if should_ignore_event_kind(&event.kind) {
            return;
        }
        if event.paths.is_empty() {
            self.desync_reason = Some("overflow_or_dropped_event".to_string());
            self.recent_changes.clear();
            return;
        }
        let changed_paths = normalized_event_paths(&self.repo_root, &event);
        if changed_paths.is_empty() {
            return;
        }
        if matches!(event.kind, EventKind::Any | EventKind::Other) {
            self.desync_reason = Some("overflow_or_dropped_event".to_string());
            self.recent_changes.clear();
            return;
        }
        for changed_path in changed_paths {
            self.current_cursor = self.current_cursor.saturating_add(1);
            self.recent_changes
                .insert(changed_path, self.current_cursor);
        }
    }
}

impl ChangeMonitorBackend for WatchmanMonitor {
    fn prepare(repo_root: &Path, state_path: PathBuf) -> ChangeMonitorSession {
        let previous_clock = load_cursor_state(&state_path).clock;
        match watchman_query(repo_root, previous_clock.as_deref()) {
            Ok(result) => ChangeMonitorSession {
                changed_paths: (result.status == MonitorStatus::Usable)
                    .then_some(result.changed_paths),
                next_cursor: result.clock,
                state_path,
                pending_snapshot: None,
                repo_root: repo_root.to_path_buf(),
                establish_baseline: false,
                backend: Some("watchman"),
                reason: result.reason,
                status: result.status,
            },
            Err(error) => {
                warn!(%error, root = %repo_root.display(), "change monitor disabled for this run");
                ChangeMonitorSession {
                    state_path,
                    backend: Some("watchman"),
                    reason: Some(format!("watchman_error:{error}")),
                    status: MonitorStatus::Disabled,
                    ..ChangeMonitorSession::default()
                }
            }
        }
    }
}

#[derive(Debug)]
struct WatchmanQueryResult {
    changed_paths: BTreeSet<String>,
    clock: Option<String>,
    status: MonitorStatus,
    reason: Option<String>,
}

const MONITOR_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum SnapshotEntryKind {
    File,
    Symlink,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SnapshotEntry {
    modified_sec: i64,
    modified_nsec: u32,
    size: u64,
    kind: SnapshotEntryKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MonitorSnapshotState {
    #[serde(default = "default_snapshot_version")]
    version: u32,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    entries: BTreeMap<String, SnapshotEntry>,
}

fn default_snapshot_version() -> u32 {
    MONITOR_SNAPSHOT_VERSION
}

fn load_cursor_state(path: &Path) -> MonitorCursorState {
    let Ok(contents) = fs::read_to_string(path) else {
        return MonitorCursorState::default();
    };
    toml::from_str(&contents).unwrap_or_default()
}

fn helper_endpoint_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("monitor-helper.json")
}

fn helper_start_lock_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("monitor-helper-start.lock")
}

fn helper_lifetime_lock_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("monitor-helper-lifetime.lock")
}

fn helper_starting_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("monitor-helper-starting")
}

fn load_starting_helper_pid(state_path: &Path) -> Option<u32> {
    fs::read_to_string(helper_starting_path(state_path))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn persist_starting_helper_pid(state_path: &Path, pid: u32) -> Result<(), HeddleError> {
    objects::fs_atomic::write_file_atomic(
        &helper_starting_path(state_path),
        format!("{pid}\n").as_bytes(),
    )?;
    Ok(())
}

fn remove_starting_helper_if_owned(state_path: &Path, pid: u32) {
    if load_starting_helper_pid(state_path) == Some(pid) {
        let _ = fs::remove_file(helper_starting_path(state_path));
    }
}

fn try_local_helper_query(
    repo_root: &Path,
    state_path: &Path,
) -> Result<Option<ChangeMonitorSession>, HeddleError> {
    let endpoint_path = helper_endpoint_path(state_path);
    let endpoint = match crate::daemon::load_endpoint(&endpoint_path) {
        Ok(endpoint) => endpoint,
        Err(HeddleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let _ = try_spawn_local_helper(repo_root, state_path)?;
            return Ok(None);
        }
        Err(error) => {
            warn!(%error, path = %endpoint_path.display(), "Ignoring unreadable monitor helper endpoint");
            let _ = try_spawn_local_helper(repo_root, state_path)?;
            return Ok(None);
        }
    };
    let response: MonitorHelperResponse = match send_json_request(
        &endpoint,
        &MonitorHelperRequest {
            version: HELPER_PROTOCOL_VERSION,
            command: "query".to_string(),
            since: load_cursor_state(state_path).clock,
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            retire_failed_helper_endpoint(state_path, &endpoint)?;
            let _ = try_spawn_local_helper(repo_root, state_path)?;
            log_helper_fallback(&error, &endpoint, "query");
            return Ok(None);
        }
    };

    Ok(Some(change_monitor_session_from_helper_response(
        repo_root.to_path_buf(),
        state_path.to_path_buf(),
        &endpoint,
        response,
    )?))
}

fn try_local_helper_refresh(repo_root: &Path, state_path: &Path) -> Result<bool, HeddleError> {
    for attempt in 0..=1 {
        let endpoint_path = helper_endpoint_path(state_path);
        let endpoint = match crate::daemon::load_endpoint(&endpoint_path) {
            Ok(endpoint) => endpoint,
            Err(HeddleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if attempt == 0 && try_spawn_local_helper(repo_root, state_path)? {
                    continue;
                }
                return Ok(false);
            }
            Err(error) => {
                warn!(%error, path = %endpoint_path.display(), "Ignoring unreadable monitor helper endpoint");
                if attempt == 0 && try_spawn_local_helper(repo_root, state_path)? {
                    continue;
                }
                return Ok(false);
            }
        };
        let response: MonitorHelperResponse = match send_json_request(
            &endpoint,
            &MonitorHelperRequest {
                version: HELPER_PROTOCOL_VERSION,
                command: "refresh".to_string(),
                since: None,
            },
        ) {
            Ok(response) => response,
            Err(error) => {
                retire_failed_helper_endpoint(state_path, &endpoint)?;
                if attempt == 0 && try_spawn_local_helper(repo_root, state_path)? {
                    continue;
                }
                log_helper_fallback(&error, &endpoint, "refresh");
                return Ok(false);
            }
        };

        if !response.ok {
            return Ok(false);
        }
        if let Some(clock) = response.clock {
            persist_cursor(state_path, &clock)?;
        }
        return Ok(true);
    }

    Ok(false)
}

fn try_spawn_local_helper(repo_root: &Path, state_path: &Path) -> Result<bool, HeddleError> {
    try_spawn_local_helper_with(state_path, || {
        spawn_local_helper_background(repo_root, state_path)
    })
}

fn try_spawn_local_helper_with(
    state_path: &Path,
    spawn: impl FnOnce() -> Result<(), HeddleError>,
) -> Result<bool, HeddleError> {
    try_spawn_local_helper_with_probe(state_path, spawn, endpoint_is_current_and_live)
}

fn try_spawn_local_helper_with_probe(
    state_path: &Path,
    spawn: impl FnOnce() -> Result<(), HeddleError>,
    endpoint_is_live: impl Fn(&Path) -> bool,
) -> Result<bool, HeddleError> {
    let _start_lease = objects::lock::RepoLock::at(helper_start_lock_path(state_path)).write()?;
    let endpoint_path = helper_endpoint_path(state_path);
    if endpoint_is_live(&endpoint_path) {
        return Ok(true);
    }

    let lifetime_lock = objects::lock::RepoLock::at(helper_lifetime_lock_path(state_path));
    if let Some(lifetime_lease) = lifetime_lock.try_write()? {
        match crate::daemon::load_endpoint(&endpoint_path) {
            Ok(endpoint) => {
                remove_endpoint_if_owned(&endpoint_path, &endpoint);
            }
            Err(HeddleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => remove_endpoint(&endpoint_path),
        }
        drop(lifetime_lease);
        spawn()?;
    }

    Ok(endpoint_is_live(&endpoint_path))
}

fn try_establish_local_helper_baseline(
    repo_root: &Path,
    state_path: &Path,
    expected_cursor: Option<&str>,
) -> Result<(), HeddleError> {
    let endpoint_path = helper_endpoint_path(state_path);
    let endpoint = match crate::daemon::load_endpoint(&endpoint_path) {
        Ok(endpoint) => endpoint,
        Err(_) => {
            let _ = try_spawn_local_helper(repo_root, state_path)?;
            return Ok(());
        }
    };
    let response: MonitorHelperResponse = match send_json_request(
        &endpoint,
        &MonitorHelperRequest {
            version: HELPER_PROTOCOL_VERSION,
            command: "baseline".to_string(),
            since: expected_cursor.map(str::to_string),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            retire_failed_helper_endpoint(state_path, &endpoint)?;
            let _ = try_spawn_local_helper(repo_root, state_path)?;
            log_helper_fallback(&error, &endpoint, "baseline");
            return Ok(());
        }
    };
    if !response.ok {
        return Err(HeddleError::Config(
            response
                .error
                .unwrap_or_else(|| "native monitor baseline failed".to_string()),
        ));
    }
    Ok(())
}

fn endpoint_is_current_and_live(path: &Path) -> bool {
    crate::daemon::load_endpoint(path).is_ok_and(|endpoint| {
        endpoint.version == HELPER_PROTOCOL_VERSION && endpoint.pid.is_none_or(pid_alive)
    })
}

fn retire_failed_helper_endpoint(
    state_path: &Path,
    expected: &EndpointState,
) -> Result<(), HeddleError> {
    let lifetime_lock = objects::lock::RepoLock::at(helper_lifetime_lock_path(state_path));
    if let Some(_lease) = lifetime_lock.try_write()? {
        remove_endpoint_if_owned(&helper_endpoint_path(state_path), expected);
    }
    Ok(())
}

fn log_helper_fallback(error: &HeddleError, endpoint: &EndpointState, operation: &'static str) {
    match error {
        HeddleError::Io(_) => {
            debug!(%error, host = %endpoint.host, port = endpoint.port, operation, "Local monitor helper unavailable; falling back");
        }
        _ => {
            warn!(%error, host = %endpoint.host, port = endpoint.port, operation, "Local monitor helper protocol failed; falling back");
        }
    }
}

fn spawn_local_helper_background(repo_root: &Path, state_path: &Path) -> Result<(), HeddleError> {
    let current_exe = std::env::current_exe()
        .map_err(|error| HeddleError::Config(format!("locate heddle executable: {error}")))?;
    let helper = local_helper_binary_for_executable(&current_exe);
    let mut child = match Command::new(helper)
        .arg("--repo-root")
        .arg(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            warn!(%error, root = %repo_root.display(), "Failed to spawn local monitor helper");
            return Ok(());
        }
    };
    let pid = child.id();
    if let Err(error) = persist_starting_helper_pid(state_path, pid) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    if crate::daemon::load_endpoint(&helper_endpoint_path(state_path))
        .ok()
        .and_then(|endpoint| endpoint.pid)
        == Some(pid)
    {
        remove_starting_helper_if_owned(state_path, pid);
    }
    Ok(())
}

fn local_helper_binary_for_executable(executable: &Path) -> PathBuf {
    let helper_name = format!("heddle-fsmonitor-worker{}", std::env::consts::EXE_SUFFIX);
    let adjacent = executable.with_file_name(&helper_name);
    if adjacent.is_file() {
        return adjacent;
    }
    executable
        .canonicalize()
        .map(|resolved| resolved.with_file_name(helper_name))
        .unwrap_or(adjacent)
}

fn handle_local_helper_stream(
    server: &mut LocalMonitorServer,
    stream: TcpStream,
) -> Result<(), HeddleError> {
    handle_json_connection(stream, |request: MonitorHelperRequest| {
        handle_local_helper_request(server, request)
    })
}

fn handle_local_helper_request(
    server: &mut LocalMonitorServer,
    request: MonitorHelperRequest,
) -> MonitorHelperResponse {
    let result = match request.command.as_str() {
        "query" => server.query(request.since.as_deref()),
        "refresh" => server.refresh(),
        "baseline" => server.establish_baseline(request.since.as_deref()),
        "shutdown" => {
            server.shutdown_requested = true;
            Ok(MonitorHelperResponse {
                version: HELPER_PROTOCOL_VERSION,
                ok: true,
                status: "disabled".to_string(),
                reason: Some("shutdown".to_string()),
                clock: None,
                changed_paths: Vec::new(),
                error: None,
            })
        }
        command => Err(HeddleError::Config(format!(
            "unknown helper command: {command}"
        ))),
    };

    match result {
        Ok(response) => response,
        Err(error) => MonitorHelperResponse {
            version: HELPER_PROTOCOL_VERSION,
            ok: false,
            status: "disabled".to_string(),
            reason: None,
            clock: None,
            changed_paths: Vec::new(),
            error: Some(error.to_string()),
        },
    }
}

/// Proof that no native change-monitor helper can create files beneath a
/// worktree while its checkout directory is being removed.
pub struct LocalMonitorShutdownGuard {
    _start_lease: objects::lock::WriteLockGuard,
    _lifetime_lease: objects::lock::WriteLockGuard,
}

/// Stop and drain the native change monitor for `repo_root`.
///
/// The returned guard keeps both the startup and lifetime locks held. Callers
/// removing the worktree must retain it until recursive removal completes.
pub fn shutdown_local_monitor_helper(
    repo_root: &Path,
) -> Result<LocalMonitorShutdownGuard, HeddleError> {
    let state_path = repo_root.join(".heddle/state/fsmonitor.toml");
    let start_lease = objects::lock::RepoLock::at(helper_start_lock_path(&state_path)).write()?;
    let endpoint_path = helper_endpoint_path(&state_path);

    let mut endpoint = crate::daemon::load_endpoint(&endpoint_path).ok();
    if endpoint.is_none()
        && let Some(starting_pid) = load_starting_helper_pid(&state_path)
    {
        for _ in 0..HELPER_START_POLLS {
            endpoint = crate::daemon::load_endpoint(&endpoint_path).ok();
            if endpoint.is_some() || !pid_alive(starting_pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(HELPER_START_POLL_MS));
        }
        if endpoint.is_none() && pid_alive(starting_pid) {
            return Err(HeddleError::Config(format!(
                "native monitor helper {starting_pid} did not finish starting before teardown"
            )));
        }
        remove_starting_helper_if_owned(&state_path, starting_pid);
    }

    if let Some(endpoint) = endpoint {
        let response: Result<MonitorHelperResponse, HeddleError> = send_json_request(
            &endpoint,
            &MonitorHelperRequest {
                version: HELPER_PROTOCOL_VERSION,
                command: "shutdown".to_string(),
                since: None,
            },
        );
        match response {
            Ok(response) if !response.ok => {
                return Err(HeddleError::Config(
                    response
                        .error
                        .unwrap_or_else(|| "native monitor refused shutdown".to_string()),
                ));
            }
            Ok(_) => {}
            Err(error) => {
                log_helper_fallback(&error, &endpoint, "shutdown");
            }
        }
    }

    let lifetime_lock = objects::lock::RepoLock::at(helper_lifetime_lock_path(&state_path));
    let mut lifetime_lease = None;
    for _ in 0..HELPER_START_POLLS {
        if let Some(lease) = lifetime_lock.try_write()? {
            lifetime_lease = Some(lease);
            break;
        }
        std::thread::sleep(Duration::from_millis(HELPER_START_POLL_MS));
    }
    let lifetime_lease = lifetime_lease.ok_or_else(|| {
        HeddleError::Config("native monitor helper did not drain before teardown".to_string())
    })?;

    remove_endpoint(&endpoint_path);
    let _ = fs::remove_file(helper_starting_path(&state_path));
    for artifact in [&state_path, &snapshot_path(&state_path)] {
        match fs::remove_file(artifact) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(HeddleError::Io(error)),
        }
    }

    Ok(LocalMonitorShutdownGuard {
        _start_lease: start_lease,
        _lifetime_lease: lifetime_lease,
    })
}

fn change_monitor_session_from_helper_response(
    repo_root: PathBuf,
    state_path: PathBuf,
    endpoint: &EndpointState,
    response: MonitorHelperResponse,
) -> Result<ChangeMonitorSession, HeddleError> {
    if !response.ok {
        return Err(HeddleError::Config(response.error.unwrap_or_else(|| {
            format!(
                "helper {}:{} returned an unknown error",
                endpoint.host, endpoint.port
            )
        })));
    }

    let status = match response.status.as_str() {
        "usable" => MonitorStatus::Usable,
        "fresh_instance" => MonitorStatus::FreshInstance,
        _ => MonitorStatus::Disabled,
    };

    Ok(ChangeMonitorSession {
        changed_paths: (status == MonitorStatus::Usable)
            .then_some(response.changed_paths.into_iter().collect()),
        next_cursor: response.clock,
        state_path,
        pending_snapshot: None,
        repo_root,
        establish_baseline: status != MonitorStatus::Usable,
        backend: Some("native-helper"),
        reason: response.reason,
        status,
    })
}

fn monitor_status_name(status: MonitorStatus) -> &'static str {
    match status {
        MonitorStatus::Disabled => "disabled",
        MonitorStatus::Usable => "usable",
        MonitorStatus::FreshInstance => "fresh_instance",
    }
}

fn should_ignore_event_kind(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Access(_))
}

fn normalized_event_paths(repo_root: &Path, event: &Event) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for path in &event.paths {
        let Ok(rel_path) = path.strip_prefix(repo_root) else {
            continue;
        };
        if rel_path.as_os_str().is_empty() || should_exclude_monitor_path(rel_path) {
            continue;
        }
        paths.insert(rel_path.to_string_lossy().replace('\\', "/"));
    }
    paths.into_iter().collect()
}

fn snapshot_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("monitor-native.bin")
}

fn load_snapshot(path: &Path) -> Result<MonitorSnapshotState, HeddleError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MonitorSnapshotState::default());
        }
        Err(error) => return Err(HeddleError::Io(error)),
    };
    let snapshot: MonitorSnapshotState = from_slice(&bytes)
        .map_err(|error| HeddleError::Config(format!("decode monitor snapshot: {error}")))?;
    if snapshot.version != MONITOR_SNAPSHOT_VERSION {
        return Ok(MonitorSnapshotState::default());
    }
    Ok(snapshot)
}

fn persist_snapshot(path: &Path, snapshot: &MonitorSnapshotState) -> Result<(), HeddleError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = to_vec_named(snapshot)
        .map_err(|error| HeddleError::Config(format!("encode monitor snapshot: {error}")))?;
    objects::fs_atomic::write_file_atomic(path, &bytes)?;
    Ok(())
}

fn scan_snapshot_entries(repo_root: &Path) -> Result<BTreeMap<String, SnapshotEntry>, HeddleError> {
    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(false)
        .follow_links(false)
        .build();
    let mut entries = BTreeMap::new();

    for entry in walker {
        let entry =
            entry.map_err(|error| HeddleError::Io(std::io::Error::other(error.to_string())))?;
        let path = entry.path();
        if path == repo_root {
            continue;
        }
        let rel_path = path.strip_prefix(repo_root).unwrap_or(path);
        if should_exclude_monitor_path(rel_path) {
            continue;
        }
        let metadata = path.symlink_metadata()?;
        let Some((modified_sec, modified_nsec)) = modified_parts(&metadata) else {
            continue;
        };
        let kind = if metadata.file_type().is_symlink() {
            SnapshotEntryKind::Symlink
        } else if metadata.is_dir() {
            SnapshotEntryKind::Directory
        } else {
            SnapshotEntryKind::File
        };
        entries.insert(
            rel_path.to_string_lossy().replace('\\', "/"),
            SnapshotEntry {
                modified_sec,
                modified_nsec,
                size: metadata.len(),
                kind,
            },
        );
    }

    Ok(entries)
}

fn should_exclude_monitor_path(rel_path: &Path) -> bool {
    rel_path
        .components()
        .next()
        .is_some_and(|component| matches!(component.as_os_str().to_str(), Some(".heddle")))
}

fn persist_cursor(state_path: &Path, clock: &str) -> Result<(), HeddleError> {
    if load_cursor_state(state_path).clock.as_deref() == Some(clock) {
        return Ok(());
    }
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let state = MonitorCursorState {
        clock: Some(clock.to_string()),
    };
    let contents = toml::to_string_pretty(&state)
        .map_err(|error| HeddleError::Config(format!("serialize fsmonitor state: {error}")))?;
    objects::fs_atomic::write_file_atomic(state_path, contents.as_bytes())?;
    Ok(())
}

fn subtree_has_changes(changed_paths: &BTreeSet<String>, dir_key: &str) -> bool {
    if dir_key.is_empty() {
        return !changed_paths.is_empty();
    }
    let prefix = format!("{dir_key}/");
    changed_paths
        .range(dir_key.to_string()..)
        .next()
        .is_some_and(|path| path == dir_key || path.starts_with(&prefix))
}

fn watchman_query(
    repo_root: &Path,
    since: Option<&str>,
) -> Result<WatchmanQueryResult, HeddleError> {
    let watch_project = run_watchman_json(&[
        Value::String("watch-project".to_string()),
        Value::String(repo_root.display().to_string()),
    ])?;
    let watch = required_string(&watch_project, "watch")?;
    let relative_root = optional_string(&watch_project, "relative_path");
    let since_clock = since.unwrap_or(INITIAL_CLOCK);

    let mut query = serde_json::Map::new();
    query.insert("fields".to_string(), serde_json::json!(["name"]));
    query.insert("since".to_string(), Value::String(since_clock.to_string()));
    query.insert(
        "expression".to_string(),
        serde_json::json!(["not", ["dirname", ".heddle"]]),
    );
    if let Some(relative_root) = &relative_root {
        query.insert(
            "relative_root".to_string(),
            Value::String(relative_root.clone()),
        );
    }

    let query_response = run_watchman_json(&[
        Value::String("query".to_string()),
        Value::String(watch),
        Value::Object(query),
    ])?;
    let fresh_instance = query_response
        .get("is_fresh_instance")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let files = query_response
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| HeddleError::Config("watchman query response missing files".to_string()))?;
    let mut changed_paths = BTreeSet::new();
    for file in files {
        let Some(name) = file
            .as_str()
            .or_else(|| file.get("name").and_then(Value::as_str))
        else {
            continue;
        };
        let normalized = match &relative_root {
            Some(root) if !root.is_empty() => format!("{root}/{name}"),
            _ => name.to_string(),
        };
        changed_paths.insert(normalized.replace('\\', "/"));
    }

    Ok(WatchmanQueryResult {
        changed_paths,
        clock: optional_string(&query_response, "clock"),
        status: if fresh_instance {
            MonitorStatus::FreshInstance
        } else {
            MonitorStatus::Usable
        },
        reason: fresh_instance.then_some("fresh_instance".to_string()),
    })
}

fn run_watchman_json(command: &[Value]) -> Result<Value, HeddleError> {
    let mut child = Command::new("watchman")
        .arg("-j")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| HeddleError::Config(format!("spawn watchman: {error}")))?;

    if let Some(mut stdin) = child.stdin.take() {
        serde_json::to_writer(&mut stdin, command)
            .map_err(|error| HeddleError::Config(format!("encode watchman query: {error}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|error| HeddleError::Config(format!("run watchman query: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(HeddleError::Config(format!(
            "watchman query failed: {}",
            stderr.trim()
        )));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| HeddleError::Config(format!("decode watchman response: {error}")))
}

fn required_string(value: &Value, key: &str) -> Result<String, HeddleError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| HeddleError::Config(format!("watchman response missing {key}")))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use objects::object::ContentHash;
    use tempfile::TempDir;

    use super::{
        HELPER_HOST, HELPER_PROTOCOL_VERSION, helper_endpoint_path,
        local_helper_binary_for_executable, shutdown_local_monitor_helper, subtree_has_changes,
        try_spawn_local_helper_with, try_spawn_local_helper_with_probe,
    };
    use crate::{DirectoryCacheEntry, WorktreeIndex};

    #[test]
    fn subtree_matching_handles_root_and_prefixes() {
        let changed_paths =
            BTreeSet::from(["src/lib.rs".to_string(), "tests/status.rs".to_string()]);

        assert!(subtree_has_changes(&changed_paths, ""));
        assert!(subtree_has_changes(&changed_paths, "src"));
        assert!(subtree_has_changes(&changed_paths, "tests"));
        assert!(!subtree_has_changes(&changed_paths, "docs"));
    }

    #[test]
    fn skip_requires_matching_clean_tree_hash() {
        let tree_hash = ContentHash::from_bytes([7; 32]);
        let mut index = WorktreeIndex::new();
        index.insert_directory(
            "src".to_string(),
            DirectoryCacheEntry {
                mtime_sec: 0,
                mtime_nsec: 0,
                child_count: 1,
                child_digest: DirectoryCacheEntry::digest_for_child_names(
                    ["lib.rs"].into_iter(),
                    1,
                )
                .unwrap(),
                clean_tree_hash: Some(tree_hash),
            },
        );

        let cached = index.get_directory("src").unwrap();
        assert_eq!(cached.clean_tree_hash, Some(tree_hash));
    }

    #[test]
    fn concurrent_cold_start_spawns_one_worker() {
        let temp = TempDir::new().unwrap();
        let state_path = Arc::new(temp.path().join(".heddle/state/fsmonitor.toml"));
        let barrier = Arc::new(Barrier::new(8));
        let spawns = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();

        for _ in 0..8 {
            let state_path = Arc::clone(&state_path);
            let barrier = Arc::clone(&barrier);
            let spawns = Arc::clone(&spawns);
            threads.push(thread::spawn(move || {
                barrier.wait();
                try_spawn_local_helper_with(&state_path, || {
                    spawns.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(std::time::Duration::from_millis(20));
                    crate::daemon::persist_endpoint(
                        &helper_endpoint_path(&state_path),
                        &crate::daemon::EndpointState {
                            version: HELPER_PROTOCOL_VERSION,
                            host: HELPER_HOST.to_string(),
                            port: 9911,
                            pid: Some(std::process::id()),
                        },
                    )
                })
                .unwrap()
            }));
        }

        assert!(threads.into_iter().all(|thread| thread.join().unwrap()));
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cold_start_checks_worker_readiness_only_before_and_after_spawn() {
        let temp = TempDir::new().unwrap();
        let state_path = temp.path().join(".heddle/state/fsmonitor.toml");
        let readiness_checks = AtomicUsize::new(0);
        let spawns = AtomicUsize::new(0);
        let ready = try_spawn_local_helper_with_probe(
            &state_path,
            || {
                spawns.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_| {
                readiness_checks.fetch_add(1, Ordering::SeqCst);
                false
            },
        )
        .unwrap();
        assert!(!ready);
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        assert_eq!(readiness_checks.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn shutdown_drains_native_watcher_before_checkout_removal() {
        let temp = TempDir::new().unwrap();
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(checkout.join(".heddle/state")).unwrap();
        std::fs::write(checkout.join("tracked.txt"), b"tracked\n").unwrap();
        let state_path = checkout.join(".heddle/state/fsmonitor.toml");
        let endpoint_path = helper_endpoint_path(&state_path);
        let helper_root = checkout.clone();
        let helper = thread::spawn(move || super::run_local_monitor_helper(&helper_root));

        for _ in 0..400 {
            if endpoint_path.exists() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            endpoint_path.exists(),
            "native helper endpoint should appear"
        );

        let shutdown = shutdown_local_monitor_helper(&checkout).unwrap();
        assert!(
            !endpoint_path.exists(),
            "shutdown should remove the helper endpoint"
        );
        objects::fs_ops::remove_path_recursively(&checkout).unwrap();
        drop(shutdown);

        helper.join().unwrap().unwrap();
        assert!(
            !checkout.exists(),
            "a drained watcher must not recreate its checkout"
        );
    }

    #[test]
    fn shutdown_tolerates_stale_endpoint_after_watcher_exit() {
        let temp = TempDir::new().unwrap();
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(checkout.join(".heddle/state")).unwrap();
        let state_path = checkout.join(".heddle/state/fsmonitor.toml");
        let endpoint_path = helper_endpoint_path(&state_path);
        let listener = std::net::TcpListener::bind((HELPER_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        crate::daemon::persist_endpoint(
            &endpoint_path,
            &crate::daemon::EndpointState {
                version: HELPER_PROTOCOL_VERSION,
                host: HELPER_HOST.to_string(),
                port,
                // A stale endpoint can outlive its worker and its PID can be
                // reused, so liveness alone cannot prove the socket is live.
                pid: Some(std::process::id()),
            },
        )
        .unwrap();

        let shutdown = shutdown_local_monitor_helper(&checkout).unwrap();
        assert!(!endpoint_path.exists());
        drop(shutdown);
    }

    #[test]
    fn overflow_and_missing_cursor_force_fresh_instance() {
        let temp = TempDir::new().unwrap();
        let state_path = temp.path().join(".heddle/state/fsmonitor.toml");
        let mut server =
            super::LocalMonitorServer::new(temp.path().to_path_buf(), state_path).unwrap();

        let missing = server.query(None).unwrap();
        assert_eq!(missing.status, "fresh_instance");

        let cursor = server.current_cursor.to_string();
        server.apply_event(notify::Event::new(notify::EventKind::Other));
        let overflow = server.query(Some(&cursor)).unwrap();
        assert_eq!(overflow.status, "fresh_instance");
        assert_eq!(
            overflow.reason.as_deref(),
            Some("overflow_or_dropped_event")
        );
        assert!(overflow.changed_paths.is_empty());
    }

    #[test]
    fn internal_other_events_do_not_desync_the_worktree_monitor() {
        let temp = TempDir::new().unwrap();
        let state_path = temp.path().join(".heddle/state/fsmonitor.toml");
        let mut server =
            super::LocalMonitorServer::new(temp.path().to_path_buf(), state_path).unwrap();
        let cursor = server.current_cursor.to_string();
        server.apply_event(
            notify::Event::new(notify::EventKind::Other)
                .add_path(temp.path().join(".heddle/state/index.bin")),
        );

        let response = server.query(Some(&cursor)).unwrap();

        assert_eq!(response.status, "usable");
        assert!(response.changed_paths.is_empty());
    }

    #[test]
    fn helper_restart_invalidates_a_durable_cursor() {
        let temp = TempDir::new().unwrap();
        let state_path = temp.path().join(".heddle/state/fsmonitor.toml");
        super::persist_cursor(&state_path, "7").unwrap();

        let mut restarted =
            super::LocalMonitorServer::new(temp.path().to_path_buf(), state_path).unwrap();
        let response = restarted.query(Some("7")).unwrap();

        assert_eq!(response.status, "fresh_instance");
        assert_eq!(response.reason.as_deref(), Some("helper_restart"));
    }

    #[test]
    fn auto_native_backend_is_explicitly_platform_gated() {
        assert_eq!(super::native_backend_supported(), cfg!(target_os = "linux"));
    }

    #[cfg(unix)]
    #[test]
    fn helper_resolution_follows_a_symlinked_cli_to_its_package() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let package_bin = temp.path().join("Cellar/heddle/1.0/bin");
        let linked_bin = temp.path().join("bin");
        std::fs::create_dir_all(&package_bin).unwrap();
        std::fs::create_dir_all(&linked_bin).unwrap();
        let executable = package_bin.join("heddle");
        let worker = package_bin.join("heddle-fsmonitor-worker");
        std::fs::write(&executable, b"cli").unwrap();
        std::fs::write(&worker, b"worker").unwrap();
        let linked_executable = linked_bin.join("heddle");
        symlink(&executable, &linked_executable).unwrap();

        assert_eq!(
            local_helper_binary_for_executable(&linked_executable),
            worker.canonicalize().unwrap()
        );
    }
}
