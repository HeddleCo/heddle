// SPDX-License-Identifier: Apache-2.0
//! Context annotations for files, symbols, line ranges, and broader state guidance.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::object::hash::{ChangeId, ContentHash};

const FILE_TARGET_ROOT: &str = "__files";
const STATE_TARGET_ROOT: &str = "__states";

/// A collection of logical annotations for a single target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBlob {
    pub format_version: u8,
    pub annotations: Vec<Annotation>,
}

/// A stable logical annotation with revision history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub annotation_id: String,
    pub scope: AnnotationScope,
    pub status: AnnotationStatus,
    pub revisions: Vec<AnnotationRevision>,
    #[serde(default)]
    pub supersedes_annotation_id: Option<String>,
    #[serde(default)]
    pub supersedes_rewrite_pct: Option<u32>,
    // --- tail-only optional fields below; new fields go here. ---
    /// Visibility scope. Pre-W1 annotations have no field on disk; rmp-serde
    /// fills the default ([`AnnotationVisibility::Public`]), preserving the
    /// pre-existing meaning ("annotations are publicly visible").
    #[serde(default)]
    pub visibility: AnnotationVisibility,
    /// Back-pointer set when this annotation was produced by resolving a
    /// discussion. Lets viewers jump from the annotation back to the
    /// discussion that produced it.
    #[serde(default)]
    pub resolved_from_discussion: Option<String>,
}

/// Visibility scope for an annotation. Determines which audiences see it on
/// read paths and during bridge export.
///
/// `Public` is the default — that matches pre-W1 behavior, where every
/// annotation was effectively public, so legacy data decodes unchanged.
/// External references (annotations that point to external systems) inherit
/// the scope of their parent annotation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationVisibility {
    #[default]
    Public,
    Internal,
    TeamScoped {
        team_id: String,
    },
    Restricted {
        scope_label: String,
    },
}

impl AnnotationVisibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::TeamScoped { .. } => "team_scoped",
            Self::Restricted { .. } => "restricted",
        }
    }
}

/// A single revision of a logical annotation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotationRevision {
    pub revision_id: String,
    pub kind: AnnotationKind,
    pub content: String,
    pub tags: Vec<String>,
    pub attribution: String,
    pub created_at: i64,
    /// BLAKE3 hash of the source bytes at the annotated scope when created.
    /// For File scope: hash of entire file blob.
    /// For Symbol/Lines: hash of the relevant byte range.
    #[serde(default)]
    pub source_hash: Option<ContentHash>,
    /// The State this revision was created against.
    /// Enables retrieving the exact source as it was at annotation time.
    #[serde(default)]
    pub created_at_state: Option<ChangeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationStatus {
    Active,
    Superseded,
}

/// The canonical annotation taxonomy the product surfaces.
///
/// `Constraint`, `Invariant`, and `Rationale` are the three kinds of
/// reasoning we keep alongside code. The lowercase serde names are the
/// wire/storage vocabulary shared with proto and the web API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationKind {
    /// A rule the code must obey. Example: "empty scope must return NoScope".
    Constraint,
    /// A property that must hold across operations. Example: "state DAG is append-only".
    Invariant,
    /// Design decision + reasoning. Example: "thread resolution walks to LCA because…".
    Rationale,
}

/// A typed target for context entries.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContextTarget {
    File { path: String },
    State { change_id: ChangeId },
}

/// What part of a file an annotation targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationScope {
    File,
    Symbol {
        name: String,
        /// Line range resolved at annotation creation time via tree-sitter.
        /// Enables the web UI to show exact code for this symbol.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_lines: Option<(u32, u32)>,
    },
    Lines(u32, u32),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    #[error("unsupported context format version {0}")]
    UnsupportedVersion(u8),
    #[error("line range start {0} exceeds end {1}")]
    InvalidLineRange(u32, u32),
    #[error("symbol name must not be empty")]
    EmptySymbol,
    #[error("file target path must not be empty")]
    EmptyTargetPath,
    #[error("context target path must be relative, got: {0}")]
    AbsoluteTargetPath(String),
    #[error("invalid context target path: {0}")]
    InvalidTargetPath(String),
    #[error("state-level guidance must use file scope only")]
    StateTargetMustUseFileScope,
    #[error("annotation {0} has no revisions")]
    MissingRevisions(String),
    #[error("invalid context encoding: {0}")]
    InvalidEncoding(String),
}

