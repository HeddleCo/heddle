// SPDX-License-Identifier: Apache-2.0
//! Types used by diff command output.

use std::borrow::Cow;

use objects::object::{FileMode, SemanticChange};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Serialize, Serializer};

use crate::{
    HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract, schema_for_report,
};

#[derive(Clone, Debug)]
pub struct DiffReport {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub changed_path_count: usize,
    pub stats: DiffStats,
    pub changes: Vec<FileChange>,
    pub semantic_changes: Option<Vec<SemanticChangeEntry>>,
    pub context: Option<Vec<FileContextEntry>>,
    pub broader_guidance: Option<Vec<ContextSnippet>>,
    /// Rendered unified-diff text, targeting a clean `git apply`
    /// round-trip (`patch(1)` compatibility is best-effort). Populated
    /// whenever line-level hunks exist regardless of the `--patch` flag,
    /// so JSON consumers always see a parseable diff.
    pub patch: Option<String>,
    pub worktree_mode: bool,
}

impl Serialize for DiffReport {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct DiffReportView<'a> {
            output_kind: &'static str,
            status: &'static str,
            from_state: &'a Option<String>,
            to_state: &'a Option<String>,
            changed_path_count: usize,
            stats: &'a DiffStats,
            changes: DiffChangesValue<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            semantic_changes: Option<&'a Vec<SemanticChangeEntry>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            context: Option<&'a Vec<FileContextEntry>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            broader_guidance: Option<&'a Vec<ContextSnippet>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            patch: Option<&'a String>,
        }

        DiffReportView {
            output_kind: self.output_kind,
            status: self.status,
            from_state: &self.from_state,
            to_state: &self.to_state,
            changed_path_count: self.changed_path_count,
            stats: &self.stats,
            changes: diff_changes_value(self),
            semantic_changes: self.semantic_changes.as_ref(),
            context: self.context.as_ref(),
            broader_guidance: self.broader_guidance.as_ref(),
            patch: self.patch.as_ref(),
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum DiffChangesValue<'a> {
    Grouped(DiffChangesGroupedRefs<'a>),
    Flat(&'a [FileChange]),
}

#[derive(Serialize)]
struct DiffChangesGroupedRefs<'a> {
    modified: Vec<&'a FileChange>,
    added: Vec<&'a FileChange>,
    deleted: Vec<&'a FileChange>,
}

fn diff_changes_value(output: &DiffReport) -> DiffChangesValue<'_> {
    if !output.worktree_mode {
        return DiffChangesValue::Flat(&output.changes);
    }

    let mut grouped = DiffChangesGroupedRefs {
        modified: Vec::new(),
        added: Vec::new(),
        deleted: Vec::new(),
    };
    for change in &output.changes {
        match change.kind.as_str() {
            "added" => grouped.added.push(change),
            "deleted" => grouped.deleted.push(change),
            _ => grouped.modified.push(change),
        }
    }
    DiffChangesValue::Grouped(grouped)
}

impl DiffReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "diff",
        machine_output_kind: MachineOutputKind::Json,
        output_discriminator: Some(OutputDiscriminator {
            field: "output_kind",
            value: "diff",
        }),
        schema: schema_for_report::<DiffReport>,
    };

    pub fn new(
        from_state: Option<String>,
        to_state: Option<String>,
        changes: Vec<FileChange>,
        semantic_changes: Option<Vec<SemanticChangeEntry>>,
        context: Option<Vec<FileContextEntry>>,
        broader_guidance: Option<Vec<ContextSnippet>>,
    ) -> Self {
        let stats = DiffStats::from_changes(&changes, semantic_changes.as_deref());
        Self::with_stats(
            from_state,
            to_state,
            changes,
            semantic_changes,
            context,
            broader_guidance,
            stats,
        )
    }

    pub fn with_stats(
        from_state: Option<String>,
        to_state: Option<String>,
        changes: Vec<FileChange>,
        semantic_changes: Option<Vec<SemanticChangeEntry>>,
        context: Option<Vec<FileContextEntry>>,
        broader_guidance: Option<Vec<ContextSnippet>>,
        stats: DiffStats,
    ) -> Self {
        Self {
            output_kind: "diff",
            status: "completed",
            changed_path_count: changes.len(),
            from_state,
            to_state,
            stats,
            changes,
            semantic_changes,
            context,
            broader_guidance,
            patch: None,
            worktree_mode: false,
        }
    }
}

