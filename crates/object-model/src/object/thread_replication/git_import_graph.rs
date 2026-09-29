// SPDX-License-Identifier: Apache-2.0
//! Git identities and ref classification shared by local and hosted import.

use serde::{Deserialize, Serialize};

use super::invalid;
use crate::error::Result;

pub const MAX_IMPORT_REFS: usize = 512;

/// Git object identity retains the hash algorithm; SHA-256 never truncates
/// into a SHA-1 address. This names commits and raw annotated tag objects.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitObjectId {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}

/// The raw Git ref target, including symbolic refs and object kinds that
/// cannot become native refs. A dangling direct target has `Unknown` kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitRefObjectType {
    Commit,
    Tag,
    Tree,
    Blob,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitRefTarget {
    Direct {
        oid: GitObjectId,
        object_type: GitRefObjectType,
    },
    Symbolic(Vec<u8>),
}

/// A Git ref with its direct target. `peeled_commit` is present only when
/// this ref resolves to a commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRefIdentity {
    pub raw_name: Vec<u8>,
    pub raw_target: GitRefTarget,
    pub peeled_commit: Option<GitObjectId>,
}

impl ImportRefIdentity {
    pub fn direct_oid(&self) -> Option<&GitObjectId> {
        match &self.raw_target {
            GitRefTarget::Direct { oid, .. } => Some(oid),
            GitRefTarget::Symbolic(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitObjectFormat {
    Sha1,
    Sha256,
}

impl GitObjectFormat {
    pub(crate) fn sley(self) -> sley_core::ObjectFormat {
        match self {
            Self::Sha1 => sley_core::ObjectFormat::Sha1,
            Self::Sha256 => sley_core::ObjectFormat::Sha256,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportRefDisposition {
    Branch,
    CommitTag,
    DefaultHead,
    RequiredNotes,
    Unsupported { reason: ImportSkipReason },
}

/// Reasons for refs omitted from native Threads and markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSkipReason {
    RemoteTracking,
    Replace,
    Pull,
    OtherNotes,
    NonCommitTag,
    SymbolicRef,
    OtherRef,
    DanglingUnsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportRefFailure {
    MissingRequiredTarget,
    InvalidDefaultHead,
    UnknownRequiredTarget,
}

/// Classify one raw Git ref before producing native Threads or markers.
pub fn classify_git_import_ref(
    reference: &ImportRefIdentity,
) -> std::result::Result<ImportRefDisposition, ImportRefFailure> {
    let name = reference.raw_name.as_slice();
    if name == b"HEAD" {
        return match &reference.raw_target {
            GitRefTarget::Symbolic(target)
                if target.starts_with(b"refs/heads/") && target.len() > b"refs/heads/".len() =>
            {
                Ok(ImportRefDisposition::DefaultHead)
            }
            _ => Err(ImportRefFailure::InvalidDefaultHead),
        };
    }
    if matches!(&reference.raw_target, GitRefTarget::Symbolic(_)) {
        return Ok(ImportRefDisposition::Unsupported {
            reason: ImportSkipReason::SymbolicRef,
        });
    }
    if name == b"refs/notes/heddle" {
        return match &reference.raw_target {
            GitRefTarget::Direct {
                object_type: GitRefObjectType::Commit,
                ..
            } => Ok(ImportRefDisposition::RequiredNotes),
            _ => Err(ImportRefFailure::MissingRequiredTarget),
        };
    }
    let kind = match &reference.raw_target {
        GitRefTarget::Direct { object_type, .. } => object_type,
        GitRefTarget::Symbolic(_) => return Err(ImportRefFailure::UnknownRequiredTarget),
    };
    if name.starts_with(b"refs/heads/") && name.len() > b"refs/heads/".len() {
        return match (
            kind,
            reference.peeled_commit.as_ref(),
            reference.direct_oid(),
        ) {
            (GitRefObjectType::Commit, Some(peeled), Some(raw)) if peeled == raw => {
                Ok(ImportRefDisposition::Branch)
            }
            (GitRefObjectType::Unknown, _, _) => Err(ImportRefFailure::MissingRequiredTarget),
            _ => Err(ImportRefFailure::UnknownRequiredTarget),
        };
    }
    if name.starts_with(b"refs/tags/") && name.len() > b"refs/tags/".len() {
        return match kind {
            GitRefObjectType::Commit | GitRefObjectType::Tag
                if reference.peeled_commit.is_some() =>
            {
                Ok(ImportRefDisposition::CommitTag)
            }
            GitRefObjectType::Tag | GitRefObjectType::Tree | GitRefObjectType::Blob => {
                Ok(ImportRefDisposition::Unsupported {
                    reason: ImportSkipReason::NonCommitTag,
                })
            }
            GitRefObjectType::Unknown => Err(ImportRefFailure::MissingRequiredTarget),
            _ => Err(ImportRefFailure::UnknownRequiredTarget),
        };
    }
    let reason = if *kind == GitRefObjectType::Unknown {
        ImportSkipReason::DanglingUnsupported
    } else if name.starts_with(b"refs/remotes/") {
        ImportSkipReason::RemoteTracking
    } else if name.starts_with(b"refs/replace/") {
        ImportSkipReason::Replace
    } else if name.starts_with(b"refs/pull/") {
        ImportSkipReason::Pull
    } else if name.starts_with(b"refs/notes/") {
        ImportSkipReason::OtherNotes
    } else {
        ImportSkipReason::OtherRef
    };
    Ok(ImportRefDisposition::Unsupported { reason })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedImportRef {
    pub raw_name: Vec<u8>,
    pub reason: ImportSkipReason,
}

/// Result of classifying a complete, raw-name-sorted frozen ref set.
/// Dispositions have the same order as the input; no advertised ref is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedImportRefs {
    pub default_branch: Vec<u8>,
    pub dispositions: Vec<ImportRefDisposition>,
    pub skipped_refs: Vec<SkippedImportRef>,
    pub native_ref_count: u32,
    pub partial: bool,
}

pub fn classify_frozen_import_refs(refs: &[ImportRefIdentity]) -> Result<ClassifiedImportRefs> {
    let mut native_ref_count = 0usize;
    let mut dispositions = Vec::with_capacity(refs.len());
    let mut skipped_refs = Vec::new();
    let mut default_branch = None;
    let mut last_name: Option<&[u8]> = None;
    let mut partial = false;
    for reference in refs {
        if reference.raw_name.is_empty()
            || last_name.is_some_and(|last| last >= reference.raw_name.as_slice())
        {
            return Err(invalid(
                "Git import refs are not strictly sorted by raw name",
            ));
        }
        last_name = Some(&reference.raw_name);
        let disposition = classify_git_import_ref(reference)
            .map_err(|error| invalid(format!("Git import ref cannot be represented: {error:?}")))?;
        if disposition == ImportRefDisposition::DefaultHead {
            let GitRefTarget::Symbolic(target) = &reference.raw_target else {
                return Err(invalid("default HEAD is not symbolic"));
            };
            default_branch = Some(target.clone());
        }
        if matches!(
            disposition,
            ImportRefDisposition::Branch | ImportRefDisposition::CommitTag
        ) {
            native_ref_count += 1;
            if native_ref_count > MAX_IMPORT_REFS {
                return Err(invalid("Git import has more than 512 native refs"));
            }
        }
        if let ImportRefDisposition::Unsupported { reason } = disposition {
            skipped_refs.push(SkippedImportRef {
                raw_name: reference.raw_name.clone(),
                reason,
            });
            partial = true;
        }
        dispositions.push(disposition);
    }
    let default_branch =
        default_branch.ok_or_else(|| invalid("Git import has no symbolic HEAD"))?;
    let default_index = refs
        .binary_search_by(|reference| reference.raw_name.cmp(&default_branch))
        .map_err(|_| invalid("Git import HEAD points to an absent branch"))?;
    if dispositions.get(default_index) != Some(&ImportRefDisposition::Branch) {
        return Err(invalid(
            "Git import HEAD does not point to a supported branch",
        ));
    }
    Ok(ClassifiedImportRefs {
        default_branch,
        dispositions,
        skipped_refs,
        native_ref_count: native_ref_count as u32,
        partial,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unrepresentable_ref_kinds_have_explicit_partial_outcomes() {
        let target = GitObjectId::Sha1([1; 20]);
        for (name, object_type, reason) in [
            (
                b"refs/remotes/origin/main".as_slice(),
                GitRefObjectType::Commit,
                ImportSkipReason::RemoteTracking,
            ),
            (
                b"refs/replace/abc".as_slice(),
                GitRefObjectType::Commit,
                ImportSkipReason::Replace,
            ),
            (
                b"refs/pull/12/head".as_slice(),
                GitRefObjectType::Commit,
                ImportSkipReason::Pull,
            ),
            (
                b"refs/notes/other".as_slice(),
                GitRefObjectType::Commit,
                ImportSkipReason::OtherNotes,
            ),
            (
                b"refs/tags/blob".as_slice(),
                GitRefObjectType::Blob,
                ImportSkipReason::NonCommitTag,
            ),
            (
                b"refs/tags/tree".as_slice(),
                GitRefObjectType::Tree,
                ImportSkipReason::NonCommitTag,
            ),
            (
                b"refs/custom/x".as_slice(),
                GitRefObjectType::Commit,
                ImportSkipReason::OtherRef,
            ),
            (
                b"refs/custom/dangling".as_slice(),
                GitRefObjectType::Unknown,
                ImportSkipReason::DanglingUnsupported,
            ),
        ] {
            let reference = ImportRefIdentity {
                raw_name: name.to_vec(),
                raw_target: GitRefTarget::Direct {
                    oid: target.clone(),
                    object_type,
                },
                peeled_commit: None,
            };
            assert_eq!(
                classify_git_import_ref(&reference),
                Ok(ImportRefDisposition::Unsupported { reason }),
                "{}",
                String::from_utf8_lossy(name),
            );
        }
        let symbolic = ImportRefIdentity {
            raw_name: b"refs/remotes/origin/HEAD".to_vec(),
            raw_target: GitRefTarget::Symbolic(b"refs/remotes/origin/main".to_vec()),
            peeled_commit: None,
        };
        assert_eq!(
            classify_git_import_ref(&symbolic),
            Ok(ImportRefDisposition::Unsupported {
                reason: ImportSkipReason::SymbolicRef,
            })
        );
        for name in [b"refs/heads/main".as_slice(), b"refs/tags/v1".as_slice()] {
            let dangling = ImportRefIdentity {
                raw_name: name.to_vec(),
                raw_target: GitRefTarget::Direct {
                    oid: target.clone(),
                    object_type: GitRefObjectType::Unknown,
                },
                peeled_commit: None,
            };
            assert_eq!(
                classify_git_import_ref(&dangling),
                Err(ImportRefFailure::MissingRequiredTarget)
            );
        }
        let detached_head = ImportRefIdentity {
            raw_name: b"HEAD".to_vec(),
            raw_target: GitRefTarget::Direct {
                oid: target,
                object_type: GitRefObjectType::Commit,
            },
            peeled_commit: None,
        };
        assert_eq!(
            classify_git_import_ref(&detached_head),
            Err(ImportRefFailure::InvalidDefaultHead)
        );
    }

    #[test]
    fn complete_ref_classification_keeps_unsupported_refs_and_requires_head() {
        let commit = GitObjectId::Sha1([4; 20]);
        let head = ImportRefIdentity {
            raw_name: b"HEAD".to_vec(),
            raw_target: GitRefTarget::Symbolic(b"refs/heads/main".to_vec()),
            peeled_commit: Some(commit.clone()),
        };
        let branch = ImportRefIdentity {
            raw_name: b"refs/heads/main".to_vec(),
            raw_target: GitRefTarget::Direct {
                oid: commit.clone(),
                object_type: GitRefObjectType::Commit,
            },
            peeled_commit: Some(commit.clone()),
        };
        let remote = ImportRefIdentity {
            raw_name: b"refs/remotes/origin/main".to_vec(),
            raw_target: GitRefTarget::Direct {
                oid: commit,
                object_type: GitRefObjectType::Commit,
            },
            peeled_commit: None,
        };
        let refs = vec![head, branch, remote];
        let result = classify_frozen_import_refs(&refs).expect("complete snapshot");
        assert_eq!(result.default_branch, b"refs/heads/main");
        assert_eq!(result.native_ref_count, 1);
        assert!(result.partial);
        assert_eq!(
            result.skipped_refs,
            vec![SkippedImportRef {
                raw_name: b"refs/remotes/origin/main".to_vec(),
                reason: ImportSkipReason::RemoteTracking,
            }]
        );
        assert_eq!(result.dispositions.len(), refs.len());
        assert!(classify_frozen_import_refs(&refs[1..]).is_err());
        let mut moved = refs;
        moved[0].raw_target = GitRefTarget::Symbolic(b"refs/heads/absent".to_vec());
        assert!(classify_frozen_import_refs(&moved).is_err());
    }
}
