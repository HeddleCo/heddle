// SPDX-License-Identifier: Apache-2.0
//! Docker CLI implementation of the treadle service boundary.

use std::{
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

use ci_config::Service;

use super::{CommandRunner, RealCommandRunner, RunningServices, ServiceError, ServiceProvider};

const DEFAULT_HEALTH_TIMEOUT_SECS: u64 = 60;
const READINESS_POLL: Duration = Duration::from_millis(500);
const TCP_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Docker-CLI provider. Local CLI does not select this automatically.
pub struct DockerProvider {
    job: String,
    runner: Box<dyn CommandRunner>,
    health_timeout_secs: u64,
    skip_readiness: bool,
}

impl std::fmt::Debug for DockerProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DockerProvider")
            .field("job", &self.job)
            .field("health_timeout_secs", &self.health_timeout_secs)
            .field("skip_readiness", &self.skip_readiness)
            .finish_non_exhaustive()
    }
}

impl DockerProvider {
    /// Construct a Docker provider for one job namespace.
    #[must_use]
    pub fn new(job: impl Into<String>) -> Self {
        Self {
            job: job.into(),
            runner: Box::new(RealCommandRunner),
            health_timeout_secs: DEFAULT_HEALTH_TIMEOUT_SECS,
            skip_readiness: false,
        }
    }

    /// Inject the external-command boundary and skip real-clock readiness.
    #[must_use]
    pub fn with_runner(job: impl Into<String>, runner: Box<dyn CommandRunner>) -> Self {
        Self {
            job: job.into(),
            runner,
            health_timeout_secs: DEFAULT_HEALTH_TIMEOUT_SECS,
            skip_readiness: true,
        }
    }

    /// Override the per-service readiness timeout.
    #[must_use]
    pub fn health_timeout_secs(mut self, seconds: u64) -> Self {
        self.health_timeout_secs = seconds;
        self
    }

    fn container_name(&self, service: &Service) -> String {
        format!("hci-{}-{}", self.job, service.name)
    }

    fn run_args(&self, service: &Service) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            self.container_name(service),
        ];
        for port in &service.ports {
            args.extend(["-p".to_string(), format!("{port}:{port}")]);
        }
        for (key, value) in &service.env {
            args.extend(["-e".to_string(), format!("{key}={value}")]);
        }
        args.push(service.image.clone());
        args
    }

    fn await_ready(&self, service: &Service) -> Result<(), ServiceError> {
        if self.skip_readiness {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(self.health_timeout_secs);
        loop {
            if self.probe_ready(service) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ServiceError::NotReady {
                    name: service.name.clone(),
                    image: service.image.clone(),
                    timeout_secs: self.health_timeout_secs,
                });
            }
            std::thread::sleep(READINESS_POLL);
        }
    }

    fn probe_ready(&self, service: &Service) -> bool {
        match &service.ready_cmd {
            Some(command) if !command.is_empty() => {
                let mut args = vec!["exec".to_string(), self.container_name(service)];
                args.extend(command.iter().cloned());
                self.runner
                    .run("docker", &args)
                    .map(|outcome| outcome.success)
                    .unwrap_or(false)
            }
            _ => service.ports.first().is_none_or(|port| tcp_probe(*port)),
        }
    }

    fn teardown(&self, names: &[String]) {
        for name in names {
            let _ = self.runner.run(
                "docker",
                &["rm".to_string(), "-f".to_string(), name.clone()],
            );
        }
    }
}

fn tcp_probe(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, TCP_PROBE_TIMEOUT).is_ok()
}

impl ServiceProvider for DockerProvider {
    fn up(&self, services: &[Service]) -> Result<RunningServices, ServiceError> {
        let mut started = Vec::new();
        for service in services {
            let name = self.container_name(service);
            let outcome = self.runner.run("docker", &self.run_args(service))?;
            if !outcome.success {
                self.teardown(&started);
                return Err(ServiceError::Provision(format!(
                    "`docker run` for service {:?} failed: {}",
                    service.name,
                    outcome.output.trim()
                )));
            }
            started.push(name);
            if let Err(error) = self.await_ready(service) {
                self.teardown(&started);
                return Err(error);
            }
        }
        Ok(RunningServices { handles: started })
    }

    fn down(&self, running: RunningServices) -> Result<(), ServiceError> {
        self.teardown(&running.handles);
        Ok(())
    }
}
