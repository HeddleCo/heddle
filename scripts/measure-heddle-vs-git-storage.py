#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Measure on-disk size of a Heddle store against git for the same history.

This is a single-run measurement, not a statistical benchmark. It clones one
exact commit's full reachable history (never a shallow clone), runs git's
default ``git gc`` and a separate ``git gc --aggressive``, imports that same
history with ``heddle import local``. ``maintenance gc``,
``maintenance gc --aggressive``, and ``maintenance repack`` each run on a
separate copy of that post-import tree. Default gc consolidates to one pack,
and a later ``--aggressive`` on that same directory reports zero objects
packed, so sequencing them would hide the aggressive result. A further clone
is imported with ``storage.delta_search.import = true`` (the documented
size-oriented import), then ``maintenance repack`` runs on that tree.

No system or user git config is consulted. Sizes are ``du -sb`` (apparent
bytes) plus allocated ``du -s --block-size=1``. Ratios are left to the report;
this script records the raw numbers and the commands that produced them.

Usage:
    python3 scripts/measure-heddle-vs-git-storage.py \\
        --work-dir /var/tmp/heddle-vs-git-storage \\
        --heddle target/release/heddle \\
        --source-root . \\
        --repo curl=https://github.com/curl/curl.git@<40-hex-sha>
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shlex
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

PRINCIPAL_NAME = "Storage Benchmark"
PRINCIPAL_EMAIL = "storage-benchmark@example.com"


def quote(cmd: list[str]) -> str:
    return " ".join(shlex.quote(part) for part in cmd)


def git_env(empty_config: Path) -> dict[str, str]:
    """Git's compiled-in defaults. Drop inherited config injection and LFS smudge skipping."""
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("GIT_CONFIG_") or key in {
            "GIT_LFS_SKIP_SMUDGE",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        }:
            env.pop(key, None)
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_CONFIG_GLOBAL"] = str(empty_config)
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GCM_INTERACTIVE"] = "never"
    return env


def heddle_env(empty_config: Path) -> dict[str, str]:
    env = git_env(empty_config)
    env["HEDDLE_PRINCIPAL_NAME"] = PRINCIPAL_NAME
    env["HEDDLE_PRINCIPAL_EMAIL"] = PRINCIPAL_EMAIL
    return env


def process_tree_peak_kb(root_pid: int) -> int:
    """Highest VmHWM (kB) among root_pid and its current descendants."""
    parent_of: dict[int, int] = {}
    hwm_of: dict[int, int] = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            text = Path(f"/proc/{entry}/status").read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        pid = int(entry)
        ppid = None
        hwm = 0
        for line in text.splitlines():
            if line.startswith("PPid:"):
                ppid = int(line.split()[1])
            elif line.startswith("VmHWM:"):
                hwm = int(line.split()[1])
        parent_of[pid] = ppid if ppid is not None else -1
        hwm_of[pid] = hwm
    children: dict[int, list[int]] = {}
    for pid, parent in parent_of.items():
        children.setdefault(parent, []).append(pid)
    peak = 0
    stack = [root_pid]
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        peak = max(peak, hwm_of.get(pid, 0))
        stack.extend(children.get(pid, []))
    return peak


def run(
    cmd: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    stdout_path: Path | None = None,
    stderr_path: Path | None = None,
    time_path: Path | None = None,
) -> dict:
    """Run a command. Wall time is perf_counter. Peak RSS is the max VmHWM sampled in the process tree."""
    del time_path  # retained so callers can name a log slot; RSS is sampled from /proc
    stdout_handle = (
        stdout_path.open("w", encoding="utf-8", errors="replace") if stdout_path else subprocess.DEVNULL
    )
    stderr_handle = (
        stderr_path.open("w", encoding="utf-8", errors="replace") if stderr_path else subprocess.DEVNULL
    )
    started = time.perf_counter()
    exit_code = 1
    process = subprocess.Popen(
        cmd,
        cwd=cwd,
        env=env,
        stdout=stdout_handle,
        stderr=stderr_handle,
        text=True,
    )
    peak_kb = 0
    try:
        while True:
            peak_kb = max(peak_kb, process_tree_peak_kb(process.pid))
            try:
                exit_code = process.wait(timeout=0.05)
                break
            except subprocess.TimeoutExpired:
                continue
    finally:
        if stdout_path:
            stdout_handle.close()
        if stderr_path:
            stderr_handle.close()
    elapsed = time.perf_counter() - started
    stderr_text = ""
    if stderr_path and stderr_path.exists():
        stderr_text = stderr_path.read_text(encoding="utf-8", errors="replace")
    return {
        "command": quote(cmd),
        "argv": cmd,
        "exit_code": exit_code,
        "wall_seconds": elapsed,
        "max_rss_kb": peak_kb or None,
        "max_rss_note": "highest VmHWM observed in the process tree, sampled about every 50ms",
        "stderr_tail": stderr_text[-4000:],
    }


