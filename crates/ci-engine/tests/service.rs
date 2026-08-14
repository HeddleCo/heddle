// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, sync::Arc};

use ci_config::Service;
use ci_engine::{
    CommandOutcome, CommandRunner, DockerProvider, FakeProvider, ServiceError, ServiceProvider,
};

struct SharedRunner(Arc<FakeProvider>);

impl CommandRunner for SharedRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandOutcome, ServiceError> {
        self.0.run(program, args)
    }
}

fn postgres() -> Service {
    Service {
        name: "db".to_string(),
        image: "postgres:17".to_string(),
        ports: vec![5432],
        env: BTreeMap::from([
            ("POSTGRES_DB".to_string(), "heddle".to_string()),
            ("POSTGRES_PASSWORD".to_string(), "postgres".to_string()),
        ]),
        ready_cmd: Some(vec!["pg_isready".to_string()]),
    }
}

#[test]
fn docker_boundary_preserves_treadle_run_and_teardown_argv() {
    let recorder = Arc::new(FakeProvider::new());
    let provider = DockerProvider::with_runner("local", Box::new(SharedRunner(recorder.clone())));
    let running = provider.up(&[postgres()]).expect("service starts");
    assert_eq!(running.handles, ["hci-local-db"]);

    provider.down(running).expect("service stops");
    let calls = recorder.calls();
    assert_eq!(
        calls[0],
        [
            "docker",
            "run",
            "-d",
            "--name",
            "hci-local-db",
            "-p",
            "5432:5432",
            "-e",
            "POSTGRES_DB=heddle",
            "-e",
            "POSTGRES_PASSWORD=postgres",
            "postgres:17",
        ]
    );
    assert_eq!(
        calls[1],
        ["docker", "rm", "-f", "hci-local-db"],
        "teardown is best effort but must issue docker rm -f"
    );
}

#[test]
fn docker_boundary_rolls_back_started_services() {
    let mut redis = postgres();
    redis.name = "cache".to_string();
    redis.image = "redis:8".to_string();
    redis.ports = vec![6379];
    redis.env.clear();

    let recorder = Arc::new(FakeProvider::failing_run_at(2));
    let provider = DockerProvider::with_runner("local", Box::new(SharedRunner(recorder.clone())));
    let error = provider
        .up(&[postgres(), redis])
        .expect_err("second service fails");
    assert!(matches!(error, ServiceError::Provision(_)));
    assert!(
        recorder
            .calls()
            .iter()
            .any(|call| call == &["docker", "rm", "-f", "hci-local-db"])
    );
}
