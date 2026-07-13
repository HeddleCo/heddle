// SPDX-License-Identifier: Apache-2.0
//! Shared next-action selection and validation for command surfaces.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use super::command_catalog::{split_recommended_action, validate_recommended_action};
use crate::cli::render::write_stdout;

#[derive(Debug, Clone, Copy)]
pub(crate) struct NextActionValidationContext<'a> {
    pub(crate) emitting_command: &'a [&'a str],
}

impl<'a> NextActionValidationContext<'a> {
    pub(crate) fn new(
        emitting_command: &'a [&'a str],
        _repository_capability: repo::RepositoryCapability,
    ) -> Self {
        Self { emitting_command }
    }

    pub(crate) fn without_repo(emitting_command: &'a [&'a str]) -> Self {
        Self { emitting_command }
    }
}

pub(crate) fn validated_json_string<T: Serialize>(
    output: &T,
    context: NextActionValidationContext<'_>,
) -> Result<String> {
    let encoded = serde_json::to_string(output)?;
    let value: Value = serde_json::from_str(&encoded)
        .context("failed to re-read command JSON for next_action validation")?;
    validate_next_actions_in_value(&value, context)?;
    Ok(encoded)
}

fn write_validated_json_stdout<T: Serialize>(
    output: &T,
    context: NextActionValidationContext<'_>,
) -> Result<()> {
    let mut encoded = validated_json_string(output, context)?;
    encoded.push('\n');
    write_stdout(&encoded)
}

/// Emit a full command JSON contract after the runtime command gate has
/// rejected `--output json-compact` for commands without a projection.
pub(crate) fn write_full_command_json<T: Serialize>(
    output: &T,
    context: NextActionValidationContext<'_>,
) -> Result<()> {
    write_validated_json_stdout(output, context)
}

/// Emit a command's JSON, choosing the full contract or the compact
/// decision-surface projection (heddle#470). The `T: CompactProjection`
/// bound is the chokepoint: any output routed through here is guaranteed
/// to have a compact projection, so a new operator verb cannot silently
/// ship the full envelope under `--output json-compact`. The compact
/// payload is only built when `compact` is set.
pub(crate) fn write_command_json<T>(
    output: &T,
    compact: bool,
    context: NextActionValidationContext<'_>,
) -> Result<()>
where
    T: Serialize + super::compact::CompactProjection,
{
    if compact {
        write_validated_json_stdout(&output.compact(), context)
    } else {
        write_validated_json_stdout(output, context)
    }
}

pub(crate) fn validate_next_actions_in_value(
    value: &Value,
    context: NextActionValidationContext<'_>,
) -> Result<()> {
    validate_next_actions_at_path(value, context, "$")
}