impl JsonSchema for DiffReport {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("DiffOutput")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        DiffReportSchema::json_schema(generator)
    }
}

impl HeddleReport for DiffReport {
    const CONTRACT: ReportContract = DiffReport::CONTRACT;
}

#[derive(Debug, JsonSchema)]
#[allow(dead_code)]
struct DiffReportSchema {
    pub output_kind: String,
    pub status: String,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub changed_path_count: usize,
    pub stats: DiffStats,
    pub changes: DiffChangesSchema,
    pub semantic_changes: Option<Vec<SemanticChangeEntry>>,
    pub context: Option<Vec<FileContextEntry>>,
    pub broader_guidance: Option<Vec<ContextSnippet>>,
    pub patch: Option<String>,
}

#[derive(Debug, JsonSchema)]
#[allow(dead_code)]
#[serde(untagged)]
enum DiffChangesSchema {
    Grouped(DiffChangesGroupedSchema),
    Flat(Vec<FileChange>),
}

#[derive(Debug, JsonSchema)]
#[allow(dead_code)]
struct DiffChangesGroupedSchema {
    pub modified: Vec<FileChange>,
    pub added: Vec<FileChange>,
    pub deleted: Vec<FileChange>,
}

#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub struct DiffStats {
    pub files_changed: usize,
    pub additions: usize,
    pub modifications: usize,
    pub deletions: usize,
    pub renames: usize,
}

impl DiffStats {
    pub fn from_changes(
        changes: &[FileChange],
        semantic_changes: Option<&[SemanticChangeEntry]>,
    ) -> Self {
        let mut stats = Self {
            files_changed: changes.len(),
            ..Self::default()
        };
        for change in changes {
            // The `--stat` path runs the source-pair diff but drops the
            // hunk vector immediately; `line_counts` carries the tally
            // it computed before discarding. Prefer it so the summary
            // stays line-accurate without the per-file RAM cost.
            let counts = change
                .line_counts
                .clone()
                .unwrap_or_else(|| change_line_counts(change.lines.as_deref()));
            stats.additions += counts.added;
            stats.modifications += counts.modified;
            stats.deletions += counts.deleted;

            let has_detail = change.line_counts.is_some() || change.lines.is_some();
            match change.kind.as_str() {
                "added" if !has_detail => stats.additions += 1,
                "modified" if !has_detail => stats.modifications += 1,
                "deleted" if !has_detail => stats.deletions += 1,
                "renamed" => stats.renames += 1,
                _ => {}
            }
        }
        if let Some(semantic) = semantic_changes {
            stats.renames += semantic
                .iter()
                .filter(|change| change.change_type == "file_renamed")
                .count();
        }
        stats
    }
}

