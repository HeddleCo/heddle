// SPDX-License-Identifier: Apache-2.0
//! Shared human rendering for actionable CLI follow-ups.

use crate::cli::style;

pub(crate) fn print_next(action: &str) {
    print_bold_action("Next", action, 0);
}

pub(crate) fn print_next_step(action: &str) {
    print_bold_action("Next step", action, 0);
}

pub(crate) fn print_command(action: &str) {
    print_bold_action("command", action, 2);
}

pub(crate) fn print_nested_next_step(action: &str) {
    print_bold_action("next step", action, 4);
}

pub(crate) fn print_nested_optional(action: &str) {
    print_dim_action("optional", action, 4);
}

pub(crate) fn print_dim_next_step(action: &str) {
    print_dim_action("Next step", action, 0);
}

fn print_bold_action(label: &str, action: &str, indent: usize) {
    if action.trim().is_empty() {
        return;
    }
    println!("{}{}: {}", " ".repeat(indent), label, style::bold(action));
}

fn print_dim_action(label: &str, action: &str, indent: usize) {
    if action.trim().is_empty() {
        return;
    }
    println!("{}{}: {}", " ".repeat(indent), label, style::dim(action));
}
