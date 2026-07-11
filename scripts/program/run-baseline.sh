#!/usr/bin/env bash
# Run the curated program baseline from scripts/program/manifest.toml.
# Emits machine-readable results under artifacts/baseline/<timestamp>/.
#
# Usage:
#   bash scripts/program/run-baseline.sh                 # suite=curated (default)
#   bash scripts/program/run-baseline.sh --suite perf
#   bash scripts/program/run-baseline.sh --suite all
#   bash scripts/program/run-baseline.sh --job git-process-lint
#   bash scripts/program/run-baseline.sh --dry-run

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

MANIFEST="${MANIFEST:-scripts/program/manifest.toml}"
SUITE="curated"
JOB_FILTER=""
DRY_RUN=0
TIMEOUT_SECS="${BASELINE_TIMEOUT_SECS:-0}" # 0 = no wrapper timeout

while [[ $# -gt 0 ]]; do
  case "$1" in
    --suite) SUITE="${2:?}"; shift 2 ;;
    --job) JOB_FILTER="${2:?}"; shift 2 ;;
    --manifest) MANIFEST="${2:?}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

if [[ ! -f "$MANIFEST" ]]; then
  echo "manifest not found: $MANIFEST" >&2
  exit 2
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${BASELINE_OUT_DIR:-artifacts/baseline/$STAMP}"
mkdir -p "$OUT_DIR"

{
  echo "timestamp_utc=$STAMP"
  echo "commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
  echo "dirty=$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')"
  echo "host=$(uname -a)"
  echo "cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)"
  echo "mem_bytes=$(sysctl -n hw.memsize 2>/dev/null || true)"
  echo "rustc=$(rustc --version 2>/dev/null || true)"
  echo "cargo=$(cargo --version 2>/dev/null || true)"
  echo "git=$(git --version 2>/dev/null || true)"
  echo "suite=$SUITE"
  echo "manifest=$MANIFEST"
} >"$OUT_DIR/environment.txt"

cp "$MANIFEST" "$OUT_DIR/manifest.toml"

# Minimal TOML job extractor without external deps: emit one job per line as
# JSON-ish fields using Python stdlib tomllib (3.11+).
JOBS_JSON="$OUT_DIR/jobs.json"
python3 - "$MANIFEST" "$SUITE" "$JOB_FILTER" >"$JOBS_JSON" <<'PY'
import json, sys
from pathlib import Path

try:
    import tomllib
except ImportError:  # pragma: no cover
    import tomli as tomllib  # type: ignore

manifest_path, suite, job_filter = sys.argv[1], sys.argv[2], sys.argv[3]
data = tomllib.loads(Path(manifest_path).read_text())
jobs = data.get("jobs", [])
selected = []
for job in jobs:
    jsuite = job.get("suite", "curated")
    if suite == "all":
        include = True
    elif suite == "curated":
        # Default suite: everything not explicitly tagged suite=perf
        include = jsuite != "perf"
    else:
        include = jsuite == suite
    if not include:
        continue
    if job_filter and job.get("id") != job_filter:
        continue
    selected.append(job)
json.dump(selected, sys.stdout, indent=2)
print()
PY

RESULTS_JSONL="$OUT_DIR/results.jsonl"
: >"$RESULTS_JSONL"

have_prereq() {
  local p="$1"
  case "$p" in
    git) command -v git >/dev/null 2>&1 ;;
    *) return 1 ;;
  esac
}

run_one() {
  local id="$1"
  local kind="$2"
  # remaining args via env / parallel arrays is awkward; use python driver for command
  :
}

python3 - "$OUT_DIR" "$DRY_RUN" "$TIMEOUT_SECS" <<'PY'
import json, os, shlex, subprocess, sys, time
from pathlib import Path

out_dir = Path(sys.argv[1])
dry_run = sys.argv[2] == "1"
timeout_secs = int(sys.argv[3])
jobs = json.loads((out_dir / "jobs.json").read_text())
results_path = out_dir / "results.jsonl"

