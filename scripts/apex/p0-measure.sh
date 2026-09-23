#!/usr/bin/env bash
# Apex Package 0 (heddle#1808): fork-only Biscuit measurement spike.
#
# Compares current biscuit-auth 6.0.0 (as each repo configures it today) with a
# minimal-feature biscuit-auth pinned to the upstream merge of
# eclipse-biscuit/biscuit-rust#306 (the `datalog-macro` feature-gating fix),
# across Heddle, Weft and Tapestry. It produces raw JSON; the narrative report
# is docs/security/apex/p0-report.md.
#
# MEASUREMENT ONLY. The git pin is applied to scratch copies (`git archive
# HEAD`) and is never written into a checkout. It is not a proposal to ship a
# git dependency: the owner is waiting for an upstream release (heddle#1792).
# No Apex code is built here.
#
# Usage:
#   p0-measure.sh --all --output FILE           run every step
#   p0-measure.sh --steps a,b --output FILE     run named steps, merging into FILE
#   p0-measure.sh --verify-report FILE          schema + completeness check, then a
#                                               self-test that a report missing
#                                               toolchain/source metadata is rejected
#
# Steps (in --all order): meta prepare deps features negatives builds sizes wasm bundle latency
#
# Environment:
#   APEX_P0_WEFT_DIR      Weft checkout containing scripts/apex-p0-measure.sh (default: ../weft)
#   APEX_P0_TAPESTRY_DIR  Tapestry checkout (default: ../tapestry)
#   APEX_P0_SCRATCH       scratch root for sources and targets (default: /home/scratch/apex-p0)
#   APEX_P0_RUNS          timed runs per measurement after one warm-up (default 10, minimum 10)
#   APEX_P0_PIN           biscuit-rust revision (default a6b72596ebe5f391b60e9b91c74edca8febdda93)
#   APEX_P0_MIN_FREE_GB   abort before a build if / has less free space (default 80)
#   APEX_P0_MAX_TARGET_GB abort before a build if scratch targets exceed this (default 120)
#   APEX_P0_WASM_BINDGEN_DIR  directory holding wasm-bindgen{,-test-runner} 0.2.127
#
# Every CARGO_TARGET_DIR is isolated under $APEX_P0_SCRATCH/targets and deleted
# as soon as its numbers are recorded.
set -euo pipefail

HEDDLE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
export HEDDLE_ROOT

exec python3 - "$@" <<'PY'
import gzip, hashlib, json, os, platform, re, shutil, statistics, subprocess, sys, tempfile, time
import tomllib
from datetime import datetime, timezone
from pathlib import Path

HEDDLE = Path(os.environ["HEDDLE_ROOT"])
WEFT = Path(os.environ.get("APEX_P0_WEFT_DIR", str(HEDDLE.parent / "weft"))).resolve()
TAPESTRY = Path(os.environ.get("APEX_P0_TAPESTRY_DIR", str(HEDDLE.parent / "tapestry"))).resolve()
SCRATCH = Path(os.environ.get("APEX_P0_SCRATCH", "/home/scratch/apex-p0")).resolve()
RUNS = int(os.environ.get("APEX_P0_RUNS", "10"))
PIN = os.environ.get("APEX_P0_PIN", "a6b72596ebe5f391b60e9b91c74edca8febdda93")
PIN_URL = "https://github.com/eclipse-biscuit/biscuit-rust"
MIN_FREE_GB = float(os.environ.get("APEX_P0_MIN_FREE_GB", "80"))
MAX_TARGET_GB = float(os.environ.get("APEX_P0_MAX_TARGET_GB", "120"))
WBG_DIR = os.environ.get("APEX_P0_WASM_BINDGEN_DIR", "")
SCHEMA = HEDDLE / "scripts/apex/p0-report.schema.json"
STEPS = ["meta", "prepare", "deps", "features", "negatives", "builds", "sizes", "wasm", "bundle", "latency"]
SRC = SCRATCH / "src"
TARGETS = SCRATCH / "targets"
MIN_RUNS = 10

# Shipped artifacts, copied from the repos' own release paths.
HEDDLE_SHIPPED = {"package": "heddle-cli", "features": "mount,client", "bin": "heddle", "profile": "release",
                  "source": ".github/workflows/release.yml (Build release binary)"}
CRYPTO_PROTO = re.compile(
    r"^(ed25519|ed25519-dalek|curve25519-dalek|x25519-dalek|signature|sha1|sha2|sha3|digest|block-buffer|"
    r"crypto-common|p256|p384|ecdsa|elliptic-curve|primeorder|rsa|pkcs1|pkcs8|spki|der|sec1|base16ct|"
    r"rfc6979|hmac|hkdf|ff|group|crypto-bigint|rand|rand_core|rand_chacha|getrandom|zeroize|subtle|"
    r"prost|prost-types|prost-derive|protobuf|ring|aes|aes-gcm|chacha20|chacha20poly1305|poly1305|"
    r"cipher|universal-hash|blake2|blake3|pem-rfc7468|base64ct|hybrid-array|typenum|generic-array)$")

def log(msg):
    print(f"[apex-p0 {time.strftime('%H:%M:%S')}] {msg}", file=sys.stderr, flush=True)

def die(msg):
    log("ERROR: " + msg)
    sys.exit(2)

def now_iso():
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()

def env_with(extra=None):
    e = os.environ.copy()
    e["CARGO_TERM_COLOR"] = "never"
    e.pop("RUSTC_WRAPPER", None)
    if extra:
        e.update(extra)
    return e

def run(cmd, cwd, extra_env=None, timeout=None):
    """Run a command; return rc, combined output, wall seconds, child CPU seconds."""
    t0 = time.monotonic()
    p = subprocess.Popen(cmd, cwd=str(cwd), env=env_with(extra_env), text=True,
                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        out, _ = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        p.kill()
        out, _ = p.communicate()
        out = (out or "") + "\n[apex-p0: timeout]"
    wall = time.monotonic() - t0
    # communicate() already reaped the child; per-child rusage is taken below
    # through RUSAGE_CHILDREN deltas by the caller when it matters.
    return p.returncode, out or "", wall

def timed(cmd, cwd, extra_env=None):
    """Wall and CPU (user+sys of the whole child tree) for one command."""
    import resource
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    load = os.getloadavg()
    rc, out, wall = run(cmd, cwd, extra_env)
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
    return rc, out, {"wall_s": round(wall, 3), "cpu_s": round(cpu, 3),
                     "load1_before": round(load[0], 2), "load5_before": round(load[1], 2), "rc": rc}

def cmdstr(cmd, cwd=None, extra_env=None):
    envs = " ".join(f"{k}={v}" for k, v in (extra_env or {}).items())
    where = f"(cd {cwd} && " if cwd else ""
    return f"{where}{envs + ' ' if envs else ''}{' '.join(cmd)}{')' if cwd else ''}"

def pct(values, q):
    v = sorted(values)
    if not v:
        return None
    k = (len(v) - 1) * q
    f, c = int(k), min(int(k) + 1, len(v) - 1)
    return v[f] + (v[c] - v[f]) * (k - f)

def stats(values):
    return {"n": len(values), "min": min(values), "p50": pct(values, 0.5), "p95": pct(values, 0.95),
            "max": max(values), "mean": statistics.fmean(values),
            "stdev": statistics.stdev(values) if len(values) > 1 else 0.0,
            "cv": (statistics.stdev(values) / statistics.fmean(values)) if len(values) > 1 and statistics.fmean(values) else 0.0}

def free_gb():
    st = os.statvfs("/")
    return st.f_bavail * st.f_frsize / 1e9

def dir_bytes(p: Path):
    if not p.exists():
        return 0
    out = subprocess.run(["du", "-sb", str(p)], stdout=subprocess.PIPE, text=True).stdout
    return int(out.split()[0]) if out else 0

def guard_disk(what):
    fg = free_gb()
    tg = dir_bytes(TARGETS) / 1e9
    log(f"disk before {what}: free={fg:.0f} GB, scratch targets={tg:.1f} GB")
    if fg < MIN_FREE_GB:
        die(f"free space {fg:.0f} GB < {MIN_FREE_GB} GB before {what}; stopping (targets kept for inspection: {TARGETS})")
    if tg > MAX_TARGET_GB:
        die(f"scratch targets {tg:.0f} GB > {MAX_TARGET_GB} GB before {what}; stopping")
    return {"free_gb": round(fg, 1), "targets_gb": round(tg, 1)}

def target(name):
    return TARGETS / name

def drop_target(name):
    p = target(name)
    if p.exists():
        shutil.rmtree(p, ignore_errors=True)

def cargo_env(tname, extra=None):
    e = {"CARGO_TARGET_DIR": str(target(tname))}
    if extra:
        e.update(extra)
    return e

def tail(s, n=3000):
    return s[-n:] if s else ""

def unsupported(id_, repo, config, command, reason, error, **extra):
    return {"id": id_, "repo": repo, "config": config, "status": "unsupported", "command": command,
            "reason": reason, "error": tail(error, 4000), **extra}

def measured(id_, repo, config, command, value, **extra):
    return {"id": id_, "repo": repo, "config": config, "status": "measured", "command": command,
            "value": value, **extra}

# ---------------------------------------------------------------- report io

def load_report(path):
    if path and Path(path).exists():
        return json.loads(Path(path).read_text())
    return {"schema_version": "apex-p0-report/1", "issue": "HeddleCo/heddle#1808", "meta": {}, "results": {}}

def save_report(doc, path):
    doc["generated_at"] = now_iso()
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    tmp = Path(str(path) + ".tmp")
    tmp.write_text(json.dumps(doc, indent=2, sort_keys=False) + "\n")
    tmp.replace(path)

def set_results(doc, key, items):
    doc.setdefault("results", {})[key] = items

# ---------------------------------------------------------------- sources

def git_info(root: Path):
    head = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"], stdout=subprocess.PIPE, text=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", str(root), "status", "--porcelain", "--untracked-files=no"],
                           stdout=subprocess.PIPE, text=True).stdout.strip()
    branch = subprocess.run(["git", "-C", str(root), "rev-parse", "--abbrev-ref", "HEAD"],
                            stdout=subprocess.PIPE, text=True).stdout.strip()
    return {"commit": head, "dirty": bool(dirty), "branch": branch, "path": str(root)}