#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub struct FileChange {
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// Rename-detector score (0.0–1.0) for `kind == "renamed"` entries.
    /// The patch renderer emits this as `similarity index N%` in the
    /// extended diff header; without it `git apply` rejects rename
    /// patches because there's no signal that `b/new` shouldn't already
    /// exist on the target side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity_score: Option<f64>,
    /// Git file mode of the content side, used by the patch renderer to
    /// emit `new file mode <mode>` (adds) / `deleted file mode <mode>`
    /// (deletes). `None` falls back to `100644` (a regular file). For an
    /// executable the renderer emits `100755`; for a symlink `120000`
    /// (and the hunk body is the link target, matching git's blob
    /// representation of a symlink). For a `modified` change it is the
    /// new (post-change) mode, paired with `old_mode`.
    #[serde(skip)]
    #[schemars(skip)]
    pub mode: Option<FileMode>,
    /// Old (pre-change) git file mode for a `modified` change. When it
    /// differs from `mode` the renderer emits `old mode`/`new mode`
    /// extended headers so a chmod (e.g. exec-bit flip) round-trips
    /// through `git apply` even when the file's content is unchanged.
    #[serde(skip)]
    #[schemars(skip)]
    pub old_mode: Option<FileMode>,
    #[serde(skip)]
    #[schemars(skip)]
    pub binary: bool,
    /// Raw symlink target bytes for each side of a change that touches a
    /// symlink. Git stores a symlink's blob as the raw bytes of its target,
    /// which on Unix need not be valid UTF-8 — so they can never flow through
    /// `content_str()`/`diff_blobs` (which require UTF-8) or be binary-marked
    /// (a `120000` placeholder-binary stanza is rejected by `git apply`).
    /// When `Some`, the patch renderer reconstructs a byte-exact target hunk
    /// from these bytes — the single byte-preserving symlink path across every
    /// surface (add/delete/edit/rename) and both backends. `None` means the
    /// change does not involve a symlink and renders as ordinary text.
    #[serde(skip)]
    #[schemars(skip)]
    pub symlink: Option<SymlinkChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<LineDiff>>,
    /// Pre-computed line tally for paths where we counted before
    /// dropping the hunk vector (the `--stat` path). When present
    /// `DiffStats` reads it instead of walking `lines`, so the
    /// summary remains accurate without us retaining the hunks.
    #[serde(skip)]
    #[schemars(skip)]
    pub line_counts: Option<LineCounts>,
    /// Trailing-newline state and total line counts per side. The
    /// patch renderer uses these to emit the unified-diff
    /// `\ No newline at end of file` marker; `diff_blobs` strips
    /// line terminators before the renderer ever sees them, so the
    /// state must be plumbed alongside the hunk vector. Defaults
    /// (`true` / `0`) mean "no marker needed", which is what
    /// status-only fast paths fall back to.
    #[serde(skip)]
    #[schemars(skip)]
    pub eol: FileEolState,
}