fn validate_next_actions_at_path(
    value: &Value,
    context: NextActionValidationContext<'_>,
    path: &str,
) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = format!("{path}.{key}");
                if matches!(key.as_str(), "next_action" | "recommended_action")
                    && let Some(action) = child.as_str()
                {
                    // Action-field contract (HeddleCo/heddle#645): "no
                    // action needed" is `null` and "not applicable" is an
                    // absent field — the empty string is never a valid
                    // serialized action. Route emitters through
                    // `normalized_action` so empties become `None` before
                    // they reach this boundary.
                    if action.trim().is_empty() {
                        return Err(next_action_validation_error(format!(
                            "empty {key} at {child_path}: serialize no-action as null (or omit \
                             the field), never \"\" — see normalized_action"
                        )));
                    }
                    validate_next_action(action, context)
                        .with_context(|| format!("invalid {key} at {child_path}"))?;
                }
                validate_next_actions_at_path(child, context, &child_path)?;
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                validate_next_actions_at_path(child, context, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

// heddle#464 close-the-class note. This validator rejects *non-canonical*
// breadcrumbs (demoted verbs, wrong repo type, self-loops) but deliberately
// does NOT generically reject state-gated verbs (`resolve`/`continue`/`abort`)
// when the emitting repo lacks merge/operation state. A sound generic check is
// impractical here for two reasons:
//   1. Thread-scoped breadcrumbs carry their state in a *different* repository
//      than the one being validated — e.g. `sync` emits
//      `heddle --repo <thread-worktree> resolve --list` after materializing the
//      conflict in that worktree, while validation runs against the repo `sync`
//      ran from (where no merge is in progress). The validator cannot see the
//      worktree's state, so a state-gate would false-positive exactly the
//      correct scoped breadcrumbs.
//   2. The validator operates on the serialized JSON with only repository
//      *capability* in context; plumbing live merge/operation state through
//      every emit site is a broad change beyond this fix-round's blast radius.
// The class is instead closed at the *source* (see the audit: `cmd_sync`,
// `cmd_land`, and `describe_thread_advice` no longer emit a `resolve` breadcrumb
// from an unmaterialized state) and guarded by the regression tests.
pub(crate) fn validate_next_action(
    action: &str,
    context: NextActionValidationContext<'_>,
) -> Result<()> {
    let action = action.trim();
    if action.is_empty() {
        return Ok(());
    }
    validate_recommended_action(action).map_err(|err| {
        next_action_validation_error(format!("action is not a valid heddle command: {err}"))
    })?;
    let argv = split_recommended_action(action).map_err(|err| {
        next_action_validation_error(format!("action cannot be tokenized: {err}"))
    })?;
    let Some(command_path) = next_action_command_path(&argv) else {
        return Ok(());
    };

    reject_demoted_breadcrumbs(action, &command_path)?;
    reject_self_loop(action, &command_path, context)?;
    Ok(())
}

fn next_action_command_path(argv: &[String]) -> Option<Vec<&str>> {
    if argv.first().map(String::as_str) != Some("heddle") {
        return None;
    }
    let command_index = first_command_index(argv)?;
    let command = argv.get(command_index)?.as_str();
    if command == "thread" || command == "bridge" || command == "doctor" {
        return argv
            .get(command_index + 1)
            .map(|subcommand| vec![command, subcommand.as_str()])
            .or_else(|| Some(vec![command]));
    }
    Some(vec![command])
}

fn first_command_index(argv: &[String]) -> Option<usize> {
    let mut index = 1;
    while index < argv.len() {
        match argv[index].as_str() {
            "--repo" | "-C" | "--output" | "--color" | "--config" | "--config-file"
            | "--config-env" => index += 2,
            "--quiet" | "-q" | "--verbose" | "-v" | "--no-color" | "--profile" => index += 1,
            token if token.starts_with("-C") && token.len() > 2 => index += 1,
            token if token.starts_with('-') => index += 1,
            _ => return Some(index),
        }
    }
    None
}

fn reject_demoted_breadcrumbs(action: &str, command_path: &[&str]) -> Result<()> {
    match command_path {
        ["ship"] => Err(next_action_validation_error(format!(
            "`ship` was renamed to `land`; next_action `{action}` is non-canonical"
        ))),
        ["merge"] => Err(next_action_validation_error(format!(
            "`merge` is an advanced merge primitive; managed-thread next_action `{action}` must use `ready`, `sync`, or `land`"
        ))),
        ["thread", "refresh"] => Err(next_action_validation_error(format!(
            "`thread refresh` is an implementation-shaped freshness primitive; next_action `{action}` must use `sync`"
        ))),
        ["thread", "resolve"] => Err(next_action_validation_error(format!(
            "`thread resolve` is not a breadcrumb; next_action `{action}` must use `resolve`, `continue`, `sync`, or `land`"
        ))),
        _ => Ok(()),
    }
}

fn reject_self_loop(
    action: &str,
    command_path: &[&str],
    context: NextActionValidationContext<'_>,
) -> Result<()> {
    if context.emitting_command == command_path {
        return Err(next_action_validation_error(format!(
            "next_action `{action}` is a self-loop for `{}`",
            context.emitting_command.join(" ")
        )));
    }
    Ok(())
}

fn next_action_validation_error(message: String) -> anyhow::Error {
    anyhow::Error::msg(message)
}

/// The single normalizer for serialized action fields
/// (HeddleCo/heddle#645). Action selectors use `String` with "empty means
/// no action"; this collapses that convention at the boundary —
/// empty/whitespace-only becomes `None`, so JSON output
/// serializes `null` (or omits the field) and `""` can never leak into a
/// `next_action`/`recommended_action`. Route every output-struct
/// assignment through here instead of ad-hoc `.is_empty()` checks; the
/// serialization walker in `validate_next_actions_at_path` rejects any
/// empty string that slips past.
pub(crate) fn normalized_action(action: impl Into<String>) -> Option<String> {
    let action = action.into();
    if action.trim().is_empty() {
        None
    } else {
        Some(action)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ctx(command: &'static [&'static str]) -> NextActionValidationContext<'static> {
        NextActionValidationContext::new(command, repo::RepositoryCapability::NativeHeddle)
    }

    #[test]
    fn validator_accepts_canonical_everyday_actions() {
        for action in [
            "heddle capture -m \"...\"",
            "heddle ready --thread feature",
            "heddle land --thread feature",
            "heddle sync --thread feature",
            "heddle resolve --list",
            "heddle continue",
            "heddle abort",
            "heddle push",
        ] {
            validate_next_action(action, ctx(&["status"]))
                .unwrap_or_else(|err| panic!("expected `{action}` to validate: {err:#}"));
        }
    }

    #[test]
    fn validator_rejects_demoted_breadcrumbs() {
        for action in [
            "heddle ship --thread feature",
            "heddle merge feature --preview",
            "heddle thread refresh feature",
            "heddle thread resolve feature",
        ] {
            assert!(
                validate_next_action(action, ctx(&["status"])).is_err(),
                "`{action}` should be rejected as a next_action"
            );
        }
    }

    #[test]
    fn validator_rejects_self_loops() {
        let err = validate_next_action(
            "heddle ready --thread feature",
            NextActionValidationContext::new(&["ready"], repo::RepositoryCapability::NativeHeddle),
        )
        .expect_err("ready must not point back to ready");
        assert!(err.to_string().contains("self-loop"));
    }

    #[test]
    fn normalized_action_maps_empty_and_whitespace_to_none() {
        assert_eq!(normalized_action(""), None);
        assert_eq!(normalized_action("   "), None);
        assert_eq!(
            normalized_action("heddle status"),
            Some("heddle status".to_string())
        );
    }

    #[test]
    fn boundary_rejects_empty_string_actions() {
        // HeddleCo/heddle#645: `""` is not a valid serialized action —
        // no-action is `null`, not-applicable is an absent field.
        for payload in [
            json!({"recommended_action": ""}),
            json!({"next_action": "  "}),
            json!({"nested": {"checks": [{"recommended_action": ""}]}}),
        ] {
            let err = validate_next_actions_in_value(&payload, ctx(&["status"]))
                .expect_err("empty-string action must fail the serialization boundary");
            assert!(
                err.to_string().contains("empty"),
                "rejection should name the empty-action contract: {err:#}"
            );
        }
    }

    #[test]
    fn boundary_accepts_null_and_absent_actions() {
        for payload in [
            json!({"recommended_action": null, "next_action": null}),
            json!({"output_kind": "status"}),
        ] {
            validate_next_actions_in_value(&payload, ctx(&["status"]))
                .expect("null/absent actions are the documented no-action encodings");
        }
    }

    #[test]
    fn recursive_validator_covers_nested_recommended_actions() {
        let payload = json!({
            "output_kind": "status",
            "recommended_action": "heddle capture -m \"...\"",
            "verification": {
                "checks": [
                    {"name": "Workflow", "recommended_action": "heddle thread resolve feature"}
                ]
            }
        });
        let err = validate_next_actions_in_value(&payload, ctx(&["status"]))
            .expect_err("nested demoted breadcrumbs must fail validation");
        assert!(
            err.to_string()
                .contains("$.verification.checks[0].recommended_action")
        );
    }
}
