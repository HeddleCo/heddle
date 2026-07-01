// SPDX-License-Identifier: Apache-2.0
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

use super::{git_hermetic, heddle, heddle_output};

pub(crate) struct GitOverlayFixture {
    temp: TempDir,
    work: PathBuf,
    origin: Option<PathBuf>,
    peer: Option<PathBuf>,
    ready_thread: Option<(String, PathBuf)>,
}

impl GitOverlayFixture {
    pub(crate) fn adopted_main() -> Self {
        let temp = TempDir::new().expect("create git-overlay fixture tempdir");
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).expect("create git-overlay worktree");
        git_hermetic(&["init", "-b", "main"], &work);
        git_hermetic(&["config", "user.name", "Heddle Test"], &work);
        git_hermetic(&["config", "user.email", "heddle@example.com"], &work);
        std::fs::write(work.join("README.md"), "base\n").expect("seed README");
        git_hermetic(&["add", "README.md"], &work);
        git_hermetic(&["commit", "-m", "base"], &work);
        heddle(&["adopt", "--ref", "main"], Some(&work)).expect("adopt main");
        Self {
            temp,
            work,
            origin: None,
            peer: None,
            ready_thread: None,
        }
    }

    pub(crate) fn with_bare_origin(mut self) -> Self {
        if self.origin.is_some() {
            return self;
        }
        let origin = self.temp.path().join("origin.git");
        self.git_at(
            self.temp.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                origin_str(&origin),
            ],
        );
        self.git(&["remote", "add", "origin", origin_str(&origin)]);
        self.git(&["push", "-u", "origin", "main"]);
        self.origin = Some(origin);
        self
    }

    pub(crate) fn with_ready_materialized_thread(mut self, thread: &str) -> Self {
        let thread_path = self.temp.path().join(thread.replace(['/', '\\'], "-"));
        let _started = self.json(&[
            "--output",
            "json",
            "start",
            thread,
            "--path",
            path_str(&thread_path),
        ]);
        std::fs::write(thread_path.join("feature.txt"), "ready work\n")
            .expect("write ready thread work");
        let _ready = self.json_at(
            &thread_path,
            &["--output", "json", "ready", "-m", "ready thread work"],
        );
        assert_eq!(_ready["status"], "completed");
        assert_eq!(_ready["verification"]["verified"], true);
        assert_eq!(_ready["verification"]["status"], "clean");
        let recommended_action = _ready["recommended_action"].as_str().unwrap_or("");
        assert!(
            recommended_action.contains(&format!("land --thread {thread} --no-push")),
            "ready helper should preserve the first-transition land action: {_ready}"
        );
        self.ready_thread = Some((thread.to_string(), thread_path));
        self
    }

    pub(crate) fn with_remote_behind(mut self) -> Self {
        if self.origin.is_none() {
            self = self.with_bare_origin();
        }
        let origin = self.origin_path().to_path_buf();
        let peer = self.temp.path().join("peer");
        self.git_at(
            self.temp.path(),
            &["clone", path_str(&origin), path_str(&peer)],
        );
        self.git_at(&peer, &["config", "user.name", "Peer"]);
        self.git_at(&peer, &["config", "user.email", "peer@example.com"]);
        std::fs::write(peer.join("README.md"), "base\npeer\n").expect("advance peer");
        self.git_at(&peer, &["add", "README.md"]);
        self.git_at(&peer, &["commit", "-m", "peer"]);
        self.git_at(&peer, &["push", "origin", "main"]);
        self.heddle(&["fetch", "origin"])
            .expect("fetch upstream drift");
        self.peer = Some(peer);
        self
    }

    pub(crate) fn with_index_lock(self) -> Self {
        std::fs::write(self.work.join(".git/index.lock"), "held by fixture\n")
            .expect("write git index lock");
        self
    }

    pub(crate) fn path(&self) -> &Path {
        &self.work
    }

    pub(crate) fn origin_path(&self) -> &Path {
        self.origin.as_deref().expect("fixture has bare origin")
    }

    pub(crate) fn ready_thread_path(&self) -> &Path {
        &self
            .ready_thread
            .as_ref()
            .expect("fixture has ready thread")
            .1
    }

    pub(crate) fn heddle(&self, args: &[&str]) -> Result<String, String> {
        heddle(args, Some(&self.work))
    }

    pub(crate) fn heddle_output(&self, args: &[&str]) -> Result<Output, String> {
        heddle_output(args, Some(&self.work))
    }

    pub(crate) fn json(&self, args: &[&str]) -> Value {
        self.json_at(&self.work, args)
    }

    pub(crate) fn json_at(&self, path: &Path, args: &[&str]) -> Value {
        let output = heddle(args, Some(path))
            .unwrap_or_else(|err| panic!("heddle {args:?} failed in {}: {err}", path.display()));
        serde_json::from_str(&output)
            .unwrap_or_else(|err| panic!("heddle {args:?} emitted invalid JSON: {err}: {output}"))
    }

    pub(crate) fn git(&self, args: &[&str]) {
        self.git_at(&self.work, args);
    }

    pub(crate) fn git_at(&self, path: &Path, args: &[&str]) {
        git_hermetic(args, path);
    }

    pub(crate) fn git_stdout(&self, args: &[&str]) -> String {
        self.git_stdout_at(&self.work, args)
    }

    pub(crate) fn git_stdout_at(&self, path: &Path, args: &[&str]) -> String {
        let output = self.git_output_at(path, args);
        assert!(
            output.status.success(),
            "git {args:?} failed in {}\nstdout: {}\nstderr: {}",
            path.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git_output_at(&self, path: &Path, args: &[&str]) -> Output {
        let mut command = Command::new("git");
        command.env_clear();
        if let Some(path_env) = std::env::var_os("PATH") {
            command.env("PATH", path_env);
        }
        command
            .env("HOME", path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env("TERM", "dumb")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(path)
            .output()
            .unwrap_or_else(|err| panic!("spawn git {args:?}: {err}"))
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("fixture path utf8")
}

fn origin_str(path: &Path) -> &str {
    path.to_str().expect("origin path utf8")
}
