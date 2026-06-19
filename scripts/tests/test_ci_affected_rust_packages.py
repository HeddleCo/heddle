import json
import tempfile
import unittest
from importlib.machinery import SourceFileLoader
from pathlib import Path


MODULE = SourceFileLoader(
    "ci_affected_rust_packages", "scripts/ci-affected-rust-packages.py"
).load_module()


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
    def select(self, paths):
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            metadata_path = td / "metadata.json"
            paths_path = td / "paths.txt"
            metadata_path.write_text(json.dumps(metadata()))
            paths_path.write_text("\n".join(paths))

            md = json.loads(metadata_path.read_text())
            names, by_name, by_dir = MODULE.workspace_packages(md)
            reverse = MODULE.reverse_dependencies(names, by_name)
            all_packages, direct, bench_all, _ = MODULE.classify_paths(paths, by_dir)
            selected = set(names) if all_packages else MODULE.closure(direct, reverse)
            return {
                "all": all_packages,
                "bench_all": bench_all,
                "selected": [name for name in names if name in selected],
            }

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

    def test_script_only_change_can_skip_cargo(self):
        result = self.select(["scripts/tests/test_fuse_bench_compare.py"])
        self.assertFalse(result["all"])
        self.assertEqual(result["selected"], [])


if __name__ == "__main__":
    unittest.main()
