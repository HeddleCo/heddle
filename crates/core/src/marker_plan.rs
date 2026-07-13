// SPDX-License-Identifier: Apache-2.0
//! Pure marker delete/list planning (no refs/oplog I/O).

/// How `thread marker delete` selects targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerDeleteSelector {
    /// Exact marker name.
    Name(String),
    /// Non-empty name prefix.
    Prefix(String),
}

/// Invalid delete selector combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerDeleteSelectorError {
    /// Both name and prefix supplied.
    Conflict,
    /// Neither name nor prefix supplied.
    Required,
}

impl MarkerDeleteSelectorError {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Conflict => "marker_delete_selector_conflict",
            Self::Required => "marker_delete_selector_required",
        }
    }
}

/// Plan delete selector from CLI flags.
pub fn plan_marker_delete_selector(
    name: Option<String>,
    prefix: Option<String>,
) -> Result<MarkerDeleteSelector, MarkerDeleteSelectorError> {
    match (name, prefix) {
        (Some(name), None) => Ok(MarkerDeleteSelector::Name(name)),
        (None, Some(prefix)) => {
            if prefix.is_empty() {
                // Empty prefix is refused separately at apply time with a
                // dedicated empty-prefix kind; treat as Required here only
                // when prefix is absent. Empty string is still Prefix so
                // callers can map to empty-prefix advice.
                Ok(MarkerDeleteSelector::Prefix(prefix))
            } else {
                Ok(MarkerDeleteSelector::Prefix(prefix))
            }
        }
        (Some(_), Some(_)) => Err(MarkerDeleteSelectorError::Conflict),
        (None, None) => Err(MarkerDeleteSelectorError::Required),
    }
}

/// Whether a bulk prefix delete should refuse (empty prefix).
pub fn marker_prefix_is_valid(prefix: &str) -> bool {
    !prefix.is_empty()
}

/// List filter: empty / missing filter matches all; non-empty is prefix match.
pub fn marker_list_filter_matches(name: &str, filter: Option<&str>) -> bool {
    match filter {
        Some(prefix) if !prefix.is_empty() => name.starts_with(prefix),
        _ => true,
    }
}

/// Human message for bulk prefix delete outcomes.
pub fn marker_bulk_delete_message(prefix: &str, count: usize) -> String {
    match count {
        0 => format!("No markers matched prefix '{prefix}'"),
        1 => format!("Deleted 1 marker matching prefix '{prefix}'"),
        n => format!("Deleted {n} markers matching prefix '{prefix}'"),
    }
}

/// Create / delete single-marker success messages.
pub fn marker_create_message(name: &str, state_id_short: &str) -> String {
    format!("Created marker '{name}' at {state_id_short}")
}

pub fn marker_delete_message(name: &str) -> String {
    format!("Deleted marker '{name}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_selector_plans() {
        assert_eq!(
            plan_marker_delete_selector(Some("m".into()), None).unwrap(),
            MarkerDeleteSelector::Name("m".into())
        );
        assert_eq!(
            plan_marker_delete_selector(None, Some("pre".into())).unwrap(),
            MarkerDeleteSelector::Prefix("pre".into())
        );
        assert_eq!(
            plan_marker_delete_selector(Some("a".into()), Some("b".into())),
            Err(MarkerDeleteSelectorError::Conflict)
        );
        assert_eq!(
            plan_marker_delete_selector(None, None),
            Err(MarkerDeleteSelectorError::Required)
        );
    }

    #[test]
    fn prefix_and_filter() {
        assert!(marker_prefix_is_valid("x"));
        assert!(!marker_prefix_is_valid(""));
        assert!(marker_list_filter_matches("foo", None));
        assert!(marker_list_filter_matches("foo", Some("")));
        assert!(marker_list_filter_matches("foobar", Some("foo")));
        assert!(!marker_list_filter_matches("bar", Some("foo")));
    }

    #[test]
    fn bulk_messages() {
        assert!(marker_bulk_delete_message("p", 0).contains("No markers"));
        assert!(marker_bulk_delete_message("p", 1).contains("1 marker"));
        assert!(marker_bulk_delete_message("p", 3).contains("3 markers"));
    }
}