def export_head(root: Path, dest: Path):
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    arch = subprocess.run(["git", "-C", str(root), "archive", "--format=tar", "HEAD"],
                          stdout=subprocess.PIPE, check=True).stdout
    subprocess.run(["tar", "-x", "-C", str(dest)], input=arch, check=True)

def sub1(text, pattern, repl, what):
    new, n = re.subn(pattern, repl, text, count=1, flags=re.M)
    if n != 1:
        die(f"overlay edit did not apply: {what}")
    return new

HEDDLE_VARIANTS = {
    "current": "HEAD unchanged: workspace `biscuit-auth = \"6\"` (defaults regex-full, datalog-macro, pem); capability-verifier adds datalog-macro (+wasm on wasm32).",
    "fork": "biscuit-auth patched to the pin; every Heddle manifest requests default-features=false and no features except `wasm` on wasm32.",
    "fork-nowasm": "fork, but also without `wasm` on wasm32 (negative test for a removed needed feature).",
    "v600-nomacro": "fork manifest edits WITHOUT the pin: biscuit-auth 6.0.0 with macros off (negative test).",
}

def heddle_overlay(variant, dest: Path):
    export_head(HEDDLE, dest)
    edits = []
    if variant == "current":
        return edits
    root = dest / "Cargo.toml"
    t = root.read_text()
    t = sub1(t, r'^biscuit-auth = "6"$', 'biscuit-auth = { version = "6", default-features = false }', "workspace biscuit-auth")
    edits.append('Cargo.toml: workspace biscuit-auth = { version = "6", default-features = false }')
    if variant != "v600-nomacro":
        t = sub1(t, r"^\[patch\.crates-io\]\n",
                 "[patch.crates-io]\n# APEX-P0 MEASUREMENT ONLY (heddle#1808) - never ship a git pin (heddle#1792)\n"
                 f'biscuit-auth = {{ git = "{PIN_URL}", rev = "{PIN}" }}\n', "patch biscuit-auth")
        edits.append(f"Cargo.toml: [patch.crates-io] biscuit-auth -> {PIN_URL}@{PIN}")
    root.write_text(t)
    cv = dest / "crates/capability-verifier/Cargo.toml"
    c = cv.read_text()
    c = sub1(c, r'^biscuit-auth = \{ version = "6\.0", default-features = false, features = \["datalog-macro"\] \}$',
             'biscuit-auth = { version = "6.0", default-features = false }', "capability-verifier native biscuit-auth")
    wasm_feats = '[]' if variant == "fork-nowasm" else '["wasm"]'
    c = sub1(c, r'^biscuit-auth = \{ version = "6\.0", default-features = false, features = \["datalog-macro", "wasm"\] \}$',
             f'biscuit-auth = {{ version = "6.0", default-features = false, features = {wasm_feats} }}',
             "capability-verifier wasm32 biscuit-auth")
    cv.write_text(c)
    edits.append(f"crates/capability-verifier/Cargo.toml: drop datalog-macro (wasm32 features {wasm_feats})")
    if variant == "fork-nowasm":
        bv = dest / "crates/biscuit-verifier/Cargo.toml"
        b = bv.read_text()
        b = sub1(b, r'^biscuit-auth = \{ workspace = true, features = \["wasm"\] \}$',
                 'biscuit-auth = { workspace = true }', "biscuit-verifier wasm32 biscuit-auth")
        bv.write_text(b)
        edits.append("crates/biscuit-verifier/Cargo.toml: wasm32 biscuit-auth without `wasm`")
    return edits

def src_dir(repo, variant):
    return SRC / f"{repo}-{variant}"

def weft_script():
    s = WEFT / "scripts/apex-p0-measure.sh"
    if not s.exists():
        die(f"{s} not found; set APEX_P0_WEFT_DIR to a Weft checkout that has it")
    return s

def weft_describe():
    rc, out, _ = run(["bash", str(weft_script()), "--describe"], WEFT)
    if rc != 0:
        die("weft --describe failed:\n" + out)
    return json.loads(out)

# ---------------------------------------------------------------- steps

def step_meta(doc):
    def cmd_out(cmd, cwd=HEDDLE):
        try:
            r = subprocess.run(cmd, cwd=str(cwd), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env_with())
            return r.stdout.strip() if r.returncode == 0 else None
        except FileNotFoundError:
            return None
    cpu = ""
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if line.startswith("model name"):
            cpu = line.split(":", 1)[1].strip()
            break
    mem = 0
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal:"):
            mem = int(line.split()[1]) * 1024
    osr = {}
    for line in Path("/etc/os-release").read_text().splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            osr[k] = v.strip('"')
    wbg = None
    if WBG_DIR:
        wbg = cmd_out([str(Path(WBG_DIR) / "wasm-bindgen"), "--version"])
    doc["meta"] = {
        "hardware": {"cpu_model": cpu, "logical_cpus": os.cpu_count(),
                     "nproc": int(cmd_out(["nproc"]) or 0), "mem_total_bytes": mem},
        "os": {"pretty_name": osr.get("PRETTY_NAME", ""), "kernel": platform.release(), "arch": platform.machine()},
        "toolchain": {
            "rustc": cmd_out(["rustc", "-V"]), "rustc_verbose": cmd_out(["rustc", "-Vv"]),
            "cargo": cmd_out(["cargo", "-V"]), "node": cmd_out(["node", "-v"]), "bun": cmd_out(["bun", "-v"]),
            "python": platform.python_version(), "wasm_bindgen": wbg,
            "rust_toolchain_file": (HEDDLE / "rust-toolchain.toml").read_text() if (HEDDLE / "rust-toolchain.toml").exists() else None,
        },
        "sources": {"heddle": git_info(HEDDLE), "weft": git_info(WEFT), "tapestry": git_info(TAPESTRY)},
        "pin": {"url": PIN_URL, "rev": PIN, "upstream_pr": "eclipse-biscuit/biscuit-rust#306",
                "baseline_version": "biscuit-auth 6.0.0 (crates.io)", "measurement_only": True,
                "note": "Not a proposal to ship a git dependency; the owner is waiting for an upstream release (heddle#1792)."},
        "method": {"runs": RUNS, "warmup_runs": 1, "interleaved_configs": True,
                   "timing_clock": "time.monotonic wall; CPU = user+sys of the whole child process tree (RUSAGE_CHILDREN delta)",
                   "clean_build": "CARGO_TARGET_DIR deleted before every run; registry sources already extracted in CARGO_HOME",
                   "min_free_gb": MIN_FREE_GB, "max_target_gb": MAX_TARGET_GB},
        "load_at_meta": {"loadavg": os.getloadavg(), "uptime": cmd_out(["uptime"])},
        "script": {"path": "scripts/apex/p0-measure.sh",
                   "sha256": hashlib.sha256((HEDDLE / "scripts/apex/p0-measure.sh").read_bytes()).hexdigest()},
    }
    return doc

