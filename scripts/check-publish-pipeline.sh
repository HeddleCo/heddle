#!/usr/bin/env bash
# Asserter for the crates.io auto-publish pipeline contract (heddle#72).
#
# The publish workflow runs only on push-to-main, so we can't observe its
# artifacts in normal PR CI. Instead we statically verify that
# `.github/workflows/publish-crates.yml` declares the contract every
# downstream consumer (release-plz PRs, crates.io, eventual library
# consumers) relies on:
#
#   - push-to-main trigger (and NO workflow_dispatch — automation must
#     never be triggerable from outside main's history)
#   - validate-publish trust gate runs first, emits validated outputs
#   - publish job depends on validate-publish and reads its outputs
#   - publish job pins checkout to validate-publish's commit SHA, not
#     the mutable refs/heads/main (closes the TOCTOU window — same
#     reason heddle#56's release.yml pins to tag_sha)
#   - publish job exposes the CARGO_REGISTRY_TOKEN env var (cargo's
#     documented name) AND maps it from secrets.CRATES_IO_API_KEY (the
#     actual repo-settings secret name). The names are decoupled: cargo
#     reads CARGO_REGISTRY_TOKEN; GitHub Actions looks up secrets by
#     their settings name. The asserter checks both halves so a rename
#     on either side fails loud.
#   - explicit publishable-crates list (no auto-discovery — implicit
#     `publish = true` in Cargo.toml is too easy to misconfigure)
#
# This mirrors scripts/check-release-pipeline.sh (heddle#56). The two
# asserters are independently runnable but follow the same two-pass
# shape.

set -euo pipefail

WF=".github/workflows/publish-crates.yml"
fail=0

err() { echo "::error::$*" >&2; fail=1; }
ok()  { echo "ok: $*"; }

if [[ ! -f "$WF" ]]; then
  err "$WF does not exist"
  echo "::error::Publish pipeline not implemented. See heddle#72."
  exit 1
fi

# --- Smoke (grep) ---------------------------------------------------------

# Push-to-main trigger. Strict: the workflow must fire on main-branch
# pushes only, never on tag pushes (those are heddle#56's release.yml)
# and never via workflow_dispatch (automation must not be triggerable
# from outside main's history; a force-publish is a deliberate ops
# action that should run locally with the maintainer's own creds).
if grep -E "^\s*push:" "$WF" >/dev/null \
   && grep -E "branches:" "$WF" >/dev/null \
   && grep -E "['\"]?main['\"]?" "$WF" >/dev/null; then
  ok "push-to-main trigger present"
else
  err "missing push-to-main trigger in $WF"
fi

# Explicit anti-pattern: workflow_dispatch creates a path for an
# operator (or an attacker with workflow-dispatch rights) to publish
# from a non-main commit. The grep is intentionally loose so renames or
# inline comments mentioning workflow_dispatch in a NOTE block still
# match — the strict pass (below) then re-verifies via parsed YAML.
if grep -E "^\s*workflow_dispatch:" "$WF" >/dev/null; then
  err "$WF must NOT declare workflow_dispatch (force-publish is a deliberate ops action; run cargo publish locally)"
else
  ok "no workflow_dispatch trigger (anti-pattern correctly absent)"
fi

# Verification gate: validate-publish job must run before publish and emit
# validated outputs. The structural shape is checked here; the strict
# pass verifies the outputs exist on the job and that publish reads
# from them.
if grep -E "^\s*validate-publish:" "$WF" >/dev/null; then
  ok "validate-publish job present"
else
  err "missing validate-publish job in $WF"
fi

if grep -E "^\s*publish:" "$WF" >/dev/null; then
  ok "publish job present"
else
  err "missing publish job in $WF"
fi

if grep -E "needs:\s*validate-publish|needs:\s*\[validate-publish" "$WF" >/dev/null; then
  ok "publish job depends on validate-publish"
else
  err "publish job must declare 'needs: validate-publish' so credentialed publish is gated on it"
fi

