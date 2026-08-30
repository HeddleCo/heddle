"""Behavior tests for the published-crate drift CI checks."""

from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
VERSION_GATE = REPO_ROOT / "scripts/check-dependency-version-bump.py"
GRAPH_CHECK = REPO_ROOT / "scripts/check-published-api-graph.py"


def run(*args: str | Path, cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(arg) for arg in args],
        cwd=cwd,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )


def sparse_path(crate: str) -> Path:
    name = crate.lower()
    if len(name) == 1:
        return Path("1") / name
    if len(name) == 2:
        return Path("2") / name
    if len(name) == 3:
        return Path("3") / name[0] / name
    return Path(name[:2]) / name[2:4] / name


def index_record(name: str, version: str, api_requirement: str | None = None) -> dict:
    deps = []
    if api_requirement is not None:
        deps.append(
            {
                "name": "api",
                "package": "heddle-api",
                "req": api_requirement,
                "features": [],
                "optional": False,
                "default_features": True,
                "target": None,
                "kind": "normal",
            }
        )
    return {"name": name, "vers": version, "deps": deps, "yanked": False}


class DependencyVersionGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.repo = Path(self.tempdir.name)
        (self.repo / ".github/workflows").mkdir(parents=True)
        (self.repo / "crates/demo").mkdir(parents=True)
        (self.repo / ".github/workflows/publish-crates.yml").write_text(
            "env:\n  PUBLISHABLE_CRATES: |\n    heddle-demo\njobs: {}\n",
            encoding="utf-8",
        )
        self.write_root("0.1.0", 'serde = "1"')
        self.write_crate('anyhow = "1"')
        run("git", "init", "-q", cwd=self.repo)
        run("git", "config", "user.name", "CI Test", cwd=self.repo)
        run("git", "config", "user.email", "ci@example.invalid", cwd=self.repo)
        self.commit("base")
        self.base = run("git", "rev-parse", "HEAD", cwd=self.repo).stdout.strip()

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def write_root(self, version: str, dependency: str) -> None:
        (self.repo / "Cargo.toml").write_text(
            "[workspace]\n"
            'members = ["crates/demo"]\n'
            "\n[workspace.package]\n"
            f'version = "{version}"\n'
            "\n[workspace.dependencies]\n"
            f"{dependency}\n",
            encoding="utf-8",
        )

    def write_crate(self, dependency: str) -> None:
        (self.repo / "crates/demo/Cargo.toml").write_text(
            "[package]\n"
            'name = "heddle-demo"\n'
            "version.workspace = true\n"
            "\n[dependencies]\n"
            f"{dependency}\n",
            encoding="utf-8",
        )

    def commit(self, message: str) -> None:
        self.assertEqual(run("git", "add", ".", cwd=self.repo).returncode, 0)
        result = run("git", "commit", "-q", "-m", message, cwd=self.repo)
        self.assertEqual(result.returncode, 0, result.stdout)

    def gate(self, base: str) -> subprocess.CompletedProcess[str]:
        return run("python3", VERSION_GATE, base, "HEAD", cwd=self.repo)

    def test_workspace_dependency_change_requires_increased_version(self) -> None:
        self.write_root("0.1.0", 'serde = "2"')
        self.commit("dependency only")

        failed = self.gate(self.base)
        self.assertEqual(failed.returncode, 1, failed.stdout)
        self.assertIn("dependency changes require an increased", failed.stdout)

        self.write_root("0.1.1", 'serde = "2"')
        self.commit("version bump")
        passed = self.gate(self.base)
        self.assertEqual(passed.returncode, 0, passed.stdout)
        self.assertIn("workspace version increased from 0.1.0 to 0.1.1", passed.stdout)

    def test_no_dependency_change_passes(self) -> None:
        (self.repo / "README.md").write_text("docs only\n", encoding="utf-8")
        self.commit("docs")
        result = self.gate(self.base)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("no publishable dependency contracts changed", result.stdout)

    def test_publishable_crate_dependency_change_is_gated(self) -> None:
        self.write_crate('anyhow = "2"')
        self.commit("crate dependency only")
        result = self.gate(self.base)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/demo/Cargo.toml [dependencies]", result.stdout)


class PublishedGraphTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name)
        self.index = self.root / "index"
        self.workflow = self.root / "publish-crates.yml"
        self.workflow.write_text(
            "env:\n"
            "  PUBLISHABLE_CRATES: |\n"
            "    heddle-alpha\n"
            "    heddle-beta\n"
            "jobs: {}\n",
            encoding="utf-8",
        )
        self.write_records(
            "heddle-api",
            [
                index_record("heddle-api", "0.18.0"),
                index_record("heddle-api", "0.19.0"),
            ],
        )

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def write_records(self, crate: str, records: list[dict]) -> None:
        path = self.index / sparse_path(crate)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            "".join(
                json.dumps(record, separators=(",", ":")) + "\n" for record in records
            ),
            encoding="utf-8",
        )

    def check(self) -> subprocess.CompletedProcess[str]:
        return run(
            "python3",
            GRAPH_CHECK,
            "--workflow",
            self.workflow,
            "--index-dir",
            self.index,
            cwd=self.root,
        )

    def test_unified_graph_passes(self) -> None:
        self.write_records(
            "heddle-alpha",
            [
                index_record("heddle-alpha", "0.9.0", "^0.18.0"),
                index_record("heddle-alpha", "1.0.0", "^0.19.0"),
            ],
        )
        self.write_records(
            "heddle-beta", [index_record("heddle-beta", "1.0.0", ">=0.19, <0.20")]
        )
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("converge on heddle-api 0.19", result.stdout)

    def test_split_graph_fails(self) -> None:
        self.write_records(
            "heddle-alpha", [index_record("heddle-alpha", "1.0.0", "^0.18.0")]
        )
        self.write_records(
            "heddle-beta", [index_record("heddle-beta", "1.0.0", "^0.19.0")]
        )
        result = self.check()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("split across 2 major/minor lines", result.stdout)
        self.assertIn("heddle-api 0.18", result.stdout)
        self.assertIn("heddle-api 0.19", result.stdout)


if __name__ == "__main__":
    unittest.main()