def step_prepare(doc):
    SRC.mkdir(parents=True, exist_ok=True)
    configs = {}
    for v in HEDDLE_VARIANTS:
        d = src_dir("heddle", v)
        edits = heddle_overlay(v, d)
        configs[f"heddle/{v}"] = {"description": HEDDLE_VARIANTS[v], "edits": edits, "path": str(d)}
    wd = weft_describe()
    for v in wd["variants"]:
        d = src_dir("weft", v)
        rc, out, _ = run(["bash", str(weft_script()), "--overlay", v, str(d)], WEFT)
        if rc != 0:
            die(f"weft overlay {v} failed:\n{out}")
        info = json.loads(out[out.index("{"):])
        configs[f"weft/{v}"] = {"description": v, "edits": info["edits"], "path": str(d)}
    # Resolve and download everything now (network allowed here only), so every
    # later build is offline and no measurement includes a download.
    fetch_log = {}
    for name, cfg in configs.items():
        rc, out, wall = run(["cargo", "fetch"], cfg["path"])
        fetch_log[name] = {"rc": rc, "wall_s": round(wall, 1), "tail": tail(out, 600)}
        if rc != 0 and not name.endswith("v600-nomacro"):
            die(f"cargo fetch failed for {name}:\n{out[-3000:]}")
        rc, out, _ = run(["cargo", "fetch", "--target", "wasm32-unknown-unknown"], cfg["path"])
    ts = TAPESTRY
    d = src_dir("tapestry", "current")
    export_head(ts, d)
    configs["tapestry/current"] = {"description": "HEAD unchanged; no Rust Biscuit in the bundle (native TS encoder).", "edits": [], "path": str(d)}
    doc["meta"]["configs"] = configs
    doc["meta"]["weft_describe"] = wd
    doc["meta"]["fetch"] = fetch_log
    return doc

def tree_lines(cwd, args, tname):
    rc, out, _ = run(["cargo", "tree", "--offline", *args], cwd, cargo_env(tname))
    return rc, out

def package_set(cwd, args, tname):
    rc, out = tree_lines(cwd, [*args, "--prefix", "none", "-f", "{p}"], tname)
    if rc != 0:
        return None, out
    pk = set()
    for line in out.splitlines():
        line = line.replace(" (*)", "").strip()
        m = re.match(r"^(\S+) v(\S+)(?: \((.*)\))?", line)
        if m:
            src = m.group(3) or "crates.io"
            if src.startswith("/") or "proc-macro" == src:
                src = "path" if src.startswith("/") else "crates.io"
            pk.add((m.group(1), m.group(2), src))
    return pk, out

def edges_from_depth_tree(out):
    edges, stack, nodes = {}, [], set()
    for line in out.splitlines():
        m = re.match(r"^(\d+)(\S+) v(\S+)", line)
        if not m:
            continue
        depth, key = int(m.group(1)), (m.group(2), m.group(3))
        nodes.add(key)
        stack = stack[:depth]
        if stack:
            edges.setdefault(stack[-1], set()).add(key)
        stack.append(key)
    return nodes, edges

def exclusive_to(cwd, args, tname, pkg="biscuit-auth"):
    """Packages in the shipped graph reachable only through `pkg` (removal upper bound)."""
    rc, out = tree_lines(cwd, [*args, "--prefix", "depth", "-f", "{p}"], tname)
    if rc != 0:
        return None
    nodes, edges = edges_from_depth_tree(out)
    roots = [n for n in nodes if not any(n in c for c in edges.values())]
    seen, todo = set(), [r for r in roots if r[0] != pkg]
    while todo:
        n = todo.pop()
        if n in seen or n[0] == pkg:
            continue
        seen.add(n)
        todo.extend(edges.get(n, ()))
    excl = sorted(f"{n} {v}" for (n, v) in nodes - seen)
    return excl

def duplicates(pkgs):
    by = {}
    for n, v, s in pkgs:
        by.setdefault(n, set()).add(v)
    return {n: sorted(vs) for n, vs in sorted(by.items()) if len(vs) > 1}

def biscuit_feats(cwd, args, tname):
    rc, out = tree_lines(cwd, ["-i", "biscuit-auth", "-e", "features", *args, "--prefix", "none", "-f", "{p}|{f}"], tname)
    if rc != 0:
        return None, None, out
    for line in out.splitlines():
        if line.startswith("biscuit-auth v"):
            pkg, _, feats = line.partition("|")
            m = re.match(r"^biscuit-auth v\S+(?: \((.*)\))?", pkg)
            return sorted(f for f in feats.split(",") if f), (m.group(1) if m and m.group(1) else "crates.io"), None
    return [], None, "biscuit-auth not in graph"

def graph_item(repo, config, scope, cwd, args, tname):
    cmd = cmdstr(["cargo", "tree", "--offline", *args, "-e", "normal,build"], cwd)
    pkgs, out = package_set(cwd, [*args, "-e", "normal,build"], tname)
    if pkgs is None:
        return unsupported(f"deps/{repo}/{config}/{scope}", repo, config, cmd, "cargo tree failed", out)
    dups = duplicates(pkgs)
    crypto_dups = {n: v for n, v in dups.items() if CRYPTO_PROTO.match(n)}
    feats, source, ferr = biscuit_feats(cwd, [*args, "-e", "normal,build"], tname)
    excl = exclusive_to(cwd, [*args, "-e", "normal,build"], tname)
    value = {
        "scope": scope, "package_count": len(pkgs),
        "packages": sorted(f"{n} {v}" for n, v, _ in pkgs),
        "duplicate_names": dups, "duplicate_crypto_protobuf": crypto_dups,
        "duplicate_count": len(dups), "duplicate_crypto_protobuf_count": len(crypto_dups),
        "biscuit_auth": {"features": feats, "source": source, "error": ferr},
        "exclusive_to_biscuit_auth": excl, "exclusive_to_biscuit_auth_count": len(excl) if excl is not None else None,
        "proc_macro_error2_present": any(n == "proc-macro-error2" for n, _, _ in pkgs),
    }
    return measured(f"deps/{repo}/{config}/{scope}", repo, config, cmd, value)

def step_deps(doc):
    items = []
    wd = doc["meta"].get("weft_describe") or weft_describe()
    ws = wd["shipped"]
    plan = [
        ("heddle", "current"), ("heddle", "fork"),
        ("weft", "current"), ("weft", "fork-pin"), ("weft", "fork-full"),
    ]
    for repo, cfg in plan:
        cwd = src_dir(repo, cfg)
        tname = f"tree-{repo}-{cfg}"
        scopes = [("workspace-all-targets", ["--workspace", "--target", "all"])]
        if repo == "heddle":
            scopes += [("shipped-heddle-cli", ["-p", HEDDLE_SHIPPED["package"], "--features", HEDDLE_SHIPPED["features"]]),
                       ("capability-verifier-wasm32", ["-p", "heddleco-capability-verifier", "--target", "wasm32-unknown-unknown"])]
        else:
            scopes += [("shipped-weft-server", ["-p", ws["package"], "--features", ws["features"]])]
        for scope, args in scopes:
            log(f"deps {repo}/{cfg} {scope}")
            items.append(graph_item(repo, cfg, scope, cwd, args, tname))
        drop_target(tname)
        # Full (unfiltered, all edge kinds incl. dev) package count, for scale.
        pk, out = package_set(cwd, ["--workspace", "--target", "all", "-e", "all"], tname)
        items.append(measured(f"deps/{repo}/{cfg}/workspace-all-edges", repo, cfg,
                              cmdstr(["cargo", "tree", "--offline", "--workspace", "--target", "all", "-e", "all"], cwd),
                              {"scope": "workspace-all-edges", "package_count": len(pk) if pk else None})
                     if pk is not None else unsupported(f"deps/{repo}/{cfg}/workspace-all-edges", repo, cfg, "cargo tree -e all", "cargo tree failed", out))
    set_results(doc, "dependency_graph", items)
    return doc