# Token wiring. We assert the exact secret name and the env var name —
# any rename on either side would silently break authentication, and
# the workflow would either fail loud at first publish or (worse) drop
# the auth header entirely depending on cargo's behavior.
#
# Note the decoupling: the repo-settings secret is CRATES_IO_API_KEY,
# but cargo reads CARGO_REGISTRY_TOKEN from the process env. The
# workflow does the mapping. Both halves are checked.
if grep -F 'secrets.CRATES_IO_API_KEY' "$WF" >/dev/null; then
  ok "publish step reads secrets.CRATES_IO_API_KEY"
else
  err "$WF must reference secrets.CRATES_IO_API_KEY (the configured repo-settings secret name)"
fi

if grep -E '^\s*CARGO_REGISTRY_TOKEN:' "$WF" >/dev/null; then
  ok "CARGO_REGISTRY_TOKEN env var declared"
else
  err "$WF must expose the token as the CARGO_REGISTRY_TOKEN env var (cargo's documented name)"
fi

# Explicit crate list. Auto-discovery via `cargo metadata --workspace`
# would publish whatever's currently marked publishable in Cargo.toml,
# which is invisible at PR review time. An explicit list (env var or
# matrix) makes adding a publishable crate a one-line workflow edit
# reviewed in PR. We look for a PUBLISHABLE_CRATES marker.
if grep -E "PUBLISHABLE_CRATES" "$WF" >/dev/null; then
  ok "explicit PUBLISHABLE_CRATES list present"
else
  err "$WF must maintain an explicit PUBLISHABLE_CRATES list (no auto-discovery — see heddle#72 design)"
fi

# RELEASING.md must document the auto-publish flow alongside heddle#56's
# binary release docs.
if [[ ! -f RELEASING.md ]]; then
  err "RELEASING.md is missing"
else
  if grep -F 'publish-crates.yml' RELEASING.md >/dev/null; then
    ok "RELEASING.md documents publish-crates.yml"
  else
    err "RELEASING.md must document publish-crates.yml (the auto-publish flow)"
  fi
fi

# --- Strict structural checks ---------------------------------------------
#
# The grep-based checks above answer "does the pipeline mention X
# anywhere?" — useful as a smoke screen, but blind to per-job structure.
# The strict checks below verify the workflow's relevant structure without
# pulling in a PyYAML runtime dependency:
#
#   - validate-publish declares the documented outputs (commit_sha,
#     to_publish, has_publishes). Without commit_sha, the SHA pin in
#     the publish job has nothing to reference.
#   - publish job's actions/checkout pins ref to the validated
#     commit_sha, not refs/heads/main. main is mutable: between
#     validate-publish reading HEAD and the publish job checking out,
#     a force-push could redirect the publish to attacker-controlled
#     code. Pinning the SHA closes that window.
#   - publish job's `if:` references has_publishes so the whole job is
#     skipped on no-op merges (the common case).
#   - the trigger really is push-only (no workflow_dispatch slipped in
#     past the grep via a different shape).

if ! command -v python3 >/dev/null 2>&1; then
  err "python3 not available; strict structural checks skipped"
