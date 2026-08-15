// SPDX-License-Identifier: Apache-2.0
//! Service-container provisioning boundary harvested from treadle.

mod docker;
mod fake;

use std::process::Command;

use ci_config::Service;
pub use docker::DockerProvider;
pub use fake::FakeProvider;
use thiserror::Error;

/// Provider handles for services started for one check.
#[derive(Debug, Default)]
pub struct RunningServices {
    /// Provider-specific handles in authored order.
    pub handles: Vec<String>,
}

/// Service provisioning failure.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// The selected provider cannot run the requested service.
    #[error(
        "service container {name:?} ({image}) requested, but this provider cannot run services"
    )]
    Unsupported {
        /// Authored service name.
        name: String,
        /// Authored image.
        image: String,
    },
    /// Container startup failed.
    #[error("service provisioning failed: {0}")]
    Provision(String),
    /// Readiness deadline expired.
    #[error("service {name:?} ({image}) did not become ready within {timeout_secs}s")]
    NotReady {
        /// Authored service name.
        name: String,
        /// Authored image.
        image: String,
        /// Elapsed readiness deadline.
        timeout_secs: u64,
    },
}

/// Service lifecycle provider used by the executor.
pub trait ServiceProvider {
    /// Start all requested services.
    fn up(&self, services: &[Service]) -> Result<RunningServices, ServiceError>;
    /// Stop services from one successful `up` call.
    fn down(&self, running: RunningServices) -> Result<(), ServiceError>;
}

/// Provider for local mode: no implicit container runtime.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopProvider;

impl ServiceProvider for NoopProvider {
    fn up(&self, services: &[Service]) -> Result<RunningServices, ServiceError> {
        match services.first() {
            Some(service) => Err(ServiceError::Unsupported {
                name: service.name.clone(),
                image: service.image.clone(),
            }),
            None => Ok(RunningServices::default()),
        }
    }

    fn down(&self, _running: RunningServices) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// Captured outcome of one external runtime command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    /// Whether the command exited successfully.
    pub success: bool,
    /// Combined diagnostic text.
    pub output: String,
}

/// Boundary around external container-runtime commands.
pub trait CommandRunner: Send + Sync {
    /// Run one argv.
    fn run(&self, program: &str, args: &[String]) -> Result<CommandOutcome, ServiceError>;
}

/// Real external-command boundary used by [`DockerProvider`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandOutcome, ServiceError> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| ServiceError::Provision(format!("spawn {program}: {error}")))?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(CommandOutcome {
            success: output.status.success(),
            output: text,
        })
    }
}
