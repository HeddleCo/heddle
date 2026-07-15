// SPDX-License-Identifier: Apache-2.0
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

use super::{git_hermetic, heddle};

pub(crate) struct GitOverlayFixture {
    temp: TempDir,
    work: PathBuf,
    origin: Option<PathBuf>,
    ready_thread: Option<(String, PathBuf)>,
}

impl GitOverlayFixture {
    pub(crate) fn imported_main() -> Self {
        let temp = TempDir::new().expect("create git-overlay fixture tempdir");
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).expect("create git-overlay worktree");
        git_hermetic(&["init", "-b", "main"], &work);
        git_hermetic(&["config", "user.name", "Heddle Test"], &work);
        git_hermetic(&["config", "user.email", "heddle@example.com"], &work);
        std::fs::write(work.join("README.md"), "base\n").expect("seed README");
        git_hermetic(&["add", "README.md"], &work);
        git_hermetic(&["commit", "-m", "base"], &work);
        heddle(&["init"], Some(&work)).expect("initialize Git Overlay");
        heddle(&["import", "git", "--ref", "main"], Some(&work)).expect("import main");
        Self {
            temp,
            work,
            origin: None,
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
            recommended_action.contains(&format!("land --thread {thread}")),
            "ready helper should preserve the first-transition land action: {_ready}"
        );
        self.ready_thread = Some((thread.to_string(), thread_path));
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
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("fixture path utf8")
}

fn origin_str(path: &Path) -> &str {
    path.to_str().expect("origin path utf8")
}
