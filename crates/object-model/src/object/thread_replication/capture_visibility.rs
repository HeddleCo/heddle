//! Privacy declarations travel inside the original signed source result.
//! A courier cannot drop local declarations while retaining its author proof.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Capture;
use crate::{
    error::{HeddleError, Result},
    object::{EntryVisibility, EntryVisibilityEntry, State, VisibilityTier},
};

/// An authored visibility floor, independent of Thread sync and audience policy.
/// Entry commitments bind to this capture's tree; unchanged commitments also
/// inherit ancestor overrides when a reader evaluates the lineage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureVisibility {
    /// No explicit tier uses the Spool's applicable default.
    pub state: Option<VisibilityTier>,
    /// Advisory only. A later authorized transition releases an embargo.
    pub embargo_until: Option<DateTime<Utc>>,
    /// Sorted, unique `(tree_id, leaf_hash)` overrides for this source tree.
    pub entries: Vec<EntryVisibilityEntry>,
}

impl CaptureVisibility {
    /// Validate bounded canonical declarations. Source publication additionally
    /// proves each named tree and salted leaf belongs to the selected closure.
    pub fn validate(&self, source: &State) -> Result<()> {
        if (self.state.is_none() && self.entries.is_empty())
            || (self.embargo_until.is_some() && self.state.is_none())
            || self.entries.len() > 4096
        {
            return Err(invalid("empty or oversized capture visibility"));
        }
        for tier in self
            .state
            .iter()
            .chain(self.entries.iter().map(|entry| &entry.tier))
        {
            match tier {
                VisibilityTier::TeamScoped { team_id: label }
                | VisibilityTier::Restricted { scope_label: label }
                | VisibilityTier::Private { scope_label: label }
                    if label.trim().is_empty()
                        || label.len() > 256
                        || label.chars().any(char::is_control) =>
                {
                    return Err(invalid("invalid capture visibility label"));
                }
                _ => {}
            }
        }
        let canonical = EntryVisibility::new(source.change_id, source.tree, self.entries.clone())
            .map_err(invalid)?;
        if canonical.entries != self.entries {
            return Err(invalid("non-canonical capture entry visibility"));
        }
        Ok(())
    }

    /// Reconstruct the existing sidecar from the source identity covered by the
    /// same signature. No caller-supplied change or root can substitute for it.
    pub fn entry_sidecar(&self, source: &State) -> Result<Option<EntryVisibility>> {
        self.validate(source)?;
        if self.entries.is_empty() {
            return Ok(None);
        }
        EntryVisibility::new(source.change_id, source.tree, self.entries.clone())
            .map(Some)
            .map_err(invalid)
    }
}

impl Capture {
    /// Decode canonical source and validate every signed source-result binding.
    pub fn validated_state(&self) -> Result<State> {
        let state = State::decode_current_msgpack(&self.state)?;
        if state.encode_current_msgpack()? != self.state {
            return Err(invalid("non-canonical capture"));
        }
        if let Some(visibility) = &self.visibility {
            visibility.validate(&state)?;
        }
        Ok(state)
    }
}

fn invalid(error: impl std::fmt::Display) -> HeddleError {
    HeddleError::InvalidObject(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Attribution, ContentHash, Principal, Tree};

    fn fixture() -> Capture {
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![],
            Attribution::human(Principal::new("owner", "")),
        );
        let mut capture: Capture = state.encode_current_msgpack().expect("state").into();
        capture.visibility = Some(CaptureVisibility {
            state: Some(VisibilityTier::Private {
                scope_label: "security".into(),
            }),
            embargo_until: None,
            entries: vec![EntryVisibilityEntry {
                tree_id: state.tree,
                leaf_hash: ContentHash::from_bytes([3; 32]),
                tier: VisibilityTier::Internal,
            }],
        });
        capture
    }

    #[test]
    fn signed_capture_visibility_is_canonical_bounded_and_subject_derived() {
        let original = fixture();
        let source = original.validated_state().expect("valid capture");
        let sidecar = original
            .visibility
            .as_ref()
            .expect("privacy")
            .entry_sidecar(&source)
            .expect("validate")
            .expect("entries");
        assert_eq!(sidecar.change_id, source.change_id);
        assert_eq!(sidecar.tree_root, source.tree);
        let bytes = rmp_serde::to_vec_named(&original).expect("encode");
        let decoded: Capture = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, original);
        for bad in 0..4 {
            let mut changed = original.clone();
            let privacy = changed.visibility.as_mut().expect("privacy");
            match bad {
                0 => privacy.entries.push(privacy.entries[0].clone()),
                1 => {
                    privacy.state = Some(VisibilityTier::Private {
                        scope_label: "".into(),
                    })
                }
                2 => privacy.entries = vec![privacy.entries[0].clone(); 4097],
                _ => {
                    privacy.entries.clear();
                    privacy.state = None;
                }
            }
            assert!(changed.validated_state().is_err(), "invalid case {bad}");
        }
    }
}
