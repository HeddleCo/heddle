# SPDX-License-Identifier: Apache-2.0
"""Unit tests for `scripts/fuse-bench-compare.py`.

The compare script is the load-bearing half of the `fuse_e2e` perf gate:
if it ever exits 0 on a broken bench run (missing estimate files,
corrupt JSON, baseline drift), regressions ship silently. These tests
pin the contract that those scenarios MUST exit non-zero.

Codex r1 on PR #91 flagged the silent-skip behavior as a P1; these
tests are the red commit that pins it pre-fix.

Run via `python3 -m unittest discover -s scripts/tests` from repo root.
"""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
SCRIPT = REPO_ROOT / "scripts" / "fuse-bench-compare.py"


def _write_baseline(path: Path, entries: dict[str, dict[str, dict[str, float]]]) -> None:
    """Write a minimal baseline JSON. `entries` maps group -> variant -> fields."""
    body: dict = {
        "_meta": {
            "captured_on": "test",
            "regenerate_with": "n/a",
            "note": "test fixture",
        }
    }
    body.update(entries)
    path.write_text(json.dumps(body))


def _write_estimate(criterion_dir: Path, group: str, variant: str, mean_ns: float) -> None:
    """Write a criterion-shape estimates.json under `<group>/<variant>/new/`."""
    target_dir = criterion_dir / group / variant / "new"
    target_dir.mkdir(parents=True, exist_ok=True)
    (target_dir / "estimates.json").write_text(
        json.dumps({"mean": {"point_estimate": mean_ns}})
    )


def _write_corrupt_estimate(criterion_dir: Path, group: str, variant: str) -> None:
    """Write a non-JSON blob where estimates.json belongs."""
    target_dir = criterion_dir / group / variant / "new"
    target_dir.mkdir(parents=True, exist_ok=True)
    (target_dir / "estimates.json").write_text("{this is not valid json")


def _run_compare(criterion_dir: Path, baseline: Path, threshold: float = 0.20):
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--criterion-dir",
            str(criterion_dir),
            "--baseline",
            str(baseline),
            "--threshold",
            str(threshold),
        ],
        capture_output=True,
        text=True,
        check=False,
    )


class FuseBenchCompareTest(unittest.TestCase):
    """Contract tests for fuse-bench-compare.py exit codes."""

    def test_all_within_budget_exits_zero(self) -> None:
        """Baseline + matching estimates within threshold → exit 0."""
        with TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            baseline = tmp_path / "baseline.json"
            criterion = tmp_path / "criterion"
            _write_baseline(
                baseline,
                {"seq_read": {"heddle": {"ns_per_iter": 1000.0}}},
            )
            _write_estimate(criterion, "seq_read", "heddle", 1050.0)  # +5%, within 20%
            result = _run_compare(criterion, baseline)
            self.assertEqual(
                result.returncode,
                0,
                f"expected success; got rc={result.returncode}\n"
                f"stdout={result.stdout}\nstderr={result.stderr}",
            )

    def test_regression_over_threshold_exits_one(self) -> None:
        """Existing-behavior sanity check: real regression still fails."""
        with TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            baseline = tmp_path / "baseline.json"
            criterion = tmp_path / "criterion"
            _write_baseline(
                baseline,
                {"seq_read": {"heddle": {"ns_per_iter": 1000.0}}},
            )
            _write_estimate(criterion, "seq_read", "heddle", 1500.0)  # +50%
            result = _run_compare(criterion, baseline)
            self.assertEqual(result.returncode, 1)
            self.assertIn("REGRESSIONS", result.stdout)

    def test_missing_estimate_file_exits_one(self) -> None:
        """Theme A (P1) — baseline entry with no matching estimates.json
        MUST be a hard failure, not a silent skip."""
        with TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            baseline = tmp_path / "baseline.json"
            criterion = tmp_path / "criterion"
            criterion.mkdir()
            _write_baseline(
                baseline,
                {"seq_read": {"heddle": {"ns_per_iter": 1000.0}}},
            )
            # NOTE: no estimates.json written; criterion dir is empty.
            result = _run_compare(criterion, baseline)
            self.assertNotEqual(
                result.returncode,
                0,
                f"compare must FAIL when expected estimates are missing.\n"
                f"stdout={result.stdout}\nstderr={result.stderr}",
            )

    def test_corrupt_estimate_exits_one(self) -> None:
        """Theme A (P2) — unreadable estimates.json MUST be a hard
        failure, not a silent skip."""
        with TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            baseline = tmp_path / "baseline.json"
            criterion = tmp_path / "criterion"
            _write_baseline(
                baseline,
                {"seq_read": {"heddle": {"ns_per_iter": 1000.0}}},
            )
            _write_corrupt_estimate(criterion, "seq_read", "heddle")
            result = _run_compare(criterion, baseline)
            self.assertNotEqual(
                result.returncode,
                0,
                f"compare must FAIL on corrupt estimates.json.\n"
                f"stdout={result.stdout}\nstderr={result.stderr}",
            )

    def test_unexpected_benchmark_id_exits_one(self) -> None:
        """Theme E (P2) — criterion dir contains a benchmark ID not in
        the baseline. Likely a rename or untracked addition; either way
        the gate must refuse to declare success."""
        with TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            baseline = tmp_path / "baseline.json"
            criterion = tmp_path / "criterion"
            _write_baseline(
                baseline,
                {"seq_read": {"heddle": {"ns_per_iter": 1000.0}}},
            )
            _write_estimate(criterion, "seq_read", "heddle", 1050.0)  # in budget
            _write_estimate(criterion, "seq_read", "experimental", 999.0)  # untracked
            result = _run_compare(criterion, baseline)
            self.assertNotEqual(
                result.returncode,
                0,
                f"compare must FAIL on unexpected benchmark IDs.\n"
                f"stdout={result.stdout}\nstderr={result.stderr}",
            )


if __name__ == "__main__":
    unittest.main()