def step_features(doc):
    """Exact per-feature needs: compile everything that consumes biscuit-auth without each feature."""
    items = []
    fork = src_dir("heddle", "fork")
    t = "feat-heddle-fork"
    guard_disk("features: heddle fork workspace check")
    cmd = ["cargo", "check", "--offline", "--workspace", "--all-targets"]
    rc, out, wall = run(cmd, fork, cargo_env(t))
    items.append({"id": "features/heddle/fork/workspace-all-targets-check", "repo": "heddle", "config": "fork",
                  "status": "measured", "command": cmdstr(cmd, fork),
                  "value": {"features_requested": [], "rc": rc, "passed": rc == 0, "wall_s": round(wall, 1),
                            "errors": sorted(set(re.findall(r"^error(?:\[E\d+\])?: .*$", out, flags=re.M)))[:20]},
                  "error": tail(out, 3000) if rc else None})
    cmd = ["cargo", "test", "--offline", "-p", "heddle-biscuit-verifier", "-p", "heddleco-capability-verifier"]
    rc, out, wall = run(cmd, fork, cargo_env(t))
    summ = re.findall(r"^test result: .*$", out, flags=re.M)
    items.append({"id": "features/heddle/fork/verifier-tests", "repo": "heddle", "config": "fork", "status": "measured",
                  "command": cmdstr(cmd, fork),
                  "value": {"rc": rc, "passed": rc == 0, "test_results": summ, "wall_s": round(wall, 1),
                            "note": "Runtime check that removing regex-full/pem/datalog-macro changes no verifier behavior covered by these suites."},
                  "error": tail(out, 3000) if rc else None})
    drop_target(t)
    # Weft authz unit suite under the baseline and the minimal-feature fork.
    wd = doc["meta"].get("weft_describe") or weft_describe()
    for cfg in [wd["baseline_variant"], wd["fork_variant"]]:
        cwd = src_dir("weft", cfg)
        t = f"feat-weft-{cfg}"
        guard_disk(f"features: weft-authz tests {cfg}")
        cmd = ["cargo", "test", "--offline", "-p", "weft-authz", "--lib", "--tests"]
        rc, out, wall = run(cmd, cwd, cargo_env(t))
        summ = re.findall(r"^test result: .*$", out, flags=re.M)
        failed = sorted(set(re.findall(r"^test (\S+) \.\.\. FAILED$", out, flags=re.M)))
        items.append({"id": "features/weft/weft-authz-tests", "repo": "weft", "config": cfg, "status": "measured",
                      "command": cmdstr(cmd, cwd),
                      "value": {"rc": rc, "passed": rc == 0, "test_results": summ, "failed_tests": failed, "wall_s": round(wall, 1)},
                      "error": tail(out, 3000) if rc else None})
        drop_target(t)
    # Static evidence for each biscuit feature.
    def grep(root, pattern, globs=("*.rs",)):
        r = subprocess.run(["git", "-C", str(root), "grep", "-nE", pattern, "--", *globs], stdout=subprocess.PIPE, text=True)
        return [l for l in r.stdout.splitlines() if l]
    evidence = {
        "datalog-macro": {"heddle": grep(HEDDLE, r"biscuit_auth::(macros|\{[^}]*macros)|\b(biscuit|block|authorizer|fact|rule|check|policy)!\("),
                          "weft": grep(WEFT, r"biscuit_auth::(macros|\{[^}]*macros)")},
        "regex-full": {"heddle": grep(HEDDLE, r"\.matches\(\$|\.matches\(\\\"", ("*.rs", "*.biscuit")),
                       "weft": grep(WEFT, r"\.matches\(\$|\.matches\(\\\"", ("*.rs",))},
        "pem": {"heddle": grep(HEDDLE, r"(PrivateKey|KeyPair|PublicKey)::from_(pem|der)|to_(pem|der)\(\).*biscuit|biscuit_auth::.*pem"),
                "weft": grep(WEFT, r"(PrivateKey|KeyPair|PublicKey)::from_(pem|der)|biscuit_auth::.*pem")},
        "wasm": {"heddle": grep(HEDDLE, r'features = \[[^]]*"wasm"', ("*Cargo.toml",)),
                 "weft": grep(WEFT, r'biscuit-auth.*"wasm"', ("*Cargo.toml",))},
    }
    items.append({"id": "features/static-usage", "repo": "heddle+weft", "config": "current", "status": "measured",
                  "command": "git grep (patterns in script step_features)", "value": evidence})
    set_results(doc, "features", items)
    return doc

def step_negatives(doc):
    items = []
    # N1: 6.0.0 with macros off fails on the known unconditional ToAnyParam import.
    for repo, cfg, pkgs in [("heddle", "v600-nomacro", ["-p", "heddle-biscuit-verifier"])]:
        cwd = src_dir(repo, cfg)
        t = f"neg-{repo}-{cfg}"
        cmd = ["cargo", "check", *pkgs]
        rc, out, _ = run(cmd, cwd, cargo_env(t))
        errs = sorted(set(re.findall(r"^error(?:\[E\d+\])?: .*$", out, flags=re.M)))
        ok = rc != 0 and "ToAnyParam" in out
        items.append({"id": f"negative/{repo}/biscuit-6.0.0-no-macro-fails", "repo": repo, "config": cfg,
                      "status": "measured", "command": cmdstr(cmd, cwd), "expect": "fail",
                      "value": {"rc": rc, "demonstrated": ok, "errors": errs[:10]}, "error": tail(out, 3000)})
        drop_target(t)
    # N2: the pin builds with macros off (heddle fork graph; wasm32 too).
    cwd = src_dir("heddle", "fork")
    t = "neg-heddle-fork"
    for label, cmd in [("native", ["cargo", "check", "--offline", "-p", "heddle-biscuit-verifier", "-p", "heddleco-capability-verifier"]),
                       ("wasm32", ["cargo", "check", "--offline", "-p", "heddleco-capability-verifier", "-p", "heddle-biscuit-verifier",
                                   "--target", "wasm32-unknown-unknown", "--lib"])]:
        rc, out, _ = run(cmd, cwd, cargo_env(t))
        feats, src, _ = biscuit_feats(cwd, ["-p", "heddleco-capability-verifier"] + (["--target", "wasm32-unknown-unknown"] if label == "wasm32" else []), t)
        items.append({"id": f"negative/heddle/pin-no-macro-builds-{label}", "repo": "heddle", "config": "fork",
                      "status": "measured", "command": cmdstr(cmd, cwd), "expect": "pass",
                      "value": {"rc": rc, "demonstrated": rc == 0, "biscuit_features": feats, "biscuit_source": src},
                      "error": tail(out, 3000) if rc else None})
    drop_target(t)
    # N3: removing a needed feature fails: `wasm` on wasm32 (biscuit's time source).
    runner = Path(WBG_DIR) / "wasm-bindgen-test-runner" if WBG_DIR else None
    for cfg, expect in [("fork", "pass"), ("fork-nowasm", "fail")]:
        cwd = src_dir("heddle", cfg)
        t = f"neg-wasm-{cfg}"
        cmd = ["cargo", "test", "--offline", "-p", "heddleco-capability-verifier", "--target", "wasm32-unknown-unknown", "--lib"]
        idn = "negative/heddle/wasm-feature-kept-passes" if cfg == "fork" else "negative/heddle/wasm-feature-removed-fails"
        if not runner or not runner.exists():
            items.append(unsupported(idn, "heddle", cfg, cmdstr(cmd, cwd), "wasm-bindgen-test-runner 0.2.127 not available",
                                     "set APEX_P0_WASM_BINDGEN_DIR", expect=expect))
            continue
        env = cargo_env(t, {"CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER": str(runner), "PATH": f"{WBG_DIR}:{os.environ['PATH']}"})
        guard_disk(f"wasm test {cfg}")
        rc, out, _ = run(cmd, cwd, env, timeout=3600)
        summ = re.findall(r"^test result: .*$", out, flags=re.M)
        panics = sorted(set(re.findall(r"panicked at [^\n]*\n[^\n]*", out)))[:5]
        errs = sorted(set(re.findall(r"^error(?:\[E\d+\])?: .*$", out, flags=re.M)))[:10]
        # A removed feature must fail *because of biscuit*, not for an unrelated reason.
        demonstrated = (rc == 0) if expect == "pass" else (rc != 0 and ("could not compile `biscuit-auth`" in out or bool(panics)))
        items.append({"id": idn, "repo": "heddle", "config": cfg, "status": "measured", "command": cmdstr(cmd, cwd, {"CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER": "wasm-bindgen-test-runner"}),
                      "expect": expect, "value": {"rc": rc, "demonstrated": demonstrated, "test_results": summ, "panics": panics, "errors": errs},
                      "error": tail(out, 3000) if rc else None})
        drop_target(t)
    # N4 (Weft): delegated to Weft's own script so its checks live with its manifests.
    guard_disk("weft --verify")
    wj = SCRATCH / "weft-verify.json"
    cmd = ["bash", str(weft_script()), "--verify", "--json", str(wj)]
    rc, out, _ = run(cmd, WEFT, {"APEX_P0_SCRATCH": str(SCRATCH), "APEX_P0_PIN": PIN})
    wv = json.loads(wj.read_text()) if wj.exists() else None
    items.append({"id": "negative/weft/verify", "repo": "weft", "config": "all", "status": "measured" if wv else "unsupported",
                  "command": cmdstr(["bash", "scripts/apex-p0-measure.sh", "--verify"], WEFT), "expect": "pass",
                  "value": {"rc": rc, "demonstrated": rc == 0, "checks": wv["checks"] if wv else None},
                  **({} if wv else {"reason": "weft --verify produced no JSON", "error": tail(out)})})
    # N5: a report missing toolchain/source metadata must fail schema validation.
    items.append({"id": "negative/report-missing-metadata-rejected", "repo": "heddle", "config": "n/a", "status": "measured",
                  "command": "p0-measure.sh --verify-report FILE (self-test)", "expect": "fail",
                  "value": {"demonstrated": schema_self_test(doc_for_selftest(doc))}})
    set_results(doc, "negative_tests", items)
    return doc