impl ContextBlob {
    /// Current encoded format version. Reject anything that isn't the
    /// current value — no live deployments to migrate from.
    pub const FORMAT_VERSION: u8 = 2;

    pub fn new(annotations: Vec<Annotation>) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            annotations,
        }
    }

    pub fn validate(&self) -> Result<(), ContextError> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(ContextError::UnsupportedVersion(self.format_version));
        }
        for annotation in &self.annotations {
            annotation.validate()?;
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ContextError> {
        rmp_serde::to_vec(self).map_err(|err| ContextError::InvalidEncoding(err.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ContextError> {
        let blob: Self = rmp_serde::from_slice(bytes)
            .map_err(|err| ContextError::InvalidEncoding(err.to_string()))?;
        blob.validate()?;
        Ok(blob)
    }
}

impl Annotation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        scope: AnnotationScope,
        kind: AnnotationKind,
        content: String,
        tags: Vec<String>,
        attribution: String,
        created_at: i64,
        source_hash: Option<ContentHash>,
        created_at_state: Option<ChangeId>,
    ) -> Self {
        Self {
            annotation_id: ChangeId::generate().to_string_full(),
            scope,
            status: AnnotationStatus::Active,
            revisions: vec![AnnotationRevision {
                revision_id: ChangeId::generate().to_string_full(),
                kind,
                content,
                tags,
                attribution,
                created_at,
                source_hash,
                created_at_state,
            }],
            supersedes_annotation_id: None,
            supersedes_rewrite_pct: None,
            visibility: AnnotationVisibility::default(),
            resolved_from_discussion: None,
        }
    }

    pub fn current_revision(&self) -> Option<&AnnotationRevision> {
        self.revisions.last()
    }

    pub fn current_revision_mut(&mut self) -> Option<&mut AnnotationRevision> {
        self.revisions.last_mut()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn revise(
        &mut self,
        kind: AnnotationKind,
        content: String,
        tags: Vec<String>,
        attribution: String,
        created_at: i64,
        source_hash: Option<ContentHash>,
        created_at_state: Option<ChangeId>,
    ) -> &AnnotationRevision {
        self.revisions.push(AnnotationRevision {
            revision_id: ChangeId::generate().to_string_full(),
            kind,
            content,
            tags,
            attribution,
            created_at,
            source_hash,
            created_at_state,
        });
        self.current_revision().expect("new revision appended")
    }

    pub fn mark_superseded(&mut self) {
        self.status = AnnotationStatus::Superseded;
    }

    pub fn validate(&self) -> Result<(), ContextError> {
        self.scope.validate()?;
        if self.annotation_id.is_empty() {
            return Err(ContextError::InvalidEncoding(
                "annotation_id must not be empty".to_string(),
            ));
        }
        if self.revisions.is_empty() {
            return Err(ContextError::MissingRevisions(self.annotation_id.clone()));
        }
        for revision in &self.revisions {
            revision.validate()?;
        }
        Ok(())
    }
}

impl AnnotationRevision {
    pub fn validate(&self) -> Result<(), ContextError> {
        if self.revision_id.is_empty() {
            return Err(ContextError::InvalidEncoding(
                "revision_id must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

impl AnnotationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Constraint => "constraint",
            Self::Invariant => "invariant",
            Self::Rationale => "rationale",
        }
    }
}

impl std::fmt::Display for AnnotationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for AnnotationKind {
    type Err = ContextError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "constraint" => Ok(Self::Constraint),
            "invariant" => Ok(Self::Invariant),
            "rationale" => Ok(Self::Rationale),
            _ => Err(ContextError::InvalidEncoding(format!(
                "invalid annotation kind '{value}'"
            ))),
        }
    }
}

impl ContextTarget {
    /// Construct a file-scope target. The path must be non-empty,
    /// relative, and walkable — it's stored inside the context tree
    /// under `__files/<path>`, and the downstream writer's
    /// `split_path` helper only understands `Component::Normal` (no
    /// `RootDir`, no `ParentDir`, no `CurDir`-only trails).
    ///
    /// Previously this accepted any non-empty string, which meant
    /// absolute paths like `/Users/me/repo/src/auth.rs` got all the
    /// way to `Repository::set_context_blob` before failing with a
    /// cryptic `"empty path"` error deep in the tree-insert routine.
    /// Rejecting here turns that into a clear
    /// `AbsoluteTargetPath`/`InvalidTargetPath` at the callsite.
    pub fn file(path: impl Into<String>) -> Result<Self, ContextError> {
        let path = path.into();
        if path.trim().is_empty() {
            return Err(ContextError::EmptyTargetPath);
        }
        let p = Path::new(&path);
        if p.is_absolute() {
            return Err(ContextError::AbsoluteTargetPath(path));
        }
        // Walk components: reject `..` anywhere (would let the path
        // escape `__files/`), and require at least one `Normal`
        // component (rejects paths like `.`, `./.`, or strings whose
        // every component is `CurDir`).
        let mut saw_normal = false;
        for component in p.components() {
            match component {
                Component::Normal(_) => saw_normal = true,
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(ContextError::InvalidTargetPath(path));
                }
                Component::RootDir | Component::Prefix(_) => {
                    // `is_absolute` above already catches the typical
                    // cases on both Unix and Windows, but belt-and-
                    // braces: if a Prefix or RootDir sneaks through
                    // on some platform, still reject.
                    return Err(ContextError::AbsoluteTargetPath(path));
                }
            }
        }
        if !saw_normal {
            return Err(ContextError::InvalidTargetPath(path));
        }
        Ok(Self::File { path })
    }

    pub fn state(change_id: ChangeId) -> Self {
        Self::State { change_id }
    }

    pub fn validate_scope(&self, scope: &AnnotationScope) -> Result<(), ContextError> {
        match self {
            Self::File { .. } => scope.validate(),
            Self::State { .. } => {
                if matches!(scope, AnnotationScope::File) {
                    Ok(())
                } else {
                    Err(ContextError::StateTargetMustUseFileScope)
                }
            }
        }
    }

    pub fn storage_path(&self) -> PathBuf {
        match self {
            Self::File { path } => Path::new(FILE_TARGET_ROOT).join(path),
            Self::State { change_id } => {
                Path::new(STATE_TARGET_ROOT).join(change_id.to_string_full())
            }
        }
    }

    pub fn legacy_storage_path(&self) -> Option<PathBuf> {
        match self {
            Self::File { path } => Some(PathBuf::from(path)),
            Self::State { .. } => None,
        }
    }

    pub fn from_storage_path(path: &Path) -> Option<Self> {
        let mut components = path.components();
        match components.next()? {
            Component::Normal(part) if part == FILE_TARGET_ROOT => {
                let rest = components.as_path();
                if rest.as_os_str().is_empty() {
                    None
                } else {
                    Some(Self::File {
                        path: rest.to_string_lossy().to_string(),
                    })
                }
            }
            Component::Normal(part) if part == STATE_TARGET_ROOT => {
                let rest = components.as_path();
                let mut state_components = rest.components();
                let Component::Normal(id) = state_components.next()? else {
                    return None;
                };
                if !state_components.as_path().as_os_str().is_empty() {
                    return None;
                }
                ChangeId::parse(&id.to_string_lossy())
                    .ok()
                    .map(|change_id| Self::State { change_id })
            }
            _ => Some(Self::File {
                path: path.to_string_lossy().to_string(),
            }),
        }
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::File { path } => Some(path),
            Self::State { .. } => None,
        }
    }

    pub fn state_id(&self) -> Option<ChangeId> {
        match self {
            Self::State { change_id } => Some(*change_id),
            Self::File { .. } => None,
        }
    }
}

