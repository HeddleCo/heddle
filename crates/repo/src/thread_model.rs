// SPDX-License-Identifier: Apache-2.0
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use objects::store::AgentUsageSummary;
use serde::{Deserialize, Serialize};

/// A validated thread id. Construction from user- or externally-supplied
/// input goes through [`ThreadId::new`], which rejects anything that is not a
/// safe single shell token (see [`validate_thread_id`]). That invariant is what
/// lets recommended-command breadcrumbs interpolate a thread id *bare* — there
/// is no whitespace or shell metacharacter to quote, by construction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ThreadId(String);

impl ThreadId {
    /// Construct a thread id from user/external input, validating it against
    /// the safe slug rule. Returns a [`ThreadIdError`] carrying an actionable
    /// rename hint when the input is empty or contains a space, a shell
    /// metacharacter, a `..` path segment, or a leading `/`.
    pub fn new(value: impl Into<String>) -> Result<Self, ThreadIdError> {
        let value = value.into();
        validate_thread_id(&value)?;
        Ok(Self(value))
    }

    /// Wrap a value WITHOUT validation. Reserved for inputs that are
    /// safe-by-construction: deserialization of thread records already on disk
    /// (validate at creation, trust thereafter) and internally-generated slug
    /// ids. Never call this on user/external input — use [`ThreadId::new`].
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
        // Persisted thread ids were validated when the thread was created; a
        // record on disk is trusted, so deserialize through `new_unchecked`
        // rather than re-running validation (and rejecting historical data).
        let value = String::deserialize(deserializer)?;
        Ok(Self::new_unchecked(value))
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

impl std::fmt::Display for ThreadIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.input.is_empty() {
            write!(f, "thread name must not be empty")
        } else {
            write!(
                f,
                "thread name '{}' is invalid: use only letters, digits, and _ - . / @ : + = \
                 (no spaces, shell metacharacters, '..' path segments, or a leading '/' or '-') — try '{}'",
                self.input, self.suggestion
            )
        }
    }
}

impl std::error::Error for ThreadIdError {}