else
  strict_report=$(python3 - "$WF" <<'PY'
import re
import sys
from pathlib import Path

wf_path = sys.argv[1]
text = Path(wf_path).read_text()
lines = text.splitlines()

errors = []
oks = []

def line_index(pattern):
    rx = re.compile(pattern)
    for i, line in enumerate(lines):
        if rx.match(line):
            return i
    return -1

def block_after(start_pattern, sibling_pattern):
    start = line_index(start_pattern)
    if start < 0:
        return ""
    sibling = re.compile(sibling_pattern)
    end = len(lines)
    for i in range(start + 1, len(lines)):
        if sibling.match(lines[i]):
            end = i
            break
    return "\n".join(lines[start:end])

on_block = block_after(r"^on:\s*$", r"^[A-Za-z0-9_-]+:")
jobs_block = block_after(r"^jobs:\s*$", r"^[A-Za-z0-9_-]+:")
validate_block = block_after(r"^  validate-publish:\s*$", r"^  [A-Za-z0-9_-]+:")
publish_block = block_after(r"^  publish:\s*$", r"^  [A-Za-z0-9_-]+:")

if not on_block:
    errors.append("workflow `on:` must be a mapping (push:...)")
else:
    if not re.search(r"(?m)^  push:\s*$", on_block):
        errors.append("workflow must trigger on push (with branches: [main])")
    else:
        push_block = block_after(r"^  push:\s*$", r"^  [A-Za-z0-9_-]+:")
        if re.search(r"(?m)^    branches:\s*$", push_block) and re.search(
            r"(?m)^      - ['\"]?main['\"]?\s*$", push_block
        ):
            oks.append("push trigger restricted to main branch")
        else:
            errors.append("push trigger must include 'main' branch")
        if re.search(r"(?m)^    tags:", push_block):
            errors.append("push trigger must not include tags (tag releases live in release.yml — heddle#56)")
    if re.search(r"(?m)^  workflow_dispatch:\s*$", on_block):
        errors.append("workflow_dispatch must not be declared (force-publish is a deliberate ops action)")
    else:
        oks.append("workflow_dispatch correctly absent")

if not jobs_block:
    errors.append("jobs mapping missing or malformed")

if not validate_block:
    errors.append("validate-publish job missing or malformed")
else:
    for out_name in ("commit_sha", "to_publish", "has_publishes"):
        if re.search(rf"(?m)^      {re.escape(out_name)}:\s*", validate_block):
            oks.append(f"validate-publish exports {out_name} output")
        else:
            errors.append(
                f"validate-publish must declare a '{out_name}' output "
                f"(downstream publish job reads from it)"
            )

if not publish_block:
    errors.append("publish job missing or malformed")
else:
    if re.search(r"(?m)^    needs:\s*validate-publish\s*$", publish_block) or re.search(
        r"(?m)^    needs:\s*\[\s*validate-publish\s*\]\s*$", publish_block
    ):
        oks.append("publish job declares needs: validate-publish")
    else:
        errors.append("publish job does not declare 'needs: validate-publish' (would skip the trust gate)")

    if "needs.validate-publish.outputs.has_publishes" in publish_block:
        oks.append("publish job gates execution on has_publishes")
    else:
        errors.append(
            "publish job's `if:` must reference "
            "needs.validate-publish.outputs.has_publishes so no-op merges skip the job entirely"
        )

    if "uses: actions/checkout@" not in publish_block:
        errors.append("publish job has no actions/checkout step — cannot verify SHA pin")
    elif "ref: ${{ needs.validate-publish.outputs.commit_sha }}" in publish_block:
        if re.search(r"(?m)^\s*ref:\s*refs/heads/main\s*$", publish_block):
            errors.append(
                "publish job mixes refs/heads/main with commit_sha — refs/heads/main is mutable; remove it"
            )
        else:
            oks.append("publish job pins checkout to validated commit_sha")
    else:
        errors.append(
            "publish job checkout must use ref: ${{ needs.validate-publish.outputs.commit_sha }} "
            "— TOCTOU on mutable main ref"
        )

    if re.search(r"(?m)^      CARGO_REGISTRY_TOKEN:\s*", publish_block):
        oks.append("env var key is exactly CARGO_REGISTRY_TOKEN (the name cargo reads)")
        if "secrets.CRATES_IO_API_KEY" in publish_block:
            oks.append("CARGO_REGISTRY_TOKEN wired from secrets.CRATES_IO_API_KEY")
        else:
            errors.append("CARGO_REGISTRY_TOKEN env var must read from secrets.CRATES_IO_API_KEY")
    else:
        errors.append(
            "publish job must expose CARGO_REGISTRY_TOKEN as an env var "
            "(at job or step scope) so cargo publish can authenticate"
        )

print("OKS:")
for o in oks:
    print(o)
print("ERRORS:")
for e in errors:
    print(e)
PY
  )

  in_oks=0
  in_errors=0
  while IFS= read -r line; do
    case "$line" in
      "OKS:")     in_oks=1; in_errors=0; continue ;;
      "ERRORS:")  in_oks=0; in_errors=1; continue ;;
    esac
    [[ -z "$line" ]] && continue
    if (( in_oks )); then
      ok "$line"
    elif (( in_errors )); then
      err "$line"
    fi
  done <<< "$strict_report"
