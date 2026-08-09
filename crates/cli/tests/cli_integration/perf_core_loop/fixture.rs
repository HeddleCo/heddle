// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use repo::{FsMonitorMode, FsMonitorSettings, Repository, WorktreeStatusOptions};
use tempfile::TempDir;

pub(super) struct PerfFixture {
    _temp: TempDir,
    root: PathBuf,
    pub(super) path_count: usize,
    dirty_generation: usize,
}

impl PerfFixture {
    pub(super) fn new(binary: &Path, path_count: usize) -> Self {
        let setup_start = Instant::now();
        let temp = TempDir::new().expect("create perf fixture under TMPDIR");
        let root = temp.path().to_path_buf();

        let init_start = Instant::now();
        run_setup(binary, &["init"], &root);
        let init_ms = init_start.elapsed().as_millis();

        let files_start = Instant::now();
        write_files(&root, path_count);
        let files_ms = files_start.elapsed().as_millis();

        let seed_start = Instant::now();
        run_setup(binary, &["capture", "-m", "perf fixture seed"], &root);
        let seed_ms = seed_start.elapsed().as_millis();

        let repo = Repository::open(&root).expect("open perf fixture");
        let mut config = repo.config().clone();
        config.worktree.fsmonitor.mode = FsMonitorMode::Native;
        config
            .save(&repo.heddle_dir().join("config.toml"))
            .expect("enable native monitor");
        drop(repo);

        let warm_start = Instant::now();
        warm_native_monitor(binary, &root);
        let warm_ms = warm_start.elapsed().as_millis();
        println!(
            "SETUP paths={path_count} total_ms={} init_ms={init_ms} files_ms={files_ms} seed_ms={seed_ms} monitor_warm_ms={warm_ms}",
            setup_start.elapsed().as_millis()
        );

        Self {
            _temp: temp,
            root,
            path_count,
            dirty_generation: 0,
        }
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    pub(super) fn make_dirty(&mut self, content: &str) {
        fs::write(self.dirty_path(), content).expect("write dirty fixture path");
    }

    pub(super) fn prepare_capture_sample(&mut self) {
        self.dirty_generation += 1;
        let content = format!("captured generation {}\n", self.dirty_generation);
        self.make_dirty(&content);
    }

    pub(super) fn prepare_full_scan_sample(&self) {
        let index = self.root.join(".heddle/state/index.bin");
        match fs::remove_file(index) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove warm index for negative control: {error}"),
        }
    }

    pub(super) fn disable_warm_path(&self) {
        let repo = Repository::open(&self.root).expect("open perf fixture");
        let mut config = repo.config().clone();
        config.worktree.fsmonitor.mode = FsMonitorMode::Off;
        config
            .save(&repo.heddle_dir().join("config.toml"))
            .expect("disable monitor for negative control");
    }

    fn dirty_path(&self) -> PathBuf {
        self.root.join("tracked/dir-00000/file-000000.txt")
    }
}

fn warm_native_monitor(binary: &Path, root: &Path) {
    let options = WorktreeStatusOptions {
        fsmonitor: FsMonitorSettings {
            mode: FsMonitorMode::Native,
        },
    };
    for _ in 0..20 {
        run_setup(binary, &["--output", "json", "status"], root);
        let repo = Repository::open(root).expect("open perf fixture for monitor warmup");
        let report = repo
            .inspect_change_monitor_with_options(&options)
            .expect("inspect native monitor during warmup");
        if report.status == "usable" {
            run_setup(binary, &["--output", "json", "status"], root);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("native monitor did not become usable during fixture warmup");
}

fn write_files(root: &Path, count: usize) {
    const FILES_PER_DIRECTORY: usize = 100;
    for index in 0..count {
        let directory = root.join(format!("tracked/dir-{:05}", index / FILES_PER_DIRECTORY));
        if index % FILES_PER_DIRECTORY == 0 {
            fs::create_dir_all(&directory).expect("create fixture directory");
        }
        fs::write(
            directory.join(format!("file-{index:06}.txt")),
            format!("fixture {index}\n{}\n", "x".repeat(64)),
        )
        .expect("write fixture file");
    }
}

fn run_setup(binary: &Path, args: &[&str], cwd: &Path) {
    let output = base_command(binary, cwd)
        .args(args)
        .output()
        .expect("run setup command");
    assert!(
        output.status.success(),
        "setup {args:?} failed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) fn base_command(binary: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(cwd)
        .env_clear()
        .env("HEDDLE_CONFIG", cwd.join(".heddle-user/config.toml"))
        .env("HEDDLE_PRINCIPAL_NAME", "Performance Contract")
        .env("HEDDLE_PRINCIPAL_EMAIL", "perf-contract@heddle.test")
        .env("HEDDLE_AGENT_PROVIDER", "release-harness")
        .env("HEDDLE_AGENT_MODEL", "core-loop-v1")
        .env("NO_COLOR", "1");
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    command
}
