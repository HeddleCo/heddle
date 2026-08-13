// SPDX-License-Identifier: Apache-2.0

use std::{fs, path::Path};

use super::*;

const FIXTURE_COMMITS: usize = 512;
const FIXTURE_BRANCHES: usize = 32;
const FIXTURE_TAGS: usize = 16;
const GIT_REACHABLE_COPY_BUDGET: u64 = 0;
const RETAINED_MIRROR_PATH_BUDGET: u64 = 0;
const RETAINED_MIRROR_BYTE_BUDGET: u64 = 0;

#[test]
#[ignore = "release-only adopt structural contract; run with `HEDDLE_PROFILE=1 cargo test --locked --release -p heddle-cli --test cli_integration adopt_no_eager_mirror_release_contract -- --ignored --nocapture`"]
fn adopt_no_eager_mirror_release_contract() {
    assert!(
        !std::hint::black_box(cfg!(debug_assertions)),
        "adopt performance contract requires cargo test --release"
    );
    assert!(
        std::env::var("HEDDLE_PROFILE").is_ok(),
        "adopt performance contract requires HEDDLE_PROFILE=1"
    );

    let fixture = AdoptPerfFixture::new();
    let output = fixture.run_adopt();
    let stdout = std::str::from_utf8(&output.stdout).expect("adopt stdout is utf8");
    let stderr = std::str::from_utf8(&output.stderr).expect("adopt stderr is utf8");
    assert!(
        output.status.success(),
        "profiled adopt failed; stdout={stdout} stderr={stderr}"
    );

    let adopted: Value = serde_json::from_str(stdout).expect("adopt stdout is JSON");
    assert_eq!(adopted["commits_imported"], FIXTURE_COMMITS);
    assert_eq!(adopted["states_created"], FIXTURE_COMMITS);
    assert_eq!(adopted["branches_synced"], FIXTURE_BRANCHES);
    assert_eq!(adopted["tags_synced"], FIXTURE_TAGS);

    let trace = profile_trace(stderr);
    let copies = structural_count(&trace, "git_reachable_copy_operations");
    let mirror = fixture.root().join(".heddle/git");
    let (mirror_paths, mirror_bytes) = retained_path_shape(&mirror);
    println!(
        "ADOPT_GATE commits={FIXTURE_COMMITS} branches={FIXTURE_BRANCHES} tags={FIXTURE_TAGS} git_reachable_copy_operations={copies} copy_budget={GIT_REACHABLE_COPY_BUDGET} retained_mirror_paths={mirror_paths} path_budget={RETAINED_MIRROR_PATH_BUDGET} retained_mirror_bytes={mirror_bytes} byte_budget={RETAINED_MIRROR_BYTE_BUDGET}"
    );

    let mut failures = Vec::new();
    if copies > GIT_REACHABLE_COPY_BUDGET {
        failures.push(format!(
            "reachable Git-object copy operations {copies} > {GIT_REACHABLE_COPY_BUDGET}"
        ));
    }
    if mirror_paths > RETAINED_MIRROR_PATH_BUDGET {
        failures.push(format!(
            "retained legacy-mirror paths {mirror_paths} > {RETAINED_MIRROR_PATH_BUDGET}"
        ));
    }
    if mirror_bytes > RETAINED_MIRROR_BYTE_BUDGET {
        failures.push(format!(
            "retained legacy-mirror bytes {mirror_bytes} > {RETAINED_MIRROR_BYTE_BUDGET}"
        ));
    }
    assert!(
        failures.is_empty(),
        "ADOPT GATE RED: {}",
        failures.join("; ")
    );
    println!("ADOPT GATE green");
}

struct AdoptPerfFixture {
    _temp: TempDir,
}

impl AdoptPerfFixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("create adopt fixture");
        let git = SleyRepository::init(temp.path()).expect("init Git fixture");
        let tree = git_empty_tree_oid(&git);
        let mut commits = Vec::with_capacity(FIXTURE_COMMITS);
        let mut parent = None;
        for generation in 0..FIXTURE_COMMITS {
            let parents = parent.into_iter().collect::<Vec<_>>();
            let commit = git_commit_with_tree(
                &git,
                None,
                tree,
                &format!("adopt-perf-{generation:04}"),
                &parents,
            );
            commits.push(commit);
            parent = Some(commit);
        }

        git_set_reference(
            &git,
            "refs/heads/main",
            *commits.last().expect("fixture tip"),
        );
        for branch in 1..FIXTURE_BRANCHES {
            let generation = branch * (FIXTURE_COMMITS - 1) / (FIXTURE_BRANCHES - 1);
            git_set_reference(
                &git,
                &format!("refs/heads/fixture-{branch:02}"),
                commits[generation],
            );
        }
        for tag in 0..FIXTURE_TAGS {
            let generation = tag * (FIXTURE_COMMITS - 1) / (FIXTURE_TAGS - 1);
            git_set_reference(&git, &format!("refs/tags/v{tag:02}"), commits[generation]);
        }
        fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("attach fixture HEAD");
        Self { _temp: temp }
    }

    fn root(&self) -> &Path {
        self._temp.path()
    }

    fn run_adopt(&self) -> std::process::Output {
        let binary = Path::new(env!("CARGO_BIN_EXE_heddle"));
        let mut command = Command::new(binary);
        command
            .current_dir(self.root())
            .env_clear()
            .env(
                "HEDDLE_CONFIG",
                self.root().join(".heddle-user/config.toml"),
            )
            .env("HEDDLE_PRINCIPAL_NAME", "Adopt Performance Contract")
            .env("HEDDLE_PRINCIPAL_EMAIL", "adopt-perf@heddle.test")
            .env("HEDDLE_AGENT_PROVIDER", "release-harness")
            .env("HEDDLE_AGENT_MODEL", "adopt-no-mirror-v1")
            .env("HEDDLE_PROFILE", "jsonl")
            .env("NO_COLOR", "1")
            .args(["adopt", "--output", "json"]);
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmpdir);
        }
        command.output().expect("run profiled adopt")
    }
}

fn profile_trace(stderr: &str) -> Value {
    stderr
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|trace| trace["schema"] == "heddle-cli-profile/v1")
        .unwrap_or_else(|| panic!("adopt profile trace missing from stderr: {stderr}"))
}

fn structural_count(trace: &Value, name: &str) -> u64 {
    trace["phases"]
        .as_array()
        .and_then(|phases| {
            phases
                .iter()
                .find(|phase| phase["name"] == "structural counters")
        })
        .and_then(|phase| phase["metrics"][name]["value"].as_u64())
        .unwrap_or_else(|| panic!("structural counter {name:?} missing from trace: {trace}"))
}

fn retained_path_shape(root: &Path) -> (u64, u64) {
    if !root.exists() {
        return (0, 0);
    }
    let mut paths = 1;
    let mut bytes = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("read retained mirror path") {
            let entry = entry.expect("read retained mirror entry");
            let metadata = entry.metadata().expect("inspect retained mirror entry");
            paths += 1;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                bytes += metadata.len();
            }
        }
    }
    (paths, bytes)
}
