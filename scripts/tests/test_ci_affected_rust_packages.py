import json
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path("scripts/ci-affected-rust-packages.sh")


def metadata():
    root = "/repo"
    packages = []
    members = []

    def add(name, crate_dir, deps=()):
        pkg_id = f"path+file://{root}/{crate_dir}#{name}@0.1.0"
        members.append(pkg_id)
        packages.append(
            {
                "id": pkg_id,
                "name": name,
                "manifest_path": f"{root}/{crate_dir}/Cargo.toml",
                "dependencies": [{"name": dep} for dep in deps],
            }
        )

    add("heddle-objects", "crates/objects")
    add("heddle-repo", "crates/repo", ["heddle-objects"])
    add("heddle-cli", "crates/cli", ["heddle-repo"])
    add("heddle-review", "crates/review")
    return {
        "packages": packages,
        "workspace_members": members,
        "workspace_root": root,
    }


class AffectedRustPackagesTests(unittest.TestCase):
    def run_selector(self, *args):
        run = subprocess.run(
            ["bash", str(SCRIPT), *args],
            text=True,
            check=True,
            capture_output=True,
        )
        outputs = {}
        for line in run.stdout.splitlines():
            if "=" in line and not line.startswith("  "):
                key, value = line.split("=", 1)
                outputs[key] = value
        return {
            "all": outputs["all_packages"] == "true",
            "bench_all": outputs["bench_all"] == "true",
            "selected": [
                package for package in outputs["package_names_csv"].split(",") if package
            ],
        }

    def select(self, paths):
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            metadata_path = td / "metadata.json"
            paths_path = td / "paths.txt"
            metadata_path.write_text(json.dumps(metadata()))
            paths_path.write_text("\n".join(paths))

            return self.run_selector(
                "--changed-paths",
                str(paths_path),
                "--metadata-json",
                str(metadata_path),
            )

    def select_all(self):
        with tempfile.TemporaryDirectory() as td:
            metadata_path = Path(td) / "metadata.json"
            metadata_path.write_text(json.dumps(metadata()))
            return self.run_selector("--all", "--metadata-json", str(metadata_path))

    def test_crate_change_selects_reverse_dependency_closure(self):
        result = self.select(["crates/objects/src/lib.rs"])
        self.assertFalse(result["all"])
        self.assertEqual(
            result["selected"],
            ["heddle-objects", "heddle-repo", "heddle-cli"],
        )

    def test_docs_select_cli_doctor_tests_only(self):
        result = self.select(["docs/json-schemas.md"])
        self.assertFalse(result["all"])
        self.assertEqual(result["selected"], ["heddle-cli"])

    def test_workspace_manifest_fails_closed_to_all_packages(self):
        result = self.select(["Cargo.lock"])
        self.assertTrue(result["all"])

    def test_explicit_all_selects_every_package_and_benchmarks(self):
        result = self.select_all()
        self.assertTrue(result["all"])
        self.assertTrue(result["bench_all"])
        self.assertEqual(
            result["selected"],
            ["heddle-objects", "heddle-repo", "heddle-cli", "heddle-review"],
        )

    def test_script_only_change_can_skip_cargo(self):
        result = self.select(["scripts/tests/test_fuse_bench_compare.py"])
        self.assertFalse(result["all"])
        self.assertEqual(result["selected"], [])

    def test_cli_contract_script_selects_cli(self):
        result = self.select(["scripts/check-default-cli-contracts.sh"])
        self.assertFalse(result["all"])
        self.assertEqual(result["selected"], ["heddle-cli"])


if __name__ == "__main__":
    unittest.main()
