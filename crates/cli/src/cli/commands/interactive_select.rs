// SPDX-License-Identifier: Apache-2.0
//! TTY-only selection for genuinely ambiguous CLI targets.

use std::io::{BufRead, Write};

use anyhow::{Context, Result, anyhow};

use super::advice::RecoveryAdvice;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectionChoice {
    pub value: String,
    pub label: String,
}

impl SelectionChoice {
    pub(crate) fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

pub(crate) fn select_ambiguous_target(
    target_kind: &'static str,
    selector: &'static str,
    command_template: &str,
    choices: Vec<SelectionChoice>,
) -> Result<String> {
    let interactive = crate::cli::is_interactive_tty();
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    select_ambiguous_target_with_io(
        target_kind,
        selector,
        command_template,
        choices,
        interactive,
        &mut stdin.lock(),
        &mut stderr.lock(),
    )
}

#[allow(clippy::too_many_arguments)]
fn select_ambiguous_target_with_io(
    target_kind: &'static str,
    selector: &'static str,
    command_template: &str,
    choices: Vec<SelectionChoice>,
    interactive: bool,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<String> {
    debug_assert!(choices.len() > 1, "selection requires genuine ambiguity");
    if !interactive {
        return Err(anyhow!(ambiguous_target_advice(
            target_kind,
            selector,
            command_template,
            &choices,
        )));
    }

    writeln!(output, "Multiple {target_kind}s match. Select one:")?;
    for (index, choice) in choices.iter().enumerate() {
        writeln!(output, "  {}) {}", index + 1, choice.label)?;
    }

    loop {
        write!(output, "Selection [1-{}] (q to cancel): ", choices.len())?;
        output.flush()?;

        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 || answer.trim().eq_ignore_ascii_case("q") {
            return Err(anyhow!(ambiguous_target_advice(
                target_kind,
                selector,
                command_template,
                &choices,
            )));
        }
        if let Ok(index) = answer.trim().parse::<usize>()
            && let Some(choice) = index.checked_sub(1).and_then(|index| choices.get(index))
        {
            return Ok(choice.value.clone());
        }
        writeln!(output, "Enter a number from 1 to {}.", choices.len())
            .context("write interactive selection validation")?;
    }
}

fn ambiguous_target_advice(
    target_kind: &'static str,
    selector: &'static str,
    command_template: &str,
    choices: &[SelectionChoice],
) -> RecoveryAdvice {
    let advice_kind = match target_kind {
        "thread" => "ambiguous_thread_selection",
        "remote" => "ambiguous_remote_selection",
        "actor" => "ambiguous_actor_selection",
        _ => "ambiguous_target_selection",
    };
    let recovery_commands = choices
        .iter()
        .map(|choice| command_template.replace(selector, &choice.value))
        .collect();
    RecoveryAdvice::safety_refusal(
        advice_kind,
        format!("Multiple {target_kind}s match; pass {selector} to select one explicitly"),
        format!("Retry with `{command_template}`."),
        format!("more than one {target_kind} is a valid target and no safe default exists"),
        format!("continuing could target the wrong {target_kind}"),
        "no target was selected and repository state was left unchanged",
        command_template.to_string(),
        recovery_commands,
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn choices() -> Vec<SelectionChoice> {
        vec![
            SelectionChoice::new("alpha", "alpha"),
            SelectionChoice::new("beta", "beta"),
        ]
    }

    #[test]
    fn interactive_selection_returns_numbered_choice() {
        let mut input = Cursor::new(b"2\n".to_vec());
        let mut output = Vec::new();
        let selected = select_ambiguous_target_with_io(
            "thread",
            "<THREAD>",
            "heddle thread show <THREAD>",
            choices(),
            true,
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(selected, "beta");
        let prompt = String::from_utf8(output).unwrap();
        assert!(prompt.contains("Multiple threads match. Select one:"));
        assert!(prompt.contains("Selection [1-2]"));
    }

    #[test]
    fn non_interactive_selection_fails_without_reading_or_prompting() {
        let mut input = Cursor::new(b"1\n".to_vec());
        let mut output = Vec::new();
        let error = select_ambiguous_target_with_io(
            "thread",
            "<THREAD>",
            "heddle thread show <THREAD>",
            choices(),
            false,
            &mut input,
            &mut output,
        )
        .unwrap_err();

        assert!(output.is_empty());
        let advice = error.downcast_ref::<RecoveryAdvice>().unwrap();
        assert_eq!(advice.kind, "ambiguous_thread_selection");
        assert_eq!(advice.primary_command, "heddle thread show <THREAD>");
    }

    #[test]
    fn actor_ambiguity_primary_command_stays_a_presence_show() {
        use super::super::command_catalog::validate_recommended_action;

        let advice = ambiguous_target_advice(
            "actor",
            "<session>",
            "heddle presence show <session>",
            &choices(),
        );
        assert_eq!(advice.kind, "ambiguous_actor_selection");
        assert_eq!(advice.primary_command, "heddle presence show <session>");
        assert_ne!(advice.primary_command, "heddle help --output json");
        validate_recommended_action(&advice.primary_command)
            .unwrap_or_else(|err| panic!("presence show multi-match next must validate: {err}"));
        assert_eq!(
            advice.recovery_commands,
            vec![
                "heddle presence show alpha".to_string(),
                "heddle presence show beta".to_string(),
            ]
        );
    }
}
