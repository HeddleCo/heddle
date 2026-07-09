// SPDX-License-Identifier: Apache-2.0
//! Optional change-monitor integration for cached worktree status.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::Instant,
};

use ignore::WalkBuilder;
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use objects::{error::HeddleError, object::Tree};
use rmp_serde::{decode::from_slice, encode::to_vec_named};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::{
    FsMonitorMode, FsMonitorSettings, WorktreeIndex,
    daemon::{
        EndpointState, HELPER_HOST, persist_endpoint, remove_endpoint, send_json_request,
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
    fn persist_current_cursor(repo_root: &Path, state_path: PathBuf) -> Result<(), HeddleError>;
}

struct LocalMonitor;
struct WatchmanMonitor;

/// fsmonitor wire-protocol version. The shared daemon scaffolding
/// stores this on the endpoint file (under `version`); fsmonitor's
/// verbs (`query`, `refresh`) speak v1 and have not been bumped.
/// The mount daemon ships with its own protocol version (v2) on a
/// separate endpoint file — see `crates/repo/src/daemon/mount.rs`.
const HELPER_PROTOCOL_VERSION: u32 = 1;
const HELPER_SPAWN_RETRIES: usize = 10;
const HELPER_SPAWN_RETRY_DELAY_MS: u64 = 50;

/// Query result and persisted state for one compare run.
#[derive(Debug, Default)]
pub(crate) struct ChangeMonitorSession {
    changed_paths: Option<BTreeSet<String>>,
    next_cursor: Option<String>,
    state_path: PathBuf,
    pending_snapshot: Option<MonitorSnapshotState>,
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
                reason: Some("disabled".to_string()),
                status: MonitorStatus::Disabled,
                ..Self::default()
            },
            FsMonitorMode::Native => try_local_helper_query(repo_root, &state_path)
                .unwrap_or(None)
                .unwrap_or_else(|| LocalMonitor::prepare(repo_root, state_path)),
            FsMonitorMode::Auto => {
                let session = try_local_helper_query(repo_root, &state_path)
                    .unwrap_or(None)
                    .unwrap_or_else(|| LocalMonitor::prepare(repo_root, state_path.clone()));
                if session.status != MonitorStatus::Disabled {
                    session
                } else {
                    WatchmanMonitor::prepare(repo_root, state_path)
                }
            }
            FsMonitorMode::Watchman => WatchmanMonitor::prepare(repo_root, state_path),
        }
    }

    pub(crate) fn changed_path_count(&self) -> u64 {
        self.changed_paths
            .as_ref()
            .map_or(0, |paths| paths.len() as u64)
    }

    pub(crate) fn can_skip_directory(
        &self,
        rel_path: &Path,
        tree: Option<&Tree>,
        index: &WorktreeIndex,
    ) -> bool {
        if self.status != MonitorStatus::Usable {
            return false;
        }
        let changed_paths = match &self.changed_paths {
            Some(paths) => paths,
            None => return false,
        };
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

    pub(crate) fn persist(&self) -> Result<(), HeddleError> {
        if let Some(snapshot) = &self.pending_snapshot {
            persist_snapshot(&snapshot_path(&self.state_path), snapshot)?;
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
}

pub(crate) fn persist_current_monitor_cursor(
    repo_root: &Path,
    settings: FsMonitorSettings,
) -> Result<(), HeddleError> {
    match settings.mode {
        FsMonitorMode::Off => Ok(()),
        FsMonitorMode::Native => {
            let state_path = repo_root.join(".heddle/state").join("fsmonitor.toml");
            if try_local_helper_refresh(repo_root, &state_path)? {
                Ok(())
            } else {
                LocalMonitor::persist_current_cursor(repo_root, state_path)
            }
        }
        FsMonitorMode::Auto => {
            let state_path = repo_root.join(".heddle/state").join("fsmonitor.toml");
            if try_local_helper_refresh(repo_root, &state_path)? {
                Ok(())
            } else {
                LocalMonitor::persist_current_cursor(repo_root, state_path)
            }
        }
        FsMonitorMode::Watchman => {
            let state_path = repo_root.join(".heddle/state").join("fsmonitor.toml");
            WatchmanMonitor::persist_current_cursor(repo_root, state_path)
        }
    }
}

pub fn run_local_monitor_helper(repo_root: &Path) -> Result<(), HeddleError> {
    let state_path = repo_root.join(".heddle/state").join("fsmonitor.toml");
    let endpoint_path = helper_endpoint_path(&state_path);
    if let Some(parent) = endpoint_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let listener = TcpListener::bind((HELPER_HOST, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    persist_endpoint(
        &endpoint_path,
        &EndpointState {
            version: HELPER_PROTOCOL_VERSION,
            host: HELPER_HOST.to_string(),
            port,
            pid: Some(std::process::id()),
        },
    )?;

    let mut server = LocalMonitorServer::new(repo_root.to_path_buf(), state_path)?;
    let result = run_local_monitor_helper_loop(&listener, &mut server);
    remove_endpoint(&endpoint_path);
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
        // `Disabled` also lets `FsMonitorMode::Auto` fall through to
        // Watchman. See docs/perf/cli-core-loop-todo.md.
        let _ = repo_root;
        ChangeMonitorSession {
            state_path,
            backend: Some("native"),
            reason: Some("helper_unavailable_no_full_scan".to_string()),
            status: MonitorStatus::Disabled,
            ..ChangeMonitorSession::default()
        }
    }

    fn persist_current_cursor(repo_root: &Path, state_path: PathBuf) -> Result<(), HeddleError> {
        // Same policy as `prepare`: do not full-tree scan just to advance a
        // cursor when the helper is down. A no-op keeps status cheap; the
        // helper's own `refresh` path still rebuilds snapshots under the
        // long-lived watcher.
        let _ = (repo_root, state_path);
        Ok(())
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
        let current_cursor = snapshot.generation.saturating_add(1);
        persist_cursor(&state_path, &current_cursor.to_string())?;
        Ok(Self {
            repo_root,
            state_path,
            snapshot_path,
            snapshot,
            current_cursor,
            startup_cursor: current_cursor,
            recent_changes: BTreeMap::new(),
            desync_reason: None,
            last_activity: Instant::now(),
            event_rx,
            _watcher: watcher,
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
        for changed_path in normalized_event_paths(&self.repo_root, &event) {
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

    fn persist_current_cursor(repo_root: &Path, state_path: PathBuf) -> Result<(), HeddleError> {
        let watch_project = run_watchman_json(&[
            Value::String("watch-project".to_string()),
            Value::String(repo_root.display().to_string()),
        ])?;
        let watch = required_string(&watch_project, "watch")?;
        let clock_response =
            run_watchman_json(&[Value::String("clock".to_string()), Value::String(watch)])?;
        let Some(clock) = optional_string(&clock_response, "clock") else {
            return Ok(());
        };
        persist_cursor(&state_path, &clock)
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

fn try_local_helper_query(
    repo_root: &Path,
    state_path: &Path,
) -> Result<Option<ChangeMonitorSession>, HeddleError> {
    let endpoint_path = helper_endpoint_path(state_path);
    let endpoint = match crate::daemon::load_endpoint(&endpoint_path) {
        Ok(endpoint) => endpoint,
        Err(HeddleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            spawn_local_helper_background(repo_root)?;
            return Ok(None);
        }
        Err(error) => {
            warn!(%error, path = %endpoint_path.display(), "Ignoring unreadable monitor helper endpoint");
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
            remove_endpoint(&endpoint_path);
            spawn_local_helper_background(repo_root)?;
            warn!(%error, host = %endpoint.host, port = endpoint.port, "Local monitor helper query failed; falling back");
            return Ok(None);
        }
    };

    Ok(Some(change_monitor_session_from_helper_response(
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
                remove_endpoint(&endpoint_path);
                if attempt == 0 && try_spawn_local_helper(repo_root, state_path)? {
                    continue;
                }
                warn!(%error, host = %endpoint.host, port = endpoint.port, "Local monitor helper refresh failed; falling back");
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
    spawn_local_helper_background(repo_root)?;
    let endpoint_path = helper_endpoint_path(state_path);
    for _ in 0..HELPER_SPAWN_RETRIES {
        if endpoint_path.exists() {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(
            HELPER_SPAWN_RETRY_DELAY_MS,
        ));
    }
    Ok(endpoint_path.exists())
}

fn spawn_local_helper_background(repo_root: &Path) -> Result<(), HeddleError> {
    let current_exe = std::env::current_exe().map_err(|error| {
        HeddleError::Config(format!("locate current heddle executable: {error}"))
    })?;
    if let Err(error) = Command::new(current_exe)
        .arg("--repo")
        .arg(repo_root)
        .arg("maintenance")
        .arg("monitor")
        .arg("--serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        warn!(%error, root = %repo_root.display(), "Failed to spawn local monitor helper");
    }
    Ok(())
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

fn change_monitor_session_from_helper_response(
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
    use std::collections::BTreeSet;

    use objects::object::ContentHash;

    use super::subtree_has_changes;
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
}
