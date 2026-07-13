// SPDX-License-Identifier: Apache-2.0
//! Shared human rendering for actionable CLI follow-ups.

use crate::cli::style;

pub(crate) fn print_next(action: &str) {
    print_bold_action("Next", action, 0);
}

pub(crate) fn print_next_step(action: &str) {
    print_bold_action("Next step", action, 0);
}

pub(crate) fn print_next_step_dim(action: &str) {
    print_dim_action("Next step", action, 0);
}

pub(crate) fn print_command(action: &str) {
    print_bold_action("command", action, 2);
}

pub(crate) fn print_optional(action: &str) {
    print_dim_action("Optional", action, 0);
}

pub(crate) fn print_nested_next_step(action: &str) {
    print_bold_action("next step", action, 4);
}

pub(crate) fn print_nested_optional(action: &str) {
    print_dim_action("optional", action, 4);
}

pub(crate) fn format_next(action: &str, indent: usize) -> Option<String> {
    format_bold_action("Next", action, indent)
}

pub(crate) fn format_next_step_dim(action: &str, indent: usize) -> Option<String> {
    format_dim_action("Next step", action, indent)
}

fn print_bold_action(label: &str, action: &str, indent: usize) {
    if let Some(line) = format_bold_action(label, action, indent) {
        println!("{line}");
    }
}

fn print_dim_action(label: &str, action: &str, indent: usize) {
    if let Some(line) = format_dim_action(label, action, indent) {
        println!("{line}");
    }
}

fn format_bold_action(label: &str, action: &str, indent: usize) -> Option<String> {
    if action.trim().is_empty() {
        return None;
    }
    Some(format!(
        "{}{}: {}",
        " ".repeat(indent),
        label,
        style::bold(action)
    ))
}

fn format_dim_action(label: &str, action: &str, indent: usize) -> Option<String> {
    if action.trim().is_empty() {
        return None;
    }
    Some(format!(
        "{}{}: {}",
        " ".repeat(indent),
        label,
        style::dim(action)
    ))
}
