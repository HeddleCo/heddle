// SPDX-License-Identifier: Apache-2.0
//! Git identities and ref classification shared by local and hosted import.

use serde::{Deserialize, Serialize};

use super::invalid;
use crate::{
    error::Result,
    object::{MarkerName, ThreadName},
};

/// Native refs (branches plus commit tags) one Git import may carry.
///
/// This is `heddle_api::import_authority::MAX_IMPORT_SOURCE_REFS` (4096,
/// HeddleCo/api#389), the bound on a host's complete discovery and weft's
/// retained-ref cap, so the converter and the import authority cannot drift.
pub const MAX_IMPORT_REFS: usize = api::import_authority::MAX_IMPORT_SOURCE_REFS;

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
    NonUtf8RefName,
    InvalidNativeName,
    RemoteTracking,
    Replace,
    Pull,
    OtherNotes,
    NonCommitTag,
    SymbolicRef,
    OtherRef,
    DanglingUnsupported,
}

impl ImportSkipReason {
    pub fn description(self) -> &'static str {
        match self {
            Self::NonUtf8RefName => "ref name is not valid UTF-8",
            Self::InvalidNativeName => "invalid Git ref name",
            Self::RemoteTracking => "remote-tracking ref",
            Self::Replace => "replace ref",
            Self::Pull => "pull ref",
            Self::OtherNotes => "other notes ref",
            Self::NonCommitTag => "tag does not point to a commit",
            Self::SymbolicRef => "symbolic ref",
            Self::OtherRef => "unsupported ref namespace",
            Self::DanglingUnsupported => "dangling unsupported ref",
        }
    }
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
    let Ok(utf8_name) = std::str::from_utf8(name) else {
        return Ok(ImportRefDisposition::Unsupported {
            reason: ImportSkipReason::NonUtf8RefName,
        });
    };
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
    if name.starts_with(b"refs/pull/") {
        return Ok(ImportRefDisposition::Unsupported {
            reason: ImportSkipReason::Pull,
        });
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
        if ThreadName::from_git_branch(&utf8_name["refs/heads/".len()..]).is_err() {
            return Ok(ImportRefDisposition::Unsupported {
                reason: ImportSkipReason::InvalidNativeName,
            });
        }
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
        if MarkerName::from_git_tag(&utf8_name["refs/tags/".len()..]).is_err() {
            return Ok(ImportRefDisposition::Unsupported {
                reason: ImportSkipReason::InvalidNativeName,
            });
        }
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

impl SkippedImportRef {
    /// Keep valid Unicode readable; escape invalid bytes rather than displaying
    /// a replacement character that could name a different, valid Git ref.
    pub fn display_name(&self) -> String {
        std::str::from_utf8(&self.raw_name)
            .map_or_else(|_| self.raw_name.escape_ascii().to_string(), str::to_owned)
    }
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
                return Err(invalid(format!(
                    "Git import has more than {MAX_IMPORT_REFS} native refs"
                )));
            }
        }
        // Provider pull refs are outside the imported branch/tag surface.
        if let ImportRefDisposition::Unsupported { reason } = disposition
            && reason != ImportSkipReason::Pull
        {
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
    fn github_provider_refs_do_not_make_import_partial() {
        let commit = GitObjectId::Sha1([4; 20]);
        let mut refs = vec![ImportRefIdentity {
            raw_name: b"HEAD".to_vec(),
            raw_target: GitRefTarget::Symbolic(b"refs/heads/main".to_vec()),
            peeled_commit: None,
        }];
        for name in [
            b"refs/heads/main".as_slice(),
            b"refs/pull/12/head".as_slice(),
            b"refs/pull/12/merge".as_slice(),
            b"refs/tags/v1".as_slice(),
        ] {
            refs.push(ImportRefIdentity {
                raw_name: name.to_vec(),
                raw_target: GitRefTarget::Direct {
                    oid: commit.clone(),
                    object_type: GitRefObjectType::Commit,
                },
                peeled_commit: Some(commit.clone()),
            });
        }
        let classified = classify_frozen_import_refs(&refs).expect("GitHub-shaped refs");
        assert!(!classified.partial);
        assert!(classified.skipped_refs.is_empty());
        assert_eq!(classified.native_ref_count, 2);
        assert_eq!(classified.default_branch, b"refs/heads/main");
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

    /// HEAD plus `native` refs alternating branch / lightweight commit tag.
    fn frozen_refs_with_native(native: usize) -> Vec<ImportRefIdentity> {
        let commit = GitObjectId::Sha1([5; 20]);
        let direct = |raw_name: String| ImportRefIdentity {
            raw_name: raw_name.into_bytes(),
            raw_target: GitRefTarget::Direct {
                oid: commit.clone(),
                object_type: GitRefObjectType::Commit,
            },
            peeled_commit: Some(commit.clone()),
        };
        let mut refs = vec![ImportRefIdentity {
            raw_name: b"HEAD".to_vec(),
            raw_target: GitRefTarget::Symbolic(b"refs/heads/b-00000".to_vec()),
            peeled_commit: None,
        }];
        refs.extend(
            (0..native)
                .step_by(2)
                .map(|i| direct(format!("refs/heads/b-{i:05}"))),
        );
        refs.extend(
            (1..native)
                .step_by(2)
                .map(|i| direct(format!("refs/tags/t-{i:05}"))),
        );
        refs
    }

    #[test]
    fn native_ref_bound_admits_4096_and_refuses_4097() {
        // The converter and the import authority share one bound (heddle#2022).
        assert_eq!(
            MAX_IMPORT_REFS,
            api::import_authority::MAX_IMPORT_SOURCE_REFS
        );
        assert_eq!(MAX_IMPORT_REFS, 4096);
        for native in [600, MAX_IMPORT_REFS] {
            let classified = classify_frozen_import_refs(&frozen_refs_with_native(native))
                .unwrap_or_else(|error| panic!("{native} native refs: {error}"));
            assert_eq!(classified.native_ref_count as usize, native);
            assert!(!classified.partial);
        }
        let error = classify_frozen_import_refs(&frozen_refs_with_native(MAX_IMPORT_REFS + 1))
            .expect_err("one native ref past the bound");
        assert!(
            error
                .to_string()
                .contains("Git import has more than 4096 native refs"),
            "{error}"
        );
    }

    #[test]
    fn reserved_branch_is_admitted_for_escaped_native_emission() {
        let oid = GitObjectId::Sha1([8; 20]);
        let reference = ImportRefIdentity {
            raw_name: b"refs/heads/heddle/reserved".to_vec(),
            raw_target: GitRefTarget::Direct {
                oid: oid.clone(),
                object_type: GitRefObjectType::Commit,
            },
            peeled_commit: Some(oid),
        };
        assert!(matches!(
            classify_git_import_ref(&reference),
            Ok(ImportRefDisposition::Branch)
        ));
    }
}