/// The single rule for thread-id validity. A valid id is non-empty, made up
/// only of the safe slug set (ASCII alphanumerics plus `_ - . / @ : + =`), has
/// no `..` segment, and does not begin with `/`. This is deliberately the same
/// safe set [`crate::shell_quote`] treats as needing no quoting, so a valid
/// thread id is always a single shell token: `feature/x`, `v1.2`, `my-thread`,
/// and `team@scope` are accepted; spaces, quotes, `;`, `|`, `$`, `&`, `*`,
/// backticks, and newlines are rejected. Thread ids flow into worktree paths,
/// so `..` and a leading `/` are rejected to keep them in-tree. A leading `-`
/// is also rejected: it is in the safe set (for `my-thread`) but a breadcrumb
/// like `heddle land --thread -foo` parses `-foo` as a flag, not the value.
pub fn validate_thread_id(value: &str) -> Result<(), ThreadIdError> {
    let safe_charset = value.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'-' | b'.' | b'/' | b'@' | b':' | b'+' | b'=')
    });
    let ok = !value.is_empty()
        && safe_charset
        && !value.contains("..")
        && !value.starts_with('/')
        // A leading '-' is in the safe set (for `my-thread`) but makes the id
        // look like a CLI flag: `heddle land --thread -foo` parses `-foo` as an
        // option, and argv-template construction panics. Reject it at the source.
        && !value.starts_with('-');
    if ok {
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
        trimmed.to_string()
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadConfidenceSummary {
    #[serde(default)]
    pub value: Option<f32>,
    #[serde(default)]
    pub band: Option<ConfidenceBand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadIntegrationPolicy {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub manual_resolution_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub id: String,
    pub thread: String,
    #[serde(default)]
    pub target_thread: Option<String>,
    #[serde(default)]
    pub parent_thread: Option<String>,
    pub mode: ThreadMode,
    pub state: ThreadState,
    pub base_state: String,
    pub base_root: String,
    #[serde(default)]
    pub current_state: Option<String>,
    #[serde(default)]
    pub merged_state: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub changed_paths: Vec<String>,
    #[serde(default)]
    pub impact_categories: Vec<ThreadImpactCategory>,
    #[serde(default)]
    pub heavy_impact_paths: Vec<String>,
    #[serde(default)]
    pub promotion_suggested: bool,
    #[serde(default = "default_freshness")]
    pub freshness: ThreadFreshness,
    #[serde(default)]
    pub verification_summary: ThreadVerificationSummary,
    #[serde(default)]
    pub confidence_summary: ThreadConfidenceSummary,
    #[serde(default)]
    pub integration_policy_result: ThreadIntegrationPolicy,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    // --- W1 tail-append fields below; new fields go here. ---
    /// Optional ephemeral-thread marker. `None` (the default) means the
    /// thread is persistent; `Some(...)` means the thread auto-collapses
    /// after `ttl_seconds` from `created_at`. The collapse is recorded
    /// as an `OpRecord::EphemeralThreadCollapse` and the thread is set
    /// to [`ThreadState::Abandoned`] — the underlying states remain
    /// addressable. Pre-W1 records have no field on disk; serde defaults
    /// to `None`, preserving "thread is persistent" behavior.
    #[serde(default)]
    pub ephemeral: Option<EphemeralMarker>,

    /// Whether the thread was created automatically by a harness
    /// integration (e.g. Claude Code's segment-rotation path) rather
    /// than by an explicit `heddle thread create` / `heddle start`
    /// invocation. Auto-threads are filtered from the default
    /// `heddle thread list` view and are eligible for sweep by
    /// `heddle thread cleanup --auto`.
    ///
    /// Pre-existing thread records have no `auto` field on disk; serde
    /// defaults to `false` so the historical "explicit" behaviour is
    /// preserved across the upgrade. (Item 2.2 of the heddle 6→8 plan.)
    #[serde(default)]
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
    #[serde(default)]
    pub shared_target_dir: Option<PathBuf>,
}

/// Ephemeral thread metadata. Lives at the tail of [`ThreadRecord`].
///
/// Ephemeral threads are spawned for short-lived agent work that should not
/// crowd `heddle log` or the thread workspace. If not promoted before
/// `ttl_seconds` elapses, the thread auto-collapses on the next read-side
/// sweep (`heddle status`, `heddle log`, `heddle thread list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadRuntimeOverlay {
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub execution_path: Option<PathBuf>,
    #[serde(default)]
    pub materialized_path: Option<PathBuf>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub heddle_session_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub native_actor_key: Option<String>,
    #[serde(default)]
    pub native_parent_actor_key: Option<String>,
    #[serde(default)]
    pub probe_source: Option<String>,
    #[serde(default)]
    pub probe_confidence: Option<f32>,
    #[serde(default)]
    pub usage_summary: Option<AgentUsageSummary>,
    #[serde(default)]
    pub last_progress_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub report_flush_state: Option<String>,
    #[serde(default)]
    pub attach_reason: Option<String>,
    #[serde(default)]
    pub thread_mode: Option<ThreadMode>,
    #[serde(default)]
    pub thread_state: Option<ThreadState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    pub record: ThreadRecord,
    pub runtime: ThreadRuntimeOverlay,
    pub is_current: bool,
    pub is_isolated: bool,
}

impl ThreadView {
    pub fn from_record(
        record: ThreadRecord,
        runtime: ThreadRuntimeOverlay,
        is_current: bool,
    ) -> Self {
        let is_isolated = path_present(runtime.path.as_ref())
            || path_present(runtime.execution_path.as_ref())
            || path_present(runtime.materialized_path.as_ref());
        Self {
            record,
            runtime,
            is_current,
            is_isolated,
        }
    }
}

fn path_present(path: Option<&PathBuf>) -> bool {
    path.is_some_and(|path| !path.as_os_str().is_empty())
}

fn default_freshness() -> ThreadFreshness {
    ThreadFreshness::Unknown
}

#[cfg(test)]
mod thread_id_tests {
    use super::*;

    #[test]
    fn accepts_safe_slugs() {
        for ok in [
            "feature/x",
            "v1.2",
            "a_b-c.d",
            "team@scope",
            "main",
            "wip+1=2",
        ] {
            assert!(
                ThreadId::new(ok).is_ok(),
                "expected '{ok}' to be a valid thread id"
            );
        }
    }

    #[test]
    fn rejects_whitespace_metachars_traversal_and_empty() {
        for bad in [
            "my feature", // space
            "a;b",        // shell separator
            "a|b",        // pipe
            "a$(x)",      // command substitution
            "a\nb",       // newline
            "a&b",        // background
            "`x`",        // backtick
            "..",         // bare traversal
            "a/../b",     // traversal segment
            "/abs",       // leading slash
            "-foo",       // leading dash — parses as a CLI flag in breadcrumbs
            "--bar",      // leading double-dash
            "",           // empty
        ] {
            assert!(
                ThreadId::new(bad).is_err(),
                "expected '{bad}' to be rejected as an invalid thread id"
            );
        }
    }

    #[test]
    fn error_message_carries_a_valid_rename_hint() {
        let err = ThreadId::new("my feature").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("my feature"),
            "names the offending input: {msg}"
        );
        assert!(msg.contains("try 'my-feature'"), "suggests a rename: {msg}");
        // The suggestion itself must be a valid thread id.
        assert!(ThreadId::new(err.suggestion.as_str()).is_ok());
    }

    #[test]
    fn deserialize_trusts_persisted_ids_without_revalidating() {
        // A record written before this rule existed (or by a future version)
        // must still deserialize — validation is at creation, not on read.
        let id: ThreadId = serde_json::from_str("\"legacy id\"").unwrap();
        assert_eq!(id.as_str(), "legacy id");
    }
}