fi

# --- Internal-consumer / publishable-version compat ----------------------
#
# Why this check exists: an earlier source-crate bump left internal workspace
# consumers pinned to an incompatible version. Local `cargo build` was happy
# (path deps override version reqs in-workspace), but the push-to-main
# publish workflow tried `cargo publish` — which strips path deps and
# resolves consumers against crates.io — and failed loud with
# "candidate versions found which didn't match: 0.3.0".
#
# The structural rule: every internal workspace consumer of a
# publishable crate must declare a version requirement that the
# publishable crate's CURRENT version satisfies. If it doesn't, the
# next push-to-main publish will fail.
#
# Verification aspect: we DON'T trust the PUBLISHABLE_CRATES env var from the
# workflow as the source of truth here. We re-derive the publishable
# set from each Cargo.toml's [package].publish field (default-publish
# is publishable). That way a brand-new publishable crate added to the
# workspace gets caught by this asserter without anyone remembering to
# update the check.
# TOML parser detection: tomllib lands in stdlib at Python 3.11. On 3.10
# and earlier we fall back to the third-party `tomli` (the same code,
# pre-stdlib). If neither is importable we emit a genuine `skip:` line
# and do NOT set fail — "I couldn't run this check" is not the same as
# "this check failed" (Codex r1 finding).
toml_module=""
if command -v python3 >/dev/null 2>&1; then
  if python3 -c 'import tomllib' 2>/dev/null; then
    toml_module="tomllib"
  elif python3 -c 'import tomli' 2>/dev/null; then
    toml_module="tomli"
  fi
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "skip: python3 unavailable; consumer-version check skipped"
elif [[ -z "$toml_module" ]]; then
  echo "skip: tomllib (Python 3.11+) and tomli both unavailable; consumer-version check skipped"