def interleaved(label, configs, prepare, command, cwd_of, env_of, before_each=None):
    """Run warm-up + RUNS timed samples, alternating configs every round."""
    samples = {c: [] for c in configs}
    warm = {}
    for r in range(RUNS + 1):
        for c in (configs if r % 2 == 0 else list(reversed(configs))):
            if before_each:
                before_each(c)
            rc, out, s = timed(command(c), cwd_of(c), env_of(c))
            if rc != 0:
                return None, {"config": c, "round": r, "output": tail(out, 4000)}
            s["round"] = r
            if r == 0:
                warm[c] = s
            else:
                samples[c].append(s)
            log(f"{label} {c} round {r}: wall {s['wall_s']:.1f}s cpu {s['cpu_s']:.1f}s load1 {s['load1_before']}")
    res = {}
    for c in configs:
        w = [s["wall_s"] for s in samples[c]]
        u = [s["cpu_s"] for s in samples[c]]
        res[c] = {"unit": "s", "warmup": warm[c], "samples": samples[c], "wall": stats(w), "cpu": stats(u),
                  "load1": stats([s["load1_before"] for s in samples[c]])}
    return res, None

def build_measure(items, id_, repo, configs, cmd, touch=None, prime=None):
    tnames = {c: f"build-{repo}-{c}-{id_.split('/')[-1]}" for c in configs}
    cwd_of = lambda c: src_dir(repo, c)
    env_of = lambda c: cargo_env(tnames[c])
    command = lambda c: cmd
    if touch is None:
        def before(c):
            guard_disk(f"{id_} {c} clean build")
            drop_target(tnames[c])
    else:
        for c in configs:
            guard_disk(f"{id_} {c} prime")
            rc, out, _ = run(prime or cmd, cwd_of(c), env_of(c))
            if rc != 0:
                for cc in configs:
                    items.append(unsupported(f"{id_}", repo, cc, cmdstr(cmd, cwd_of(cc)), "priming build failed", out))
                    drop_target(tnames[cc])
                return
        def before(c):
            p = cwd_of(c) / touch
            p.write_text(p.read_text() + "\n")
    res, err = interleaved(id_, configs, None, command, cwd_of, env_of, before)
    for c in configs:
        if res is None:
            items.append(unsupported(id_, repo, c, cmdstr(cmd, cwd_of(c), {"CARGO_TARGET_DIR": "<isolated>"}),
                                     f"build failed in config {err['config']} round {err['round']}", err["output"]))
        else:
            items.append(measured(id_, repo, c, cmdstr(cmd, cwd_of(c), {"CARGO_TARGET_DIR": "<isolated>"}), res[c],
                                  kind="incremental" if touch else "clean", touched=touch))
    for c in configs:
        drop_target(tnames[c])

def step_builds(doc):
    items = []
    set_results(doc, "builds", items)  # saved even if a disk guard aborts mid-step
    wd = doc["meta"].get("weft_describe") or weft_describe()
    H = ["current", "fork"]
    W = [wd["baseline_variant"], wd["fork_variant"]]
    build_measure(items, "builds/biscuit-auth-only/clean", "heddle", H,
                  ["cargo", "build", "--offline", "-p", "biscuit-auth"])
    build_measure(items, "builds/heddle-verifier-leaves/clean", "heddle", H,
                  ["cargo", "build", "--offline", "-p", "heddle-biscuit-verifier", "-p", "heddleco-capability-verifier"])
    build_measure(items, "builds/weft-authz/clean", "weft", W,
                  ["cargo", "build", "--offline", "-p", wd["leaf_packages"][0]])
    hcmd = ["cargo", "build", "--offline", "-p", HEDDLE_SHIPPED["package"], "--features", HEDDLE_SHIPPED["features"], "--bin", HEDDLE_SHIPPED["bin"]]
    build_measure(items, "builds/heddle-cli/incremental-touch-biscuit-verifier", "heddle", H, hcmd,
                  touch="crates/biscuit-verifier/src/lib.rs")
    inc = wd["incremental"]
    wcmd = ["cargo", "build", "--offline", "-p", inc["package"], "--features", inc["features"], "--bin", inc["bin"]]
    build_measure(items, "builds/weft-server/incremental-touch-weft-authz", "weft", W, wcmd, touch=inc["touch"])
    set_results(doc, "builds", items)
    return doc

def file_sizes(p: Path):
    b = p.read_bytes()
    zst = subprocess.run(["zstd", "-19", "-q", "-c"], input=b, stdout=subprocess.PIPE).stdout
    br = subprocess.run(["node", "-e", "const z=require('zlib');let c=[];process.stdin.on('data',d=>c.push(d));process.stdin.on('end',()=>process.stdout.write(String(z.brotliCompressSync(Buffer.concat(c),{params:{[z.constants.BROTLI_PARAM_QUALITY]:11}}).length)))"],
                        input=b, stdout=subprocess.PIPE).stdout
    return {"bytes": len(b), "gzip9_bytes": len(gzip.compress(b, 9)), "zstd19_bytes": len(zst),
            "brotli11_bytes": int(br) if br else None, "sha256": hashlib.sha256(b).hexdigest()}

def one_build_size(items, id_, repo, cfg, cmd, artifact_rel, extra_env=None, keep=False):
    cwd = src_dir(repo, cfg)
    t = f"size-{repo}-{cfg}-{id_.split('/')[-1]}"
    guard_disk(f"{id_} {cfg}")
    rc, out, s = timed(cmd, cwd, cargo_env(t, extra_env))
    art = target(t) / artifact_rel
    if rc != 0 or not art.exists():
        items.append(unsupported(id_, repo, cfg, cmdstr(cmd, cwd), "build failed or artifact missing", out, artifact=artifact_rel))
    else:
        items.append(measured(id_, repo, cfg, cmdstr(cmd, cwd), {**file_sizes(art), "artifact": artifact_rel,
                              "build_single_sample": s, "target_dir_bytes": dir_bytes(target(t))},
                              note="size is deterministic for a given toolchain/lock; build time here is ONE sample (informational only)"))
    if not keep:
        drop_target(t)
    return t

def step_sizes(doc):
    items = []
    set_results(doc, "binary_sizes", items)  # saved even if a disk guard aborts mid-step
    wd = doc["meta"].get("weft_describe") or weft_describe()
    for cfg in ["current", "fork"]:
        one_build_size(items, "sizes/heddle-cli-release", "heddle", cfg,
                       ["cargo", "build", "--offline", "--release", "-p", "heddle-cli", "--features", HEDDLE_SHIPPED["features"], "--bin", "heddle"],
                       "release/heddle")
        one_build_size(items, "sizes/capability-verifier-cdylib-release", "heddle", cfg,
                       ["cargo", "build", "--offline", "--release", "-p", "heddleco-capability-verifier", "--lib"],
                       "release/libheddleco_capability_verifier.so")
    ws = wd["shipped"]
    for cfg in [wd["baseline_variant"], wd["fork_variant"]]:
        one_build_size(items, "sizes/weft-server-production", "weft", cfg,
                       ["cargo", "build", "--offline", "--profile", ws["profile"], "-p", ws["package"], "--features", ws["features"], "--bin", ws["bin"]],
                       f"{ws['profile']}/{ws['bin']}")
    set_results(doc, "binary_sizes", items)
    return doc