impl AnnotationScope {
    pub fn validate(&self) -> Result<(), ContextError> {
        match self {
            Self::File => Ok(()),
            Self::Symbol {
                name,
                resolved_lines,
            } => {
                if name.is_empty() {
                    return Err(ContextError::EmptySymbol);
                }
                if let Some((start, end)) = resolved_lines
                    && start > end
                {
                    return Err(ContextError::InvalidLineRange(*start, *end));
                }
                Ok(())
            }
            Self::Lines(start, end) => {
                if start > end {
                    Err(ContextError::InvalidLineRange(*start, *end))
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File, Self::File) => true,
            (Self::Symbol { name: a, .. }, Self::Symbol { name: b, .. }) => a == b,
            (Self::Lines(a1, a2), Self::Lines(b1, b2)) => a1 == b1 && a2 == b2,
            _ => false,
        }
    }

    pub fn symbol_name(&self) -> Option<&str> {
        match self {
            Self::Symbol { name, .. } => Some(name),
            _ => None,
        }
    }

    pub fn line_range(&self) -> Option<(u32, u32)> {
        match self {
            Self::Lines(start, end) => Some((*start, *end)),
            Self::Symbol {
                resolved_lines: Some((start, end)),
                ..
            } => Some((*start, *end)),
            _ => None,
        }
    }
}

impl std::fmt::Display for AnnotationScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File => write!(f, "file"),
            Self::Symbol { name, .. } => write!(f, "symbol:{name}"),
            Self::Lines(start, end) => write!(f, "lines:{start}-{end}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ContextTarget::file validation --------------------------------

    #[test]
    fn context_target_accepts_relative_paths() {
        // Plain relative, nested, and dotfile forms should all pass.
        assert!(ContextTarget::file("src/auth.rs").is_ok());
        assert!(ContextTarget::file("a/b/c.txt").is_ok());
        assert!(ContextTarget::file(".gitignore").is_ok());
        assert!(ContextTarget::file("a").is_ok());
        // A leading `./` is pure noise; still accepted (the CurDir
        // components are ignored, and `a` is a Normal component).
        assert!(ContextTarget::file("./a").is_ok());
    }

    #[test]
    fn context_target_rejects_empty_path() {
        assert!(matches!(
            ContextTarget::file(""),
            Err(ContextError::EmptyTargetPath)
        ));
        assert!(matches!(
            ContextTarget::file("   "),
            Err(ContextError::EmptyTargetPath)
        ));
    }

    #[test]
    fn context_target_rejects_absolute_path_unix() {
        let err = ContextTarget::file("/Users/me/repo/src/auth.rs").unwrap_err();
        assert!(
            matches!(err, ContextError::AbsoluteTargetPath(ref p) if p == "/Users/me/repo/src/auth.rs"),
            "got {err:?}"
        );
        // Root alone also absolute.
        assert!(matches!(
            ContextTarget::file("/"),
            Err(ContextError::AbsoluteTargetPath(_))
        ));
    }

    #[test]
    fn context_target_rejects_parent_escape() {
        // `..` anywhere would let a writer escape `__files/` inside
        // the context tree.
        assert!(matches!(
            ContextTarget::file("../etc/passwd"),
            Err(ContextError::InvalidTargetPath(_))
        ));
        assert!(matches!(
            ContextTarget::file("src/../../escape"),
            Err(ContextError::InvalidTargetPath(_))
        ));
    }

    #[test]
    fn context_target_rejects_all_dot_components() {
        // A path made entirely of `.`/`./.` is non-empty under the
        // old check but has no Normal component to write under, so
        // downstream writes would fail cryptically. Catch it here.
        assert!(matches!(
            ContextTarget::file("."),
            Err(ContextError::InvalidTargetPath(_))
        ));
        assert!(matches!(
            ContextTarget::file("./."),
            Err(ContextError::InvalidTargetPath(_))
        ));
    }

    #[test]
    fn roundtrips_revision_with_missing_source_hash_and_present_state() {
        let created_at_state = ChangeId::generate();
        let blob = ContextBlob::new(vec![Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Rationale,
            "Entry point".to_string(),
            vec!["critical".to_string()],
            "test@example.com".to_string(),
            1700000000,
            None,
            Some(created_at_state),
        )]);

        let encoded = blob.encode().unwrap();
        let decoded = ContextBlob::decode(&encoded).unwrap();
        let revision = decoded.annotations[0].current_revision().unwrap();
        assert_eq!(revision.source_hash, None);
        assert_eq!(revision.created_at_state, Some(created_at_state));
    }

