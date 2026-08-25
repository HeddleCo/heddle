// SPDX-License-Identifier: Apache-2.0
//! Pure harness relay JSON helpers (no process/registry I/O).
//!
//! Payload parse, path-based value extraction, and OpenCode tool status
//! classification. CLI still owns process detection and timeline mutation.

use std::collections::BTreeMap;

use objects::object::TimelineToolCallStatus;
use serde_json::Value;

/// Parse a harness relay payload string into JSON (or null + warning).
pub fn parse_relay_payload(payload: &str) -> (Value, Option<String>) {
    if payload.trim().is_empty() {
        return (Value::Null, None);
    }
    match serde_json::from_str::<Value>(payload) {
        Ok(value) => (value, None),
        Err(err) => (
            Value::Null,
            Some(format!(
                "warning: failed to parse harness relay payload as JSON: {err}; continuing with null payload"
            )),
        ),
    }
}

/// Walk nested object path segments; return string/bool/number as string.
pub fn value_string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    match current {
        Value::String(s) => Some(s.clone()),
        Value::Bool(v) => Some(v.to_string()),
        Value::Number(v) => Some(v.to_string()),
        _ => None,
    }
}

/// First successful [`value_string`] across alternate paths.
pub fn first_value_string(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| value_string(value, path))
}

/// String array at a nested path.
pub fn value_string_array(value: &Value, path: &[&str]) -> Option<Vec<String>> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect()
    })
}

/// Join string array elements with commas.
pub fn value_array_join(value: &Value, path: &[&str]) -> Option<String> {
    value_string_array(value, path).map(|items| items.join(","))
}

/// Nested u64 value.
pub fn value_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_u64()
}

/// Nested u64 as string.
pub fn value_u64_string(value: &Value, path: &[&str]) -> Option<String> {
    value_u64(value, path).map(|v| v.to_string())
}

/// Nested f64 cost in dollars → micros as string.
pub fn value_cost_micros(value: &Value, path: &[&str]) -> Option<String> {
    value_cost_micros_u64(value, path).map(|v| v.to_string())
}

/// Nested f64 cost in dollars → micros.
pub fn value_cost_micros_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_f64().map(|v| (v * 1_000_000.0).round() as u64)
}

/// Append unique non-empty strings.
pub fn merge_string_vec(target: &mut Vec<String>, incoming: Vec<String>) {
    for item in incoming {
        if !item.trim().is_empty() && !target.contains(&item) {
            target.push(item);
        }
    }
}

/// Build a BTreeMap from optional pairs.
pub fn map_from_pairs<const N: usize>(
    pairs: [(&str, Option<String>); N],
) -> BTreeMap<String, String> {
    pairs
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_string(), value)))
        .collect()
}

/// OpenCode tool name from common payload shapes.
pub fn opencode_tool_name(payload: &Value) -> String {
    first_value_string(
        payload,
        &[
            &["tool", "name"],
            &["toolName"],
            &["tool_name"],
            &["tool"],
            &["name"],
        ],
    )
    .unwrap_or_else(|| "tool".to_string())
}

/// Classify OpenCode tool status from payload fields.
pub fn opencode_tool_status(payload: &Value) -> TimelineToolCallStatus {
    let status = first_value_string(
        payload,
        &[
            &["status"],
            &["tool", "status"],
            &["result", "status"],
            &["output", "status"],
        ],
    )
    .unwrap_or_default()
    .to_ascii_lowercase();
    if status.contains("cancel") {
        TimelineToolCallStatus::Cancelled
    } else if status.contains("fail")
        || status.contains("error")
        || payload.get("error").is_some()
        || payload.get("exception").is_some()
    {
        TimelineToolCallStatus::Failed
    } else {
        TimelineToolCallStatus::Succeeded
    }
}

/// Operator verification claim policy (pure facts for success-claim gate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerificationClaimPolicyFacts {
    pub allow_land_publish_followup: bool,
    pub allow_matching_workflow_action: bool,
}

/// Trust-side facts for [`repository_verification_allows_success_claim`]
/// (grouped so the claim helper stays under clippy's arg limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationClaimTrustFacts<'a> {
    pub verified: bool,
    pub recommended_action: &'a str,
    pub remote_drift: &'a str,
    pub workflow_status: &'a str,
}

/// Whether operator output may claim success given verification state.
///
/// `is_land_landed` is true when action is Land and status is landed.
/// Matching-workflow path requires trust workflow ready and recommended action
/// equality (caller supplies those facts).
pub fn repository_verification_allows_success_claim(
    output_status: &str,
    trust: VerificationClaimTrustFacts<'_>,
    is_land_landed: bool,
    recommended_matches_trust: bool,
    policy: VerificationClaimPolicyFacts,
) -> bool {
    if trust.verified || matches!(output_status, "blocked" | "failed") {
        return true;
    }
    if policy.allow_land_publish_followup
        && is_land_landed
        && trust.recommended_action == "heddle push"
        && matches!(trust.remote_drift, "remote_untracked" | "remote_ahead")
    {
        return true;
    }
    if policy.allow_matching_workflow_action
        && trust.workflow_status == "ready"
        && recommended_matches_trust
    {
        return true;
    }
    false
}

/// Raw Git operation handoff recovery primary command.
pub fn raw_git_preservation_command() -> &'static str {
    "heddle verify"
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_relay_payload_and_paths() {
        let (v, warn) = parse_relay_payload("");
        assert!(v.is_null());
        assert!(warn.is_none());
        let (v, warn) = parse_relay_payload("{");
        assert!(v.is_null());
        assert!(warn.is_some());
        let (v, _) = parse_relay_payload(r#"{"tool":{"name":"edit"},"status":"failed"}"#);
        assert_eq!(opencode_tool_name(&v), "edit");
        assert_eq!(opencode_tool_status(&v), TimelineToolCallStatus::Failed);
        assert_eq!(value_string(&v, &["tool", "name"]).as_deref(), Some("edit"));
    }

    #[test]
    fn verification_claim_and_land_rewrite() {
        let verified = VerificationClaimTrustFacts {
            verified: true,
            recommended_action: "",
            remote_drift: "",
            workflow_status: "",
        };
        assert!(repository_verification_allows_success_claim(
            "completed",
            verified,
            false,
            false,
            VerificationClaimPolicyFacts::default()
        ));
        let unverified = VerificationClaimTrustFacts {
            verified: false,
            recommended_action: "",
            remote_drift: "",
            workflow_status: "",
        };
        assert!(!repository_verification_allows_success_claim(
            "completed",
            unverified,
            false,
            false,
            VerificationClaimPolicyFacts::default()
        ));
        let land_push = VerificationClaimTrustFacts {
            verified: false,
            recommended_action: "heddle push",
            remote_drift: "remote_ahead",
            workflow_status: "",
        };
        assert!(repository_verification_allows_success_claim(
            "landed",
            land_push,
            true,
            false,
            VerificationClaimPolicyFacts {
                allow_land_publish_followup: true,
                allow_matching_workflow_action: false,
            }
        ));
        let _ = json!({});
        assert_eq!(raw_git_preservation_command(), "heddle verify");
    }
}