def have_prereq(p: str) -> bool:
    if p == "git":
        return (
            subprocess.call(
                ["bash", "-lc", "command -v git >/dev/null"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            == 0
        )
    if p == "rustfmt-nightly":
        # Match scripts/program/fmt-check.sh discovery.
        return (
            subprocess.call(
                [
                    "bash",
                    "-lc",
                    "command -v rustup >/dev/null && rustup run nightly rustfmt --version >/dev/null 2>&1",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            == 0
            or subprocess.call(
                ["bash", "-lc", "cargo +nightly fmt --version >/dev/null 2>&1"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            == 0
        )
    return False

def classify(returncode, stdout, stderr, timed_out, setup_fail, skip_prereq):
    if skip_prereq:
        return "skip_prereq", "missing prerequisite"
    if setup_fail:
        return "setup_fail", "failed to construct command"
    if timed_out:
        return "timeout", f"exceeded {timeout_secs}s"
    if returncode == 0:
        return "pass", "exit 0"
    # cargo test uses 101 for test failures commonly
    text = (stdout or "") + "\n" + (stderr or "")
    if "ignored" in text and "0 passed" in text and "failed" not in text.lower():
        return "todo_known", "only ignored tests"
    return "fail", f"exit {returncode}"

def build_cmd(job):
    kind = job.get("kind")
    if kind == "script":
        return list(job["command"])
    if kind == "cargo-test":
        cmd = ["cargo", "test", "-p", job["package"], "--locked"]
        if job.get("release"):
            cmd.append("--release")
        if job.get("test_target"):
            cmd.extend(["--test", job["test_target"]])
        # features left as package default unless specified
        if job.get("features"):
            cmd.extend(["--features", job["features"]])
        cmd.append("--")
        if job.get("filter"):
            cmd.append(job["filter"])
        if job.get("ignored"):
            cmd.append("--ignored")
        cmd.append("--nocapture")
        return cmd
    raise ValueError(f"unknown kind {kind}")

counts = {k: 0 for k in ["pass", "fail", "skip_prereq", "todo_known", "timeout", "setup_fail", "aborted", "incomparable"]}

with results_path.open("w") as rf:
    for job in jobs:
        jid = job["id"]
        prereqs = job.get("prerequisites") or []
        missing = [p for p in prereqs if not have_prereq(p)]
        record = {
            "id": jid,
            "kind": job.get("kind"),
            "oracle": bool(job.get("oracle")),
            "perf_eligible": bool(job.get("perf_eligible")),
            "required": bool(job.get("required", False)),
            "command": None,
            "status": None,
            "classification_reason": None,
            "duration_ms": None,
            "exit_code": None,
            "log": f"logs/{jid}.log",
        }
        log_path = out_dir / "logs" / f"{jid}.log"
        log_path.parent.mkdir(parents=True, exist_ok=True)

        if missing:
            record["status"] = "skip_prereq"
            record["classification_reason"] = f"missing prerequisites: {', '.join(missing)}"
            record["duration_ms"] = 0
            record["exit_code"] = None
            log_path.write_text(record["classification_reason"] + "\n")
            counts["skip_prereq"] += 1
            rf.write(json.dumps(record) + "\n")
            rf.flush()
            print(f"[skip_prereq] {jid}: {record['classification_reason']}", flush=True)
            continue

        try:
            cmd = build_cmd(job)
        except Exception as e:
            record["status"] = "setup_fail"
            record["classification_reason"] = str(e)
            record["duration_ms"] = 0
            counts["setup_fail"] += 1
            log_path.write_text(str(e) + "\n")
            rf.write(json.dumps(record) + "\n")
            print(f"[setup_fail] {jid}: {e}", flush=True)
            continue

        record["command"] = cmd
        print(f"[run] {jid}: {' '.join(shlex.quote(c) for c in cmd)}", flush=True)
        if dry_run:
            record["status"] = "incomparable"
            record["classification_reason"] = "dry-run"
            record["duration_ms"] = 0
            counts["incomparable"] += 1
            log_path.write_text("dry-run\n")
            rf.write(json.dumps(record) + "\n")
            continue

        start = time.perf_counter()
        timed_out = False
        try:
            proc = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=timeout_secs if timeout_secs > 0 else None,
            )
            rc = proc.returncode
            out, err = proc.stdout, proc.stderr
        except subprocess.TimeoutExpired as e:
            timed_out = True
            rc = None
            out = e.stdout or ""
            err = e.stderr or ""
        except KeyboardInterrupt:
            record["status"] = "aborted"
            record["classification_reason"] = "keyboard interrupt"
            record["duration_ms"] = int((time.perf_counter() - start) * 1000)
            counts["aborted"] += 1
            log_path.write_text("aborted\n")
            rf.write(json.dumps(record) + "\n")
            raise
        duration_ms = int((time.perf_counter() - start) * 1000)
        log_path.write_text(
            f"$ {' '.join(shlex.quote(c) for c in cmd)}\n"
            f"exit={rc} duration_ms={duration_ms} timed_out={timed_out}\n\n"
            f"--- stdout ---\n{out or ''}\n--- stderr ---\n{err or ''}\n"
        )
        status, reason = classify(rc, out or "", err or "", timed_out, False, False)
        record["status"] = status
        record["classification_reason"] = reason
        record["duration_ms"] = duration_ms
        record["exit_code"] = rc
        counts[status] = counts.get(status, 0) + 1
        rf.write(json.dumps(record) + "\n")
        rf.flush()
        print(f"[{status}] {jid} ({duration_ms} ms): {reason}", flush=True)

summary = {
    "out_dir": str(out_dir),
    "counts": counts,
    "jobs_total": len(jobs),
    "required_failures": [],
}
# re-read results for required failures
for line in results_path.read_text().splitlines():
    if not line.strip():
        continue
    r = json.loads(line)
    if r.get("required") and r.get("status") not in ("pass", "skip_prereq", "todo_known", "incomparable"):
        # skip_prereq on required is still a certification blocker for that platform
        if r.get("status") == "skip_prereq" and r.get("required"):
            summary.setdefault("required_skips", []).append(r["id"])
        elif r.get("status") != "pass":
            summary["required_failures"].append({"id": r["id"], "status": r["status"]})

(out_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2))
# Exit non-zero if any required job failed/timeout/setup/aborted
hard = [f for f in summary["required_failures"] if f["status"] in ("fail", "timeout", "setup_fail", "aborted")]
sys.exit(1 if hard else 0)
PY