def du_apparent(path: Path) -> int | None:
    if not path.exists():
        return None
    completed = subprocess.run(
        ["du", "-sb", str(path)],
        check=True,
        capture_output=True,
        text=True,
    )
    return int(completed.stdout.split()[0])


def du_allocated(path: Path) -> int | None:
    if not path.exists():
        return None
    completed = subprocess.run(
        ["du", "-s", "--block-size=1", str(path)],
        check=True,
        capture_output=True,
        text=True,
    )
    return int(completed.stdout.split()[0])


def directory_breakdown(root: Path, max_depth: int = 2) -> dict[str, int]:
    """Apparent-byte sizes of root and of directories down to max_depth."""
    if not root.exists():
        return {}
    sizes: dict[str, int] = {".": du_apparent(root) or 0}
    for current, dirnames, _filenames in os.walk(root):
        rel = Path(current).relative_to(root)
        depth = 0 if rel == Path(".") else len(rel.parts)
        if depth >= max_depth:
            dirnames.clear()
            continue
        for name in list(dirnames):
            child = Path(current) / name
            key = str(child.relative_to(root))
            size = du_apparent(child)
            if size is not None:
                sizes[key] = size
        if depth + 1 >= max_depth:
            dirnames.clear()
    return sizes


def file_census(root: Path) -> dict:
    if not root.exists():
        return {"files": 0, "dirs": 0, "symlinks": 0}
    files = dirs = links = 0
    for current, dirnames, filenames in os.walk(root):
        dirs += len(dirnames)
        for name in filenames:
            path = Path(current) / name
            if path.is_symlink():
                links += 1
            else:
                files += 1
    return {"files": files, "dirs": dirs, "symlinks": links}


