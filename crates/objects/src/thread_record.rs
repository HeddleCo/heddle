// SPDX-License-Identifier: Apache-2.0
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A Git-valid thread identity. Command breadcrumbs must quote its exact UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ThreadId(String);

impl ThreadId {
    /// Validate Git branch syntax, the 1024-byte full-ref limit and native reservation.
    pub fn new(value: impl Into<String>) -> Result<Self, ThreadIdError> {
        let value = value.into();
        validate_thread_id(&value)?;
        Ok(Self(value))
    }

    /// Wrap a value WITHOUT validation. Reserved for inputs that are
    /// safe-by-construction: fields read through validated thread records
    /// and internally-generated ids. Never call this on user/external input — use [`ThreadId::new`].
    pub(crate) fn new_unchecked(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ThreadId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Rejection from [`ThreadId::new`] / [`validate_thread_id`]. Its `Display` is
/// a clear, actionable CLI message naming the offending input and suggesting a
/// valid rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadIdError {
    input: String,
    suggestion: String,
}

impl ThreadIdError {
    pub fn suggestion(&self) -> &str {
        &self.suggestion
    }
}

impl std::fmt::Display for ThreadIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.input.is_empty() {
            write!(f, "thread name must not be empty")
        } else {
            write!(
                f,
                "thread name '{}' is invalid: use a Git branch name other than HEAD or the reserved heddle/ namespace (full ref at most 1024 UTF-8 bytes) — try '{}'",
                self.input, self.suggestion
            )
        }
    }
}

impl std::error::Error for ThreadIdError {}

/// Git branch syntax is owned by Sley, including the valid short name `@`.
pub fn validate_thread_id(value: &str) -> Result<(), ThreadIdError> {
    let git_name = crate::name_encoding::git_name(value);
    if git_name != "HEAD"
        && git_name.len() + "refs/heads/".len() <= 1024
        && sley_refs::BranchRefNameBuf::from_branch_name(&git_name).is_ok()
        && !crate::object::is_reserved_heddle_namespace(value)
    {
        Ok(())
    } else {
        Err(ThreadIdError {
            input: value.to_string(),
            suggestion: suggest_thread_id(value),
        })
    }
}

/// Best-effort slugify for the rename hint: map every disallowed character to
/// `-`, collapse runs, drop `..`, and trim. Always returns a non-empty,
/// [`validate_thread_id`]-valid string.
fn suggest_thread_id(value: &str) -> String {
    let value = if crate::object::is_reserved_heddle_namespace(value) {
        value.split_once('/').map(|(_, rest)| rest).unwrap_or(value)
    } else {
        value
    };
    let mut slug = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    while slug.contains("..") {
        slug = slug.replace("..", "-");
    }
    let trimmed = slug.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "thread".to_string()
    } else {
        let mut suggestion = trimmed[..trimmed.len().min(1000)]
            .trim_end_matches('.')
            .to_string();
        if suggestion == "HEAD" || suggestion.ends_with(".lock") {
            suggestion.push_str("-thread");
        }
        suggestion
    }
}

/// How a thread's worktree is realised on disk. Three flavours:
///
/// * [`ThreadMode::Materialized`] — clonefile-or-reflink the captured
///   tree into a thread directory. Real `read(2)`-able bytes, ~zero
///   disk cost via shared extents (APFS / btrfs / XFS w/ reflinks).
///   Day-one default on reflink-capable filesystems and the path the
///   stat-cache fast no-op + manifest sidecar were built for. See
///   `docs/design/clonefile-threads.md`.
/// * [`ThreadMode::Virtualized`] — project the captured tree through
///   a content-addressed FUSE/FSKit/ProjFS mount. Nothing on disk
///   until the kernel asks. Useful for repos too large to materialize
///   or when the CAS is remote-backed.
/// * [`ThreadMode::Solid`] — full file copies with no shared extents.
///   Strong isolation; the only choice on ext4 / NTFS hosts that have
///   neither reflinks nor a usable mount API.
///
/// The discriminant names match the user-facing `--workspace` flag
/// values so a single vocabulary spans the CLI, the JSON contract,
/// and the thread record on disk. Pre-rename data using the older
/// `"lightweight"` (clonefile) / `"materialized"` (full-copy) names
/// will fail to deserialize and require a re-export — intentional;
/// silently degrading isolation modes is the wrong default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadMode {
    Materialized,
    Virtualized,
    Solid,
}

