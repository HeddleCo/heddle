// SPDX-License-Identifier: Apache-2.0
use std::{
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};

use super::*;

struct PerfCase {
    name: &'static str,
    args: &'static [&'static str],
    release_budget: Duration,
    expect_json: bool,
}

#[ignore = "release-mode command-surface perf smoke; run with `cargo test --release -p heddle-cli --test cli_integration core_loop_command_surface_perf_smoke -- --ignored --nocapture`"]
#[test]
fn core_loop_command_surface_perf_smoke() {
    let temp = setup_core_loop_fixture();

    let cases = [
        PerfCase {
            name: "bare help",
            args: &[],
            release_budget: Duration::from_millis(250),
            expect_json: false,
        },
        PerfCase {
            name: "help",
            args: &["help"],
            release_budget: Duration::from_millis(250),
            expect_json: false,
        },
        PerfCase {
            name: "command catalog json",
            args: &["--output", "json", "commands"],
            release_budget: Duration::from_millis(350),
            expect_json: true,
        },
        PerfCase {
            name: "status text",
            args: &["status"],
            release_budget: Duration::from_millis(650),
            expect_json: false,
        },
        PerfCase {
            name: "status short",
            args: &["status", "--short"],
            release_budget: Duration::from_millis(650),
            expect_json: false,
        },
        PerfCase {
            name: "status json",
            args: &["--output", "json", "status"],
            release_budget: Duration::from_millis(850),
            expect_json: true,
        },
        PerfCase {
            name: "workspace text",
            args: &["status"],
            release_budget: Duration::from_millis(650),
            expect_json: false,
        },
        PerfCase {
            name: "workspace json",
            args: &["--output", "json", "status"],
            release_budget: Duration::from_millis(850),
            expect_json: true,
        },
        PerfCase {
            name: "thread list json",
            args: &["--output", "json", "thread", "list"],
            release_budget: Duration::from_millis(850),
            expect_json: true,
        },
        PerfCase {
            name: "log json",
            args: &["--output", "json", "log"],
            release_budget: Duration::from_millis(850),
            expect_json: true,
        },
        PerfCase {
            name: "diff json",
            args: &["--output", "json", "diff"],
            release_budget: Duration::from_millis(1_000),
            expect_json: true,
        },
        PerfCase {
            name: "ready json",
            args: &["--output", "json", "ready"],
            release_budget: Duration::from_millis(1_500),
            expect_json: true,
        },
    ];

    let mut total = Duration::ZERO;
    println!("core loop command surface perf smoke:");
    for case in cases {
        let (elapsed, output) = run_timed_heddle(case.args, temp.path());
        total += elapsed;
        let stdout = std::str::from_utf8(&output.stdout).unwrap_or("");
        let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
        assert!(
            output.status.success(),
            "{} should succeed; elapsed={elapsed:?} stdout={stdout} stderr={stderr}",
            case.name
        );
        if case.expect_json {
            serde_json::from_str::<Value>(stdout)
                .unwrap_or_else(|_| panic!("{} should emit JSON: {stdout}", case.name));
        }
        println!(
            "  {:<22} {:>7} ms  budget={} ms",
            case.name,
            elapsed.as_millis(),
            case.release_budget.as_millis()
        );
        if !cfg!(debug_assertions) {
            assert!(
                elapsed <= case.release_budget,
                "{} exceeded release budget: elapsed={elapsed:?}, budget={:?}",
                case.name,
                case.release_budget
            );
        }
    }
    println!("  {:<22} {:>7} ms", "total", total.as_millis());
}

fn setup_core_loop_fixture() -> TempDir {
    let temp = TempDir::new().unwrap();
    run_ok(&["init"], temp.path());
    write_even_spread_files(temp.path(), 300);
    run_ok(&["capture", "-m", "seed"], temp.path());

    for index in 0..24 {
        let name = format!("perf/thread-{index:02}");
        let args = ["thread", "create", name.as_str()];
        run_ok(&args, temp.path());
    }

    std::fs::write(temp.path().join("tracked-00/file-000.txt"), "dirty\n").unwrap();
    temp
}

fn write_even_spread_files(root: &Path, count: usize) {
    for index in 0..count {
        let dir = root.join(format!("tracked-{:02}", index % 20));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("file-{index:03}.txt")),
            format!("fixture file {index}\n{}\n", "x".repeat(80)),
        )
        .unwrap();
    }
}

fn run_ok(args: &[&str], cwd: &Path) {
    let (_, output) = run_timed_heddle(args, cwd);
    let stdout = std::str::from_utf8(&output.stdout).unwrap_or("");
    let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        output.status.success(),
        "{args:?} should succeed; stdout={stdout} stderr={stderr}"
    );
}

fn run_timed_heddle(args: &[&str], cwd: &Path) -> (Duration, Output) {
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(cwd)
        .env("HEDDLE_CONFIG", cwd.join(".heddle-user/config.toml"))
        .env_remove("HEDDLE_PROFILE")
        .output()
        .expect("run heddle");
    (start.elapsed(), output)
}
