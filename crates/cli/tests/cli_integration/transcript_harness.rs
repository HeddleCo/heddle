// SPDX-License-Identifier: Apache-2.0
//! Reusable, latency-free CLI journey transcripts and ceremony budgets.

use std::{fmt, path::Path};

use super::heddle_output;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DecisionCounts {
    pub commands: usize,
    pub choices: usize,
}

impl DecisionCounts {
    pub fn total(self) -> usize {
        self.commands + self.choices
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DecisionBudget {
    pub max_commands: usize,
    pub max_choices: usize,
    pub max_total: usize,
}

#[derive(Clone, Debug)]
pub(super) struct RecordedCommand {
    pub argv: Vec<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl RecordedCommand {
    pub fn assert_success(&self) {
        assert_eq!(
            self.exit_code,
            Some(0),
            "command failed: heddle {}\nstdout: {}\nstderr: {}",
            self.argv.join(" "),
            self.stdout,
            self.stderr
        );
    }

    pub fn json(&self) -> serde_json::Value {
        self.assert_success();
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!(
                "heddle {} did not emit JSON: {error}\nstdout: {}\nstderr: {}",
                self.argv.join(" "),
                self.stdout,
                self.stderr
            )
        })
    }
}

#[derive(Clone, Debug)]
enum TranscriptEntry {
    Command(RecordedCommand),
    Choice { prompt: String, selection: String },
}

#[derive(Debug)]
pub(super) struct BudgetExceeded {
    journey: String,
    observed: DecisionCounts,
    budget: DecisionBudget,
}

impl BudgetExceeded {
    pub fn observed(&self) -> DecisionCounts {
        self.observed
    }
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ceremony exceeded: observed commands={}, choices={}, total={}; budget commands<={}, choices<={}, total<={}",
            self.journey,
            self.observed.commands,
            self.observed.choices,
            self.observed.total(),
            self.budget.max_commands,
            self.budget.max_choices,
            self.budget.max_total
        )
    }
}

pub(super) struct SessionTranscript {
    journey: String,
    cwd: std::path::PathBuf,
    entries: Vec<TranscriptEntry>,
}

impl SessionTranscript {
    pub fn new(journey: impl Into<String>, cwd: &Path) -> Self {
        Self {
            journey: journey.into(),
            cwd: cwd.to_path_buf(),
            entries: Vec::new(),
        }
    }

    pub fn run(&mut self, args: &[&str]) -> RecordedCommand {
        let output = heddle_output(args, Some(&self.cwd))
            .unwrap_or_else(|error| panic!("failed to run heddle {}: {error}", args.join(" ")));
        let command = RecordedCommand {
            argv: args.iter().map(|arg| (*arg).to_string()).collect(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        self.entries.push(TranscriptEntry::Command(command.clone()));
        command
    }

    pub fn record_choice(&mut self, prompt: impl Into<String>, selection: impl Into<String>) {
        self.entries.push(TranscriptEntry::Choice {
            prompt: prompt.into(),
            selection: selection.into(),
        });
    }

    pub fn counts(&self) -> DecisionCounts {
        self.entries.iter().fold(
            DecisionCounts {
                commands: 0,
                choices: 0,
            },
            |mut counts, entry| {
                match entry {
                    TranscriptEntry::Command(_) => counts.commands += 1,
                    TranscriptEntry::Choice { .. } => counts.choices += 1,
                }
                counts
            },
        )
    }

    pub fn check_budget(&self, budget: DecisionBudget) -> Result<(), BudgetExceeded> {
        let observed = self.counts();
        if observed.commands <= budget.max_commands
            && observed.choices <= budget.max_choices
            && observed.total() <= budget.max_total
        {
            Ok(())
        } else {
            Err(BudgetExceeded {
                journey: self.journey.clone(),
                observed,
                budget,
            })
        }
    }

    pub fn assert_budget(&self, budget: DecisionBudget) {
        if let Err(error) = self.check_budget(budget) {
            panic!("{error}\n{self}");
        }
    }
}

impl fmt::Display for SessionTranscript {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "# {}", self.journey)?;
        for entry in &self.entries {
            match entry {
                TranscriptEntry::Command(command) => {
                    writeln!(formatter, "$ heddle {}", command.argv.join(" "))?;
                    writeln!(
                        formatter,
                        "[exit {}]",
                        command
                            .exit_code
                            .map_or_else(|| "signal".to_string(), |code| code.to_string())
                    )?;
                    write_stream(formatter, "stdout", &command.stdout, &self.cwd)?;
                    write_stream(formatter, "stderr", &command.stderr, &self.cwd)?;
                }
                TranscriptEntry::Choice { prompt, selection } => {
                    writeln!(formatter, "? {prompt}")?;
                    writeln!(formatter, "> {selection}")?;
                }
            }
        }
        let counts = self.counts();
        writeln!(
            formatter,
            "# decisions: {} (commands: {}, choices: {})",
            counts.total(),
            counts.commands,
            counts.choices
        )
    }
}

fn write_stream(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    stream: &str,
    cwd: &Path,
) -> fmt::Result {
    if stream.is_empty() {
        return Ok(());
    }
    let normalized = stream.replace(&cwd.display().to_string(), "<cwd>");
    writeln!(formatter, "[{name}]")?;
    writeln!(formatter, "{}", normalized.trim_end())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn prompt_selection_counts_as_a_decision_without_inventing_a_command() {
        let cwd = TempDir::new().unwrap();
        let mut transcript = SessionTranscript::new("interactive selection", cwd.path());
        transcript.record_choice("Select a thread", "feature/api");

        assert_eq!(
            transcript.counts(),
            DecisionCounts {
                commands: 0,
                choices: 1,
            }
        );
        let error = transcript
            .check_budget(DecisionBudget {
                max_commands: 0,
                max_choices: 0,
                max_total: 0,
            })
            .expect_err("a prompt choice must consume ceremony budget");
        assert_eq!(error.observed().total(), 1);
    }
}