    #[test]
    fn roundtrip_serialization() {
        let blob = ContextBlob::new(vec![Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Invariant,
            "Entry point".to_string(),
            vec!["constraint".to_string()],
            "test@example.com".to_string(),
            1700000000,
            None,
            None,
        )]);

        let bytes = blob.encode().unwrap();
        let decoded = ContextBlob::decode(&bytes).unwrap();
        assert_eq!(blob, decoded);
    }

    #[test]
    fn validate_good_blob() {
        let blob = ContextBlob::new(vec![]);
        blob.validate().unwrap();
    }

    #[test]
    fn validate_bad_version() {
        let blob = ContextBlob {
            format_version: 99,
            annotations: vec![],
        };
        assert!(matches!(
            blob.validate(),
            Err(ContextError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn validate_bad_line_range() {
        let blob = ContextBlob::new(vec![Annotation::new(
            AnnotationScope::Lines(20, 10),
            AnnotationKind::Rationale,
            "bad".to_string(),
            vec![],
            "test".to_string(),
            0,
            None,
            None,
        )]);
        assert!(matches!(
            blob.validate(),
            Err(ContextError::InvalidLineRange(20, 10))
        ));
    }

    #[test]
    fn validate_empty_symbol() {
        let blob = ContextBlob::new(vec![Annotation::new(
            AnnotationScope::Symbol {
                name: String::new(),
                resolved_lines: None,
            },
            AnnotationKind::Rationale,
            "bad".to_string(),
            vec![],
            "test".to_string(),
            0,
            None,
            None,
        )]);
        assert!(matches!(blob.validate(), Err(ContextError::EmptySymbol)));
    }

    #[test]
    fn scope_matching() {
        assert!(AnnotationScope::File.matches(&AnnotationScope::File));
        assert!(
            AnnotationScope::Symbol {
                name: "foo".into(),
                resolved_lines: None
            }
            .matches(&AnnotationScope::Symbol {
                name: "foo".into(),
                resolved_lines: Some((1, 5))
            })
        );
        assert!(
            !AnnotationScope::Symbol {
                name: "foo".into(),
                resolved_lines: None
            }
            .matches(&AnnotationScope::Symbol {
                name: "bar".into(),
                resolved_lines: None
            })
        );
        assert!(AnnotationScope::Lines(1, 10).matches(&AnnotationScope::Lines(1, 10)));
    }

    #[test]
    fn state_targets_only_allow_file_scope() {
        let target = ContextTarget::state(ChangeId::generate());
        assert!(target.validate_scope(&AnnotationScope::File).is_ok());
        assert!(matches!(
            target.validate_scope(&AnnotationScope::Lines(1, 2)),
            Err(ContextError::StateTargetMustUseFileScope)
        ));
    }

    #[test]
    fn context_target_storage_roundtrip() {
        let file = ContextTarget::file("src/main.rs").unwrap();
        assert_eq!(
            ContextTarget::from_storage_path(&file.storage_path()),
            Some(file.clone())
        );

        let state = ContextTarget::state(ChangeId::generate());
        assert_eq!(
            ContextTarget::from_storage_path(&state.storage_path()),
            Some(state)
        );
    }
}