/// The raw symlink target bytes for each side of a symlink change. A
/// symlink's git blob is exactly its target bytes (no trailing newline), so
/// these are the authoritative content the patch renderer emits. `old` is
/// `None` on an add, `new` is `None` on a delete, and both are `Some` on a
/// target-edit or rename-with-edit. The bytes come from the same loaders the
/// hunk path uses (`symlink_target_bytes` for the worktree, the stored blob
/// for a tree side), so a non-UTF-8 target survives without lossy conversion.
#[derive(Clone, Debug, Default)]
pub struct SymlinkChange {
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

/// Trailing-newline state for both sides of a file change, plus the
/// total line count per side. The patch renderer reads these to decide
/// whether to emit `\ No newline at end of file` and where.
#[derive(Clone, Copy, Debug)]
pub struct FileEolState {
    pub old_has_final_newline: bool,
    pub new_has_final_newline: bool,
    pub old_line_count: usize,
    pub new_line_count: usize,
}

impl Default for FileEolState {
    fn default() -> Self {
        Self {
            old_has_final_newline: true,
            new_has_final_newline: true,
            old_line_count: 0,
            new_line_count: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct LineDiff {
    pub prefix: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_line: Option<usize>,
}

impl LineDiff {
    pub fn new(prefix: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            content: content.into(),
            old_line: None,
            new_line: None,
        }
    }

    pub fn with_lines(
        prefix: impl Into<String>,
        content: impl Into<String>,
        old_line: Option<usize>,
        new_line: Option<usize>,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            content: content.into(),
            old_line,
            new_line,
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct FileContextEntry {
    pub path: String,
    pub annotations: Vec<ContextSnippet>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ContextSnippet {
    pub annotation_id: String,
    pub kind: String,
    pub content: String,
    pub revision_count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct LineCounts {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
}

pub fn change_line_counts(lines: Option<&[LineDiff]>) -> LineCounts {
    let mut counts = LineCounts::default();
    let mut index = 0usize;
    let lines = lines.unwrap_or_default();
    while index < lines.len() {
        let line = &lines[index];
        if line.prefix == "-"
            && let Some(next) = lines.get(index + 1)
            && next.prefix == "+"
            && should_render_modified_pair(&line.content, &next.content)
        {
            counts.modified += 1;
            index += 2;
            continue;
        }
        match line.prefix.as_str() {
            "+" => counts.added += 1,
            "-" => counts.deleted += 1,
            _ => {}
        }
        index += 1;
    }
    counts
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SemanticChangeEntry {
    pub change_type: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<String>,
}

impl From<SemanticChange> for SemanticChangeEntry {
    fn from(change: SemanticChange) -> Self {
        semantic_change_entry_fields(change).into()
    }
}

impl From<SemanticChangeEntryFields> for SemanticChangeEntry {
    fn from(fields: SemanticChangeEntryFields) -> Self {
        Self {
            change_type: fields.change_type,
            description: fields.description,
            path: fields.path,
            from_path: fields.from_path,
            to_path: fields.to_path,
            old_name: fields.old_name,
            new_name: fields.new_name,
            importance: fields.importance,
        }
    }
}

pub fn should_render_modified_pair(removed: &str, added: &str) -> bool {
    let prefix_len = common_prefix_boundary(removed, added);
    let suffix_len = common_suffix_boundary(&removed[prefix_len..], &added[prefix_len..]);
    let shared_len = prefix_len + suffix_len;
    let max_len = removed.len().max(added.len());

    // The `~` row is a review affordance for one logical line edit.
    // If two adjacent delete/add lines barely overlap, keeping the
    // normal two-line patch shape is clearer and avoids visually
    // gluing unrelated code together.
    shared_len >= 4 && shared_len * 3 >= max_len
}

fn common_prefix_boundary(left: &str, right: &str) -> usize {
    let mut boundary = 0;
    for ((left_index, left_char), (_, right_char)) in left.char_indices().zip(right.char_indices())
    {
        if left_char != right_char {
            break;
        }
        boundary = left_index + left_char.len_utf8();
    }
    boundary
}

fn common_suffix_boundary(left_tail: &str, right_tail: &str) -> usize {
    let mut boundary = 0;
    for ((left_index, left_char), (_, right_char)) in left_tail
        .char_indices()
        .rev()
        .zip(right_tail.char_indices().rev())
    {
        if left_char != right_char {
            break;
        }
        boundary = left_tail.len() - left_index;
    }
    boundary
}

struct SemanticChangeEntryFields {
    pub change_type: String,
    pub description: String,
    pub path: Option<String>,
    pub from_path: Option<String>,
    pub to_path: Option<String>,
    pub old_name: Option<String>,
    pub new_name: Option<String>,
    pub importance: Option<String>,
}

fn semantic_change_entry_fields(change: SemanticChange) -> SemanticChangeEntryFields {
    match change {
        SemanticChange::FileAdded { path } => SemanticChangeEntryFields {
            change_type: "file_added".to_string(),
            description: format!("File added: {}", path.display()),
            path: Some(path.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: None,
            importance: None,
        },
        SemanticChange::FileDeleted { path } => SemanticChangeEntryFields {
            change_type: "file_deleted".to_string(),
            description: format!("File deleted: {}", path.display()),
            path: Some(path.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: None,
            importance: None,
        },
        SemanticChange::FileModified {
            path,
            classification,
            importance,
            ..
        } => SemanticChangeEntryFields {
            change_type: if let Some(cls) = classification {
                format!("file_modified:{:?}", cls).to_lowercase()
            } else {
                "file_modified".to_string()
            },
            description: if let Some(cls) = classification {
                format!("File modified ({:?}): {}", cls, path.display())
            } else {
                format!("File modified: {}", path.display())
            },
            path: Some(path.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: None,
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionDeleted {
            file,
            name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_deleted".to_string(),
            description: format!("Function deleted: {} in {}", name, file.display()),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(name),
            new_name: None,
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::SignatureChanged {
            file,
            name,
            old_signature,
            new_signature,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "signature_changed".to_string(),
            description: format!("Signature changed: {} in {}", name, file.display()),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(old_signature),
            new_name: Some(new_signature),
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FileRenamed { from, to } => SemanticChangeEntryFields {
            change_type: "file_renamed".to_string(),
            description: format!("File renamed: {} -> {}", from.display(), to.display()),
            path: None,
            from_path: Some(from.display().to_string()),
            to_path: Some(to.display().to_string()),
            old_name: None,
            new_name: None,
            importance: None,
        },
        SemanticChange::FunctionAdded {
            file,
            name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_added".to_string(),
            description: format!("Function added: {} in {}", name, file.display()),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: Some(name),
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionExtracted {
            file,
            name,
            source_file,
            source_name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_extracted".to_string(),
            description: if let Some(source_name) = &source_name {
                let source_file = source_file.as_ref().unwrap_or(&file);
                format!(
                    "Function extracted: {} from {} in {}",
                    name,
                    source_name,
                    source_file.display()
                )
            } else {
                format!("Function extracted: {} in {}", name, file.display())
            },
            path: Some(file.display().to_string()),
            from_path: source_file.map(|path| path.display().to_string()),
            to_path: None,
            old_name: source_name,
            new_name: Some(name),
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionRenamed {
            file,
            old_name,
            new_name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_renamed".to_string(),
            description: format!(
                "Function renamed: {} -> {} in {}",
                old_name,
                new_name,
                file.display()
            ),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(old_name),
            new_name: Some(new_name),
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionModified {
            file,
            name,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_modified".to_string(),
            description: format!("Function modified: {} in {}", name, file.display()),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(name),
            new_name: None,
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::FunctionMoved {
            file,
            name,
            old_start_line,
            new_start_line,
            importance,
        } => SemanticChangeEntryFields {
            change_type: "function_moved".to_string(),
            description: format!(
                "Function moved: {} in {} ({} -> {})",
                name,
                file.display(),
                old_start_line + 1,
                new_start_line + 1
            ),
            path: Some(file.display().to_string()),
            from_path: None,
            to_path: None,
            old_name: Some(name),
            new_name: None,
            importance: importance.map(|i| format!("{i:?}").to_lowercase()),
        },
        SemanticChange::DependencyAdded { name, version } => SemanticChangeEntryFields {
            change_type: "dependency_added".to_string(),
            description: format!("Dependency added: {}@{}", name, version),
            path: None,
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: Some(name),
            importance: None,
        },
        SemanticChange::DependencyRemoved { name } => SemanticChangeEntryFields {
            change_type: "dependency_removed".to_string(),
            description: format!("Dependency removed: {}", name),
            path: None,
            from_path: None,
            to_path: None,
            old_name: Some(name),
            new_name: None,
            importance: None,
        },
        SemanticChange::Custom { change_type, .. } => SemanticChangeEntryFields {
            change_type: format!("custom:{}", change_type),
            description: format!("Custom change: {}", change_type),
            path: None,
            from_path: None,
            to_path: None,
            old_name: None,
            new_name: None,
            importance: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use objects::object::{ChangeImportance, SemanticChange};
    use serde_json::Value;

    use super::SemanticChangeEntry;

    #[test]
    fn semantic_change_json_uses_importance_field_not_old_name() {
        let entry = SemanticChangeEntry::from(SemanticChange::FileModified {
            path: PathBuf::from("src/lib.rs"),
            classification: None,
            importance: Some(ChangeImportance::Medium),
            confidence: None,
        });
        let json = serde_json::to_value(entry).expect("semantic entry serializes");

        assert_eq!(json["importance"], "medium");
        assert!(json.get("old_name").is_none(), "{json}");
    }

    #[test]
    fn semantic_rename_json_uses_path_fields() {
        let entry = SemanticChangeEntry::from(SemanticChange::FileRenamed {
            from: PathBuf::from("src/old.rs"),
            to: PathBuf::from("src/new.rs"),
        });
        let json = serde_json::to_value(entry).expect("semantic rename serializes");

        assert_eq!(json["change_type"], "file_renamed");
        assert_eq!(json["from_path"], "src/old.rs");
        assert_eq!(json["to_path"], "src/new.rs");
        assert!(json.get("old_name").is_none(), "{json}");
        assert!(matches!(json["from_path"], Value::String(_)));
        assert!(matches!(json["to_path"], Value::String(_)));
    }
}