def step_wasm(doc):
    items = []
    set_results(doc, "wasm", items)  # saved even if a disk guard aborts mid-step
    wd = doc["meta"].get("weft_describe") or weft_describe()
    wbg = Path(WBG_DIR) / "wasm-bindgen" if WBG_DIR else None
    for cfg in ["current", "fork"]:
        t = one_build_size(items, "wasm/capability-verifier-release-raw", "heddle", cfg,
                           ["cargo", "build", "--offline", "--release", "-p", "heddleco-capability-verifier", "--target", "wasm32-unknown-unknown", "--lib"],
                           "wasm32-unknown-unknown/release/heddleco_capability_verifier.wasm", keep=True)
        raw = target(t) / "wasm32-unknown-unknown/release/heddleco_capability_verifier.wasm"
        idn = "wasm/capability-verifier-wasm-bindgen-web"
        if wbg and wbg.exists() and raw.exists():
            outd = SCRATCH / f"wbg-{cfg}"
            shutil.rmtree(outd, ignore_errors=True)
            cmd = [str(wbg), str(raw), "--out-dir", str(outd), "--target", "web", "--out-name", "capability_verifier"]
            rc, out, _ = run(cmd, SCRATCH)
            art = outd / "capability_verifier_bg.wasm"
            js = outd / "capability_verifier.js"
            if rc == 0 and art.exists():
                items.append(measured(idn, "heddle", cfg, " ".join(cmd), {**file_sizes(art), "js_glue": file_sizes(js),
                                      "note": "wasm-bindgen 0.2.127 --target web output, no wasm-opt (not installed); npm package additionally runs wasm-opt -O"}))
            else:
                items.append(unsupported(idn, "heddle", cfg, " ".join(cmd), "wasm-bindgen failed", out))
            shutil.rmtree(outd, ignore_errors=True)
        else:
            items.append(unsupported(idn, "heddle", cfg, "wasm-bindgen --target web", "wasm-bindgen 0.2.127 not available or raw wasm missing", ""))
        drop_target(t)
    # Weft Worker WASM (heddle-iroh-object-provider) via its own build script.
    for cfg in [wd["baseline_variant"], wd["fork_variant"]]:
        cwd = src_dir("weft", cfg) / "crates/heddle-iroh-object-provider"
        t = f"size-weft-{cfg}-worker-wasm"
        idn = "wasm/weft-worker-heddle-iroh-object-provider"
        cmd = ["bash", "scripts/build-wasm.sh"]
        if not (wbg and wbg.exists()):
            items.append(unsupported(idn, "weft", cfg, cmdstr(cmd, cwd), "wasm-bindgen 0.2.127 not available", ""))
            continue
        guard_disk(f"{idn} {cfg}")
        env = cargo_env(t, {"PATH": f"{WBG_DIR}:{os.environ['PATH']}"})
        rc, out, s = timed(cmd, cwd, env)
        art = cwd / "worker/pkg/heddle_iroh_object_provider_bg.wasm"
        raw = target(t) / "wasm32-unknown-unknown/release/heddle_iroh_object_provider.wasm"
        if rc == 0 and art.exists():
            items.append(measured(idn, "weft", cfg, cmdstr(cmd, cwd), {**file_sizes(art), "raw_cargo_wasm": file_sizes(raw) if raw.exists() else None,
                                  "build_single_sample": s, "artifact": "worker/pkg/heddle_iroh_object_provider_bg.wasm"}))
        else:
            items.append(unsupported(idn, "weft", cfg, cmdstr(cmd, cwd), "Worker wasm build failed", out))
        drop_target(t)
    set_results(doc, "wasm", items)
    return doc

def step_bundle(doc):
    items = []
    d = src_dir("tapestry", "current")
    if not d.exists():
        export_head(TAPESTRY, d)
    rc, out, _ = run(["bun", "install", "--frozen-lockfile"], d)
    if rc != 0:
        items.append(unsupported("bundle/tapestry-production", "tapestry", "current", cmdstr(["bun", "install", "--frozen-lockfile"], d), "bun install failed", out))
        set_results(doc, "bundle", items)
        return doc
    cmd = ["bun", "run", "build"]
    rc, out, s = timed(cmd, d)
    client = d / ".svelte-kit/output/client"
    if rc != 0 or not client.exists():
        items.append(unsupported("bundle/tapestry-production", "tapestry", "current", cmdstr(cmd, d), "tapestry build failed", out))
        set_results(doc, "bundle", items)
        return doc
    def collect(root, exts):
        tot = {"files": 0, "bytes": 0, "gzip9_bytes": 0, "brotli11_bytes": 0}
        files = []
        for p in sorted(root.rglob("*")):
            if p.is_file() and p.suffix in exts:
                fs = file_sizes(p)
                files.append((p, fs))
                tot["files"] += 1
                for k in ("bytes", "gzip9_bytes", "brotli11_bytes"):
                    tot[k] += fs[k] or 0
        return tot, files
    js_tot, js_files = collect(client, {".js"})
    all_tot, _ = collect(client, {".js", ".css", ".wasm"})
    markers = {"src/lib/client/biscuit.ts": "302e020100300506032b657004220420",
               "src/lib/owner-authorization/subject-biscuit.ts": "owner_capability"}
    chunks = {}
    for src_file, marker in markers.items():
        hits = []
        for p, fs in js_files:
            if marker in p.read_text(errors="ignore"):
                hits.append({"chunk": str(p.relative_to(client)), **{k: fs[k] for k in ("bytes", "gzip9_bytes", "brotli11_bytes")}})
        chunks[src_file] = {"marker": marker, "source_bytes": (d / src_file).stat().st_size, "chunks_containing": hits}
    wasm_files = [{"file": str(p.relative_to(client)), "bytes": p.stat().st_size} for p in client.rglob("*.wasm")]
    items.append(measured("bundle/tapestry-production", "tapestry", "current", cmdstr(cmd, d),
                          {"client_js": js_tot, "client_js_css_wasm": all_tot, "biscuit_modules": chunks,
                           "client_wasm_files": wasm_files, "build_single_sample": s,
                           "rust_biscuit_in_bundle": False,
                           "fork_delta_bytes": 0,
                           "fork_delta_reason": "Tapestry ships a native TS Biscuit encoder and bundles no Rust Biscuit/capability-verifier wasm, so the fork pin cannot change this bundle; measured once."}))
    shutil.rmtree(d / "node_modules", ignore_errors=True)
    shutil.rmtree(d / ".svelte-kit", ignore_errors=True)
    set_results(doc, "bundle", items)
    return doc