impl std::fmt::Display for ThreadMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThreadMode::Materialized => write!(f, "materialized"),
            ThreadMode::Virtualized => write!(f, "virtualized"),
            ThreadMode::Solid => write!(f, "solid"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Draft,
    Active,
    Ready,
    Blocked,
    Merged,
    Abandoned,
    Promoted,
}

impl std::fmt::Display for ThreadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThreadState::Draft => write!(f, "draft"),
            ThreadState::Active => write!(f, "active"),
            ThreadState::Ready => write!(f, "ready"),
            ThreadState::Blocked => write!(f, "blocked"),
            ThreadState::Merged => write!(f, "merged"),
            ThreadState::Abandoned => write!(f, "abandoned"),
            ThreadState::Promoted => write!(f, "promoted"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadFreshness {
    Current,
    Stale,
    Unknown,
}

impl std::fmt::Display for ThreadFreshness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThreadFreshness::Current => write!(f, "current"),
            ThreadFreshness::Stale => write!(f, "stale"),
            ThreadFreshness::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadImpactCategory {
    DependencyGraph,
    BuildRuntimeConfig,
    GeneratedOutputs,
    RepoWideRefactor,
    PublicApiSurface,
}

impl std::fmt::Display for ThreadImpactCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThreadImpactCategory::DependencyGraph => write!(f, "dependency_graph"),
            ThreadImpactCategory::BuildRuntimeConfig => write!(f, "build_runtime_config"),
            ThreadImpactCategory::GeneratedOutputs => write!(f, "generated_outputs"),
            ThreadImpactCategory::RepoWideRefactor => write!(f, "repo_wide_refactor"),
            ThreadImpactCategory::PublicApiSurface => write!(f, "public_api_surface"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceBand {
    Low,
    Medium,
    High,
}

impl std::fmt::Display for ConfidenceBand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfidenceBand::Low => write!(f, "low"),
            ConfidenceBand::Medium => write!(f, "medium"),
            ConfidenceBand::High => write!(f, "high"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThreadVerificationSummary {
    #[serde(default)]
    pub tests_passed: Option<bool>,
    #[serde(default)]
    pub tests_failed: Option<u32>,
    #[serde(default)]
    pub coverage_pct: Option<f32>,
    #[serde(default)]
    pub lint_warnings: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThreadConfidenceSummary {
    #[serde(default)]
    pub value: Option<f32>,
    #[serde(default)]
    pub band: Option<ConfidenceBand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ThreadIntegrationPolicy {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub manual_resolution_state: Option<String>,
    /// True only when `manual_resolution_state` was captured by an actual
    /// human conflict resolution (`heddle sync` materialized conflicts, then
    /// `heddle resolve` cleared them). False when the same field was set by a
    /// fully-automatic conflict-free integration (e.g. a clean 3-way merge of
    /// two threads that touch disjoint files). Both populate
    /// `manual_resolution_state` to mark the thread land-ready, but only the
    /// former should be reported as "manually resolved" to the operator.
    /// Pre-existing on-disk records have no field and serde defaults to
    /// `false`, so a stale clean-merge record never claims a manual resolution.
    #[serde(default)]
    pub conflicts_resolved_manually: bool,
}

impl ThreadIntegrationPolicy {
    /// Zero landing fields an unauthenticated peer can forge.
    pub fn clear_untrusted_landing_fields(&mut self) {
        self.manual_resolution_state = None;
        self.conflicts_resolved_manually = false;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub id: String,
    pub thread: String,
    pub target_thread: Option<String>,
    pub parent_thread: Option<String>,
    pub mode: ThreadMode,
    pub state: ThreadState,
    pub base_state: String,
    pub base_root: String,
    pub current_state: Option<String>,
    pub merged_state: Option<String>,
    pub task: Option<String>,
    pub changed_paths: Vec<String>,
    pub impact_categories: Vec<ThreadImpactCategory>,
    pub heavy_impact_paths: Vec<String>,
    pub promotion_suggested: bool,
    pub freshness: ThreadFreshness,
    pub verification_summary: ThreadVerificationSummary,
    pub confidence_summary: ThreadConfidenceSummary,
    pub integration_policy_result: ThreadIntegrationPolicy,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    // --- W1 tail-append fields below; new fields go here. ---
    /// Optional ephemeral-thread marker. `None` means the thread is
    /// persistent; `Some(...)` means the thread auto-collapses after
    /// `ttl_seconds` from `created_at`. The collapse is recorded
    /// as an `OpRecord::EphemeralThreadCollapse` and the thread is set
    /// to [`ThreadState::Abandoned`] — the underlying states remain
    /// addressable.
    pub ephemeral: Option<EphemeralMarker>,

    /// Whether the thread was created automatically by a harness
    /// integration (e.g. Claude Code's segment-rotation path) rather
    /// than by an explicit `heddle thread create` / `heddle start`
    /// invocation. Auto-threads are filtered from the default
    /// `heddle thread list` view and are eligible for sweep by
    /// `heddle thread cleanup --auto`.
    ///
    pub auto: bool,

    /// When the thread was started with `heddle start --shared-target`,
    /// this is the absolute path of the cargo `target/` directory the
    /// thread's checkout has been redirected to (via a `.cargo/config.toml`
    /// committed inside the checkout). `None` for threads that use
    /// cargo's default per-checkout `target/` (or for non-Rust
    /// workspaces). Recorded so `heddle thread show` can surface the
    /// arrangement and downstream tooling can locate build artefacts
    /// without re-deriving the fingerprint. (Item 2.1 of the heddle
    /// 6→8 plan.)
    pub shared_target_dir: Option<PathBuf>,
}

/// Ephemeral thread metadata. Lives at the tail of [`ThreadRecord`].
///
/// Ephemeral threads are spawned for short-lived agent work that should not
/// crowd `heddle log` or the thread workspace. If not promoted before
/// `ttl_seconds` elapses, the thread auto-collapses on the next read-side
/// sweep (`heddle status`, `heddle log`, `heddle thread list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EphemeralMarker {
    /// Time-to-live, in seconds, measured from [`ThreadRecord::created_at`].
    pub ttl_seconds: u32,
    /// When this marker was attached. Usually equal to the thread's own
    /// `created_at`, but kept separately so a thread can be retroactively
    /// marked ephemeral by a later operation if we ever need to.
    pub created_at: DateTime<Utc>,
    /// When `true` (the default), the auto-collapse sweep collapses the
    /// thread on TTL expiry. Setting `false` produces a warning at expiry
    /// but leaves the thread alive — useful for "ephemeral but I'm not
    /// done yet" situations during debugging.
    #[serde(default = "default_auto_collapse")]
    pub auto_collapse: bool,
}

fn default_auto_collapse() -> bool {
    true
}

impl EphemeralMarker {
    pub fn new(ttl_seconds: u32) -> Self {
        Self {
            ttl_seconds,
            created_at: Utc::now(),
            auto_collapse: true,
        }
    }

    /// Compute the absolute expiry timestamp.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.created_at + chrono::Duration::seconds(self.ttl_seconds as i64)
    }

    /// Whether this marker has expired at the given instant.
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at()
    }
}

impl ThreadRecord {
    pub fn thread_id(&self) -> ThreadId {
        // A persisted record's id was validated at creation — trust it.
        ThreadId::new_unchecked(self.id.clone())
    }
}