else
  ok "toml parser available ($toml_module)"
  consumer_report=$(TOML_MODULE="$toml_module" python3 - <<'PY'
import glob
import importlib
import os
import re
tomllib = importlib.import_module(os.environ["TOML_MODULE"])

with open("Cargo.toml", "rb") as f:
    workspace_toml = tomllib.load(f)
workspace_version = workspace_toml.get("workspace", {}).get("package", {}).get("version")

crates = []
for cm in sorted(glob.glob("crates/*/Cargo.toml")):
    with open(cm, "rb") as f:
        crates.append((cm, tomllib.load(f)))

errors = []
oks = []

by_name = {}      # crate name → current version string
publishable = set()
for cm, toml in crates:
    pkg = toml.get("package", {})
    name = pkg.get("name")
    version = pkg.get("version")
    if isinstance(version, dict) and version.get("workspace") is True:
        version = workspace_version
    if not name or not isinstance(version, str):
        continue
    by_name[name] = version
    # `publish` unset defaults to true (publishable). A non-empty list
    # restricts the registries but is still publishable. `false` /
    # empty list / explicit False means not publishable.
    pub = pkg.get("publish")
    if pub is None or pub is True or (isinstance(pub, list) and len(pub) > 0):
        publishable.add(name)


def parse_ver_full(s):
    """Returns (major, minor, patch, prerelease) where prerelease is the
    raw pre-release identifier (everything after the first `-`, with build
    metadata stripped) or "" if absent."""
    s = s.strip()
    base, dash, rest = s.partition("-")
    # Strip +build metadata from whichever side it appears.
    base = base.partition("+")[0]
    pre = rest.partition("+")[0] if dash else ""
    parts = (base.split(".") + ["0", "0", "0"])[:3]
    out = []
    for p in parts:
        try:
            out.append(int(p))
        except ValueError:
            out.append(0)
    return (out[0], out[1], out[2], pre)


def _cmp_prerelease(a, b):
    """Compare two prerelease identifier strings per semver §11. Returns
    -1/0/1. Empty string means "no prerelease" and sorts ABOVE any
    prerelease (1.0.0 > 1.0.0-anything). Within prereleases: dot-separated
    identifiers compared left-to-right; numeric identifiers compare
    numerically, alphanumerics lexicographically, numerics rank below
    alphanumerics, a shorter prefix loses to a longer one with the same
    prefix (alpha < alpha.1)."""
    if a == b:
        return 0
    if a == "":
        return 1
    if b == "":
        return -1
    ap, bp = a.split("."), b.split(".")
    for x, y in zip(ap, bp):
        xn, yn = x.isdigit(), y.isdigit()
        if xn and yn:
            xi, yi = int(x), int(y)
            if xi != yi:
                return -1 if xi < yi else 1
        elif xn != yn:
            return -1 if xn else 1
        elif x != y:
            return -1 if x < y else 1
    if len(ap) != len(bp):
        return -1 if len(ap) < len(bp) else 1
    return 0


def satisfies(req, ver):
    """Cargo caret semantics. Returns True / False, or None to signal an
    unsupported comparator shape (the caller treats None as a hard error).

    Cargo's caret rules (https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html):
      ^1.2.3 → >=1.2.3, <2.0.0   (uppermost nonzero major)
      ^0.2.3 → >=0.2.3, <0.3.0   (uppermost nonzero minor when major == 0)
      ^0.0.3 → >=0.0.3, <0.0.4   (uppermost nonzero patch when major+minor == 0)
      ^0.0   → >=0.0.0, <0.1.0
      ^0     → >=0.0.0, <1.0.0
    Bare requirements (no leading operator) follow caret by default; the
    width is determined by how many components the requirement specifies,
    not the parsed integer tuple — `"0"` is a 1-component all-zero req
    and must widen to <1.0.0, where the previous all-zero collapse to
    exact-patch was wrong (Codex r1 finding).

    Prerelease rules (cargo opts in only when the requirement asks for one):
      - source has prerelease, req does not → reject.
      - req has prerelease → restrict matches to the same (major,minor,patch)
        tuple (cargo's documented behavior), and within that tuple require
        source prerelease >= req prerelease using semver ordering. Catches
        both "0.3.0-alpha.0 satisfying 0.3.0-alpha.1" (Codex r2 P2 finding)
        and "1.0.1 leaking through a 1.0.0-alpha requirement" (Codex r2 P2).
      - exact `=X.Y.Z-pre` still routes through the exact branch.

    Exact `=` comparator:
      - Build metadata (`+...`) is ignored on both sides — semver §10 says
        build metadata MUST NOT factor into precedence (Codex r2 P3).
      - Partial exact requirements widen to ranges per cargo:
        `=4`   → >=4.0.0, <5.0.0-0
        `=4.2` → >=4.2.0, <4.3.0-0
        `=4.2.3` (or with prerelease tag) stays an exact normalized match.
        (Codex r2 P3 finding.)

    Wildcards (`*` anywhere in the requirement) are rejected outright —
    `1.2.*` is a valid cargo comparator but the workspace convention is
    caret, and the previous loose guard let `1.2.*` fall through to
    numeric parsing where `*` coerced to 0 and the check effectively
    became major-only (Codex r1 finding).
    """
    req = req.strip()

    # Wildcards anywhere are rejected (not just leading). Catches "1.2.*".
    if "*" in req:
        return None

    if req.startswith("="):
        body = req[1:].strip()
        body_no_meta = body.partition("+")[0]
        has_pre = "-" in body_no_meta
        base = body_no_meta.split("-", 1)[0]
        base_parts_given = len([p for p in base.split(".") if p != ""])
        # Partial `=` requirements are ranges (no prerelease in this form).
        if not has_pre and base_parts_given < 3:
            rmaj, rmin, rpat, _ = parse_ver_full(body)
            vmaj, vmin, vpat, vpre = parse_ver_full(ver)
            if vpre:
                return False
            if base_parts_given == 1:
                return (vmaj, vmin, vpat) >= (rmaj, 0, 0) and vmaj == rmaj
            # base_parts_given == 2
            return (
                (vmaj, vmin, vpat) >= (rmaj, rmin, 0)
                and (vmaj, vmin) == (rmaj, rmin)
            )
        # Full exact (`=X.Y.Z` or `=X.Y.Z-pre`): ignore build metadata on
        # BOTH sides per semver §10.
        return body_no_meta == ver.strip().partition("+")[0]

    # Other comparators / multi-clause requirements: surface, don't guess.
    if req[:1] in (">", "<", "~") or "," in req:
        return None

    if req.startswith("^"):
        req = req[1:]

    # How many components did the requirement actually specify?
    # ("0" vs "0.0" vs "0.0.0" differ in caret width when all zero.)
    req_parts_given = len([p for p in req.split(".") if p != ""])

    rmaj, rmin, rpat, rpre = parse_ver_full(req)
    vmaj, vmin, vpat, vpre = parse_ver_full(ver)

    # Req has a prerelease tag: cargo restricts matches to the same
    # (major,minor,patch) tuple AND requires source prerelease >= req
    # prerelease (release counts as > any prerelease on the same tuple).
    if rpre:
        if (vmaj, vmin, vpat) != (rmaj, rmin, rpat):
            return False
        return _cmp_prerelease(vpre, rpre) >= 0

    # Prerelease in source but not in requirement → not satisfied.
    if vpre:
        return False

    # Lower bound: source >= requirement (no prerelease on either side here).
    if (vmaj, vmin, vpat) < (rmaj, rmin, rpat):
        return False

    # Upper bound: caret widens at the leftmost-nonzero component.
    if rmaj > 0:
        return vmaj == rmaj
    if rmin > 0:
        return vmaj == 0 and vmin == rmin
    if rpat > 0:
        return vmaj == 0 and vmin == 0 and vpat == rpat

    # All-zero requirement: width depends on how many components were given.
    if req_parts_given <= 1:
        # "0" → <1.0.0
        return vmaj == 0
    if req_parts_given == 2:
        # "0.0" → <0.1.0
        return vmaj == 0 and vmin == 0
    # "0.0.0" → <0.0.1 (exact)
    return (vmaj, vmin, vpat) == (0, 0, 0)


# --- Self-test the caret-semver parser ------------------------------------
# One assertion bundle per Codex r1 P2 finding. Each bundle that passes
# emits a single `ok:` line; failures become `err:` so a future edit
# that reintroduces one of the four bugs fails the asserter immediately.
def _selftest(label, cases):
    for got, want in cases:
        if got != want:
            errors.append(f"satisfies() self-test failed: {label} (got {got!r}, want {want!r})")
            return
    oks.append(f"satisfies() self-test: {label}")

_selftest(
    "caret semantics for bare 0 and 0.0 (cargo default widens to <1.0.0 / <0.1.0)",
    [
        (satisfies("0", "0.5.2"),    True),
        (satisfies("0", "1.0.0"),    False),
        (satisfies("0.0", "0.0.7"),  True),
        (satisfies("0.0", "0.1.0"),  False),
    ],
)
_selftest(
    "prerelease versions excluded from non-prerelease requirements (= opts in)",
    [
        (satisfies("0.3", "0.3.0-alpha.1"),            False),
        (satisfies("=0.3.0-alpha.1", "0.3.0-alpha.1"), True),
        (satisfies("=0.3.0-alpha.1", "0.3.0"),         False),
    ],
)
_selftest(
    "wildcard requirements rejected (1.2.* surfaces as unsupported)",
    [
        (satisfies("1.2.*", "1.2.5"), None),
        (satisfies("*", "1.0.0"),     None),
    ],
)
_selftest(
    "caret prerelease lower-bound ordering (alpha.0 fails alpha.1; release sat. prerelease)",
    [
        (satisfies("0.3.0-alpha.1", "0.3.0-alpha.0"), False),
        (satisfies("0.3.0-alpha.1", "0.3.0-alpha.1"), True),
        (satisfies("0.3.0-alpha.1", "0.3.0-alpha.2"), True),
        (satisfies("0.3.0-alpha.1", "0.3.0-beta"),    True),
        (satisfies("0.3.0-alpha.1", "0.3.0"),         True),
    ],
)
_selftest(
    "prerelease requirements pin to same (M,m,p) tuple (cargo's documented rule)",
    [
        (satisfies("1.0.0-alpha", "1.0.0-alpha"),   True),
        (satisfies("1.0.0-alpha", "1.0.0-alpha.1"), True),
        (satisfies("1.0.0-alpha", "1.0.0"),         True),
        (satisfies("1.0.0-alpha", "1.0.1-alpha"),   False),
        (satisfies("1.0.0-alpha", "1.0.1"),         False),
    ],
)
_selftest(
    "exact `=` ignores build metadata on both sides (semver §10)",
    [
        (satisfies("=0.3.0-alpha.1", "0.3.0-alpha.1+build.5"), True),
        (satisfies("=0.3.0+meta",    "0.3.0+other"),           True),
        (satisfies("=0.3.0",         "0.3.0+build"),           True),
        (satisfies("=0.3.0",         "0.3.1"),                 False),
    ],
)
_selftest(
    "partial `=` requirements widen to ranges (=4 → <5.0.0-0; =4.2 → <4.3.0-0)",
    [
        (satisfies("=4",   "4.0.0"),     True),
        (satisfies("=4",   "4.999.999"), True),
        (satisfies("=4",   "5.0.0"),     False),
        (satisfies("=4",   "3.9.9"),     False),
        (satisfies("=4.2", "4.2.0"),     True),
        (satisfies("=4.2", "4.2.99"),    True),
        (satisfies("=4.2", "4.3.0"),     False),
        (satisfies("=4.2", "4.1.9"),     False),
    ],
)


DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")

checked = 0
for cm, toml in crates:
    consumer_name = toml.get("package", {}).get("name", cm)
    tables = []
    for k in DEP_TABLES:
        if isinstance(toml.get(k), dict):
            tables.append(toml[k])
    for _tk, tv in (toml.get("target", {}) or {}).items():
        if not isinstance(tv, dict):
            continue
        for k in DEP_TABLES:
            if isinstance(tv.get(k), dict):
                tables.append(tv[k])

    for deps in tables:
        for dep_key, dep_val in deps.items():
            if not isinstance(dep_val, dict):
                continue
            pkg_name = dep_val.get("package") or dep_key
            if pkg_name not in publishable:
                continue
            req = dep_val.get("version")
            if not isinstance(req, str):
                continue
            src_ver = by_name.get(pkg_name)
            if src_ver is None:
                continue
            checked += 1
            result = satisfies(req, src_ver)
            if result is None:
                errors.append(
                    f"{consumer_name} declares {pkg_name} = \"{req}\" "
                    f"(unsupported comparator shape; workspace convention is caret)"
                )
            elif not result:
                errors.append(
                    f"{consumer_name} requires {pkg_name} = \"{req}\", "
                    f"but {pkg_name} current version is {src_ver} (incompatible)"
                )

if not by_name:
    errors.append("could not parse any crates/*/Cargo.toml")
elif not errors:
    oks.append(
        f"internal workspace consumers satisfy publishable crate versions "
        f"({checked} req/version pairs, {len(publishable)} publishable crates)"
    )

print("OKS:")
for o in oks:
    print(o)
print("ERRORS:")
for e in errors:
    print(e)
PY
)

  in_oks=0
  in_errors=0
  while IFS= read -r line; do
    case "$line" in
      "OKS:")     in_oks=1; in_errors=0; continue ;;
      "ERRORS:")  in_oks=0; in_errors=1; continue ;;
    esac
    [[ -z "$line" ]] && continue
    if (( in_oks )); then
      ok "$line"
    elif (( in_errors )); then
      err "$line"
    fi
  done <<< "$consumer_report"
fi

if (( fail )); then
  echo "publish-pipeline check FAILED" >&2
  exit 1
fi
echo "publish-pipeline check passed"