BENCH_RS = r'''
//! Apex P0 latency bench (heddle#1808). Measurement only; mirrors the shapes the
//! applications emit: weft-authz `mint_at` authority facts, the offline agent
//! delegation (AgentAttenuation + signed pop_delegation), and the shared
//! heddle-biscuit-verifier `verify_at_with_resource` path used by hosts.
use std::time::Instant;

use biscuit_auth::{Algorithm, Biscuit, KeyPair, PrivateKey};
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signer as _, SigningKey};
use heddle_biscuit_verifier as hv;

const MAX_TOKEN_BYTES: usize = 64 * 1024; // heddle-thread-api credentials / weft identity RPC bound

fn keypair(seed: u8) -> KeyPair {
    KeyPair::from(&PrivateKey::from_bytes(&[seed; 32], Algorithm::Ed25519).expect("fixture key"))
}

fn q(s: &str) -> String { format!("{s:?}") }

fn mint(root: &KeyPair, pop: &SigningKey, now: DateTime<Utc>, rights: usize) -> String {
    let exp = now + Duration::hours(1);
    let mut b = Biscuit::builder();
    let uuid = "0190f5d2-6b1e-7c1a-9f00-00000000c0de";
    for f in [
        format!("user({})", q("alice")), format!("session({})", q("sess-0190f5d2")),
        format!("issued_at({})", now.to_rfc3339()), format!("expires_at({})", exp.to_rfc3339()),
        format!("subject_kind({})", q("user")), format!("subject_user_uuid({})", q(uuid)),
        format!("amr({})", q("passkey")),
    ] { b = b.fact(f.as_str()).expect("fact"); }
    for i in 0..rights {
        let action = ["admin", "write", "read"][i % 3];
        b = b.fact(format!("right({}, {}, {})", q("spool"), q(&format!("org/acme/p{i}")), q(action)).as_str()).expect("right");
    }
    b = b.fact(format!("right({}, {}, {})", q("spool"), q("org/acme"), q("admin")).as_str()).expect("right");
    for f in [format!("device({})", q("dev-01")), format!("credential_id({})", q("cred-01")),
              format!("device_pop_key({})", q(&hex::encode(pop.verifying_key().as_bytes()))),
              "request_signed_session(true)".to_string(), "root_established(true)".to_string()] {
        b = b.fact(f.as_str()).expect("fact");
    }
    b = b.check(format!("check if time($now), $now < {}", exp.to_rfc3339()).as_str()).expect("check");
    b.build(root).expect("build").to_base64().expect("b64")
}

fn attenuate(parent: &str, parent_signer: &SigningKey, child: &SigningKey, now: DateTime<Utc>, ops: usize, res: usize, i: usize) -> String {
    let child_pub = child.verifying_key().to_bytes();
    let stmt = hv::key_delegation::statement(parent, &child_pub).expect("statement");
    let sig = parent_signer.sign(&stmt).to_bytes();
    let mut allowed_ops = vec!["ReadContent".to_string(), "ObserveThread".to_string()];
    for k in 0..ops { allowed_ops.push(format!("Op{k:04}")); }
    let mut allowed_res = vec![("spool".to_string(), "org/acme".to_string())];
    for k in 0..res { allowed_res.push(("spool".to_string(), format!("org/acme/r{k:04}"))); }
    let block = hv::delegation::AgentAttenuation {
        agent_id: format!("agent-{i}"), expires_at: now + Duration::minutes(50),
        allowed_operations: Some(allowed_ops), allowed_resources: Some(allowed_res),
    }.block().expect("block");
    hv::key_delegation::append(parent, &child_pub, &sig, block).expect("append")
}

fn verify(tok: &str, root: &KeyPair, now: DateTime<Utc>) -> bool {
    hv::verify_at_with_resource(tok, &[root.public()], &[], "ReadContent", Some(("spool", "org/acme/project")), now).is_ok()
}

fn chain(root: &KeyPair, now: DateTime<Utc>, hops: usize, ops: usize, res: usize, rights: usize) -> String {
    let mut signer = SigningKey::from_bytes(&[7; 32]);
    let mut tok = mint(root, &signer, now, rights);
    for i in 0..hops {
        let child = SigningKey::from_bytes(&[(i % 250) as u8 + 3; 32]);
        tok = attenuate(&tok, &signer, &child, now, ops, res, i);
        signer = child;
    }
    tok
}

fn percentile(v: &mut Vec<u64>, q: f64) -> u64 {
    v.sort_unstable();
    let k = ((v.len() - 1) as f64 * q).round() as usize;
    v[k]
}

fn measure<F: FnMut() -> bool>(name: &str, iters: usize, bytes: usize, mut f: F) {
    for _ in 0..(iters / 10).max(3) { assert!(f(), "{name} warm-up rejected"); }
    let mut s = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let ok = f();
        s.push(t.elapsed().as_nanos() as u64);
        assert!(ok, "{name} rejected during measurement");
    }
    let mean = s.iter().sum::<u64>() / s.len() as u64;
    let mut c = s.clone();
    let (p50, p95, p99) = (percentile(&mut c, 0.50), percentile(&mut c, 0.95), percentile(&mut c, 0.99));
    let max = *c.last().unwrap_or(&0);
    println!("{{\"op\":\"{name}\",\"n\":{},\"p50_ns\":{p50},\"p95_ns\":{p95},\"p99_ns\":{p99},\"max_ns\":{max},\"mean_ns\":{mean},\"token_bytes\":{bytes},\"samples_ns\":{:?}}}", s.len(), s);
}

fn main() {
    let iters: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200);
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("t");
    let root = keypair(11);
    let pop = SigningKey::from_bytes(&[7; 32]);
    let root_tok = mint(&root, &pop, now, 3);
    assert!(verify(&root_tok, &root, now), "root token must verify");
    let one = chain(&root, now, 1, 0, 0, 3);
    assert!(verify(&one, &root, now), "one-hop token must verify");

    // Max hops: grow ordinary agent hops until the host size bound or the verifier rejects.
    let mut max_hops = 1usize;
    let mut tok = one.clone();
    let mut signer = SigningKey::from_bytes(&[3; 32]);
    let stop = loop {
        let child = SigningKey::from_bytes(&[((max_hops + 1) % 250) as u8 + 3; 32]);
        let next = attenuate(&tok, &signer, &child, now, 0, 0, max_hops + 1);
        if next.len() > MAX_TOKEN_BYTES { break format!("next hop would be {} base64 bytes > {MAX_TOKEN_BYTES}", next.len()); }
        if !verify(&next, &root, now) { break format!("verifier rejects {} hops", max_hops + 1); }
        tok = next; signer = child; max_hops += 1;
        if max_hops >= 4096 { break "4096-hop harness cap".to_string(); }
    };
    let max_tok = tok;
    let tok8 = chain(&root, now, 8.min(max_hops), 0, 0, 3);
    let tok16 = chain(&root, now, 16.min(max_hops), 0, 0, 3);

    // Worst-accepted candidates: wide caveat lists (evaluation) and many rights
    // (fact volume). Largest accepted size found by doubling, then bisection.
    let largest = |ok: &dyn Fn(usize) -> bool| -> usize {
        let mut lo = 1usize;
        while lo < 8192 && ok(lo * 2) { lo *= 2; }
        let mut hi = lo * 2;
        while hi - lo > 1 { let mid = (lo + hi) / 2; if ok(mid) { lo = mid } else { hi = mid } }
        lo
    };
    let wide = largest(&|n| { let t = chain(&root, now, 1, n, n, 3); t.len() <= MAX_TOKEN_BYTES && verify(&t, &root, now) });
    let wide_tok = chain(&root, now, 1, wide, wide, 3);
    let rights = largest(&|n| { let t = mint(&root, &pop, now, n); t.len() <= MAX_TOKEN_BYTES && verify(&t, &root, now) });
    let rights_tok = mint(&root, &pop, now, rights);
    println!("{{\"shape\":{{\"max_hops\":{max_hops},\"max_hops_stop\":{:?},\"wide_caveat_entries\":{wide},\"max_rights_facts\":{rights},\"root_bytes\":{},\"one_hop_bytes\":{},\"max_hop_bytes\":{},\"wide_bytes\":{},\"rights_bytes\":{}}}}}",
             stop, root_tok.len(), one.len(), max_tok.len(), wide_tok.len(), rights_tok.len());

    let mut i = 0usize;
    measure("mint", iters, root_tok.len(), || { i += 1; !mint(&root, &pop, now, 3).is_empty() });
    let child = SigningKey::from_bytes(&[3; 32]);
    measure("attenuate_one_hop", iters, one.len(), || { !attenuate(&root_tok, &pop, &child, now, 0, 0, 1).is_empty() });
    measure("verify_root", iters, root_tok.len(), || verify(&root_tok, &root, now));
    measure("verify_one_hop", iters, one.len(), || verify(&one, &root, now));
    measure("verify_8_hops", iters, tok8.len(), || verify(&tok8, &root, now));
    measure("verify_16_hops", iters, tok16.len(), || verify(&tok16, &root, now));
    let slow = (iters / 4).max(20);
    measure("verify_max_hops", slow, max_tok.len(), || verify(&max_tok, &root, now));
    measure("verify_worst_wide_caveats", slow, wide_tok.len(), || verify(&wide_tok, &root, now));
    measure("verify_worst_many_rights", slow, rights_tok.len(), || verify(&rights_tok, &root, now));
}
'''

def bench_manifest(cfg, heddle_src: Path):
    ws = tomllib.loads((heddle_src / "Cargo.toml").read_text())["workspace"]["dependencies"]
    dalek = ws["ed25519-dalek"]
    dalek_v = dalek["version"] if isinstance(dalek, dict) else dalek
    if cfg == "current":
        biscuit = 'biscuit-auth = "6"'
        patch = ""
    else:
        biscuit = 'biscuit-auth = { version = "6", default-features = false }'
        patch = f'[patch.crates-io]\nbiscuit-auth = {{ git = "{PIN_URL}", rev = "{PIN}" }}\n'
    return f'''[package]
name = "apex-p0-bench"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
{biscuit}
heddle-biscuit-verifier = {{ path = "{heddle_src}/crates/biscuit-verifier" }}
chrono = "0.4"
ed25519-dalek = "{dalek_v}"
hex = "0.4"

[profile.release]
lto = true
codegen-units = 1
strip = true

{patch}'''