def pack_files(packs: Path) -> list[dict]:
    if not packs.is_dir():
        return []
    rows = []
    for path in sorted(packs.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        header = path.read_bytes()[:16]
        rows.append(
            {
                "rel": str(path.relative_to(packs)),
                "bytes": path.stat().st_size,
                "header_hex": header.hex(),
            }
        )
    return rows


def sqlite_overview(heddle: Path) -> dict:
    found = []
    if not heddle.exists():
        return {"databases": found}
    for path in sorted(heddle.rglob("*")):
        if not path.is_file():
            continue
        if path.suffix not in {".sqlite", ".sqlite3", ".db"} and "sqlite" not in path.name:
            continue
        entry: dict = {
            "rel": str(path.relative_to(heddle)),
            "bytes": path.stat().st_size,
        }
        try:
            uri = f"file:{path}?mode=ro"
            connection = sqlite3.connect(uri, uri=True)
            tables = [
                row[0]
                for row in connection.execute(
                    "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
                )
            ]
            entry["tables"] = {}
            for table in tables:
                if not table.replace("_", "").isalnum():
                    continue
                count = connection.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
                entry["tables"][table] = count
            if "sha_map" in tables:
                grouped = connection.execute(
                    "SELECT kind, COUNT(*) FROM sha_map GROUP BY kind ORDER BY kind"
                ).fetchall()
                entry["sha_map_by_kind"] = {str(kind): count for kind, count in grouped}
            connection.close()
        except sqlite3.Error as error:
            entry["sqlite_error"] = str(error)
        found.append(entry)
    return {"databases": found}


def read_json_stdout(path: Path | None) -> dict | None:
    if path is None or not path.exists():
        return None
    text = path.read_text(encoding="utf-8", errors="replace").strip()
    if not text:
        return None
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        return {"_unparsed": True, "prefix": text[:500]}
    if not isinstance(value, dict):
        return {"_value": value}
    trimmed: dict = {}
    verification = value.get("verification")
    if isinstance(verification, dict):
        trimmed["verification_status"] = verification.get("status")
        trimmed["verification_verified"] = verification.get("verified")
    for key, item in value.items():
        if key == "verification":
            continue
        if isinstance(item, (str, int, float, bool)) or item is None:
            trimmed[key] = item
            continue
        if isinstance(item, list) and len(item) <= 30:
            trimmed[key] = item
            continue
        encoded = json.dumps(item)
        if len(encoded) <= 4000:
            trimmed[key] = item
    return trimmed


def git_capture(repo: Path, env: dict[str, str]) -> dict:
    git_dir = repo / ".git"
    shallow = (git_dir / "shallow").exists()
    head = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    ).stdout.strip()
    is_shallow = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "--is-shallow-repository"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    ).stdout.strip()
    commits = int(
        subprocess.run(
            ["git", "-C", str(repo), "rev-list", "--count", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
            env=env,
        ).stdout.strip()
    )
    merges = int(
        subprocess.run(
            ["git", "-C", str(repo), "rev-list", "--count", "--merges", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
            env=env,
        ).stdout.strip()
    )
    revert_subjects = subprocess.run(
        ["git", "-C", str(repo), "log", "--grep=^Revert", "--pretty=%H"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )
    revert_subject_count = len([line for line in revert_subjects.stdout.splitlines() if line.strip()])
    count_objects = subprocess.run(
        ["git", "-C", str(repo), "count-objects", "-v"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    ).stdout
    parsed_counts = {}
    for line in count_objects.splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        parsed_counts[key.strip()] = value.strip()
    reachable = subprocess.run(
        ["git", "-C", str(repo), "rev-list", "--objects", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )
    reachable_objects = len([line for line in reachable.stdout.splitlines() if line.strip()])
    type_bytes = {"commit": 0, "tree": 0, "blob": 0, "tag": 0}
    type_count = {"commit": 0, "tree": 0, "blob": 0, "tag": 0}
    batch = subprocess.run(
        [
            "git",
            "-C",
            str(repo),
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objecttype) %(objectsize)",
            "--unordered",
        ],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )
    database_objects = 0
    for line in batch.stdout.splitlines():
        parts = line.split()
        if len(parts) != 2:
            continue
        kind, size = parts
        database_objects += 1
        if kind in type_bytes:
            type_count[kind] += 1
            type_bytes[kind] += int(size)
    refs = subprocess.run(
        ["git", "-C", str(repo), "show-ref"],
        capture_output=True,
        text=True,
        env=env,
    ).stdout.splitlines()
    tip_by_ext: dict[str, dict[str, int]] = {}
    tip = subprocess.run(
        ["git", "-C", str(repo), "ls-tree", "-r", "-l", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )
    for line in tip.stdout.splitlines():
        # <mode> <type> <sha> <size>\t<path>
        meta, _, path = line.partition("\t")
        fields = meta.split()
        if len(fields) < 4 or not path:
            continue
        try:
            size = int(fields[3])
        except ValueError:
            continue
        suffix = Path(path).suffix.lower() or "(none)"
        bucket = tip_by_ext.setdefault(suffix, {"files": 0, "bytes": 0})
        bucket["files"] += 1
        bucket["bytes"] += size
    top_ext = sorted(tip_by_ext.items(), key=lambda item: item[1]["bytes"], reverse=True)[:12]
    return {
        "head": head,
        "shallow_file": shallow,
        "is_shallow_repository": is_shallow,
        "commit_count": commits,
        "merge_commit_count": merges,
        "revert_subject_count": revert_subject_count,
        "count_objects_raw": count_objects,
        "count_objects": parsed_counts,
        "reachable_rev_list_objects": reachable_objects,
        "database_objects": database_objects,
        "database_objects_by_type": type_count,
        "uncompressed_payload_bytes_by_type": type_bytes,
        "refs": refs,
        "tip_largest_extensions": [
            {"extension": ext, "files": info["files"], "bytes": info["bytes"]} for ext, info in top_ext
        ],
        "du_git_bytes": du_apparent(git_dir),
        "du_git_allocated_bytes": du_allocated(git_dir),
        "breakdown_bytes": directory_breakdown(git_dir, 2),
    }


def measure_git_gc(clone: Path, aggressive: bool, logs: Path, env: dict[str, str], label: str) -> dict:
    before = git_capture(clone, env)
    cmd = ["git", "-C", str(clone), "gc", "--aggressive"] if aggressive else ["git", "-C", str(clone), "gc"]
    timed = run(
        cmd,
        env=env,
        stdout_path=logs / f"{label}.stdout",
        stderr_path=logs / f"{label}.stderr",
        time_path=logs / f"{label}.time",
    )
    after = git_capture(clone, env) if timed["exit_code"] == 0 else None
    return {"before_gc": before, "gc": timed, "after_gc": after}


def heddle_store_snapshot(repo: Path, label: str) -> dict:
    heddle = repo / ".heddle"
    return {
        "label": label,
        "du_heddle_bytes": du_apparent(heddle),
        "du_heddle_allocated_bytes": du_allocated(heddle),
        "du_git_bytes_alongside": du_apparent(repo / ".git"),
        "breakdown_bytes": directory_breakdown(heddle, 2),
        "census": file_census(heddle),
        "loose_file_counts": {
            "blobs": file_census(heddle / "objects" / "blobs")["files"] if heddle.exists() else 0,
            "trees": file_census(heddle / "objects" / "trees")["files"] if heddle.exists() else 0,
            "states": file_census(heddle / "objects" / "states")["files"] if heddle.exists() else 0,
        },
        "pack_files": pack_files(heddle / "packs"),
        "sqlite": sqlite_overview(heddle),
    }


def enable_import_delta_search(repo: Path) -> str:
    """Set storage.delta_search.import = true without touching the other two switches."""
    config = repo / ".heddle" / "config.toml"
    original = config.read_text(encoding="utf-8") if config.exists() else ""
    lines = original.splitlines(keepends=True)
    in_section = False
    found_section = False
    replaced = False
    rewritten: list[str] = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_section = stripped == "[storage.delta_search]"
            found_section = found_section or in_section
            rewritten.append(line if line.endswith("\n") else line + "\n")
            continue
        if in_section and (stripped.startswith("import ") or stripped.startswith("import=")):
            rewritten.append("import = true\n")
            replaced = True
            continue
        rewritten.append(line if line.endswith("\n") or line == "" else line + "\n")
    if not found_section:
        if rewritten and not rewritten[-1].endswith("\n"):
            rewritten[-1] += "\n"
        rewritten.append("\n[storage.delta_search]\nimport = true\n")
        note = "appended [storage.delta_search] import = true"
    elif not replaced:
        raise RuntimeError(f"{config} has [storage.delta_search] but no import key")
    else:
        note = "set import = true inside the existing [storage.delta_search] section"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text("".join(rewritten), encoding="utf-8")
    return note


def import_heddle(
    heddle_bin: Path,
    repo: Path,
    ref: str,
    logs: Path,
    env: dict[str, str],
    label: str,
    *,
    init_first: bool,
) -> dict:
    steps = []
    if init_first:
        init = run(
            [str(heddle_bin), "--output", "json", "init"],
            cwd=repo,
            env=env,
            stdout_path=logs / f"{label}-init.stdout",
            stderr_path=logs / f"{label}-init.stderr",
            time_path=logs / f"{label}-init.time",
        )
        steps.append({"name": "init", **init, "json": read_json_stdout(logs / f"{label}-init.stdout")})
        if init["exit_code"] != 0:
            return {"steps": steps, "snapshot": heddle_store_snapshot(repo, label)}
        addition = enable_import_delta_search(repo)
        steps.append({"name": "config", "appended": addition, "config_bytes": (repo / ".heddle" / "config.toml").stat().st_size})
    imported = run(
        [str(heddle_bin), "--output", "json", "import", "local", "--ref", ref],
        cwd=repo,
        env=env,
        stdout_path=logs / f"{label}-import.stdout",
        stderr_path=logs / f"{label}-import.stderr",
        time_path=logs / f"{label}-import.time",
    )
    steps.append({"name": "import", **imported, "json": read_json_stdout(logs / f"{label}-import.stdout")})
    snapshot = heddle_store_snapshot(repo, f"{label}-after-import")
    maintenance = []
    # Each maintenance command gets its own copy of the post-import tree.
    # Default gc consolidates to one pack and a later --aggressive run then
    # refuses to rebuild it, so sequencing them on one directory would hide
    # the aggressive result.
    if imported["exit_code"] == 0 and not init_first:
        for name, argv in (
            ("gc", [str(heddle_bin), "--output", "json", "maintenance", "gc"]),
            ("gc-aggressive", [str(heddle_bin), "--output", "json", "maintenance", "gc", "--aggressive"]),
            ("repack", [str(heddle_bin), "--output", "json", "maintenance", "repack"]),
        ):
            copy = repo.parent / f"{repo.name}-{name}"
            print(f"    copy for {name}", file=sys.stderr, flush=True)
            if copy.exists():
                raise RuntimeError(f"refusing to reuse {copy}")
            subprocess.run(["cp", "-a", str(repo), str(copy)], check=True)
            timed = run(
                argv,
                cwd=copy,
                env=env,
                stdout_path=logs / f"{label}-{name}.stdout",
                stderr_path=logs / f"{label}-{name}.stderr",
                time_path=logs / f"{label}-{name}.time",
            )
            maintenance.append(
                {
                    "name": name,
                    "copy": str(copy),
                    **timed,
                    "json": read_json_stdout(logs / f"{label}-{name}.stdout"),
                    "snapshot": heddle_store_snapshot(copy, f"{label}-after-{name}"),
                }
            )
    elif imported["exit_code"] == 0 and init_first:
        # Delta-search import is already a packed state. Repack is the other
        # compact the CLI exposes, measured on this same tree after the
        # post-import snapshot above.
        timed = run(
            [str(heddle_bin), "--output", "json", "maintenance", "repack"],
            cwd=repo,
            env=env,
            stdout_path=logs / f"{label}-repack.stdout",
            stderr_path=logs / f"{label}-repack.stderr",
            time_path=logs / f"{label}-repack.time",
        )
        maintenance.append(
            {
                "name": "repack",
                **timed,
                "json": read_json_stdout(logs / f"{label}-repack.stdout"),
                "snapshot": heddle_store_snapshot(repo, f"{label}-after-repack"),
            }
        )
    return {"steps": steps, "after_import": snapshot, "maintenance": maintenance}


def prepare_mirror(mirror: Path, url: str, revision: str, logs: Path, env: dict[str, str]) -> dict:
    if mirror.exists():
        head = subprocess.run(
            ["git", "--git-dir", str(mirror), "rev-parse", "refs/heads/measured"],
            capture_output=True,
            text=True,
            env=env,
        )
        if head.returncode == 0 and head.stdout.strip() == revision and not (mirror / "shallow").exists():
            return {"reused": True, "command": None, "exit_code": 0, "head": revision}
        raise RuntimeError(f"refusing to reuse {mirror}: it is not an exact full fetch of {revision}")
    mirror.parent.mkdir(parents=True, exist_ok=True)
    init = run(
        ["git", "init", "--bare", "--quiet", str(mirror)],
        env=env,
        stdout_path=logs / "git-init-bare.stdout",
        stderr_path=logs / "git-init-bare.stderr",
    )
    if init["exit_code"] != 0:
        return {"reused": False, "init": init}
    fetch_cmd = [
        "git",
        "--git-dir",
        str(mirror),
        "fetch",
        "--no-tags",
        "--no-recurse-submodules",
        url,
        f"{revision}:refs/heads/measured",
    ]
    fetched = run(
        fetch_cmd,
        env=env,
        stdout_path=logs / "git-fetch.stdout",
        stderr_path=logs / "git-fetch.stderr",
        time_path=logs / "git-fetch.time",
    )
    if fetched["exit_code"] != 0:
        return {"reused": False, "init": init, "fetch": fetched}
    subprocess.run(
        ["git", "--git-dir", str(mirror), "symbolic-ref", "HEAD", "refs/heads/measured"],
        check=True,
        env=env,
    )
    head = subprocess.run(
        ["git", "--git-dir", str(mirror), "rev-parse", "refs/heads/measured"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    ).stdout.strip()
    if head != revision:
        raise RuntimeError(f"fetched {head}, wanted {revision}")
    if (mirror / "shallow").exists():
        raise RuntimeError(f"{mirror} is shallow; refusing to measure it")
    show_ref = subprocess.run(
        ["git", "--git-dir", str(mirror), "show-ref"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    ).stdout.splitlines()
    return {
        "reused": False,
        "init": init,
        "fetch": fetched,
        "head": head,
        "shallow_file": False,
        "show_ref": show_ref,
    }


def clone_from_mirror(mirror: Path, dest: Path, logs: Path, env: dict[str, str], label: str) -> dict:
    if dest.exists():
        raise RuntimeError(f"refusing to reuse existing clone {dest}")
    cmd = ["git", "clone", "--quiet", "--no-local", "--no-tags", str(mirror), str(dest)]
    cloned = run(
        cmd,
        env=env,
        stdout_path=logs / f"{label}-clone.stdout",
        stderr_path=logs / f"{label}-clone.stderr",
        time_path=logs / f"{label}-clone.time",
    )
    if cloned["exit_code"] != 0:
        return cloned
    if (dest / ".git" / "shallow").exists():
        raise RuntimeError(f"{dest} is shallow")
    is_shallow = subprocess.run(
        ["git", "-C", str(dest), "rev-parse", "--is-shallow-repository"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    ).stdout.strip()
    if is_shallow != "false":
        raise RuntimeError(f"{dest} reports shallow={is_shallow}")
    head = subprocess.run(
        ["git", "-C", str(dest), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    ).stdout.strip()
    cloned["head"] = head
    cloned["is_shallow_repository"] = is_shallow
    return cloned


def write_json(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def parse_repo(spec: str) -> tuple[str, str, str]:
    if "=" not in spec or "@" not in spec.split("=", 1)[1]:
        raise argparse.ArgumentTypeError("repo must be name=url@<40-hex-sha>")
    name, rest = spec.split("=", 1)
    url, revision = rest.rsplit("@", 1)
    if not name or "/" in name or name.startswith("."):
        raise argparse.ArgumentTypeError(f"bad repo name {name!r}")
    if len(revision) != 40 or any(char not in "0123456789abcdef" for char in revision.lower()):
        raise argparse.ArgumentTypeError(f"{name}: revision must be a 40-hex commit, got {revision!r}")
    return name, url, revision.lower()


def host_info(heddle_bin: Path, source_root: Path, env: dict[str, str]) -> dict:
    def capture(cmd: list[str]) -> str:
        completed = subprocess.run(cmd, capture_output=True, text=True, env=env)
        return (completed.stdout or completed.stderr).strip()

    source_head = ""
    if (source_root / ".git").exists():
        source_head = capture(["git", "-C", str(source_root), "rev-parse", "HEAD"])
    version = capture([str(heddle_bin), "--version"])
    mem_total = ""
    meminfo = Path("/proc/meminfo")
    if meminfo.exists():
        for line in meminfo.read_text(encoding="utf-8").splitlines():
            if line.startswith("MemTotal:"):
                mem_total = line
                break
    df = subprocess.run(["df", "-T", "-B1", "."], capture_output=True, text=True, check=True)
    return {
        "uname": platform.platform(),
        "python": platform.python_version(),
        "nproc": os.cpu_count(),
        "mem_total": mem_total,
        "df": df.stdout.strip(),
        "git_version": capture(["git", "--version"]),
        "heddle_bin": str(heddle_bin),
        "heddle_version": version,
        "source_root": str(source_root),
        "source_head": source_head,
        "principal_name": PRINCIPAL_NAME,
        "principal_email": PRINCIPAL_EMAIL,
        "git_config": "GIT_CONFIG_NOSYSTEM=1 and GIT_CONFIG_GLOBAL pointing at an empty file",
    }


def measure_repo(name: str, url: str, revision: str, args: argparse.Namespace, base_env: dict[str, str], heddle_environment: dict[str, str]) -> dict:
    root = args.work_dir / name
    logs = root / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    result_path = root / "result.json"
    print(f"== {name} {revision}", file=sys.stderr, flush=True)
    mirror_info = prepare_mirror(root / "mirror.git", url, revision, logs, base_env)
    payload: dict = {
        "name": name,
        "url": url,
        "revision": revision,
        "mirror": mirror_info,
    }
    if mirror_info.get("fetch", {}).get("exit_code", 0) not in (0, None) or mirror_info.get("init", {}).get("exit_code", 0) not in (0, None):
        payload["failed"] = "fetch"
        write_json(result_path, payload)
        return payload
    if "head" not in mirror_info and mirror_info.get("fetch", {}).get("exit_code", 1) != 0:
        payload["failed"] = "fetch"
        write_json(result_path, payload)
        return payload

    print("  git gc (default) clone", file=sys.stderr, flush=True)
    payload["git_default_clone"] = clone_from_mirror(root / "mirror.git", root / "git-default", logs, base_env, "git-default")
    if payload["git_default_clone"].get("head") not in (None, revision):
        raise RuntimeError(f"git-default HEAD {payload['git_default_clone'].get('head')} != {revision}")
    if payload["git_default_clone"]["exit_code"] != 0:
        payload["failed"] = "git-default-clone"
        write_json(result_path, payload)
        return payload
    payload["git_default"] = measure_git_gc(root / "git-default", False, logs, base_env, "git-default-gc")
    write_json(result_path, payload)

    print("  git gc --aggressive clone", file=sys.stderr, flush=True)
    payload["git_aggressive_clone"] = clone_from_mirror(
        root / "mirror.git", root / "git-aggressive", logs, base_env, "git-aggressive"
    )
    if payload["git_aggressive_clone"].get("head") not in (None, revision):
        raise RuntimeError(
            f"git-aggressive HEAD {payload['git_aggressive_clone'].get('head')} != {revision}"
        )
    if payload["git_aggressive_clone"]["exit_code"] != 0:
        payload["failed"] = "git-aggressive-clone"
        write_json(result_path, payload)
        return payload
    payload["git_aggressive"] = measure_git_gc(root / "git-aggressive", True, logs, base_env, "git-aggressive-gc")
    write_json(result_path, payload)

    print("  heddle import local", file=sys.stderr, flush=True)
    payload["heddle_default_clone"] = clone_from_mirror(
        root / "mirror.git", root / "heddle-default", logs, base_env, "heddle-default"
    )
    if payload["heddle_default_clone"].get("head") not in (None, revision):
        raise RuntimeError(
            f"heddle-default HEAD {payload['heddle_default_clone'].get('head')} != {revision}"
        )
    if payload["heddle_default_clone"]["exit_code"] == 0:
        payload["heddle_default"] = import_heddle(
            args.heddle,
            root / "heddle-default",
            "measured",
            logs,
            heddle_environment,
            "heddle-default",
            init_first=False,
        )
    write_json(result_path, payload)

    if not args.skip_delta:
        print("  heddle import with delta_search.import=true", file=sys.stderr, flush=True)
        try:
            payload["heddle_delta_clone"] = clone_from_mirror(
                root / "mirror.git", root / "heddle-delta", logs, base_env, "heddle-delta"
            )
            if payload["heddle_delta_clone"].get("head") not in (None, revision):
                raise RuntimeError(
                    f"heddle-delta HEAD {payload['heddle_delta_clone'].get('head')} != {revision}"
                )
            if payload["heddle_delta_clone"]["exit_code"] == 0:
                payload["heddle_delta"] = import_heddle(
                    args.heddle,
                    root / "heddle-delta",
                    "measured",
                    logs,
                    heddle_environment,
                    "heddle-delta",
                    init_first=True,
                )
        except Exception as error:
            payload["heddle_delta_error"] = str(error)
            print(f"  delta import failed: {error}", file=sys.stderr, flush=True)
    write_json(result_path, payload)
    summarize_line(payload)
    return payload


def bytes_of(snapshot: dict | None) -> int | None:
    if not snapshot:
        return None
    value = snapshot.get("du_heddle_bytes")
    return value if isinstance(value, int) else None


def summarize_line(payload: dict) -> None:
    git_default = (payload.get("git_default") or {}).get("after_gc") or {}
    git_aggressive = (payload.get("git_aggressive") or {}).get("after_gc") or {}
    heddle = payload.get("heddle_default") or {}
    after_import = bytes_of(heddle.get("after_import"))
    last = after_import
    maintenance = heddle.get("maintenance") or []
    phases = {"after_import": after_import}
    for step in maintenance:
        phases[step["name"]] = bytes_of(step.get("snapshot"))
        if phases[step["name"]] is not None:
            last = phases[step["name"]]
    git_bytes = git_default.get("du_git_bytes")
    agg_bytes = git_aggressive.get("du_git_bytes")

    def ratio(num: int | None, den: int | None) -> str:
        if not num or not den:
            return "-"
        return f"{num / den:.3f}"

    print(
        "\t".join(
            [
                payload["name"],
                str(git_bytes),
                str(agg_bytes),
                str(after_import),
                str(phases.get("gc")),
                str(phases.get("gc-aggressive")),
                str(phases.get("repack")),
                "H/git " + ratio(last, git_bytes),
                "git/H " + ratio(git_bytes, last),
            ]
        ),
        file=sys.stderr,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--heddle", type=Path, required=True)
    parser.add_argument("--source-root", type=Path, default=Path("."))
    parser.add_argument("--repo", action="append", required=True, help="name=url@40-hex-sha")
    parser.add_argument("--skip-delta", action="store_true")
    args = parser.parse_args()
    args.heddle = args.heddle.resolve()
    args.source_root = args.source_root.resolve()
    args.work_dir = args.work_dir.resolve()
    if not args.heddle.is_file():
        print(f"heddle binary not found: {args.heddle}", file=sys.stderr)
        return 69
    repos = [parse_repo(spec) for spec in args.repo]
    args.work_dir.mkdir(parents=True, exist_ok=True)
    empty_config = args.work_dir / "empty-gitconfig"
    empty_config.write_text("", encoding="utf-8")
    base = git_env(empty_config)
    heddle_environment = heddle_env(empty_config)
    summary = {
        "host": host_info(args.heddle, args.source_root, base),
        "repos": [],
    }
    write_json(args.work_dir / "summary.json", summary)
    exit_code = 0
    for name, url, revision in repos:
        try:
            payload = measure_repo(name, url, revision, args, base, heddle_environment)
        except Exception as error:
            exit_code = 1
            result_path = args.work_dir / name / "result.json"
            print(f"FAILED {name}: {error}", file=sys.stderr, flush=True)
            existing: dict = {}
            if result_path.exists():
                try:
                    loaded = json.loads(result_path.read_text(encoding="utf-8"))
                    if isinstance(loaded, dict):
                        existing = loaded
                except json.JSONDecodeError:
                    existing = {}
            if existing.get("git_default") or existing.get("heddle_default"):
                existing["failed_late"] = str(error)
                write_json(result_path, existing)
                payload = existing
            else:
                payload = {"name": name, "url": url, "revision": revision, "failed": str(error)}
                write_json(result_path, payload)
        summary["repos"].append({"name": name, "result": str(args.work_dir / name / "result.json"), "failed": payload.get("failed")})
        write_json(args.work_dir / "summary.json", summary)
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
