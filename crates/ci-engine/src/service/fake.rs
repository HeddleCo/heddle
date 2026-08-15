// SPDX-License-Identifier: Apache-2.0
//! Recording command boundary for service-provider tests.

use std::sync::Mutex;

use super::{CommandOutcome, CommandRunner, ServiceError};

/// A recording command runner with configurable `docker run` failure.
#[derive(Debug)]
pub struct FakeProvider {
    calls: Mutex<Vec<Vec<String>>>,
    fail_run_at: Option<usize>,
    run_count: Mutex<usize>,
}

impl FakeProvider {
    /// Construct a recorder whose calls all succeed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_run_at: None,
            run_count: Mutex::new(0),
        }
    }

    /// Construct a recorder whose first `docker run` fails.
    #[must_use]
    pub fn failing() -> Self {
        Self::failing_run_at(1)
    }

    /// Construct a recorder whose one-based `docker run` number fails.
    #[must_use]
    pub fn failing_run_at(number: usize) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_run_at: Some(number),
            run_count: Mutex::new(0),
        }
    }

    /// Snapshot every recorded full argv.
    #[must_use]
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("calls mutex poisoned").clone()
    }
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for FakeProvider {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandOutcome, ServiceError> {
        let mut call = Vec::with_capacity(args.len() + 1);
        call.push(program.to_string());
        call.extend(args.iter().cloned());
        let success = if args.first().map(String::as_str) == Some("run") {
            let mut count = self.run_count.lock().expect("run-count mutex poisoned");
            *count += 1;
            self.fail_run_at != Some(*count)
        } else {
            true
        };
        self.calls.lock().expect("calls mutex poisoned").push(call);
        Ok(CommandOutcome {
            success,
            output: if success {
                String::new()
            } else {
                "fake failure".to_string()
            },
        })
    }
}