def step_latency(doc):
    items = []
    bins = {}
    for cfg in ["current", "fork"]:
        hsrc = src_dir("heddle", cfg)
        b = SCRATCH / f"bench-{cfg}"
        shutil.rmtree(b, ignore_errors=True)
        (b / "src").mkdir(parents=True)
        (b / "Cargo.toml").write_text(bench_manifest(cfg, hsrc))
        (b / "src/main.rs").write_text(BENCH_RS)
        shutil.copy(hsrc / "Cargo.lock", b / "Cargo.lock")
        t = f"bench-{cfg}"
        guard_disk(f"bench build {cfg}")
        cmd = ["cargo", "build", "--release"]
        rc, out, _ = run(cmd, b, cargo_env(t))
        if rc != 0:
            items.append(unsupported(f"latency/bench-build", "heddle", cfg, cmdstr(cmd, b), "bench build failed", out))
            continue
        exe = SCRATCH / f"apex-p0-bench-{cfg}"
        shutil.copy(target(t) / "release/apex-p0-bench", exe)
        rc2, tree, _ = run(["cargo", "tree", "--offline", "-i", "biscuit-auth", "-e", "features", "--prefix", "none", "-f", "{p}|{f}"], b, cargo_env(t))
        bins[cfg] = {"exe": exe, "size": file_sizes(exe), "biscuit": tree.splitlines()[0] if tree else None,
                     "command": cmdstr(cmd, b), "manifest": (b / "Cargo.toml").read_text()}
        drop_target(t)
    if len(bins) < 2:
        set_results(doc, "latency", items)
        return doc
    # Interleave processes: round 0 is the warm-up and is discarded.
    per = {c: [] for c in bins}
    shape = {}
    for r in range(RUNS + 1):
        order = ["current", "fork"] if r % 2 == 0 else ["fork", "current"]
        for c in order:
            load = os.getloadavg()
            p = subprocess.run([str(bins[c]["exe"]), "200"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            if p.returncode != 0:
                items.append(unsupported("latency/run", "heddle", c, str(bins[c]["exe"]), "bench run failed", p.stdout))
                set_results(doc, "latency", items)
                return doc
            ops = {}
            for line in p.stdout.splitlines():
                o = json.loads(line)
                if "shape" in o:
                    shape[c] = o["shape"]
                else:
                    o["load1_before"] = round(load[0], 2)
                    o["round"] = r
                    ops[o["op"]] = o
            if r > 0:
                per[c].append(ops)
            log(f"latency {c} round {r}: verify_one_hop p50 {ops['verify_one_hop']['p50_ns']/1000:.0f}us load1 {load[0]:.1f}")
    for c in bins:
        opnames = list(per[c][0].keys())
        for op in opnames:
            runs = [rr[op] for rr in per[c]]
            pooled = [x for rr in runs for x in rr["samples_ns"]]
            value = {
                "unit": "ns", "runs": [{k: rr[k] for k in ("round", "n", "p50_ns", "p95_ns", "p99_ns", "max_ns", "mean_ns", "load1_before")} for rr in runs],
                "pooled": {"n": len(pooled), "p50_ns": pct(pooled, 0.5), "p95_ns": pct(pooled, 0.95), "p99_ns": pct(pooled, 0.99), "max_ns": max(pooled)},
                "run_p50_spread": stats([rr["p50_ns"] for rr in runs]),
                "token_bytes": runs[0]["token_bytes"],
            }
            items.append(measured(f"latency/{op}", "heddle", c, f"{bins[c]['exe'].name} 200", value))
        items.append(measured("latency/bench-binary", "heddle", c, bins[c]["command"],
                              {**bins[c]["size"], "biscuit_auth": bins[c]["biscuit"], "shape": shape.get(c), "manifest": bins[c]["manifest"],
                               "source_sha256": hashlib.sha256(BENCH_RS.encode()).hexdigest()}))
    for c in bins:
        bins[c]["exe"].unlink(missing_ok=True)
        shutil.rmtree(SCRATCH / f"bench-{c}", ignore_errors=True)
    set_results(doc, "latency", items)
    return doc

# ---------------------------------------------------------------- verification

def doc_for_selftest(doc):
    return json.loads(json.dumps(doc))

def validate(doc):
    """Return a list of problems (empty = valid)."""
    import jsonschema
    problems = []
    schema = json.loads(SCHEMA.read_text())
    v = jsonschema.Draft202012Validator(schema)
    for e in sorted(v.iter_errors(doc), key=lambda e: list(e.path)):
        problems.append(f"schema: {'/'.join(map(str, e.path)) or '<root>'}: {e.message[:300]}")
    # Completeness rules the schema cannot express.
    res = doc.get("results", {})
    for key in ("dependency_graph", "builds", "binary_sizes", "wasm", "bundle", "latency", "features", "negative_tests"):
        if not res.get(key):
            problems.append(f"results.{key}: missing or empty")
    for key, items in res.items():
        for it in items or []:
            if it.get("status") == "measured":
                val = it.get("value")
                if val is None:
                    problems.append(f"{it.get('id')}: measured with null value")
                if key == "builds" and isinstance(val, dict) and len(val.get("samples", [])) < MIN_RUNS:
                    problems.append(f"{it.get('id')}/{it.get('config')}: {len(val.get('samples', []))} samples < {MIN_RUNS}")
                if key == "latency" and isinstance(val, dict) and "runs" in val and len(val["runs"]) < MIN_RUNS:
                    problems.append(f"{it.get('id')}/{it.get('config')}: {len(val['runs'])} runs < {MIN_RUNS}")
    for it in res.get("negative_tests", []) or []:
        if it.get("status") == "measured" and not (it.get("value") or {}).get("demonstrated"):
            problems.append(f"{it.get('id')}: negative test not demonstrated")
    return problems

def schema_self_test(doc):
    """A report with missing toolchain/source metadata must be rejected."""
    bad = json.loads(json.dumps(doc))
    bad.setdefault("meta", {}).pop("toolchain", None)
    bad["meta"].pop("sources", None)
    import jsonschema
    schema = json.loads(SCHEMA.read_text())
    errs = list(jsonschema.Draft202012Validator(schema).iter_errors(bad))
    missing = [e for e in errs if "toolchain" in e.message or "sources" in e.message]
    return len(missing) >= 2

def verify_report(path):
    doc = json.loads(Path(path).read_text())
    problems = validate(doc)
    for p in problems:
        print("FAIL  " + p)
    st = schema_self_test(doc)
    print(("PASS" if st else "FAIL") + "  self-test: report without meta.toolchain and meta.sources is rejected by the schema")
    counts = {}
    for key, items in doc.get("results", {}).items():
        for it in items:
            counts.setdefault(key, {"measured": 0, "unsupported": 0})[it["status"]] += 1
    for k, c in sorted(counts.items()):
        print(f"      {k}: {c['measured']} measured, {c['unsupported']} unsupported")
    ok = not problems and st
    print(f"apex-p0 verify-report: {'OK' if ok else 'FAILED'} ({path})")
    return 0 if ok else 1

# ---------------------------------------------------------------- main

def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print("usage: p0-measure.sh --all --output FILE | --steps a,b --output FILE | --verify-report FILE")
        return 0
    if argv[0] == "--verify-report":
        if len(argv) != 2:
            die("--verify-report FILE")
        return verify_report(argv[1])
    output, steps = None, None
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--all":
            steps = STEPS
        elif a == "--steps":
            steps = [s for s in argv[i + 1].split(",") if s]
            i += 1
        elif a == "--output":
            output = argv[i + 1]
            i += 1
        else:
            die(f"unknown argument {a}")
        i += 1
    if not output or not steps:
        die("need --output FILE and --all or --steps")
    if RUNS < MIN_RUNS:
        die(f"APEX_P0_RUNS={RUNS} < {MIN_RUNS}")
    unknown = [s for s in steps if s not in STEPS]
    if unknown:
        die(f"unknown steps {unknown}")
    output = str(Path(output).resolve())
    SCRATCH.mkdir(parents=True, exist_ok=True)
    TARGETS.mkdir(parents=True, exist_ok=True)
    doc = load_report(output)
    for s in steps:
        log(f"step {s} (load {os.getloadavg()})")
        t0 = time.monotonic()
        try:
            doc = globals()[f"step_{s}"](doc)
        except SystemExit:
            doc.setdefault("meta", {}).setdefault("step_log", {})[s] = {
                "aborted_at": now_iso(), "loadavg_end": os.getloadavg(), "free_gb": round(free_gb(), 1)}
            save_report(doc, output)
            raise
        doc.setdefault("meta", {}).setdefault("step_log", {})[s] = {
            "finished_at": now_iso(), "wall_s": round(time.monotonic() - t0, 1), "loadavg_end": os.getloadavg()}
        save_report(doc, output)
    return 0

sys.exit(main(sys.argv[1:]))
PY
